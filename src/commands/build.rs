//! `cargo burst build` — run `cargo build` (or whatever cargo args you
//! pass) on the remote server, then fetch the produced top-level
//! artifacts back into the local `target/<profile>/` so you can run
//! them locally without re-building.
//!
//! Most of the actual work — provisioning, mount, rsync, heartbeat,
//! reaper scheduling — lives in [`crate::commands::remote`]. This
//! subcommand is a thin clap shell plus the post-cargo artifact fetch.

use anyhow::{Result, anyhow};
use clap::Args;

use crate::commands::remote::{self, RemoteCtx, RemoteOptions};
use crate::ssh;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// How long to keep the server alive after this build before
    /// auto-deleting. Overrides config's `keep_alive_secs`.
    #[arg(long, value_name = "SECONDS")]
    pub keep_alive: Option<u64>,
    /// Skip the size-summary confirmation prompt even on first sync.
    #[arg(long)]
    pub yes: bool,
    /// Don't schedule the auto-delete reaper. Server (and volume) stay
    /// alive indefinitely until you run `cargo burst down`.
    #[arg(long)]
    pub no_reap: bool,
    /// Skip the artifact fetch step. The cargo run still happens on the
    /// remote, but the resulting binary stays on the volume. Useful if
    /// you only want the cache populated and don't need a runnable
    /// binary locally (e.g. a pure type-check via `cargo build --tests`
    /// where you'll re-run `cargo burst test` anyway).
    #[arg(long)]
    pub no_fetch: bool,
    /// Forward an environment variable to the remote cargo invocation.
    /// `NAME` forwards the local value of `$NAME`; `NAME=value` sets
    /// it verbatim. Repeatable. Per-run `--env` overrides the
    /// `forward_env` config field on a name conflict.
    #[arg(long = "env", value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Args forwarded verbatim to `cargo` on the remote. Defaults to
    /// `["build"]` when none are supplied.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    // Args after `--` are forwarded as cargo flags. The verb (`build`)
    // is always implicit, so we splice it in front unless the user
    // already typed it.
    let cargo_args = remote::prepend_cargo_verb("build", args.cargo_args.clone());

    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
        cli_env: args.env.clone(),
    };

    let no_fetch = args.no_fetch;

    remote::with_remote(opts, "Build", move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> = cargo_args.iter().map(|a| remote::shell_escape(a)).collect();
        let cmd = remote::build_remote_cmd(&ctx, &format!("cargo {}", escaped.join(" ")));
        let status = remote::run_cargo_with_hints(&ctx, "build", &cmd).await?;
        if !status.success() {
            return Err(anyhow!("cargo exited with status {status}"));
        }

        // Artifact fetch. We only do this for the `build` verb — `check`,
        // `clippy`, `doc`, `fmt`, `run`, `test` etc. either don't produce
        // useful artifacts or wouldn't benefit from a copy back. Anything
        // else is a no-op (cargo run executed on the remote and we already
        // streamed its stdout via run_remote).
        if no_fetch {
            return Ok(());
        }
        let verb = cargo_verb(&cargo_args);
        if verb != Some("build") {
            return Ok(());
        }

        let profile_dir = cargo_profile_dir(&cargo_args);
        let triples = cargo_target_triples(&cargo_args);

        // Compose the list of (remote, local) artifact directories to
        // fetch. Cargo's layout:
        //   - no `--target`: artifacts at `target/<profile>/`
        //   - one or more `--target X`: artifacts at `target/<X>/<profile>/`
        //     for each X, and *nothing* at the bare `target/<profile>/`.
        // Mirror that exactly so a multi-target invocation pulls every
        // artifact set back to the equivalent local subdir.
        let fetches: Vec<(String, std::path::PathBuf)> = if triples.is_empty() {
            vec![(
                format!("{}/{profile_dir}/", ctx.target_dir),
                ctx.workspace_root.join("target").join(&profile_dir),
            )]
        } else {
            triples
                .iter()
                .map(|t| {
                    (
                        format!("{}/{t}/{profile_dir}/", ctx.target_dir),
                        ctx.workspace_root
                            .join("target")
                            .join(t)
                            .join(&profile_dir),
                    )
                })
                .collect()
        };

        // Hetzner's CCX boxes are linux x86_64. If the local machine
        // doesn't match AND the user didn't explicitly cross-compile
        // (i.e. no `--target`), the fetched binary is built for an arch
        // the local OS can't execute. Warn rather than skip: the user
        // might genuinely want the binary for ssh/scp onwards. When
        // `--target` is explicit the user already knows they're cross-
        // compiling for deployment, so the warning would just be noise.
        let host_os = std::env::consts::OS;
        let host_arch = std::env::consts::ARCH;
        if triples.is_empty() && (host_os != "linux" || host_arch != "x86_64") {
            tracing::warn!(
                local = %format!("{host_os}-{host_arch}"),
                remote = "linux-x86_64",
                "fetched binary was built for the remote target — it will \
                 not run on this machine. Pass `--no-fetch` to skip the \
                 download if you don't need the artifact locally."
            );
        }

        for (remote_artifacts, local_artifacts) in &fetches {
            std::fs::create_dir_all(local_artifacts)
                .map_err(|e| anyhow!("creating {}: {e}", local_artifacts.display()))?;
            tracing::info!(
                from = %remote_artifacts,
                to = %local_artifacts.display(),
                "fetching top-level artifacts"
            );
            // top_level_only=true: skip deps/, build/, incremental/,
            // .fingerprint/ etc. Local cargo will rebuild those on its next
            // invocation regardless — pulling them back would just waste
            // bandwidth and disk. The binary you actually want lives at the
            // top level alongside .so/.dylib outputs.
            ssh::rsync_from(
                &ctx.server_ip,
                "work",
                &ctx.ssh_key_path,
                remote_artifacts,
                local_artifacts,
                true,
            )
            .await?;
        }
        Ok(())
    })
    .await
}

