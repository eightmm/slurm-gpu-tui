#!/usr/bin/env bash
# Build and install native binaries on this host only.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD=1
ARGS=()
for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        -h|--help)
            echo "usage: $0 [--no-build] [--role master|node] [--mode gpu|cpu] [--prefix PATH] [--no-service]"
            echo "SGPU_BIN_DIR selects prebuilt binaries. Run node installation locally on each node."
            exit 0 ;;
        *) ARGS+=("$arg") ;;
    esac
done
[ "$(id -u)" = 0 ] || { echo "Run this installer as root (sudo bash scripts/install.sh)" >&2; exit 1; }
BIN_DIR="${SGPU_BIN_DIR:-$REPO/target/release}"
if [ "$BUILD" = 1 ]; then
    command -v cargo >/dev/null || { echo "Rust 1.93+ is required to build; use --no-build with SGPU_BIN_DIR for prebuilt binaries" >&2; exit 1; }
    cargo build --manifest-path "$REPO/Cargo.toml" --locked --release --bins -j "${SGPU_BUILD_JOBS:-2}"
fi
[ -x "$BIN_DIR/sgpu-install" ] || { echo "missing native installer in $BIN_DIR" >&2; exit 1; }
exec "$BIN_DIR/sgpu-install" install --bin-dir "$BIN_DIR" "${ARGS[@]}"
