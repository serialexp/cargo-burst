//! `cargo burst audit` — summarize the JSONL audit log into a
//! readable table.
//!
//! Three angles the user typically wants:
//!
//! 1. **Sessions** — how many server lifetimes were there, average
//!    duration, average commands per session, how each ended.
//! 2. **Cold vs warm** — split commands by `fresh_server`. The cold
//!    column shows what one-time provisioning costs; the warm column
//!    shows what burst's per-command overhead actually is in steady
//!    state.
//! 3. **Aggregate split** — for the entire window, how much wall time
//!    was spent in cargo vs sync vs provision. Answers "where does
//!    the wall time go".
//! 4. **Per-verb** — same split but bucketed by `build`/`test`/
//!    `check`/`clippy`/`bench`. Answers "what's expensive".
//!
//! Numbers in this report are means rather than medians — easier to
//! compute, and with the small samples a typical user will accumulate
//! the difference is rarely material. If we ever care, switching to
//! medians is a one-function change in `Aggregator::format`.
//!
//! No filters in v1; the log is small enough that printing everything
//! is fine.

use anyhow::{Context, Result};
use clap::Args;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::audit::{self, Event, TerminationReason};

#[derive(Args, Debug, Default)]
pub struct AuditArgs {
    /// Hourly server cost in EUR. When provided, the report
    /// includes a cumulative cost section based on total server
    /// lifetime across terminated sessions. Hetzner's monthly cap
    /// equals `hourly_rate × 720h`, so for a CCX63 this is
    /// `monthly_eur / 720` — e.g. €0.520 in hel1/fsn1, €0.871 in
    /// sin. Pass any value to compare what-if costs across regions.
    #[arg(long, value_name = "EUR_PER_HOUR")]
    pub rate: Option<f64>,
}

pub async fn run(args: AuditArgs) -> Result<()> {
    let path = audit::audit_log_path()?;
    let (events, skipped) = match load_events(&path) {
        Ok(e) => e,
        Err(e) => {
            // No log yet → friendly message, not an error. The file
            // doesn't exist until the first cargo-burst command runs.
            if matches!(e.downcast_ref::<std::io::Error>(), Some(io) if io.kind() == std::io::ErrorKind::NotFound)
            {
                println!("No audit log yet at {}.", path.display());
                println!("Run any `cargo burst` command and the log will start populating.");
                return Ok(());
            }
            return Err(e);
        }
    };

    if events.is_empty() {
        println!("Audit log at {} is empty.", path.display());
        return Ok(());
    }

    let stats = compute(&events);
    // Best-effort load of current keep_alive_secs for the what-if
    // model. If config can't be loaded (token missing, file missing,
    // unreadable), the report drops the what-if section gracefully.
    let current_keep_alive = crate::config::Config::load().ok().map(|c| c.keep_alive_secs);
    print_report(&stats, &path, events.len(), skipped, args.rate, current_keep_alive);
    Ok(())
}

/// Load every event in the JSONL log. Lines that fail to parse are
/// counted and reported once at the end — old logs predating a
/// schema change shouldn't break the entire summary, but they also
/// shouldn't spam one warn line per skipped record.
fn load_events(path: &Path) -> Result<(Vec<Event>, usize)> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading line {} of {}", i + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(ev) => out.push(ev),
            Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

#[derive(Default, Debug, Clone, Copy)]
struct PhaseStats {
    count: u32,
    sum_provision: f64,
    sum_sync: f64,
    sum_cargo: f64,
}

impl PhaseStats {
    fn add(&mut self, p: f64, s: f64, c: f64) {
        self.count = self.count.saturating_add(1);
        self.sum_provision += p;
        self.sum_sync += s;
        self.sum_cargo += c;
    }
    fn total(&self) -> f64 {
        self.sum_provision + self.sum_sync + self.sum_cargo
    }
    fn mean_provision(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_provision / self.count as f64 }
    }
    fn mean_sync(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_sync / self.count as f64 }
    }
    fn mean_cargo(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_cargo / self.count as f64 }
    }
    fn mean_total(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.total() / self.count as f64 }
    }
}

#[derive(Default, Debug)]
struct SessionStats {
    provisioned: u32,
    terminated: u32,
    total_lifetime_secs: f64,
    total_commands_in_terminated: u32,
    by_reason: BTreeMap<TerminationReason, u32>,
}

