//! Shared remote-build machinery used by `build` and `test`.
//!
//! Everything that's identical between "run a cargo build on the server"
//! and "run a cargo test on the server" lives here:
//!
//! - State-locked provisioning (volume, shared server, attach).
//! - First-run exclude prompt + size summary.
//! - SSH wait, mount-ensure, rsync (parallelised).
//! - Heartbeat task that keeps the per-project `last_used_rfc3339`
//!   fresh during the long cargo phase.
//! - Post-cargo cleanup: heartbeat shutdown, final `last_used` bump,
//!   server + volume reaper spawns.
//!
//! The only thing that differs between `build` and `test` is *what
//! cargo command(s) get run* against the prepared remote — and that's
//! exactly the closure each subcommand passes into `with_remote`.

use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::commands::reap;
use crate::config::{self, Config, ProjectState, State, StateLock};
use crate::project::{ProjectKey, SHARED_SERVER_NAME};
use crate::provider::{
    self, Provider, ProvisionError, ServerInfo, ServerSpec, SshKeyId, VolumeInfo,
    VolumeSpec,
};
use crate::ssh;

/// How often the heartbeat task bumps `last_used_rfc3339` while a build
/// or test run is in progress. Must be << `keep_alive_secs` so a previous
/// reaper, when it wakes mid-current-run, sees a fresh timestamp and
/// bows out instead of deleting the server out from under us.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// SSH come-up window after the server reports "running".
const SSH_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a `server_provisioning_started_at` marker is treated as
/// "still in flight" by waiters. If the claimant takes longer than this
/// without writing a `server_id` (probably crashed), the next arrival
/// steals the slot rather than waiting forever. Sized to be comfortably
/// larger than a normal provision (~25s on AWS, ~50s on Hetzner) so we
/// don't false-positive on a slightly-slow run.
const PROVISIONING_STALE_AFTER_SECS: i64 = 180;

/// How often a waiter re-checks `state.json` for either `server_id`
/// becoming populated or the provisioning marker going stale.
const PROVISIONING_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What a subcommand needs in order to talk to the prepared remote.
/// Passed into the closure given to [`with_remote`].
pub struct RemoteCtx {
    pub server_ip: String,
    pub ssh_key_path: PathBuf,
    /// Local workspace root — the dir cargo-burst was invoked from
    /// (or its workspace ancestor). Closures use this to drop fetched
    /// artifacts into the local `target/` tree.
    pub workspace_root: PathBuf,
    /// `/home/work/src/<hash>/` — where the rsync'd source landed.
    pub remote_src: String,
    /// `/mnt/cache/<hash>/target` — `CARGO_TARGET_DIR` for the run.
    pub target_dir: String,
    /// `/mnt/cache/<hash>/sccache` — `SCCACHE_DIR` for the run.
    pub sccache_dir: String,
    /// Resolved environment variables to export ahead of every cargo
    /// invocation. Already merged across global config, project
    /// config, and CLI `--env` flags by the time the closure sees it
    /// (CLI wins, project additive over global). Names here always
    /// have a concrete value — entries that resolved to "no value"
    /// were filtered out.
    pub env: Vec<(String, String)>,
}

/// Knobs callers expose to users (mirrored on every subcommand that
/// drives a remote run).
pub struct RemoteOptions {
    /// Override `Config.keep_alive_secs` for the server reaper.
    pub keep_alive: Option<u64>,
    /// Skip the first-run "apply suggested excludes?" prompt.
    pub yes: bool,
    /// Don't schedule reapers — leave the server (and volume) alive
    /// indefinitely.
    pub no_reap: bool,
    /// Raw `--env` flag values from the CLI (`VAR` or `VAR=value`).
    /// Resolved against the local environment inside `with_remote`,
    /// then merged with `Config.forward_env`.
    pub cli_env: Vec<String>,
}

