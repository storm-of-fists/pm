#!/usr/bin/env bash
# Deploy the hogs dedicated server to a Linux box (Hostinger VPS).
#
#   deploy/deploy.sh user@your-box-ip
#
# What it does, in order:
#   1. cargo build --release -p hogs (here — the binary is self-contained:
#      SDL statically linked, assets optional with in-code fallbacks)
#   2. rsync the binary + assets + params template to ~/hogs-stage on the box
#   3. install to /opt/hogs, create the `hogs` system user, install the
#      systemd unit, open UDP 48223 in ufw (if ufw is present), restart
#   4. print status + what your friends type to join
#
# Needs: ssh access to the box with sudo, rsync on both ends.
#
# The session PASSWORD is set on the box, not here: edit
# /opt/hogs/hogs.env (HOGS_PASSWORD=...) and `sudo systemctl restart hogs`.
# Hostinger note: if the panel has its own firewall, open UDP 48223
# there too — ufw on the box is not the whole story.
#
# glibc note: the binary is built on THIS machine, so the box needs a
# glibc at least as new (Ubuntu 22.04+ box with a WSL Ubuntu build is
# fine). If the service dies with a GLIBC error: rsync the repo to the
# box and `cargo build --release -p hogs` there instead — everything
# else in this script stays the same.

set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${1:-${HOGS_HOST:-}}"
if [[ -z "$HOST" ]]; then
    echo "usage: deploy/deploy.sh user@box-ip   (or set HOGS_HOST)" >&2
    exit 1
fi
PORT=48223

echo "==> building release"
cargo build --release -p hogs

echo "==> staging to $HOST:~/hogs-stage"
rsync -az --mkpath target/release/hogs "$HOST:hogs-stage/hogs"
rsync -az --mkpath examples/hogs/assets/*.glb "$HOST:hogs-stage/assets/"
rsync -az deploy/hogs.service "$HOST:hogs-stage/hogs.service"

echo "==> installing on the box (sudo)"
ssh "$HOST" bash -s <<REMOTE
set -euo pipefail
sudo useradd --system --home /opt/hogs --shell /usr/sbin/nologin hogs 2>/dev/null || true
sudo mkdir -p /opt/hogs/assets
sudo install -m 755 ~/hogs-stage/hogs /opt/hogs/hogs
sudo cp ~/hogs-stage/assets/*.glb /opt/hogs/assets/
# First deploy only: seed the env file (the password lives HERE).
if [[ ! -f /opt/hogs/hogs.env ]]; then
    echo 'HOGS_PASSWORD=' | sudo tee /opt/hogs/hogs.env > /dev/null
    sudo chmod 600 /opt/hogs/hogs.env
    echo "NOTE: open server — set HOGS_PASSWORD in /opt/hogs/hogs.env to lock it"
fi
sudo chown -R hogs:hogs /opt/hogs
sudo install -m 644 ~/hogs-stage/hogs.service /etc/systemd/system/hogs.service
sudo systemctl daemon-reload
sudo systemctl enable hogs
if command -v ufw > /dev/null; then
    sudo ufw allow $PORT/udp || true
fi
sudo systemctl restart hogs
sleep 1
sudo systemctl --no-pager --lines 5 status hogs || true
REMOTE

BOX_IP="${HOST#*@}"
echo
echo "==> done. friends join with:"
echo "      hogs client addr=$BOX_IP:$PORT password=<the one in /opt/hogs/hogs.env>"
echo "    or type $BOX_IP:$PORT into the menu's address field."
echo "==> watch it:  ssh $HOST journalctl -u hogs -f"
