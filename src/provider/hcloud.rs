//! Hetzner Cloud impl of the [`Provider`] trait.
//!
//! Thin adapter over `crate::hcloud::HCloud` (the REST client). Every
//! method here is a small translation:
//!
//! - `String` ↔ `i64` ID conversion at the boundary.
//! - Hetzner's nested response shapes flattened into `ServerInfo` /
//!   `VolumeInfo` / `ImageInfo`.
//! - Hetzner's status strings collapsed onto `ServerStatus` /
//!   `VolumeStatus` / `ImageStatus`.
//! - HTTP 412 + `resource_unavailable` → `ProvisionError::NoCapacity`.
//!
//! All "real" HTTP logic stays in `crate::hcloud` — this file is just
//! the shape adapter so consumers can talk to `&dyn Provider` instead
//! of the Hetzner-specific client.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

use crate::hcloud::{
    CreateServerRequest, CreateVolumeRequest, HCloud, ImageRef, Server, Volume,
};

use super::{
    AttachedDevice, ImageId, ImageInfo, ImageStatus, Provider, ProvisionError, ServerId,
    ServerInfo, ServerSpec, ServerStatus, SshKeyId, VolumeId, VolumeInfo, VolumeSpec,
    VolumeStatus,
};

/// 1 hour — Hetzner bills servers in 1-hour increments. A server alive
/// for 5 minutes costs the same as one alive for 59 minutes, so the
/// reaper waits until ~58m before retiring.
const HETZNER_BILLING_MINIMUM: Duration = Duration::from_secs(3600);

/// Hetzner-flavored [`Provider`]. Wraps the existing REST client.
pub struct HcloudProvider {
    client: HCloud,
}

impl HcloudProvider {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: HCloud::new(token)?,
        })
    }
}

// ── ID conversion helpers ─────────────────────────────────────────────

fn parse_i64<S: AsRef<str>>(s: S, what: &str) -> Result<i64> {
    s.as_ref()
        .parse::<i64>()
        .with_context(|| format!("provider id {s:?} is not a valid {what} id (expected integer)", s = s.as_ref()))
}

fn server_id_to_i64(id: &ServerId) -> Result<i64> { parse_i64(id, "server") }
fn volume_id_to_i64(id: &VolumeId) -> Result<i64> { parse_i64(id, "volume") }
fn image_id_to_i64(id: &ImageId) -> Result<i64> { parse_i64(id, "image") }
fn ssh_key_id_to_i64(id: &SshKeyId) -> Result<i64> { parse_i64(id, "ssh_key") }

// ── Status string mapping ─────────────────────────────────────────────

fn map_server_status(s: &str) -> ServerStatus {
    match s {
        "initializing" => ServerStatus::Initializing,
        "starting" => ServerStatus::Starting,
        "running" => ServerStatus::Running,
        "stopping" => ServerStatus::Stopping,
        "off" | "stopped" => ServerStatus::Stopped,
        other => ServerStatus::Other(other.to_string()),
    }
}

fn map_volume_status(s: &str) -> VolumeStatus {
    match s {
        "creating" => VolumeStatus::Creating,
        "available" => VolumeStatus::Available,
        other => VolumeStatus::Other(other.to_string()),
    }
}

fn map_image_status(s: &str) -> ImageStatus {
    match s {
        "creating" => ImageStatus::Creating,
        "available" => ImageStatus::Available,
        other => ImageStatus::Other(other.to_string()),
    }
}

fn server_to_info(s: Server) -> ServerInfo {
    ServerInfo {
        id: ServerId(s.id.to_string()),
        status: map_server_status(&s.status),
        public_ip: s.public_net.ipv4.as_ref().map(|i| i.ip.clone()),
        server_type: s.server_type.name.clone(),
        region: s.datacenter.as_ref().map(|d| d.location.name.clone()),
    }
}

fn volume_to_info(v: Volume) -> VolumeInfo {
    VolumeInfo {
        id: VolumeId(v.id.to_string()),
        size_gb: v.size,
        status: map_volume_status(&v.status),
        region: v.location.clone(),
        attached_to: v.server.map(|sid| ServerId(sid.to_string())),
    }
}

// ── Capacity-error detection ──────────────────────────────────────────

