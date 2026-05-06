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
    print_report(&stats, &path, events.len(), skipped, args.rate);
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

#[derive(Default, Debug)]
struct Stats {
    sessions: SessionStats,
    cold: PhaseStats,
    warm: PhaseStats,
    by_verb: BTreeMap<String, PhaseStats>,
}

/// Pure stats computation over a slice of events. No I/O, no logging.
/// Easy to unit-test by hand-constructing event vectors.
fn compute(events: &[Event]) -> Stats {
    let mut s = Stats::default();
    for ev in events {
        match ev {
            Event::ServerProvisioned { .. } => {
                s.sessions.provisioned = s.sessions.provisioned.saturating_add(1);
            }
            Event::ServerTerminated {
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

/// Compute total server cost given a per-hour rate. Pure function so
/// it's easy to unit-test independently of the report formatter.
fn cost_eur(total_lifetime_secs: f64, rate_per_hour: f64) -> f64 {
    (total_lifetime_secs / 3600.0) * rate_per_hour
}

fn print_report(s: &Stats, path: &Path, raw_events: usize, skipped: usize, rate: Option<f64>) {
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

    fn cmd(verb: &str, fresh: bool, p: f64, s: f64, c: f64) -> Event {
        Event::Command {
            ts: "2026-05-06T00:00:00Z".into(),
            server_id: 1,
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
            server_id: 1,
            started_at: "2026-05-06T00:00:00Z".into(),
            ended_at: "2026-05-06T00:00:00Z".into(),
            lifetime_secs: secs,
            command_count: n,
            reason: r,
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
                server_id: 1,
                server_type: "ccx63".into(),
                image_id: 100,
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
