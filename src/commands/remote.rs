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
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::commands::reap;
use crate::config::{self, Config, ProjectState, State, StateLock};
use crate::hcloud::{
    CreateServerRequest, CreateVolumeRequest, HCloud, ImageRef, Server, Volume,
};
use crate::project::{ProjectKey, SHARED_SERVER_NAME};
use crate::ssh;

/// How often the heartbeat task bumps `last_used_rfc3339` while a build
/// or test run is in progress. Must be << `keep_alive_secs` so a previous
/// reaper, when it wakes mid-current-run, sees a fresh timestamp and
/// bows out instead of deleting the server out from under us.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Hetzner server-create polling for "running" status.
const SERVER_BOOT_TIMEOUT: Duration = Duration::from_secs(180);
/// SSH come-up window after the server reports "running".
const SSH_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

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
    let cfg = Config::load()?;
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    let cwd = std::env::current_dir().context("getting current dir")?;
    let project = ProjectKey::discover(&cwd)?;
    tracing::info!(
        workspace = %project.workspace_root.display(),
        hash = %project.hash,
        "resolved cargo workspace"
    );

    // Wall-time anchor for the audit log's `provision_secs` — we want
    // to attribute "everything before cargo runs" to provisioning, so
    // we start the timer at the very top of `with_remote` and stop it
    // after `wait_for_ssh` returns (when the box is actually ready).
    let provision_start = Instant::now();

    // Provisioning block: image check, ssh key, scan, volume, server,
    // attach. Held under the state lock so two concurrent burst commands
    // can't each spin up their own CCX63 (Hetzner happily creates two
    // servers with the same name) or clobber each other's saves with
    // stale in-memory state.
    let ssh_key_path = cfg.ssh_key_path()?;
    let pubkey = ssh::ensure_ssh_key(&ssh_key_path).await?;
    let (server, fresh_server, volume, fresh_volume, extra_excludes) = {
        let _lock = StateLock::acquire().await?;
        let mut state = State::load()?;

        state
            .projects
            .entry(project.hash.clone())
            .or_insert_with(|| ProjectState {
                workspace_path: project.workspace_root.display().to_string(),
                volume_id: None,
                last_used_rfc3339: None,
                excludes: None,
                volume_started_at: None,
            });

        let image_id = state
            .image_id
            .ok_or_else(|| anyhow!("no baked image yet — run `cargo burst image build` first"))?;

        let hetzner_key = hcloud.ensure_ssh_key("cargo-burst", &pubkey).await?;

        // Local size summary + (on first run) confirm excludes. Doing
        // it before provisioning means the user sees what's about to
        // be sync'd before any cloud resources are created — and lets
        // us bail early if they Ctrl-C at the prompt.
        let extra_excludes = scan_and_confirm_excludes(&project, &mut state, opts.yes)?;

        let (volume, fresh_volume) = ensure_volume(&hcloud, &cfg, &project, &mut state).await?;
        let (server, fresh_server) =
            ensure_shared_server(&hcloud, &cfg, image_id, hetzner_key.id, &mut state).await?;
        let volume = ensure_volume_attached(&hcloud, volume, server.id).await?;

        // Bump this project's last_used immediately — see reap.rs's
        // "(2) Activity-since-spawn" logic. Without this, a previous
        // reaper firing mid-our-run would happily delete the server
        // out from under us.
        if let Some(p) = state.projects.get_mut(&project.hash) {
            p.last_used_rfc3339 = Some(now_rfc3339());
        }
        state.save()?;
        (server, fresh_server, volume, fresh_volume, extra_excludes)
        // _lock drops here; concurrent runs can now provision.
    };

    let server_ip = server
        .public_net
        .ipv4
        .as_ref()
        .map(|i| i.ip.clone())
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
    let mount_script = render_mount_script(volume.id, &project.hash);
    let extra: Vec<&str> = extra_excludes.iter().map(String::as_str).collect();
    let sync_start = Instant::now();
    tracing::info!(
        volume = volume.id,
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
        &extra,
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
    let server_id_for_audit = server.id;
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
        if let Err(e) = reap::spawn_server(server.id, keep_alive) {
            tracing::warn!("failed to spawn server reaper: {e}");
        }
        if let Err(e) = reap::spawn_volume(volume.id, project.hash.clone(), volume_keep_alive) {
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

async fn ensure_volume(
    hcloud: &HCloud,
    cfg: &Config,
    project: &ProjectKey,
    state: &mut State,
) -> Result<(Volume, bool)> {
    let p = state.projects.get_mut(&project.hash).expect("inserted earlier");

    if let Some(id) = p.volume_id {
        match hcloud.get_volume(id).await {
            Ok(v) => return Ok((v, false)),
            Err(e) => {
                tracing::warn!("volume {id} from state.json not found ({e}); will recreate");
                // Volume vanished server-side; close out the lifetime
                // ledger before the upcoming `begin_volume_session`
                // would emit an `orphaned` for it anyway.
                crate::audit::end_volume_session(p, &project.hash, id, crate::audit::TerminationReason::Stale);
                p.volume_id = None;
            }
        }
    }
    if let Some(v) = hcloud.find_volume(&project.volume_name()).await? {
        tracing::info!(id = v.id, "found existing volume by name");
        p.volume_id = Some(v.id);
        return Ok((v, false));
    }

    tracing::info!(
        name = %project.volume_name(),
        size_gb = cfg.volume_gb,
        "creating new volume"
    );
    let mut labels = HashMap::new();
    labels.insert("managed-by".into(), "cargo-burst".into());
    labels.insert("project-hash".into(), project.hash.clone());
    let resp = hcloud
        .create_volume(CreateVolumeRequest {
            name: project.volume_name(),
            size: cfg.volume_gb,
            location: cfg.region.clone(),
            format: "ext4".into(),
            labels,
            automount: false,
        })
        .await?;
    if let Some(action) = resp.action {
        hcloud.wait_action(action.id, Duration::from_secs(120)).await?;
    }
    for action in resp.next_actions {
        hcloud.wait_action(action.id, Duration::from_secs(120)).await?;
    }
    p.volume_id = Some(resp.volume.id);
    crate::audit::begin_volume_session(
        p,
        &project.hash,
        resp.volume.id,
        cfg.volume_gb,
        &cfg.region,
    );
    Ok((resp.volume, true))
}

async fn ensure_shared_server(
    hcloud: &HCloud,
    cfg: &Config,
    image_id: i64,
    ssh_key_id: i64,
    state: &mut State,
) -> Result<(Server, bool)> {
    if let Some(id) = state.server_id {
        match hcloud.get_server(id).await {
            Ok(s) if matches!(s.status.as_str(), "running" | "starting" | "initializing") => {
                tracing::info!(id, status = %s.status, "reusing shared server from state");
                return Ok((s, false));
            }
            Ok(s) => {
                tracing::warn!(id, status = %s.status, "stale server in state; deleting");
                let _ = hcloud.delete_server(id).await;
                crate::audit::end_server_session(state, id, crate::audit::TerminationReason::Stale);
                state.server_id = None;
            }
            Err(e) => {
                tracing::warn!("server {id} from state.json not found ({e}); will recreate");
                // Server vanished server-side. Close the lifetime
                // ledger so the next provision starts a fresh session.
                crate::audit::end_server_session(state, id, crate::audit::TerminationReason::Stale);
                state.server_id = None;
            }
        }
    }

    if let Some(s) = hcloud.find_server(SHARED_SERVER_NAME).await? {
        if matches!(s.status.as_str(), "running" | "starting" | "initializing") {
            tracing::info!(id = s.id, status = %s.status, "found shared server by name");
            state.server_id = Some(s.id);
            return Ok((s, false));
        }
        tracing::warn!(id = s.id, status = %s.status, "deleting stale server found by name");
        let _ = hcloud.delete_server(s.id).await;
        crate::audit::end_server_session(state, s.id, crate::audit::TerminationReason::Stale);
    }

    tracing::info!(
        name = SHARED_SERVER_NAME,
        image = image_id,
        server_type = %cfg.server_type,
        "provisioning shared server"
    );
    let mut labels = HashMap::new();
    labels.insert("managed-by".into(), "cargo-burst".into());
    labels.insert("role".into(), "shared".into());
    let create = hcloud
        .create_server(CreateServerRequest {
            name: SHARED_SERVER_NAME.to_string(),
            server_type: cfg.server_type.clone(),
            image: ImageRef::Id(image_id),
            location: cfg.region.clone(),
            ssh_keys: vec![ssh_key_id],
            volumes: vec![],
            user_data: None,
            labels,
            start_after_create: true,
        })
        .await?;

    hcloud
        .wait_action(create.action.id, SERVER_BOOT_TIMEOUT)
        .await
        .context("waiting for server-create action")?;

    let server = hcloud.get_server(create.server.id).await?;
    state.server_id = Some(server.id);
    crate::audit::begin_server_session(
        state,
        server.id,
        &cfg.server_type,
        image_id,
        &cfg.region,
    );
    Ok((server, true))
}

async fn ensure_volume_attached(
    hcloud: &HCloud,
    volume: Volume,
    server_id: i64,
) -> Result<Volume> {
    match volume.server {
        Some(id) if id == server_id => {
            tracing::info!(volume = volume.id, "volume already attached to shared server");
            Ok(volume)
        }
        Some(other) => Err(anyhow!(
            "volume {} is attached to server {other}, not the shared server {server_id}. \
             Detach manually if intentional.",
            volume.id
        )),
        None => {
            tracing::info!(volume = volume.id, server = server_id, "attaching volume");
            hcloud.attach_volume(volume.id, server_id).await?;
            hcloud.get_volume(volume.id).await
        }
    }
}

/// Render the inline mount script we ssh-execute. Idempotent: short-
/// circuits if already mounted, formats ext4 on first use, waits up to
/// ~10s for the device file to appear after attach.
fn render_mount_script(volume_id: i64, hash: &str) -> String {
    format!(
        r#"sudo bash -s <<'BURST_MOUNT'
set -euo pipefail
target=/mnt/cache/{hash}
dev=/dev/disk/by-id/scsi-0HC_Volume_{volume_id}

if mountpoint -q "$target"; then
    exit 0
fi

for _ in $(seq 1 20); do
    [ -e "$dev" ] && break
    sleep 0.5
done
if [ ! -e "$dev" ]; then
    echo "volume {volume_id} not attached (device $dev never appeared)" >&2
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
    project: &ProjectKey,
    state: &mut State,
    auto_yes: bool,
) -> Result<Vec<String>> {
    let saved = state
        .projects
        .get(&project.hash)
        .and_then(|p| p.excludes.clone());
    let mut user_excludes: Vec<String> = saved.clone().unwrap_or_default();

    let merged = build_merged_excludes(&user_excludes);
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
    if first_run && !auto_yes {
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

fn build_merged_excludes(user: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ssh::DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    v.extend(user.iter().cloned());
    v
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
    let cargo_args = if cargo_args.is_empty() {
        vec![default_verb.to_string()]
    } else {
        cargo_args
    };
    with_remote(opts, label, move |ctx: RemoteCtx| async move {
        let escaped: Vec<String> = cargo_args.iter().map(|a| shell_escape(a)).collect();
        let cmd = build_remote_cmd(&ctx, &format!("cargo {}", escaped.join(" ")));
        let status = ssh::run_remote(&ctx.server_ip, "work", &ctx.ssh_key_path, &cmd).await?;
        if !status.success() {
            return Err(anyhow!("cargo exited with status {status}"));
        }
        Ok(())
    })
    .await
}

/// Render the boilerplate that wraps every cargo invocation: pin
/// `CARGO_TARGET_DIR` and `SCCACHE_DIR` to the project's volume slot,
/// put `~/.cargo/bin` on PATH (so `cargo-nextest` is reachable), make
/// sure those dirs exist, and `cd` into the rsync'd source.
///
/// `body` is appended after the prelude — typically one or more
/// `cargo …` invocations. Combine with `&&` if you're chaining.
pub fn build_remote_cmd(ctx: &RemoteCtx, body: &str) -> String {
    format!(
        "set -euo pipefail; \
         export CARGO_TARGET_DIR={target}; \
         export SCCACHE_DIR={sccache}; \
         export PATH=$HOME/.cargo/bin:$PATH; \
         mkdir -p {target} {sccache}; \
         cd {src}; \
         {body}",
        target = ctx.target_dir,
        sccache = ctx.sccache_dir,
        src = ctx.remote_src,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_to_verb_canonicalizes_test() {
        assert_eq!(label_to_verb("Tests"), "test");
    }

    #[test]
    fn label_to_verb_lowercases_others() {
        assert_eq!(label_to_verb("Build"), "build");
        assert_eq!(label_to_verb("Check"), "check");
        assert_eq!(label_to_verb("Clippy"), "clippy");
        assert_eq!(label_to_verb("Bench"), "bench");
    }
}
