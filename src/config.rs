//! Persistent configuration and state.
//!
//! Two files under `$XDG_CONFIG_HOME/cargo-burst/` (typically
//! `~/.config/cargo-burst/`):
//!
//! - `config.toml` — user-edited settings (provider selector + per-provider
//!   creds, defaults).
//! - `state.json` — tool-managed runtime state (known projects → volume IDs,
//!   currently-alive server, last image ID).
//!
//! Both are kept tiny and human-readable on purpose. If the tool ever gets
//! confused about state, deleting `state.json` and rerunning is meant to be a
//! safe recovery (volumes/servers will be re-discovered from the provider).
//!
//! ## v0.17 config shape
//!
//! Top-level fields are provider-agnostic; provider-specifics nest under
//! `[hetzner]` / `[aws]`. The `provider` selector picks the active one.
//!
//! ```toml
//! provider = "hetzner"     # or "aws"
//! keep_alive_secs = 300
//! volume_gb = 200
//! # … other cross-provider fields …
//!
//! [hetzner]
//! token = "…"
//! regions = ["nbg1", "fsn1"]
//! server_type = "ccx63"
//! ```
//!
//! Old (≤v0.16) configs with `hetzner_token` / `region` / `server_type` at
//! the top level are rejected with a clear error pointing at the new shape
//! (no backwards-compat layer per Rule #8).

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::provider::{ImageId, ServerId, VolumeId};

/// Which cloud provider this config is for. Top-level selector picks
/// the nested `[hetzner]` or `[aws]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Hetzner,
    Aws,
}

impl Default for ProviderKind {
    fn default() -> Self { ProviderKind::Hetzner }
}

/// User-edited settings. Defaults are filled in for any missing field so
/// users only have to write the parts they actually want to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Which provider to talk to. Defaults to `hetzner` for continuity
    /// with pre-v0.17 single-provider use.
    #[serde(default)]
    pub provider: ProviderKind,

    /// Hetzner-specific settings, used when `provider = "hetzner"`.
    #[serde(default)]
    pub hetzner: Option<HetznerConfig>,

    /// AWS-specific settings, used when `provider = "aws"`. Placeholder
    /// for Phase B; deserialized so a forward-looking config doesn't
    /// fail to parse.
    #[serde(default)]
    pub aws: Option<AwsConfig>,

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

/// Hetzner-specific configuration. Lives under `[hetzner]` in
/// `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HetznerConfig {
    /// Hetzner Cloud API token (read+write). Required when
    /// `provider = "hetzner"`.
    pub token: String,
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
    #[serde(default = "default_hetzner_region", deserialize_with = "one_or_many")]
    pub region: Vec<String>,
    /// Plural alias for `region`. Same string-or-list semantics. If
    /// both `region` and `regions` are set, `regions` wins.
    #[serde(default, deserialize_with = "one_or_many")]
    pub regions: Vec<String>,
    /// Hetzner server type (e.g. `ccx63`, `ccx53`, `ccx43`).
    #[serde(default = "default_server_type")]
    pub server_type: String,
}

/// AWS-specific configuration. Lives under `[aws]` in `config.toml`.
/// Placeholder shape — Phase B will populate behaviour around it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AwsConfig {
    /// AWS region codes to try in order. Like Hetzner's `regions`,
    /// these are tried left-to-right on capacity errors.
    #[serde(default, deserialize_with = "one_or_many")]
    pub regions: Vec<String>,
    /// EC2 instance type (e.g. `c7a.48xlarge`).
    #[serde(default)]
    pub instance_type: String,
    /// Use spot instances by default. Spot pricing on c7a-class
    /// machines is ~70-80% off on-demand; the 2-minute interruption
    /// notice is long enough that almost any cargo run completes
    /// before it hits. Bake (one-shot, long-running, must succeed)
    /// uses on-demand regardless.
    #[serde(default = "default_use_spot")]
    pub use_spot: bool,
    /// EBS volume type for project caches. `None` → `gp3` (best
    /// price/perf for cargo-style write-heavy bursts). Override to
    /// `Some("io2")` etc. for very hot caches (rare).
    #[serde(default)]
    pub volume_type: Option<String>,
}

