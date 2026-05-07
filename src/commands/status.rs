//! `cargo burst status` — show what cargo-burst currently has provisioned
//! on Hetzner: image, per-project volumes, currently-alive servers.
//!
//! When a server is up and reachable we also probe the three database
//! services baked into the image (postgres / mysql / redis). It's a
//! ~1s round-trip and gives users early confirmation that integration
//! tests will have the DBs they expect once they start using them —
//! which matters because the help text on `test`/`bench` advertises
//! the connection strings, and "advertised but not actually up" is
//! exactly the failure mode worth surfacing here.

use anyhow::Result;

use crate::config::{Config, State};
use crate::hcloud::HCloud;
use crate::ssh;

/// Connection-string text we print alongside each DB's up/down state.
/// Kept in lockstep with the help text on `test`/`bench` and the
/// `Database services` table in README.md — if you change one, change
/// the others too.
const DB_SERVICES: &[(&str, &str)] = &[
    ("postgres", "postgres://postgres@localhost:5432/postgres  (no password)"),
    ("mysql", "mysql://root:root@localhost:3306/"),
    ("redis", "redis://localhost:6379/"),
];

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    let state = State::load()?;
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    println!("regions:      {}", cfg.region_preference().join(", "));
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
    let mut server_ip: Option<String> = None;
    match state.server_id {
        Some(id) => match hcloud.get_server(id).await {
            Ok(s) => {
                let ip = s.public_net.ipv4.as_ref().map(|i| i.ip.clone());
                println!(
                    "server:       {id}  type={}  status={}  ip={}",
                    s.server_type.name,
                    s.status,
                    ip.as_deref().unwrap_or("-")
                );
                if s.status == "running" {
                    server_ip = ip;
                }
            }
            Err(_) => println!("server:       {id} (deleted upstream)"),
        },
        None => println!("server:       - (none provisioned)"),
    }
    if let Some(last) = state.last_used_any() {
        println!("last build:   {last}  (across all projects)");
    }
    println!();

    // Database services. Always print the connection strings (so users
    // know what's on offer even when no server is up); when a server
    // *is* up, probe the ports and annotate each line with up/down.
    println!("databases:");
    let probe = if let Some(ip) = server_ip.as_deref() {
        match cfg.ssh_key_path() {
            Ok(key) => probe_db_ports(ip, &key).await,
            Err(_) => None,
        }
    } else {
        None
    };
    for (name, conn) in DB_SERVICES {
        let state_str = match probe.as_ref() {
            Some(states) => match states.iter().find(|(n, _)| n == name).map(|(_, up)| *up) {
                Some(true) => "up  ",
                Some(false) => "down",
                None => "?   ",
            },
            None => "-   ", // no running server to probe against
        };
        println!("  [{state_str}] {name:<8}  {conn}");
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

/// SSH to the running server and probe the three DB ports via bash's
/// built-in `/dev/tcp` (no need to install netcat). Returns a vector
/// of `(name, is_up)` pairs in the same order as [`DB_SERVICES`], or
/// `None` if the SSH probe itself failed (e.g. server is up in
/// Hetzner's view but sshd hasn't come back yet after a reboot).
///
/// The remote script intentionally emits a line per service even on
/// failure (`up` / `down`), and exits 0 unconditionally — otherwise
/// `capture_remote` would treat a single down service as a hard
/// error and we'd report nothing. We use `timeout 1` per port so a
/// hung service can't stretch the status command's wall time past
/// ~3 s in the worst case.
async fn probe_db_ports(host: &str, key_path: &std::path::Path) -> Option<Vec<(String, bool)>> {
    // Single-quote the bash heredoc so the local shell doesn't expand
    // anything. The remote shell does all the variable expansion.
    let script = r#"bash -c '
for spec in postgres:5432 mysql:3306 redis:6379; do
  name=${spec%%:*}
  port=${spec##*:}
  if timeout 1 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null; then
    echo "$name up"
  else
    echo "$name down"
  fi
done
'"#;
    let out = ssh::capture_remote(host, "work", key_path, script).await.ok()?;
    let mut results = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        results.push((name.to_string(), state == "up"));
    }
    if results.is_empty() { None } else { Some(results) }
}
