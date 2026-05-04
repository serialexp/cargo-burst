//! `cargo burst clippy` — run `cargo clippy` (or whatever cargo args
//! you pass) on the remote server.
//!
//! Pure passthrough — no artifact fetch. clippy is CPU-bound on the
//! same MIR cargo check produces, so the speedup vs. running locally
//! mirrors `cargo burst check`. Output (warnings, errors, lint
//! diagnostics) streams back over SSH.
//!
//! Common pattern: `cargo burst clippy -- --all-targets -- -D warnings`
//! to run the same gate CI does, on faster infra.
//!
//! The shared remote setup (provision, mount, rsync, heartbeat,
//! reaper) lives in [`crate::commands::remote`].

use anyhow::Result;
use clap::Args;

use crate::commands::remote::{self, RemoteOptions};

#[derive(Args, Debug)]
pub struct ClippyArgs {
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
    /// Args forwarded verbatim to `cargo` on the remote. Defaults to
    /// `["clippy"]` when none are supplied.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: ClippyArgs) -> Result<()> {
    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
    };
    remote::run_cargo_passthrough(opts, "Clippy", "clippy", args.cargo_args).await
}
