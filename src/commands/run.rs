//! `cargo burst run` — run `cargo run` on the remote server.
//!
//! Pure passthrough — no artifact fetch. Useful when you want to
//! exercise a binary on the CCX63's 48 cores instead of your laptop's
//! ~12, especially for embarrassingly-parallel CPU-bound work
//! (perf debugging, parameter sweeps, large-input one-offs). The
//! binary is built and executed entirely on the remote; stdout and
//! stderr stream back over SSH.
//!
//! Caveats:
//!   - No TTY by default. If your binary checks `isatty(stdout)` to
//!     decide whether to colorize, it'll see "not a TTY" and likely
//!     drop colors. Force them with `--env CARGO_TERM_COLOR=always`
//!     or whatever your binary's equivalent is.
//!   - Stdin is closed (`/dev/null`). Interactive binaries won't
//!     work today; if you need that we can add `--tty` later.
//!   - File outputs written by the binary stay on the remote in the
//!     project's volume-backed `target/` (or wherever it wrote
//!     them). They aren't fetched. If you need them back, add a
//!     `target/criterion`-style fetch step like `bench` does.
//!
//! The shared remote setup (provision, mount, rsync, heartbeat,
//! reaper) lives in [`crate::commands::remote`]. Args after `--` go
//! verbatim to cargo, including the second `--` separating cargo's
//! flags from the binary's arguments:
//!
//!   cargo burst run -- --release --bin foo -- arg1 arg2

use anyhow::Result;
use clap::Args;

use crate::commands::remote::{self, RemoteOptions};

#[derive(Args, Debug)]
pub struct RunArgs {
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
    /// Forward an environment variable to the remote cargo invocation
    /// AND to the binary it spawns. `NAME` forwards the local value of
    /// `$NAME`; `NAME=value` sets it verbatim. Repeatable. Per-run
    /// `--env` overrides the `forward_env` config field on a name
    /// conflict.
    #[arg(long = "env", value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Args forwarded verbatim to `cargo` on the remote. The leading
    /// `--` is optional: `cargo burst run --release --bin foo` and
    /// `cargo burst run -- --release --bin foo` both work. An *inner*
    /// `--` (separating `cargo run` args from the binary's argv) is
    /// preserved, so `cargo burst run --release --bin foo -- arg1
    /// arg2` works the way you'd expect. Defaults to `["run"]` when
    /// none are supplied.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: RunArgs) -> Result<()> {
    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
        cli_env: args.env.clone(),
    };
    remote::run_cargo_passthrough(opts, "Run", "run", args.cargo_args).await
}
