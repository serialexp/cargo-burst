#!/bin/bash
# Cloud-init user-data script for `cargo burst image build`.
#
# This runs on a freshly-provisioned Ubuntu 24.04 Hetzner Cloud server.
# When it finishes, it powers the machine off — the cargo-burst controller
# polls server status and snapshots the disk once the machine is `off`.
#
# The placeholder `__CARGO_BURST_PUBKEY__` is replaced by the controller
# before submission with the user's local SSH public key.

set -euo pipefail
exec > /var/log/cargo-burst-bake.log 2>&1

echo "[cargo-burst] bake started at $(date -u +%FT%TZ)"

# Step 0: make ourselves debuggable BEFORE doing anything that can fail.
#
# (a) Inject the user's SSH public key into root's authorized_keys. Hetzner
#     normally does this via cloud-init's cc_ssh module, but providing our
#     own user-data script can interfere with that on some images. Doing it
#     ourselves means `ssh root@<ip>` works even if everything else breaks.
install -d -m 700 /root/.ssh
cat > /root/.ssh/authorized_keys <<'AUTHKEYS'
__CARGO_BURST_PUBKEY__
AUTHKEYS
chmod 600 /root/.ssh/authorized_keys

# (b) Start a Python HTTP server on :8473 serving /var/log so the bake log
#     and cloud-init log are reachable from a browser. Python 3 ships in
#     the base Ubuntu 24.04 image. nohup + & detaches it from our shell so
#     a `set -e` further down can't kill it.
#
#     We register `.log` as `text/plain` and override `guess_type` so files
#     with no extension (e.g. /var/log/syslog) also serve as text/plain.
#     Plain `python3 -m http.server` falls back to `application/octet-stream`
#     for unknown types, which most browsers turn into a download prompt.
cat > /tmp/cargo-burst-httpd.py <<'PY'
import mimetypes
mimetypes.add_type('text/plain', '.log')
mimetypes.add_type('text/plain', '.txt')
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from functools import partial

class TextyHandler(SimpleHTTPRequestHandler):
    def guess_type(self, path):
        t = super().guess_type(path)
        # Bare /var/log/cloud-init.log etc. or any unknown — show as text.
        return 'text/plain; charset=utf-8' if t in (None, 'application/octet-stream') else t

ThreadingHTTPServer(('', 8473), partial(TextyHandler, directory='/var/log')).serve_forever()
PY
nohup python3 /tmp/cargo-burst-httpd.py \
    > /tmp/cargo-burst-http.log 2>&1 &
echo $! > /tmp/cargo-burst-http.pid
echo "[cargo-burst] log HTTP server started on :8473 (pid $(cat /tmp/cargo-burst-http.pid))"
echo "[cargo-burst] watch progress at: http://<this-server-ip>:8473/cargo-burst-bake.log"

# Disable the systemd timers that periodically run `apt-get update` and
# `unattended-upgrades`. On a freshly-booted Ubuntu 24.04 cloud image these
# fire after a randomized 0-15 minute delay and silently grab the dpkg lock,
# blocking us for 5-10 minutes if we just `pgrep` and wait. We're going to
# snapshot this disk in a few minutes anyway, so background updates are
# pointless here.
echo "[cargo-burst] disabling apt-daily / unattended-upgrades systemd units"
systemctl stop apt-daily.timer apt-daily.service \
                apt-daily-upgrade.timer apt-daily-upgrade.service \
                unattended-upgrades.service 2>/dev/null || true
systemctl mask apt-daily.service apt-daily-upgrade.service \
                unattended-upgrades.service 2>/dev/null || true

# Even after stopping the timers, an apt process may already be in-flight.
# `flock -xn` on the real dpkg lock is the canonical way to wait for it
# (it's what `apt` itself uses) — far more robust than pgrep matching.
echo "[cargo-burst] waiting for dpkg lock…"
wait_loops=0
while ! flock -xn /var/lib/dpkg/lock-frontend true 2>/dev/null; do
    if [ $((wait_loops % 5)) -eq 0 ]; then
        running="$(pgrep -af 'apt|dpkg|unattended' | head -3 | tr '\n' ';' || true)"
        echo "[cargo-burst] still waiting for dpkg lock (loop=$wait_loops): ${running:-none}"
    fi
    wait_loops=$((wait_loops + 1))
    sleep 2
