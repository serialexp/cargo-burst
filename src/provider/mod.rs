//! Cloud-provider abstraction.
//!
//! cargo-burst was originally Hetzner-only. Hetzner's 1-hour minimum
//! billing increment turns out to be expensive enough that we want to
//! be able to target other providers (AWS EC2 with per-second billing,
//! primarily). This module defines the `Provider` trait that the rest
//! of the codebase talks to; concrete impls live in submodules
//! (`hcloud` here, `ec2` in Phase B).
//!
//! Design rules:
//!
//! - **IDs are opaque strings.** Hetzner uses `i64`, AWS uses
//!   `i-0abcd…`, others may use UUIDs. Stuffing each into its own type
//!   makes consumers cope with cross-provider differences they don't
//!   care about. We give each resource its own newtype (`ServerId`,
//!   `VolumeId`, `ImageId`, `SshKeyId`) for type safety, but the
//!   contents are always `String`.
//!
//! - **Specs in, Info out.** Creation methods take a Spec describing
//!   what's wanted; queries return Info describing what is.
//!   Provider-specific quirks (Hetzner's "labels", AWS' "tags") live
//!   inside the Spec as a generic `BTreeMap`.
//!
//! - **Errors are typed only where the caller acts on them.**
//!   `ProvisionError::NoCapacity` is surfaced as a distinct variant
//!   because the caller falls back to a different region. Everything
//!   else is `anyhow::Error` and bubbled up unchanged.
//!
//! - **Billing minimum is a provider property.** The reaper uses it to
//!   keep a paid-for server alive until its minimum lifetime expires,
//!   so a fresh `cargo burst check` 5 minutes after the previous run
//!   doesn't pay for two billing increments.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

pub mod aws;
pub mod hcloud;

use std::sync::Arc;

/// Build the active provider from a [`config::Config`].
///
/// Fails if the chosen provider's credential section isn't usable.
/// Async because `Ec2Provider::new` consults the AWS default
/// credential chain (which may hit IMDS).
pub async fn from_config(cfg: &crate::config::Config) -> anyhow::Result<Arc<dyn Provider>> {
    use crate::config::ProviderKind;
    match cfg.provider {
        ProviderKind::Hetzner => {
            let h = cfg.hetzner.as_ref().ok_or_else(|| {
                anyhow::anyhow!("provider = \"hetzner\" but no [hetzner] section in config")
            })?;
            Ok(Arc::new(hcloud::HcloudProvider::new(h.token.clone())?))
        }
        ProviderKind::Aws => {
            let a = cfg.aws.as_ref().ok_or_else(|| {
                anyhow::anyhow!("provider = \"aws\" but no [aws] section in config")
            })?;
            if a.regions.is_empty() {
                return Err(anyhow::anyhow!(
                    "[aws].regions is empty — set at least one region (e.g. regions = [\"us-east-1\"])"
                ));
            }
            if a.instance_type.is_empty() {
                return Err(anyhow::anyhow!(
                    "[aws].instance_type is empty — set e.g. instance_type = \"c7a.48xlarge\""
                ));
            }
            Ok(Arc::new(aws::Ec2Provider::new(a.clone()).await?))
        }
    }
}

// ── ID newtypes ────────────────────────────────────────────────────────

/// Generate an opaque newtype wrapper around `String` with `Display`,
/// `Serialize`/`Deserialize` (transparent), and `From<String>`/`From<&str>`.
/// Used for each resource ID kind so the type system rejects "passed a
/// volume ID where a server ID was expected".
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[allow(dead_code)]
            pub fn as_str(&self) -> &str { &self.0 }
            #[allow(dead_code)]
            pub fn into_string(self) -> String { self.0 }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl From<String> for $name { fn from(s: String) -> Self { Self(s) } }
        impl From<&str> for $name { fn from(s: &str) -> Self { Self(s.to_string()) } }
        impl AsRef<str> for $name { fn as_ref(&self) -> &str { &self.0 } }
    };
}

id_type! {
    /// Opaque server identifier — Hetzner's i64 stringified, AWS's
    /// `i-…`, etc.
    ServerId
}
id_type! {
    /// Opaque volume / EBS-volume identifier.
    VolumeId
}
id_type! {
    /// Opaque image / AMI / snapshot identifier.
    ImageId
}
id_type! {
    /// Opaque SSH-key identifier (Hetzner ssh_key id, AWS key-pair name).
    SshKeyId
}

