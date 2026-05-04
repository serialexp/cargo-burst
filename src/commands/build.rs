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
    /// Args forwarded verbatim to `cargo` on the remote. Defaults to
    /// `["build"]` when none are supplied.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    let cargo_args = if args.cargo_args.is_empty() {
        vec!["build".to_string()]
    } else {
        args.cargo_args.clone()
    };

    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
    };

    let no_fetch = args.no_fetch;

    remote::with_remote(opts, "Build", move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> = cargo_args.iter().map(|a| remote::shell_escape(a)).collect();
        let cmd = remote::build_remote_cmd(&ctx, &format!("cargo {}", escaped.join(" ")));
        let status = ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;
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
        let remote_artifacts = format!("{}/{profile_dir}/", ctx.target_dir);
        let local_artifacts = ctx.workspace_root.join("target").join(&profile_dir);
        std::fs::create_dir_all(&local_artifacts)
            .map_err(|e| anyhow!("creating {}: {e}", local_artifacts.display()))?;

        // Hetzner's CCX boxes are linux x86_64. If the local machine
        // doesn't match, the fetched binary is built for an arch the
        // local OS can't execute — fine if you're cross-compiling for
        // deployment to a linux amd64 host, but surprising otherwise.
        // Warn rather than skip: the user might genuinely want the
        // binary for ssh/scp onwards.
        let host_os = std::env::consts::OS;
        let host_arch = std::env::consts::ARCH;
        if host_os != "linux" || host_arch != "x86_64" {
            tracing::warn!(
                local = %format!("{host_os}-{host_arch}"),
                remote = "linux-x86_64",
                "fetched binary was built for the remote target — it will \
                 not run on this machine. Pass `--no-fetch` to skip the \
                 download if you don't need the artifact locally."
            );
        }

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
            &remote_artifacts,
            &local_artifacts,
            true,
        )
        .await?;
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
