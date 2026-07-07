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
# not traverse it, so use /opt/sgpu instead. For push-mode agents prefer a
# shared-FS path via SGPU_INSTALL_DIR (nodes must exec the venv from it).
if [ -n "$SGPU_INSTALL_DIR" ]; then
    DIR="$SGPU_INSTALL_DIR"
elif [ "$(id -u)" = "0" ]; then
    DIR="/opt/sgpu"
else
    DIR="$HOME/.sgpu/app"
fi

if [ -d "$DIR/.git" ]; then
    echo "[bootstrap] updating existing install at $DIR"
    git -C "$DIR" fetch --quiet origin
    git -C "$DIR" reset --hard --quiet origin/main
else
    echo "[bootstrap] installing to $DIR"
    mkdir -p "$(dirname "$DIR")"
    git clone --quiet "$REPO" "$DIR"
fi

exec bash "$DIR/install.sh"
