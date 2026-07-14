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

# ── Stop node agents and remove sgpu's node units (best effort) ────────────
# Do not run `nvidia-smi -pm 0`: persistence may have been enabled by the site
# before sgpu was installed. Removing our boot unit preserves that policy.
if command -v sinfo >/dev/null 2>&1; then
    echo "[uninstall] stopping node agents and removing node units..."
    for n in $(sinfo -N -h -o %N 2>/dev/null | sort -u); do
        timeout 8 ssh -o BatchMode=yes -o ConnectTimeout=3 "$n" \
            'as_root() { if [ "$(id -u)" = "0" ]; then "$@"; else sudo -n "$@"; fi; }
             if [ -f /etc/systemd/system/sgpu-cpu-agent.service ]; then
                 as_root systemctl disable --now sgpu-cpu-agent.service >/dev/null 2>&1 || true
                 as_root rm -f /etc/systemd/system/sgpu-cpu-agent.service
             fi
             pkill -f "bin/[s]gpu-agent" 2>/dev/null || true
             rm -f /tmp/sgpu-agent.lock /tmp/sgpu-agent.log*
             if [ -f /etc/systemd/system/sgpu-gpu-persistence.service ]; then
                 as_root systemctl disable --now sgpu-gpu-persistence.service >/dev/null 2>&1 || true
                 as_root rm -f /etc/systemd/system/sgpu-gpu-persistence.service
             fi
             as_root systemctl daemon-reload >/dev/null 2>&1 || true' \
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
# env-supplied dirs are only removed when they look like sgpu's own data
# (marker file check) — a mistyped/hijacked env var must not rm -rf elsewhere
_rm_data_dir() {
    local d="$1" m
    shift
    if [ -z "$d" ] || [ "$d" = "/" ] || [ ! -e "$d" ]; then
        return
    fi
    for m in "$@"; do
        if [ -e "$d/$m" ]; then
            rm -rf "$d"
            return
        fi
    done
    echo "[uninstall] skipping $d (no sgpu files inside — not an sgpu dir?)"
}
_rm_data_dir "${SLURM_GPU_TUI_DATA_DIR:-/tmp/slurm-gpu-tui}" data.json collector.lock
_rm_data_dir "${SLURM_GPU_TUI_STATE_DIR:-$HOME/.sgpu/state}" usage.json idle_state.json inventory.json
rm -rf "$HOME/.sgpu/nodes"

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