/// True if the error is Hetzner's "this server-type isn't currently
/// available in this location" capacity signal (HTTP 412 + JSON body
/// containing `code: "resource_unavailable"`).
pub(crate) fn is_capacity_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("resource_unavailable")
}

// ── Provider impl ─────────────────────────────────────────────────────

#[async_trait]
impl Provider for HcloudProvider {
    fn name(&self) -> &'static str { "hetzner" }

    fn billing_minimum(&self) -> Duration { HETZNER_BILLING_MINIMUM }

    // ── SSH keys ────────────────────────────────────────────────────

    async fn ensure_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKeyId> {
        let key = self.client.ensure_ssh_key(name, public_key).await?;
        Ok(SshKeyId(key.id.to_string()))
    }

    async fn delete_ssh_key(&self, id: &SshKeyId) -> Result<()> {
        let raw = ssh_key_id_to_i64(id)?;
        self.client.delete_ssh_key(raw).await
    }

    // ── Images ──────────────────────────────────────────────────────

    async fn bake_image_from_server(
        &self,
        src: &ServerId,
        description: &str,
    ) -> Result<ImageId> {
        let raw = server_id_to_i64(src)?;
        let res = self.client.create_image_from_server(raw, description).await?;
        // Wait inline for the snapshot action; the image won't be
        // usable until that completes anyway. wait_image_ready is
        // called separately by the caller for the image-available
        // gating.
        self.client
            .wait_action(res.action_id, Duration::from_secs(30 * 60))
            .await
            .context("waiting for snapshot action")?;
        Ok(ImageId(res.image_id.to_string()))
    }

    async fn wait_image_ready(&self, id: &ImageId, timeout: Duration) -> Result<()> {
        let raw = image_id_to_i64(id)?;
        self.client.wait_image_ready(raw, timeout).await.map(|_| ())
    }

