//! `cargo burst down` — delete the shared server now (bypassing the
//! reaper's keep-alive timer). Project volumes are preserved by
//! default; `--with-volumes` deletes those too for a clean slate.

use anyhow::Result;
use clap::Args;

use crate::config::{self, Config, State};
use crate::hcloud::HCloud;

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
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    // ── Server ────────────────────────────────────────────────────
    let server_deleted = if let Some(id) = state.server_id {
        println!("Deleting shared server {id}…");
        hcloud.delete_server(id).await?;
        // Re-read state under the lock and clear server_id. Only
        // clear if it still points at the server we just deleted —
        // a concurrent build may have re-provisioned in the gap, in
        // which case we should leave the new server alone. Also
        // close out the audit-log session under the same lock so
        // the lifetime/command-count is recorded with reason=down.
        config::update_state(|s| {
            if s.server_id == Some(id) {
                s.server_id = None;
            }
            crate::audit::end_server_session(
                s,
                id,
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
    // It's fine to use stale state here — even if a build provisions
    // a new volume in parallel, our delete loop only acts on the IDs
    // we observed, and the per-project state cleanup re-reads under
    // the lock and only clears entries that still match.
    let volumes: Vec<(String, i64)> = state
        .projects
        .iter()
        .filter_map(|(hash, p)| p.volume_id.map(|id| (hash.clone(), id)))
        .collect();

    if volumes.is_empty() {
        println!("No project volumes to delete.");
        return Ok(());
    }

    println!("Deleting {} project volume(s)…", volumes.len());
    for (hash, vol_id) in &volumes {
        println!("  - volume {vol_id} (project {hash})");
    }

    // Detach + delete each volume. The detach call is idempotent
    // enough that we ignore failures and try the delete anyway —
    // when the server got deleted just above, Hetzner auto-detaches
    // its attached volumes, so detach here will commonly no-op.
    // delete_volume is the operation that actually has to succeed.
    let mut delete_failures: Vec<(i64, String)> = Vec::new();
    for (_hash, vol_id) in &volumes {
        if let Err(e) = hcloud.detach_volume(*vol_id).await {
            tracing::debug!(
                volume = vol_id,
                error = %e,
                "detach failed (likely already detached after server delete); continuing"
            );
        }
        if let Err(e) = hcloud.delete_volume(*vol_id).await {
            tracing::error!(volume = vol_id, error = %e, "delete failed");
            delete_failures.push((*vol_id, e.to_string()));
        }
    }

    // Single locked RMW: clear volume_id for every project whose
    // recorded volume we just deleted, AND emit one audit
    // termination event per success. We re-load state under the
    // lock so a concurrent build that re-provisioned a different
    // volume on a project doesn't get its new volume_id stomped.
    let deleted_set: std::collections::HashSet<i64> = volumes
        .iter()
        .map(|(_, id)| *id)
        .filter(|id| !delete_failures.iter().any(|(fid, _)| fid == id))
        .collect();
    config::update_state(|s| {
        for (hash, _) in &volumes {
            let Some(p) = s.projects.get_mut(hash) else { continue };
            let Some(recorded) = p.volume_id else { continue };
            if deleted_set.contains(&recorded) {
                p.volume_id = None;
                crate::audit::end_volume_session(
                    p,
                    hash,
                    recorded,
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