done
echo "[cargo-burst] dpkg lock available; proceeding"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential clang lld curl git rsync ca-certificates pkg-config \
    libssl-dev mold xz-utils

# ── Database services for integration tests ────────────────────────────
#
# Tests that need a real Postgres / MySQL / Redis can hit them on
# localhost on the standard ports. All three bind to 127.0.0.1 only —
# nothing here is exposed to the public internet.
#
# Connection strings (matching the conventions most test fixtures
# expect; documented in README.md too):
#
#   postgres://postgres@localhost:5432/postgres   (no password)
#   mysql://root:root@localhost:3306/
#   redis://localhost:6379/
#
# Data persistence: the data dirs live on the server's root disk, so
# every cold-boot server (after a reap) starts with empty databases.
# Within one server lifetime, DB state survives across `cargo burst
# test` runs. This matches typical integration-test patterns where
# each test creates its own DB or wraps in a transaction. Bart's
# per-project volumes hold target/ + sccache only — DB data does NOT
# follow the volume across reaps.
echo "[cargo-burst] installing database services"
apt-get install -y --no-install-recommends \
    postgresql postgresql-contrib \
    mysql-server \
    redis-server

# ── Postgres ──
#
# Default Ubuntu install creates a `postgres` superuser bound to
# OS-level peer auth on the local socket. We replace pg_hba.conf
# entirely with a config that:
#   - keeps peer auth for the `postgres` OS user (so `sudo -u
#     postgres psql` keeps working for image-build debugging)
#   - trusts every TCP connection from 127.0.0.1 (so test fixtures
#     can connect with no password)
#
# Trust auth on a localhost-only-bound port is fine for ephemeral
# build VMs that aren't reachable from outside. It would NOT be fine
# on a long-running multi-tenant box.
PG_VERSION="$(ls /etc/postgresql | sort -V | tail -n1)"
PG_CONF_DIR="/etc/postgresql/${PG_VERSION}/main"
echo "[cargo-burst] configuring postgres ${PG_VERSION} for trust auth on localhost"
cat > "${PG_CONF_DIR}/pg_hba.conf" <<'PGHBA'
# Written by cargo-burst bake. localhost trust is intentional — the
# image is only used for short-lived per-user build VMs that bind
# postgres to 127.0.0.1.
local   all             postgres                                peer
local   all             all                                     peer
host    all             all             127.0.0.1/32            trust
host    all             all             ::1/128                 trust
PGHBA
chown postgres:postgres "${PG_CONF_DIR}/pg_hba.conf"
chmod 0640 "${PG_CONF_DIR}/pg_hba.conf"
systemctl enable postgresql

# ── MySQL ──
#
# Ubuntu's default mysql-server install creates a `root@localhost`
# user with `auth_socket` plugin (only the OS root user can connect,
# no password). Test fixtures expect a usable password. Switch to
# `caching_sha2_password` and set root/root.
#
# We need mysqld running to ALTER USER, then stop it cleanly so the
# datadir is in a quiescent state for the snapshot.
echo "[cargo-burst] configuring mysql with root/root credentials"
systemctl start mysql
mysql --user=root <<'MYSQL'
ALTER USER 'root'@'localhost' IDENTIFIED WITH caching_sha2_password BY 'root';
FLUSH PRIVILEGES;
MYSQL
systemctl stop mysql
systemctl enable mysql

# ── Redis ──
#
# Stock /etc/redis/redis.conf already binds to 127.0.0.1 with no auth.
# Just enable the unit; nothing to tweak.
echo "[cargo-burst] enabling redis"
systemctl enable redis-server

