//! Append-only JSONL audit log.
//!
//! Records the lifecycle of cargo-burst's cloud resources (server,
//! volumes) and every cargo invocation that runs on them, so the user
//! can later answer questions like "is burst paying for itself?":
//! how many commands per server lifetime, how long was each session,
//! how often is the reaper firing prematurely on a quiet hour, etc.
//!
//! ## Format
//!
//! One JSON object per line, written to `<config_dir>/audit.jsonl`.
//! Append-only (`OpenOptions::append`), never rotated or truncated by
//! cargo-burst itself — log volume is small enough (a handful of
//! events per build) that the user can rotate manually if it ever
//! matters. Each line is self-contained: every event carries a
//! human-readable RFC3339 timestamp, plus the resource ID(s) needed
//! to reconstruct sessions post-hoc.
//!
//! ## Event tagging
//!
//! Events use serde's internally-tagged enum representation with the
//! discriminator field literally named `event`. Concrete shapes for
//! each variant are below. New variants can be added without breaking
//! readers as long as they tolerate unknown discriminators (jq does
//! by default).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use crate::config;

/// Reason a resource was destroyed. Surfaced in the
/// `server_terminated` / `volume_terminated` events so post-hoc
/// analysis can distinguish "reaper retired an idle resource"
/// (working as intended) from "user ran `cargo burst down`" (cost
/// management) from "we cleaned up a corrupted state entry"
/// (something went sideways).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// Idle reaper fired after the configured keep-alive window.
    Reap,
    /// User ran `cargo burst down`.
    Down,
    /// `ensure_shared_server` / `ensure_volume` found a resource ID
    /// in state that pointed at a stopped or vanished resource and
    /// cleaned it up before provisioning a replacement.
    Stale,
    /// Fresh provisioning detected a session for a *different*
    /// resource ID still recorded in state — likely from a previous
    /// run that was killed before its termination event landed.
    /// We emit this so the lifetime accounting still closes.
    Orphaned,
}

/// All audit events. Tagged by the `event` field — `serde(tag = ...)`
/// expands into JSON like `{"event":"server_provisioned", ...}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    ServerProvisioned {
        ts: String,
        server_id: i64,
        server_type: String,
        image_id: i64,
        region: String,
    },
    ServerTerminated {
        ts: String,
        server_id: i64,
        started_at: String,
        ended_at: String,
        lifetime_secs: f64,
        command_count: u32,
        reason: TerminationReason,
    },
    VolumeProvisioned {
        ts: String,
        project_hash: String,
        project_root: String,
        volume_id: i64,
        size_gb: u32,
        region: String,
    },
    VolumeTerminated {
        ts: String,
        project_hash: String,
        project_root: String,
        volume_id: i64,
        started_at: String,
        ended_at: String,
        lifetime_secs: f64,
        reason: TerminationReason,
    },
    Command {
        ts: String,
        server_id: i64,
        project_hash: String,
        /// One of `build`, `test`, `check`, `clippy`, `bench`. We
        /// derive this from the per-subcommand label rather than
        /// parsing cargo args, because the label is already a clean
        /// canonical name.
        verb: String,
        success: bool,
        /// Wall-time breakdown of the run, in seconds. The sum of
        /// these three is `elapsed_secs`. Splitting them out is the
        /// whole point of this telemetry — Bart can answer "how
        /// useful is burst?" by comparing cargo time vs everything
        /// around it, and crucially distinguish cold (`fresh_server`)
        /// runs (where provision dominates) from warm reuses (where
        /// it shouldn't).
        ///
        /// - `provision_secs`: from cargo-burst start to ssh-ready.
        ///   Includes any volume/server API churn + the SSH wait.
        ///   Small (a few seconds) on warm reuses; tens of seconds
        ///   on a cold provision.
        /// - `sync_secs`: mount + rsync of the workspace to the
        ///   remote. Runs in parallel internally; this is wall time
        ///   of the joined future.
        /// - `cargo_secs`: the actual cargo invocation on the remote.
        ///   This is what burst is supposed to make fast.
        provision_secs: f64,
        sync_secs: f64,
        cargo_secs: f64,
        /// Total wall time from cargo-burst start to cargo finish.
        /// Equals `provision_secs + sync_secs + cargo_secs` modulo
        /// rounding; redundant but saves every consumer from
        /// re-summing.
        elapsed_secs: f64,
        /// `true` if this run had to provision a brand-new server
        /// (vs reusing the shared one). Drives the cold/warm split
        /// in any per-command analysis: on `fresh_server=true`, the
        /// large `provision_secs` is amortized over later reuses;
        /// on `fresh_server=false`, provision_secs is just the
        /// per-command overhead of going through cargo-burst.
        fresh_server: bool,
        /// `true` if this run had to create a fresh per-project
        /// volume (first time using burst on this workspace, or
        /// after the volume reaper fired). Tracked separately
        /// because volume creation is independent of server
        /// provisioning — you can have a warm server but a fresh
        /// volume, or vice versa.
        fresh_volume: bool,
    },
}