// ── Specs / Info ──────────────────────────────────────────────────────

/// What a caller wants when provisioning a new server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    /// Provider-specific server-class string (e.g. `ccx63`,
    /// `c7a.48xlarge`). Validation is the provider's problem.
    pub server_type: String,
    pub image: ImageId,
    pub region: String,
    pub ssh_keys: Vec<SshKeyId>,
    /// Cloud-init user-data script. Optional.
    pub user_data: Option<String>,
    /// Labels / tags. Providers translate; common ones cargo-burst
    /// sets: `managed-by=cargo-burst`, `role=shared|bake`.
    pub labels: BTreeMap<String, String>,
    /// Volumes to attach at boot, where the provider supports it
    /// (Hetzner optimisation). AWS ignores — attach happens after
    /// the instance is reachable.
    pub attached_volumes: Vec<VolumeId>,
}

/// What a caller wants when creating a new volume / EBS volume.
#[derive(Debug, Clone)]
pub struct VolumeSpec {
    pub name: String,
    pub size_gb: u32,
    /// Hetzner location code / AWS AZ.
    pub region: String,
    pub labels: BTreeMap<String, String>,
}

/// Status snapshot of an existing server.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: ServerId,
    pub status: ServerStatus,
    pub public_ip: Option<String>,
    pub server_type: String,
    pub region: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Initializing,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// Anything the provider's API returned that we don't model.
    Other(String),
}

impl ServerStatus {
    /// True for any of the "the box is alive enough to keep using"
    /// states — what `ensure_shared_server` treats as "reuse this".
    pub fn is_alive(&self) -> bool {
        matches!(
            self,
            ServerStatus::Initializing | ServerStatus::Starting | ServerStatus::Running
        )
    }

    pub fn as_str(&self) -> &str {
        match self {
            ServerStatus::Initializing => "initializing",
            ServerStatus::Starting => "starting",
            ServerStatus::Running => "running",
            ServerStatus::Stopping => "stopping",
            ServerStatus::Stopped => "stopped",
            ServerStatus::Other(s) => s.as_str(),
        }
    }
}

/// Status snapshot of an existing volume.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub id: VolumeId,
    pub size_gb: u32,
    pub status: VolumeStatus,
    pub region: Option<String>,
    pub attached_to: Option<ServerId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeStatus {
    Creating,
    Available,
    Other(String),
}

impl VolumeStatus {
    pub fn as_str(&self) -> &str {
        match self {
            VolumeStatus::Creating => "creating",
            VolumeStatus::Available => "available",
            VolumeStatus::Other(s) => s.as_str(),
        }
    }
}

/// Status snapshot of an existing image / snapshot / AMI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ImageInfo {
    pub id: ImageId,
    pub status: ImageStatus,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageStatus {
    Creating,
    Available,
    Other(String),
}

impl ImageStatus {
    pub fn as_str(&self) -> &str {
        match self {
            ImageStatus::Creating => "creating",
            ImageStatus::Available => "available",
            ImageStatus::Other(s) => s.as_str(),
        }
    }
}

/// Information returned by [`Provider::attach_volume`]. The OS-visible
/// device path is provider-specific (Hetzner exposes
/// `/dev/disk/by-id/scsi-0HC_Volume_<id>`, AWS Nitro instances need a
/// NVMe-serial → EBS-id mapping). The trait just returns whatever the
/// impl computed; the consumer uses it verbatim in the mount script.
#[derive(Debug, Clone)]
pub struct AttachedDevice {
    /// Path under `/dev/` (or `/dev/disk/by-id/...`) where the
    /// attached volume appears on the remote host.
    pub device_path: String,
}

/// Errors that callers want to discriminate by type. Anything not
/// listed here is wrapped in `Other(anyhow::Error)` and bubbled up.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// Provider refused the create because the requested
    /// server_type/region combination is currently at capacity.
    /// Caller responds by trying the next region in their fallback
    /// list. On Hetzner: HTTP 412 with `code: resource_unavailable`.
    /// On AWS: spot `InsufficientInstanceCapacity`.
    #[error(
        "no capacity in any of the configured regions for server type {server_type:?}; tried {regions:?}"
    )]
    NoCapacity {
        server_type: String,
        regions: Vec<String>,
    },
    /// Anything else — auth, quota, network, validation. Caller
    /// surfaces these directly.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// ── The trait ──────────────────────────────────────────────────────────

