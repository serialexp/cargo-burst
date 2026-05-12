//! Detect when cargo-burst's stdout is being piped into a tool that
//! buffers all output until EOF (`tail -60`, `wc`, `sort`, `less`,
//! etc.) and fail fast — before we provision a server or run cargo.
//!
//! Motivation: agents reliably write
//!
//!     cargo burst test ... 2>&1 | tail -60
//!
//! to bound the output context. `tail -N` (no `-f`) reads its entire
//! input into a fixed-size buffer and only emits the last N lines on
//! EOF. While cargo-burst is running, the consumer sees zero bytes —
//! identical from the outside to a real hang. If the consumer's bash
//! `timeout` fires before cargo-burst exits, tail's buffer is
//! discarded entirely and the consumer never sees any output at all.
//!
//! Documenting "don't do this" in CLAUDE.md didn't stop it. Refusing
//! to run is the durable fix: cargo-burst exits non-zero in <100ms
//! with a short error message that lands in the (small) last-60-lines
//! window when tail finally flushes, so the agent gets an actionable
//! diagnostic instead of a 5-minute timeout.
//!
//! Linux-only — relies on `/proc`. macOS hosts no-op (no detection),
//! which is consistent with the README's "Linux host preferred"
//! disclaimer and avoids a porting tax for a defensive feature.

/// If our stdout is being piped into a known-buffering tool, return
/// its full command line for the error message. Otherwise return
/// `None` — meaning "fine, proceed."
///
/// Returns `None` when:
/// - stdout isn't a pipe (TTY, file redirect, /dev/null, etc.)
/// - the pipe consumer is a streaming tool (`tee`, `cat`, `grep`,
///   `tail -f`, …)
/// - we can't tell (not Linux, /proc unreadable, consumer process
///   already exited and got reaped between our reads)
///
/// False negatives are acceptable here — we'd rather let a borderline
/// case through than block a legitimate `cargo burst | some-script`
/// invocation. False *positives* would be much worse, so the
/// buffering-tool list is intentionally small and matched on the
/// canonical program basename.
pub fn detect_buffering_pipe_consumer() -> Option<String> {
    detect_impl()
}

#[cfg(target_os = "linux")]
fn detect_impl() -> Option<String> {
    use std::fs;

    let link = fs::read_link("/proc/self/fd/1").ok()?;
    let target = link.to_string_lossy().into_owned();
    // Pipes look like `pipe:[12345678]` in /proc. Everything else
    // (regular files, /dev/null, /dev/pts/*, etc.) is fine — those
    // either land output somewhere readable or are an interactive
    // TTY the user is watching directly.
    if !target.starts_with("pipe:[") {
        return None;
    }

    // The pipe inode is shared between the write end (us, fd 1) and
    // the read end (the consumer's stdin, fd 0). Walk /proc/*/fd to
    // find any process other than us that has this same target in
    // its fd table — that's the reader.
    let our_pid = std::process::id();
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == our_pid {
            continue;
        }
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            // Permission denied on another user's process, or it
            // exited between readdir and now. Skip silently.
            continue;
        };
        for fd in fds.flatten() {
            let Ok(fd_link) = fs::read_link(fd.path()) else {
                continue;
            };
            if fd_link.to_string_lossy() != target {
                continue;
            }
            // Found the consumer. Pull its cmdline.
            let cmdline = read_cmdline(&entry.path())?;
            if cmdline.is_empty() {
                return None;
            }
            let prog = std::path::Path::new(&cmdline[0])
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cmdline[0].clone());
            if is_buffering_tool(&prog, &cmdline) {
                return Some(cmdline.join(" "));
            }
            return None;
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detect_impl() -> Option<String> {
    // /proc is Linux-specific. macOS could in principle implement
    // this via libproc or lsof but the implementation cost isn't
    // worth it for a defensive check — macOS hosts are a tiny
    // minority of cargo-burst users (the README explicitly says
    // "Linux host preferred").
    None
}