/// Run `f` on a prepared remote.
///
/// Provisions the shared server + this project's volume, mounts the
/// volume, rsyncs source, runs the heartbeat, calls `f` (which is
/// expected to drive whatever cargo command(s) make sense for the
/// subcommand), then cleans up: shuts the heartbeat, bumps `last_used`
/// one final time, spawns the reapers (unless `no_reap`), prints a
/// summary line, and finally propagates `f`'s result.
///
/// `label` is the human-readable name shown in the summary (e.g.
/// `"Build"` or `"Tests"`).
///
/// Crucially the cleanup runs *whether `f` succeeded or not*, so a
/// failed cargo invocation still leaves a healthy reaper schedule
/// behind. The caller's error (if any) is propagated only after that.
pub async fn with_remote<F, Fut>(
    opts: RemoteOptions,
    label: &str,
    f: F,
) -> Result<()>
where
    F: FnOnce(RemoteCtx) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    // Refuse to run if stdout is piped into a tool that buffers
    // everything until EOF (`tail -60`, `wc`, `sort`, etc.). Those
    // consumers — common in agent invocations bounding context — hide
    // every byte cargo-burst emits during the run, making a normal
    // multi-minute build look identical to a hang. Fail fast in <100ms
    // with a message that explains the workaround; tail's window will
    // flush our error when we exit, so even an agent reading through
    // `| tail -60` ends up seeing what went wrong. Linux-only; on
    // macOS the detection no-ops and we proceed unchanged.
    if let Some(consumer) = crate::pipe_guard::detect_buffering_pipe_consumer() {
        return Err(anyhow!(crate::pipe_guard::buffering_pipe_error(&consumer)));
    }

    let cwd = std::env::current_dir().context("getting current dir")?;
    let project = ProjectKey::discover(&cwd)?;
    tracing::info!(
        workspace = %project.workspace_root.display(),
        hash = %project.hash,
        "resolved cargo workspace"
    );

    // Load global config and layer the project's
    // `.config/cargo-burst.toml` on top, if present. This has to
    // happen *after* workspace discovery (we need the path) but
    // before anything that consumes config — most notably the HCloud
    // client (whose token comes from config) and the env-resolution
    // step below (which reads `cfg.forward_env`).
    let cfg = Config::load_for_workspace(Some(&project.workspace_root))?;
    let provider = provider::from_config(&cfg).await?;

    // Resolve the merged environment now so it's a flat
    // `Vec<(name, value)>` by the time `build_remote_cmd` consumes
    // it. Local env lookups happen here, on the host that actually
    // has the values — moving this work into `build_remote_cmd`
    // would mean re-reading `std::env` for every cargo phase even
    // though nothing about it changes mid-run.
    let env = resolve_env(&cfg.forward_env, &opts.cli_env);

    // Wall-time anchor for the audit log's `provision_secs` — we want
    // to attribute "everything before cargo runs" to provisioning, so
    // we start the timer at the very top of `with_remote` and stop it
    // after `wait_for_ssh` returns (when the box is actually ready).
    let provision_start = Instant::now();

    // Provisioning runs in three phases re: the state lock. The state
    // lock protects only short read-modify-write windows on
    // `state.json`; it does NOT serialize the long-running provider
    // calls below. (An earlier design held the lock across everything,
    // which made every concurrent `cargo burst …` invocation — even
    // read-only `status` / `audit` — block for tens of seconds while
    // someone else was provisioning. The 2s lock-acquire timeout would
    // then trip and fail the command outright.)
    //
    //   Phase 1 (brief lock, claim-or-wait): ensure this project's
    //     entry exists, snapshot `image_id`. If `state.server_id` is
    //     already populated, fast-path through. Otherwise either claim
    //     the provisioning slot by writing `server_provisioning_started_at`,
    //     or — if another process has claimed it — drop the lock and
    //     poll every 5s until the slot clears (then re-enter the loop;
    //     usually we'll see the new `server_id` and fast-path).
    //
    //   Phase 2 (NO lock): scan/prompt for excludes, ensure the
    //     provider-side SSH key, peek at existing volume region,
    //     create/reuse the shared server, create/reuse the volume,
    //     attach it. These all mutate a *local* `State` snapshot.
    //
    //   Phase 3 (brief lock via update_state): merge the fields we own
    //     back into the canonical state. Always clears the provisioning
    //     marker if we were the claimant — including on the error path,
    //     so a failed run doesn't wedge concurrent peers.
    let ssh_key_path = cfg.ssh_key_path()?;
    let pubkey = ssh::ensure_ssh_key(&ssh_key_path).await?;

    // ── Phase 1: ensure project entry, claim or wait, snapshot image_id ──
    let (image_id, mut state, claimed_provisioning) = claim_or_wait_for_provisioning(&project).await?;

    // ── Phase 2: API work, no lock held ─────────────────────────────────
    //
    // Wrapped in an async block so we can ALWAYS clear the provisioning
    // marker afterward, whether Phase 2 succeeds or errors out.
    let phase2_result: Result<_> = async {
        let ssh_key_id = provider.ensure_ssh_key("cargo-burst", &pubkey).await?;

        // Local size summary + (on first run) confirm excludes. Doing
        // it before provisioning means the user sees what's about to
        // be sync'd before any cloud resources are created — and lets
        // us bail early if they Ctrl-C at the prompt. Holds no lock,
        // so a parallel `status` from another terminal Just Works even
        // if the user is sitting at the prompt.
        let extra_excludes = scan_and_confirm_excludes(&cfg, &project, &mut state, opts.yes)?;

        // Region-fallback path. We have to know the *server's* region
        // before creating/reusing a volume, because Hetzner volumes are
        // regional — a hel1 volume can't attach to a fsn1 server. So:
        //   1. Peek at any existing volume's region (no state writes).
        //   2. Compute the order in which to attempt regions: that
        //      volume's region first (so we keep the cache when we can),
        //      then the user's preference list, deduped.
        //   3. Provision (or reuse) the server, falling through capacity
        //      errors to the next region.
        //   4. Match the volume to the server's actual region: existing
        //      volume in the right region → reuse; wrong region → delete
        //      and recreate (build cache rebuilds in ~30s, the alternative
        //      is failing the build entirely).
        let existing_volume_region =
            peek_volume_region(provider.as_ref(), &project, &state).await;
        let attempt_regions = compute_attempt_regions(&cfg, existing_volume_region.as_deref());
        let (server, fresh_server, server_region) = ensure_shared_server(
            provider.as_ref(),
            &cfg,
            &image_id,
            &ssh_key_id,
            &mut state,
            &attempt_regions,
        )
        .await?;
        let (volume, fresh_volume) =
            ensure_volume(provider.as_ref(), &cfg, &project, &mut state, &server_region).await?;
        let device = ensure_volume_attached(provider.as_ref(), &volume, &server.id).await?;
        Ok((server, fresh_server, volume, fresh_volume, device, extra_excludes))
    }
    .await;

    // ── Phase 3: merge provisioning mutations back into canonical state ──
    //
    // We "own" the fields below for this project. `update_state` takes
    // the lock briefly, re-reads the on-disk state (so we don't blow
    // away unrelated changes another concurrent run made), and merges
    // our snapshot's view of those fields in. Bumping `last_used`
    // immediately defends against a reaper firing mid-our-run — see
    // reap.rs's "(2) Activity-since-spawn" logic.
    //
    // Crucially this runs whether Phase 2 succeeded or failed — so a
    // failed run still clears the provisioning marker (if we set it),
    // unblocking concurrent peers immediately instead of making them
    // wait out the staleness timeout.
    let hash_for_merge = project.hash.clone();
    let phase2_ok = phase2_result.is_ok();
    let new_server_id = state.server_id.clone();
    let new_server_session = state.current_server_session.clone();
    let project_snapshot = state.projects.get(&hash_for_merge).cloned();
    if let Err(e) = config::update_state(move |s| {
        if claimed_provisioning {
            s.server_provisioning_started_at = None;
        }
        if phase2_ok {
            s.server_id = new_server_id;
            s.current_server_session = new_server_session;
            if let Some(snap) = project_snapshot {
                let entry = s
                    .projects
                    .entry(hash_for_merge.clone())
                    .or_insert_with(|| snap.clone());
                entry.volume_id = snap.volume_id;
                entry.excludes = snap.excludes;
                entry.volume_started_at = snap.volume_started_at;
                entry.last_used_rfc3339 = Some(now_rfc3339());
                // workspace_path is set when the project entry is first
                // created in Phase 1; nothing to merge here.
            }
        }
        Ok(())
    })
    .await
    {
        // Best-effort. If we can't take the lock to merge, log it but
        // still propagate the underlying Phase 2 result.
        tracing::warn!("failed to merge provisioning state: {e}");
    }
    let (server, fresh_server, volume, fresh_volume, device, extra_excludes) = phase2_result?;

    let server_ip = server
        .public_ip
        .clone()
        .ok_or_else(|| anyhow!("server has no IPv4"))?;

    // SSH wait first; everything below talks to the host.
    ssh::wait_for_ssh(&server_ip, "work", &ssh_key_path, SSH_WAIT_TIMEOUT).await?;
    // Anything before this counted as "provisioning" — server + volume
    // API calls, ssh-key sync, the local size scan, and the boot/SSH
    // wait. Anything after counts as either sync or cargo time.
    let provision_elapsed = provision_start.elapsed();

    // Spawn the heartbeat task for the rsync+cargo phase. Aborts on
    // drop, so even if `f` panics or `?`s out, the heartbeat is killed
    // before we try to bump `last_used` one final time.
    let heartbeat = spawn_heartbeat(project.hash.clone());

    // Mount-ensure and rsync run concurrently — disjoint paths
    // (/mnt/cache/<hash>/ vs /home/work/src/<hash>/), so no shared
    // state. The standalone mkdir we used to do is folded into rsync
    // via --rsync-path. One less SSH round-trip per run.
    let remote_src = format!("/home/work/src/{}/", project.hash);
    let mount_script = render_mount_script(&device.device_path, &project.hash);
    // Compose the full exclude list for rsync — defaults filtered by
    // `unexclude`, plus `cfg.extra_excludes`, plus state-saved
    // interactive extras. Same composition the size scanner used,
    // so what got reported in the pre-sync summary is exactly what
    // rsync will skip.
    let merged_excludes = build_merged_excludes(&cfg, &extra_excludes);
    let exclude_args: Vec<&str> = merged_excludes.iter().map(String::as_str).collect();
    let sync_start = Instant::now();
    tracing::info!(
        volume = %volume.id,
        hash = %project.hash,
        "ensuring volume mounted + rsyncing source (in parallel)"
    );

    let mount_fut = async {
        let status = ssh::run_remote(&server_ip, "work", &ssh_key_path, &mount_script).await?;
        if !status.success() {
            return Err(anyhow!("volume mount failed: {status}"));
        }
        Ok::<_, anyhow::Error>(())
    };
    let rsync_fut = ssh::rsync_to(
        &server_ip,
        "work",
        &ssh_key_path,
        &project.workspace_root,
        &remote_src,
        &exclude_args,
        Some(&remote_src),
    );

    tokio::try_join!(mount_fut, rsync_fut)?;
    let sync_elapsed = sync_start.elapsed();
    tracing::info!(elapsed = ?sync_elapsed, "rsync + mount complete");

    // Hand the prepared environment off to the caller. They run
    // whatever cargo subcommand makes sense; we just measure how long
    // it took.
    let target_dir = format!("/mnt/cache/{}/target", project.hash);
    let sccache_dir = format!("/mnt/cache/{}/sccache", project.hash);
    let ctx = RemoteCtx {
        server_ip: server_ip.clone(),
        ssh_key_path: ssh_key_path.clone(),
        workspace_root: project.workspace_root.clone(),
        remote_src: remote_src.clone(),
        target_dir,
        sccache_dir,
        env: env.clone(),
    };

    let cargo_start = Instant::now();
    let cargo_result = f(ctx).await;
    let cargo_elapsed = cargo_start.elapsed();
    tracing::info!(
        elapsed = ?cargo_elapsed,
        ok = cargo_result.is_ok(),
        "remote cargo phase finished"
    );

    // Cleanup ALWAYS runs, even on cargo failure — we still want a
    // healthy reaper schedule and a recorded last_used.
    heartbeat.shutdown().await;

    // Bump last_used + record this command in the audit log + bump
    // the session counter, all under one state-lock acquisition.
    // `label` is already canonical ("Build", "Tests", "Check",
    // "Clippy", "Bench") so we just lowercase + collapse "tests" →
    // "test" to match cargo's own verb spelling.
    let hash_for_final = project.hash.clone();
    let server_id_for_audit = server.id.clone();
    let provider_name = provider.name().to_string();
    let verb = label_to_verb(label);
    let success = cargo_result.is_ok();
    let provision_secs = provision_elapsed.as_secs_f64();
    let sync_secs = sync_elapsed.as_secs_f64();
    let cargo_secs = cargo_elapsed.as_secs_f64();
    if let Err(e) = config::update_state(move |s| {
        if let Some(p) = s.projects.get_mut(&hash_for_final) {
            p.last_used_rfc3339 = Some(now_rfc3339());
        }
        crate::audit::record_command(
            s,
            &crate::audit::CommandSample {
                server_id: server_id_for_audit,
                provider: &provider_name,
                project_hash: &hash_for_final,
                verb: &verb,
                success,
                provision_secs,
                sync_secs,
                cargo_secs,
                fresh_server,
                fresh_volume,
            },
        );
        Ok(())
    })
    .await
    {
        tracing::warn!("failed to bump last_used after run: {e}");
    }

    if !opts.no_reap {
        let keep_alive = opts.keep_alive.unwrap_or(cfg.keep_alive_secs);
        let volume_keep_alive = cfg.volume_keep_alive_secs;
        if let Err(e) = reap::spawn_server(server.id.clone(), keep_alive) {
            tracing::warn!("failed to spawn server reaper: {e}");
        }
        if let Err(e) =
            reap::spawn_volume(volume.id.clone(), project.hash.clone(), volume_keep_alive)
        {
            tracing::warn!("failed to spawn volume reaper: {e}");
        }
        let outcome = if cargo_result.is_ok() { "✓" } else { "✗" };
        println!(
            "{outcome} {label} {} in {:.1}s. Server {} reaps in {}s; project volume {} reaps after {}s of inactivity.",
            if cargo_result.is_ok() { "done" } else { "failed" },
            cargo_elapsed.as_secs_f64(),
            server.id,
            keep_alive,
            volume.id,
            volume_keep_alive,
        );
    } else {
        let outcome = if cargo_result.is_ok() { "✓" } else { "✗" };
        println!(
            "{outcome} {label} {} in {:.1}s. Server {} and volume {} stay up (--no-reap).",
            if cargo_result.is_ok() { "done" } else { "failed" },
            cargo_elapsed.as_secs_f64(),
            server.id,
            volume.id,
        );
    }

    cargo_result
}

