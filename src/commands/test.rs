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

use anyhow::{Result, anyhow};
use clap::Args;

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

        // Doctests pass.
        let body = format!("cargo test --doc{extra}");
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
