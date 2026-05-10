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
use std::path::{Path, PathBuf};

/// User-edited settings. Defaults are filled in for any missing field so
/// users only have to write the parts they actually want to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Hetzner Cloud API token (read+write). Required.
    pub hetzner_token: String,
    /// Hetzner location codes (e.g. `hel1`, `nbg1`, `fsn1`, `ash`).
    /// Accepts either a single string or a list — both are upgraded
    /// to a list internally:
    ///
    ///     region = "hel1"                    # single region
    ///     region = ["hel1", "fsn1"]          # ordered fallback list
    ///
    /// `regions` is an exact alias kept for users who prefer the
    /// pluralised name; if both are set, `regions` wins. When the
    /// first region returns Hetzner's "resource_unavailable" capacity
    /// error, we fall through to the next, and so on.
    ///
    /// Volumes are regional in Hetzner — they only attach to servers
    /// in the same location — so falling back to a different region
    /// requires recreating the project volume there. The old volume
    /// is deleted (it's a build cache; the loss is one fresh-build
    /// penalty, ~30s on a CCX63), and the next session continues in
    /// the fallback region until that one runs out too.
    #[serde(default = "default_region", deserialize_with = "one_or_many")]
    pub region: Vec<String>,
    /// Plural alias for `region`. Same string-or-list semantics. If
    /// both `region` and `regions` are set, `regions` wins.
    #[serde(default, deserialize_with = "one_or_many")]
    pub regions: Vec<String>,
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
    /// Names of local environment variables to forward to every remote
    /// `cargo …` invocation. Names whose local value is unset are
    /// silently skipped (debug-logged), so it's fine to keep
    /// `RUST_LOG` in the list even on runs where you didn't set it.
    ///
    /// Per-run `--env VAR` and `--env VAR=value` always win over this
    /// list; project-level config (`<workspace>/.config/cargo-burst.toml`)
    /// is appended to this list (additive merge).
    ///
    /// Default empty — opting in is explicit so we never silently
    /// leak host-specific state.
    #[serde(default)]
    pub forward_env: Vec<String>,
    /// Patterns added to the rsync exclude list on top of cargo-burst's
    /// built-in defaults (see `ssh::DEFAULT_EXCLUDES`). Typically
    /// project-scoped (set in `<workspace>/.config/cargo-burst.toml`)
    /// rather than global, but accepted at the global level too for
    /// patterns the user always wants excluded everywhere.
    #[serde(default)]
    pub extra_excludes: Vec<String>,
    /// Default-exclude patterns to NOT apply (as exact-match strings
    /// against `ssh::DEFAULT_EXCLUDES`). The common project-scoped
    /// case is `[".git/"]` for repos whose binary needs the working
    /// tree to be a real git checkout. Like `extra_excludes`, accepted
    /// at the global level too — set globally if you ALWAYS want a
    /// particular default off.
    #[serde(default)]
    pub unexclude: Vec<String>,
}

/// All-optional patch deserialised from
/// `<workspace>/.config/cargo-burst.toml`. Fields that are `Some(_)`
/// override the corresponding global config field; `forward_env` is
/// the one exception — it's *additive* (project list appended to
/// global, deduped) so a project can add to the global allow-list
/// without having to repeat what the user already configured globally.
///
/// `hetzner_token` is intentionally accepted by the deserializer (so
/// a typo'd token field doesn't fail parsing in some confusing way)
/// but rejected with a clear error in [`Config::load_for_workspace`] —
/// committing a token is a security incident waiting to happen.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Refused — present only so the deserializer can produce a
    /// targeted error rather than "unknown field".
    #[serde(default)]
    pub hetzner_token: Option<String>,
    #[serde(default, deserialize_with = "one_or_many_opt")]
    pub region: Option<Vec<String>>,
    #[serde(default, deserialize_with = "one_or_many_opt")]
    pub regions: Option<Vec<String>>,
    #[serde(default)]
    pub server_type: Option<String>,
    #[serde(default)]
    pub keep_alive_secs: Option<u64>,
    #[serde(default)]
    pub volume_keep_alive_secs: Option<u64>,
    #[serde(default)]
    pub volume_gb: Option<u32>,
    #[serde(default)]
    pub ssh_key_path: Option<PathBuf>,
    /// Appended to the global `forward_env`, deduped (preserves
    /// global ordering, then project ordering for new entries).
    #[serde(default)]
    pub forward_env: Option<Vec<String>>,
    /// Patterns added to the rsync exclude list on top of cargo-burst's
    /// built-in defaults. Useful for project-specific clutter that
    /// shouldn't reach the remote (large fixtures, local-only outputs).
    /// Composed after `unexclude`, so an entry here can re-add an
    /// extension of something `unexclude` removed (rare, but possible).
    #[serde(default)]
    pub extra_excludes: Option<Vec<String>>,
    /// Built-in default-exclude patterns to NOT apply for this project.
    /// Pattern matching is by exact string equality against
    /// `ssh::DEFAULT_EXCLUDES`. Most common use: `unexclude = [".git/"]`
    /// when a binary needs the repo to be present on the remote (build
    /// stamping, git-introspecting code, scripts that shell out to
    /// `git`). Entries that don't match any default are warned about
    /// once at sync time but not fatal — typos shouldn't block builds.
    #[serde(default)]
    pub unexclude: Option<Vec<String>>,
}

