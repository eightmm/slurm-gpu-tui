#!/usr/bin/env bash
# Install and control the remote-cluster metrics bridge (user-level, no root).
#
#   grafana/setup-bridge.sh install [user@remote-master]   # set up + start
#   grafana/setup-bridge.sh status      # timer, file age, last fetch result
#   grafana/setup-bridge.sh pause       # stop mirroring; dashboards go
#                                       # "No data" (honest) within ~5 min
#   grafana/setup-bridge.sh resume      # start mirroring again
#   grafana/setup-bridge.sh once        # single fetch right now
#
# install copies sgpu-remote-bridge.sh to ~/.local/bin, the user units to
# ~/.config/systemd/user, writes ~/.config/sgpu/bridge.env (0600) from the
# argument or $SGPU_BRIDGE_REMOTE, and enables the 30s timer. The host
# needs `loginctl enable-linger` for the timer to run without a session.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$HOME/.config/sgpu/bridge.env"
UNIT_DIR="$HOME/.config/systemd/user"

_prefix() {  # metric prefix from env file (default master_)
    grep -s "^SGPU_BRIDGE_PREFIX=" "$ENV_FILE" | cut -d= -f2 | grep . || echo master_
}
_out_file() {
    grep -s "^SGPU_BRIDGE_OUT=" "$ENV_FILE" | cut -d= -f2 | grep . \
        || echo "/tmp/slurm-gpu-tui/$(_prefix)sgpu.prom"
}

case "${1:-}" in
install)
    REMOTE="${2:-${SGPU_BRIDGE_REMOTE:-}}"
    if [ -z "$REMOTE" ] && [ ! -f "$ENV_FILE" ]; then
        echo "usage: $0 install user@remote-master   (or set SGPU_BRIDGE_REMOTE)"; exit 2
    fi
    mkdir -p "$HOME/.local/bin" "$UNIT_DIR" "$HOME/.config/sgpu"
    install -m 755 "$REPO/grafana/sgpu-remote-bridge.sh" "$HOME/.local/bin/"
    install -m 644 "$REPO/sgpu-master-bridge.service" "$REPO/sgpu-master-bridge.timer" "$UNIT_DIR/"
    if [ -n "$REMOTE" ]; then
        printf 'SGPU_BRIDGE_REMOTE=%s\n' "$REMOTE" > "$ENV_FILE"
        chmod 600 "$ENV_FILE"
    fi
    ssh -o BatchMode=yes -o ConnectTimeout=5 \
        "$(grep '^SGPU_BRIDGE_REMOTE=' "$ENV_FILE" | cut -d= -f2)" true \
        || { echo "FAIL: passwordless ssh to the remote does not work"; exit 1; }
    systemctl --user daemon-reload
    systemctl --user enable --now sgpu-master-bridge.timer
    systemctl --user start sgpu-master-bridge.service
    echo "bridge installed; first fetch done:"
    exec "$0" status
    ;;
status)
    systemctl --user is-enabled sgpu-master-bridge.timer 2>/dev/null \
        && systemctl --user list-timers sgpu-master-bridge.timer --no-pager | sed -n 2p \
        || echo "timer: not installed/enabled"
    f=$(_out_file)
    if [ -f "$f" ]; then
        echo "file: $f ($(( $(date +%s) - $(stat -c %Y "$f") ))s old, $(grep -c "^$(_prefix)" "$f" || true) series)"
        grep "^$(_prefix)sgpu_bridge_up" "$f" || true
    else
        echo "file: $f absent (paused or never fetched)"
    fi
    ;;
pause)
    systemctl --user stop sgpu-master-bridge.timer
    rm -f "$(_out_file)"
    echo "paused — series drop out of Prometheus within ~5 min (honest No data)"
    ;;
resume)
    systemctl --user start sgpu-master-bridge.timer
    systemctl --user start sgpu-master-bridge.service
    echo "resumed"
    ;;
once)
    systemctl --user start sgpu-master-bridge.service
    exec "$0" status
    ;;
*)
    echo "usage: $0 install [user@remote-master] | status | pause | resume | once"; exit 2
    ;;
esac