/// Accounting for "we killed a server only to bring it back up shortly
/// after" cycles. Each entry pairs a `ServerTerminated` event with the
/// next `ServerProvisioned` event in time order; the gap between the
/// two is the churn interval. Useful for spotting cases where the
/// keep-alive window is set too aggressively (lots of `reap` →
/// reprovision pairs at small gaps), or where the user is `down`-ing
/// then immediately needing the server back.
///
/// Buckets are inclusive (≤). A 0-second gap counts in all three.
///
/// `reap_*` fields are restricted to terminations whose reason is
/// `Reap`, because the what-if simulator only models the reaper:
/// `down` is user-driven (a longer keep-alive doesn't undo `cargo
/// burst down`), and `stale`/`orphaned` are bookkeeping cleanups that
/// happen independently of the keep-alive timer.
#[derive(Default, Debug)]
struct ChurnStats {
    /// Total `ServerTerminated → next ServerProvisioned` pairs we saw.
    /// Excludes terminations with no following provision (i.e. the
    /// final termination in the log).
    total_pairs: u32,
    within_10m: u32,
    within_30m: u32,
    within_60m: u32,
    /// Per-bucket breakdown by the termination reason, so the user can
    /// tell whether short-gap reprovisions were the reaper firing too
    /// soon vs explicit `down` regrets.
    within_60m_by_reason: BTreeMap<TerminationReason, u32>,
    /// Sum of gaps for the bucketed (≤ 60 min) pairs, used to compute
    /// the mean gap of the close-churn population.
    sum_gap_secs_within_60m: f64,

    /// Reap-only pair total — the population the what-if model
    /// applies to. Down/Stale/Orphaned pairs aren't modeled.
    reap_total_pairs: u32,
    /// Reap-only counts and gap-sums per bucket. `[10m, 30m, 60m]`.
    /// gap-sums are seconds, used to derive extra runtime under a
    /// hypothetical larger keep-alive.
    reap_within: [u32; 3],
    reap_within_sum_gap_secs: [f64; 3],
}

#[derive(Default, Debug)]
struct Stats {
    sessions: SessionStats,
    churn: ChurnStats,
    cold: PhaseStats,
    warm: PhaseStats,
    by_verb: BTreeMap<String, PhaseStats>,
}

/// Pure stats computation over a slice of events. No I/O, no logging.
/// Easy to unit-test by hand-constructing event vectors.
fn compute(events: &[Event]) -> Stats {
    let mut s = Stats::default();
    // Pending = the most-recent ServerTerminated that hasn't yet been
    // paired with a ServerProvisioned. We pair on the very next
    // provision regardless of how distant — the buckets (≤10/30/60m)
    // bin the gap, and pairs whose gap exceeds 60m simply contribute
    // to total_pairs without landing in any bucket.
    let mut pending_term: Option<(String, TerminationReason)> = None;
    for ev in events {
        match ev {
            Event::ServerProvisioned { ts, .. } => {
                s.sessions.provisioned = s.sessions.provisioned.saturating_add(1);
                if let Some((ended_at, reason)) = pending_term.take() {
                    let gap = audit::elapsed_secs(&ended_at, ts);
                    s.churn.total_pairs = s.churn.total_pairs.saturating_add(1);
                    if gap <= 60.0 * 60.0 {
                        s.churn.within_60m = s.churn.within_60m.saturating_add(1);
                        s.churn.sum_gap_secs_within_60m += gap;
                        *s.churn.within_60m_by_reason.entry(reason).or_default() += 1;
                        if gap <= 30.0 * 60.0 {
                            s.churn.within_30m = s.churn.within_30m.saturating_add(1);
                        }
                        if gap <= 10.0 * 60.0 {
                            s.churn.within_10m = s.churn.within_10m.saturating_add(1);
                        }
                    }
                    // Reap-only what-if accounting.
                    if reason == TerminationReason::Reap {
                        s.churn.reap_total_pairs = s.churn.reap_total_pairs.saturating_add(1);
                        let buckets = [10.0 * 60.0, 30.0 * 60.0, 60.0 * 60.0];
                        for (i, &limit) in buckets.iter().enumerate() {
                            if gap <= limit {
                                s.churn.reap_within[i] =
                                    s.churn.reap_within[i].saturating_add(1);
                                s.churn.reap_within_sum_gap_secs[i] += gap;
                            }
                        }
                    }
                }
            }
            Event::ServerTerminated {
                ended_at,
                lifetime_secs,
                command_count,
                reason,
                ..
            } => {
                s.sessions.terminated = s.sessions.terminated.saturating_add(1);
                s.sessions.total_lifetime_secs += lifetime_secs;
                s.sessions.total_commands_in_terminated =
                    s.sessions.total_commands_in_terminated.saturating_add(*command_count);
                *s.sessions.by_reason.entry(*reason).or_default() += 1;
                // Record this termination as the candidate for the
                // next provision pairing. If a previous termination
                // was still pending (i.e. two terminations in a row
                // with no provision between — shouldn't happen on a
                // healthy log but can on a corrupted one), we drop
                // it: the closer-in-time termination is the more
                // honest pairing.
                pending_term = Some((ended_at.clone(), *reason));
            }
            Event::Command {
                verb,
                provision_secs,
                sync_secs,
                cargo_secs,
                fresh_server,
                ..
            } => {
                if *fresh_server {
                    s.cold.add(*provision_secs, *sync_secs, *cargo_secs);
                } else {
                    s.warm.add(*provision_secs, *sync_secs, *cargo_secs);
                }
                s.by_verb
                    .entry(verb.clone())
                    .or_default()
                    .add(*provision_secs, *sync_secs, *cargo_secs);
            }
            // Volume events aren't surfaced in the summary today —
            // they're recorded for future reporting (e.g. "you've
            // had X volumes alive for Y hours"). Skip silently.
            Event::VolumeProvisioned { .. } | Event::VolumeTerminated { .. } => {}
        }
    }
    s
}

