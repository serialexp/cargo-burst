//! `cargo burst test` — run the project's test suite on the remote
//! server.
//!
//! Default behavior matches `cargo test`'s coverage but on faster
//! infrastructure: nextest for unit + integration tests, then
//! `cargo test --doc` for doctests (since nextest can't run those).
//! If nextest reports any failures the doctest pass is skipped — we
//! already know the codebase isn't green and there's no value in
//! running more tests on it.
//!
//! Escape hatches:
//!
//! - `--no-doctests`: skip the `cargo test --doc` step. Useful for
//!   projects without doctests, or if you've decided you don't care
//!   about exercising them under burst.
//! - `--cargo-test`: run plain `cargo test` instead of nextest. Use
//!   this if your tests don't play nicely with nextest's per-test
//!   process model (rare — usually fixtures set up via `OnceCell` or
//!   similar that you don't want to pay for per-test).
//!
//! Args after `--` are forwarded to whichever runner ends up running.
//! Common cargo flags (`--release`, `-p foo`, `--features bar`) are
//! understood by both nextest and cargo, so the same args work for
//! either path.

use anyhow::{Context, Result, anyhow};
use clap::Args;
use std::path::Path;
use std::process::Command;

use crate::commands::remote::{self, RemoteCtx, RemoteOptions};
use crate::ssh;

#[derive(Args, Debug)]
pub struct TestArgs {
    /// How long to keep the server alive after this run before
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
    /// Skip the `cargo test --doc` step that runs after nextest.
    #[arg(long)]
    pub no_doctests: bool,
    /// Use plain `cargo test` instead of nextest. Implies the doctest
    /// step is part of the same invocation (cargo test runs everything),
    /// so `--no-doctests` is ignored under this mode.
    #[arg(long, conflicts_with = "no_doctests")]
    pub cargo_test: bool,
    /// Args forwarded verbatim to the test runner on the remote.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: TestArgs) -> Result<()> {
    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
    };

    let cargo_args = args.cargo_args.clone();
    let no_doctests = args.no_doctests;
    let use_cargo_test = args.cargo_test;

    remote::with_remote(opts, "Tests", move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> =
            cargo_args.iter().map(|a| remote::shell_escape(a)).collect();
        let extra = if escaped.is_empty() {
            String::new()
        } else {
            format!(" {}", escaped.join(" "))
        };

        if use_cargo_test {
            // Single shot: vanilla cargo test (covers unit + integration
            // + doctests, like the user expects from a bare `cargo test`).
            let body = format!("cargo test{extra}");
            let cmd = remote::build_remote_cmd(&ctx, &body);
            let status = ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;
            if !status.success() {
                return Err(anyhow!("cargo test exited with status {status}"));
            }
            return Ok(());
        }

        // Default path: nextest first.
        let body = format!("cargo nextest run{extra}");
        let cmd = remote::build_remote_cmd(&ctx, &body);
        let nextest_status =
            ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;

        if !nextest_status.success() {
            // Bail before doctests — no point exercising them on a
            // codebase we already know is broken. nextest's default
            // is run-everything-then-report, so by the time we're
            // here the user has the full unit+integration picture.
            return Err(anyhow!(
                "nextest reported failures (status {nextest_status}); skipping doctests"
            ));
        }

        if no_doctests {
            return Ok(());
        }

        // `cargo test --doc` errors out with `no library targets found in
        // package` (exit 101) for workspaces that consist only of binary
        // crates. nextest already covered everything that exists in that
        // case, so the doctest pass would be pure noise. Probe the local
        // workspace via `cargo metadata` and skip when no package has a
        // library-shaped target.
        match workspace_has_library_target(&ctx.workspace_root) {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    "no library targets in workspace; skipping `cargo test --doc`"
                );
                return Ok(());
            }
            Err(e) => {
                // Don't fail the whole run on a metadata hiccup — fall
                // back to running the doctest pass and letting cargo
                // itself report the truth.
                tracing::warn!(error = %e, "library-target probe failed; running doctests anyway");
            }
        }

        // Doctests pass. `--quiet` collapses cargo's per-test
        // chatter to one character per test; for the very common
        // case of "lib target with zero ```rust fences" this turns
        // a multi-line `Doc-tests <crate> / running 0 tests / test
        // result: ok. 0 passed; …` block per crate into a single
        // summary line per crate. Failures still print in full.
        // We splice `--quiet` ahead of the user's args so that an
        // explicit `-- --verbose` (or `-vv`) still wins via cargo's
        // last-flag-wins convention.
        let body = format!("cargo test --doc --quiet{extra}");
        let cmd = remote::build_remote_cmd(&ctx, &body);
        let doc_status =
            ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;
        if !doc_status.success() {
            return Err(anyhow!("cargo test --doc exited with status {doc_status}"));
        }
        Ok(())
    })
    .await
}

