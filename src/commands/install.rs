//! `cargo burst install` — write a marked block of usage instructions
//! into `~/.claude/CLAUDE.md` so Claude Code knows when and how to
//! reach for cargo-burst.
//!
//! The block is bounded by HTML comment markers so we can rewrite it
//! in place on subsequent runs (after a feature lands, bump the
//! [`INSTRUCTIONS`] string and re-run install). Content outside the
//! markers is preserved untouched. If the markers are absent the
//! block is appended to the end of the file (and the file is created
//! if it doesn't exist).
//!
//! Mirrors the `my-docs install` pattern.

use anyhow::{Context, Result};
use std::path::PathBuf;

const START_MARKER: &str = "<!-- cargo-burst:start -->";
const END_MARKER: &str = "<!-- cargo-burst:end -->";

/// What gets written between the markers. Edit this when the public
/// surface (subcommands, flags, behavior) changes; users re-run
/// `cargo burst install` to pick up the new copy.
const INSTRUCTIONS: &str = r##"## cargo-burst

You have access to the `cargo-burst` CLI tool, which runs cargo commands on a remote Hetzner Cloud server (CCX63: 48 dedicated cores, 192 GB RAM) with a persistent per-project volume that keeps `target/` and the sccache directory warm between sessions. The user prefers this for big Rust builds because their local machine is slower and they don't want to heat the room.

### When to use cargo-burst

Reach for `cargo burst` instead of plain `cargo` when:
- The user explicitly asks ("run this on burst", "build on the remote")
- A build or test run is taking long enough on the local machine that the user is visibly waiting
- The user has just made changes that touch many crates and wants to verify everything still compiles (`cargo burst check`)
- You're about to run benchmarks — bench numbers are only comparable on consistent hardware (`cargo burst bench`)
- A clippy gate is being run as a CI parity check (`cargo burst clippy`)
- A binary is embarrassingly parallel / CPU-bound and would gain meaningfully from 48 cores instead of the laptop's (`cargo burst run`)

Do **not** silently switch to `cargo burst` if the user said `cargo`. They might be intentionally testing locally (e.g. with hardware-specific features, debugging a remote-vs-local discrepancy, or running a binary that needs their local network/files).

### Subcommands (complete list)

Run-subcommands — execute a cargo verb on the remote, output streams back live:

| Command                       | What it runs on the remote                                                       | Fetches back                                         |
|-------------------------------|----------------------------------------------------------------------------------|------------------------------------------------------|
| `cargo burst build [args]`    | `cargo build [args]`                                                             | top-level files in `target/<profile>/`               |
| `cargo burst test [args]`     | `cargo nextest run` + `cargo test --doc` (doctest pass auto-skipped if args contain `--test`/`--bin`/`--example`/`--bench` or their plurals, or if the workspace has no library targets) | nothing                                              |
| `cargo burst check [args]`    | `cargo check [args]`                                                             | nothing                                              |
| `cargo burst clippy [args]`   | `cargo clippy [args]`                                                            | nothing                                              |
| `cargo burst bench [args]`    | `cargo bench [args]`                                                             | `target/criterion/` recursively (if it exists)       |
| `cargo burst run [args]`      | `cargo run [args]` — build + execute the binary on the CCX63; stdout/stderr stream back, stdin is closed (no TTY allocation for the binary). Useful for embarrassingly-parallel CPU-bound runs (parameter sweeps, big one-offs, perf debugging). File outputs stay on the remote unless you rsync them yourself. | nothing                                              |

Management subcommands (no remote cargo phase):