/// Fields that go into a `command` event. Bundled into a struct so
/// the `record_command` call site reads cleanly and we can grow the
/// shape later without renumbering positional args.
pub struct CommandSample<'a> {
    pub server_id: i64,
    pub project_hash: &'a str,
    pub verb: &'a str,
    pub success: bool,
    pub provision_secs: f64,
    pub sync_secs: f64,
    pub cargo_secs: f64,
    pub fresh_server: bool,
    pub fresh_volume: bool,
}

/// Path to the JSONL audit log. Lives next to `state.json` and
/// `config.toml`. Created on first append.
pub fn audit_log_path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("audit.jsonl"))
}

/// Serialize `event` and append a single line to the audit log.
///
/// Failures are reported up rather than swallowed — call sites that
/// want best-effort behavior (don't fail a real cargo run because the
/// audit log couldn't be written) should `if let Err(e) = ...
/// tracing::warn!(...)` themselves. This module deliberately stays
/// simple and surfaces I/O errors honestly.
pub fn append(event: &Event) -> Result<()> {
    let path = audit_log_path()?;
    let line = serde_json::to_string(event).context("serializing audit event")?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(line.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

/// Best-effort wrapper for `append` — logs a warning on failure
/// instead of returning the error. Used at lifecycle hook sites where
/// we never want a write to the audit log to break a real build.
pub fn append_or_warn(event: &Event) {
    if let Err(e) = append(event) {
        tracing::warn!("failed to write audit event: {e}");
    }
}

/// RFC3339 timestamp at the current moment. Used to stamp every
/// event. Falls back to a "1970-01-01T00:00:00Z" sentinel only if the
/// time crate's formatter rejects the value, which for a well-known
/// format on `now_utc()` should never happen — but we don't want to
/// `unwrap` in audit code.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Compute elapsed seconds between two RFC3339 timestamps. Returns 0.0
/// rather than erroring on bad input — audit accounting should
/// degrade gracefully rather than block a termination event.
pub fn elapsed_secs(started_at: &str, ended_at: &str) -> f64 {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    };
    match (parse(started_at), parse(ended_at)) {
        (Some(a), Some(b)) => (b - a).as_seconds_f64().max(0.0),
        _ => 0.0,
    }
}

// ── Session bookkeeping helpers ───────────────────────────────────────
//
// These bridge the audit log (the user-facing record) and `state.json`
// (the in-flight accounting that lets us close out lifetimes correctly
// even across multiple cargo-burst invocations against the same
// long-lived server).
//
// All four take `&mut State` / `&mut ProjectState` so the caller can
// fold them into an existing `update_state` closure or the
// `ensure_*`-helpers' state mutations without an extra round-trip.

/// Open a new server session: emit `server_provisioned`, record
/// session metadata in state. If a stale session is already present
/// (e.g. previous run was killed before its termination event
/// landed), close it out as `orphaned` first so the lifetime
/// accounting still balances.
pub fn begin_server_session(
    state: &mut config::State,
    server_id: i64,
    server_type: &str,
    image_id: i64,
    region: &str,
) {
    if let Some(prev) = state.current_server_session.take() {
        // Different server than the one we're about to record means
        // we lost track — most likely a previous cargo-burst process
        // died between `delete_server` and writing `state.json`. We
        // can't recover the real `ended_at`; using "now" is the best
        // we can do, and the `orphaned` reason flags it for the user.
        emit_server_terminated(&prev, TerminationReason::Orphaned);
    }
    let now = now_rfc3339();
    append_or_warn(&Event::ServerProvisioned {
        ts: now.clone(),
        server_id,
        server_type: server_type.to_string(),
        image_id,
        region: region.to_string(),
    });
    state.current_server_session = Some(config::ServerSession {
        server_id,
        started_at: now,
        command_count: 0,
    });
}

/// Close the current server session, emitting `server_terminated`
/// with the given reason. No-op if there's no recorded session, or
/// if the recorded session is for a different server (in which case
/// we leave it alone — likely indicates a logic bug; surface via a
/// warning rather than corrupting state).
pub fn end_server_session(
    state: &mut config::State,
    server_id: i64,
    reason: TerminationReason,
) {
    let Some(session) = state.current_server_session.take() else {
        return;
    };
    if session.server_id != server_id {
        tracing::warn!(
            recorded = session.server_id,
            terminating = server_id,
            "current_server_session does not match the server being terminated; \
             leaving session intact"
        );
        state.current_server_session = Some(session);
        return;
    }
    emit_server_terminated(&session, reason);
}

/// Bump the session's command counter and emit a `command` event.
/// No-op on the counter if no session exists (e.g. running against a
/// server we found by name without ever having recorded its
/// provision — `state.json` was wiped between runs). The event is
/// always emitted; only the counter increment is conditional.
pub fn record_command(state: &mut config::State, sample: &CommandSample<'_>) {
    let elapsed_secs = sample.provision_secs + sample.sync_secs + sample.cargo_secs;
    append_or_warn(&Event::Command {
        ts: now_rfc3339(),
        server_id: sample.server_id,
        project_hash: sample.project_hash.to_string(),
        verb: sample.verb.to_string(),
        success: sample.success,
        provision_secs: sample.provision_secs,
        sync_secs: sample.sync_secs,
        cargo_secs: sample.cargo_secs,
        elapsed_secs,
        fresh_server: sample.fresh_server,
        fresh_volume: sample.fresh_volume,
    });
    if let Some(s) = state.current_server_session.as_mut() {
        if s.server_id == sample.server_id {
            s.command_count = s.command_count.saturating_add(1);
        }
    }
}

/// Open a new volume session for `project_hash`: emit
/// `volume_provisioned`, record the start timestamp in
/// `ProjectState`. If a previous start timestamp is still recorded
/// (orphaned from a crash), close it out as `orphaned` first.
pub fn begin_volume_session(
    p: &mut config::ProjectState,
    project_hash: &str,
    volume_id: i64,
    size_gb: u32,
    region: &str,
) {
    if let Some(prev_started) = p.volume_started_at.take() {
        // We don't know the previous volume_id (it's already gone
        // from state by the time `ensure_volume` is creating a new
        // one). Emit an orphaned termination keyed by the new
        // volume_id and the recorded started_at — imperfect but
        // honest about the gap.
        emit_volume_terminated(
            project_hash,
            &p.workspace_path,
            volume_id,
            &prev_started,
            TerminationReason::Orphaned,
        );
    }
    let now = now_rfc3339();
    append_or_warn(&Event::VolumeProvisioned {
        ts: now.clone(),
        project_hash: project_hash.to_string(),
        project_root: p.workspace_path.clone(),
        volume_id,
        size_gb,
        region: region.to_string(),
    });
    p.volume_started_at = Some(now);
}

/// Close the current volume session for `project_hash`. No-op if
/// there's no recorded start (e.g. volume already terminated, or
/// state was cleared between invocations).
pub fn end_volume_session(
    p: &mut config::ProjectState,
    project_hash: &str,
    volume_id: i64,
    reason: TerminationReason,
) {
    let Some(started_at) = p.volume_started_at.take() else {
        return;
    };
    emit_volume_terminated(project_hash, &p.workspace_path, volume_id, &started_at, reason);
}

fn emit_server_terminated(session: &config::ServerSession, reason: TerminationReason) {
    let ended = now_rfc3339();
    append_or_warn(&Event::ServerTerminated {
        ts: ended.clone(),
        server_id: session.server_id,
        started_at: session.started_at.clone(),
        ended_at: ended.clone(),
        lifetime_secs: elapsed_secs(&session.started_at, &ended),
        command_count: session.command_count,
        reason,
    });
}

fn emit_volume_terminated(
    project_hash: &str,
    project_root: &str,
    volume_id: i64,
    started_at: &str,
    reason: TerminationReason,
) {
    let ended = now_rfc3339();
    append_or_warn(&Event::VolumeTerminated {
        ts: ended.clone(),
        project_hash: project_hash.to_string(),
        project_root: project_root.to_string(),
        volume_id,
        started_at: started_at.to_string(),
        ended_at: ended.clone(),
        lifetime_secs: elapsed_secs(started_at, &ended),
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_provisioned_round_trips() {
        let e = Event::ServerProvisioned {
            ts: "2026-05-05T13:00:00Z".into(),
            server_id: 42,
            server_type: "ccx63".into(),
            image_id: 100,
            region: "hel1".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""event":"server_provisioned""#));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::ServerProvisioned { server_id, .. } => assert_eq!(server_id, 42),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn termination_reason_serializes_snake_case() {
        let json = serde_json::to_string(&TerminationReason::Reap).unwrap();
        assert_eq!(json, "\"reap\"");
        let json = serde_json::to_string(&TerminationReason::Orphaned).unwrap();
        assert_eq!(json, "\"orphaned\"");
    }

    #[test]
    fn elapsed_handles_normal_pair() {
        let e = elapsed_secs("2026-05-05T13:00:00Z", "2026-05-05T13:00:30Z");
        assert!((e - 30.0).abs() < 0.001);
    }

    #[test]
    fn elapsed_handles_bad_input() {
        assert_eq!(elapsed_secs("not-a-date", "also-not"), 0.0);
        assert_eq!(elapsed_secs("2026-05-05T13:00:00Z", "garbage"), 0.0);
    }

    #[test]
    fn elapsed_clamps_negative_to_zero() {
        // ended < started shouldn't yield negative; surface as 0.
        let e = elapsed_secs("2026-05-05T13:00:30Z", "2026-05-05T13:00:00Z");
        assert_eq!(e, 0.0);
    }
}