fn default_use_spot() -> bool { true }

/// All-optional patch deserialised from
/// `<workspace>/.config/cargo-burst.toml`. Fields that are `Some(_)`
/// override the corresponding global config field; `forward_env` is
/// the one exception — it's *additive* (project list appended to
/// global, deduped) so a project can add to the global allow-list
/// without having to repeat what the user already configured globally.
///
/// `[hetzner].token` / `[aws]` credentials are refused outright — tokens
/// belong in the per-user global config, not in a file that gets
/// committed.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Provider override — switch between hetzner/aws on a per-project
    /// basis. Rare, but lets a project that needs spot-instance pricing
    /// pick AWS even when the user's global default is hetzner.
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    /// Hetzner section override. Token is rejected with an error;
    /// regions/server_type can be overridden freely.
    #[serde(default)]
    pub hetzner: Option<ProjectHetznerConfig>,
    /// AWS section override.
    #[serde(default)]
    pub aws: Option<AwsConfig>,
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
    #[serde(default)]
    pub extra_excludes: Option<Vec<String>>,
    /// Built-in default-exclude patterns to NOT apply for this project.
    #[serde(default)]
    pub unexclude: Option<Vec<String>>,
}

/// Project-level Hetzner overrides. `token` is intentionally accepted
/// by the deserializer (so a typo'd token field doesn't fail parsing
/// in some confusing way) but rejected with a clear error in
/// [`Config::load_for_workspace`] — committing a token is a security
/// incident waiting to happen.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectHetznerConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default, deserialize_with = "one_or_many_opt")]
    pub region: Option<Vec<String>>,
    #[serde(default, deserialize_with = "one_or_many_opt")]
    pub regions: Option<Vec<String>>,
    #[serde(default)]
    pub server_type: Option<String>,
}

fn default_hetzner_region() -> Vec<String> { vec!["hel1".into()] }

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
/// stays `None`.
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
/// safe to delete to force re-discovery from the provider.
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
    pub image_id: Option<ImageId>,
    /// ID of the shared server, if currently alive. Cleared by the reaper
    /// when it deletes the server.
    #[serde(default)]
    pub server_id: Option<ServerId>,
    /// Per-project state, keyed by `project::ProjectKey` (sha256-prefix of
    /// the workspace root path).
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectState>,
    /// Session accounting for the currently-alive shared server.
    #[serde(default)]
    pub current_server_session: Option<ServerSession>,
}

/// In-flight accounting for a single server lifetime. Persisted to
/// `state.json` so the count survives across cargo-burst invocations
/// (the shared server outlives each individual `cargo burst <verb>`
/// run). Finalized — and zeroed — when the server is destroyed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSession {
    pub server_id: ServerId,
    /// Provider that owns this session ("hetzner" / "aws"). Recorded
    /// so cross-provider audit summaries can split costs correctly.
    #[serde(default = "default_provider_name")]
    pub provider: String,
    /// RFC3339 timestamp the server was provisioned at.
    pub started_at: String,
    /// Cumulative count of cargo phases that ran during this lifetime.
    pub command_count: u32,
}

fn default_provider_name() -> String { "hetzner".into() }

impl State {
    /// Most-recent `last_used_rfc3339` across every project, parsed.
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
    /// Provider volume ID for this project's `target/` cache, if created.
    #[serde(default)]
    pub volume_id: Option<VolumeId>,
    /// Last time *this* project ran a build (RFC3339).
    #[serde(default)]
    pub last_used_rfc3339: Option<String>,
    /// User-confirmed extra rsync excludes (on top of `ssh::DEFAULT_EXCLUDES`).
    /// `None` means we've never prompted the user about excludes for this
    /// project.
    #[serde(default)]
    pub excludes: Option<Vec<String>>,
    /// RFC3339 timestamp the current volume was provisioned at.
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

/// Path to the project-level config file inside a workspace.
pub fn project_config_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".config").join("cargo-burst.toml")
}

