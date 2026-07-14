#!/bin/bash
# One-line installer/updater for sgpu:
#   curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
#
# Fresh machine: clones the repo and runs install.sh.
# Already installed: hard-resets to latest origin/main and reinstalls
# (venv, services, PATH links are all refreshed by install.sh).
#
# Install dir: $SGPU_INSTALL_DIR (default: ~/.sgpu/app). For push-mode
# agents pick a shared-filesystem path so compute nodes can exec the venv.
set -e

REPO="https://github.com/eightmm/slurm-gpu-tui.git"

# Default install dir. As root, $HOME is /root (mode 700) — other users could
# not traverse it. Prefer /home/shared/sgpu when that shared-FS dir exists:
# compute nodes can exec the venv from it, which is what enables push-mode
# agents out of the box. Fall back to /opt/sgpu (local disk, SSH-pull only).
# Override either with SGPU_INSTALL_DIR.
if [ -n "$SGPU_INSTALL_DIR" ]; then
    DIR="$SGPU_INSTALL_DIR"
elif [ "$(id -u)" = "0" ] && [ -d /home/shared ]; then
    DIR="/home/shared/sgpu"
elif [ "$(id -u)" = "0" ]; then
    DIR="/opt/sgpu"
else
    DIR="$HOME/.sgpu/app"
fi

if [ -d "$DIR/.git" ]; then
    echo "[bootstrap] updating existing install at $DIR"
    # local edits (site-patched service file, agent tweaks) would be silently
    # destroyed by reset --hard — refuse unless explicitly forced
    if [ -n "$(git -C "$DIR" status --porcelain 2>/dev/null)" ] && [ -z "${SGPU_FORCE_UPDATE:-}" ]; then
        echo "[bootstrap] ERROR: $DIR has local changes:" >&2
        git -C "$DIR" status --short >&2
        echo "[bootstrap] commit/stash them, or re-run with SGPU_FORCE_UPDATE=1 to discard" >&2
        exit 1
    fi
    git -C "$DIR" fetch --quiet origin
    git -C "$DIR" reset --hard --quiet origin/main
else
    echo "[bootstrap] installing to $DIR"
    mkdir -p "$(dirname "$DIR")"
    git clone --quiet "$REPO" "$DIR"
fi

exec bash "$DIR/install.sh"
