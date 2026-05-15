//! `cargo burst image build` — provision a temporary server, run the bake
//! script via cloud-init, snapshot the disk, destroy the server.
//!
//! Run once per toolchain refresh. The resulting image ID is saved to
//! `state.json` and used by every subsequent `cargo burst build`.

use anyhow::{Context, Result, anyhow};
use clap::Args;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::provider::{self, ImageId, Provider, ServerId, ServerSpec, ServerStatus};
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
    /// Server type to use for the bake VM. Defaults are
    /// provider-specific (`cpx22` on Hetzner, `m7a.large` on AWS).
    /// AWS's `t3.medium` burstable was tried first and exhausted its
    /// CPU credits during the rust toolchain + sccache compile phase;
    /// `m7a.large` (2 vCPU sustained, 8 GB RAM) finishes the same
    /// workload in ~10 min for roughly the same price-per-hour.
    #[arg(long)]
    pub bake_server_type: Option<String>,
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
    let provider = provider::from_config(&cfg).await?;

    // 1. Local SSH key + register with provider.
    let ssh_key_path = cfg.ssh_key_path()?;
    tracing::info!(path = %ssh_key_path.display(), "ensuring local SSH key");
    let pubkey = ssh::ensure_ssh_key(&ssh_key_path).await?;
    let ssh_key_id = provider.ensure_ssh_key("cargo-burst", &pubkey).await?;
    tracing::info!(id = %ssh_key_id, "ssh key registered with provider");

    // 2. Build user-data: substitute the public key into the bake script.
    //    Note: the bake script is Hetzner-flavored (Ubuntu image name,
    //    apt-based). Phase B will need a parallel AWS bake path.
    let user_data = BAKE_SCRIPT.replace("__CARGO_BURST_PUBKEY__", pubkey.trim());

    // 3. Provision the bake server. Cloud-init runs the script on first
    //    boot. We never ssh in during the bake — the script runs to
    //    completion and powers the machine off, which is our completion
    //    signal.
    //
    //    On Hetzner the bake uses a base Ubuntu image, identified by the
    //    well-known name "ubuntu-24.04". We pass it through the provider
    //    trait's ImageId which here happens to be that string rather
    //    than a snapshot id — `HcloudProvider::create_server` parses the
    //    ImageId as an i64 ID, which "ubuntu-24.04" obviously isn't, so
    //    bake currently still talks to the HCloud client directly via
    //    the create_server path. We accept this temporary leakage: bake
    //    is Hetzner-flavored end-to-end (apt commands in the script),
    //    so we don't claim cross-provider parity here. The trait-only
    //    path comes back online once we have a baked snapshot ID.
    let server_name = format!(
        "cargo-burst-bake-{}",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::parse("[year][month][day]-[hour][minute][second]")?)
            .unwrap_or_else(|_| "now".into())
    );
    let mut labels = BTreeMap::new();
    labels.insert("managed-by".into(), "cargo-burst".into());
    // `role=bake` is load-bearing: Ec2Provider routes labelled bake
    // servers to on-demand regardless of `use_spot` (one-shot, must
    // succeed). Don't drop this label without also revisiting that
    // dispatch.
    labels.insert("role".into(), "bake".into());

    // Pick the bake instance/server type. Default depends on the
    // provider: cpx22 on Hetzner, m7a.large on AWS. The bake does
    // sustained-CPU work (rust toolchain compile, sccache build) so
    // a burstable t3 class isn't viable here — it ran out of credits
    // before completing.
    let bake_server_type = args.bake_server_type.clone().unwrap_or_else(|| {
        match provider.name() {
            "aws" => "m7a.large".to_string(),
            _ => "cpx22".to_string(),
        }
    });

    // Image-bake "image" is the base OS — Hetzner accepts the name
    // "ubuntu-24.04" as an image reference. We stuff that into the
    // ImageId newtype and the HcloudProvider's create_server path
    // detects the non-numeric form. (Implementation note: we wrap it
    // in ImageId here and HcloudProvider parses it specially.)
    //
    // For now Phase A keeps image-bake Hetzner-only and uses the
    // underlying client directly via a thin internal helper on the
    // provider type. To avoid leaking that, we go through a
    // bake-specific path: most providers offer a "bake from base OS"
    // primitive, but the API surface differs enough that we keep it
    // in the impl rather than the public trait.
    tracing::info!(
        name = %server_name,
        server_type = %bake_server_type,
        provider = %provider.name(),
        "provisioning bake VM"
    );
    // We pass the well-known base-image name as a magic ImageId
    // string; both `HcloudProvider` and `Ec2Provider` detect the
    // "ubuntu-24.04" magic name and translate it (hcloud image
    // reference on Hetzner; SSM-parameter-store AMI lookup on AWS).
    let create = match provider
        .create_server(ServerSpec {
            name: server_name.clone(),
            server_type: bake_server_type.clone(),
            image: ImageId("ubuntu-24.04".into()),
            region: cfg.region_preference().remove(0),
            ssh_keys: vec![ssh_key_id.clone()],
            user_data: Some(user_data),
            labels,
            attached_volumes: Vec::new(),
        })
        .await
    {
        Ok(info) => info,
        Err(crate::provider::ProvisionError::NoCapacity { regions, .. }) => {
            return Err(anyhow!(
                "bake region {:?} is at capacity; pass --region or try later",
                regions
            ));
        }
        Err(crate::provider::ProvisionError::Other(e)) => return Err(e),
    };
    let server_id = create.id.clone();
    let server_ip = create
        .public_ip
        .clone()
        .unwrap_or_else(|| "<no-ipv4>".into());
    tracing::info!(id = %server_id, ip = %server_ip, "bake VM provisioned");
    println!();
    println!("  bake VM:     id={server_id}  ip={server_ip}");
    println!("  watch log:   http://{server_ip}:8473/cargo-burst-bake.log");
    println!("  ssh debug:   ssh -i {} root@{server_ip}", ssh_key_path.display());
    println!();

    // From here on, any failure must clean up the bake server (unless the
    // user passed --keep-on-failure).
    let bake_outcome = run_bake_inner(
        provider.as_ref(),
        &server_id,
        args.description.as_deref(),
    )
    .await;

    match &bake_outcome {
        Ok(image_id) => {
            tracing::info!(image_id = %image_id, "snapshot ready; deleting bake VM");
            if let Err(e) = provider.delete_server(&server_id).await {
                tracing::warn!("failed to delete bake VM {server_id}: {e}");
            }
            let image_id_clone = image_id.clone();
            crate::config::update_state(move |s| {
                s.image_id = Some(image_id_clone);
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
                     (ssh work@{server_ip}). Error: {e}"
                );
            } else {
                tracing::error!("bake failed; deleting server {server_id}. Error: {e}");
                if let Err(e2) = provider.delete_server(&server_id).await {
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
    provider: &dyn Provider,
    server_id: &ServerId,
    description: Option<&str>,
) -> Result<ImageId> {
    // 1. Wait for cloud-init to power the machine off.
    let bake_start = Instant::now();
    tracing::info!("waiting for bake script to finish (server will power off)");
    let mut last_status: Option<String> = None;
    loop {
        let server = provider
            .get_server(server_id)
            .await?
            .ok_or_else(|| anyhow!("bake VM disappeared mid-bake"))?;
        let status_str = server.status.as_str().to_string();
        if last_status.as_deref() != Some(&status_str) {
            tracing::info!(status = %status_str, elapsed_secs = bake_start.elapsed().as_secs(), "bake VM status");
            last_status = Some(status_str.clone());
        }
        if matches!(server.status, ServerStatus::Stopped) {
            tracing::info!(elapsed = ?bake_start.elapsed(), "bake script complete");
            break;
        }
        if bake_start.elapsed() >= BAKE_TIMEOUT {
            return Err(anyhow!(
                "bake script did not complete within {:?} (server still {})",
                BAKE_TIMEOUT,
                status_str
            ));
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    // 2. Snapshot.
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
    let image_id = provider
        .bake_image_from_server(server_id, &description)
        .await
        .context("creating image")?;
    tracing::info!(image_id = %image_id, "snapshot in progress");

    provider
        .wait_image_ready(&image_id, SNAPSHOT_TIMEOUT)
        .await
        .with_context(|| format!("waiting for image {image_id}"))?;
    tracing::info!(elapsed = ?snap_start.elapsed(), "snapshot ready");

    Ok(image_id)
}