# Helper script: block until each DB port is accepting connections, or
# 30s elapsed, or the service obviously isn't running. cargo-burst
# subcommands that need DBs (`test`, `bench`) call this before invoking
# cargo, so the first integration test on a freshly-booted server
# doesn't race postgres/mysql startup. On warm runs every port is
# already up, so the loop returns in ~5 ms.
install -d -m 0755 /usr/local/bin
cat > /usr/local/bin/cargo-burst-wait-for-databases <<'WAIT'
#!/bin/bash
# Wait for postgres / mysql / redis to be reachable on localhost.
# Runs at the top of `cargo burst test` and `cargo burst bench`.
# Exits 0 when all three are reachable; warns and exits 0 on timeout
# (the user's code might not need every service — let it fail with
# a real error rather than us masking it).
set -u
deadline=$(( $(date +%s) + 30 ))
ports=(5432 3306 6379)
for port in "${ports[@]}"; do
    while true; do
        if (echo > /dev/tcp/127.0.0.1/$port) 2>/dev/null; then
            break
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "warn: port $port not reachable within 30s; continuing anyway" >&2
            break
        fi
        sleep 0.2
    done
done
WAIT
chmod 0755 /usr/local/bin/cargo-burst-wait-for-databases

# Dedicated build user. Sudo is allowed but not used by the build flow —
# it's there to make manual debugging via `cargo burst shell` painless.
useradd -m -s /bin/bash -G sudo work
echo "work ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/work

install -d -m 700 -o work -g work /home/work/.ssh
cat > /home/work/.ssh/authorized_keys <<'EOF'
__CARGO_BURST_PUBKEY__
EOF
chmod 600 /home/work/.ssh/authorized_keys
chown work:work /home/work/.ssh/authorized_keys

# Rust toolchain: stable, minimal profile (no docs/source). rustup lives
# in /home/work/.rustup; cargo bins land in /home/work/.cargo/bin.
sudo -iu work bash <<'EOFINNER'
set -euo pipefail
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
cat > ~/.cargo/config.toml <<'TOML'
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[build]
# sccache wraps rustc invocations; turning it on by default means the
# very first build on a fresh volume populates the cache without the user
# having to remember to set RUSTC_WRAPPER.
rustc-wrapper = "/usr/local/bin/sccache"
TOML
EOFINNER

# sccache. Static musl build, no shared-lib drama.
SCCACHE_VER="0.8.2"
curl -fsSL \
    "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VER}/sccache-v${SCCACHE_VER}-x86_64-unknown-linux-musl.tar.gz" \
    -o /tmp/sccache.tar.gz
tar -xzf /tmp/sccache.tar.gz -C /tmp
install -m 0755 \
    "/tmp/sccache-v${SCCACHE_VER}-x86_64-unknown-linux-musl/sccache" \
    /usr/local/bin/sccache
rm -rf /tmp/sccache*

# cargo-nextest, since we'll want it for `cargo burst test` later.
sudo -iu work bash <<'EOFINNER'
set -euo pipefail
source ~/.cargo/env
curl -LsSf https://get.nexte.st/latest/linux \
    | tar zxf - -C ~/.cargo/bin
EOFINNER

# Mountpoint + helper script the controller invokes after attach to mount
# the project's volume at /mnt/cache. ext4 mkfs is automatic via the
# Hetzner volume-create flow, so we only mount here.
install -d -m 0755 /mnt/cache
cat > /usr/local/bin/cargo-burst-mount-volume <<'EOF'
#!/bin/bash
# Mount the (single) attached Hetzner volume at /mnt/cache. Idempotent.
set -euo pipefail
target=/mnt/cache
if mountpoint -q "$target"; then
    exit 0
fi
# Hetzner exposes attached volumes as /dev/disk/by-id/scsi-0HC_Volume_<id>.
# We assume there's exactly one volume attached at any given time (the
# project's target/ cache); pick the first match.
dev="$(ls /dev/disk/by-id/scsi-0HC_Volume_* 2>/dev/null | head -n1)"
if [ -z "$dev" ]; then
    echo "no Hetzner volume attached" >&2
    exit 1
fi
# If the volume hasn't been formatted yet (very first attach for a new
# project), give it ext4 now. Hetzner's `format=ext4` create-time option
# usually handles this, but we double-check so manual-volume cases still work.
if ! blkid "$dev" >/dev/null 2>&1; then
    mkfs.ext4 -q -L cargo-burst-cache "$dev"