/// Cloud provider that can spin up servers, manage persistent volumes
/// for warm `target/` caches, and bake reusable images.
///
/// `Send + Sync` so callers can stash an `Arc<dyn Provider>` and clone
/// it across tasks. `async-trait` desugars the async methods to
/// `Pin<Box<dyn Future + Send>>`, which is what we need for dyn
/// dispatch.
#[async_trait]
#[allow(dead_code)]
pub trait Provider: Send + Sync {
    /// Short identifier for logging / audit ("hetzner", "aws").
    fn name(&self) -> &'static str;

    /// Minimum billable lifetime per server. Hetzner: 1h. AWS: 0s.
    /// The reaper uses this to delay deletion until the paid window
    /// is (nearly) up, so two `cargo burst` invocations 5 minutes
    /// apart don't accidentally pay for two billing increments.
    fn billing_minimum(&self) -> Duration;

    // ── SSH keys ────────────────────────────────────────────────────

    /// Idempotent: ensure a key with the given name and public-key
    /// material is registered with the provider. Recreate if the
    /// stored key has different material.
    async fn ensure_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKeyId>;

    /// Best-effort delete. Used by tests / debug tooling, not the
    /// production lifecycle (key sticks around across runs).
    async fn delete_ssh_key(&self, id: &SshKeyId) -> Result<()>;

    // ── Images ──────────────────────────────────────────────────────

    /// Snapshot a (powered-off) source server into a reusable image.
    /// Returns the new image id; callers should `wait_image_ready`
    /// before using it.
    async fn bake_image_from_server(
        &self,
        src: &ServerId,
        description: &str,
    ) -> Result<ImageId>;

    /// Poll until the image transitions out of `creating` to
    /// `available`, or fail if it goes anywhere else.
    async fn wait_image_ready(&self, id: &ImageId, timeout: Duration) -> Result<()>;

    /// Look up an image's current state. Returns `Ok(None)` if the
    /// image was deleted upstream (so callers can treat the
    /// state-file id as stale).
    async fn get_image(&self, id: &ImageId) -> Result<Option<ImageInfo>>;

    // ── Servers ─────────────────────────────────────────────────────

    /// Create a new server. Surfaces `NoCapacity` explicitly so the
    /// caller can retry in another region.
    async fn create_server(
        &self,
        spec: ServerSpec,
    ) -> std::result::Result<ServerInfo, ProvisionError>;

    /// Look up a server's current state by id. `Ok(None)` if it
    /// was deleted upstream.
    async fn get_server(&self, id: &ServerId) -> Result<Option<ServerInfo>>;

    /// Look up a server by its `name` label/tag (cargo-burst's
    /// `cargo-burst-shared` is the standard one). Returns the first
    /// match or `Ok(None)`.
    async fn find_server(&self, name: &str) -> Result<Option<ServerInfo>>;

    /// Delete a server. Idempotent: if it's already gone, no error.
    async fn delete_server(&self, id: &ServerId) -> Result<()>;

    /// Block until the server transitions to `running` (or fail).
    async fn wait_server_running(
        &self,
        id: &ServerId,
        timeout: Duration,
    ) -> Result<ServerInfo>;

    // ── Volumes ─────────────────────────────────────────────────────

    /// Create a new (detached) volume in the given region.
    async fn create_volume(&self, spec: VolumeSpec) -> Result<VolumeInfo>;

    /// Look up a volume's current state by id. `Ok(None)` if it
    /// was deleted upstream.
    async fn get_volume(&self, id: &VolumeId) -> Result<Option<VolumeInfo>>;

    /// Look up a volume by name. Returns the first match or
    /// `Ok(None)`.
    async fn find_volume(&self, name: &str) -> Result<Option<VolumeInfo>>;

    /// Attach a volume to a server. Returns the OS-visible device
    /// path the consumer's mount script should use.
    async fn attach_volume(
        &self,
        vol: &VolumeId,
        srv: &ServerId,
    ) -> Result<AttachedDevice>;

    /// Detach a volume (best-effort — returns Ok if already
    /// detached).
    async fn detach_volume(&self, vol: &VolumeId) -> Result<()>;

    /// Delete a volume. The caller is responsible for detaching first
    /// (Hetzner refuses to delete an attached volume).
    async fn delete_volume(&self, id: &VolumeId) -> Result<()>;
}
