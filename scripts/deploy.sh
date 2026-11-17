#!/usr/bin/env bash
# Verify the checkout, then install and reconcile node publishers for a master.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "Run this command as root" >&2; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bash "$REPO/scripts/check.sh"
cargo build --manifest-path "$REPO/Cargo.toml" --locked --release --bins -j "${SGPU_BUILD_JOBS:-2}"
exec bash "$REPO/scripts/install.sh" --no-build "$@"
