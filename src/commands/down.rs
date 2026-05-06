//! `cargo burst down` — delete the shared server now (bypassing the
//! reaper's keep-alive timer). All project volumes are preserved.

use anyhow::Result;

use crate::config::{self, Config, State};
use crate::hcloud::HCloud;

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    // Read once outside the lock to find what to delete; the actual state
    // write happens under the lock below so we don't race a concurrent
    // build that's mid-provision.
    let state = State::load()?;
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    let id = match state.server_id {
        Some(id) => id,
        None => {
            println!("No server currently provisioned.");
            return Ok(());
        }
    };

    println!("Deleting shared server {id}…");
    hcloud.delete_server(id).await?;
    // Re-read state under the lock and clear server_id. Only clear if it
    // still points at the server we just deleted — a concurrent build may
    // have re-provisioned in the gap, in which case we should leave the
    // new server alone. Also close out the audit-log session under
    // the same lock so the lifetime/command-count is recorded with
    // reason=down.
    config::update_state(|s| {
        if s.server_id == Some(id) {
            s.server_id = None;
        }
        crate::audit::end_server_session(s, id, crate::audit::TerminationReason::Down);
        Ok(())
    })
    .await?;
    println!("✓ Server {id} deleted (all project volumes preserved).");
    Ok(())
}
