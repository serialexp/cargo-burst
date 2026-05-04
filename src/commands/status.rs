//! `cargo burst status` — show what cargo-burst currently has provisioned
//! on Hetzner: image, per-project volumes, currently-alive servers.

use anyhow::Result;

use crate::config::{Config, State};
use crate::hcloud::HCloud;

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    let state = State::load()?;
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    println!("region:       {}", cfg.region);
    println!("server type:  {}", cfg.server_type);
    println!();

    match state.image_id {
        Some(id) => match hcloud.get_image(id).await {
            Ok(img) => println!(
                "image:        {} ({})  desc={}",
                id,
                img.status,
                img.description.as_deref().unwrap_or("-")
            ),
            Err(e) => println!("image:        {id} (lookup failed: {e})"),
        },
        None => println!("image:        none — run `cargo burst image build`"),
    }
    println!();

    // Shared server (one across all projects).
    match state.server_id {
        Some(id) => match hcloud.get_server(id).await {
            Ok(s) => println!(
                "server:       {id}  type={}  status={}  ip={}",
                s.server_type.name,
                s.status,
                s.public_net.ipv4.as_ref().map(|i| i.ip.as_str()).unwrap_or("-")
            ),
            Err(_) => println!("server:       {id} (deleted upstream)"),
        },
        None => println!("server:       - (none provisioned)"),
    }
    if let Some(last) = state.last_used_any() {
        println!("last build:   {last}  (across all projects)");
    }
    println!();

    if state.projects.is_empty() {
        println!("(no projects registered yet)");
        return Ok(());
    }
    println!("Projects:");
    for (hash, p) in &state.projects {
        println!("  {hash}  {}", p.workspace_path);
        match p.volume_id {
            Some(id) => match hcloud.get_volume(id).await {
                Ok(v) => println!(
                    "    volume:  {id}  size={}GB  status={}  attached_to={:?}",
                    v.size, v.status, v.server
                ),
                Err(e) => println!("    volume:  {id}  (lookup failed: {e})"),
            },
            None => println!("    volume:  -  (reaped or not yet created)"),
        }
        match &p.last_used_rfc3339 {
            Some(last) => println!("    last:    {last}"),
            None => println!("    last:    never"),
        }
    }
    Ok(())
}
