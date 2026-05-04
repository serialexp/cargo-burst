//! Thin client around the subset of the Hetzner Cloud REST API that
//! cargo-burst needs: SSH keys, servers, volumes, and images.
//!
//! No code generation, no full SDK — just typed requests for the ~10
//! endpoints we actually call. Hetzner's API is small enough that this is
//! cheaper than pulling a generated client.

mod types;

pub use types::*;

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

const BASE_URL: &str = "https://api.hetzner.cloud/v1";

/// Wrapper holding the API token + a reusable HTTP client.
#[derive(Clone)]
pub struct HCloud {
    token: String,
    http: Client,
}

impl HCloud {
    pub fn new(token: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent("cargo-burst/0.1")
            .timeout(Duration::from_secs(60))
            .build()
            .context("building reqwest client")?;
        Ok(Self { token, http })
    }

    /// Issue an authenticated request. Body is JSON; response is parsed as
    /// JSON unless the response is 204 (no content).
    async fn request<B: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R> {
        let url = format!("{BASE_URL}{path}");
        let mut req = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "{method} {url} → {status}: {body}",
                body = text.chars().take(500).collect::<String>()
            ));
        }
        if status == StatusCode::NO_CONTENT || text.is_empty() {
            // Caller is expected to use `()` as R for these endpoints.
            return serde_json::from_str("null")
                .or_else(|_| serde_json::from_str("{}"))
                .context("decoding empty response");
        }
        serde_json::from_str(&text)
            .with_context(|| format!("decoding response from {method} {url}: {text}"))
    }

    // ── SSH keys ────────────────────────────────────────────────────────

    /// Find an SSH key by name. Hetzner enforces uniqueness on names per
    /// project, so this is sufficient for "do we already have it?".
    pub async fn find_ssh_key(&self, name: &str) -> Result<Option<SshKey>> {
        let path = format!("/ssh_keys?name={}", urlencoding::encode_minimal(name));
        let resp: SshKeysResponse = self.request::<(), _>(Method::GET, &path, None).await?;
        Ok(resp.ssh_keys.into_iter().next())
    }

    pub async fn create_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKey> {
        let body = CreateSshKeyRequest {
            name: name.to_string(),
            public_key: public_key.to_string(),
        };
        let resp: CreateSshKeyResponse = self
            .request(Method::POST, "/ssh_keys", Some(&body))
            .await?;
        Ok(resp.ssh_key)
    }

    /// Idempotent helper: returns the existing key if its name matches, or
    /// creates a new one. If a key with the same name exists but with a
    /// different public key, it's deleted and recreated — covers the case
    /// where the local SSH key was regenerated.
    pub async fn ensure_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKey> {
        if let Some(existing) = self.find_ssh_key(name).await? {
            if existing.public_key.trim() == public_key.trim() {
                return Ok(existing);
            }
            tracing::info!(id = existing.id, "ssh key public_key changed; recreating");
            self.delete_ssh_key(existing.id).await?;
        }
        self.create_ssh_key(name, public_key).await
    }

    pub async fn delete_ssh_key(&self, id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .request::<(), _>(Method::DELETE, &format!("/ssh_keys/{id}"), None)
            .await?;
        Ok(())
    }

    // ── Servers ─────────────────────────────────────────────────────────

    pub async fn create_server(&self, req: CreateServerRequest) -> Result<CreateServerResponse> {
        self.request(Method::POST, "/servers", Some(&req)).await
    }

    pub async fn get_server(&self, id: i64) -> Result<Server> {
        let resp: ServerResponse = self
            .request::<(), _>(Method::GET, &format!("/servers/{id}"), None)
            .await?;
        Ok(resp.server)
    }

    pub async fn find_server(&self, name: &str) -> Result<Option<Server>> {
        let path = format!("/servers?name={}", urlencoding::encode_minimal(name));
        let resp: ServersResponse = self.request::<(), _>(Method::GET, &path, None).await?;
        Ok(resp.servers.into_iter().next())
    }

    pub async fn delete_server(&self, id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .request::<(), _>(Method::DELETE, &format!("/servers/{id}"), None)
            .await?;
        Ok(())
    }

    /// Hetzner returns 202 + an Action whose progress we'd normally poll.
    /// For our use we just want the resulting image id, so we wait inline.
    pub async fn create_image_from_server(
        &self,
        server_id: i64,
        description: &str,
    ) -> Result<CreateImageResult> {
        #[derive(Serialize)]
        struct Req<'a> {
            description: &'a str,
            #[serde(rename = "type")]
            type_: &'a str,
        }
        let body = Req { description, type_: "snapshot" };
        let resp: CreateImageActionResponse = self
            .request(
                Method::POST,
                &format!("/servers/{server_id}/actions/create_image"),
                Some(&body),
            )
            .await?;
        Ok(CreateImageResult {
            image_id: resp.image.id,
            action_id: resp.action.id,
        })
    }

    pub async fn get_image(&self, id: i64) -> Result<Image> {
        let resp: ImageResponse = self
            .request::<(), _>(Method::GET, &format!("/images/{id}"), None)
            .await?;
        Ok(resp.image)
    }

    pub async fn delete_image(&self, id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .request::<(), _>(Method::DELETE, &format!("/images/{id}"), None)
            .await?;
        Ok(())
    }

    // ── Volumes ─────────────────────────────────────────────────────────

    pub async fn create_volume(&self, req: CreateVolumeRequest) -> Result<CreateVolumeResponse> {
        self.request(Method::POST, "/volumes", Some(&req)).await
    }

    pub async fn get_volume(&self, id: i64) -> Result<Volume> {
        let resp: VolumeResponse = self
            .request::<(), _>(Method::GET, &format!("/volumes/{id}"), None)
            .await?;
        Ok(resp.volume)
    }

    pub async fn find_volume(&self, name: &str) -> Result<Option<Volume>> {
        let path = format!("/volumes?name={}", urlencoding::encode_minimal(name));
        let resp: VolumesResponse = self.request::<(), _>(Method::GET, &path, None).await?;
        Ok(resp.volumes.into_iter().next())
    }

    pub async fn attach_volume(&self, volume_id: i64, server_id: i64) -> Result<()> {
        #[derive(Serialize)]
        struct Req { server: i64, automount: bool }
        let _: serde_json::Value = self
            .request(
                Method::POST,
                &format!("/volumes/{volume_id}/actions/attach"),
                Some(&Req { server: server_id, automount: false }),
            )
            .await?;
        Ok(())
    }

    /// Detach `volume_id` from whichever server it's attached to. Hetzner
    /// requires a volume to be detached before delete; the volume reaper
    /// calls this immediately before `delete_volume`. No-op-friendly:
    /// the API returns success even if the volume is already detached.
    pub async fn detach_volume(&self, volume_id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .request::<(), _>(
                Method::POST,
                &format!("/volumes/{volume_id}/actions/detach"),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn delete_volume(&self, id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .request::<(), _>(Method::DELETE, &format!("/volumes/{id}"), None)
            .await?;
        Ok(())
    }

    // ── Actions ─────────────────────────────────────────────────────────

    /// Poll an action until it reaches `success` or `error`.
    pub async fn wait_action(&self, action_id: i64, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let resp: ActionResponse = self
                .request::<(), _>(Method::GET, &format!("/actions/{action_id}"), None)
                .await?;
            match resp.action.status.as_str() {
                "success" => return Ok(()),
                "error" => {
                    return Err(anyhow!(
                        "action {action_id} failed: {:?}",
                        resp.action.error
                    ));
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!("action {action_id} did not complete in time"));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// Wait for an image to transition out of `creating` to `available` or
    /// `unavailable`.
    pub async fn wait_image_ready(&self, image_id: i64, timeout: Duration) -> Result<Image> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let img = self.get_image(image_id).await?;
            match img.status.as_str() {
                "available" => return Ok(img),
                "creating" => {}
                other => {
                    return Err(anyhow!(
                        "image {image_id} entered unexpected status {other}"
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "image {image_id} did not become available in time"
                ));
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Tiny URL-encoder for the few simple values we pass in query strings.
/// reqwest's builder handles this for us when we pass query() pairs, but
/// the inline format!()s above keep the call sites readable and we only
/// need to encode a handful of identifier characters.
mod urlencoding {
    pub fn encode_minimal(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
}

pub struct CreateImageResult {
    pub image_id: i64,
    pub action_id: i64,
}
