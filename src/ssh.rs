//! Thin wrappers around the system `ssh` and `rsync` binaries.
//!
//! We deliberately shell out instead of pulling in `russh`/`thrussh` —
//! `ssh` and `rsync` are universally available on Linux dev machines, the
//! options surface is well-understood, and the subprocess boundary keeps
//! the binary small.

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Common ssh options for talking to a freshly-provisioned cloud VM whose
/// host key we haven't seen before. We deliberately disable host-key
/// checking and known_hosts persistence — these are short-lived servers
/// reachable only via the IP Hetzner just allocated to us, and pinning a
/// host key per session would just produce noise.
///
/// Connection multiplexing is on (`ControlMaster=auto`,
/// `ControlPersist=120s`): the first ssh/rsync invocation against a host
/// opens a master connection, and every subsequent one reuses the existing
/// TCP+crypto channel via a Unix socket. From a high-RTT location (Japan →
/// Helsinki ≈ 280 ms) this turns the per-invocation handshake from ~1.5–3 s
/// into <50 ms. ControlPersist keeps the socket alive briefly between
/// invocations so back-to-back `cargo burst check` runs benefit too.
fn base_ssh_opts(key_path: &Path, host: &str) -> Vec<String> {
    let cm = control_socket_path(host);
    vec![
        "-o".into(), "StrictHostKeyChecking=no".into(),
        "-o".into(), "UserKnownHostsFile=/dev/null".into(),
        "-o".into(), "LogLevel=ERROR".into(),
        "-o".into(), "ConnectTimeout=10".into(),
        "-o".into(), "ServerAliveInterval=30".into(),
        "-o".into(), "ControlMaster=auto".into(),
        "-o".into(), format!("ControlPath={}", cm.display()),
        "-o".into(), "ControlPersist=120s".into(),
        "-i".into(), key_path.display().to_string(),
    ]
}

/// Compute the Unix socket path used for ssh ControlMaster multiplexing
/// for a given host.
///
/// Constraints:
/// - Unix domain socket paths cap at ~104–108 chars depending on OS, so
///   `~/Library/Application Support/dev.serialexp.cargo-burst/` plus a
///   SHA-256 of host+user+port (40+ chars from ssh's `%C` token) blows
///   past the macOS limit. We sidestep the whole problem by anchoring
///   under `/tmp/cargo-burst-<user>/` (< 30 chars on every platform we
///   target) and using a short hash we control.
/// - The socket dir is per-user to avoid collisions on multi-user boxes,
///   and per-host so two cargo-burst sessions targeting different
///   servers don't trample each other.
///
/// `mkdir -p` on the parent dir is best-effort here; if it fails ssh
/// will surface the error on first connect and the user gets a real
/// diagnostic instead of a panic.
fn control_socket_path(host: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    // 8 bytes = 16 hex chars: plenty for collision avoidance among the
    // <10 hosts a single user could plausibly have alive at once.
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    let dir = PathBuf::from("/tmp").join(format!("cargo-burst-{user}"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("cm-{short}.sock"))
}

// Note: we deliberately don't expose a `close_control_master` helper.
// `ControlPersist=120s` reaps the socket within two minutes of the last
// client exit, the per-host hash means a recycled IP almost certainly
// produces a different socket name, and OpenSSH gracefully falls back
// to a fresh connection if it ever finds a master pointing at a corpse.
// The added complexity of looking up the live IP before each `delete_server`
// call to send `ssh -O exit` isn't worth it.

/// Wait for SSH to come up on a freshly-booted host.
///
/// Two-phase probe to minimise the gap between "sshd is ready" and "we
/// noticed":
///
/// 1. **TCP probe** every 500 ms with a 1 s `connect` timeout. While the
///    server's network stack is settling, this either fails fast (RST when
///    the port is closed) or times out at 1 s (when packets are silently
///    dropped). Cheap — no `ssh` process spawn, no handshake, no auth.
/// 2. **Real SSH** once TCP succeeds. The full handshake confirms sshd is
///    actually serving (vs. just listening), and the auth confirms our key
///    is in place. If this fails, fall back to the TCP loop — sshd may
///    have bound the socket without finishing init.
///
/// The previous implementation invoked `ssh` itself for every probe, which
/// could stall up to `ConnectTimeout=10s` per attempt against a host that
/// silently drops packets. The cold-boot wait was 35 s for a server that
/// reported `Startup finished in 16.5 s` — most of the gap was polling
/// overshoot, not real boot time.
pub async fn wait_for_ssh(
    host: &str,
    user: &str,
    key_path: &Path,
    timeout: std::time::Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    let probe_interval = std::time::Duration::from_millis(500);
    let probe_timeout = std::time::Duration::from_secs(1);
    let addr = format!("{host}:22");

    loop {
        // Phase 1: TCP probe.
        let connect = tokio::time::timeout(
            probe_timeout,
            tokio::net::TcpStream::connect(&addr),
        )
        .await;
        match connect {
            Ok(Ok(stream)) => {
                drop(stream);
                // Phase 2: real ssh handshake.
                let mut cmd = Command::new("ssh");
                cmd.args(base_ssh_opts(key_path, host));
                cmd.arg(format!("{user}@{host}"));
                cmd.arg("true");
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null());
                let status = cmd.status().await.context("spawning ssh")?;
                if status.success() {
                    tracing::info!(host, elapsed = ?start.elapsed(), "ssh is up");
                    return Ok(());
                }
                // sshd has the port but auth/handshake failed — usually
                // means sshd is mid-init. Loop and try again shortly.
            }
            Ok(Err(_)) | Err(_) => {
                // TCP not ready (RST, ECONNREFUSED, EHOSTUNREACH, or our
                // 1 s timeout fired). Just keep polling.
            }
        }

        if start.elapsed() >= timeout {
            return Err(anyhow!(
                "ssh to {host} did not come up within {:?}",
                timeout
            ));
        }
        tokio::time::sleep(probe_interval).await;
    }
}