// ── Heartbeat ──────────────────────────────────────────────────────────

/// RAII guard for the heartbeat task. Holding one of these guarantees
/// the heartbeat is aborted when the guard drops — error paths in
/// `with_remote` clean up automatically without us having to remember
/// to call `.abort()` at every `?` site.
pub struct HeartbeatGuard(Option<tokio::task::JoinHandle<()>>);

impl HeartbeatGuard {
    /// Stop the heartbeat and wait for the task to finish. Use this
    /// before the final `last_used` bump so we know there are no
    /// in-flight updates that might race the reaper-spawn write.
    pub async fn shutdown(mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
            let _ = h.await;
        }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

fn spawn_heartbeat(project_hash: String) -> HeartbeatGuard {
    let handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            let hash = project_hash.clone();
            if let Err(e) = config::update_state(move |s| {
                if let Some(p) = s.projects.get_mut(&hash) {
                    p.last_used_rfc3339 = Some(now_rfc3339());
                }
                Ok(())
            })
            .await
            {
                tracing::warn!("heartbeat: failed to bump last_used: {e}");
            }
        }
    });
    HeartbeatGuard(Some(handle))
}

// ── Provisioning helpers ──────────────────────────────────────────────

/// Phase 1: acquire a snapshot of state for the calling invocation,
/// either by reusing an already-provisioned server, by claiming the
/// provisioning slot, or by waiting for a peer to finish.
///
/// Returns `(image_id, state_snapshot, claimed_provisioning)`.
/// `claimed_provisioning == true` means *we* wrote
/// `server_provisioning_started_at` and we are responsible for clearing
/// it in Phase 3 (success OR failure). `false` means either we
/// fast-pathed on an already-populated `server_id` or another process
/// did the provisioning and we just woke up to its result; either way,
/// no marker cleanup is required from us.
///
/// The loop terminates as soon as one of three things is true:
///   - `state.server_id` is `Some` (something is already provisioned —
///     ensure_shared_server will validate it's alive shortly).
///   - The provisioning slot is free (no marker, or stale marker), and
///     we successfully claim it.
///   - An I/O error bubbles up from `State::load` or `state.save`.
///
/// `PROVISIONING_POLL_INTERVAL` (5s) is the wait granularity. A
/// "Waiting for shared server provisioning by another cargo-burst
/// process…" line is printed exactly once on the first wait so the user
/// understands why the command isn't progressing; subsequent polls are
/// silent to avoid spam. On exit from a wait, we log how long we waited.
async fn claim_or_wait_for_provisioning(
    project: &ProjectKey,
) -> Result<(crate::provider::ImageId, State, bool)> {
    let mut wait_started: Option<Instant> = None;
    loop {
        let outcome = {
            let _lock = StateLock::acquire().await?;
            let mut s = State::load()?;
            s.projects
                .entry(project.hash.clone())
                .or_insert_with(|| ProjectState {
                    workspace_path: project.workspace_root.display().to_string(),
                    volume_id: None,
                    last_used_rfc3339: None,
                    excludes: None,
                    volume_started_at: None,
                });
            let image_id = s.image_id.clone().ok_or_else(|| {
                anyhow!("no baked image yet — run `cargo burst image build` first")
            })?;
            // Fast path: someone (us, last run; or a peer that just
            // finished) has a server in state. ensure_shared_server
            // will check it's alive and act accordingly.
            if s.server_id.is_some() {
                s.save()?;
                Phase1Outcome::Proceed { image_id, state: s, claimed: false }
            } else {
                let now = now_rfc3339();
                let should_claim = match s.server_provisioning_started_at.as_deref() {
                    None => true,
                    Some(ts) => is_marker_stale(ts, &now),
                };
                if should_claim {
                    s.server_provisioning_started_at = Some(now);
                    s.save()?;
                    Phase1Outcome::Proceed { image_id, state: s, claimed: true }
                } else {
                    // Someone else holds the slot. Drop the lock and poll.
                    Phase1Outcome::Wait
                }
            }
            // _lock drops here whatever branch we took.
        };

        match outcome {
            Phase1Outcome::Proceed { image_id, state, claimed } => {
                if let Some(t0) = wait_started {
                    let waited = t0.elapsed();
                    tracing::info!(
                        waited_secs = waited.as_secs_f64(),
                        "peer finished provisioning shared server; continuing"
                    );
                }
                return Ok((image_id, state, claimed));
            }
            Phase1Outcome::Wait => {
                if wait_started.is_none() {
                    wait_started = Some(Instant::now());
                    println!(
                        "Waiting for shared server provisioning by another cargo-burst process… \
                         (polling state.json every {}s)",
                        PROVISIONING_POLL_INTERVAL.as_secs()
                    );
                }
                tokio::time::sleep(PROVISIONING_POLL_INTERVAL).await;
            }
        }
    }
}

enum Phase1Outcome {
    Proceed { image_id: crate::provider::ImageId, state: State, claimed: bool },
    Wait,
}

/// True if the RFC3339 timestamp `ts` is older than
/// `PROVISIONING_STALE_AFTER_SECS` relative to `now`. Parse failures
/// are treated as "stale" — better to steal a malformed slot than
/// wedge forever on bad state.
fn is_marker_stale(ts: &str, now: &str) -> bool {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    };
    match (parse(ts), parse(now)) {
        (Some(then), Some(now_t)) => (now_t - then).whole_seconds() > PROVISIONING_STALE_AFTER_SECS,
        _ => true,
    }
}

/// Read the project's existing volume's region without touching state.
/// Used to bias the region-attempt order toward "wherever the cache
/// already is" so a non-fallback run is identical to the pre-fallback
/// behaviour: same region every time, no migration churn.
async fn peek_volume_region(
    provider: &dyn Provider,
    project: &ProjectKey,
    state: &State,
) -> Option<String> {
    if let Some(p) = state.projects.get(&project.hash) {
        if let Some(id) = p.volume_id.as_ref() {
            if let Ok(Some(v)) = provider.get_volume(id).await {
                return v.region;
            }
        }
    }
    // State-less recovery path: volume might exist by name even if our
    // local state.json was deleted.
    match provider.find_volume(&project.volume_name()).await {
        Ok(Some(v)) => v.region,
        _ => None,
    }
}

/// Compute the regions to try, in priority order, for server provisioning.
///
/// Rules:
/// 1. If the project's existing volume is in a region that's *also* in
///    the user's `regions` preference, try that region first — keeping
///    the build cache is worth a few hundred ms of extra preference work.
/// 2. Append every region from the preference list (deduped). The volume
///    region might already be there; if so, step 1 hoisted it.
/// 3. If the volume is in a region the user has since removed from their
///    preference list, *don't* try that region (they explicitly opted out)
///    — but emit a warning so the user knows the cache is about to be
///    torn down on the first cold run.
fn compute_attempt_regions(cfg: &Config, existing_volume_region: Option<&str>) -> Vec<String> {
    let pref = cfg.region_preference();
    let mut out: Vec<String> = Vec::with_capacity(pref.len());
    if let Some(r) = existing_volume_region {
        if pref.iter().any(|p| p == r) {
            out.push(r.to_string());
        } else {
            tracing::warn!(
                volume_region = r,
                preference = ?pref,
                "existing volume's region is not in the configured `regions` list; \
                 it will be deleted and rebuilt in a configured region on next provision"
            );
        }
    }
    for r in pref {
        if !out.iter().any(|x| x == &r) {
            out.push(r);
        }
    }
    out
}

async fn ensure_volume(
    provider: &dyn Provider,
    cfg: &Config,
    project: &ProjectKey,
    state: &mut State,
    target_region: &str,
) -> Result<(VolumeInfo, bool)> {
    let provider_name = provider.name();
    let p = state.projects.get_mut(&project.hash).expect("inserted earlier");

    // (1) Try the volume cached in state. Region match → reuse;
    // mismatch → orphan + delete (we can't attach across regions),
    // not-found → forget and fall through.
    if let Some(id) = p.volume_id.clone() {
        match provider.get_volume(&id).await {
            Ok(Some(v)) => {
                if v.region.as_deref() == Some(target_region) {
                    return Ok((v, false));
                }
                tracing::warn!(
                    volume = %v.id,
                    volume_region = ?v.region,
                    server_region = target_region,
                    "abandoning project volume — server fell back to a different region; \
                     cache will rebuild on this run (~30s penalty)"
                );
                if let Err(e) = provider.delete_volume(&v.id).await {
                    tracing::warn!(
                        volume = %v.id,
                        error = %e,
                        "could not delete cross-region volume; you may need to clean it up manually"
                    );
                }
                crate::audit::end_volume_session(
                    p,
                    &project.hash,
                    &v.id,
                    provider_name,
                    crate::audit::TerminationReason::Stale,
                );
                p.volume_id = None;
            }
            Ok(None) => {
                tracing::warn!("volume {id} from state.json no longer exists upstream; will recreate");
                crate::audit::end_volume_session(
                    p,
                    &project.hash,
                    &id,
                    provider_name,
                    crate::audit::TerminationReason::Stale,
                );
                p.volume_id = None;
            }
            Err(e) => {
                tracing::warn!("volume {id} lookup failed ({e}); will recreate");
                crate::audit::end_volume_session(
                    p,
                    &project.hash,
                    &id,
                    provider_name,
                    crate::audit::TerminationReason::Stale,
                );
                p.volume_id = None;
            }
        }
    }

    // (2) Try by name (state.json may be stale or freshly recreated).
    if let Some(v) = provider.find_volume(&project.volume_name()).await? {
        if v.region.as_deref() == Some(target_region) {
            tracing::info!(id = %v.id, "found existing volume by name");
            p.volume_id = Some(v.id.clone());
            return Ok((v, false));
        }
        tracing::warn!(
            volume = %v.id,
            volume_region = ?v.region,
            server_region = target_region,
            "deleting by-name volume in wrong region"
        );
        if let Err(e) = provider.delete_volume(&v.id).await {
            tracing::warn!(
                volume = %v.id,
                error = %e,
                "could not delete cross-region volume; you may need to clean it up manually"
            );
        }
    }

    // (3) Create fresh in the target region.
    tracing::info!(
        name = %project.volume_name(),
        size_gb = cfg.volume_gb,
        region = target_region,
        "creating new volume"
    );
    let mut labels = BTreeMap::new();
    labels.insert("managed-by".into(), "cargo-burst".into());
    labels.insert("project-hash".into(), project.hash.clone());
    let info = provider
        .create_volume(VolumeSpec {
            name: project.volume_name(),
            size_gb: cfg.volume_gb,
            region: target_region.to_string(),
            labels,
        })
        .await?;
    p.volume_id = Some(info.id.clone());
    crate::audit::begin_volume_session(
        p,
        &project.hash,
        info.id.clone(),
        provider_name,
        cfg.volume_gb,
        target_region,
    );
    Ok((info, true))
}