/// Format a duration in seconds into the most readable unit. Spends
/// roughly the same number of significant digits across orders of
/// magnitude: under 60s → "12.3s", 1–60min → "12m 34s", 1h+ →
/// "1h 23m". Doesn't attempt sub-second precision because the audit
/// log doesn't either at any meaningful confidence.
fn fmt_secs(secs: f64) -> String {
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// Render either a percentage or a placeholder when the denominator
/// is zero. Used for "X% of total wall time" cells.
fn fmt_pct(num: f64, denom: f64) -> String {
    if denom <= 0.0 { "—".to_string() } else { format!("{:.0}%", 100.0 * num / denom) }
}

/// "  (40%)" or empty when num is zero. Lets the churn report stay
/// quiet for buckets that didn't fire instead of cluttering with "0%".
fn churn_pct_suffix(num: u32, denom: u32) -> String {
    if num == 0 || denom == 0 { String::new() } else { format!("  ({:.0}%)", 100.0 * num as f64 / denom as f64) }
}

/// Compute total server cost given a per-hour rate. Pure function so
/// it's easy to unit-test independently of the report formatter.
fn cost_eur(total_lifetime_secs: f64, rate_per_hour: f64) -> f64 {
    (total_lifetime_secs / 3600.0) * rate_per_hour
}

/// One row of the keep-alive what-if model: "if we'd had keep_alive=T
/// instead of K, what would have happened across all the historical
/// reap→reprovision pairs?"
#[derive(Debug, Clone, Copy, PartialEq)]
struct WhatIf {
    proposed_keep_alive_secs: f64,
    /// Reaps that wouldn't have happened — the next use landed inside
    /// the new keep-alive window, so the server would have stayed up.
    saved_provisions: u32,
    /// Wall-clock saved across those saved provisions, computed as
    /// `saved × (mean_cold_provision - mean_warm_provision)`. Falls
    /// back to 0 if either phase has no samples or the warm mean is
    /// somehow ≥ the cold one (shouldn't happen with real data).
    wall_saved_secs: f64,
    /// Extra server-uptime under the proposal:
    ///   - For each saved pair: gap_i - K (server now lives gap_i
    ///     idle seconds instead of K).
    ///   - For each reap pair the proposal STILL reaps (gap_i > T):
    ///     T - K (reaper fires later).
    /// Values < 0 are clamped to 0 — the proposal is a "longer
    /// keep-alive" so by construction T ≥ K.
    extra_runtime_secs: f64,
}

/// Pure what-if computation: see `WhatIf`. `bucket_idx` selects
/// 0=10m, 1=30m, 2=60m from the per-bucket reap stats. Returns `None`
/// when the proposal isn't actually a relaxation (T ≤ K) — there's
/// nothing useful to say in that direction.
fn what_if_keep_alive(
    churn: &ChurnStats,
    cold: &PhaseStats,
    warm: &PhaseStats,
    current_keep_alive_secs: f64,
    bucket_idx: usize,
) -> Option<WhatIf> {
    let t_secs = match bucket_idx {
        0 => 10.0 * 60.0,
        1 => 30.0 * 60.0,
        2 => 60.0 * 60.0,
        _ => return None,
    };
    if t_secs <= current_keep_alive_secs {
        return None;
    }
    let k = current_keep_alive_secs;
    let saved = churn.reap_within[bucket_idx];
    let saved_sum_gap = churn.reap_within_sum_gap_secs[bucket_idx];
    let unsaved = churn.reap_total_pairs.saturating_sub(saved);

    let provision_delta = (cold.mean_provision() - warm.mean_provision()).max(0.0);
    let wall_saved = saved as f64 * provision_delta;

    // Saved pairs: server stays alive for the whole gap instead of
    // reaping at K, so each contributes (gap - K).
    let saved_extra = (saved_sum_gap - saved as f64 * k).max(0.0);
    // Unsaved pairs: still reap, but later — at T instead of K.
    let unsaved_extra = unsaved as f64 * (t_secs - k);

    Some(WhatIf {
        proposed_keep_alive_secs: t_secs,
        saved_provisions: saved,
        wall_saved_secs: wall_saved,
        extra_runtime_secs: saved_extra + unsaved_extra,
    })
}

fn print_report(
    s: &Stats,
    path: &Path,
    raw_events: usize,
    skipped: usize,
    rate: Option<f64>,
    current_keep_alive_secs: Option<u64>,
) {
    println!("cargo burst audit — {}", path.display());
    if skipped == 0 {
        println!("  ({raw_events} events)");
    } else {
        println!(
            "  ({raw_events} events; {skipped} pre-schema lines skipped — \
             clear the log with `rm {}` to drop them)",
            path.display()
        );
    }
    println!();

    // ── Sessions ───────────────────────────────────────────────────
    println!("Sessions");
    println!("  servers provisioned:    {}", s.sessions.provisioned);
    println!("  servers terminated:     {}", s.sessions.terminated);
    if s.sessions.terminated > 0 {
        let avg_lifetime = s.sessions.total_lifetime_secs / s.sessions.terminated as f64;
        let avg_cmds =
            s.sessions.total_commands_in_terminated as f64 / s.sessions.terminated as f64;
        println!("  total server lifetime:  {}", fmt_secs(s.sessions.total_lifetime_secs));
        println!("  avg lifetime / session: {}", fmt_secs(avg_lifetime));
        println!("  avg commands / session: {avg_cmds:.1}");
        if !s.sessions.by_reason.is_empty() {
            let parts: Vec<String> = s
                .sessions
                .by_reason
                .iter()
                .map(|(r, n)| format!("{r:?}={n}"))
                .collect();
            println!("  closed by reason:       {}", parts.join("  "));
        }
    }
    println!();

    // ── Server churn ───────────────────────────────────────────────
    // "How often did we kill a server only to bring it back up shortly
    // after?" — short-gap reprovisions (especially after `reap`) are a
    // signal the keep-alive window is set too aggressively. Skipped
    // when the log has fewer than two server lifetimes' worth of data
    // — there's nothing meaningful to say yet.
    if s.churn.total_pairs > 0 {
        println!("Server churn (terminate → next provision)");
        println!("  reprovisioned after a kill: {} times", s.churn.total_pairs);
        println!(
            "    within 10 min:  {}{}",
            s.churn.within_10m,
            churn_pct_suffix(s.churn.within_10m, s.churn.total_pairs),
        );
        println!(
            "    within 30 min:  {}{}",
            s.churn.within_30m,
            churn_pct_suffix(s.churn.within_30m, s.churn.total_pairs),
        );
        println!(
            "    within 60 min:  {}{}",
            s.churn.within_60m,
            churn_pct_suffix(s.churn.within_60m, s.churn.total_pairs),
        );
        if s.churn.within_60m > 0 {
            let mean_gap = s.churn.sum_gap_secs_within_60m / s.churn.within_60m as f64;
            println!("  mean gap (≤60m pairs): {}", fmt_secs(mean_gap));
            if !s.churn.within_60m_by_reason.is_empty() {
                let parts: Vec<String> = s
                    .churn
                    .within_60m_by_reason
                    .iter()
                    .map(|(r, n)| format!("{r:?}={n}"))
                    .collect();
                println!("  ≤60m by termination reason: {}", parts.join("  "));
            }
        }
        println!();
    }

    // ── What-if: longer keep-alive ─────────────────────────────────
    // Only worth emitting if (a) we know the current K, (b) there's at
    // least one Reap pair to model against, and (c) we have both cold
    // and warm samples so the wall-clock-saved column is meaningful.
    if let Some(current_k) = current_keep_alive_secs
        && s.churn.reap_total_pairs > 0
        && s.cold.count > 0
        && s.warm.count > 0
    {
        let k_secs = current_k as f64;
        println!("Keep-alive what-if (current keep_alive_secs = {current_k})");
        println!(
            "  Models only Reap pairs ({} of {} total churn pairs); Down/Stale/Orphaned",
            s.churn.reap_total_pairs, s.churn.total_pairs,
        );
        println!("  terminations would happen identically under any keep-alive.");
        println!();
        let header = if rate.is_some() {
            format!(
                "  {:<10} {:>8}  {:>11}  {:>13}  {:>10}  {:>10}",
                "proposed", "saved", "wall saved", "extra runtime", "extra €", "€/h saved"
            )
        } else {
            format!(
                "  {:<10} {:>8}  {:>11}  {:>13}",
                "proposed", "saved", "wall saved", "extra runtime"
            )
        };
        println!("{header}");
        let mut any = false;
        for (idx, label) in [(0, "10 min"), (1, "30 min"), (2, "60 min")] {
            let Some(w) = what_if_keep_alive(&s.churn, &s.cold, &s.warm, k_secs, idx) else {
                continue;
            };
            any = true;
            if let Some(rate) = rate {
                let extra_eur = cost_eur(w.extra_runtime_secs, rate);
                // Wall-clock saved per euro spent — converted to "€/h
                // of saved wall time". Higher = better deal. Falls
                // back to "—" when extra cost is zero (free win).
                let eff = if extra_eur > 0.0 {
                    let saved_h = w.wall_saved_secs / 3600.0;
                    if saved_h > 0.0 {
                        format!("€{:.4}", extra_eur / saved_h)
                    } else {
                        "—".to_string()
                    }
                } else if w.wall_saved_secs > 0.0 {
                    "free".to_string()
                } else {
                    "—".to_string()
                };
                println!(
                    "  {:<10} {:>8}  {:>11}  {:>13}  {:>10}  {:>10}",
                    label,
                    w.saved_provisions,
                    fmt_secs(w.wall_saved_secs),
                    fmt_secs(w.extra_runtime_secs),
                    format!("€{extra_eur:.4}"),
                    eff,
                );
            } else {
                println!(
                    "  {:<10} {:>8}  {:>11}  {:>13}",
                    label,
                    w.saved_provisions,
                    fmt_secs(w.wall_saved_secs),
                    fmt_secs(w.extra_runtime_secs),
                );
            }
        }
        if !any {
            println!(
                "  (current keep_alive_secs of {current_k}s already covers all the modeled buckets)"
            );
        }
        println!();
    }

    // ── Cost ──────────────────────────────────────────────────────
    // Only printed when the user passes `--rate`. Hetzner bills per
    // second up to a monthly cap of (hourly × 720h), so for short
    // runs `total_lifetime / 3600 × rate` is exact; only sustained
    // 24/7 use would hit the cap. We're well below that.
    if let Some(rate) = rate {
        println!("Cost (at €{rate:.4}/h)");
        println!("  total runtime: {}", fmt_secs(s.sessions.total_lifetime_secs));
        let cost = cost_eur(s.sessions.total_lifetime_secs, rate);
        println!("  cost so far:   €{cost:.4}");
        println!();
    }

    // ── Cold vs warm ───────────────────────────────────────────────
    println!("Commands by server state");
    println!(
        "  {:<10} {:>6}  {:>10}  {:>10}  {:>12}  {:>10}",
        "", "count", "provision", "sync", "cargo", "total"
    );
    print_phase_row("  cold:", &s.cold);
    print_phase_row("  warm:", &s.warm);
    println!();

    // ── Aggregate split ────────────────────────────────────────────
    let total = s.cold.total() + s.warm.total();
    let agg_provision = s.cold.sum_provision + s.warm.sum_provision;
    let agg_sync = s.cold.sum_sync + s.warm.sum_sync;
    let agg_cargo = s.cold.sum_cargo + s.warm.sum_cargo;
    println!("Where the wall time went");
    if total > 0.0 {
        println!(
            "  provision: {:>10}  ({})",
            fmt_secs(agg_provision),
            fmt_pct(agg_provision, total)
        );
        println!(
            "  sync:      {:>10}  ({})",
            fmt_secs(agg_sync),
            fmt_pct(agg_sync, total)
        );
        println!(
            "  cargo:     {:>10}  ({})",
            fmt_secs(agg_cargo),
            fmt_pct(agg_cargo, total)
        );
        println!("  ────────────────────────");
        println!("  total:     {:>10}", fmt_secs(total));
    } else {
        println!("  (no command events yet)");
    }
    println!();

    // ── Per-verb ──────────────────────────────────────────────────
    if !s.by_verb.is_empty() {
        println!("Per-verb (means)");
        println!(
            "  {:<8} {:>6}  {:>10}  {:>10}  {:>10}  {:>10}",
            "verb", "count", "provision", "sync", "cargo", "total"
        );
        for (verb, p) in &s.by_verb {
            println!(
                "  {:<8} {:>6}  {:>10}  {:>10}  {:>10}  {:>10}",
                verb,
                p.count,
                fmt_secs(p.mean_provision()),
                fmt_secs(p.mean_sync()),
                fmt_secs(p.mean_cargo()),
                fmt_secs(p.mean_total()),
            );
        }
    }
}

fn print_phase_row(label: &str, p: &PhaseStats) {
    if p.count == 0 {
        println!("  {:<10} {:>6}  {:>10}  {:>10}  {:>12}  {:>10}", label, 0, "—", "—", "—", "—");
        return;
    }
    println!(
        "  {:<10} {:>6}  {:>10}  {:>10}  {:>12}  {:>10}",
        label,
        p.count,
        fmt_secs(p.mean_provision()),
        fmt_secs(p.mean_sync()),
        fmt_secs(p.mean_cargo()),
        fmt_secs(p.mean_total()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::provider::{ImageId, ServerId};

    fn cmd(verb: &str, fresh: bool, p: f64, s: f64, c: f64) -> Event {
        Event::Command {
            ts: "2026-05-06T00:00:00Z".into(),
            server_id: ServerId("1".into()),
            provider: "hetzner".into(),
            project_hash: "h".into(),
            verb: verb.into(),
            success: true,
            provision_secs: p,
            sync_secs: s,
            cargo_secs: c,
            elapsed_secs: p + s + c,
            fresh_server: fresh,
            fresh_volume: false,
        }
    }

    fn term(secs: f64, n: u32, r: TerminationReason) -> Event {
        Event::ServerTerminated {
            ts: "2026-05-06T00:00:00Z".into(),
            server_id: ServerId("1".into()),
            provider: "hetzner".into(),
            started_at: "2026-05-06T00:00:00Z".into(),
            ended_at: "2026-05-06T00:00:00Z".into(),
            lifetime_secs: secs,
            command_count: n,
            reason: r,
        }
    }

    fn term_at(ended_at: &str, r: TerminationReason) -> Event {
        Event::ServerTerminated {
            ts: ended_at.into(),
            server_id: ServerId("1".into()),
            provider: "hetzner".into(),
            started_at: "2026-05-06T00:00:00Z".into(),
            ended_at: ended_at.into(),
            lifetime_secs: 0.0,
            command_count: 0,
            reason: r,
        }
    }

    fn prov_at(ts: &str) -> Event {
        Event::ServerProvisioned {
            ts: ts.into(),
            server_id: ServerId("1".into()),
            provider: "hetzner".into(),
            server_type: "ccx63".into(),
            image_id: ImageId("100".into()),
            region: "hel1".into(),
        }
    }

    #[test]
    fn cold_warm_split() {
        let evs = vec![
            cmd("build", true, 30.0, 5.0, 10.0),  // cold
            cmd("build", false, 4.0, 5.0, 8.0),    // warm
            cmd("check", false, 4.0, 5.0, 3.0),    // warm
        ];
        let s = compute(&evs);
        assert_eq!(s.cold.count, 1);
        assert_eq!(s.warm.count, 2);
        assert!((s.cold.mean_provision() - 30.0).abs() < 1e-6);
        assert!((s.warm.mean_cargo() - 5.5).abs() < 1e-6);
    }

    #[test]
    fn per_verb_buckets() {
        let evs = vec![
            cmd("build", false, 5.0, 5.0, 30.0),
            cmd("build", false, 5.0, 5.0, 20.0),
            cmd("test", false, 5.0, 5.0, 60.0),
        ];
        let s = compute(&evs);
        assert_eq!(s.by_verb.get("build").unwrap().count, 2);
        assert_eq!(s.by_verb.get("test").unwrap().count, 1);
        let build_mean_cargo = s.by_verb.get("build").unwrap().mean_cargo();
        assert!((build_mean_cargo - 25.0).abs() < 1e-6);
    }

    #[test]
    fn session_stats_aggregate() {
        let evs = vec![
            Event::ServerProvisioned {
                ts: "2026-05-06T00:00:00Z".into(),
                server_id: ServerId("1".into()),
                provider: "hetzner".into(),
                server_type: "ccx63".into(),
                image_id: ImageId("100".into()),
                region: "hel1".into(),
            },
            term(120.0, 3, TerminationReason::Reap),
            term(60.0, 1, TerminationReason::Down),
        ];
        let s = compute(&evs);
        assert_eq!(s.sessions.provisioned, 1);
        assert_eq!(s.sessions.terminated, 2);
        assert!((s.sessions.total_lifetime_secs - 180.0).abs() < 1e-6);
        assert_eq!(s.sessions.total_commands_in_terminated, 4);
        assert_eq!(s.sessions.by_reason.get(&TerminationReason::Reap), Some(&1));
        assert_eq!(s.sessions.by_reason.get(&TerminationReason::Down), Some(&1));
    }

    #[test]
    fn fmt_secs_picks_unit() {
        assert_eq!(fmt_secs(12.3), "12.3s");
        assert_eq!(fmt_secs(125.0), "2m 05s");
        assert_eq!(fmt_secs(3725.0), "1h 02m");
    }

    #[test]
    fn fmt_pct_handles_zero_denom() {
        assert_eq!(fmt_pct(0.0, 0.0), "—");
        assert_eq!(fmt_pct(50.0, 100.0), "50%");
    }

    #[test]
    fn churn_buckets_are_inclusive_and_nested() {
        // Three terminate→reprovision pairs:
        //   1. 5min gap   → counts in 10/30/60
        //   2. 20min gap  → counts in 30/60
        //   3. 45min gap  → counts in 60 only
        let evs = vec![
            prov_at("2026-05-06T00:00:00Z"),
            term_at("2026-05-06T00:10:00Z", TerminationReason::Reap),
            prov_at("2026-05-06T00:15:00Z"),                    // 5m
            term_at("2026-05-06T00:20:00Z", TerminationReason::Reap),
            prov_at("2026-05-06T00:40:00Z"),                    // 20m
            term_at("2026-05-06T00:50:00Z", TerminationReason::Down),
            prov_at("2026-05-06T01:35:00Z"),                    // 45m
        ];
        let s = compute(&evs);
        assert_eq!(s.churn.total_pairs, 3);
        assert_eq!(s.churn.within_10m, 1);
        assert_eq!(s.churn.within_30m, 2);
        assert_eq!(s.churn.within_60m, 3);
        // Mean gap of the ≤60m population = (5+20+45)/3 = 23.333 min
        let mean_min = s.churn.sum_gap_secs_within_60m / s.churn.within_60m as f64 / 60.0;
        assert!((mean_min - (5.0 + 20.0 + 45.0) / 3.0).abs() < 1e-6);
        // Reasons: two reap, one down within the ≤60m bucket.
        assert_eq!(s.churn.within_60m_by_reason.get(&TerminationReason::Reap), Some(&2));
        assert_eq!(s.churn.within_60m_by_reason.get(&TerminationReason::Down), Some(&1));
    }

    #[test]
    fn churn_above_60m_counts_pair_but_no_bucket() {
        let evs = vec![
            prov_at("2026-05-06T00:00:00Z"),
            term_at("2026-05-06T00:10:00Z", TerminationReason::Reap),
            prov_at("2026-05-06T03:00:00Z"),                    // ~170m later
        ];
        let s = compute(&evs);
        assert_eq!(s.churn.total_pairs, 1);
        assert_eq!(s.churn.within_10m, 0);
        assert_eq!(s.churn.within_30m, 0);
        assert_eq!(s.churn.within_60m, 0);
        assert_eq!(s.churn.sum_gap_secs_within_60m, 0.0);
    }

    #[test]
    fn churn_no_pairs_when_log_ends_on_termination() {
        let evs = vec![
            prov_at("2026-05-06T00:00:00Z"),
            term_at("2026-05-06T00:10:00Z", TerminationReason::Reap),
        ];
        let s = compute(&evs);
        assert_eq!(s.churn.total_pairs, 0);
    }

    #[test]
    fn churn_first_provision_isnt_paired() {
        // Provision-only log (no preceding terminate) doesn't manufacture a pair.
        let evs = vec![prov_at("2026-05-06T00:00:00Z")];
        let s = compute(&evs);
        assert_eq!(s.churn.total_pairs, 0);
    }

    fn phase(count: u32, p: f64) -> PhaseStats {
        // Synthesize a PhaseStats whose mean_provision is `p`.
        PhaseStats {
            count,
            sum_provision: p * count as f64,
            sum_sync: 0.0,
            sum_cargo: 0.0,
        }
    }

    #[test]
    fn what_if_skipped_when_proposal_not_a_relaxation() {
        let churn = ChurnStats {
            reap_total_pairs: 5,
            reap_within: [3, 4, 5],
            reap_within_sum_gap_secs: [600.0, 1200.0, 1800.0],
            ..Default::default()
        };
        let cold = phase(1, 30.0);
        let warm = phase(1, 3.0);
        // Current K = 15 min — 10m bucket isn't a relaxation, return None.
        assert!(what_if_keep_alive(&churn, &cold, &warm, 15.0 * 60.0, 0).is_none());
        // 30m and 60m still are.
        assert!(what_if_keep_alive(&churn, &cold, &warm, 15.0 * 60.0, 1).is_some());
        assert!(what_if_keep_alive(&churn, &cold, &warm, 15.0 * 60.0, 2).is_some());
    }

    #[test]
    fn what_if_savings_and_runtime_math() {
        // Current K = 5 min (300s). Three reap pairs, gaps 6m, 20m, 50m.
        // For T=10m: only the 6m pair is "saved". Other two reap at 10m
        //            instead of 5m → +5m each.
        //            Saved-pair extra runtime = 6m - 5m = 1m.
        //            Total extra = 1m + 2 × 5m = 11m.
        //            Saved provisions = 1; wall_saved = 1 × (30s - 3s) = 27s.
        let churn = ChurnStats {
            reap_total_pairs: 3,
            reap_within: [1, 2, 3],
            reap_within_sum_gap_secs: [6.0 * 60.0, (6.0 + 20.0) * 60.0, (6.0 + 20.0 + 50.0) * 60.0],
            ..Default::default()
        };
        let cold = phase(1, 30.0);
        let warm = phase(1, 3.0);
        let k = 5.0 * 60.0;

        let w10 = what_if_keep_alive(&churn, &cold, &warm, k, 0).unwrap();
        assert_eq!(w10.saved_provisions, 1);
        assert!((w10.wall_saved_secs - 27.0).abs() < 1e-6);
        assert!((w10.extra_runtime_secs - 11.0 * 60.0).abs() < 1e-6);

        // T=30m: pairs 6m and 20m are saved (gaps 6m+20m=26m of idle).
        //   saved_extra = 26m - 2 × 5m = 16m
        //   unsaved (the 50m pair) = 30m - 5m = 25m
        //   total extra = 16m + 25m = 41m
        //   wall saved = 2 × 27 = 54s
        let w30 = what_if_keep_alive(&churn, &cold, &warm, k, 1).unwrap();
        assert_eq!(w30.saved_provisions, 2);
        assert!((w30.wall_saved_secs - 54.0).abs() < 1e-6);
        assert!((w30.extra_runtime_secs - 41.0 * 60.0).abs() < 1e-6);

        // T=60m: all three saved, gap sum 76m, saved_extra = 76 - 3×5 = 61m,
        //   no unsaved → 61m total, wall saved = 3 × 27 = 81s.
        let w60 = what_if_keep_alive(&churn, &cold, &warm, k, 2).unwrap();
        assert_eq!(w60.saved_provisions, 3);
        assert!((w60.wall_saved_secs - 81.0).abs() < 1e-6);
        assert!((w60.extra_runtime_secs - 61.0 * 60.0).abs() < 1e-6);
    }

    #[test]
    fn what_if_clamps_provision_delta_at_zero() {
        // Pathological: warm provision somehow ≥ cold (no real run will
        // produce this, but we mustn't emit negative wall_saved).
        let churn = ChurnStats {
            reap_total_pairs: 1,
            reap_within: [1, 1, 1],
            reap_within_sum_gap_secs: [400.0, 400.0, 400.0],
            ..Default::default()
        };
        let cold = phase(1, 5.0);
        let warm = phase(1, 10.0);
        let w = what_if_keep_alive(&churn, &cold, &warm, 300.0, 0).unwrap();
        assert_eq!(w.wall_saved_secs, 0.0);
    }

    #[test]
    fn cost_eur_basic() {
        // 1 hour at €0.520/h = €0.520
        assert!((cost_eur(3600.0, 0.520) - 0.520).abs() < 1e-9);
        // 7m 15s = 435s at €0.520/h ≈ €0.06283
        assert!((cost_eur(435.0, 0.520) - 0.06283333).abs() < 1e-6);
        // Same runtime in Singapore (€0.871/h) ≈ €0.10525
        assert!((cost_eur(435.0, 0.871) - 0.10524583).abs() < 1e-6);
        // Zero runtime → zero cost regardless of rate
        assert_eq!(cost_eur(0.0, 0.871), 0.0);
    }
}
