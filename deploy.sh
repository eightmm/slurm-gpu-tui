#!/usr/bin/env bash
# Verify and deploy the checkout to the local role; never contact other nodes.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "$REPO/scripts/check.sh"
cargo build --manifest-path "$REPO/Cargo.toml" --locked --release --bins -j "${SGPU_BUILD_JOBS:-2}"
exec bash "$REPO/install.sh" --no-build "$@"
