//! Hidden internal subcommands: sleep, then maybe delete a provider resource.
//!
//! Two reapers, same shape:
//!
//! - `__reap-server` deletes the shared server after `keep_alive_secs`
//!   of no builds across *any* project — but never before the provider's
//!   `billing_minimum` lifetime has elapsed. On Hetzner that's 1 hour,
//!   so a server created at 14:00 with a 5-minute keep-alive doesn't
//!   die at 14:05 but at ~14:58 (the latest moment that's still inside
//!   the paid window). On AWS (per-second billing) it dies right at
//!   the configured keep-alive.
//! - `__reap-volume` deletes a project's volume after
//!   `volume_keep_alive_secs` of no builds for *that* project.
//!
//! Spawned in the background by the cargo verbs. Not part of the
//! user-facing CLI surface.
//!
//! ## Billing-minimum logic
//!
//! On wake-up the server reaper computes:
//!
//!   delete_at = max(idle_deadline, creation_time + billing_minimum - safety)
//!
//! where `idle_deadline` is the usual "last_used + keep_alive" cutoff.
//! If we wake up before `delete_at`, we re-sleep the difference, then
//! re-check. The safety margin is 2 minutes — Hetzner snaps the
//! billing increment at provision time, not at the second the server
//! goes away, so we want to be safely *inside* the paid window when
//! we send the delete.

use anyhow::{Context, Result};
use clap::Args;
use std::time::Duration;

use crate::config::{Config, State, StateLock};
use crate::provider::{self, ServerId, VolumeId};

/// Headroom on the billing-minimum window — we'd rather kill the
/// server 2 minutes early than 30 seconds late and pay for an extra
/// increment.
const BILLING_SAFETY_MARGIN: Duration = Duration::from_secs(120);

// ── Server reaper ──────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ReapServerArgs {
    #[arg(long)]
    pub server_id: String,
    #[arg(long)]
    pub after_secs: u64,
}

pub async fn run_server(args: ReapServerArgs) -> Result<()> {
    let server_id = ServerId(args.server_id.clone());
    let spawn_time = time::OffsetDateTime::now_utc();

    tokio::time::sleep(Duration::from_secs(args.after_secs)).await;

    let _lock = StateLock::acquire().await?;

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("server-reaper: failed to load config ({e}); leaving server alive");
            return Ok(());
        }
    };
    let state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("server-reaper: failed to load state ({e}); leaving server alive");
            return Ok(());
        }
    };

    // (1) State may have already been overwritten with a different
    // server_id. If so, bow out without re-spawning.
    if state.server_id.as_ref() != Some(&server_id) {
        tracing::info!(
            ours = %server_id,
            current = ?state.server_id,
            "server-reaper: state.server_id no longer matches; bowing out"
        );
        return Ok(());
    }

    // (2) Activity-since-spawn check. If anything ran in the window,
    // re-spawn a successor.
    if let Some(last) = state.last_used_any() {
        if last > spawn_time {
            tracing::info!(
                "server-reaper: activity at {last} > spawn {spawn_time}; \
                 re-spawning successor reaper"
            );
            drop(_lock);
            if let Err(e) = spawn_server(server_id, args.after_secs) {
                tracing::error!("server-reaper: failed to re-spawn successor: {e}");
            }
            return Ok(());
        }
    }

    // (3) Billing-minimum gate. Construct the provider so we can read
    // its billing_minimum, then wait out the remaining paid time if
    // any. The session record carries the provisioning timestamp.
    let provider = match provider::from_config(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("server-reaper: provider init failed ({e}); leaving server alive");
            return Ok(());
        }
    };
    let billing_min = provider.billing_minimum();
    if billing_min > Duration::from_secs(0) {
        if let Some(session) = state.current_server_session.as_ref() {
            if session.server_id == server_id {
                if let Ok(created) = time::OffsetDateTime::parse(
                    &session.started_at,
                    &time::format_description::well_known::Rfc3339,
                ) {
                    let now = time::OffsetDateTime::now_utc();
                    let earliest = created
                        + time::Duration::seconds(billing_min.as_secs() as i64)
                        - time::Duration::seconds(BILLING_SAFETY_MARGIN.as_secs() as i64);
                    if now < earliest {
                        let wait = earliest - now;
                        let wait_secs = wait.whole_seconds().max(0) as u64;
                        tracing::info!(
                            "server-reaper: holding server until billing minimum elapses \
                             ({wait_secs}s remaining of paid window)"
                        );
                        drop(_lock);
                        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                        // Re-acquire lock + re-check activity. If
                        // anyone bumped last_used during our extra
                        // wait, fall through into the re-spawn path
                        // by recursing through a fresh successor.
                        let _lock2 = StateLock::acquire().await?;
                        let state2 = State::load()?;
                        if state2.server_id.as_ref() != Some(&server_id) {
                            return Ok(());
                        }
                        if let Some(last) = state2.last_used_any() {
                            // "Activity since now-ish" — anything
                            // after the original spawn time still
                            // counts. We re-spawn instead of deleting.
                            if last > spawn_time {
                                drop(_lock2);
                                if let Err(e) =
                                    spawn_server(server_id, args.after_secs)
                                {
                                    tracing::error!(
                                        "server-reaper: failed to re-spawn after \
                                         billing wait: {e}"
                                    );
                                }
                                return Ok(());
                            }
                        }
                        // Fall through to the delete path below,
                        // with the lock held by _lock2.
                        drop(_lock2);
                    }
                }
            }
        }
    }

    // (4) Delete.
    if let Err(e) = provider.delete_server(&server_id).await {
        tracing::error!("server-reaper: failed to delete server {server_id}: {e}");
        return Ok(());
    }
    tracing::info!("server-reaper: deleted shared server {server_id}");

    let _lock3 = StateLock::acquire().await?;
    let mut state = State::load()?;
    if state.server_id.as_ref() == Some(&server_id) {
        state.server_id = None;
    }
    crate::audit::end_server_session(
        &mut state,
        &server_id,
        crate::audit::TerminationReason::Reap,
    );
    state.save().ok();
    Ok(())
}

