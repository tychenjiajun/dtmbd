#!/usr/bin/env bash
# One-shot installer for the DTMB → IPTV box (Raspberry Pi OS / Debian arm64).
#   sudo ./deploy/install.sh [names.json]
# Builds the binary if cargo is available, installs the daemon, enables it at boot.
set -euo pipefail

NAMES_SRC="${1:-}"
BIN="${BIN:-}"

if [[ -z "$BIN" ]]; then
  if [[ -x ./target/release/dtmb ]]; then
    BIN=./target/release/dtmb
  elif command -v cargo >/dev/null; then
    echo "==> building (first Pi build takes a few minutes)"
    cargo build --release
    BIN=./target/release/dtmb
  else
    echo "no binary and no cargo: install rust (https://rustup.rs) or copy target/release/dtmb here" >&2
    exit 1
  fi
fi

echo "==> runtime dependency"
if command -v apt-get >/dev/null; then
  apt-get install -y --no-install-recommends libusb-1.0-0 >/dev/null
fi

echo "==> user + directories"
id -u dtmb >/dev/null 2>&1 || useradd --system --home /var/lib/dtmb --shell /usr/sbin/nologin dtmb
install -d -o dtmb -g dtmb -m 0755 /var/lib/dtmb /etc/dtmb

echo "==> binary"
install -m 0755 "$BIN" /usr/local/bin/dtmb

if [[ -n "$NAMES_SRC" ]]; then
  install -m 0644 "$NAMES_SRC" /etc/dtmb/names.json
  echo "    name patch → /etc/dtmb/names.json"
fi

echo "==> udev rule (unprivileged USB access)"
install -m 0644 deploy/99-dtmb.rules /etc/udev/rules.d/99-dtmb.rules
udevadm control --reload-rules && udevadm trigger

echo "==> systemd unit"
install -m 0644 deploy/dtmb.service /etc/systemd/system/dtmb.service
systemctl daemon-reload
systemctl enable --now dtmb

sleep 2
systemctl --no-pager --lines=15 status dtmb || true
IP=$(hostname -I | awk '{print $1}')
cat <<MSG

done.  First start scans the band (a few minutes), then:

  playlist:  http://dtmb.local:8080/playlist.m3u
             http://${IP}:8080/playlist.m3u
  status:    http://${IP}:8080/status.json
  logs:      journalctl -u dtmb -f
  rescan:    sudo systemctl stop dtmb && sudo -u dtmb /usr/local/bin/dtmb scan \\
                 --names /etc/dtmb/names.json --save /var/lib/dtmb/channels.json
MSG