/// Pre-v0.17 fields that must NOT appear at the top level of the new
/// config. If we see any of these, the user is on the old shape and
/// needs a one-time migration. Per Rule #8 we don't ship a compat
/// layer; the error tells them exactly what to do.
const LEGACY_TOP_LEVEL_FIELDS: &[&str] =
    &["hetzner_token", "region", "regions", "server_type"];

impl Config {
    /// Load `config.toml`. Returns a helpful error pointing at the path if
    /// missing — first-run UX is "the tool tells you where to put the file".
    pub fn load() -> Result<Self> {
        Self::load_for_workspace(None)
    }

    /// Load global config and, if `workspace_root` is `Some(_)` and a
    /// `<workspace>/.config/cargo-burst.toml` exists, layer it on top.
    pub fn load_for_workspace(workspace_root: Option<&Path>) -> Result<Self> {
        let path = config_path()?;
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "config not found at {}. Create it with at least:\n  \
                 provider = \"hetzner\"\n  [hetzner]\n  token = \"…\"",
                path.display()
            )
        })?;
        check_for_legacy_shape(&text, &path)?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate_credentials(&path)?;

        if let Some(root) = workspace_root {
            let proj_path = project_config_path(root);
            match fs::read_to_string(&proj_path) {
                Ok(proj_text) => {
                    let project: ProjectConfig = toml::from_str(&proj_text)
                        .with_context(|| format!("parsing {}", proj_path.display()))?;
                    if project
                        .hetzner
                        .as_ref()
                        .and_then(|h| h.token.as_ref())
                        .is_some()
                    {
                        return Err(anyhow!(
                            "{} sets `[hetzner].token`, which is refused at the \
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

    /// Bare-bones credential check — provider-section must exist with
    /// non-empty token.
    fn validate_credentials(&self, path: &Path) -> Result<()> {
        match self.provider {
            ProviderKind::Hetzner => {
                let h = self.hetzner.as_ref().ok_or_else(|| {
                    anyhow!(
                        "provider = \"hetzner\" but no [hetzner] section in {}",
                        path.display()
                    )
                })?;
                if h.token.trim().is_empty() {
                    return Err(anyhow!(
                        "[hetzner].token is empty in {}",
                        path.display()
                    ));
                }
            }
            ProviderKind::Aws => {
                // AWS credentials come from the default credential chain
                // (env, ~/.aws/credentials, IMDS). We can't validate them
                // here without an actual API call, so just require the
                // section to be present.
                if self.aws.is_none() {
                    return Err(anyhow!(
                        "provider = \"aws\" but no [aws] section in {}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Apply a [`ProjectConfig`] patch in place.
    pub fn merge_project(&mut self, project: ProjectConfig) {
        if let Some(p) = project.provider {
            self.provider = p;
        }
        if let Some(h) = project.hetzner {
            let target = self.hetzner.get_or_insert_with(|| HetznerConfig {
                token: String::new(),
                region: default_hetzner_region(),
                regions: Vec::new(),
                server_type: default_server_type(),
            });
            if let Some(v) = h.region {
                target.region = v;
            }
            if let Some(v) = h.regions {
                target.regions = v;
            }
            if let Some(v) = h.server_type {
                target.server_type = v;
            }
            // token is rejected upstream
        }
        if let Some(a) = project.aws {
            let target = self.aws.get_or_insert_with(AwsConfig::default);
            if !a.regions.is_empty() {
                target.regions = a.regions;
            }
            if !a.instance_type.is_empty() {
                target.instance_type = a.instance_type;
            }
            // use_spot has a default-true; an explicit project override
            // wins.
            target.use_spot = a.use_spot;
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
            for name in extra {
                if !self.forward_env.iter().any(|n| n == &name) {
                    self.forward_env.push(name);
                }
            }
        }
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

    /// Ordered list of regions to try for server provisioning, for the
    /// active provider.
    pub fn region_preference(&self) -> Vec<String> {
        match self.provider {
            ProviderKind::Hetzner => {
                let Some(h) = self.hetzner.as_ref() else {
                    return default_hetzner_region();
                };
                if !h.regions.is_empty() {
                    h.regions.clone()
                } else if !h.region.is_empty() {
                    h.region.clone()
                } else {
                    default_hetzner_region()
                }
            }
            ProviderKind::Aws => {
                self.aws
                    .as_ref()
                    .map(|a| a.regions.clone())
                    .unwrap_or_default()
            }
        }
    }

    /// Server type / instance type for the active provider.
    pub fn server_type(&self) -> String {
        match self.provider {
            ProviderKind::Hetzner => self
                .hetzner
                .as_ref()
                .map(|h| h.server_type.clone())
                .unwrap_or_else(default_server_type),
            ProviderKind::Aws => self
                .aws
                .as_ref()
                .map(|a| a.instance_type.clone())
                .unwrap_or_default(),
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

/// Reject pre-v0.17 configs that still have top-level `hetzner_token`,
/// `region`, `regions`, or `server_type` fields. Pinpoint scan rather
/// than a deserialize attempt — we want to tell the user *exactly*
/// what to move.
fn check_for_legacy_shape(text: &str, path: &Path) -> Result<()> {
    // A "top-level" field is any line that starts (ignoring leading
    // whitespace) with the field name followed by `=`, before any
    // `[section]` header appears. We do a one-pass scan.
    let mut found: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if let Some(eq) = trimmed.find('=') {
            let name = trimmed[..eq].trim();
            for f in LEGACY_TOP_LEVEL_FIELDS {
                if name == *f {
                    found.push(*f);
                }
            }
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "{}: pre-v0.17 fields at top level ({}). cargo-burst v0.17 moved \
         Hetzner-specific settings into a `[hetzner]` section. Update to:\n\n  \
         provider = \"hetzner\"\n  [hetzner]\n  token = \"…\"\n  regions = [\"…\"]\n  \
         server_type = \"ccx63\"\n",
        path.display(),
        found.join(", "),
    ))
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

    /// Atomic-ish save: write to `state.json.tmp` then rename.
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
/// `state.json`.
pub struct StateLock {
    _file: fs::File,
}

/// Hard cap on how long we'll wait for another process to drop the
/// state lock. State writes are short — touch state.json, fsync, drop
/// the lock — so the only way the lock is held longer than this is a
/// crashed/wedged peer or a misbehaving long-running task holding it
/// across an await (the reaper used to do this). Either way, the
/// answer is "stop waiting and move on" rather than hang the user's
/// `cargo burst` invocation forever.
const LOCK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl StateLock {
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

        let deadline = std::time::Instant::now() + LOCK_WAIT_TIMEOUT;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(anyhow!(
                            "state lock {} held by another cargo-burst process for >{}s; \
                             refusing to wait longer. State writes are supposed to be \
                             short. If this keeps happening, a peer process is wedged — \
                             try `pgrep -af cargo-burst` and kill any stale `__reap-*` \
                             daemons.",
                            path.display(),
                            LOCK_WAIT_TIMEOUT.as_secs()
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    return Err(anyhow!("locking {}: {e}", path.display()));
                }
            }
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {}
}

/// Read-modify-write `state.json` under the global lock.
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

    fn parse_with_validate(toml: &str) -> Result<Config> {
        check_for_legacy_shape(toml, Path::new("test.toml"))?;
        let cfg: Config =
            toml::from_str(toml).with_context(|| "parsing test toml")?;
        cfg.validate_credentials(Path::new("test.toml"))?;
        Ok(cfg)
    }

    /// Note: this prefix ends BEFORE the `[hetzner]` section so test
    /// callers can prepend top-level fields. To append fields inside
    /// `[hetzner]`, use [`MINIMAL_HETZNER_WITH_SECTION`].
    const MINIMAL_HETZNER_PRE_SECTION: &str = "provider = \"hetzner\"\n";
    const MINIMAL_HETZNER: &str = r#"
provider = "hetzner"
[hetzner]
token = "x"
"#;

    #[test]
    fn region_accepts_single_string() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}region = \"hel1\"\n"
        ));
        let h = cfg.hetzner.as_ref().unwrap();
        assert_eq!(h.region, vec!["hel1".to_string()]);
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }

    #[test]
    fn region_accepts_array() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}region = [\"hel1\", \"fsn1\", \"nbg1\"]\n"
        ));
        let h = cfg.hetzner.as_ref().unwrap();
        assert_eq!(h.region, vec!["hel1", "fsn1", "nbg1"]);
        assert_eq!(cfg.region_preference(), vec!["hel1", "fsn1", "nbg1"]);
    }

    #[test]
    fn regions_alias_accepts_single_string() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}regions = \"fsn1\"\n"
        ));
        let h = cfg.hetzner.as_ref().unwrap();
        assert_eq!(h.regions, vec!["fsn1".to_string()]);
        assert_eq!(cfg.region_preference(), vec!["fsn1".to_string()]);
    }

    #[test]
    fn regions_alias_accepts_array() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}regions = [\"hel1\", \"fsn1\"]\n"
        ));
        assert_eq!(cfg.region_preference(), vec!["hel1", "fsn1"]);
    }

    #[test]
    fn plural_regions_wins_when_both_set() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}region = \"hel1\"\nregions = [\"fsn1\", \"nbg1\"]\n"
        ));
        assert_eq!(cfg.region_preference(), vec!["fsn1", "nbg1"]);
    }

    #[test]
    fn region_defaults_when_neither_field_set() {
        let cfg = parse(MINIMAL_HETZNER);
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }

    fn parse_project(toml: &str) -> ProjectConfig {
        toml::from_str(toml).expect("parse project config")
    }

    #[test]
    fn project_replaces_simple_fields() {
        let mut cfg = parse(MINIMAL_HETZNER);
        let proj = parse_project(r#"
volume_gb = 50
keep_alive_secs = 60
[hetzner]
server_type = "ccx53"
"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.server_type(), "ccx53");
        assert_eq!(cfg.volume_gb, 50);
        assert_eq!(cfg.keep_alive_secs, 60);
        assert_eq!(cfg.volume_keep_alive_secs, default_volume_keep_alive());
    }

    #[test]
    fn project_replaces_regions_not_merges() {
        let mut cfg = parse(&format!(
            "{MINIMAL_HETZNER}region = [\"hel1\", \"fsn1\"]\n"
        ));
        let proj = parse_project(r#"[hetzner]
region = "nbg1""#);
        cfg.merge_project(proj);
        let h = cfg.hetzner.as_ref().unwrap();
        assert_eq!(h.region, vec!["nbg1".to_string()]);
    }

    #[test]
    fn project_forward_env_is_additive_and_deduped() {
        // Top-level fields go BEFORE the [hetzner] section in TOML.
        let mut cfg = parse(&format!(
            "{MINIMAL_HETZNER_PRE_SECTION}forward_env = [\"RUST_LOG\", \"RUST_BACKTRACE\"]\n[hetzner]\ntoken = \"x\"\n"
        ));
        let proj = parse_project(r#"forward_env = ["DATABASE_URL", "RUST_LOG"]"#);
        cfg.merge_project(proj);
        assert_eq!(
            cfg.forward_env,
            vec!["RUST_LOG", "RUST_BACKTRACE", "DATABASE_URL"]
        );
    }

    #[test]
    fn project_extra_excludes_are_additive_and_deduped() {
        let mut cfg = parse(&format!(
            "{MINIMAL_HETZNER_PRE_SECTION}extra_excludes = [\"fixtures/large/\"]\n[hetzner]\ntoken = \"x\"\n"
        ));
        let proj = parse_project(r#"extra_excludes = ["fixtures/large/", "*.bak"]"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.extra_excludes, vec!["fixtures/large/", "*.bak"]);
    }

    #[test]
    fn project_unexclude_is_additive_and_deduped() {
        let mut cfg = parse(&format!(
            "{MINIMAL_HETZNER_PRE_SECTION}unexclude = [\".git/\"]\n[hetzner]\ntoken = \"x\"\n"
        ));
        let proj = parse_project(r#"unexclude = [".git/", ".vscode/"]"#);
        cfg.merge_project(proj);
        assert_eq!(cfg.unexclude, vec![".git/", ".vscode/"]);
    }

    #[test]
    fn project_unexclude_only_set_at_project_level_works() {
        let mut cfg = parse(MINIMAL_HETZNER);
        cfg.merge_project(parse_project(r#"unexclude = [".git/"]"#));
        assert_eq!(cfg.unexclude, vec![".git/"]);
    }

    #[test]
    fn project_unset_fields_dont_clobber_global() {
        let mut cfg = parse(r#"
provider = "hetzner"
volume_gb = 200
[hetzner]
token = "x"
server_type = "ccx63"
"#);
        cfg.merge_project(parse_project(""));
        assert_eq!(cfg.server_type(), "ccx63");
        assert_eq!(cfg.volume_gb, 200);
    }

    #[test]
    fn project_token_is_caught_at_deserialize() {
        let proj = parse_project(r#"[hetzner]
token = "leaked""#);
        assert_eq!(
            proj.hetzner.as_ref().unwrap().token.as_deref(),
            Some("leaked")
        );
    }

    #[test]
    fn project_unknown_field_fails_to_parse() {
        let result: std::result::Result<ProjectConfig, _> =
            toml::from_str(r#"servr_type = "ccx53""#);
        assert!(result.is_err(), "expected typo to fail");
    }

    #[test]
    fn explicit_empty_array_falls_back_to_default() {
        let cfg = parse(&format!(
            "{MINIMAL_HETZNER}region = []\n"
        ));
        assert_eq!(cfg.region_preference(), vec!["hel1".to_string()]);
    }

    #[test]
    fn legacy_top_level_fields_are_rejected() {
        let bad = r#"
hetzner_token = "x"
region = "hel1"
"#;
        let err = parse_with_validate(bad).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("pre-v0.17"),
            "expected migration hint, got: {msg}"
        );
        assert!(msg.contains("hetzner_token"));
    }

    #[test]
    fn missing_provider_section_rejected() {
        let bad = r#"provider = "hetzner""#;
        let err = parse_with_validate(bad).unwrap_err();
        assert!(format!("{err}").contains("[hetzner]"));
    }

    #[test]
    fn aws_provider_requires_section() {
        let bad = r#"provider = "aws""#;
        let err = parse_with_validate(bad).unwrap_err();
        assert!(format!("{err}").contains("[aws]"));
    }

    #[test]
    fn aws_minimal_parses() {
        let ok = r#"
provider = "aws"
[aws]
regions = ["us-east-1"]
instance_type = "c7a.48xlarge"
"#;
        let cfg = parse_with_validate(ok).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Aws);
        assert_eq!(cfg.region_preference(), vec!["us-east-1".to_string()]);
        assert_eq!(cfg.server_type(), "c7a.48xlarge");
        assert!(cfg.aws.unwrap().use_spot);
    }
}
