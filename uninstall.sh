#!/bin/bash
# One-line uninstaller for sgpu:
#   curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
#
# Removes: collector service (system/user/background), /usr/local/bin
# symlinks, node push agents, runtime data, and the install directory.
set -u

# ── Privileges ──────────────────────────────────────────────────────────────
SUDO="sudo"
HAS_SUDO=false
if [ "$(id -u)" = "0" ]; then
    HAS_SUDO=true
    SUDO=""
elif sudo -n true 2>/dev/null; then
    HAS_SUDO=true
fi

# ── Locate the install dir ──────────────────────────────────────────────────
INSTALL_DIR=""
if [ -n "${SGPU_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="$SGPU_INSTALL_DIR"
elif [ -L /usr/local/bin/sgpu ]; then
    # /usr/local/bin/sgpu -> $DIR/bin/sgpu
    INSTALL_DIR="$(dirname "$(dirname "$(readlink -f /usr/local/bin/sgpu)")")"
else
    for d in /opt/sgpu "$HOME/.sgpu/app"; do
        [ -e "$d/bin/sgpu" ] && INSTALL_DIR="$d" && break
    done
fi

# ── Stop node agents (best effort, needs slurm + ssh) ───────────────────────
if command -v sinfo >/dev/null 2>&1; then
    echo "[uninstall] stopping node agents..."
    for n in $(sinfo -N -h -o %N 2>/dev/null | sort -u); do
        timeout 8 ssh -o BatchMode=yes -o ConnectTimeout=3 "$n" \
            'pkill -f "bin/[s]gpu-agent"; rm -f /tmp/sgpu-agent.lock /tmp/sgpu-agent.log*' \
            >/dev/null 2>&1 &
    done
    wait
fi

# ── Stop collector ──────────────────────────────────────────────────────────
echo "[uninstall] stopping collector..."
if $HAS_SUDO && [ -f /etc/systemd/system/sgpu-collector.service ]; then
    $SUDO systemctl stop sgpu-collector 2>/dev/null
    $SUDO systemctl disable sgpu-collector 2>/dev/null
    $SUDO rm -f /etc/systemd/system/sgpu-collector.service
    $SUDO systemctl daemon-reload
fi
if [ -f "$HOME/.config/systemd/user/sgpu-collector.service" ]; then
    systemctl --user stop sgpu-collector 2>/dev/null
    systemctl --user disable sgpu-collector 2>/dev/null
    rm -f "$HOME/.config/systemd/user/sgpu-collector.service"
    systemctl --user daemon-reload 2>/dev/null
fi
pkill -f "bin/[s]gpu-collector" 2>/dev/null

# ── Remove symlinks, data, install dir ──────────────────────────────────────
if $HAS_SUDO; then
    $SUDO rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector /usr/local/bin/chkgpu /etc/sudoers.d/sgpu
fi
rm -rf "${SLURM_GPU_TUI_DATA_DIR:-/tmp/slurm-gpu-tui}" "$HOME/.sgpu/nodes" \
       "${SLURM_GPU_TUI_STATE_DIR:-$HOME/.sgpu/state}"

if [ -n "$INSTALL_DIR" ] && [ -e "$INSTALL_DIR/bin/sgpu" ]; then
    # Marker check above keeps a bad INSTALL_DIR from deleting the wrong tree
    echo "[uninstall] removing $INSTALL_DIR"
    rm -rf "$INSTALL_DIR"
    rmdir "$HOME/.sgpu" 2>/dev/null  # only if now empty
else
    echo "[uninstall] install dir not found (already removed?)"
fi

# ── PATH line added by install.sh (non-sudo installs) ───────────────────────
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    if [ -n "$INSTALL_DIR" ] && [ -f "$rc" ] && grep -qF "$INSTALL_DIR/bin" "$rc"; then
        sed -i "\\|$INSTALL_DIR/bin|d; /^# sgpu$/d" "$rc"
        echo "[uninstall] cleaned PATH line in $rc"
    fi
done

echo "[uninstall] done"