/// Provision (or reuse) the shared server, trying each region in
/// `attempt_regions` until one accepts the create or all fail. The
/// returned `String` is the region the server actually ended up in —
/// used downstream to align the project's volume.
///
/// Reuse path is unchanged: if a running server already exists (per
/// state, or by name lookup) we just hand it back; the region we
/// return is read from the server's `datacenter.location.name`.
async fn ensure_shared_server(
    provider: &dyn Provider,
    cfg: &Config,
    image_id: &crate::provider::ImageId,
    ssh_key_id: &SshKeyId,
    state: &mut State,
    attempt_regions: &[String],
) -> Result<(ServerInfo, bool, String)> {
    let region_fallback: &str = attempt_regions
        .first()
        .map(String::as_str)
        .unwrap_or("unknown");

    let server_region = |s: &ServerInfo| -> String {
        s.region.clone().unwrap_or_else(|| region_fallback.to_string())
    };

    if let Some(id) = state.server_id.clone() {
        match provider.get_server(&id).await {
            Ok(Some(s)) if s.status.is_alive() => {
                let region = server_region(&s);
                tracing::info!(id = %id, status = s.status.as_str(), region = %region, "reusing shared server from state");
                return Ok((s, false, region));
            }
            Ok(Some(s)) => {
                tracing::warn!(id = %id, status = s.status.as_str(), "stale server in state; deleting");
                let _ = provider.delete_server(&id).await;
                crate::audit::end_server_session(state, &id, crate::audit::TerminationReason::Stale);
                state.server_id = None;
            }
            Ok(None) => {
                tracing::warn!("server {id} from state.json no longer exists upstream; will recreate");
                crate::audit::end_server_session(state, &id, crate::audit::TerminationReason::Stale);
                state.server_id = None;
            }
            Err(e) => {
                tracing::warn!("server {id} lookup failed ({e}); will recreate");
                crate::audit::end_server_session(state, &id, crate::audit::TerminationReason::Stale);
                state.server_id = None;
            }
        }
    }

    if let Some(s) = provider.find_server(SHARED_SERVER_NAME).await? {
        if s.status.is_alive() {
            let region = server_region(&s);
            tracing::info!(id = %s.id, status = s.status.as_str(), region = %region, "found shared server by name");
            state.server_id = Some(s.id.clone());
            return Ok((s, false, region));
        }
        tracing::warn!(id = %s.id, status = s.status.as_str(), "deleting stale server found by name");
        let id_clone = s.id.clone();
        let _ = provider.delete_server(&id_clone).await;
        crate::audit::end_server_session(state, &id_clone, crate::audit::TerminationReason::Stale);
    }

    if attempt_regions.is_empty() {
        return Err(anyhow!(
            "no regions configured for provisioning (set `regions = [...]` in config.toml)"
        ));
    }
    let server_type = cfg.server_type();
    let provider_name = provider.name();
    for region in attempt_regions {
        tracing::info!(
            name = SHARED_SERVER_NAME,
            image = %image_id,
            server_type = %server_type,
            region = %region,
            "provisioning shared server"
        );
        let mut labels = BTreeMap::new();
        labels.insert("managed-by".into(), "cargo-burst".into());
        labels.insert("role".into(), "shared".into());
        let spec = ServerSpec {
            name: SHARED_SERVER_NAME.to_string(),
            server_type: server_type.clone(),
            image: image_id.clone(),
            region: region.clone(),
            ssh_keys: vec![ssh_key_id.clone()],
            user_data: None,
            labels,
            attached_volumes: Vec::new(),
        };
        match provider.create_server(spec).await {
            Ok(info) => {
                state.server_id = Some(info.id.clone());
                crate::audit::begin_server_session(
                    state,
                    info.id.clone(),
                    provider_name,
                    &server_type,
                    image_id.clone(),
                    region,
                );
                return Ok((info, true, region.clone()));
            }
            Err(ProvisionError::NoCapacity { .. }) => {
                tracing::warn!(region = %region, "region at capacity; trying next");
                continue;
            }
            Err(ProvisionError::Other(e)) => return Err(e),
        }
    }
    Err(anyhow!(
        "every region in the preference list ({:?}) reported no capacity; \
         try again later, or add another region to your config",
        attempt_regions
    ))
}

async fn ensure_volume_attached(
    provider: &dyn Provider,
    volume: &VolumeInfo,
    server_id: &crate::provider::ServerId,
) -> Result<crate::provider::AttachedDevice> {
    match volume.attached_to.as_ref() {
        Some(id) if id == server_id => {
            tracing::info!(volume = %volume.id, "volume already attached to shared server");
            // Already attached, but we still need to know the device
            // path. For Hetzner the formula is deterministic from the
            // volume ID; we recover it by issuing a fresh attach (the
            // call is idempotent — Hetzner returns success when the
            // volume is already attached to the requested server).
            provider.attach_volume(&volume.id, server_id).await
        }
        Some(other) => Err(anyhow!(
            "volume {} is attached to server {other}, not the shared server {server_id}. \
             Detach manually if intentional.",
            volume.id
        )),
        None => {
            tracing::info!(volume = %volume.id, server = %server_id, "attaching volume");
            provider.attach_volume(&volume.id, server_id).await
        }
    }
}

/// Render the inline mount script we ssh-execute. The device path is
/// provider-specific (Hetzner: `/dev/disk/by-id/scsi-0HC_Volume_<id>`,
/// AWS Nitro: `/dev/nvme<N>n1`) and is passed in by the caller.
/// Idempotent: short-circuits if already mounted, formats ext4 on
/// first use, waits up to ~10s for the device file to appear.
fn render_mount_script(device_path: &str, hash: &str) -> String {
    format!(
        r#"sudo bash -s <<'BURST_MOUNT'
set -euo pipefail
target=/mnt/cache/{hash}
dev={device_path}

if mountpoint -q "$target"; then
    exit 0
fi

for _ in $(seq 1 30); do
    [ -e "$dev" ] && break
    sleep 1
done
if [ ! -e "$dev" ]; then
    echo "device $dev never appeared (volume not attached?)" >&2
    ls -la /dev/disk/by-id/ 2>&1 >&2 || true
    exit 1
fi

if ! blkid "$dev" >/dev/null 2>&1; then
    mkfs.ext4 -q -L "burst-{hash}" "$dev"
fi

install -d -m 0755 "$target"
mount -o noatime "$dev" "$target"
chown work:work "$target"
install -d -o work -g work "$target/target" "$target/sccache"
BURST_MOUNT
"#
    )
}

// ── Exclude scan + first-run prompt ───────────────────────────────────