fn default_region() -> Vec<String> { vec!["hel1".into()] }

/// Accept either a TOML string or a TOML array-of-strings and produce
/// a `Vec<String>`. Lets `region = "hel1"` and `region = ["hel1", "fsn1"]`
/// both round-trip through the same field, so users don't have to
/// remember whether the field is singular or plural.
fn one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// `Option<Vec<String>>` variant of [`one_or_many`]: a missing field
/// stays `None` (so we can tell "user didn't set this in the project
/// file" apart from "user explicitly set it to []") while a present
/// string-or-array deserialises into `Some(_)`.
fn one_or_many_opt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Option::<OneOrMany>::deserialize(deserializer).map(|opt| {
        opt.map(|v| match v {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        })
    })
}
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
    /// Session accounting for the currently-alive shared server. Set
    /// when `ensure_shared_server` provisions a fresh box, incremented
    /// per cargo phase, finalized into a `server_terminated` audit
    /// event when the server is deleted. `None` between sessions.
    /// Lives in `state.json` rather than memory because a single
    /// session can span many cargo-burst invocations of the same
    /// long-lived server.
    #[serde(default)]
    pub current_server_session: Option<ServerSession>,
}

/// In-flight accounting for a single server lifetime. Persisted to
/// `state.json` so the count survives across cargo-burst invocations
/// (the shared server outlives each individual `cargo burst <verb>`
/// run). Finalized — and zeroed — when the server is destroyed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSession {
    pub server_id: i64,
    /// RFC3339 timestamp the server was provisioned at.
    pub started_at: String,
    /// Cumulative count of cargo phases that ran during this lifetime.
    /// Bumped by `with_remote` after the closure returns (regardless
    /// of success — a failed `cargo build` still consumed remote
    /// resources).
    pub command_count: u32,
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
    /// RFC3339 timestamp the current volume was provisioned at. Set
    /// when `ensure_volume` creates a fresh volume, cleared when the
    /// volume reaper destroys it. Used to compute volume lifetime in
    /// the `volume_terminated` audit event.
    #[serde(default)]
    pub volume_started_at: Option<String>,
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

/// Path to the project-level config file inside a workspace, if it
/// exists. Mirrors cargo-nextest's convention of stashing per-project
/// tool config under `<workspace>/.config/<tool>.toml` so projects
/// don't accumulate a flotilla of dotfiles at the root.
pub fn project_config_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".config").join("cargo-burst.toml")
}

impl Config {
    /// Load `config.toml`. Returns a helpful error pointing at the path if
    /// missing — first-run UX is "the tool tells you where to put the file".
    pub fn load() -> Result<Self> {
        Self::load_for_workspace(None)
    }

