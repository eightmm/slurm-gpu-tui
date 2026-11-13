#!/usr/bin/env bash
# Remove only the selected local role; preserve data and shared payloads.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${SGPU_INSTALL_DIR:-}"
if [ -n "$PREFIX" ] && [ -x "$PREFIX/bin/sgpu-install" ]; then
    INSTALLER="$PREFIX/bin/sgpu-install"
elif command -v sgpu-install >/dev/null; then
    INSTALLER="$(command -v sgpu-install)"
elif [ -x "${SGPU_BIN_DIR:-$REPO/target/release}/sgpu-install" ]; then
    INSTALLER="${SGPU_BIN_DIR:-$REPO/target/release}/sgpu-install"
else
    echo "native installer not found; set SGPU_INSTALL_DIR or SGPU_BIN_DIR" >&2
    exit 1
fi
exec "$INSTALLER" uninstall "$@"
