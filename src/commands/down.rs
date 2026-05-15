//! `cargo burst down` — delete the shared server now (bypassing the
//! reaper's keep-alive timer). Project volumes are preserved by
//! default; `--with-volumes` deletes those too for a clean slate.

use anyhow::Result;
use clap::Args;

use crate::config::{self, Config, State};
use crate::provider::{self, VolumeId};

#[derive(Args, Debug)]
pub struct DownArgs {
    /// Also delete every project's volume. Without this flag, volumes
    /// are preserved and the per-project volume reaper will eventually
    /// reap them after `volume_keep_alive_secs` of inactivity.
    ///
    /// Use this when you want a clean slate — e.g. before changing
    /// `volume_gb`, migrating to a different region preference, or
    /// just because you've moved on from a project and don't want to
    /// wait an hour for the reaper. Volumes are build caches; the
    /// next session will rebuild them from scratch (~30s on a CCX63).
    #[arg(long)]
    pub with_volumes: bool,
}

pub async fn run(args: DownArgs) -> Result<()> {
    let cfg = Config::load()?;
    // Read once outside the lock to find what to delete; the actual
    // state writes happen via update_state below so we don't race a
    // concurrent build that's mid-provision.
    let state = State::load()?;
    let provider = provider::from_config(&cfg).await?;

    // ── Server ────────────────────────────────────────────────────
    let server_deleted = if let Some(id) = state.server_id.clone() {
        println!("Deleting shared server {id}…");
        provider.delete_server(&id).await?;
        let id_for_state = id.clone();
        config::update_state(move |s| {
            if s.server_id.as_ref() == Some(&id_for_state) {
                s.server_id = None;
            }
            crate::audit::end_server_session(
                s,
                &id_for_state,
                crate::audit::TerminationReason::Down,
            );
            Ok(())
        })
        .await?;
        println!("✓ Server {id} deleted.");
        true
    } else {
        if !args.with_volumes {
            println!("No server currently provisioned.");
            return Ok(());
        }
        // No server, but the user asked for volumes too — proceed.
        println!("No server currently provisioned; proceeding with volume cleanup.");
        false
    };

    // ── Volumes (only with --with-volumes) ────────────────────────
    if !args.with_volumes {
        if server_deleted {
            println!("(Project volumes preserved. Run `cargo burst down --with-volumes` to delete those too.)");
        }
        return Ok(());
    }

    // Collect volume IDs to delete from the snapshot we read above.
    let volumes: Vec<(String, VolumeId)> = state
        .projects
        .iter()
        .filter_map(|(hash, p)| p.volume_id.clone().map(|id| (hash.clone(), id)))
        .collect();

    if volumes.is_empty() {
        println!("No project volumes to delete.");
        return Ok(());
    }

    println!("Deleting {} project volume(s)…", volumes.len());
    for (hash, vol_id) in &volumes {
        println!("  - volume {vol_id} (project {hash})");
    }

    let mut delete_failures: Vec<(VolumeId, String)> = Vec::new();
    for (_hash, vol_id) in &volumes {
        if let Err(e) = provider.detach_volume(vol_id).await {
            tracing::debug!(
                volume = %vol_id,
                error = %e,
                "detach failed (likely already detached after server delete); continuing"
            );
        }
        if let Err(e) = provider.delete_volume(vol_id).await {
            tracing::error!(volume = %vol_id, error = %e, "delete failed");
            delete_failures.push((vol_id.clone(), e.to_string()));
        }
    }

    let deleted_set: std::collections::HashSet<VolumeId> = volumes
        .iter()
        .map(|(_, id)| id.clone())
        .filter(|id| !delete_failures.iter().any(|(fid, _)| fid == id))
        .collect();
    let provider_name = provider.name().to_string();
    let volumes_for_closure: Vec<(String, VolumeId)> = volumes.clone();
    config::update_state(move |s| {
        for (hash, _) in &volumes_for_closure {
            let Some(p) = s.projects.get_mut(hash) else { continue };
            let Some(recorded) = p.volume_id.clone() else { continue };
            if deleted_set.contains(&recorded) {
                p.volume_id = None;
                crate::audit::end_volume_session(
                    p,
                    hash,
                    &recorded,
                    &provider_name,
                    crate::audit::TerminationReason::Down,
                );
            }
        }
        Ok(())
    })
    .await?;

    let succeeded = volumes.len() - delete_failures.len();
    println!("✓ Deleted {succeeded} volume(s).");
    if !delete_failures.is_empty() {
        println!(
            "✗ Failed to delete {} volume(s):",
            delete_failures.len()
        );
        for (id, err) in &delete_failures {
            println!("    {id}: {err}");
        }
        // Treat failures as a real error so scripts/CI notice. The
        // partial successes above already happened — they're not
        // rolled back, but the exit code reflects "this command
        // didn't fully complete".
        return Err(anyhow::anyhow!(
            "{} volume(s) failed to delete",
            delete_failures.len()
        ));
    }
    Ok(())
}
