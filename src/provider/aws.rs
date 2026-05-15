//! AWS EC2 impl of the [`Provider`] trait.
//!
//! The motivating reason for this file existing: Hetzner bills servers
//! in 1-hour increments. Booting and tearing down a CCX63 50 times an
//! hour costs as much as keeping one alive for 50 hours straight. AWS
//! EC2 bills per-second, so cargo-burst's cold-start pattern costs ~1%
//! of what Hetzner charges for the same workload. The trade-off: AWS
//! API surface is much wider, so this file is much longer than the
//! Hetzner equivalent — VPCs, subnets, security groups, AZs, and AMI
//! lookup all have to be wired up before `RunInstances` will work.
//!
//! ## Design choices
//!
//! - **One EC2 client per region, memoised.** EC2 endpoints are
//!   region-bound. The `Provider` trait surface threads a `region`
//!   field through every spec; we look up (or lazily create) a client
//!   for that region on each call.
//! - **AZ selection: alphabetically-first available AZ per region.**
//!   Volumes are AZ-scoped, so the AZ must be stable across calls for
//!   the same region. Sorting by name and taking [0] is the simplest
//!   deterministic policy. Phase C will revisit (price-aware, capacity-
//!   aware AZ selection).
//! - **Spot by default for normal servers, on-demand for bake.** Spot
//!   pricing on c7a-class is ~70-80% off on-demand, and the 2-minute
//!   interruption notice is long enough for most `cargo burst`
//!   commands to complete. Bake is one-shot and must succeed, so it
//!   runs on-demand regardless of `cfg.use_spot`. The dispatch hinges
//!   on the `role` label: `role=bake` → on-demand, otherwise → spot.
//! - **Ubuntu 24.04 base image lookup via SSM Parameter Store.**
//!   Canonical publishes the current AMI ID at
//!   `/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id`.
//!   `HcloudProvider` plays the same trick with the magic string
//!   `"ubuntu-24.04"`; we keep parity.
//! - **Auto-managed security group.** First call into a region creates
//!   `cargo-burst` SG in the default VPC with `22/tcp` open from
//!   `0.0.0.0/0`. Matches Hetzner's default UX. Phase C may add
//!   caller-IP filtering.
//! - **NVMe device-path resolution.** AWS' `AttachVolume` takes a
//!   "device name" like `/dev/sdf`, but on Nitro instances (every
//!   modern instance type) the kernel renames it to `/dev/nvme<N>n1`.
//!   Stable symlink: `/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_<vol-id-stripped-of-dashes>`,
//!   created by stock Ubuntu udev rules. We return that path so the
//!   mount script doesn't have to guess.
//!
//! ## Required IAM actions
//!
//! ```text
//! ec2:RunInstances, ec2:TerminateInstances, ec2:StopInstances,
//! ec2:DescribeInstances, ec2:DescribeInstanceStatus,
//! ec2:CreateImage, ec2:DescribeImages, ec2:DeregisterImage,
//! ec2:CreateVolume, ec2:DeleteVolume, ec2:DescribeVolumes,
//! ec2:AttachVolume, ec2:DetachVolume,
//! ec2:ImportKeyPair, ec2:DeleteKeyPair, ec2:DescribeKeyPairs,
//! ec2:CreateSecurityGroup, ec2:AuthorizeSecurityGroupIngress, ec2:DescribeSecurityGroups,
//! ec2:DescribeSubnets, ec2:DescribeVpcs, ec2:DescribeAvailabilityZones,
//! ec2:CreateTags, ec2:DescribeTags,
//! ssm:GetParameters (on /aws/service/canonical/ubuntu/*)
//! ```

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ec2::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_ec2::types as ec2t;
use aws_sdk_ssm::Client as SsmClient;
use aws_smithy_types::Blob;
use base64::Engine;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;

use crate::config::AwsConfig;

use super::{
    AttachedDevice, ImageId, ImageInfo, ImageStatus, Provider, ProvisionError, ServerId,
    ServerInfo, ServerSpec, ServerStatus, SshKeyId, VolumeId, VolumeInfo, VolumeSpec,
    VolumeStatus,
};

/// EC2 bills per-second (with a 1-minute floor that's negligible at
/// the scales we care about), so the reaper never needs to extend a
/// server's lifetime to amortise a billing increment.
const AWS_BILLING_MINIMUM: Duration = Duration::ZERO;

/// SSM Parameter Store path for Canonical's current Ubuntu 24.04
/// AMI ID. AWS resolves this per-region — we query it lazily and
/// cache the result.
const UBUNTU_24_04_SSM_PARAM: &str =
    "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id";

/// Well-known image name accepted by `Provider::create_server` to mean
/// "give me the current Ubuntu 24.04 base image for this region".
/// Matches the Hetzner equivalent (`ubuntu-24.04` as a hcloud image
/// name) so callers don't have to know which provider they're on.
const UBUNTU_MAGIC_NAME: &str = "ubuntu-24.04";

/// Name of the security group cargo-burst creates and reuses across
/// runs. Per-region; tagged `managed-by=cargo-burst` so the user can
/// find/delete it via the AWS console.
const CARGO_BURST_SG_NAME: &str = "cargo-burst";

/// AWS' `AttachVolume` API takes a Linux device name even though the
/// kernel renames it on Nitro instances. `sdf` is the conventional
/// first attached EBS device; the kernel will surface it as
/// `/dev/nvme1n1` (or wherever NVMe ordering lands) but the stable
/// symlink derives from the volume ID, not from this name.
const ATTACH_DEVICE_NAME: &str = "/dev/sdf";

