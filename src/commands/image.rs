//! `cargo burst image build` — provision a temporary server, run the bake
//! script via cloud-init, snapshot the disk, destroy the server.
//!
//! Run once per toolchain refresh. The resulting image ID is saved to
//! `state.json` and used by every subsequent `cargo burst build`.

use anyhow::{Context, Result, anyhow};
use clap::Args;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::hcloud::{
    CreateServerRequest, HCloud, ImageRef,
};
use crate::ssh;

/// Hard ceiling on the bake script's wall-clock runtime. The script
/// installs a Rust toolchain, sccache, and nextest from the network — on
/// a slow Hetzner network day this can stretch, so we give it 20 minutes.
const BAKE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How long we wait for Hetzner to finish snapshotting after `shutdown`.
/// Snapshots of a small disk usually finish in 2–5 minutes; 30 is a safe
/// ceiling.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Args, Debug)]
pub struct ImageBuildArgs {
    /// Server type to use for the bake VM. Defaults to `cpx22` because
    /// the bake is I/O- and download-bound, not CPU-bound — paying for a
    /// dedicated-core VM here is wasted.
    #[arg(long, default_value = "cpx22")]
    pub bake_server_type: String,
    /// Description on the resulting snapshot. Defaults to a timestamped
    /// "cargo-burst <date>" so multiple snapshots are distinguishable.
    #[arg(long)]
    pub description: Option<String>,
    /// Keep the bake VM alive on failure, so you can ssh in to debug
    /// `/var/log/cargo-burst-bake.log`. By default we destroy on failure
    /// to avoid leaking billable resources.
    #[arg(long)]
    pub keep_on_failure: bool,
}

const BAKE_SCRIPT: &str = include_str!("../../scripts/bake-image.sh");

pub async fn run(args: ImageBuildArgs) -> Result<()> {
    let cfg = Config::load()?;
    // Image bake is long-running; we touch state.json only at the very end
    // to record the new image id. Don't load it now — `update_state` below
    // re-reads under the lock at write time.
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    // 1. Local SSH key + register on Hetzner.
    let ssh_key_path = cfg.ssh_key_path()?;
    tracing::info!(path = %ssh_key_path.display(), "ensuring local SSH key");
    let pubkey = ssh::ensure_ssh_key(&ssh_key_path).await?;
    let hetzner_key = hcloud.ensure_ssh_key("cargo-burst", &pubkey).await?;
    tracing::info!(id = hetzner_key.id, "ssh key registered with Hetzner");

    // 2. Build user-data: substitute the public key into the bake script.
    let user_data = BAKE_SCRIPT.replace("__CARGO_BURST_PUBKEY__", pubkey.trim());

    // 3. Provision the bake server. We pass the bake script as user_data;
    //    cloud-init runs it on first boot. We never ssh in during the bake
    //    — the script runs to completion and powers the machine off,
    //    which is our completion signal.
    let server_name = format!(
        "cargo-burst-bake-{}",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::parse("[year][month][day]-[hour][minute][second]")?)
            .unwrap_or_else(|_| "now".into())
    );
    let mut labels = HashMap::new();
    labels.insert("managed-by".into(), "cargo-burst".into());
    labels.insert("role".into(), "bake".into());

    tracing::info!(name = %server_name, server_type = %args.bake_server_type, "provisioning bake VM");
    let create = hcloud
        .create_server(CreateServerRequest {
            name: server_name.clone(),
            server_type: args.bake_server_type.clone(),
            image: ImageRef::Name("ubuntu-24.04".into()),
            // Image bake uses the user's first-preference region. No
            // capacity fallback here: this is a manual, one-shot
            // operation, and if that region is full the user can
            // simply wait or re-run after rotating their config.
            location: cfg.region_preference().remove(0),
            ssh_keys: vec![hetzner_key.id],
            volumes: vec![],
            user_data: Some(user_data),
            labels,
            start_after_create: true,
        })
        .await?;
    let server_id = create.server.id;
    let server_ip = create
        .server
        .public_net
        .ipv4
        .as_ref()
        .map(|i| i.ip.clone())
        .unwrap_or_else(|| "<no-ipv4>".into());
    tracing::info!(id = server_id, ip = %server_ip, "bake VM provisioned");
    println!();
    println!("  bake VM:     id={server_id}  ip={server_ip}");
    println!("  watch log:   http://{server_ip}:8473/cargo-burst-bake.log");
    println!("  ssh debug:   ssh -i {} root@{server_ip}", ssh_key_path.display());
    println!();

    // From here on, any failure must clean up the bake server (unless the
    // user passed --keep-on-failure).
    let bake_outcome = run_bake_inner(&hcloud, server_id, args.description.as_deref()).await;

    match &bake_outcome {
        Ok(image_id) => {
            tracing::info!(image_id, "snapshot ready; deleting bake VM");
            if let Err(e) = hcloud.delete_server(server_id).await {
                tracing::warn!("failed to delete bake VM {server_id}: {e}");
            }
            crate::config::update_state(|s| {
                s.image_id = Some(*image_id);
                Ok(())
            })
            .await?;
            println!("✓ Image baked: id={image_id}");
            println!("  Saved to {}", crate::config::state_path()?.display());
        }
        Err(e) => {
            if args.keep_on_failure {
                tracing::error!(
                    "bake failed; leaving server {server_id} alive for debugging \
                     (ssh work@{}). Error: {e}",
                    create
                        .server
                        .public_net
                        .ipv4
                        .as_ref()
                        .map(|i| i.ip.as_str())
                        .unwrap_or("?"),
                );
            } else {
                tracing::error!("bake failed; deleting server {server_id}. Error: {e}");
                if let Err(e2) = hcloud.delete_server(server_id).await {
                    tracing::warn!("failed to delete bake VM {server_id}: {e2}");
                }
            }
        }
    }

    bake_outcome.map(|_| ())
}