fn scan_and_confirm_excludes(
    cfg: &Config,
    project: &ProjectKey,
    state: &mut State,
    auto_yes: bool,
) -> Result<Vec<String>> {
    let saved = state
        .projects
        .get(&project.hash)
        .and_then(|p| p.excludes.clone());
    let mut user_excludes: Vec<String> = saved.clone().unwrap_or_default();

    let merged = build_merged_excludes(cfg, &user_excludes);
    let matcher = build_matcher(&project.workspace_root, &merged)?;

    let sizes = top_level_sizes(&project.workspace_root, &matcher)?;
    let total: u64 = sizes.values().sum();

    println!();
    println!("Workspace: {}", project.workspace_root.display());
    println!("Would sync (after excludes): {}", format_bytes(total));
    println!();
    println!("Top-level dirs by size (post-exclude):");
    let mut entries: Vec<_> = sizes.iter().collect();
    entries.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
    for (name, size) in entries.iter().take(15) {
        let path = project.workspace_root.join(name);
        let fully_excluded = matcher.matched(&path, path.is_dir()).is_ignore();
        let marker = if fully_excluded { " [excluded]" } else { "" };
        let display_size = if fully_excluded {
            "       —".to_string()
        } else {
            format_bytes(**size)
        };
        println!("  {display_size:>10}  {name}{marker}");
    }
    println!();

    let first_run = saved.is_none();
    // Non-interactive stdin (CI, piped input, automation harness) is
    // treated the same as `--yes`: skip the prompt entirely, don't
    // auto-add suggestions to the exclude list. Blocking on a prompt
    // that no human can answer is the worst outcome; silently
    // assuming "yes" on EOF (the previous behavior) is only marginally
    // better — it could quietly drop directories the script-author
    // didn't expect to be dropped. Sync-as-is is the safe default.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if first_run && !auto_yes && !interactive {
        tracing::info!(
            "stdin is not a TTY; skipping first-run exclude prompt and syncing as-is. \
             Pass `--yes` to silence this, or set `extra_excludes` in config to opt in."
        );
    }
    if should_prompt_excludes(first_run, auto_yes, interactive) {
        let suggestions: Vec<&String> = entries
            .iter()
            .filter(|(name, size)| {
                **size >= 50 * 1024 * 1024
                    && matches!(
                        name.as_str(),
                        "dist" | ".next" | "build" | "out" | "tmp"
                    )
            })
            .map(|(n, _)| *n)
            .collect();

        if !suggestions.is_empty() {
            println!("Suggested additional excludes (large, usually not needed for cargo):");
            for s in &suggestions {
                println!("  {s}/");
            }
            println!();
            println!("Apply suggested excludes? [Y/n] (anything else = sync as-is)");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            let a = answer.trim().to_lowercase();
            if a.is_empty() || a == "y" || a == "yes" {
                for s in &suggestions {
                    user_excludes.push(format!("{s}/"));
                }
            }
        }
    }

    if !user_excludes.is_empty() {
        println!("Additional excludes (on top of defaults): {}", user_excludes.join(", "));
        println!();
    }

    let project_state = state.projects.get_mut(&project.hash).expect("inserted earlier");
    project_state.excludes = Some(user_excludes.clone());

    Ok(user_excludes)
}

/// Should the first-run exclude prompt actually be shown?
///
/// Pure function so the policy is unit-testable without owning a
/// real TTY. The rule is "show only when we've never asked before
/// (first_run), the caller didn't opt out (auto_yes), and stdin is
/// a real terminal (interactive)". Any non-interactive caller
/// (CI, piped stdin, automation harness) is treated the same as
/// `--yes`: no prompt, no auto-applied suggestions, sync as-is.
fn should_prompt_excludes(first_run: bool, auto_yes: bool, interactive: bool) -> bool {
    first_run && !auto_yes && interactive
}

/// Compose the final rsync exclude list:
///
///   1. `ssh::DEFAULT_EXCLUDES`, minus anything the user listed in
///      `cfg.unexclude` (exact-match against default entries).
///   2. `cfg.extra_excludes` — additional patterns from global +
///      project config.
///   3. Interactive/state-saved excludes the user confirmed during
///      a previous first-run prompt (`ProjectState.excludes`).
///
/// Last segment wins on duplicate patterns (rsync handles repeats
/// fine; just untidy log output otherwise). Returned in deterministic
/// order so the size scanner and rsync see the same set.
///
/// Matching is **trailing-slash-insensitive**: the built-in defaults
/// are written in gitignore syntax (`.git/`, `target/`, …) but to a
/// user `.git` and `.git/` mean the same thing. Requiring the exact
/// gitignore form makes the config a footgun — you write `.git` in
/// `unexclude`, nothing happens, and the only signal is a warning
/// that looks like the entry is bogus. Normalize both sides by
/// stripping a single trailing slash before comparing.
///
/// Side effect: warns once for each `cfg.unexclude` entry that doesn't
/// match a default — almost always a typo, occasionally a stale entry
/// from a future where the default list was different. Non-fatal so
/// we don't block builds on a config typo.
fn build_merged_excludes(cfg: &Config, user: &[String]) -> Vec<String> {
    let unexclude: &[String] = &cfg.unexclude;
    let mut out: Vec<String> = Vec::new();
    for d in ssh::DEFAULT_EXCLUDES {
        if !unexclude.iter().any(|u| unexclude_matches_default(u, d)) {
            out.push((*d).to_string());
        }
    }
    for u in unexclude {
        if !ssh::DEFAULT_EXCLUDES
            .iter()
            .any(|d| unexclude_matches_default(u, d))
        {
            tracing::warn!(
                pattern = %u,
                "`unexclude` entry doesn't match any built-in default; \
                 nothing to remove (typo? stale entry?)"
            );
        }
    }
    for pat in &cfg.extra_excludes {
        out.push(pat.clone());
    }
    out.extend(user.iter().cloned());
    out
}

/// Compare a user-supplied `unexclude` entry against a built-in default
/// pattern, ignoring a single trailing slash on either side. Pure
/// function so the slash-normalization rule is unit-testable. We
/// strip *all* trailing slashes (so `.git//` and `.git/` both match
/// `.git/`) — a genuinely malformed pattern is more usefully treated
/// as the same thing than as a separate typo.
fn unexclude_matches_default(user: &str, default_pat: &str) -> bool {
    user.trim_end_matches('/') == default_pat.trim_end_matches('/')
}

fn build_matcher(root: &Path, patterns: &[String]) -> Result<ignore::gitignore::Gitignore> {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    for p in patterns {
        b.add_line(None, p)
            .with_context(|| format!("invalid exclude pattern {p:?}"))?;
    }
    b.build().context("compiling exclude patterns")
}

fn top_level_sizes(
    root: &Path,
    matcher: &ignore::gitignore::Gitignore,
) -> Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let is_dir = path.is_dir();

        if matcher.matched(&path, is_dir).is_ignore() {
            out.insert(name, 0);
            continue;
        }

        let size = if is_dir {
            dir_size(&path, matcher)
        } else {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        };
        out.insert(name, size);
    }
    Ok(out)
}

fn dir_size(dir: &Path, matcher: &ignore::gitignore::Gitignore) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&d) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let is_dir = meta.is_dir();
            if matcher.matched(&path, is_dir).is_ignore() {
                continue;
            }
            if is_dir {
                stack.push(path);
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 {
        format!("{:>5} {}", v as u64, UNITS[i])
    } else {
        format!("{:>5.1} {}", v, UNITS[i])
    }
}

// ── Misc utilities shared across subcommands ──────────────────────────

/// Current time as RFC3339 UTC. Returned as `String` so call sites
/// don't have to thread lifetimes through into `State`.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Cheap shell quoting for cargo args. Wraps anything containing a
/// non-shell-safe character in single quotes; embedded single quotes
/// are escaped via the standard `'\''` trick.
pub fn shell_escape(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '=' | ':' | ',' | '+' | '@'))
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Run a single `cargo <verb> [args…]` invocation on the remote and
/// propagate its exit status as a `Result`. No artifact fetch, no
/// secondary commands — meant for the simple passthrough subcommands
/// (`check`, `clippy`) where the user only wants the diagnostics
/// streamed to their terminal.
///
/// `default_verb` is substituted when `cargo_args` is empty (so
/// `cargo burst check` runs `cargo check`, while
/// `cargo burst check -- --tests` runs `cargo check --tests`).
///
/// `label` is forwarded to [`with_remote`] for the summary line.
pub async fn run_cargo_passthrough(
    opts: RemoteOptions,
    label: &'static str,
    default_verb: &'static str,
    cargo_args: Vec<String>,
) -> Result<()> {
    let cargo_args = prepend_cargo_verb(default_verb, cargo_args);
    with_remote(opts, label, move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> = cargo_args.iter().map(|a| shell_escape(a)).collect();
        let cmd = build_remote_cmd(&ctx, &format!("cargo {}", escaped.join(" ")));
        let status = run_cargo_with_hints(&ctx, default_verb, &cmd).await?;
        if !status.success() {
            return Err(anyhow!("cargo exited with status {status}"));
        }
        Ok(())
    })
    .await
}

/// Run a cargo invocation on the prepared remote, streaming output
/// to the caller's terminal AND scanning the captured stream for
/// hint-eligible failure patterns. On a non-zero exit, prints any
/// detected hint to stderr before returning the status to the
/// caller.
///
/// All cargo-verb call sites should use this rather than calling
/// `ssh::run_remote` directly — the per-verb `verb_for_hint`
/// (`"build"`, `"test"`, …) is the string we'll splice into the
/// suggested `cargo burst <verb> --env …` command line.
///
/// Returns the raw exit status; the caller still decides whether to
/// fail, retry, or chain to a follow-up phase (e.g. test's nextest
/// → doctest sequence).
pub async fn run_cargo_with_hints(
    ctx: &RemoteCtx,
    verb_for_hint: &str,
    cmd: &str,
) -> Result<std::process::ExitStatus> {
    // force_tty=true: allocates a remote PTY so that when the local
    // ssh client is killed (Ctrl-C, SIGTERM via signal handler) sshd
    // forwards SIGHUP to the cargo process on the remote. Without
    // this, the cargo run would keep going for minutes after we
    // abandoned it, on a server we're paying for.
    let (status, captured) = ssh::run_remote_capturing(
        &ctx.server_ip,
        "work",
        &ctx.ssh_key_path,
        cmd,
        true,
    )
    .await?;
    if !status.success() {
        if let Some(hint) = crate::hints::detect_env_hint(&captured) {
            // Hint goes to stderr so it doesn't get tangled in stdout
            // pipelines (e.g. `cargo burst test | tee out.log`). The
            // formatter starts with a leading newline so the hint
            // visually separates from cargo's own error trail.
            eprint!("{}", hint.format(verb_for_hint));
        }
    }
    Ok(status)
}

