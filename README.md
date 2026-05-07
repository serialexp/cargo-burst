# cargo-burst

Spin up a Hetzner Cloud server on demand to run `cargo build`, `cargo test`,
`cargo check`, `cargo clippy`, or `cargo bench` offsite, with a persistent
per-project volume that keeps `target/` and the sccache directory warm
between sessions. The server is destroyed after a short idle window;
volumes get reaped after a longer per-project idle window so you're not
paying €10/month for a cache you forgot about.

Designed for the case where your local machine has plenty of cores but
you'd rather not heat the room (or your CPU's thermals) every build.

## Status

Pre-alpha. Linux host preferred (uses `ssh` and `rsync`). macOS hosts work
too, but `cargo burst build` will warn that the fetched binary won't run
locally since the remote is `linux-x86_64` — pass `--no-fetch` if you don't
need the artifact back.

## Install

```sh
cargo install cargo-burst
```

Or directly from the repo:

```sh
cargo install --git https://github.com/serialexp/cargo-burst
```

## Quick start

```sh
# 1. Put your Hetzner API token in ~/.config/cargo-burst/config.toml
mkdir -p ~/.config/cargo-burst
cat > ~/.config/cargo-burst/config.toml <<'EOF'
hetzner_token          = "<your token here>"
# Try regions in this order when provisioning a server. Each entry is
# an Hetzner location code (`hel1`, `fsn1`, `nbg1`, `ash`, `sin`, …).
# If the first region returns "resource_unavailable" (CCX-class fully
# booked), the next is tried automatically. Volumes are regional, so
# falling back rebuilds the project's build cache in the new region
# (~30s penalty).
regions                = ["hel1", "fsn1", "nbg1"]
# Or, for a single region with no fallback:
# region               = "hel1"
server_type            = "ccx63"
keep_alive_secs        = 300       # server reaper: 5 min idle
volume_keep_alive_secs = 3600      # volume reaper: 1 hour idle (per project)
volume_gb              = 200
EOF

# 2. One-time: bake an image with rust + mold + sccache (~5-10 min)
cargo burst image build

# 3. Run cargo on the remote
cd ~/Projects/some-rust-thing
cargo burst build --release        # build + fetch top-level artifacts back
cargo burst test                   # cargo nextest run + cargo test --doc
cargo burst check                  # type-check, no fetch
cargo burst clippy -- -- -D warnings
cargo burst bench                  # criterion HTML reports rsynced back
```

## Subcommands

| Command                      | What it runs on the remote               | Fetches back                                  |
|------------------------------|------------------------------------------|-----------------------------------------------|
| `cargo burst build [args]`   | `cargo build [args]`                     | top-level files in `target/<profile>/`        |
| `cargo burst test [args]`    | `cargo nextest run` + `cargo test --doc` | nothing                                       |
| `cargo burst check [args]`   | `cargo check [args]`                     | nothing                                       |
| `cargo burst clippy [args]`  | `cargo clippy [args]`                    | nothing                                       |
| `cargo burst bench [args]`   | `cargo bench [args]`                     | `target/criterion/` (recursive, if present)   |
| `cargo burst status`         | —                                        | shows what's provisioned + last-used per project |
| `cargo burst audit`          | —                                        | summarises the lifecycle log (sessions, cold-vs-warm timings, wall-time split, per-verb means; pass `--rate EUR_PER_HOUR` for cost) |
| `cargo burst down`           | deletes the running server               | (volume kept until volume reaper fires)       |
| `cargo burst image build`    | bakes a fresh base image                 | —                                             |

Args after `--` go to cargo verbatim:

```sh
cargo burst test -- --test integration   # nextest filter
cargo burst build -- --release --features=foo
cargo burst clippy -- --all-targets -- -D warnings   # lint args after the second --
```

Common flags on every run-subcommand:
- `--keep-alive SECONDS` — override server idle timer for this run
- `--no-reap` — leave the server (and volume) alive indefinitely
- `--no-fetch` — skip artifact fetch (build, bench)
- `--yes` — skip the first-run "apply suggested rsync excludes?" prompt

## How it works

1. **Image** — A single Hetzner snapshot baked once with Ubuntu 24.04 +
   rustup stable + mold + sccache + cargo-nextest + git + rsync + a `work`
   user authorized with your local SSH public key. Rebuild only when you
   want a newer toolchain.
2. **Volume** — One Hetzner volume per cargo project (keyed on the
   absolute path of the workspace). Holds `CARGO_TARGET_DIR` and the
   sccache cache. ext4, default 200 GB. Auto-deleted after
   `volume_keep_alive_secs` of per-project inactivity.
3. **Server** — Created from the image on first `build`/`test`/`check`/
   `clippy`/`bench`, shared across all your projects, auto-deleted after
   `keep_alive_secs` of inactivity. Servers are billed by the hour while
   alive; deleted servers cost nothing.
4. **Sync** — Source rsync'd up from your workspace (respecting `.gitignore`
   plus a first-run-suggested exclude list). The volume mount and the
   source rsync run concurrently per build to minimise SSH round-trips.
5. **Reapers** — Detached background processes scheduled after each run.
   Re-check the per-project / global idle timer when they wake; if there's
   been activity they bow out and re-spawn a successor for the new deadline.

## Cost (rough)

- CCX63 (48 dedicated cores, 192 GB) at €0.5201/hr in hel1/fsn1
  (€374.49/mo cap = hourly × 720). Singapore is €0.871/hr.
- 30 min of usage per day → ~€8/mo runtime + ~€9/mo for one 200 GB
  volume → ~€17/mo total for a 4× core uplift over a 12-core local
  machine with no thermal throttling.
- Reach for `cargo burst audit --rate 0.5201` after a few sessions
  to see actual cost-so-far against your real usage instead of the
  back-of-envelope number above.
- Volume reaper means inactive projects stop costing volume rent after
  `volume_keep_alive_secs` (default 1h); the next build re-creates the
  volume from scratch (~30s for the cargo fresh-build penalty).

## License

MIT or Apache-2.0, at your option.
