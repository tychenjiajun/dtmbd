#!/usr/bin/env bash
# One-shot installer for the DTMB → IPTV box (Raspberry Pi OS / Debian arm64).
#   sudo ./deploy/install.sh [names.json]
# Builds the binary if cargo is available, installs the daemon, enables it at boot.
set -euo pipefail

NAMES_SRC="${1:-}"
BIN="${BIN:-}"

if [[ -z "$BIN" ]]; then
  if [[ -x ./target/release/dtmbd ]]; then
    BIN=./target/release/dtmbd
  elif command -v cargo >/dev/null; then
    echo "==> building (first Pi build takes a few minutes)"
    cargo build --release
    BIN=./target/release/dtmbd
  else
    echo "no binary and no cargo: install rust (https://rustup.rs) or copy target/release/dtmbd here" >&2
    exit 1
  fi
fi

echo "==> runtime dependency"
if command -v apt-get >/dev/null; then
  apt-get install -y --no-install-recommends libusb-1.0-0 >/dev/null
fi

echo "==> user + directories"
id -u dtmbd >/dev/null 2>&1 || useradd --system --home /var/lib/dtmbd --shell /usr/sbin/nologin dtmbd
install -d -o dtmbd -g dtmbd -m 0755 /var/lib/dtmbd /etc/dtmbd

echo "==> binary"
install -m 0755 "$BIN" /usr/local/bin/dtmbd

if [[ -n "$NAMES_SRC" ]]; then
  install -m 0644 "$NAMES_SRC" /etc/dtmbd/names.json
  echo "    name patch → /etc/dtmbd/names.json"
fi

echo "==> udev rule (unprivileged USB access)"
install -m 0644 deploy/99-dtmbd.rules /etc/udev/rules.d/99-dtmbd.rules
udevadm control --reload-rules && udevadm trigger

echo "==> systemd unit"
install -m 0644 deploy/dtmbd.service /etc/systemd/system/dtmbd.service
systemctl daemon-reload
systemctl enable --now dtmbd

sleep 2
systemctl --no-pager --lines=15 status dtmbd || true
IP=$(hostname -I | awk '{print $1}')
cat <<MSG

done.  First start scans the band (a few minutes), then:

  playlist:  http://dtmbd.local:8080/playlist.m3u
             http://${IP}:8080/playlist.m3u
  status:    http://${IP}:8080/status.json
  logs:      journalctl -u dtmbd -f
  rescan:    sudo systemctl stop dtmbd && sudo -u dtmbd /usr/local/bin/dtmbd scan \\
                 --names /etc/dtmbd/names.json --save /var/lib/dtmbd/channels.json
MSG