/// Return true if any package in the workspace exposes a doctest-eligible
/// target (`lib`, `rlib`, `dylib`, `cdylib`, `staticlib`, or `proc-macro`).
/// Anything else — pure-binary crates, examples, tests — produces no
/// doctests and would cause `cargo test --doc` to error with
/// "no library targets found in package".
///
/// Uses `cargo metadata --no-deps` against the locally-rsync'd workspace
/// rather than parsing `Cargo.toml` ourselves: cargo's auto-target rules
/// (`src/lib.rs`, `[lib]` sections, workspace inheritance) are subtle and
/// re-implementing them is a recipe for drift. `--no-deps` keeps it fast
/// (no registry resolution).
fn workspace_has_library_target(workspace_root: &Path) -> Result<bool> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version=1"])
        .current_dir(workspace_root)
        .output()
        .context("running `cargo metadata`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "`cargo metadata` exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata JSON")?;
    let packages = meta
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("cargo metadata: `packages` array missing"))?;
    for pkg in packages {
        let Some(targets) = pkg.get("targets").and_then(|v| v.as_array()) else {
            continue;
        };
        for target in targets {
            let Some(kinds) = target.get("kind").and_then(|v| v.as_array()) else {
                continue;
            };
            for kind in kinds {
                if matches!(
                    kind.as_str(),
                    Some("lib")
                        | Some("rlib")
                        | Some("dylib")
                        | Some("cdylib")
                        | Some("staticlib")
                        | Some("proc-macro")
                ) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a throwaway workspace at `dir` whose root Cargo.toml is
    /// `manifest` and whose only source file is `main.rs` or `lib.rs` per
    /// `is_lib`. Returns the dir path.
    fn make_crate(dir: &Path, manifest: &str, is_lib: bool) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        let body = if is_lib {
            "pub fn x() -> u32 { 1 }\n"
        } else {
            "fn main() {}\n"
        };
        let name = if is_lib { "lib.rs" } else { "main.rs" };
        fs::write(dir.join("src").join(name), body).unwrap();
    }

    #[test]
    fn bin_only_crate_reports_no_lib() {
        let tmp = tempdir();
        make_crate(
            tmp.path(),
            r#"
[package]
name = "binonly"
version = "0.0.0"
edition = "2021"
"#,
            false,
        );
        assert!(!workspace_has_library_target(tmp.path()).unwrap());
    }

    #[test]
    fn lib_crate_reports_lib() {
        let tmp = tempdir();
        make_crate(
            tmp.path(),
            r#"
[package]
name = "libonly"
version = "0.0.0"
edition = "2021"
"#,
            true,
        );
        assert!(workspace_has_library_target(tmp.path()).unwrap());
    }

    #[test]
    fn proc_macro_crate_reports_lib() {
        let tmp = tempdir();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            r#"
[package]
name = "pmcrate"
version = "0.0.0"
edition = "2021"

[lib]
proc-macro = true
"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("src/lib.rs"),
            "// proc-macro stub\n",
        )
        .unwrap();
        assert!(workspace_has_library_target(tmp.path()).unwrap());
    }

    /// Minimal in-test temp-dir helper so we don't pull in `tempfile`
    /// just for the test config. Drops the directory on Drop.
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }
    fn tempdir() -> TmpDir {
        // Use nanoseconds + pid for uniqueness — the test runs are
        // serialized within a single binary, but parallel test workers
        // (nextest) would otherwise collide on a shared name.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("cargo-burst-test-{pid}-{nanos}"));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
