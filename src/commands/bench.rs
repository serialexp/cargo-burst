//! `cargo burst bench` — run `cargo bench` on the remote server.
//!
//! The strongest case for remote execution: bench results are only
//! meaningful relative to consistent hardware. Running on the CCX63
//! every time means thermals, background load, and CPU governor state
//! on your laptop don't pollute the numbers run-to-run. Comparing
//! today's bench to last week's is actually defensible.
//!
//! After the bench run finishes, if criterion's `target/criterion/`
//! report dir exists on the remote we rsync it back recursively so
//! you can browse the HTML reports locally. Pass `--no-fetch` to skip
//! that. Projects using libtest's built-in `#[bench]` (no criterion)
//! produce no report dir; the existence probe short-circuits the
//! fetch in that case.
//!
//! Note that the actual numbers stream to stdout during the run via
//! the SSH session, just like build/test — `--no-fetch` only skips
//! the HTML report copy, you don't lose the textual results.
//!
//! The shared remote setup (provision, mount, rsync, heartbeat,
//! reaper) lives in [`crate::commands::remote`].

use anyhow::{Result, anyhow};
use clap::Args;

use crate::commands::remote::{self, RemoteCtx, RemoteOptions};
use crate::ssh;

#[derive(Args, Debug)]
pub struct BenchArgs {
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
    /// Skip the criterion-report fetch step. The bench run still happens
    /// on the remote and its stdout streams back, but the HTML report
    /// dir stays on the volume. Useful if you only care about the
    /// numbers in your terminal and don't want to wait on the rsync.
    #[arg(long)]
    pub no_fetch: bool,
    /// Args forwarded verbatim to `cargo bench` on the remote.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub async fn run(args: BenchArgs) -> Result<()> {
    let opts = RemoteOptions {
        keep_alive: args.keep_alive,
        yes: args.yes,
        no_reap: args.no_reap,
    };
    let cargo_args = args.cargo_args.clone();
    let no_fetch = args.no_fetch;

    remote::with_remote(opts, "Bench", move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> = cargo_args.iter().map(|a| remote::shell_escape(a)).collect();
        let extra = if escaped.is_empty() {
            String::new()
        } else {
            format!(" {}", escaped.join(" "))
        };
        let body = format!("cargo bench{extra}");
        let cmd = remote::build_remote_cmd(&ctx, &body);
        let status = ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;
        if !status.success() {
            return Err(anyhow!("cargo bench exited with status {status}"));
        }

        if no_fetch {
            return Ok(());
        }

        // Probe for the criterion report dir. Projects that don't use
        // criterion (e.g. libtest #[bench]) won't have one — silently
        // skipping the fetch is the right behavior there.
        let probe = format!(
            "test -d {target}/criterion && echo yes || echo no",
            target = ctx.target_dir
        );
        let out =
            ssh::capture_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &probe).await?;
        if out.trim() != "yes" {
            tracing::debug!("no target/criterion dir on remote — skipping report fetch");
            return Ok(());
        }

        let remote_reports = format!("{}/criterion/", ctx.target_dir);
        let local_reports = ctx.workspace_root.join("target").join("criterion");
        std::fs::create_dir_all(&local_reports)
            .map_err(|e| anyhow!("creating {}: {e}", local_reports.display()))?;
        tracing::info!(
            from = %remote_reports,
            to = %local_reports.display(),
            "fetching criterion reports"
        );
        // Recursive — criterion reports are nested by benchmark group
        // and aren't part of the cargo build cache, so it's fine to
        // pull the whole tree (typically a few MB).
        ssh::rsync_from(
            &ctx.server_ip,
            "work",
            &ctx.ssh_key_path,
            &remote_reports,
            &local_reports,
            false,
        )
        .await?;
        Ok(())
    })
    .await
}
