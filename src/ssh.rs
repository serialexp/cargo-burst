//! Thin wrappers around the system `ssh` and `rsync` binaries.
//!
//! We deliberately shell out instead of pulling in `russh`/`thrussh` —
//! `ssh` and `rsync` are universally available on Linux dev machines, the
//! options surface is well-understood, and the subprocess boundary keeps
//! the binary small.

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

// ── ssh kill-propagation infrastructure ───────────────────────────────
//
// We track the PIDs of every ssh child we spawn so the signal handler
// installed in `main.rs` can SIGTERM them on Ctrl-C / `kill <pid>`.
// Killing the local ssh cleanly closes its TCP connection, and when
// the spawn site requested a PTY (via `force_tty`), sshd then sends
// SIGHUP to the remote process, so a kill on the laptop actually
// stops the cargo run on the CCX63 — instead of leaving an orphaned
// build chugging through to completion on a server we're paying for.
//
// Why a global registry and not a single passed-around handle?
//   - Multiple ssh children can be in flight concurrently (mount +
//     rsync run in parallel inside `with_remote`).
//   - The signal handler lives in a tokio::spawn at top level and
//     doesn't have a path to whatever subcommand is running.
//   - We don't track Child handles directly because we still want
//     the original tokio task to own and `wait()` on its child; the
//     handler just needs to fire a SIGTERM.

/// Returns the singleton registry of currently-running ssh child PIDs.
fn ssh_pid_registry() -> &'static Mutex<HashSet<u32>> {
    static R: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// RAII guard returned from each ssh spawn site. Adds the child's
/// PID to the registry on construction and removes it on drop —
/// works for normal-completion paths, `?` early returns, and
/// panics. Doesn't fire on signal-killed parent processes (Drop
/// doesn't run then), but the signal handler doesn't care: a stale
/// entry in the registry is harmless because we exit immediately
/// after kill_all_tracked_ssh anyway.
struct TrackedSshGuard {
    pid: u32,
}

impl TrackedSshGuard {
    fn new(pid: u32) -> Self {
        if pid != 0 {
            if let Ok(mut g) = ssh_pid_registry().lock() {
                g.insert(pid);
            }
        }
        Self { pid }
    }
}

impl Drop for TrackedSshGuard {
    fn drop(&mut self) {
        if self.pid != 0 {
            if let Ok(mut g) = ssh_pid_registry().lock() {
                g.remove(&self.pid);
            }
        }
    }
}