/// Shell prefix that blocks until postgres/mysql/redis are reachable
/// on the remote, or 30 s, whichever comes first.
///
/// Used by `cargo burst test` and `cargo burst bench`, which are the
/// subcommands likely to want a real database. Idempotent and ~5 ms
/// once the services are warm.
///
/// The `command -v … || true` wrapper degrades gracefully when the
/// remote image is older than v0.4.0 (no `cargo-burst-wait-for-
/// databases` script baked in) — the user just doesn't get the wait,
/// matching pre-v0.4 behaviour. They can rebake to opt in.
pub const DB_WAIT_PREFIX: &str =
    "(command -v cargo-burst-wait-for-databases >/dev/null \
     && cargo-burst-wait-for-databases || true) && ";

/// Render the boilerplate that wraps every cargo invocation: pin
/// `CARGO_TARGET_DIR` and `SCCACHE_DIR` to the project's volume slot,
/// put `~/.cargo/bin` on PATH (so `cargo-nextest` is reachable), export
/// any user-forwarded env vars (from `forward_env` config + `--env`
/// flags, already resolved into `ctx.env`), make sure target/sccache
/// dirs exist, and `cd` into the rsync'd source.
///
/// User-forwarded vars are exported *after* our internal pins, so a
/// user can't accidentally clobber `CARGO_TARGET_DIR` (we'd lose the
/// volume cache benefit) or `PATH` (we'd lose `cargo-nextest`).
/// Refusing those names is enforced upstream in `resolve_env` —
/// here we trust the input.
///
/// `body` is appended after the prelude — typically one or more
/// `cargo …` invocations. Combine with `&&` if you're chaining.
pub fn build_remote_cmd(ctx: &RemoteCtx, body: &str) -> String {
    let user_exports = ctx
        .env
        .iter()
        .map(|(k, v)| format!("export {k}={};", shell_escape(v)))
        .collect::<Vec<_>>()
        .join(" ");
    let user_exports = if user_exports.is_empty() {
        String::new()
    } else {
        format!("{user_exports} ")
    };
    format!(
        // `-tt` (set on the ssh side, for signal propagation) makes
        // the remote see a PTY, which makes Rust tooling switch to
        // interactive output: ANSI colors from cargo, an in-place
        // progress bar from nextest that uses `\r` instead of `\n`
        // and never flushes a complete line until the run is over.
        // Two internal-pin env vars walk that back without losing
        // signal propagation:
        //
        //   CARGO_TERM_COLOR=never — strips color escapes so the
        //     captured-for-hint-detection output is plain text and
        //     `hints::detect_env_hint`'s substring match still works.
        //
        //   CI=true — the universal "force non-interactive plain
        //     output" lever Rust tools all respect. Switches nextest
        //     to its plain reporter (line-per-test, newline-terminated)
        //     so an agent or piped consumer actually sees the test
        //     results stream by instead of an opaque ~5s of silence
        //     followed by "Finished" and nothing else. Cargo treats
        //     it the same way (suppresses progress spinner).
        //
        // Both can be overridden by user `--env CARGO_TERM_COLOR=…`
        // / `--env CI=…` (exported after this).
        // Pre-start the sccache server with stdin/stdout/stderr all
        // redirected to /dev/null *before* cargo runs. The baked image
        // sets `rustc-wrapper = /usr/local/bin/sccache` globally, so
        // every cargo invocation pipes rustc through sccache. If the
        // sccache server isn't already running, the client lazy-forks
        // it from the cargo process tree — and that daemon inherits
        // cargo's stdin/stdout/stderr, which under `-tt` are the
        // remote PTY's slave fds. After cargo exits, the sccache
        // server keeps the PTY open, sshd refuses to close the channel
        // until every PTY-slave fd is released, and our local
        // `child.wait()` hangs indefinitely. (Symptom: `cargo burst
        // test` "sometimes hangs" — specifically when the sccache
        // server has timed out since the previous run and needs a
        // fresh start.)
        //
        // Forcing the start here, with all three std fds bound to
        // /dev/null inside a backgrounded subshell, daemonizes the
        // server with no references to the PTY. Cargo's later
        // sccache-client calls find the server already running and
        // never fork. `|| true` because old baked images may not have
        // sccache at all — graceful degradation rather than a hard
        // fail under `set -e`.
        "set -euo pipefail; \
         export CARGO_TARGET_DIR={target}; \
         export SCCACHE_DIR={sccache}; \
         export PATH=$HOME/.cargo/bin:$PATH; \
         export CARGO_TERM_COLOR=never; \
         export CI=true; \
         {user_exports}\
         mkdir -p {target} {sccache}; \
         (sccache --start-server </dev/null >/dev/null 2>&1 || true); \
         cd {src}; \
         {body}",
        target = ctx.target_dir,
        sccache = ctx.sccache_dir,
        src = ctx.remote_src,
    )
}

/// Names we refuse to forward to the remote even if the user asks.
/// These are either pinned by `build_remote_cmd` itself (clobbering
/// them would silently break the cache or runtime), or completely
/// host-specific and meaningless on the remote (HOME/USER/PATH
/// pointing at the local filesystem).
const FORBIDDEN_ENV_NAMES: &[&str] = &[
    "CARGO_TARGET_DIR",
    "SCCACHE_DIR",
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "PWD",
    "OLDPWD",
    "SHELL",
];

/// Combine the global+project `forward_env` allow-list with per-run
/// `--env` flags into a flat `Vec<(name, value)>` ready for export
/// on the remote.
///
/// Resolution rules:
///
/// 1. For each name in `forward_env`, look up `std::env::var(name)`.
///    Unset → skip with a debug log (it's fine to keep `RUST_LOG` in
///    the list across runs where you didn't set it).
/// 2. For each `--env` flag value:
///    - `NAME` → forward `$NAME` from local. If unset, *warn* — the
///      user explicitly asked for this one, so silence would mask a
///      typo or shell-startup bug.
///    - `NAME=value` → set verbatim. The value goes through verbatim;
///      shell-escaping happens later in `build_remote_cmd`.
/// 3. `--env` wins over `forward_env` on duplicate names (last one
///    wins among `--env` itself, in argv order).
/// 4. [`FORBIDDEN_ENV_NAMES`] are dropped with a warning. Forwarding
///    them would either silently break the remote build or carry
///    host-only paths into a different filesystem.
pub fn resolve_env(forward: &[String], cli_env: &[String]) -> Vec<(String, String)> {
    use std::collections::BTreeMap;

    // BTreeMap so output ordering is stable (handy for tests and for
    // the eventual shell command being deterministic).
    let mut out: BTreeMap<String, String> = BTreeMap::new();

    let forbidden = |name: &str| FORBIDDEN_ENV_NAMES.iter().any(|n| *n == name);

    for name in forward {
        if forbidden(name) {
            tracing::warn!(name, "refusing to forward reserved env var");
            continue;
        }
        match std::env::var(name) {
            Ok(value) => {
                out.insert(name.clone(), value);
            }
            Err(_) => {
                tracing::debug!(name, "forward_env entry is locally unset; skipping");
            }
        }
    }

    for spec in cli_env {
        let (name, value) = match spec.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (spec.clone(), None),
        };
        if name.is_empty() {
            tracing::warn!(spec, "ignoring `--env` with empty name");
            continue;
        }
        if forbidden(&name) {
            tracing::warn!(name, "refusing to forward reserved env var");
            continue;
        }
        match value {
            Some(v) => {
                out.insert(name, v);
            }
            None => match std::env::var(&name) {
                Ok(v) => {
                    out.insert(name, v);
                }
                Err(_) => {
                    // Explicit `--env NAME` with no local value is
                    // worth surfacing — unlike forward_env's standing
                    // list, this was a one-off ask.
                    tracing::warn!(name, "`--env {}` is locally unset; not forwarding", name);
                }
            },
        }
    }

    out.into_iter().collect()
}

/// Map the human-readable label that subcommands pass into `with_remote`
/// (`"Build"`, `"Tests"`, `"Check"`, `"Clippy"`, `"Bench"`) to a
/// canonical lowercase cargo verb for the audit log.
///
/// Only "Tests" needs a special case — we record `"test"` to match
/// cargo's own verb name. Anything else just lowercases.
fn label_to_verb(label: &str) -> String {
    match label.to_ascii_lowercase().as_str() {
        "tests" => "test".to_string(),
        other => other.to_string(),
    }
}

