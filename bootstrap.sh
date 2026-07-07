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
DIR="${SGPU_INSTALL_DIR:-$HOME/.sgpu/app}"

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
