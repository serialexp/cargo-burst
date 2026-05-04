# cargo-burst

Spin up a Hetzner Cloud server on demand to run `cargo build` / `cargo test`
offsite, with a persistent per-project volume that keeps `target/` and the
sccache directory warm between sessions. Server is destroyed after a short
idle window; the volume sticks around at ~€0.04/GB/month.

Designed for the case where your local machine has plenty of cores but
you'd rather not heat the room (or your CPU's thermals) every build.

## Status

Pre-alpha. Linux host preferred (uses `ssh` and `rsync`). macOS hosts work too,
but `cargo burst build` will warn that the fetched binary won't run locally
since the remote is `linux-x86_64` — pass `--no-fetch` if you don't need the
artifact back.

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
hetzner_token = "<your token here>"
region        = "hel1"
server_type   = "ccx63"
keep_alive_secs = 300
volume_gb       = 200
EOF

# 2. One-time: bake an image with rust + mold + sccache (~5-10 min)
cargo burst image build

# 3. Build something
cd ~/Projects/some-rust-thing
cargo burst build --release
```

## How it works

1. **Image** — A single Hetzner snapshot baked once with Ubuntu 24.04 +
   rustup stable + mold + sccache + git + rsync + a `work` user with
   your local SSH public key. Rebuild the image only when you want a
   newer toolchain.
2. **Volume** — One Hetzner volume per cargo project (keyed on the
   absolute path of the workspace). Holds `target/` and sccache's local
   cache. ext4, default 200 GB.
3. **Server** — Created from the image when you first run `build` or
   `test`, stays alive across subsequent invocations, auto-deleted after
   `keep_alive_secs` of inactivity. Server is billed by the hour while
   alive; deleted servers cost nothing.

## Cost (rough)

- CCX63 (48 dedicated cores, 192 GB) at €0.31/hr running cost.
- 30 min of usage per day → ~€5/mo runtime + ~€9/mo for 200 GB volume.
- ~€14/mo total for a 4× core uplift over a 12-core local machine
  with no thermal throttling.

## License

MIT or Apache-2.0, at your option.
