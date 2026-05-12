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
use crate::project::ProjectKey;
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
    // Detect the workspace (if any) so the printed config reflects
    // whatever `<workspace>/.config/cargo-burst.toml` overrides
    // — running `cargo burst status` from inside a project should
    // show the *effective* config for that project, not just the
    // global defaults. Outside any cargo workspace we silently fall
    // back to global-only.
    let cwd = std::env::current_dir()?;
    let workspace_root = ProjectKey::discover(&cwd)
        .ok()
        .map(|p| p.workspace_root);
    let cfg = Config::load_for_workspace(workspace_root.as_deref())?;
    let state = State::load()?;
    let hcloud = HCloud::new(cfg.hetzner_token.clone())?;

    if let Some(root) = workspace_root.as_deref() {
        let proj = crate::config::project_config_path(root);
        if proj.exists() {
            println!("project cfg:  {} (applied)", proj.display());
        }
    }
    println!("regions:      {}", cfg.region_preference().join(", "));
    println!("server type:  {}", cfg.server_type);
    if !cfg.forward_env.is_empty() {
        println!("forward_env:  {}", cfg.forward_env.join(", "));
    }
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
    let key_path = cfg.ssh_key_path().ok();
    let probe = if let (Some(ip), Some(key)) = (server_ip.as_deref(), key_path.as_deref()) {
        probe_db_ports(ip, key).await
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

    // Top processes — what's actually eating CPU on the remote right
    // now. The "agent runs `cargo burst status` to see why a build
    // feels slow" use case needs this concretely: with PIDs in hand,
    // a user can `cargo burst down` for a hard reset or
    // `ssh work@<ip> kill <pid>` for a targeted clear. Skipped when
    // no server is up — there are no processes to list.
    if let (Some(ip), Some(key)) = (server_ip.as_deref(), key_path.as_deref()) {
        match fetch_top_processes(ip, key).await {
            Some(text) if !text.trim().is_empty() => {
                println!("top processes (by CPU, snapshot):");
                for line in text.lines() {
                    println!("  {line}");
                }
                println!("  (kill via: ssh work@{ip} kill <PID>  — or `cargo burst down` for a clean slate)");
                println!();
            }
            Some(_) => {
                // ps returned nothing — extremely unusual but
                // possible on a freshly-booted server before
                // anything spawned. Suppress quietly.
            }
            None => {
                println!("top processes: (probe failed — server may be mid-init)");
                println!();
            }
        }
    }

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
/// SSH to the remote and snapshot the top 10 processes by CPU. Returns
/// the raw text — header line first, then one process per line — so
/// the caller can render it verbatim. Returns `None` if the SSH probe
/// itself fails.
///
/// We use `ps -e` (every process, including other users') because
/// `cargo burst run` and Hetzner cloud-init bring up things under
/// multiple uids that all count toward "what's keeping this box
/// busy". The column set — `pid,user,pcpu,pmem,etime,args` — gives
/// an agent enough to act on:
///   - `pid` for `kill <pid>`
///   - `user` to spot processes the build user spawned vs. system
///   - `pcpu`/`pmem` to see what's actually loaded
///   - `etime` (elapsed wall time) to spot a long-runaway job vs. a
///     short spike
///   - `args` (full command line) to identify which crate or binary
///     this rustc/cargo invocation belongs to
///
/// `--sort=-pcpu` orders highest-CPU first; `head -11` keeps the
/// header plus 10 rows. `cut -c1-220` truncates pathologically long
/// rustc command lines (frequently >2 KB) at a width that's still
/// usable for identification.
///
/// `2>/dev/null || true` on the outer pipe so transient ps failures
/// (e.g. a process disappearing mid-snapshot) don't take down the
/// whole status command.
async fn fetch_top_processes(host: &str, key_path: &std::path::Path) -> Option<String> {
    let script = "ps -eo pid,user,pcpu,pmem,etime,args --sort=-pcpu 2>/dev/null \
                  | head -11 \
                  | cut -c1-220 \
                  || true";
    ssh::capture_remote(host, "work", key_path, script).await.ok()
}

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
