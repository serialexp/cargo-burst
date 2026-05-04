//! Persistent configuration and state.
//!
//! Two files under `$XDG_CONFIG_HOME/cargo-burst/` (typically
//! `~/.config/cargo-burst/`):
//!
//! - `config.toml` — user-edited settings (Hetzner token, defaults).
//! - `state.json` — tool-managed runtime state (known projects → volume IDs,
//!   currently-alive server, last image ID).
//!
//! Both are kept tiny and human-readable on purpose. If the tool ever gets
//! confused about state, deleting `state.json` and rerunning is meant to be a
//! safe recovery (volumes/servers will be re-discovered from Hetzner).

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// User-edited settings. Defaults are filled in for any missing field so
/// users only have to write the parts they actually want to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Hetzner Cloud API token (read+write). Required.
    pub hetzner_token: String,
    /// Hetzner location code (e.g. `hel1`, `nbg1`, `fsn1`, `ash`).
    #[serde(default = "default_region")]
    pub region: String,
    /// Hetzner server type (e.g. `ccx63`, `ccx53`, `ccx43`).
    #[serde(default = "default_server_type")]
    pub server_type: String,
    /// How long the server stays alive after the last successful build
    /// before being auto-deleted.
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u64,
    /// How long a project's volume stays alive after the last build for
    /// *that* project before being auto-deleted. Defaults to 1 hour.
    ///
    /// Volumes are billed by provisioned size, not by usage — a 200 GB
    /// volume is ~$10/month even if you only build once a week. Reaping
    /// idle volumes is far cheaper than keeping them warm; the cost of
    /// rebuilding the cache from scratch on the next build is one cargo
    /// fresh-build (~30s on a CCX63), which is dwarfed by a month of
    /// volume rent.
    #[serde(default = "default_volume_keep_alive")]
    pub volume_keep_alive_secs: u64,
    /// Default size for newly-created project volumes.
    #[serde(default = "default_volume_gb")]
    pub volume_gb: u32,
    /// Path to the SSH private key cargo-burst uses to talk to the server.
    /// Defaults to `<config_dir>/ssh_key`. Created on first use if missing.
    #[serde(default)]
    pub ssh_key_path: Option<PathBuf>,
}

fn default_region() -> String { "hel1".into() }
fn default_server_type() -> String { "ccx63".into() }
fn default_keep_alive() -> u64 { 300 }
fn default_volume_gb() -> u32 { 200 }
fn default_volume_keep_alive() -> u64 { 3600 }

/// Tool-managed runtime state. Updated on every successful operation;
/// safe to delete to force re-discovery from Hetzner.
///
/// One server is shared across all projects — provisioning a fresh CCX-class
/// box per project would (a) blow Bart's dedicated-core quota and (b) waste
/// money since concurrent builds on one machine are perfectly fine. The
/// server lives at the top level; per-project state is a volume id plus
/// each project's own last-used timestamp (used to reap idle volumes
/// independently of the server).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// ID of the currently-baked base image, if any.
    #[serde(default)]
    pub image_id: Option<i64>,
    /// ID of the shared server, if currently alive. Cleared by the reaper
    /// when it deletes the server.
    #[serde(default)]
    pub server_id: Option<i64>,
    /// Per-project state, keyed by `project::ProjectKey` (sha256-prefix of
    /// the workspace root path).
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectState>,
}