/// Run a command on the remote host, streaming stdout/stderr back to the
/// caller's terminal as it arrives. Returns the exit status.
pub async fn run_remote(
    host: &str,
    user: &str,
    key_path: &Path,
    remote_cmd: &str,
) -> Result<std::process::ExitStatus> {
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_opts(key_path, host));
    cmd.arg(format!("{user}@{host}"));
    cmd.arg(remote_cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().context("spawning ssh")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Mirror remote stdout to ours so the user sees `cargo` output
            // in real time.
            println!("{line}");
        }
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{line}");
        }
    });
    let status = child.wait().await.context("waiting on ssh")?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    Ok(status)
}

/// Run a remote command and capture its stdout (no streaming). Useful for
/// queries like `df -h` whose output is small and we want to parse.
pub async fn capture_remote(
    host: &str,
    user: &str,
    key_path: &Path,
    remote_cmd: &str,
) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_opts(key_path, host));
    cmd.arg(format!("{user}@{host}"));
    cmd.arg(remote_cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let out = cmd.output().await.context("spawning ssh")?;
    if !out.status.success() {
        return Err(anyhow!(
            "remote command failed (status {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// rsync `src/` (with trailing slash semantics — copy contents, not the dir
/// itself) to `user@host:dest`. `extra_excludes` are appended on top of the
/// default exclude list.
///
/// If `remote_mkdir` is `Some(path)`, that path is `mkdir -p`'d on the remote
/// inside rsync's *own* SSH session (via `--rsync-path`), so we don't need a
/// separate ssh round-trip to create the destination directory before
/// transfer. The mkdir runs before rsync's first write.
pub async fn rsync_to(
    host: &str,
    user: &str,
    key_path: &Path,
    src: &Path,
    dest: &str,
    extra_excludes: &[&str],
    remote_mkdir: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new("rsync");
    // No `--info=progress2`: the progress line uses carriage-return
    // updates that look fine on a real TTY but, when piped into an
    // agent's tool-result, become a wall of partial-progress
    // fragments. rsync defaults to silent on success and emits a
    // useful diagnostic on failure, which is exactly what we want
    // here.
    cmd.arg("-az").arg("--delete");
    if let Some(path) = remote_mkdir {
        // The remote shell rsync's ssh transport spawns runs this in place
        // of `rsync`. The single-quoted path defends against unusual hashes
        // (today they're hex, but this keeps the path opaque to the shell).
        cmd.arg("--rsync-path")
            .arg(format!("mkdir -p '{path}' && rsync"));
    }
    cmd.arg("-e");
    let ssh_inner = {
        let mut s = String::from("ssh");
        for opt in base_ssh_opts(key_path, host) {
            s.push(' ');
            // Quote any opt containing whitespace.
            if opt.contains(char::is_whitespace) {
                s.push('"');
                s.push_str(&opt);
                s.push('"');
            } else {
                s.push_str(&opt);
            }
        }
        s
    };
    cmd.arg(ssh_inner);
    // Default excludes — keep the server-side copy from inheriting the
    // user's local target/, .git/, IDE droppings, etc.
    for e in DEFAULT_EXCLUDES {
        cmd.arg("--exclude").arg(e);
    }
    for e in extra_excludes {
        cmd.arg("--exclude").arg(e);
    }
    // Trailing slash on the source so rsync copies *contents*, matching the
    // mental model of "make dest equal to src".
    let src_arg = format!("{}/", src.display());
    cmd.arg(src_arg);
    cmd.arg(format!("{user}@{host}:{dest}"));
    let status = cmd.status().await.context("spawning rsync")?;
    if !status.success() {
        return Err(anyhow!("rsync failed with status {status}"));
    }
    Ok(())
}

/// rsync `user@host:src` back to local `dest/`.
///
/// If `top_level_only` is true, the transfer is non-recursive: only
/// regular files at the top level of `src` (when it's a directory) are
/// copied; subdirectories are skipped entirely. We use this for the
/// post-build artifact fetch — `target/<profile>/` has the binaries we
/// want at the top level alongside huge subdirs (`deps/`, `build/`,
/// `incremental/`, `.fingerprint/`) that the local cargo would just
/// rebuild on its next invocation anyway. Pulling them back would
/// waste bandwidth and disk for no gain.
pub async fn rsync_from(
    host: &str,
    user: &str,
    key_path: &Path,
    src: &str,
    dest: &Path,
    top_level_only: bool,
) -> Result<()> {
    let mut cmd = Command::new("rsync");
    cmd.arg("-a");
    if top_level_only {
        // We want top-level *files* in src/ but none of its subdirs
        // (deps/, build/, .fingerprint/ etc. for a cargo target dir).
        // The naive `--no-r` combo with `-a` is a footgun: rsync prints
        // "skipping directory ." and transfers nothing, because without
        // recursion it refuses to enter the source dir at all.
        // `--exclude='/*/'` is the right idiom: keep -r on, but match
        // (and skip) every directory entry directly under the src root,
        // so only top-level files come through.
        cmd.arg("--exclude").arg("/*/");
    }
    cmd.arg("-z");
    cmd.arg("-e");
    let ssh_inner = {
        let mut s = String::from("ssh");
        for opt in base_ssh_opts(key_path, host) {
            s.push(' ');
            if opt.contains(char::is_whitespace) {
                s.push('"'); s.push_str(&opt); s.push('"');
            } else { s.push_str(&opt); }
        }
        s
    };
    cmd.arg(ssh_inner);
    cmd.arg(format!("{user}@{host}:{src}"));
    cmd.arg(dest);
    let status = cmd.status().await.context("spawning rsync")?;
    if !status.success() {
        return Err(anyhow!("rsync failed with status {status}"));
    }
    Ok(())
}

/// Generate a fresh ed25519 SSH keypair at `path` if one doesn't exist
/// already. Returns the public key in OpenSSH format (one line, ready to
/// paste into authorized_keys).
pub async fn ensure_ssh_key(path: &Path) -> Result<String> {
    let pub_path = pub_key_path(path);
    if path.exists() && pub_path.exists() {
        return std::fs::read_to_string(&pub_path)
            .with_context(|| format!("reading {}", pub_path.display()))
            .map(|s| s.trim().to_string());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // ssh-keygen writes both the private key at `path` and the public key
    // at `path.pub`.
    let mut cmd = Command::new("ssh-keygen");
    cmd.arg("-t").arg("ed25519");
    cmd.arg("-N").arg(""); // no passphrase — tool-managed key
    cmd.arg("-C").arg("cargo-burst");
    cmd.arg("-f").arg(path);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let out = cmd.output().await.context("spawning ssh-keygen")?;
    if !out.status.success() {
        return Err(anyhow!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let pub_text = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("reading {}", pub_path.display()))?;
    Ok(pub_text.trim().to_string())
}

/// `<priv>` → `<priv>.pub`. ssh-keygen's convention is to append `.pub` to
/// the full private-key path, including any existing extension.
fn pub_key_path(priv_path: &Path) -> PathBuf {
    let mut p = priv_path.as_os_str().to_owned();
    p.push(".pub");
    PathBuf::from(p)
}

/// Default rsync excludes. Anything matching these never leaves the local
/// machine. These are gitignore-syntax patterns (no leading slash means
/// "match anywhere in the tree"), shared with the local size scanner so the
/// reported pre-sync sizes reflect what rsync will actually transfer.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "target/",
    ".git/",
    "node_modules/",
    ".direnv/",
    ".vscode/",
    ".idea/",
    "*.swp",
    ".DS_Store",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_socket_path_is_per_host_and_short() {
        let a = control_socket_path("1.2.3.4");
        let b = control_socket_path("5.6.7.8");
        let a2 = control_socket_path("1.2.3.4");
        // Different hosts → different paths.
        assert_ne!(a, b);
        // Same host → stable path (so a second invocation reuses the
        // existing master rather than opening a new one).
        assert_eq!(a, a2);
        // Comfortably under macOS's ~104-byte sun_path limit even with
        // a long username.
        assert!(
            a.as_os_str().len() < 90,
            "control socket path too long: {} ({} chars)",
            a.display(),
            a.as_os_str().len()
        );
        // Should be a real .sock under /tmp/cargo-burst-<user>/.
        assert!(a.starts_with("/tmp/"));
        assert!(a.extension().is_some_and(|e| e == "sock"));
    }

    #[test]
    fn base_ssh_opts_includes_control_master() {
        let opts = base_ssh_opts(Path::new("/tmp/key"), "1.2.3.4");
        assert!(opts.iter().any(|o| o == "ControlMaster=auto"));
        assert!(opts.iter().any(|o| o == "ControlPersist=120s"));
        assert!(opts.iter().any(|o| o.starts_with("ControlPath=")));
    }
}
