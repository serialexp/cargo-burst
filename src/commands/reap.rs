//! Hidden internal subcommands: sleep, then maybe delete a Hetzner resource.
//!
//! Two reapers, same shape:
//!
//! - `__reap-server` deletes the shared CCX server after `keep_alive_secs`
//!   of no builds across *any* project.
//! - `__reap-volume` deletes a project's volume after
//!   `volume_keep_alive_secs` of no builds for *that* project. Volumes are
//!   billed by provisioned size — a 200 GB cache that sits idle for a week
//!   is ~$2 thrown away. Reaping idle volumes and rebuilding the cache on
//!   next use (one cargo fresh-build, ~30s) is the cheaper trade.
//!
//! Spawned in the background by `cargo burst build`. Not part of the
//! user-facing CLI surface — users go through `cargo burst down` or wait
//! for the keep-alive timers.
//!
//! Decision flow on wake-up (under the global state lock):
//!
//! 1. If the resource we were asked to reap no longer matches what's in
//!    state (server_id changed, or the project's volume_id changed) →
//!    bow out (someone re-provisioned).
//! 2. If the relevant `last_used_rfc3339` is *after* our spawn time →
//!    activity happened in the keep-alive window. Bow out, but **re-spawn
//!    a fresh reaper** so the lifecycle is still covered if the active
//!    build crashes mid-flight without spawning its own end-of-build
//!    reaper.
//! 3. Else → delete the resource and clear the corresponding state field.
//!
//! The re-spawn step in (2) is what lets us keep the design robust to
//! "build process died mid-cargo": the heartbeat in the controller stops,
//! `last_used` goes stale, and the next reaper in the chain (which we
//! just re-spawned) will see the staleness and clean up.

use anyhow::{Context, Result};
use clap::Args;
use std::time::Duration;

use crate::config::{Config, State, StateLock};
use crate::hcloud::HCloud;

// ── Server reaper ──────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ReapServerArgs {
    #[arg(long)]
    pub server_id: i64,
    #[arg(long)]
    pub after_secs: u64,
}

pub async fn run_server(args: ReapServerArgs) -> Result<()> {
    // Record the time we were spawned so we can detect "another build
    // bumped state.json after us" — that means the keep-alive timer
    // should restart and we should bow out.
    let spawn_time = time::OffsetDateTime::now_utc();

    tokio::time::sleep(Duration::from_secs(args.after_secs)).await;

    // Read state under the global lock so we don't race a build that's
    // mid-provision or mid-heartbeat.
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

    // (1) Defensive: state may have already been overwritten with a different
    // server_id (someone manually nuked + re-provisioned). If so, the server
    // we were asked to reap is no longer "ours" — bow out without
    // re-spawning, since the new owner has its own lifecycle.
    if state.server_id != Some(args.server_id) {
        tracing::info!(
            ours = args.server_id,
            current = ?state.server_id,
            "server-reaper: state.server_id no longer matches; bowing out"
        );
        return Ok(());
    }

    // (2) Activity-since-spawn check. The server reaper takes max across
    // every project — it doesn't care which project ran a build, only that
    // *something* ran in the window.
    if let Some(last) = state.last_used_any() {
        if last > spawn_time {
            tracing::info!(
                "server-reaper: activity at {last} > spawn {spawn_time}; \
                 re-spawning successor reaper"
            );
            // Drop the lock before we fork — the successor will need
            // to acquire it eventually, and there's nothing left to do
            // here that depends on the lock.
            drop(_lock);
            if let Err(e) = spawn_server(args.server_id, args.after_secs) {
                tracing::error!("server-reaper: failed to re-spawn successor: {e}");
            }
            return Ok(());
        }
    }

    // (3) Delete. Hetzner detaches every attached volume automatically
    // when the server is deleted, so we don't need to touch the volumes
    // here — they survive (until their own per-volume reapers fire).
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;
    if let Err(e) = hcloud.delete_server(args.server_id).await {
        tracing::error!("server-reaper: failed to delete server {}: {e}", args.server_id);
        return Ok(());
    }
    tracing::info!("server-reaper: deleted shared server {}", args.server_id);

    // Clear server_id under the lock we already hold. We re-load to pick
    // up any concurrent state writes since our earlier read, and only
    // clear if the state still points at the server we deleted.
    let mut state = State::load()?;
    if state.server_id == Some(args.server_id) {
        state.server_id = None;
        state.save().ok();
    }
    Ok(())
}