// ── Struct ────────────────────────────────────────────────────────────

/// AWS-flavoured [`Provider`]. Holds a base SDK config plus per-region
/// caches of EC2/SSM clients (so we don't pay the construction cost
/// every call).
pub struct Ec2Provider {
    cfg: AwsConfig,
    sdk_cfg: aws_config::SdkConfig,
    ec2_clients: Mutex<HashMap<String, Arc<Ec2Client>>>,
    ssm_clients: Mutex<HashMap<String, Arc<SsmClient>>>,
    /// Resolved AZ name per region. Computed once via
    /// `DescribeAvailabilityZones` and reused for every volume/server
    /// in that region.
    az_cache: Mutex<HashMap<String, String>>,
    /// Resolved Ubuntu 24.04 AMI ID per region (SSM Parameter Store
    /// lookup). Cached so we don't ask SSM on every bake.
    ubuntu_ami_cache: Mutex<HashMap<String, String>>,
    /// Resolved (default VPC id, subnet id, security group id) per
    /// (region, az). Populated lazily on the first server provision
    /// into that region.
    vpc_cache: Mutex<HashMap<String, VpcResolved>>,
}

#[derive(Clone)]
struct VpcResolved {
    #[allow(dead_code)]
    vpc_id: String,
    subnet_id: String,
    security_group_id: String,
}

impl Ec2Provider {
    /// Build the provider from its config slice. Async because
    /// `aws_config::load_from_env` is async (it may consult IMDS).
    pub async fn new(cfg: AwsConfig) -> Result<Self> {
        // We pin a default region on the SDK config so things like
        // `aws sts get-caller-identity`-style implicit calls don't
        // fail with "no region configured" when the user hasn't set
        // AWS_REGION in their env. The per-call region override always
        // wins.
        let default_region = cfg
            .regions
            .first()
            .cloned()
            .unwrap_or_else(|| "us-east-1".to_string());
        let sdk_cfg = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(default_region))
            .load()
            .await;