/// Spawn a detached child that sleeps then runs the server-reap-decision
/// flow above.
pub fn spawn_server(server_id: ServerId, after_secs: u64) -> Result<()> {
    spawn_detached(&[
        "__reap-server".into(),
        "--server-id".into(),
        server_id.into_string(),
        "--after-secs".into(),
        after_secs.to_string(),
    ])
}

// ── Volume reaper ──────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ReapVolumeArgs {
    #[arg(long)]
    pub volume_id: String,
    /// Project hash (sha256-prefix of workspace root).
    #[arg(long)]
    pub project_hash: String,
    #[arg(long)]
    pub after_secs: u64,
}

pub async fn run_volume(args: ReapVolumeArgs) -> Result<()> {
    let volume_id = VolumeId(args.volume_id.clone());
    let spawn_time = time::OffsetDateTime::now_utc();
    tokio::time::sleep(Duration::from_secs(args.after_secs)).await;

    let _lock = StateLock::acquire().await?;

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("volume-reaper: failed to load config ({e}); leaving volume alive");
            return Ok(());
        }
    };
    let state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("volume-reaper: failed to load state ({e}); leaving volume alive");
            return Ok(());
        }
    };

    let project = match state.projects.get(&args.project_hash) {
        Some(p) => p,
        None => {
            tracing::info!(
                hash = %args.project_hash,
                "volume-reaper: project no longer in state; bowing out"
            );
            return Ok(());
        }
    };
    if project.volume_id.as_ref() != Some(&volume_id) {
        tracing::info!(
            ours = %volume_id,
            current = ?project.volume_id,
            hash = %args.project_hash,
            "volume-reaper: state.projects[hash].volume_id no longer matches; bowing out"
        );
        return Ok(());
    }

    if let Some(last) = project.last_used_rfc3339.as_deref() {
        if let Ok(parsed) =
            time::OffsetDateTime::parse(last, &time::format_description::well_known::Rfc3339)
        {
            if parsed > spawn_time {
                tracing::info!(
                    hash = %args.project_hash,
                    "volume-reaper: activity at {parsed} > spawn {spawn_time}; \
                     re-spawning successor reaper"
                );
                drop(_lock);
                if let Err(e) =
                    spawn_volume(volume_id, args.project_hash.clone(), args.after_secs)
                {
                    tracing::error!("volume-reaper: failed to re-spawn successor: {e}");
                }
                return Ok(());
            }
        }
    }

    let provider = match provider::from_config(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("volume-reaper: provider init failed ({e}); leaving volume alive");
            return Ok(());
        }
    };
    if let Err(e) = provider.detach_volume(&volume_id).await {
        tracing::warn!(
            "volume-reaper: detach for volume {volume_id} failed (continuing): {e}"
        );
    }
    if let Err(e) = provider.delete_volume(&volume_id).await {
        tracing::error!("volume-reaper: failed to delete volume {volume_id}: {e}");
        return Ok(());
    }
    tracing::info!(
        volume = %volume_id,
        hash = %args.project_hash,
        "volume-reaper: deleted idle project volume"
    );

    let provider_name = provider.name();
    let mut state = State::load()?;
    if let Some(p) = state.projects.get_mut(&args.project_hash) {
        if p.volume_id.as_ref() == Some(&volume_id) {
            p.volume_id = None;
        }
        crate::audit::end_volume_session(
            p,
            &args.project_hash,
            &volume_id,
            provider_name,
            crate::audit::TerminationReason::Reap,
        );
        state.save().ok();
    }
    Ok(())
}

/// Spawn a detached child that sleeps then runs the volume-reap-decision
/// flow above.
pub fn spawn_volume(
    volume_id: VolumeId,
    project_hash: String,
    after_secs: u64,
) -> Result<()> {
    spawn_detached(&[
        "__reap-volume".into(),
        "--volume-id".into(),
        volume_id.into_string(),
        "--project-hash".into(),
        project_hash,
        "--after-secs".into(),
        after_secs.to_string(),
    ])
}

// ── Shared spawn helper ────────────────────────────────────────────────

fn spawn_detached(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("locating cargo-burst binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc_setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().context("spawning reaper")?;
    Ok(())
}

#[cfg(unix)]
fn libc_setsid() -> std::io::Result<()> {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    let r = unsafe { setsid() };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