/// Spawn a detached child that sleeps then runs the server-reap-decision
/// flow above. Survives the parent exiting; does not survive across reboots.
pub fn spawn_server(server_id: i64, after_secs: u64) -> Result<()> {
    spawn_detached(&[
        "__reap-server".into(),
        "--server-id".into(),
        server_id.to_string(),
        "--after-secs".into(),
        after_secs.to_string(),
    ])
}

// ── Volume reaper ──────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ReapVolumeArgs {
    #[arg(long)]
    pub volume_id: i64,
    /// Project hash (sha256-prefix of workspace root). The reaper only
    /// fires if `state.projects[hash].volume_id` still matches `volume_id`
    /// at wake-up — guards against re-provisions during the sleep.
    #[arg(long)]
    pub project_hash: String,
    #[arg(long)]
    pub after_secs: u64,
}

pub async fn run_volume(args: ReapVolumeArgs) -> Result<()> {
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

    // (1) Project still exists with a matching volume_id?
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
    if project.volume_id != Some(args.volume_id) {
        tracing::info!(
            ours = args.volume_id,
            current = ?project.volume_id,
            hash = %args.project_hash,
            "volume-reaper: state.projects[hash].volume_id no longer matches; bowing out"
        );
        return Ok(());
    }

    // (2) Activity-since-spawn check, this time scoped to *this* project.
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
                    spawn_volume(args.volume_id, args.project_hash.clone(), args.after_secs)
                {
                    tracing::error!("volume-reaper: failed to re-spawn successor: {e}");
                }
                return Ok(());
            }
        }
    }

    // (3) Detach (if attached) then delete. Hetzner refuses to delete a
    // volume that's still attached, so we always detach first. The detach
    // call is idempotent enough that we ignore failures and try the delete
    // anyway — if it really is still attached, `delete_volume` will fail
    // and we'll surface that error.
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;
    if let Err(e) = hcloud.detach_volume(args.volume_id).await {
        tracing::warn!(
            "volume-reaper: detach for volume {} failed (continuing): {e}",
            args.volume_id
        );
    }
    if let Err(e) = hcloud.delete_volume(args.volume_id).await {
        tracing::error!("volume-reaper: failed to delete volume {}: {e}", args.volume_id);
        return Ok(());
    }
    tracing::info!(
        volume = args.volume_id,
        hash = %args.project_hash,
        "volume-reaper: deleted idle project volume"
    );

    // Clear volume_id from per-project state. Re-load under the held lock
    // so we don't clobber concurrent writes; only clear if it still points
    // at the volume we just deleted.
    let mut state = State::load()?;
    if let Some(p) = state.projects.get_mut(&args.project_hash) {
        if p.volume_id == Some(args.volume_id) {
            p.volume_id = None;
            state.save().ok();
        }
    }
    Ok(())
}

/// Spawn a detached child that sleeps then runs the volume-reap-decision
/// flow above.
pub fn spawn_volume(volume_id: i64, project_hash: String, after_secs: u64) -> Result<()> {
    spawn_detached(&[
        "__reap-volume".into(),
        "--volume-id".into(),
        volume_id.to_string(),
        "--project-hash".into(),
        project_hash,
        "--after-secs".into(),
        after_secs.to_string(),
    ])
}

// ── Shared spawn helper ────────────────────────────────────────────────

/// Spawn `cargo-burst <args>` as a detached child, with stdio redirected
/// to /dev/null and a fresh session id (so closing the terminal doesn't
/// SIGHUP it). Used by both reapers; lives here so they share exactly one
/// implementation of the detach mechanics.
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