    async fn get_image(&self, id: &ImageId) -> Result<Option<ImageInfo>> {
        let raw = image_id_to_i64(id)?;
        match self.client.get_image(raw).await {
            Ok(img) => Ok(Some(ImageInfo {
                id: ImageId(img.id.to_string()),
                status: map_image_status(&img.status),
                description: img.description.clone(),
            })),
            Err(e) => {
                if is_not_found(&e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    // ── Servers ─────────────────────────────────────────────────────

    async fn create_server(
        &self,
        spec: ServerSpec,
    ) -> std::result::Result<ServerInfo, ProvisionError> {
        // Hetzner accepts an image as either a numeric snapshot id or a
        // well-known name like "ubuntu-24.04" (used by the bake path
        // to start from a fresh base OS). We pick which `ImageRef`
        // variant to send based on whether the id parses as i64.
        let image_ref: ImageRef = match spec.image.as_str().parse::<i64>() {
            Ok(id) => ImageRef::Id(id),
            Err(_) => ImageRef::Name(spec.image.as_str().to_string()),
        };
        let ssh_keys: Vec<i64> = spec
            .ssh_keys
            .iter()
            .map(|k| ssh_key_id_to_i64(k))
            .collect::<Result<_>>()
            .map_err(ProvisionError::Other)?;
        let attached_volumes: Vec<i64> = spec
            .attached_volumes
            .iter()
            .map(|v| volume_id_to_i64(v))
            .collect::<Result<_>>()
            .map_err(ProvisionError::Other)?;

        let mut labels = HashMap::with_capacity(spec.labels.len());
        for (k, v) in &spec.labels {
            labels.insert(k.clone(), v.clone());
        }

        let req = CreateServerRequest {
            name: spec.name.clone(),
            server_type: spec.server_type.clone(),
            image: image_ref,
            location: spec.region.clone(),
            ssh_keys,
            volumes: attached_volumes,
            user_data: spec.user_data.clone(),
            labels,
            start_after_create: true,
        };

        match self.client.create_server(req).await {
            Ok(create) => {
                // Wait for the create action to converge before
                // returning the (now-final) server.
                if let Err(e) = self
                    .client
                    .wait_action(create.action.id, Duration::from_secs(180))
                    .await
                {
                    return Err(ProvisionError::Other(e));
                }
                let server = self
                    .client
                    .get_server(create.server.id)
                    .await
                    .map_err(ProvisionError::Other)?;
                Ok(server_to_info(server))
            }
            Err(e) if is_capacity_error(&e) => Err(ProvisionError::NoCapacity {
                server_type: spec.server_type,
                regions: vec![spec.region],
            }),
            Err(e) => Err(ProvisionError::Other(e)),
        }
    }

    async fn get_server(&self, id: &ServerId) -> Result<Option<ServerInfo>> {
        let raw = server_id_to_i64(id)?;
        match self.client.get_server(raw).await {
            Ok(s) => Ok(Some(server_to_info(s))),
            Err(e) => {
                if is_not_found(&e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn find_server(&self, name: &str) -> Result<Option<ServerInfo>> {
        Ok(self.client.find_server(name).await?.map(server_to_info))
    }

    async fn delete_server(&self, id: &ServerId) -> Result<()> {
        let raw = server_id_to_i64(id)?;
        match self.client.delete_server(raw).await {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn wait_server_running(
        &self,
        id: &ServerId,
        timeout: Duration,
    ) -> Result<ServerInfo> {
        let raw = server_id_to_i64(id)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let s = self.client.get_server(raw).await?;
            let info = server_to_info(s);
            if info.status == ServerStatus::Running {
                return Ok(info);
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "server {id} did not reach `running` within {timeout:?} (status={:?})",
                    info.status
                ));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // ── Volumes ─────────────────────────────────────────────────────

    async fn create_volume(&self, spec: VolumeSpec) -> Result<VolumeInfo> {
        let mut labels = HashMap::with_capacity(spec.labels.len());
        for (k, v) in &spec.labels {
            labels.insert(k.clone(), v.clone());
        }
        let resp = self
            .client
            .create_volume(CreateVolumeRequest {
                name: spec.name,
                size: spec.size_gb,
                location: spec.region,
                format: "ext4".into(),
                labels,
                automount: false,
            })
            .await?;
        if let Some(action) = resp.action {
            self.client
                .wait_action(action.id, Duration::from_secs(120))
                .await?;
        }
        for action in resp.next_actions {
            self.client
                .wait_action(action.id, Duration::from_secs(120))
                .await?;
        }
        Ok(volume_to_info(resp.volume))
    }

    async fn get_volume(&self, id: &VolumeId) -> Result<Option<VolumeInfo>> {
        let raw = volume_id_to_i64(id)?;
        match self.client.get_volume(raw).await {
            Ok(v) => Ok(Some(volume_to_info(v))),
            Err(e) => {
                if is_not_found(&e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn find_volume(&self, name: &str) -> Result<Option<VolumeInfo>> {
        Ok(self.client.find_volume(name).await?.map(volume_to_info))
    }

    async fn attach_volume(
        &self,
        vol: &VolumeId,
        srv: &ServerId,
    ) -> Result<AttachedDevice> {
        let vraw = volume_id_to_i64(vol)?;
        let sraw = server_id_to_i64(srv)?;
        self.client.attach_volume(vraw, sraw).await?;
        // Hetzner's device-path formula is deterministic from the
        // volume id — `/dev/disk/by-id/scsi-0HC_Volume_<id>`. No
        // post-attach polling needed; the mount script just waits
        // for the device file to appear.
        Ok(AttachedDevice {
            device_path: format!("/dev/disk/by-id/scsi-0HC_Volume_{vraw}"),
        })
    }

    async fn detach_volume(&self, vol: &VolumeId) -> Result<()> {
        let raw = volume_id_to_i64(vol)?;
        match self.client.detach_volume(raw).await {
            Ok(()) => Ok(()),
            // Already-detached / vanished volumes — fine.
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn delete_volume(&self, id: &VolumeId) -> Result<()> {
        let raw = volume_id_to_i64(id)?;
        match self.client.delete_volume(raw).await {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Heuristic: did Hetzner respond with a 404? The REST client
/// stringifies errors as `METHOD URL → STATUS: body` so substring
/// match is the cheapest reliable detector. `not_found` is Hetzner's
/// JSON code; `404` covers the bare-HTTP path.
fn is_not_found(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("→ 404") || s.contains("\"not_found\"")
}
