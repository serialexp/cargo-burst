//! JSON shapes for the Hetzner Cloud API endpoints we use.
//!
//! Only the fields we actually read are modeled — Hetzner adds fields all
//! the time and we don't want to fail decoding when they do. `serde`'s
//! default of "ignore unknown fields" is what we want here, so we don't
//! mark anything `deny_unknown_fields`.

use serde::{Deserialize, Serialize};

// ── SSH keys ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
    pub public_key: String,
}

#[derive(Deserialize)]
pub(super) struct SshKeysResponse {
    pub ssh_keys: Vec<SshKey>,
}

#[derive(Deserialize)]
pub(super) struct CreateSshKeyResponse {
    pub ssh_key: SshKey,
}

#[derive(Serialize)]
pub(super) struct CreateSshKeyRequest {
    pub name: String,
    pub public_key: String,
}

// ── Servers ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub public_net: PublicNet,
    pub server_type: ServerType,
    pub created: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicNet {
    pub ipv4: Option<Ipv4>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ipv4 {
    pub ip: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerType {
    pub name: String,
    pub cores: u32,
    pub memory: f64,
    pub disk: u32,
}

#[derive(Serialize, Debug)]
pub struct CreateServerRequest {
    pub name: String,
    pub server_type: String,
    pub image: ImageRef,
    pub location: String,
    pub ssh_keys: Vec<i64>,
    /// Hetzner expects volume IDs as integers in this list; volumes already
    /// belonging to the same project can be attached at boot.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<i64>,
    /// cloud-init / user-data script. Optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
    /// Tag the resource so it's easy to find / clean up manually.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub labels: std::collections::HashMap<String, String>,
    /// Default true — server boots immediately. We always want this.
    pub start_after_create: bool,
}

/// Hetzner accepts an image as either a numeric ID (snapshot) or a string
/// name (`ubuntu-24.04`, etc.).
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum ImageRef {
    Id(i64),
    Name(String),
}

#[derive(Deserialize, Debug)]
pub struct CreateServerResponse {
    pub server: Server,
    pub action: Action,
    /// Hetzner returns a one-time root password unless we use SSH keys.
    /// We always pass SSH keys so this should always be `None`.
    #[serde(default)]
    pub root_password: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ServerResponse { pub server: Server }
#[derive(Deserialize)]
pub(super) struct ServersResponse { pub servers: Vec<Server> }

// ── Volumes ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    pub id: i64,
    pub name: String,
    pub size: u32,
    pub status: String,
    /// Server ID this volume is currently attached to, if any.
    #[serde(default)]
    pub server: Option<i64>,
    /// Linux device path the volume appears at when attached
    /// (e.g. `/dev/disk/by-id/scsi-0HC_Volume_<id>`).
    pub linux_device: String,
    pub location: Location,
    pub created: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub name: String,
}

#[derive(Serialize, Debug)]
pub struct CreateVolumeRequest {
    pub name: String,
    /// Size in GB.
    pub size: u32,
    /// Either `location` (creates unattached) or `server` (creates attached
    /// to that server). We always create unattached and attach explicitly,
    /// because the volume outlives the server.
    pub location: String,
    pub format: String, // "ext4" or "xfs" — we always pass "ext4"
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub labels: std::collections::HashMap<String, String>,
    /// Avoids a separate ext4-mkfs step on first attach.
    pub automount: bool,
}

#[derive(Deserialize, Debug)]
pub struct CreateVolumeResponse {
    pub volume: Volume,
    /// Hetzner returns one or more actions when creating with format/automount.
    #[serde(default)]
    pub action: Option<Action>,
    #[serde(default)]
    pub next_actions: Vec<Action>,
}

#[derive(Deserialize)]
pub(super) struct VolumeResponse { pub volume: Volume }
#[derive(Deserialize)]
pub(super) struct VolumesResponse { pub volumes: Vec<Volume> }

// ── Images ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub id: i64,
    pub status: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImageResponse { pub image: Image }

#[derive(Deserialize, Debug)]
pub(super) struct CreateImageActionResponse {
    pub action: Action,
    pub image: Image,
}

// ── Actions ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub id: i64,
    pub status: String,
    #[serde(default)]
    pub error: Option<ActionError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionError {
    pub code: String,
    pub message: String,
}

#[derive(Deserialize)]
pub(super) struct ActionResponse { pub action: Action }
