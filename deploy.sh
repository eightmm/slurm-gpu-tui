#!/usr/bin/env bash
# Deploy the current checkout into the production venv and restart the service.
# Run as root (the service is managed system-wide); works under sudo too.
#
# Development happens in this repo; the systemd unit (sgpu-collector.service)
# runs from a separate prod venv so editing code here never touches the live
# collector. The prod venv lives on the shared FS (see docs/PUSH.md): node
# agents exec the same venv path (bin/sgpu-agent) over NFS.
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
PROD_VENV="${SGPU_PROD_VENV:-/home/shared/sgpu/.venv}"

# uv may not be on root's PATH (it installs under a user's ~/.local/bin).
# Override with SGPU_UV=/path/to/uv when auto-detection misses.
UV="${SGPU_UV:-$(command -v uv 2>/dev/null || true)}"
if [ -z "$UV" ]; then
    for home in /usr/local/bin $(ls -d /home/*/.local/bin 2>/dev/null); do
        [ -x "$home/uv" ] && UV="$home/uv" && break
    done
fi
[ -n "$UV" ] || { echo "uv not found — set SGPU_UV=/path/to/uv"; exit 1; }

# systemctl needs root; if not already root, prefix with sudo.
SUDO=""
[ "$(id -u)" = "0" ] || SUDO="sudo"

cd "$REPO"
echo "== tests =="
"$UV" run --project "$REPO" pytest tests/ -q

echo "== install -> $PROD_VENV =="
[ -d "$PROD_VENV" ] || "$UV" venv "$PROD_VENV"
"$UV" pip install --quiet --reinstall-package sgpu --python "$PROD_VENV/bin/python" "$REPO"
"$PROD_VENV/bin/python" -c "import sgpu; print('sgpu', sgpu.__name__, 'ok')"

echo "== restart service =="
$SUDO systemctl restart sgpu-collector
sleep 3
systemctl status sgpu-collector --no-pager -n 5