#[cfg(target_os = "linux")]
fn read_cmdline(proc_dir: &std::path::Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(proc_dir.join("cmdline")).ok()?;
    Some(
        bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// Decide whether `prog` (the basename of argv[0]) is a tool that
/// buffers all input until EOF. Pure function — easy to unit-test
/// across the cases we care about.
///
/// `tail` is special-cased: `tail -f` / `--follow` streams lines as
/// they arrive (that's the whole point of `-f`), so it's fine. The
/// classic `tail -N` form is the problem.
///
/// Conservative: an unknown program is *always* assumed to stream.
/// We'd rather miss a legitimate problem than block a user's
/// reasonable pipeline.
pub fn is_buffering_tool(prog: &str, cmdline: &[String]) -> bool {
    match prog {
        "tail" => !cmdline
            .iter()
            .skip(1)
            .any(|a| a == "-f" || a == "--follow" || a.starts_with("--follow=")
                || a == "-F" || a.contains('f') && a.starts_with('-') && !a.starts_with("--")),
        // Read-everything-then-emit tools. All of these are common
        // "I want to look at the result" pipe consumers that hide
        // streaming output entirely.
        "wc" | "sort" | "less" | "more" | "head" => true,
        // Everything else — `tee`, `cat`, `grep`, `awk`, `sed`, user
        // scripts, etc. — is assumed to stream.
        _ => false,
    }
}

/// Format the error users (and agents) see when detection fires.
/// Pulled into its own function for readability and so a test can
/// pin the exact phrasing — the message has to be self-explanatory
/// because it'll most likely be read by an LLM agent that has no
/// other context.
pub fn buffering_pipe_error(consumer_cmdline: &str) -> String {
    format!(
        "stdout is being piped into a tool that buffers all output until cargo-burst exits:\n\
         \n    → {consumer_cmdline}\n\n\
         This hides cargo's live Compiling/PASS output and makes long runs look hung\n\
         from the outside. Cargo-burst's output is already terse — the right answer is\n\
         to run without the pipe so you can watch the build stream. If you genuinely\n\
         need to bound output size, redirect to a file and `tail -f` it from another\n\
         shell:\n\
         \n    cargo burst ... > /tmp/burst.log 2>&1 &\n    \
         tail -f /tmp/burst.log\n\
         \n\
         To diagnose a *real* hang: `cargo burst status` in another shell shows the\n\
         top processes on the remote with PIDs."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tail_without_follow_is_buffering() {
        assert!(is_buffering_tool("tail", &s(&["tail", "-60"])));
        assert!(is_buffering_tool("tail", &s(&["tail", "-n", "100"])));
        assert!(is_buffering_tool("tail", &s(&["tail"])));
    }

    #[test]
    fn tail_with_follow_streams() {
        assert!(!is_buffering_tool("tail", &s(&["tail", "-f", "/tmp/x"])));
        assert!(!is_buffering_tool("tail", &s(&["tail", "--follow", "/tmp/x"])));
        assert!(!is_buffering_tool(
            "tail",
            &s(&["tail", "--follow=name", "/tmp/x"])
        ));
        assert!(!is_buffering_tool("tail", &s(&["tail", "-F", "/tmp/x"])));
        // -fn 50 (combined flags) — the `f` is present.
        assert!(!is_buffering_tool("tail", &s(&["tail", "-fn", "50"])));
    }

    #[test]
    fn wc_sort_less_more_head_are_buffering() {
        assert!(is_buffering_tool("wc", &s(&["wc", "-l"])));
        assert!(is_buffering_tool("sort", &s(&["sort"])));
        assert!(is_buffering_tool("less", &s(&["less"])));
        assert!(is_buffering_tool("more", &s(&["more"])));
        assert!(is_buffering_tool("head", &s(&["head", "-n", "10"])));
    }

    #[test]
    fn streaming_tools_pass_through() {
        // Common "good" consumers — tee, cat, grep without flags
        // that change streaming behavior, plus arbitrary user scripts.
        assert!(!is_buffering_tool("tee", &s(&["tee", "out.log"])));
        assert!(!is_buffering_tool("cat", &s(&["cat"])));
        assert!(!is_buffering_tool("grep", &s(&["grep", "PASS"])));
        assert!(!is_buffering_tool("awk", &s(&["awk", "{print $1}"])));
        assert!(!is_buffering_tool("sed", &s(&["sed", "s/foo/bar/"])));
        assert!(!is_buffering_tool(
            "my-script.sh",
            &s(&["./my-script.sh", "--arg"])
        ));
    }

    #[test]
    fn error_message_mentions_consumer_and_workarounds() {
        let msg = buffering_pipe_error("tail -60");
        assert!(msg.contains("tail -60"));
        assert!(msg.contains("tail -f"));
        assert!(msg.contains("cargo burst status"));
    }
}