| Command                          | What it does                                                                              |
|----------------------------------|-------------------------------------------------------------------------------------------|
| `cargo burst status`             | Show what's currently provisioned (server, volumes, last-used per project, current cost). |
| `cargo burst audit [--rate EUR_PER_HOUR]` | Summarize the lifecycle log: session count, cold-vs-warm timings, wall-time split (provision/sync/cargo), per-verb means, churn buckets (10/30/60-min terminate→reprovision pairs), and a keep-alive what-if model. Pass `--rate` to get cost figures alongside wall-time. |
| `cargo burst down [--with-volumes]` | Delete the running server now. Volumes are preserved by default; `--with-volumes` deletes all project volumes too (clean slate). |
| `cargo burst image build`        | Bake a fresh base image (Ubuntu + rust + mold + sccache + nextest + postgres/mysql/redis). One-time per toolchain refresh, ~5–10 min. |
| `cargo burst install`            | Refresh this instructions block in `~/.claude/CLAUDE.md` (re-run after upgrading cargo-burst). |

### Forwarding cargo args

The leading `--` is **optional** — args are forwarded to cargo verbatim. Both forms work:

```
cargo burst build --release --features=foo
cargo burst build -- --release --features=foo
cargo burst test --test integration_tests
cargo burst run --release --bin solver -- --threads 48     # inner -- routes to binary's argv
cargo burst clippy --all-targets -- -D warnings           # inner -- routes to clippy lint args
cargo burst test --release -- --nocapture                 # inner -- routes to test-runner args
```

Use the explicit leading `--` only when you need to pass something that would otherwise look like a burst-specific flag (e.g. `cargo burst build -- --no-fetch some-feature` — rare).

### Common flags (on every run-subcommand)

- `--keep-alive SECONDS` — override the server idle timer for this run.
- `--no-reap` — don't schedule auto-deletion; server + volume stay alive until `cargo burst down`.
- `--no-fetch` — skip the artifact rsync (only meaningful for `build` and `bench`).
- `--yes` — skip the first-run "apply suggested rsync excludes?" prompt.
- `--env VAR[=VALUE]` — forward an environment variable. `--env RUST_LOG` forwards your local `$RUST_LOG` value; `--env DATABASE_URL=postgres://…` sets it verbatim. Repeatable. Use for DATABASE_URL / REDIS_URL / etc. that integration tests need.

Test-only flags:

- `--no-doctests` — skip the `cargo test --doc` pass after nextest.
- `--cargo-test` — use plain `cargo test` instead of nextest (rare; only when tests have nextest-incompatible fixtures).

### Things to know

- **`run` is fully supported** and is the right tool when the user has a CPU-bound binary they want to run on 48 cores. It does not fetch any file outputs back — if the binary writes results to disk, they stay on the remote (the user can `cargo burst run -- … --output /tmp/out.json` then handle retrieval separately).
- **Source is rsync'd, not mounted.** Files written remotely (e.g. by build scripts or `cargo burst run`) don't appear locally. `target/` lives on the remote volume; only `build`'s and `bench`'s explicit fetch step pulls things back.
- **Arch warning on macOS hosts.** `cargo burst build` warns that the fetched binary is `linux-x86_64` and won't run locally. Pass `--no-fetch` if the user only cared about exercising the build, not getting a runnable binary.
- **Test failures skip doctests.** `cargo burst test` runs nextest first; if any tests fail it bails before `cargo test --doc`.
- **Integration DBs are pre-provisioned.** `cargo burst test` and `cargo burst bench` block until postgres (5432), mysql (3306), and redis (6379) are reachable on the remote. Connection strings: `postgres://postgres@localhost:5432/postgres` (no password, trust auth), `mysql://root:root@localhost:3306/`, `redis://localhost:6379/`. Pass `--env DATABASE_URL=…` to forward the connection string the tests expect. DB data lives on the server's root disk and is reset on every cold boot.
- **First-run prompt.** Initial sync into a project prompts to confirm rsync excludes. Pass `--yes` to bypass when scripting.
- **Output streams live.** stdout/stderr from cargo come back over the SSH session in real time.
- **Killing locally kills remotely.** Ctrl-C / SIGTERM on cargo-burst propagates to the remote cargo process; you don't leave orphan builds running on the paid VM.
- **Per-project config.** `<workspace>/.config/cargo-burst.toml` can override `server_type`, `volume_gb`, `forward_env`, `unexclude`, `extra_excludes`. Check there if a project sets `forward_env = [...]` to know what env you should pass via `--env`.

