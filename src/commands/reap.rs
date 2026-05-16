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

    // Lock discipline: the state lock guards `state.json`, nothing else.
    // It must NEVER be held across AWS / provider API calls — those can
    // stall on retries against a torn-down instance and would deadlock
    // every concurrent `cargo burst` invocation. Every block below that
    // takes the lock either reads-and-decides or writes-and-returns;
    // none of them straddle a provider call.

    // (1) Snapshot the bits we need under the lock, then drop it.
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("server-reaper: failed to load config ({e}); leaving server alive");
            return Ok(());
        }
    };
    enum InitialDecision {
        Bow,
        Respawn,
        Proceed {
            session_started_at: Option<String>,
        },
    }
    let decision = {
        let _lock = StateLock::acquire().await?;
        let state = match State::load() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    "server-reaper: failed to load state ({e}); leaving server alive"
                );
                return Ok(());
            }
        };
        if state.server_id.as_ref() != Some(&server_id) {
            tracing::info!(
                ours = %server_id,
                current = ?state.server_id,
                "server-reaper: state.server_id no longer matches; bowing out"
            );
            InitialDecision::Bow
        } else if state
            .last_used_any()
            .is_some_and(|last| last > spawn_time)
        {
            tracing::info!(
                last_used = ?state.last_used_any(),
                spawn = %spawn_time,
                "server-reaper: activity since spawn; re-spawning successor"
            );
            InitialDecision::Respawn
        } else {
            let session_started_at = state
                .current_server_session
                .as_ref()
                .filter(|s| s.server_id == server_id)
                .map(|s| s.started_at.clone());
            InitialDecision::Proceed { session_started_at }
        }
        // _lock drops here.
    };
    let session_started_at = match decision {
        InitialDecision::Bow => return Ok(()),
        InitialDecision::Respawn => {
            if let Err(e) = spawn_server(server_id, args.after_secs) {
                tracing::error!("server-reaper: failed to re-spawn successor: {e}");
            }
            return Ok(());
        }
        InitialDecision::Proceed { session_started_at } => session_started_at,
    };

    // (2) Provider construction — outside the lock. SDK init can do
    // credential resolution / network calls.
    let provider = match provider::from_config(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("server-reaper: provider init failed ({e}); leaving server alive");
            return Ok(());
        }
    };

    // (3) Billing-minimum gate — also outside the lock. We only sleep.
    let billing_min = provider.billing_minimum();
    if billing_min > Duration::from_secs(0) {
        if let Some(started_at) = session_started_at.as_deref() {
            if let Ok(created) = time::OffsetDateTime::parse(
                started_at,
                &time::format_description::well_known::Rfc3339,
            ) {
                let now = time::OffsetDateTime::now_utc();
                let earliest = created
                    + time::Duration::seconds(billing_min.as_secs() as i64)
                    - time::Duration::seconds(BILLING_SAFETY_MARGIN.as_secs() as i64);
                if now < earliest {
                    let wait_secs = (earliest - now).whole_seconds().max(0) as u64;
                    tracing::info!(
                        "server-reaper: holding server until billing minimum elapses \
                         ({wait_secs}s remaining of paid window)"
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                    // Re-check activity after the extra sleep. If anyone
                    // used the server during the billing wait, hand off
                    // to a successor rather than killing it.
                    let respawn = {
                        let _lock = StateLock::acquire().await?;
                        let state2 = State::load()?;
                        if state2.server_id.as_ref() != Some(&server_id) {
                            return Ok(());
                        }
                        state2.last_used_any().is_some_and(|last| last > spawn_time)
                    };
                    if respawn {
                        if let Err(e) = spawn_server(server_id, args.after_secs) {
                            tracing::error!(
                                "server-reaper: failed to re-spawn after billing wait: {e}"
                            );
                        }
                        return Ok(());
                    }
                }
            }
        }
    }

    // (4) Delete — outside the lock. AWS retries can take many seconds.
    if let Err(e) = provider.delete_server(&server_id).await {
        tracing::error!("server-reaper: failed to delete server {server_id}: {e}");
        return Ok(());
    }
    tracing::info!("server-reaper: deleted shared server {server_id}");

    // (5) Final state mutation under a fresh, short lock acquisition.
    {
        let _lock = StateLock::acquire().await?;
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
    }
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

    // Same lock discipline as run_server: lock only for state.json
    // reads/writes, never across provider API calls.
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("volume-reaper: failed to load config ({e}); leaving volume alive");
            return Ok(());
        }
    };
    enum InitialDecision {
        Bow,
        Respawn,
        Proceed,
    }
    let decision = {
        let _lock = StateLock::acquire().await?;
        let state = match State::load() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    "volume-reaper: failed to load state ({e}); leaving volume alive"
                );
                return Ok(());
            }
        };
        let Some(project) = state.projects.get(&args.project_hash) else {
            tracing::info!(
                hash = %args.project_hash,
                "volume-reaper: project no longer in state; bowing out"
            );
            return Ok(());
        };
        if project.volume_id.as_ref() != Some(&volume_id) {
            tracing::info!(
                ours = %volume_id,
                current = ?project.volume_id,
                hash = %args.project_hash,
                "volume-reaper: state.projects[hash].volume_id no longer matches; bowing out"
            );
            InitialDecision::Bow
        } else if project
            .last_used_rfc3339
            .as_deref()
            .and_then(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .is_some_and(|parsed| parsed > spawn_time)
        {
            tracing::info!(
                hash = %args.project_hash,
                "volume-reaper: activity since spawn; re-spawning successor"
            );
            InitialDecision::Respawn
        } else {
            InitialDecision::Proceed
        }
        // _lock drops here.
    };
    match decision {
        InitialDecision::Bow => return Ok(()),
        InitialDecision::Respawn => {
            if let Err(e) =
                spawn_volume(volume_id, args.project_hash.clone(), args.after_secs)
            {
                tracing::error!("volume-reaper: failed to re-spawn successor: {e}");
            }
            return Ok(());
        }
        InitialDecision::Proceed => {}
    }

    // Provider init + AWS calls — outside the lock.
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

    // Final state write under a fresh, short lock acquisition.
    let provider_name = provider.name();
    {
        let _lock = StateLock::acquire().await?;
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