/// Ensure `cargo_args` starts with the cargo subcommand verb that the
/// user invoked (`build`/`check`/`clippy`/…). Args passed after `--`
/// from the CLI are flags for that verb, not a replacement of it —
/// `cargo burst check -- --all-targets` should run `cargo check
/// --all-targets`, not `cargo --all-targets`.
///
/// If the user already started their args with the verb (legacy
/// habit, e.g. `cargo burst build -- build --release`), we leave
/// it alone — prepending again would produce `cargo build build
/// --release`, which fails.
pub fn prepend_cargo_verb(verb: &str, args: Vec<String>) -> Vec<String> {
    let already_has_verb = args
        .iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-'))
        == Some(verb);
    if already_has_verb {
        return args;
    }
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(verb.to_string());
    out.extend(args);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_to_verb_canonicalizes_test() {
        assert_eq!(label_to_verb("Tests"), "test");
    }

    #[test]
    fn unexclude_matches_default_ignores_trailing_slash() {
        // Either side may or may not have a trailing slash; semantic
        // equality should hold across all four combinations.
        assert!(unexclude_matches_default(".git", ".git/"));
        assert!(unexclude_matches_default(".git/", ".git/"));
        assert!(unexclude_matches_default(".git/", ".git"));
        assert!(unexclude_matches_default(".git", ".git"));
        assert!(unexclude_matches_default("target", "target/"));
        assert!(unexclude_matches_default("node_modules", "node_modules/"));
        // Multiple trailing slashes collapse (malformed but harmless).
        assert!(unexclude_matches_default(".git//", ".git/"));
    }

    #[test]
    fn unexclude_matches_default_rejects_unrelated() {
        assert!(!unexclude_matches_default(".gitignore", ".git/"));
        assert!(!unexclude_matches_default("target", "node_modules/"));
        assert!(!unexclude_matches_default("", ".git/"));
    }

    #[test]
    fn build_merged_excludes_strips_default_for_dot_git_without_slash() {
        // The bug we're fixing: `unexclude = [".git"]` (no trailing
        // slash) used to fail to match `.git/` in DEFAULT_EXCLUDES and
        // .git/ would still be excluded. Now it should drop out of
        // the merged list.
        let mut cfg = cfg_with("nbg1", &[]);
        cfg.unexclude = vec![".git".into()];
        let merged = build_merged_excludes(&cfg, &[]);
        assert!(
            !merged.iter().any(|p| p == ".git/" || p == ".git"),
            "expected .git/ to be removed by unexclude = [\".git\"]; got {merged:?}"
        );
        // Sanity: other defaults are still present.
        assert!(merged.iter().any(|p| p == "target/"));
    }

    #[test]
    fn is_marker_stale_returns_false_for_fresh_marker() {
        // 10s old — well under the 180s staleness threshold.
        let now = time::OffsetDateTime::now_utc();
        let then = now - time::Duration::seconds(10);
        let now_s = now.format(&time::format_description::well_known::Rfc3339).unwrap();
        let then_s = then.format(&time::format_description::well_known::Rfc3339).unwrap();
        assert!(!is_marker_stale(&then_s, &now_s));
    }

    #[test]
    fn is_marker_stale_returns_true_past_threshold() {
        // 200s old — past the 180s threshold; another arrival should
        // assume the claimant crashed and steal the slot.
        let now = time::OffsetDateTime::now_utc();
        let then = now - time::Duration::seconds(200);
        let now_s = now.format(&time::format_description::well_known::Rfc3339).unwrap();
        let then_s = then.format(&time::format_description::well_known::Rfc3339).unwrap();
        assert!(is_marker_stale(&then_s, &now_s));
    }

    #[test]
    fn is_marker_stale_returns_true_for_unparseable_marker() {
        // Garbage in state.json should not block forever — treat as stale.
        let now = time::OffsetDateTime::now_utc();
        let now_s = now.format(&time::format_description::well_known::Rfc3339).unwrap();
        assert!(is_marker_stale("not-a-timestamp", &now_s));
    }

    #[test]
    fn label_to_verb_lowercases_others() {
        assert_eq!(label_to_verb("Build"), "build");
        assert_eq!(label_to_verb("Check"), "check");
        assert_eq!(label_to_verb("Clippy"), "clippy");
        assert_eq!(label_to_verb("Bench"), "bench");
    }

    fn cfg_with(region: &str, regions: &[&str]) -> Config {
        Config {
            provider: crate::config::ProviderKind::Hetzner,
            hetzner: Some(crate::config::HetznerConfig {
                token: "x".into(),
                region: vec![region.into()],
                regions: regions.iter().map(|s| s.to_string()).collect(),
                server_type: "ccx63".into(),
            }),
            aws: None,
            keep_alive_secs: 300,
            volume_keep_alive_secs: 3600,
            volume_gb: 200,
            ssh_key_path: None,
            forward_env: Vec::new(),
            extra_excludes: Vec::new(),
            unexclude: Vec::new(),
        }
    }

    #[test]
    fn should_prompt_excludes_truth_table() {
        // first_run × auto_yes × interactive → expected
        for (first_run, auto_yes, interactive, expected) in [
            (false, false, true, false),  // not first run → no prompt
            (false, true,  true, false),  // not first run → no prompt
            (false, false, false, false), // not first run → no prompt
            (true,  true,  true, false),  // --yes wins
            (true,  true,  false, false), // --yes + non-tty: still skip
            (true,  false, false, false), // first-run + non-tty: skip
            (true,  false, true,  true),  // first-run, no --yes, real TTY → prompt
        ] {
            assert_eq!(
                should_prompt_excludes(first_run, auto_yes, interactive),
                expected,
                "first_run={first_run} auto_yes={auto_yes} interactive={interactive}",
            );
        }
    }

    #[test]
    fn merged_excludes_default_when_unconfigured() {
        let cfg = cfg_with("hel1", &[]);
        let merged = build_merged_excludes(&cfg, &[]);
        // Every default present, no user extras.
        for d in ssh::DEFAULT_EXCLUDES {
            assert!(merged.iter().any(|m| m == d), "missing default {d}");
        }
        assert_eq!(merged.len(), ssh::DEFAULT_EXCLUDES.len());
    }

    #[test]
    fn merged_excludes_unexclude_removes_the_default() {
        let mut cfg = cfg_with("hel1", &[]);
        cfg.unexclude = vec![".git/".into()];
        let merged = build_merged_excludes(&cfg, &[]);
        assert!(!merged.iter().any(|m| m == ".git/"));
        // Other defaults still there.
        assert!(merged.iter().any(|m| m == "target/"));
    }

    #[test]
    fn merged_excludes_extra_excludes_appended() {
        let mut cfg = cfg_with("hel1", &[]);
        cfg.extra_excludes = vec!["fixtures/large/".into(), "*.bak".into()];
        let merged = build_merged_excludes(&cfg, &[]);
        // Defaults still present, extras appended afterward.
        assert!(merged.iter().any(|m| m == "target/"));
        assert!(merged.iter().any(|m| m == "fixtures/large/"));
        assert!(merged.iter().any(|m| m == "*.bak"));
    }

    #[test]
    fn merged_excludes_state_user_excludes_appended_last() {
        let cfg = cfg_with("hel1", &[]);
        let user = vec!["dist/".to_string()];
        let merged = build_merged_excludes(&cfg, &user);
        assert_eq!(merged.last(), Some(&"dist/".to_string()));
    }

    #[test]
    fn merged_excludes_combines_unexclude_extra_and_user() {
        let mut cfg = cfg_with("hel1", &[]);
        cfg.unexclude = vec![".git/".into()];
        cfg.extra_excludes = vec!["fixtures/".into()];
        let merged = build_merged_excludes(&cfg, &["dist/".to_string()]);
        assert!(!merged.iter().any(|m| m == ".git/"));
        assert!(merged.iter().any(|m| m == "fixtures/"));
        assert!(merged.iter().any(|m| m == "dist/"));
    }

    #[test]
    fn merged_excludes_unknown_unexclude_is_warned_not_fatal() {
        let mut cfg = cfg_with("hel1", &[]);
        cfg.unexclude = vec!["definitely_not_a_default/".into()];
        // Doesn't panic; just emits a warn (which we don't capture in
        // this test). The default list still comes through unchanged.
        let merged = build_merged_excludes(&cfg, &[]);
        assert_eq!(merged.len(), ssh::DEFAULT_EXCLUDES.len());
    }

    #[test]
    fn attempt_regions_default_path() {
        // No `regions` configured → `region` becomes a single-element
        // list. No existing volume → that single element is what we try.
        let cfg = cfg_with("hel1", &[]);
        assert_eq!(compute_attempt_regions(&cfg, None), vec!["hel1"]);
    }

    #[test]
    fn attempt_regions_existing_volume_hoists_its_region() {
        // `regions = ["hel1", "fsn1", "nbg1"]`, volume already in fsn1
        // → fsn1 first (cache-preserving), then hel1 and nbg1.
        let cfg = cfg_with("hel1", &["hel1", "fsn1", "nbg1"]);
        assert_eq!(
            compute_attempt_regions(&cfg, Some("fsn1")),
            vec!["fsn1", "hel1", "nbg1"]
        );
    }

    #[test]
    fn attempt_regions_volume_in_region_thats_not_in_pref_list() {
        // User has a volume in nbg1 but reconfigured `regions` to omit
        // nbg1. We should try only the configured regions and let the
        // ensure_volume cleanup path delete the now-orphaned volume.
        let cfg = cfg_with("hel1", &["hel1", "fsn1"]);
        assert_eq!(
            compute_attempt_regions(&cfg, Some("nbg1")),
            vec!["hel1", "fsn1"]
        );
    }

    #[test]
    fn attempt_regions_dedup_when_volume_already_first() {
        let cfg = cfg_with("hel1", &["hel1", "fsn1"]);
        // Volume is in hel1 (first preference). Don't double up.
        assert_eq!(
            compute_attempt_regions(&cfg, Some("hel1")),
            vec!["hel1", "fsn1"]
        );
    }

    #[test]
    fn capacity_error_recognized_in_real_hetzner_body() {
        // Verbatim 412 body shape we get back from POST /servers when
        // a CCX63 isn't currently available in the requested region.
        let e = anyhow!(
            "POST https://api.hetzner.cloud/v1/servers → 412 Precondition Failed: \
             {{\"error\":{{\"code\":\"resource_unavailable\",\
             \"message\":\"error during placement\",\"details\":{{}}}}}}"
        );
        assert!(crate::provider::hcloud::is_capacity_error(&e));
    }

    #[test]
    fn prepend_verb_when_user_passes_only_flags() {
        // `cargo burst check -- --all-targets` → cargo_args = ["--all-targets"]
        // Without the prepend, the body would become `cargo --all-targets`
        // and cargo would reject it.
        let out = prepend_cargo_verb("check", vec!["--all-targets".into()]);
        assert_eq!(out, vec!["check", "--all-targets"]);
    }

    #[test]
    fn prepend_verb_on_empty_args() {
        // `cargo burst check` (no `--` segment) should still run `cargo check`.
        assert_eq!(prepend_cargo_verb("check", vec![]), vec!["check"]);
    }

    #[test]
    fn prepend_verb_skips_when_user_already_typed_it() {
        // Legacy habit: `cargo burst build -- build --release`. Don't
        // double it up.
        let out = prepend_cargo_verb(
            "build",
            vec!["build".into(), "--release".into()],
        );
        assert_eq!(out, vec!["build", "--release"]);
    }

    #[test]
    fn prepend_verb_handles_leading_flags_then_different_verb() {
        // Cargo's own `+toolchain` style isn't supported here, but if
        // someone types `cargo burst clippy -- --all-targets check`,
        // the first non-flag token is `check`, which doesn't match
        // `clippy` — so we still prepend, producing
        // `cargo clippy --all-targets check`. Cargo will then complain
        // about the extra positional. That's fine — better than us
        // silently accepting nonsense.
        let out = prepend_cargo_verb(
            "clippy",
            vec!["--all-targets".into(), "check".into()],
        );
        assert_eq!(out, vec!["clippy", "--all-targets", "check"]);
    }

    #[test]
    fn other_412s_are_not_capacity_errors() {
        // Different 412 codes mean things we can't fix by trying
        // another region — must surface to the user.
        let e = anyhow!("POST → 412 Precondition Failed: code:\"invalid_input\"");
        assert!(!crate::provider::hcloud::is_capacity_error(&e));
        let e = anyhow!("POST → 401 Unauthorized");
        assert!(!crate::provider::hcloud::is_capacity_error(&e));
    }

    /// Pick env var names with a `CARGO_BURST_TEST_` prefix so we don't
    /// stomp on anything the user might already have set when running
    /// the suite locally. Tests that need to control a value set it
    /// explicitly; tests that need it unset use a name unlikely to
    /// exist (`_NEVER_SET_<rand>`).
    #[test]
    fn resolve_env_skips_unset_forward_entries() {
        // Pick a name that's almost certainly not in the test process's
        // environment. We never set it.
        let v = resolve_env(
            &["CARGO_BURST_TEST_DEFINITELY_UNSET_XYZZY".into()],
            &[],
        );
        assert!(v.is_empty(), "unset forward_env entries must be dropped");
    }

    #[test]
    fn resolve_env_picks_up_set_forward_entries() {
        // Set a unique-named var, ask resolve_env to forward it, expect
        // the resolved (k,v) pair.
        let name = "CARGO_BURST_TEST_FWD_OK";
        // SAFETY: tests can race other tests touching the same env var.
        // We use a unique name per test to avoid that.
        unsafe { std::env::set_var(name, "yes please"); }
        let v = resolve_env(&[name.into()], &[]);
        unsafe { std::env::remove_var(name); }
        assert_eq!(v, vec![(name.to_string(), "yes please".to_string())]);
    }

    #[test]
    fn resolve_env_cli_inline_value_wins() {
        let name = "CARGO_BURST_TEST_CLI_WINS";
        unsafe { std::env::set_var(name, "from-shell"); }
        let v = resolve_env(
            &[name.into()],
            &[format!("{name}=from-cli")],
        );
        unsafe { std::env::remove_var(name); }
        assert_eq!(v, vec![(name.to_string(), "from-cli".to_string())]);
    }

    #[test]
    fn resolve_env_cli_bare_name_resolves_locally() {
        let name = "CARGO_BURST_TEST_CLI_BARE";
        unsafe { std::env::set_var(name, "looked up"); }
        let v = resolve_env(&[], &[name.into()]);
        unsafe { std::env::remove_var(name); }
        assert_eq!(v, vec![(name.to_string(), "looked up".to_string())]);
    }

    #[test]
    fn resolve_env_value_with_equals_in_it_survives() {
        // `--env DSN=postgres://u:p@h/db?sslmode=disable` — the second
        // `=` is part of the value, not a key separator. split_once
        // handles this correctly; verify.
        let v = resolve_env(
            &[],
            &["DSN=postgres://u:p@h/db?sslmode=disable".into()],
        );
        assert_eq!(
            v,
            vec![(
                "DSN".to_string(),
                "postgres://u:p@h/db?sslmode=disable".to_string()
            )]
        );
    }

    #[test]
    fn resolve_env_drops_forbidden_names() {
        // Even if the user explicitly asks, we refuse PATH/HOME/CARGO_TARGET_DIR.
        let v = resolve_env(
            &["PATH".into(), "CARGO_TARGET_DIR".into()],
            &["HOME=/who/cares".into()],
        );
        assert!(v.is_empty(), "forbidden names must never reach the remote");
    }

    #[test]
    fn build_remote_cmd_emits_user_exports_after_pins() {
        let ctx = RemoteCtx {
            server_ip: "1.2.3.4".into(),
            ssh_key_path: PathBuf::from("/dev/null"),
            workspace_root: PathBuf::from("/tmp"),
            remote_src: "/home/work/src/abc".into(),
            target_dir: "/mnt/cache/abc/target".into(),
            sccache_dir: "/mnt/cache/abc/sccache".into(),
            env: vec![
                ("RUST_LOG".into(), "debug".into()),
                // Value with spaces + `?` glob meta — must be quoted
                // by shell_escape, otherwise the remote shell would
                // tokenise it into multiple positional args and the
                // export would be malformed.
                (
                    "MESSAGE".into(),
                    "hello world ?".into(),
                ),
            ],
        };
        let cmd = build_remote_cmd(&ctx, "cargo build");

        // Pins come first.
        let cargo_target_pos = cmd.find("export CARGO_TARGET_DIR").unwrap();
        let user_pos = cmd.find("export RUST_LOG").unwrap();
        assert!(
            cargo_target_pos < user_pos,
            "user-forwarded env must be exported AFTER our internal \
             pins so the user can't accidentally clobber them"
        );

        // Values that contain shell metacharacters get single-quoted.
        assert!(
            cmd.contains("export MESSAGE='hello world ?';"),
            "MESSAGE value must be shell-quoted; got: {cmd}"
        );
        // RUST_LOG=debug is alphanumeric only — shell_escape correctly
        // leaves it bare. Just confirm it's there.
        assert!(cmd.contains("export RUST_LOG=debug;"));
    }

    #[test]
    fn build_remote_cmd_pins_ci_and_no_color_before_user_exports() {
        // Regression: without CI=true the remote sees a PTY (via `-tt`
        // for signal propagation) and nextest switches to its
        // interactive reporter — carriage-return progress bar, no
        // newline-flushed line output, agent consumers see nothing.
        // Both pins must come BEFORE user_exports so a user `--env`
        // can still override them, but they're set by default.
        let ctx = RemoteCtx {
            server_ip: "1.2.3.4".into(),
            ssh_key_path: PathBuf::from("/dev/null"),
            workspace_root: PathBuf::from("/tmp"),
            remote_src: "/home/work/src/abc".into(),
            target_dir: "/mnt/cache/abc/target".into(),
            sccache_dir: "/mnt/cache/abc/sccache".into(),
            env: vec![("CI".into(), "user-override".into())],
        };
        let cmd = build_remote_cmd(&ctx, "cargo test");
        let internal_ci = cmd
            .find("export CI=true;")
            .expect("internal CI=true pin must be present");
        let internal_no_color = cmd
            .find("export CARGO_TERM_COLOR=never;")
            .expect("internal CARGO_TERM_COLOR=never pin must be present");
        let user_ci = cmd
            .find("export CI=user-override;")
            .expect("user override of CI must be exported");
        assert!(
            internal_ci < user_ci,
            "internal CI pin must come before user override so user wins"
        );
        assert!(
            internal_no_color < user_ci,
            "internal CARGO_TERM_COLOR pin must come before user exports too"
        );
    }

    #[test]
    fn build_remote_cmd_no_env_omits_user_exports_block() {
        let ctx = RemoteCtx {
            server_ip: "1.2.3.4".into(),
            ssh_key_path: PathBuf::from("/dev/null"),
            workspace_root: PathBuf::from("/tmp"),
            remote_src: "/home/work/src/abc".into(),
            target_dir: "/mnt/cache/abc/target".into(),
            sccache_dir: "/mnt/cache/abc/sccache".into(),
            env: vec![],
        };
        let cmd = build_remote_cmd(&ctx, "cargo check");
        // Should still produce valid syntax — the empty user_exports
        // block musn't leave a dangling `; ` that breaks the chain.
        assert!(cmd.contains("export PATH=$HOME/.cargo/bin:$PATH;"));
        assert!(cmd.contains("mkdir -p /mnt/cache/abc/target /mnt/cache/abc/sccache;"));
    }
}