### When NOT to suggest cargo burst

- The user is debugging something local-environment-specific (PATH, env vars, native deps installed only on their box).
- The binary opens a port that should be reachable from the user's local browser/curl, reads local files outside the workspace, or otherwise expects to be on the user's machine. (`cargo burst run` puts the binary on a remote IP.)
- The build is fast enough that SSH+rsync overhead would make `cargo burst` slower than just running locally (small crates, single-file changes with a warm local cache).
- The user is iterating very tightly and doesn't have an active `cargo burst` server warm — every cold start adds ~30s of provisioning.
"##;

/// Splice [`INSTRUCTIONS`] into `content` between the cargo-burst
/// markers. If the markers exist, the body between them is replaced
/// (and the markers themselves preserved). If they don't, a new
/// marked block is appended (or written as the entire file if
/// `content` is empty).
pub fn update_section(content: &str, instructions: &str) -> String {
    let section = format!("{START_MARKER}\n{instructions}\n{END_MARKER}");

    if let (Some(start), Some(end)) = (content.find(START_MARKER), content.find(END_MARKER))
        && end > start
    {
        let before = &content[..start];
        let after = &content[end + END_MARKER.len()..];
        return format!("{before}{section}{after}");
    }

    if content.is_empty() {
        return format!("{section}\n");
    }

    let mut out = content.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&section);
    out.push('\n');
    out
}

pub async fn run() -> Result<()> {
    let home = directories::BaseDirs::new()
        .context("could not resolve home directory")?
        .home_dir()
        .to_path_buf();
    let claude_md: PathBuf = home.join(".claude").join("CLAUDE.md");

    let existing = match std::fs::read_to_string(&claude_md) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context(format!("reading {}", claude_md.display())),
    };

    let updated = update_section(&existing, INSTRUCTIONS);

    if updated == existing {
        // Marker block was already up-to-date — no write needed.
        // (Not currently reachable since we always replace the body
        // even if it's byte-identical, but keeping the branch for
        // when we add a content-hash short-circuit.)
        println!("cargo-burst instructions already current at {}", claude_md.display());
        return Ok(());
    }

    if let Some(parent) = claude_md.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&claude_md, updated)
        .with_context(|| format!("writing {}", claude_md.display()))?;

    let action = if existing.contains(START_MARKER) {
        "Updated"
    } else if existing.is_empty() {
        "Created"
    } else {
        "Appended"
    };
    println!("{action} cargo-burst instructions in {}", claude_md.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_when_no_markers() {
        let out = update_section("# existing\n\nhello\n", "BODY");
        assert!(out.starts_with("# existing\n\nhello\n"));
        assert!(out.contains(START_MARKER));
        assert!(out.contains("BODY"));
        assert!(out.contains(END_MARKER));
    }

    #[test]
    fn replaces_existing_section() {
        let original = format!("intro\n\n{START_MARKER}\nold body\n{END_MARKER}\noutro\n");
        let out = update_section(&original, "new body");
        assert!(out.contains("new body"));
        assert!(!out.contains("old body"));
        assert!(out.contains("intro\n"));
        assert!(out.contains("outro\n"));
    }

    #[test]
    fn creates_when_empty() {
        let out = update_section("", "BODY");
        assert!(out.starts_with(START_MARKER));
        assert!(out.contains("BODY"));
        assert!(out.trim_end().ends_with(END_MARKER));
    }

    #[test]
    fn adds_trailing_newline_when_missing() {
        let out = update_section("no newline at end", "BODY");
        assert!(out.contains("no newline at end\n"));
        assert!(out.contains(START_MARKER));
    }
}