/// Inner bake flow: poll until the server powers off (signal that the
/// cloud-init script ran to completion), then snapshot. Returns the new
/// image's ID on success.
async fn run_bake_inner(
    hcloud: &HCloud,
    server_id: i64,
    description: Option<&str>,
) -> Result<i64> {
    // 1. Wait for cloud-init to power the machine off. Hetzner exposes
    //    server status: `running`, `off`, etc.
    let bake_start = Instant::now();
    tracing::info!("waiting for bake script to finish (server will power off)");
    let mut last_status = String::new();
    loop {
        let server = hcloud.get_server(server_id).await?;
        if server.status != last_status {
            tracing::info!(status = %server.status, elapsed_secs = bake_start.elapsed().as_secs(), "bake VM status");
            last_status = server.status.clone();
        }
        if server.status == "off" {
            tracing::info!(elapsed = ?bake_start.elapsed(), "bake script complete");
            break;
        }
        if bake_start.elapsed() >= BAKE_TIMEOUT {
            return Err(anyhow!(
                "bake script did not complete within {:?} (server still {})",
                BAKE_TIMEOUT,
                server.status
            ));
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    // 2. Snapshot. Hetzner returns an action + the new image id immediately,
    //    but the image won't be `available` until the snapshot finishes.
    let description = description
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "cargo-burst image baked {}",
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Iso8601::DEFAULT)
                    .unwrap_or_default()
            )
        });
    tracing::info!(%description, "creating snapshot");
    let snap_start = Instant::now();
    let create = hcloud
        .create_image_from_server(server_id, &description)
        .await
        .context("creating image")?;
    tracing::info!(image_id = create.image_id, action_id = create.action_id, "snapshot in progress");

    hcloud
        .wait_image_ready(create.image_id, SNAPSHOT_TIMEOUT)
        .await
        .with_context(|| format!("waiting for image {}", create.image_id))?;
    tracing::info!(elapsed = ?snap_start.elapsed(), "snapshot ready");

    Ok(create.image_id)
}