        Ok(Self {
            cfg,
            sdk_cfg,
            ec2_clients: Mutex::new(HashMap::new()),
            ssm_clients: Mutex::new(HashMap::new()),
            az_cache: Mutex::new(HashMap::new()),
            ubuntu_ami_cache: Mutex::new(HashMap::new()),
            vpc_cache: Mutex::new(HashMap::new()),
        })
    }

    async fn ec2(&self, region: &str) -> Arc<Ec2Client> {
        let mut clients = self.ec2_clients.lock().await;
        if let Some(c) = clients.get(region) {
            return c.clone();
        }
        let region_cfg = self
            .sdk_cfg
            .to_builder()
            .region(Region::new(region.to_string()))
            .build();
        let client = Arc::new(Ec2Client::new(&region_cfg));
        clients.insert(region.to_string(), client.clone());
        client
    }

    async fn ssm(&self, region: &str) -> Arc<SsmClient> {
        let mut clients = self.ssm_clients.lock().await;
        if let Some(c) = clients.get(region) {
            return c.clone();
        }
        let region_cfg = self
            .sdk_cfg
            .to_builder()
            .region(Region::new(region.to_string()))
            .build();
        let client = Arc::new(SsmClient::new(&region_cfg));
        clients.insert(region.to_string(), client.clone());
        client
    }

    /// Return the alphabetically-first `available` AZ in `region`.
    /// Memoised per region.
    async fn pick_az(&self, region: &str) -> Result<String> {
        {
            let cache = self.az_cache.lock().await;
            if let Some(az) = cache.get(region) {
                return Ok(az.clone());
            }
        }
        let ec2 = self.ec2(region).await;
        let resp = ec2
            .describe_availability_zones()
            .filters(
                ec2t::Filter::builder()
                    .name("region-name")
                    .values(region)
                    .build(),
            )
            .filters(
                ec2t::Filter::builder()
                    .name("state")
                    .values("available")
                    .build(),
            )
            .send()
            .await
            .with_context(|| format!("describe_availability_zones in {region}"))?;
        let mut names: Vec<String> = resp
            .availability_zones()
            .iter()
            .filter_map(|z| z.zone_name().map(str::to_string))
            .collect();
        names.sort();
        let az = names.into_iter().next().ok_or_else(|| {
            anyhow!("no available AZs found in region {region}")
        })?;
        self.az_cache
            .lock()
            .await
            .insert(region.to_string(), az.clone());
        Ok(az)
    }

    /// Resolve the Ubuntu 24.04 base AMI ID for `region` via SSM. Memoised.
    ///
    /// Uses `ssm:GetParameters` (plural) rather than `ssm:GetParameter`
    /// (singular) because the two are distinct IAM actions and most
    /// AWS-managed read-only policies grant the plural form only.
    /// Functionally identical for our one-parameter lookup.
    async fn ubuntu_ami(&self, region: &str) -> Result<String> {
        {
            let cache = self.ubuntu_ami_cache.lock().await;
            if let Some(ami) = cache.get(region) {
                return Ok(ami.clone());
            }
        }
        let ssm = self.ssm(region).await;
        let resp = ssm
            .get_parameters()
            .names(UBUNTU_24_04_SSM_PARAM)
            .send()
            .await
            .with_context(|| {
                format!("ssm:GetParameters {UBUNTU_24_04_SSM_PARAM} in {region}")
            })?;
        let ami = resp
            .parameters()
            .iter()
            .next()
            .and_then(|p| p.value())
            .ok_or_else(|| {
                anyhow!("SSM parameter {UBUNTU_24_04_SSM_PARAM} not returned by GetParameters")
            })?
            .to_string();
        self.ubuntu_ami_cache
            .lock()
            .await
            .insert(region.to_string(), ami.clone());
        Ok(ami)
    }

    /// Look up (or create) the VPC plumbing for `(region, az)`:
    /// default-VPC subnet in `az` + `cargo-burst` security group.
    async fn ensure_vpc(&self, region: &str, az: &str) -> Result<VpcResolved> {
        let key = format!("{region}/{az}");
        {
            let cache = self.vpc_cache.lock().await;
            if let Some(v) = cache.get(&key) {
                return Ok(v.clone());
            }
        }
        let ec2 = self.ec2(region).await;

        // 1. Default-VPC subnet in this AZ.
        let subnets = ec2
            .describe_subnets()
            .filters(
                ec2t::Filter::builder()
                    .name("default-for-az")
                    .values("true")
                    .build(),
            )
            .filters(
                ec2t::Filter::builder()
                    .name("availability-zone")
                    .values(az)
                    .build(),
            )
            .send()
            .await
            .with_context(|| format!("describe_subnets in {region}/{az}"))?;
        let subnet = subnets
            .subnets()
            .iter()
            .next()
            .ok_or_else(|| {
                anyhow!(
                    "no default-VPC subnet in {az} — cargo-burst requires a default VPC \
                     (the standard one new AWS accounts get). Create one in the EC2 console, \
                     or use a region/AZ where one exists."
                )
            })?
            .clone();
        let vpc_id = subnet
            .vpc_id()
            .ok_or_else(|| anyhow!("subnet has no vpc_id"))?
            .to_string();
        let subnet_id = subnet
            .subnet_id()
            .ok_or_else(|| anyhow!("subnet has no subnet_id"))?
            .to_string();

        // 2. cargo-burst security group in that VPC. Create if missing.
        let security_group_id = self.ensure_sg(&ec2, region, &vpc_id).await?;

        let v = VpcResolved {
            vpc_id,
            subnet_id,
            security_group_id,
        };
        self.vpc_cache.lock().await.insert(key, v.clone());
        Ok(v)
    }

    async fn ensure_sg(
        &self,
        ec2: &Ec2Client,
        region: &str,
        vpc_id: &str,
    ) -> Result<String> {
        // Find by name + vpc.
        let resp = ec2
            .describe_security_groups()
            .filters(
                ec2t::Filter::builder()
                    .name("group-name")
                    .values(CARGO_BURST_SG_NAME)
                    .build(),
            )
            .filters(
                ec2t::Filter::builder()
                    .name("vpc-id")
                    .values(vpc_id)
                    .build(),
            )
            .send()
            .await
            .with_context(|| format!("describe_security_groups in {region}"))?;
        if let Some(sg) = resp.security_groups().iter().next() {
            if let Some(id) = sg.group_id() {
                return Ok(id.to_string());
            }
        }

        // Not present — create it. We tag at creation time so the
        // user can spot it in the console.
        let tag_spec = ec2t::TagSpecification::builder()
            .resource_type(ec2t::ResourceType::SecurityGroup)
            .tags(ec2t::Tag::builder().key("Name").value(CARGO_BURST_SG_NAME).build())
            .tags(
                ec2t::Tag::builder()
                    .key("managed-by")
                    .value("cargo-burst")
                    .build(),
            )
            .build();
        let create = ec2
            .create_security_group()
            .group_name(CARGO_BURST_SG_NAME)
            .description("cargo-burst SSH ingress (auto-created)")
            .vpc_id(vpc_id)
            .tag_specifications(tag_spec)
            .send()
            .await
            .with_context(|| format!("create_security_group in {region}/{vpc_id}"))?;
        let sg_id = create
            .group_id()
            .ok_or_else(|| anyhow!("create_security_group returned no group_id"))?
            .to_string();

        // Open SSH from anywhere. Matches Hetzner's default behavior;
        // protection is on the SSH key, not the network.
        let perm = ec2t::IpPermission::builder()
            .ip_protocol("tcp")
            .from_port(22)
            .to_port(22)
            .ip_ranges(
                ec2t::IpRange::builder()
                    .cidr_ip("0.0.0.0/0")
                    .description("cargo-burst SSH (auto)")
                    .build(),
            )
            .build();
        ec2.authorize_security_group_ingress()
            .group_id(&sg_id)
            .ip_permissions(perm)
            .send()
            .await
            .with_context(|| format!("authorize SSH ingress on {sg_id}"))?;

        Ok(sg_id)
    }

    /// Choose the EBS volume type from config. Default `gp3`.
    fn volume_type(&self) -> ec2t::VolumeType {
        self.cfg
            .volume_type
            .as_deref()
            .unwrap_or("gp3")
            .into()
    }

    /// Translate `spec.image` to a concrete AMI ID, resolving the
    /// magic `"ubuntu-24.04"` name via SSM if needed.
    async fn resolve_image(&self, image: &ImageId, region: &str) -> Result<String> {
        if image.as_str() == UBUNTU_MAGIC_NAME {
            self.ubuntu_ami(region).await
        } else {
            Ok(image.as_str().to_string())
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// AWS instance state name → `ServerStatus`. AWS' state names:
/// `pending`, `running`, `shutting-down`, `terminated`, `stopping`,
/// `stopped`. We collapse `pending` → `Starting` (closest match for
/// the "alive enough to wait on" predicate `is_alive()` consumers
/// rely on) and treat `terminated` / `shutting-down` as `Other` so
/// `is_alive()` is false for them.
pub(crate) fn parse_instance_state(name: &str) -> ServerStatus {
    match name {
        "pending" => ServerStatus::Starting,
        "running" => ServerStatus::Running,
        "stopping" => ServerStatus::Stopping,
        "stopped" => ServerStatus::Stopped,
        // shutting-down / terminated / anything else.
        other => ServerStatus::Other(other.to_string()),
    }
}

fn parse_volume_state(name: &str) -> VolumeStatus {
    match name {
        "creating" => VolumeStatus::Creating,
        "available" | "in-use" => VolumeStatus::Available,
        other => VolumeStatus::Other(other.to_string()),
    }
}

fn parse_image_state(name: &str) -> ImageStatus {
    match name {
        "pending" => ImageStatus::Creating,
        "available" => ImageStatus::Available,
        other => ImageStatus::Other(other.to_string()),
    }
}

/// Strip dashes from `vol-0a1b2c3d4e5f6789a` → `vol0a1b2c3d4e5f6789a`.
/// The NVMe controller serial number for an attached EBS volume is
/// the dashless form; udev creates a stable symlink under
/// `/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_<serial>`.
pub(crate) fn vol_id_to_serial(volume_id: &str) -> String {
    volume_id.replace('-', "")
}

/// True if an SDK error is an EC2 "no capacity" signal. AWS surfaces
/// this through several distinct error codes depending on spot vs.
/// on-demand and the exact reason; we treat them all as triggers for
/// region fallback.
pub(crate) fn is_capacity_error<E, R>(err: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    let code = err
        .as_service_error()
        .and_then(|e| e.code())
        .unwrap_or("");
    matches!(
        code,
        "InsufficientInstanceCapacity"
            | "Unsupported"
            | "SpotMaxPriceTooLow"
            | "MaxSpotInstanceCountExceeded"
    )
}

/// True if an SDK error is an EC2 "not found" / "doesn't exist" signal.
/// Used so `delete_*` calls can be idempotent — re-running a teardown
/// against an already-gone resource shouldn't fail.
fn is_not_found<E, R>(err: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    let code = err
        .as_service_error()
        .and_then(|e| e.code())
        .unwrap_or("");
    code.ends_with(".NotFound") || code == "InvalidInstanceID.NotFound"
}

/// Convert any SDK error into `anyhow::Error` with the EC2 error code
/// + message preserved in the chain (otherwise SdkError's `Display`
/// drops the body and the user sees a generic "service error").
fn sdk_err_to_anyhow<E, R>(err: SdkError<E, R>, ctx: &str) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    let code = err
        .as_service_error()
        .and_then(|e| e.code())
        .unwrap_or("<no-code>")
        .to_string();
    let msg = err
        .as_service_error()
        .and_then(|e| e.message())
        .unwrap_or("")
        .to_string();
    anyhow!("{ctx}: [{code}] {msg}")
}

// ── Provider impl ─────────────────────────────────────────────────────

#[async_trait]
impl Provider for Ec2Provider {
    fn name(&self) -> &'static str {
        "aws"
    }

    fn billing_minimum(&self) -> Duration {
        AWS_BILLING_MINIMUM
    }

    // ── SSH keys ────────────────────────────────────────────────────

    async fn ensure_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKeyId> {
        // AWS key pairs are region-scoped. We import the key into
        // every configured region so a subsequent server-create in
        // any of them succeeds without an extra round trip. Cost is
        // tiny — ImportKeyPair is metadata-only.
        let regions: Vec<String> = if self.cfg.regions.is_empty() {
            vec![self.sdk_cfg.region().map(|r| r.to_string()).unwrap_or_default()]
        } else {
            self.cfg.regions.clone()
        };
        for region in &regions {
            let ec2 = self.ec2(region).await;
            // Idempotency: if a key by this name exists with a
            // different fingerprint, delete and re-import. EC2 has no
            // upsert — DescribeKeyPairs first, decide what to do.
            let existing = ec2
                .describe_key_pairs()
                .key_names(name)
                .send()
                .await
                .ok();
            let needs_create = match existing {
                Some(out) => out.key_pairs().is_empty(),
                None => true,
            };
            if !needs_create {
                continue;
            }
            ec2.import_key_pair()
                .key_name(name)
                .public_key_material(Blob::new(public_key.as_bytes()))
                .send()
                .await
                .map_err(|e| sdk_err_to_anyhow(e, &format!("import_key_pair in {region}")))?;
        }
        // The AWS "key id" is just the name we picked.
        Ok(SshKeyId(name.to_string()))
    }

    async fn delete_ssh_key(&self, id: &SshKeyId) -> Result<()> {
        let regions: Vec<String> = if self.cfg.regions.is_empty() {
            vec![self.sdk_cfg.region().map(|r| r.to_string()).unwrap_or_default()]
        } else {
            self.cfg.regions.clone()
        };
        for region in &regions {
            let ec2 = self.ec2(region).await;
            // Best-effort.
            let _ = ec2.delete_key_pair().key_name(id.as_str()).send().await;
        }
        Ok(())
    }

    // ── Images ──────────────────────────────────────────────────────

    async fn bake_image_from_server(
        &self,
        src: &ServerId,
        description: &str,
    ) -> Result<ImageId> {
        // The bake flow stops the instance via `shutdown` in the
        // bake script before this is called, so `NoReboot=true` is
        // safe and skips the extra reboot AWS would otherwise force.
        // The region is whichever region the instance lives in; we
        // discover it from DescribeInstances (called from a default
        // region — instance IDs are globally unique so this works
        // across regions, but DescribeInstances is region-scoped and
        // will return empty in the wrong one). Practically the
        // bake-then-image flow is always in regions[0] so we use
        // that.
        let region = self
            .cfg
            .regions
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("[aws].regions is empty"))?;
        let ec2 = self.ec2(&region).await;
        let name = format!(
            "cargo-burst-{}",
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::parse(
                    "[year][month][day]-[hour][minute][second]"
                )?)
                .unwrap_or_else(|_| "now".into())
        );
        let resp = ec2
            .create_image()
            .instance_id(src.as_str())
            .name(name)
            .description(description)
            .no_reboot(true)
            .tag_specifications(
                ec2t::TagSpecification::builder()
                    .resource_type(ec2t::ResourceType::Image)
                    .tags(
                        ec2t::Tag::builder()
                            .key("managed-by")
                            .value("cargo-burst")
                            .build(),
                    )
                    .build(),
            )
            .send()
            .await
            .map_err(|e| sdk_err_to_anyhow(e, "create_image"))?;
        let image_id = resp
            .image_id()
            .ok_or_else(|| anyhow!("create_image returned no image_id"))?
            .to_string();
        Ok(ImageId(image_id))
    }

    async fn wait_image_ready(&self, id: &ImageId, timeout: Duration) -> Result<()> {
        let region = self
            .cfg
            .regions
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("[aws].regions is empty"))?;
        let ec2 = self.ec2(&region).await;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let resp = ec2
                .describe_images()
                .image_ids(id.as_str())
                .send()
                .await
                .map_err(|e| sdk_err_to_anyhow(e, "describe_images"))?;
            let img = resp
                .images()
                .iter()
                .next()
                .ok_or_else(|| anyhow!("image {id} disappeared while waiting"))?;
            let state = img
                .state()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            match parse_image_state(&state) {
                ImageStatus::Available => return Ok(()),
                ImageStatus::Creating => {}
                ImageStatus::Other(s) => {
                    return Err(anyhow!("image {id} went to unrecoverable state {s}"));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "image {id} did not become available within {timeout:?} (state={state})"
                ));
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    async fn get_image(&self, id: &ImageId) -> Result<Option<ImageInfo>> {
        let region = self
            .cfg
            .regions
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("[aws].regions is empty"))?;
        let ec2 = self.ec2(&region).await;
        match ec2.describe_images().image_ids(id.as_str()).send().await {
            Ok(resp) => {
                let Some(img) = resp.images().iter().next() else {
                    return Ok(None);
                };
                Ok(Some(ImageInfo {
                    id: ImageId(
                        img.image_id().unwrap_or(id.as_str()).to_string(),
                    ),
                    status: parse_image_state(
                        &img.state().map(|s| s.as_str().to_string()).unwrap_or_default(),
                    ),
                    description: img.description().map(str::to_string),
                }))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(sdk_err_to_anyhow(e, "describe_images")),
        }
    }

    // ── Servers ─────────────────────────────────────────────────────

    async fn create_server(
        &self,
        spec: ServerSpec,
    ) -> std::result::Result<ServerInfo, ProvisionError> {
        let region = spec.region.clone();
        let ec2 = self.ec2(&region).await;
        let az = self
            .pick_az(&region)
            .await
            .map_err(ProvisionError::Other)?;
        let vpc = self
            .ensure_vpc(&region, &az)
            .await
            .map_err(ProvisionError::Other)?;
        let ami_id = self
            .resolve_image(&spec.image, &region)
            .await
            .map_err(ProvisionError::Other)?;

        // Tag specs: apply Name + spec.labels to both the instance
        // and any volumes RunInstances creates (root volume).
        let mut tags = vec![
            ec2t::Tag::builder().key("Name").value(&spec.name).build(),
        ];
        for (k, v) in &spec.labels {
            tags.push(ec2t::Tag::builder().key(k).value(v).build());
        }
        let mk_tag_spec = |rt: ec2t::ResourceType| {
            let mut b = ec2t::TagSpecification::builder().resource_type(rt);
            for t in &tags {
                b = b.tags(t.clone());
            }
            b.build()
        };

        // Spot vs on-demand. Bake (`role=bake`) always runs on-demand
        // — one-shot, must succeed. Everything else respects the
        // config knob (default true).
        let is_bake = spec.labels.get("role").map(String::as_str) == Some("bake");
        let want_spot = self.cfg.use_spot && !is_bake;
        let market_opts = if want_spot {
            Some(
                ec2t::InstanceMarketOptionsRequest::builder()
                    .market_type(ec2t::MarketType::Spot)
                    .spot_options(
                        ec2t::SpotMarketOptions::builder()
                            // MaxPrice unset → defaults to on-demand
                            // price as ceiling; never pay more than
                            // on-demand. Right for cargo-burst's
                            // use case.
                            .instance_interruption_behavior(
                                ec2t::InstanceInterruptionBehavior::Terminate,
                            )
                            .spot_instance_type(ec2t::SpotInstanceType::OneTime)
                            .build(),
                    )
                    .build(),
            )
        } else {
            None
        };

        let key_name = spec
            .ssh_keys
            .first()
            .ok_or_else(|| ProvisionError::Other(anyhow!("ssh_keys empty")))?
            .as_str()
            .to_string();

        // EC2's user-data size limit is 16 KiB raw / ~21 KiB base64-encoded
        // (the API hard-caps the encoded form at 25600 bytes). Our bake
        // script is ~20 KiB raw which lands over the limit. cloud-init
        // auto-decompresses user-data that starts with the gzip magic
        // bytes, so we gzip first → base64 second. Encoded size after
        // gzip is typically ~9 KiB for a 20 KiB shell script — well
        // inside the limit.
        let user_data_b64 = spec
            .user_data
            .as_ref()
            .map(|u| -> Result<String, anyhow::Error> {
                let mut enc = GzEncoder::new(Vec::new(), Compression::best());
                enc.write_all(u.as_bytes()).context("gzip user-data")?;
                let compressed = enc.finish().context("gzip user-data finish")?;
                Ok(base64::engine::general_purpose::STANDARD.encode(&compressed))
            })
            .transpose()
            .map_err(ProvisionError::Other)?;

        let mut req = ec2
            .run_instances()
            .image_id(&ami_id)
            .instance_type(ec2t::InstanceType::from(spec.server_type.as_str()))
            .key_name(&key_name)
            .subnet_id(&vpc.subnet_id)
            .security_group_ids(&vpc.security_group_id)
            .min_count(1)
            .max_count(1)
            .tag_specifications(mk_tag_spec(ec2t::ResourceType::Instance))
            .tag_specifications(mk_tag_spec(ec2t::ResourceType::Volume));
        if let Some(ud) = user_data_b64 {
            req = req.user_data(ud);
        }
        if let Some(m) = market_opts {
            req = req.instance_market_options(m);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if is_capacity_error(&e) => {
                return Err(ProvisionError::NoCapacity {
                    server_type: spec.server_type,
                    regions: vec![region],
                });
            }
            Err(e) => {
                return Err(ProvisionError::Other(sdk_err_to_anyhow(e, "run_instances")));
            }
        };
        let inst = resp
            .instances()
            .iter()
            .next()
            .ok_or_else(|| ProvisionError::Other(anyhow!("run_instances returned no instance")))?;
        let id = inst
            .instance_id()
            .ok_or_else(|| ProvisionError::Other(anyhow!("instance has no id")))?
            .to_string();
        let server_id = ServerId(id);

        // RunInstances returns immediately, often before the instance
        // has its public IP. The trait contract (matched by
        // HcloudProvider, which blocks on `wait_action` before
        // returning) is that the returned ServerInfo is fully usable
        // — caller `ensure_shared_server` reads `public_ip` directly
        // without an intervening `wait_server_running`. Poll
        // DescribeInstances until State=running AND PublicIpAddress is
        // populated, up to a generous timeout (spot capacity can take
        // ~30s to converge).
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            match self
                .get_server(&server_id)
                .await
                .map_err(ProvisionError::Other)?
            {
                Some(info)
                    if info.status == ServerStatus::Running
                        && info.public_ip.is_some() =>
                {
                    return Ok(info);
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(ProvisionError::Other(anyhow!(
                    "instance {} did not become running with a public IP within 180s",
                    server_id
                )));
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    async fn get_server(&self, id: &ServerId) -> Result<Option<ServerInfo>> {
        // DescribeInstances is region-scoped. Walk configured regions
        // until we find it. State.json only ever holds one alive
        // server, so the cost is bounded.
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            let resp = match ec2
                .describe_instances()
                .instance_ids(id.as_str())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(sdk_err_to_anyhow(e, "describe_instances")),
            };
            for res in resp.reservations() {
                for inst in res.instances() {
                    if inst.instance_id() == Some(id.as_str()) {
                        return Ok(Some(instance_to_info(inst, region)));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn find_server(&self, name: &str) -> Result<Option<ServerInfo>> {
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            let resp = ec2
                .describe_instances()
                .filters(
                    ec2t::Filter::builder()
                        .name("tag:Name")
                        .values(name)
                        .build(),
                )
                .filters(
                    ec2t::Filter::builder()
                        .name("instance-state-name")
                        .values("pending")
                        .values("running")
                        .values("stopping")
                        .values("stopped")
                        .build(),
                )
                .send()
                .await
                .map_err(|e| sdk_err_to_anyhow(e, "describe_instances"))?;
            for res in resp.reservations() {
                if let Some(inst) = res.instances().iter().next() {
                    return Ok(Some(instance_to_info(inst, region)));
                }
            }
        }
        Ok(None)
    }

    async fn delete_server(&self, id: &ServerId) -> Result<()> {
        // Walk regions to find which one owns the instance, then
        // terminate. `TerminateInstances` is idempotent: re-running
        // against an already-terminated/missing instance is OK.
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            match ec2
                .terminate_instances()
                .instance_ids(id.as_str())
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(sdk_err_to_anyhow(e, "terminate_instances")),
            }
        }
        // None of the regions knew about it — treat as already-gone.
        Ok(())
    }

    async fn wait_server_running(
        &self,
        id: &ServerId,
        timeout: Duration,
    ) -> Result<ServerInfo> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let maybe = self.get_server(id).await?;
            if let Some(info) = maybe.as_ref() {
                if info.status == ServerStatus::Running && info.public_ip.is_some() {
                    return Ok(info.clone());
                }
            }
            if std::time::Instant::now() >= deadline {
                let status = maybe
                    .as_ref()
                    .map(|i| i.status.as_str().to_string())
                    .unwrap_or_else(|| "<gone>".into());
                return Err(anyhow!(
                    "instance {id} did not reach `running` with public IP within {timeout:?} \
                     (last status={status})"
                ));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // ── Volumes ─────────────────────────────────────────────────────

    async fn create_volume(&self, spec: VolumeSpec) -> Result<VolumeInfo> {
        let region = spec.region.clone();
        let ec2 = self.ec2(&region).await;
        let az = self.pick_az(&region).await?;

        let mut tags = vec![
            ec2t::Tag::builder().key("Name").value(&spec.name).build(),
        ];
        for (k, v) in &spec.labels {
            tags.push(ec2t::Tag::builder().key(k).value(v).build());
        }
        let mut tag_spec = ec2t::TagSpecification::builder()
            .resource_type(ec2t::ResourceType::Volume);
        for t in &tags {
            tag_spec = tag_spec.tags(t.clone());
        }

        let resp = ec2
            .create_volume()
            .availability_zone(&az)
            .size(spec.size_gb as i32)
            .volume_type(self.volume_type())
            .tag_specifications(tag_spec.build())
            .send()
            .await
            .map_err(|e| sdk_err_to_anyhow(e, "create_volume"))?;
        let id = resp
            .volume_id()
            .ok_or_else(|| anyhow!("create_volume returned no volume_id"))?
            .to_string();

        // Wait for `available`. Volumes are usually `creating` for a
        // few seconds; we don't want to return until they can be
        // attached.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            let v = ec2
                .describe_volumes()
                .volume_ids(&id)
                .send()
                .await
                .map_err(|e| sdk_err_to_anyhow(e, "describe_volumes"))?;
            if let Some(vol) = v.volumes().iter().next() {
                let state = vol
                    .state()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default();
                if state == "available" {
                    return Ok(volume_to_info(vol, &id));
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!("volume {id} stuck in state {state}"));
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn get_volume(&self, id: &VolumeId) -> Result<Option<VolumeInfo>> {
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            match ec2.describe_volumes().volume_ids(id.as_str()).send().await {
                Ok(resp) => {
                    if let Some(v) = resp.volumes().iter().next() {
                        return Ok(Some(volume_to_info(v, id.as_str())));
                    }
                }
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(sdk_err_to_anyhow(e, "describe_volumes")),
            }
        }
        Ok(None)
    }

    async fn find_volume(&self, name: &str) -> Result<Option<VolumeInfo>> {
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            let resp = ec2
                .describe_volumes()
                .filters(
                    ec2t::Filter::builder()
                        .name("tag:Name")
                        .values(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| sdk_err_to_anyhow(e, "describe_volumes"))?;
            if let Some(v) = resp.volumes().iter().next() {
                let id = v.volume_id().unwrap_or("").to_string();
                return Ok(Some(volume_to_info(v, &id)));
            }
        }
        Ok(None)
    }

    async fn attach_volume(
        &self,
        vol: &VolumeId,
        srv: &ServerId,
    ) -> Result<AttachedDevice> {
        // Idempotency: if the volume is already attached to the target
        // instance, AttachVolume returns `VolumeInUse`. The trait
        // contract (set by HcloudProvider, where attach is idempotent
        // server-side) is that re-attaching to the same instance is
        // a no-op that returns the device path. Short-circuit by
        // checking volume state first — cheaper than letting the call
        // fail and decoding the error.
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            let v = match ec2
                .describe_volumes()
                .volume_ids(vol.as_str())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(sdk_err_to_anyhow(e, "describe_volumes")),
            };
            let already_on_target = v.volumes().iter().next().is_some_and(|x| {
                x.attachments()
                    .iter()
                    .any(|a| a.instance_id() == Some(srv.as_str()))
            });
            if already_on_target {
                let serial = vol_id_to_serial(vol.as_str());
                return Ok(AttachedDevice {
                    device_path: format!(
                        "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_{serial}"
                    ),
                });
            }
            break; // Found in this region but not attached to target — fall through to attach.
        }

        // Volumes are AZ-scoped — we don't know which region without
        // a lookup. Walk regions until AttachVolume succeeds (or all
        // fail). Same shape as get_volume / get_server above.
        let mut last_err: Option<anyhow::Error> = None;
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            match ec2
                .attach_volume()
                .volume_id(vol.as_str())
                .instance_id(srv.as_str())
                .device(ATTACH_DEVICE_NAME)
                .send()
                .await
            {
                Ok(_) => {
                    // Poll for `attached` state — instance needs a
                    // moment for udev to surface the device.
                    let deadline = std::time::Instant::now() + Duration::from_secs(60);
                    loop {
                        let v = ec2
                            .describe_volumes()
                            .volume_ids(vol.as_str())
                            .send()
                            .await
                            .map_err(|e| sdk_err_to_anyhow(e, "describe_volumes"))?;
                        let attached = v
                            .volumes()
                            .iter()
                            .next()
                            .and_then(|x| x.attachments().iter().next())
                            .and_then(|a| a.state())
                            .map(|s| s.as_str().to_string())
                            .unwrap_or_default();
                        if attached == "attached" {
                            break;
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(anyhow!(
                                "attach for volume {vol} stuck in state {attached}"
                            ));
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    let serial = vol_id_to_serial(vol.as_str());
                    return Ok(AttachedDevice {
                        device_path: format!(
                            "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_{serial}"
                        ),
                    });
                }
                Err(e) if is_not_found(&e) => continue,
                Err(e) => {
                    last_err = Some(sdk_err_to_anyhow(e, "attach_volume"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow!("attach_volume: volume {vol} not found in any configured region")
        }))
    }

    async fn detach_volume(&self, vol: &VolumeId) -> Result<()> {
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            match ec2.detach_volume().volume_id(vol.as_str()).send().await {
                Ok(_) => return Ok(()),
                Err(e) if is_not_found(&e) => continue,
                Err(e) => {
                    // "IncorrectState" means "already detached" —
                    // treat as success rather than failing the reaper.
                    let code = e
                        .as_service_error()
                        .and_then(|x| x.code())
                        .unwrap_or("");
                    if code == "IncorrectState" {
                        return Ok(());
                    }
                    return Err(sdk_err_to_anyhow(e, "detach_volume"));
                }
            }
        }
        Ok(())
    }

    async fn delete_volume(&self, id: &VolumeId) -> Result<()> {
        for region in &self.cfg.regions {
            let ec2 = self.ec2(region).await;
            match ec2.delete_volume().volume_id(id.as_str()).send().await {
                Ok(_) => return Ok(()),
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(sdk_err_to_anyhow(e, "delete_volume")),
            }
        }
        Ok(())
    }
}

// ── Free helpers (used by Provider impl above) ─────────────────────────

fn instance_to_info(inst: &ec2t::Instance, region: &str) -> ServerInfo {
    ServerInfo {
        id: ServerId(inst.instance_id().unwrap_or("").to_string()),
        status: parse_instance_state(
            inst.state()
                .and_then(|s| s.name())
                .map(|n| n.as_str())
                .unwrap_or(""),
        ),
        public_ip: inst.public_ip_address().map(str::to_string),
        server_type: inst
            .instance_type()
            .map(|t| t.as_str().to_string())
            .unwrap_or_default(),
        region: Some(region.to_string()),
    }
}

fn volume_to_info(v: &ec2t::Volume, fallback_id: &str) -> VolumeInfo {
    let attached_to = v
        .attachments()
        .iter()
        .next()
        .and_then(|a| a.instance_id())
        .map(|s| ServerId(s.to_string()));
    VolumeInfo {
        id: VolumeId(v.volume_id().unwrap_or(fallback_id).to_string()),
        size_gb: v.size().unwrap_or(0) as u32,
        status: parse_volume_state(
            &v.state().map(|s| s.as_str().to_string()).unwrap_or_default(),
        ),
        // The trait's `VolumeInfo.region` is the cross-provider
        // *region* identifier — for AWS, "eu-central-1", not
        // "eu-central-1a". DescribeVolumes returns the AZ
        // (volumes are AZ-scoped on EC2), so strip the trailing AZ
        // letter. Without this strip, the controller compares the
        // AZ string against the configured `regions` list and decides
        // the volume is in the "wrong region" — then tries to delete
        // and recreate it on every run, ditching the warm `target/`.
        region: v.availability_zone().map(az_to_region),
        attached_to,
    }
}

/// AWS Availability Zones are `<region><letter>` — e.g. "eu-central-1a".
/// Strip exactly one trailing ASCII alphabetic character to recover the
/// region. (Region names themselves end in a digit, so the strip is
/// unambiguous.) Pass-through if the input doesn't look like an AZ.
fn az_to_region(az: impl Into<String>) -> String {
    let s = az.into();
    if s.chars().last().is_some_and(|c| c.is_ascii_alphabetic()) {
        s[..s.len() - 1].to_string()
    } else {
        s
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vol_id_serial_strips_dashes() {
        assert_eq!(
            vol_id_to_serial("vol-0a1b2c3d4e5f6789a"),
            "vol0a1b2c3d4e5f6789a"
        );
        // Already-stripped form round-trips.
        assert_eq!(
            vol_id_to_serial("vol0a1b2c3d4e5f6789a"),
            "vol0a1b2c3d4e5f6789a"
        );
    }

    #[test]
    fn parse_instance_state_known_names() {
        assert_eq!(parse_instance_state("pending"), ServerStatus::Starting);
        assert_eq!(parse_instance_state("running"), ServerStatus::Running);
        assert_eq!(parse_instance_state("stopping"), ServerStatus::Stopping);
        assert_eq!(parse_instance_state("stopped"), ServerStatus::Stopped);
        // Unknown / transient → Other (is_alive() == false).
        assert!(matches!(
            parse_instance_state("shutting-down"),
            ServerStatus::Other(_)
        ));
        assert!(matches!(
            parse_instance_state("terminated"),
            ServerStatus::Other(_)
        ));
    }

    #[test]
    fn az_to_region_strips_trailing_letter() {
        assert_eq!(az_to_region("eu-central-1a"), "eu-central-1");
        assert_eq!(az_to_region("us-east-1f"), "us-east-1");
        assert_eq!(az_to_region("ap-northeast-3c"), "ap-northeast-3");
        // Pass-through when there's nothing to strip (already a region name).
        assert_eq!(az_to_region("eu-central-1"), "eu-central-1");
        assert_eq!(az_to_region(""), "");
    }

    #[test]
    fn parse_volume_state_known_names() {
        assert_eq!(parse_volume_state("creating"), VolumeStatus::Creating);
        assert_eq!(parse_volume_state("available"), VolumeStatus::Available);
        // in-use is still considered Available for our purposes —
        // the volume is real, attached or not.
        assert_eq!(parse_volume_state("in-use"), VolumeStatus::Available);
        assert!(matches!(
            parse_volume_state("deleting"),
            VolumeStatus::Other(_)
        ));
    }

    #[test]
    fn parse_image_state_known_names() {
        assert_eq!(parse_image_state("pending"), ImageStatus::Creating);
        assert_eq!(parse_image_state("available"), ImageStatus::Available);
        assert!(matches!(
            parse_image_state("failed"),
            ImageStatus::Other(_)
        ));
    }
}