/// SIGTERM every currently-tracked ssh child. Called from the global
/// signal handler. Doesn't wait for the children to exit — the caller
/// is expected to give them a short grace period and then `exit()`.
pub fn kill_all_tracked_ssh() {
    let pids: Vec<u32> = ssh_pid_registry()
        .lock()
        .map(|g| g.iter().copied().collect())
        .unwrap_or_default();
    for pid in pids {
        // SIGTERM (not SIGKILL): gives the local ssh client a chance
        // to send the channel-close to sshd cleanly, which is what
        // triggers the SIGHUP→remote-process chain when we requested
        // `-tt`. SIGKILL would yank the ssh client without the close
        // round-trip, and the remote could stay alive longer.
        //
        // Safety: `kill(pid, SIGTERM)` with a valid pid is a no-op
        // if the target has already exited (returns ESRCH). Unsafe
        // because libc::kill is FFI; semantically safe to call from
        // any thread.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

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
                cmd.kill_on_drop(true);
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
///
/// `kill_on_drop(true)` ensures that if this function's future is
/// dropped (panic, `?`-error elsewhere in the join, abort) the local
/// ssh process is killed too, so we don't leak a connection. The
/// global PID registry covers the signal-killed-parent case.
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
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawning ssh")?;
    let _guard = TrackedSshGuard::new(child.id().unwrap_or(0));
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

/// Like `run_remote`, but additionally captures combined stdout +
/// stderr into a returned `String` while still streaming both to the
/// caller's terminal in real time. Used by the cargo-verb call sites
/// so we can post-mortem the output for actionable hints
/// (`hints::detect_env_hint` etc.) on failure.
///
/// `force_tty=true` passes `-tt` to ssh, which allocates a remote
/// PTY for the command. With a PTY, dropping the ssh connection
/// (which is what happens when the local process gets SIGTERM /
/// SIGINT) causes sshd to SIGHUP the remote command — so a Ctrl-C
/// on the laptop actually stops the cargo run on the remote, instead
/// of orphaning a multi-minute build on a server we're paying for.
/// Without `-tt`, sshd doesn't propagate signals on connection drop
/// and the remote command runs to completion. We only opt in here
/// (and from the cargo-verb call sites), not on short-lived helpers
/// like `run_remote` (mount script) or `capture_remote`.
///
/// Memory cost is bounded by however much cargo prints during the
/// run — for a green test suite that's a few KB, for a failed
/// compilation it might be a few hundred KB. Either way well under
/// any threshold worth streaming-with-cap-and-truncate logic for.
pub async fn run_remote_capturing(
    host: &str,
    user: &str,
    key_path: &Path,
    remote_cmd: &str,
    force_tty: bool,
) -> Result<(std::process::ExitStatus, String)> {
    use std::sync::{Arc, Mutex};
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_opts(key_path, host));
    if force_tty {
        // `-tt` (vs single `-t`) forces TTY even when ssh's local
        // stdin is not a TTY. We give ssh a null stdin (below) so
        // without the double-t it would silently fall back to no-PTY
        // mode and we'd lose signal propagation.
        cmd.arg("-tt");
    }
    cmd.arg(format!("{user}@{host}"));
    cmd.arg(remote_cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawning ssh")?;
    let _guard = TrackedSshGuard::new(child.id().unwrap_or(0));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Single mutex around the combined buffer — interleaving stdout
    // and stderr in the order lines actually appear on the wire is
    // what we want for hint detection (panic backtraces span both).
    let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cap_out = captured.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            println!("{line}");
            if let Ok(mut g) = cap_out.lock() {
                g.push_str(&line);
                g.push('\n');
            }
        }
    });
    let cap_err = captured.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{line}");
            if let Ok(mut g) = cap_err.lock() {
                g.push_str(&line);
                g.push('\n');
            }
        }
    });
    // Grab abort handles before consuming the JoinHandles in the
    // timeout below — once `await`ed (even inside a timeout), the
    // JoinHandle is moved and we can't reach back to abort the task.
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let status = child.wait().await.context("waiting on ssh")?;
    // Defense in depth: ssh has exited, so its stdout/stderr pipes
    // should EOF and our reader tasks should drain in milliseconds.
    // If they're still pending after a short grace period something
    // pathological is keeping the pipes open (most likely a remote
    // grandchild that inherited the PTY's slave fd — pre-starting
    // sccache in `build_remote_cmd` should prevent the known case,
    // but other build-script daemons could theoretically do the
    // same). Abort and move on rather than letting the function
    // hang forever; the captured buffer is only used for post-hoc
    // hint detection, so missing a few trailing bytes is fine.
    let drain_deadline = std::time::Duration::from_secs(2);
    let drained = tokio::time::timeout(drain_deadline, async {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            "ssh exited but stdout/stderr drain stalled past 2s; aborting readers \
             (this usually means a remote grandchild — build-script daemon or \
             similar — is holding a PTY slave fd open after the main process exited)"
        );
        stdout_abort.abort();
        stderr_abort.abort();
    }
    // By the time both tasks have joined, the only remaining Arc
    // owner is the local binding — `try_unwrap` succeeds and we can
    // move out the String without an extra clone.
    let combined = match Arc::try_unwrap(captured) {
        Ok(m) => m.into_inner().unwrap_or_default(),
        Err(arc) => arc.lock().map(|g| g.clone()).unwrap_or_default(),
    };
    Ok((status, combined))
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
    cmd.kill_on_drop(true);
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
    excludes: &[&str],
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
    // Caller composes the full exclude list — defaults, project's
    // `unexclude`, project's `extra_excludes`, and the historical
    // interactive `state.excludes`. We just feed it to rsync.
    for e in excludes {
        cmd.arg("--exclude").arg(e);
    }
    // Trailing slash on the source so rsync copies *contents*, matching the
    // mental model of "make dest equal to src".
    let src_arg = format!("{}/", src.display());
    cmd.arg(src_arg);
    cmd.arg(format!("{user}@{host}:{dest}"));
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawning rsync")?;
    let _guard = TrackedSshGuard::new(child.id().unwrap_or(0));
    let status = child.wait().await.context("waiting on rsync")?;
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
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawning rsync")?;
    let _guard = TrackedSshGuard::new(child.id().unwrap_or(0));
    let status = child.wait().await.context("waiting on rsync")?;
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
