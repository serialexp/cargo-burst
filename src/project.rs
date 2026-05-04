//! Identify the cargo workspace we're operating in and produce a stable
//! key for it. Volumes and per-project state are looked up by this key.

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A stable identifier for a cargo workspace. We hash the absolute,
/// canonicalized workspace root path — moving the project to a new path is
/// treated as a new project (and produces a fresh volume), which is the safe
/// default. If the user later wants to migrate, they can edit `state.json`
/// by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectKey {
    /// Canonical absolute path to the workspace root (the directory
    /// containing the top-level `Cargo.toml`).
    pub workspace_root: PathBuf,
    /// 16-hex-char prefix of sha256(workspace_root) — short enough to embed
    /// in Hetzner resource names (which have a 100-char limit) but unique
    /// enough that collisions across personal projects are negligible.
    pub hash: String,
}

impl ProjectKey {
    /// Discover the workspace root by walking up from `start` until we find
    /// a directory containing a `Cargo.toml` with a `[workspace]` section,
    /// or until we hit the filesystem root (in which case the nearest
    /// `Cargo.toml` is treated as the workspace root).
    pub fn discover(start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", start.display()))?;
        let mut nearest_cargo: Option<PathBuf> = None;
        let mut cursor: Option<&Path> = Some(&start);
        while let Some(dir) = cursor {
            let manifest = dir.join("Cargo.toml");
            if manifest.is_file() {
                if nearest_cargo.is_none() {
                    nearest_cargo = Some(dir.to_path_buf());
                }
                // Cheap detection: parse just enough TOML to know if this is
                // a workspace root. We don't validate the rest — if the file
                // is malformed, cargo itself will complain later.
                let text = std::fs::read_to_string(&manifest).unwrap_or_default();
                if text.contains("[workspace]") {
                    return Ok(Self::from_root(dir.to_path_buf()));
                }
            }
            cursor = dir.parent();
        }
        let root = nearest_cargo.ok_or_else(|| {
            anyhow!(
                "no Cargo.toml found in {} or any parent directory",
                start.display()
            )
        })?;
        Ok(Self::from_root(root))
    }

    fn from_root(workspace_root: PathBuf) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(workspace_root.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        let hash = digest
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        Self { workspace_root, hash }
    }

    /// A short, human-friendly project name from the leaf directory. Used in
    /// resource names so they're scannable in the Hetzner console.
    pub fn short_name(&self) -> String {
        self.workspace_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".into())
            // Hetzner resource names allow [a-z0-9-_], lowercase.
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect::<String>()
    }

    /// Resource name for this project's volume. Stable across runs.
    pub fn volume_name(&self) -> String {
        format!("cargo-burst-{}-{}", self.short_name(), self.hash)
    }
}

/// Name of the (single) shared server cargo-burst provisions. Stable across
/// runs so we can look the server up from Hetzner if local state is lost.
/// One server, all projects — see `State` docs for why.
pub const SHARED_SERVER_NAME: &str = "cargo-burst-srv";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_for_same_path() {
        let p = PathBuf::from("/tmp/foo/bar");
        let a = ProjectKey::from_root(p.clone());
        let b = ProjectKey::from_root(p);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn hash_differs_for_different_paths() {
        let a = ProjectKey::from_root(PathBuf::from("/tmp/foo/bar"));
        let b = ProjectKey::from_root(PathBuf::from("/tmp/foo/baz"));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn short_name_sanitizes() {
        let p = ProjectKey::from_root(PathBuf::from("/tmp/My Cool Project!"));
        let n = p.short_name();
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
