//! `cargo burst check` — run `cargo check` (or whatever cargo args you
//! pass) on the remote server.
//!
//! Pure passthrough — no artifact fetch. The win is the type-check
//! itself running on the CCX63's 48 cores instead of your laptop, with
//! the warm `target/` and `sccache` from previous runs already on the
//! attached volume. Output streams back over SSH like build/test.
//!
//! The shared remote setup (provision, mount, rsync, heartbeat,
//! reaper) lives in [`crate::commands::remote`].

use anyhow::Result;
use clap::Args;

use crate::commands::remote::{self, RemoteOptions};

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// How long to keep the server alive after this check before
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
    /// Forward an environment variable to the remote cargo invocation.
    /// `NAME` forwards the local value of `$NAME`; `NAME=value` sets
    /// it verbatim. Repeatable. Per-run `--env` overrides the
    /// `forward_env` config field on a name conflict.
    #[arg(long = "env", value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Args forwarded verbatim to `cargo` on the remote. Defaults to
    /// `["check"]` when none are supplied.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: CheckArgs) -> Result<()> {
    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
        cli_env: args.env.clone(),
    };
    remote::run_cargo_passthrough(opts, "Check", "check", args.cargo_args).await
}