/// First non-flag token in `args` is the cargo verb (build / test /
/// check / clippy / …). Returns `None` only when the user passed an
/// empty arg list — we already substitute `["build"]` upstream so this
/// shouldn't fire in practice.
fn cargo_verb(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| !a.starts_with('-'))
}

/// Determine which `target/<dir>/` cargo will write artifacts to,
/// based on `--release` / `--profile=X` flags. Lazy version: handles
/// the standard four profiles (`dev`, `release`, `test`, `bench`)
/// plus arbitrary custom profiles which cargo writes to
/// `target/<custom-name>/` literally. Doesn't handle obscure cases
/// like `--profile` followed by a quoted value containing `=`.
fn cargo_profile_dir(args: &[String]) -> String {
    if args.iter().any(|a| a == "--release" || a == "-r") {
        return "release".into();
    }
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--profile" {
            if let Some(name) = iter.next() {
                return profile_to_dir(name).into();
            }
        } else if let Some(name) = a.strip_prefix("--profile=") {
            return profile_to_dir(name).into();
        }
    }
    "debug".into()
}

fn profile_to_dir(profile: &str) -> &str {
    match profile {
        "release" | "bench" => "release",
        "dev" | "test" => "debug",
        // Custom profile — cargo uses the profile name as the dir.
        custom => custom,
    }
}

/// Extract every `--target <triple>` / `--target=<triple>` value from
/// the cargo args, in argv order, deduplicated (cargo itself ignores
/// duplicate `--target` values, so we mirror that — fetching the same
/// directory twice would be a waste).
///
/// When this returns an empty Vec, cargo writes artifacts to
/// `target/<profile>/`. When it returns one or more triples, cargo
/// writes artifacts to `target/<triple>/<profile>/` for each triple
/// and *nothing* to the bare `target/<profile>/`.
fn cargo_target_triples(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        let triple = if a == "--target" {
            iter.next().map(String::as_str)
        } else {
            a.strip_prefix("--target=")
        };
        if let Some(t) = triple {
            if !t.is_empty() && !out.iter().any(|existing| existing == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn no_target_returns_empty() {
        assert!(cargo_target_triples(&s(&["build", "--release"])).is_empty());
    }

    #[test]
    fn space_form_single() {
        assert_eq!(
            cargo_target_triples(&s(&["build", "--target", "aarch64-unknown-linux-gnu"])),
            vec!["aarch64-unknown-linux-gnu".to_string()]
        );
    }

    #[test]
    fn equals_form_single() {
        assert_eq!(
            cargo_target_triples(&s(&["build", "--target=x86_64-unknown-linux-musl"])),
            vec!["x86_64-unknown-linux-musl".to_string()]
        );
    }

    #[test]
    fn multiple_targets_preserve_order() {
        assert_eq!(
            cargo_target_triples(&s(&[
                "build",
                "--release",
                "--target",
                "aarch64-unknown-linux-gnu",
                "--target=x86_64-unknown-linux-musl",
            ])),
            vec![
                "aarch64-unknown-linux-gnu".to_string(),
                "x86_64-unknown-linux-musl".to_string(),
            ]
        );
    }

    #[test]
    fn duplicate_targets_collapsed() {
        assert_eq!(
            cargo_target_triples(&s(&[
                "build",
                "--target",
                "aarch64-unknown-linux-gnu",
                "--target=aarch64-unknown-linux-gnu",
            ])),
            vec!["aarch64-unknown-linux-gnu".to_string()]
        );
    }

    #[test]
    fn dangling_target_flag_is_ignored() {
        // `--target` with no value — malformed; cargo would error, we
        // just skip it rather than panic.
        assert!(cargo_target_triples(&s(&["build", "--target"])).is_empty());
    }

    #[test]
    fn target_dir_flag_is_not_a_target() {
        // `--target-dir` shares a prefix; make sure we don't accidentally
        // treat it (or any other future `--target<something>` flag) as
        // an arch triple.
        assert!(cargo_target_triples(&s(&["build", "--target-dir", "/tmp/x"])).is_empty());
        assert!(cargo_target_triples(&s(&["build", "--target-dir=/tmp/x"])).is_empty());
    }
}