impl State {
    /// Most-recent `last_used_rfc3339` across every project, parsed. Returns
    /// `None` if no project has a parseable timestamp. Used by the *server*
    /// reaper, which doesn't care which project ran a build — only whether
    /// *some* project ran one inside the keep-alive window.
    pub fn last_used_any(&self) -> Option<time::OffsetDateTime> {
        self.projects
            .values()
            .filter_map(|p| p.last_used_rfc3339.as_deref())
            .filter_map(|s| {
                time::OffsetDateTime::parse(
                    s,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok()
            })
            .max()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    /// Human-readable workspace root path (for `status` output).
    pub workspace_path: String,
    /// Hetzner volume ID for this project's `target/` cache, if created.
    /// Cleared by the volume reaper when it deletes the volume.
    #[serde(default)]
    pub volume_id: Option<i64>,
    /// Last time *this* project ran a build (RFC3339). Drives both reapers:
    /// the server reaper takes the max across all projects (it doesn't care
    /// which project was active, only that *some* project was), while the
    /// volume reaper compares this project's timestamp against its own
    /// spawn time.
    #[serde(default)]
    pub last_used_rfc3339: Option<String>,
    /// User-confirmed extra rsync excludes (on top of `ssh::DEFAULT_EXCLUDES`).
    /// `None` means we've never prompted the user about excludes for this
    /// project — the next build will offer the suggested-excludes flow.
    /// `Some(_)` (even empty) means the user has been prompted and the
    /// flow should not re-run.
    #[serde(default)]
    pub excludes: Option<Vec<String>>,
}

/// Resolve the per-user config directory, creating it on demand.
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "serialexp", "cargo-burst")
        .ok_or_else(|| anyhow!("could not resolve XDG config directory"))?;
    let dir = dirs.config_dir().to_path_buf();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn config_path() -> Result<PathBuf> { Ok(config_dir()?.join("config.toml")) }
pub fn state_path()  -> Result<PathBuf> { Ok(config_dir()?.join("state.json")) }

impl Config {
    /// Load `config.toml`. Returns a helpful error pointing at the path if
    /// missing — first-run UX is "the tool tells you where to put the file".
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "config not found at {}. Create it with at least:\n  hetzner_token = \"…\"",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if cfg.hetzner_token.trim().is_empty() {
            return Err(anyhow!(
                "hetzner_token is empty in {}",
                path.display()
            ));
        }
        Ok(cfg)
    }

    /// Resolved SSH key path: explicit `ssh_key_path` if set, else the
    /// per-config default.
    pub fn ssh_key_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.ssh_key_path {
            return Ok(p.clone());
        }
        Ok(config_dir()?.join("ssh_key"))
    }
}

impl State {
    /// Load `state.json`, treating "missing" as "empty state".
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
        }
    }

    /// Atomic-ish save: write to `state.json.tmp` then rename. Avoids
    /// half-written state if the tool is killed mid-write.
    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("renaming {}", tmp.display()))?;
        Ok(())
    }
}

/// Cooperative cross-process exclusive lock guarding mutations to
/// `state.json`. cargo-burst processes can run concurrently (different
/// projects' builds against the same shared server), and they all touch
/// the same state file. The lock serialises:
///
/// - Server provisioning (so two concurrent first-time builds don't each
///   create their own CCX63 — see `ensure_shared_server`).
/// - Volume create/attach.
/// - Read-modify-write of `state.json`. Without the lock, two processes
///   loading state, mutating different fields, and saving back would lose
///   one set of changes.
///
/// Implemented via `flock(LOCK_EX)` on `<config_dir>/state.lock`. POSIX
/// closes the fd (and releases the lock) automatically on process death,
/// so a crashed `cargo burst` never leaves a stuck lock — no PID-file
/// liveness check needed.
pub struct StateLock {
    /// Held only to keep the fd open. Drop releases the flock.
    _file: fs::File,
}

impl StateLock {
    /// Acquire the lock, blocking until it's available. Logs a one-time
    /// "waiting" message so the user knows why their command is paused.
    ///
    /// Polled rather than blocking-syscalled so we can stay async-friendly:
    /// `flock`'s blocking variant would tie up the tokio runtime thread
    /// indefinitely. The poll loop sleeps 500ms between attempts.
    pub async fn acquire() -> Result<Self> {
        use fs2::FileExt;
        let path = config_dir()?.join("state.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        let mut announced = false;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !announced {
                        tracing::info!(
                            "waiting for another cargo-burst process to release the state lock…"
                        );
                        announced = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    return Err(anyhow!("locking {}: {e}", path.display()));
                }
            }
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // Drop closes the file, which releases the flock. Explicit unlock
        // here would be redundant.
    }
}

/// Read-modify-write `state.json` under the global lock. Use this for any
/// state mutation outside the long-held provisioning lock — every save
/// re-reads the file first, so concurrent processes can't clobber each
/// other's changes.
pub async fn update_state<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut State) -> Result<()>,
{
    let _lock = StateLock::acquire().await?;
    let mut state = State::load()?;
    f(&mut state)?;
    state.save()?;
    Ok(())
}