fi
mount -o noatime "$dev" "$target"
chown work:work "$target"
install -d -o work -g work "$target/target" "$target/sccache"
EOF
chmod 0755 /usr/local/bin/cargo-burst-mount-volume

# Trim slow-but-useless services from the runtime boot path. The image is
# fully baked at this point — cloud-init has nothing to do on subsequent
# boots, and we don't ship anything via snap. On a baked Ubuntu 24.04 image
# `systemd-analyze blame` showed:
#
#   3.318s cloud-init.service
#   1.698s systemd-networkd-wait-online.service
#   1.599s cloud-init-local.service
#     602ms snapd.seeded.service
#
# Disabling all of these shaves ~6s off userspace boot, which means
# `wait_for_ssh` returns ~6s sooner on every cold start.
echo "[cargo-burst] disabling cloud-init for the runtime image"
# Standard cloud-init opt-out: presence of this file makes cloud-init
# exit immediately on every boot.
mkdir -p /etc/cloud
touch /etc/cloud/cloud-init.disabled
# Belt-and-braces: mask the units too. mask is stronger than disable —
# even unit-name aliases can't pull these in.
systemctl mask cloud-init.service cloud-init-local.service \
                cloud-config.service cloud-final.service \
                cloud-init-network.service 2>/dev/null || true

# Pin DNS to 1.1.1.1 / 1.0.0.1 ourselves.
#
# With cloud-init disabled, nothing rewrites /etc/resolv.conf each boot.
# On stock Ubuntu 24.04 cloud images /etc/resolv.conf is a symlink into
# /run/systemd/resolve/stub-resolv.conf (127.0.0.53), which is only useful
# if systemd-resolved has an upstream configured — and that upstream
# normally comes from DHCP options that cloud-init/networkd negotiate at
# boot. With cloud-init off, the symlink target may be stale or empty and
# we get "Could not resolve host" errors mid-build.
#
# Simpler than reviving a slice of cloud-init: replace the symlink with
# a static resolv.conf pointing at Cloudflare. A build server only needs
# to reach the public internet (crates.io, GitHub) — there's no internal
# Hetzner-private DNS we'd lose by skipping DHCP-supplied resolvers.
echo "[cargo-burst] pinning /etc/resolv.conf to 1.1.1.1 / 1.0.0.1"
rm -f /etc/resolv.conf
cat > /etc/resolv.conf <<'RESOLV'
# Static resolvers — written by cargo-burst bake. cloud-init is masked
# on this image, so nothing else will rewrite this file at boot.
nameserver 1.1.1.1
nameserver 1.0.0.1
options edns0 trust-ad
RESOLV
chmod 0644 /etc/resolv.conf

echo "[cargo-burst] removing snapd"
# snapd.seeded.service sits on the critical chain to multi-user.target on
# stock Ubuntu cloud images. We don't use snaps; purging is faster than
# masking and keeps the image smaller.
systemctl stop snapd.service snapd.socket snapd.seeded.service 2>/dev/null || true
apt-get -y purge snapd 2>/dev/null || true
rm -rf /var/cache/snapd /root/snap /home/work/snap

# Mark bake complete.
echo "$(date -u +%FT%TZ)" > /etc/cargo-burst-baked

# Stop the diagnostic HTTP server so it isn't part of the snapshot.
if [ -f /tmp/cargo-burst-http.pid ]; then
    kill "$(cat /tmp/cargo-burst-http.pid)" 2>/dev/null || true
    rm -f /tmp/cargo-burst-http.pid /tmp/cargo-burst-http.log /tmp/cargo-burst-httpd.py
fi

# Shrink the snapshot. apt caches and cloud-init logs would otherwise
# inflate the image by hundreds of MB.
apt-get clean
rm -rf /var/lib/apt/lists/*
cloud-init clean --logs || true

echo "[cargo-burst] bake complete; powering off"
# Schedule the shutdown so cloud-init has time to flush its own state
# before we tip over.
shutdown -h +0