    /// Load global config and, if `workspace_root` is `Some(_)` and a
    /// `<workspace>/.config/cargo-burst.toml` exists, layer it on top.
    ///
    /// Project-config rules (kept in lockstep with the docs on
    /// [`ProjectConfig`]):
    ///
    /// - All non-`forward_env` fields *replace* the global value when set.
    /// - `forward_env` is *additive* — project list appended to global,
    ///   deduped, preserving the global ordering for entries that
    ///   appear in both.
    /// - `hetzner_token` is rejected outright; tokens belong in the
    ///   per-user global config, not in a file that gets committed.
    pub fn load_for_workspace(workspace_root: Option<&Path>) -> Result<Self> {
        let path = config_path()?;
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "config not found at {}. Create it with at least:\n  hetzner_token = \"…\"",
                path.display()
            )
        })?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if cfg.hetzner_token.trim().is_empty() {
            return Err(anyhow!(
                "hetzner_token is empty in {}",
                path.display()
            ));
        }

        if let Some(root) = workspace_root {
            let proj_path = project_config_path(root);
            match fs::read_to_string(&proj_path) {
                Ok(proj_text) => {
                    let project: ProjectConfig = toml::from_str(&proj_text)
                        .with_context(|| format!("parsing {}", proj_path.display()))?;
                    if project.hetzner_token.is_some() {
                        return Err(anyhow!(
                            "{} sets `hetzner_token`, which is refused at the \
                             project level — tokens belong in your global \
                             {} (this file is meant to be committed; tokens are not)",
                            proj_path.display(),
                            path.display(),
                        ));
                    }
                    cfg.merge_project(project);
                    tracing::debug!(
                        path = %proj_path.display(),
                        "applied project config"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow!("reading {}: {e}", proj_path.display()));
                }
            }
        }

        Ok(cfg)
    }

    /// Apply a [`ProjectConfig`] patch in place. Replace semantics for
    /// every field except `forward_env`, which is appended-and-deduped.
    /// Public for tests; production callers should go through
    /// [`Config::load_for_workspace`].
    pub fn merge_project(&mut self, project: ProjectConfig) {
        if let Some(v) = project.region {
            self.region = v;
        }
        if let Some(v) = project.regions {
            self.regions = v;
        }
        if let Some(v) = project.server_type {
            self.server_type = v;
        }
        if let Some(v) = project.keep_alive_secs {
            self.keep_alive_secs = v;
        }
        if let Some(v) = project.volume_keep_alive_secs {
            self.volume_keep_alive_secs = v;
        }
        if let Some(v) = project.volume_gb {
            self.volume_gb = v;
        }
        if let Some(v) = project.ssh_key_path {
            self.ssh_key_path = Some(v);
        }
        if let Some(extra) = project.forward_env {
            // Preserve global ordering for shared entries; append
            // project-only entries in their original order. Linear
            // scans are fine — these lists are typically a handful
            // of names, not thousands.
            for name in extra {
                if !self.forward_env.iter().any(|n| n == &name) {
                    self.forward_env.push(name);
                }
            }
        }
        // Excludes: both fields are additive across global → project,
        // deduped. Replace semantics would be surprising here — a
        // project specifying `extra_excludes = ["foo"]` shouldn't
        // silently undo a global pattern.
        if let Some(extra) = project.extra_excludes {
            for pat in extra {
                if !self.extra_excludes.iter().any(|p| p == &pat) {
                    self.extra_excludes.push(pat);
                }
            }
        }
        if let Some(extra) = project.unexclude {
            for pat in extra {
                if !self.unexclude.iter().any(|p| p == &pat) {
                    self.unexclude.push(pat);
                }
            }
        }
    }

    /// Ordered list of regions to try for server provisioning.
    /// `regions` wins if set; otherwise `region`. If both are empty
    /// (only possible when the user explicitly writes `region = []`)
    /// we fall back to the global default so callers always get at
    /// least one entry.
    pub fn region_preference(&self) -> Vec<String> {
        if !self.regions.is_empty() {
            self.regions.clone()
        } else if !self.region.is_empty() {
            self.region.clone()
        } else {
            default_region()
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        toml::from_str(toml).expect("parse config")
    }

    #[test]
    fn region_accepts_single_string() {
        let cfg = parse(r#"hetzner_token = "x"
region = "hel1""#);
        assert_eq!(cfg.region, vec!["hel1".to_string()]);
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }

    #[test]
    fn region_accepts_array() {
        let cfg = parse(r#"hetzner_token = "x"
region = ["hel1", "fsn1", "nbg1"]"#);
        assert_eq!(cfg.region, vec!["hel1", "fsn1", "nbg1"]);
        assert_eq!(cfg.region_preference(), vec!["hel1", "fsn1", "nbg1"]);
    }

    #[test]
    fn regions_alias_accepts_single_string() {
        let cfg = parse(r#"hetzner_token = "x"
regions = "fsn1""#);
        assert_eq!(cfg.regions, vec!["fsn1".to_string()]);
        // Plural wins when present, even as a single string.
        assert_eq!(cfg.region_preference(), vec!["fsn1".to_string()]);
    }

    #[test]
    fn regions_alias_accepts_array() {
        let cfg = parse(r#"hetzner_token = "x"
regions = ["hel1", "fsn1"]"#);
        assert_eq!(cfg.regions, vec!["hel1", "fsn1"]);
        assert_eq!(cfg.region_preference(), vec!["hel1", "fsn1"]);
    }

    #[test]
    fn plural_regions_wins_when_both_set() {
        let cfg = parse(r#"hetzner_token = "x"
region = "hel1"
regions = ["fsn1", "nbg1"]"#);
        // Both fields keep their parsed values; preference picks regions.
        assert_eq!(cfg.region, vec!["hel1".to_string()]);
        assert_eq!(cfg.regions, vec!["fsn1", "nbg1"]);
        assert_eq!(cfg.region_preference(), vec!["fsn1", "nbg1"]);
    }

    #[test]
    fn region_defaults_when_neither_field_set() {
        let cfg = parse(r#"hetzner_token = "x""#);
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }

    fn parse_project(toml: &str) -> ProjectConfig {
        toml::from_str(toml).expect("parse project config")
    }

    #[test]
    fn project_replaces_simple_fields() {
        let mut cfg = parse(r#"hetzner_token = "x""#);
        let proj = parse_project(r#"
server_type = "ccx53"
volume_gb = 50
keep_alive_secs = 60
"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.server_type, "ccx53");
        assert_eq!(cfg.volume_gb, 50);
        assert_eq!(cfg.keep_alive_secs, 60);
        // Untouched defaults survive.
        assert_eq!(cfg.volume_keep_alive_secs, default_volume_keep_alive());
    }

    #[test]
    fn project_replaces_regions_not_merges() {
        let mut cfg = parse(r#"hetzner_token = "x"
region = ["hel1", "fsn1"]"#);
        let proj = parse_project(r#"region = "nbg1""#);
        cfg.merge_project(proj);
        // Replace, not extend — a project saying "I want nbg1" doesn't
        // mean "and also keep all the global fallbacks I never asked
        // for". The whole list is the project's intent.
        assert_eq!(cfg.region, vec!["nbg1".to_string()]);
    }

    #[test]
    fn project_forward_env_is_additive_and_deduped() {
        let mut cfg = parse(r#"hetzner_token = "x"
forward_env = ["RUST_LOG", "RUST_BACKTRACE"]"#);
        let proj = parse_project(r#"forward_env = ["DATABASE_URL", "RUST_LOG"]"#);
        cfg.merge_project(proj);
        // Global ordering preserved; project-only entries appended;
        // RUST_LOG appears once even though it's in both lists.
        assert_eq!(
            cfg.forward_env,
            vec!["RUST_LOG", "RUST_BACKTRACE", "DATABASE_URL"]
        );
    }

    #[test]
    fn project_extra_excludes_are_additive_and_deduped() {
        let mut cfg = parse(r#"hetzner_token = "x"
extra_excludes = ["fixtures/large/"]"#);
        let proj = parse_project(r#"extra_excludes = ["fixtures/large/", "*.bak"]"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.extra_excludes, vec!["fixtures/large/", "*.bak"]);
    }

    #[test]
    fn project_unexclude_is_additive_and_deduped() {
        let mut cfg = parse(r#"hetzner_token = "x"
unexclude = [".git/"]"#);
        let proj = parse_project(r#"unexclude = [".git/", ".vscode/"]"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.unexclude, vec![".git/", ".vscode/"]);
    }

    #[test]
    fn project_unexclude_only_set_at_project_level_works() {
        // No global unexclude, project adds one.
        let mut cfg = parse(r#"hetzner_token = "x""#);
        cfg.merge_project(parse_project(r#"unexclude = [".git/"]"#));
        assert_eq!(cfg.unexclude, vec![".git/"]);
    }

    #[test]
    fn project_unset_fields_dont_clobber_global() {
        let mut cfg = parse(r#"hetzner_token = "x"
server_type = "ccx63"
volume_gb = 200"#);
        // Empty project file — should be a no-op.
        cfg.merge_project(parse_project(""));
        assert_eq!(cfg.server_type, "ccx63");
        assert_eq!(cfg.volume_gb, 200);
    }

    #[test]
    fn project_token_is_caught_at_deserialize() {
        // The deserializer accepts `hetzner_token` so we can produce a
        // targeted error in load_for_workspace; the parsed struct
        // surfaces it as Some(_).
        let proj = parse_project(r#"hetzner_token = "leaked""#);
        assert_eq!(proj.hetzner_token.as_deref(), Some("leaked"));
    }

    #[test]
    fn project_unknown_field_fails_to_parse() {
        // deny_unknown_fields keeps typos honest.
        let result: Result<ProjectConfig, _> =
            toml::from_str(r#"servr_type = "ccx53""#);
        assert!(result.is_err(), "expected typo to fail");
    }

    #[test]
    fn explicit_empty_array_falls_back_to_default() {
        // `region = []` would otherwise produce an empty preference
        // list, which every caller would have to defend against.
        // region_preference returns the global default in that case.
        let cfg = parse(r#"hetzner_token = "x"
region = []"#);
        assert!(cfg.region.is_empty());
        assert!(cfg.regions.is_empty());
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }
}
