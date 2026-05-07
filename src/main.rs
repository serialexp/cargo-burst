//! cargo-burst — spin up a Hetzner Cloud server on demand for `cargo build`,
//! with a persistent per-project volume that survives between sessions.
//!
//! Entry point. Wires up logging, parses CLI args, and dispatches to the
//! command modules in `commands/`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

mod audit;
mod commands;
mod config;
mod hcloud;
mod project;
mod ssh;

/// Top-level CLI. We support invocation both as a standalone binary
/// (`cargo-burst <args>`) and as a cargo subcommand (`cargo burst <args>`).
/// When invoked via `cargo burst …`, cargo passes `burst` as the first arg;
/// we strip it before clap parsing.
#[derive(Parser, Debug)]
#[command(
    name = "cargo-burst",
    version,
    about = "Run cargo builds on a Hetzner Cloud server with a persistent target/ volume.",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the base image (Ubuntu + rust + mold + sccache).
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Build the current cargo project on the remote server.
    Build(commands::build::BuildArgs),
    /// Run the current cargo project's tests on the remote server.
    /// Defaults to `cargo nextest run` followed by `cargo test --doc`
    /// (matching `cargo test`'s coverage but exploiting nextest's
    /// per-process parallelism on the CCX63's 48 cores).
    ///
    /// Database services available on the remote (localhost-only,
    /// started on every cold boot; the test runner blocks until all
    /// three ports are reachable):
    ///
    ///   postgres://postgres@localhost:5432/postgres   (no password, trust auth)
    ///   mysql://root:root@localhost:3306/
    ///   redis://localhost:6379/
    ///
    /// DB data lives on the server's root disk, not the per-project
    /// volume — every cold-booted server starts with empty databases.
    #[command(verbatim_doc_comment)]
    Test(commands::test::TestArgs),
    /// Run `cargo check` on the remote server. Pure passthrough — no
    /// artifact fetch, output streams back over SSH.
    Check(commands::check::CheckArgs),
    /// Run `cargo clippy` on the remote server. Pure passthrough — no
    /// artifact fetch, output streams back over SSH.
    Clippy(commands::clippy::ClippyArgs),
    /// Run `cargo bench` on the remote server. Consistent hardware
    /// makes bench numbers comparable across runs. By default the
    /// criterion HTML report dir (if present) is rsynced back into
    /// the local `target/criterion/` after the run.
    ///
    /// Database services available on the remote (same as `test`,
    /// localhost-only, started on every cold boot; the bench runner
    /// blocks until all three ports are reachable):
    ///
    ///   postgres://postgres@localhost:5432/postgres   (no password, trust auth)
    ///   mysql://root:root@localhost:3306/
    ///   redis://localhost:6379/
    #[command(verbatim_doc_comment)]
    Bench(commands::bench::BenchArgs),
    /// Show what's currently provisioned (server, volumes, current cost).
    Status,
    /// Summarize the audit log: how many sessions, cold-vs-warm
    /// command timings, and the wall-time split between provision /
    /// sync / cargo. Useful for answering "is burst worth it?".
    Audit(commands::audit::AuditArgs),
    /// Delete the running server now. Project volumes are preserved by
    /// default; pass `--with-volumes` to delete those too (clean slate).
    Down(commands::down::DownArgs),
    /// Write usage instructions for cargo-burst into `~/.claude/CLAUDE.md`
    /// so Claude Code knows when and how to reach for it. Idempotent —
    /// re-run after upgrading to refresh the instructions block.
    Install,
    /// Internal: detached reaper used by `build` to delete the shared
    /// server after the keep-alive timer expires. Not a user-facing command.
    #[command(name = "__reap-server", hide = true)]
    ReapServer(commands::reap::ReapServerArgs),
    /// Internal: detached reaper used by `build` to delete a project's
    /// volume after `volume_keep_alive_secs` of inactivity for that
    /// project. Not a user-facing command.
    #[command(name = "__reap-volume", hide = true)]
    ReapVolume(commands::reap::ReapVolumeArgs),
}

#[derive(Subcommand, Debug)]
enum ImageAction {
    /// Bake a fresh snapshot.
    Build(commands::image::ImageBuildArgs),
}

fn install_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();

    // Strip the leading `burst` arg if invoked as `cargo burst …`. Cargo
    // passes the subcommand name through as argv[1], which would otherwise
    // confuse clap (it would try to match `burst` as a subcommand of
    // cargo-burst itself).
    let mut args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("burst") {
        args.remove(1);
    }

    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Image { action } => match action {
            ImageAction::Build(args) => commands::image::run(args).await,
        },
        Command::Build(args) => commands::build::run(args).await,
        Command::Test(args) => commands::test::run(args).await,
        Command::Check(args) => commands::check::run(args).await,
        Command::Clippy(args) => commands::clippy::run(args).await,
        Command::Bench(args) => commands::bench::run(args).await,
        Command::Status => commands::status::run().await,
        Command::Audit(args) => commands::audit::run(args).await,
        Command::Down(args) => commands::down::run(args).await,
        Command::Install => commands::install::run().await,
        Command::ReapServer(args) => commands::reap::run_server(args).await,
        Command::ReapVolume(args) => commands::reap::run_volume(args).await,
    }
}
