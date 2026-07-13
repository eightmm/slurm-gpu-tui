#!/usr/bin/env bash
# Deploy the current checkout into the production venv and restart the service.
#
# Development happens in this repo; the systemd unit (sgpu-collector.service)
# runs from a separate prod venv so editing code here never touches the live
# collector. The prod venv must live on the shared FS (see docs/PUSH.md):
# node agents exec the same venv path (bin/sgpu-agent) over NFS.
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
PROD_VENV="${SGPU_PROD_VENV:-/home/shared/sgpu/.venv}"
OWNER="${SGPU_PROD_OWNER:-jaemin}"

# uv lives in the owner's home, not on root's PATH. The prod venv must stay
# owned by $OWNER (the service User=), so when run as root we do the venv work
# via `runuser -u $OWNER` and keep only the systemctl restart as root.
UV="$(command -v uv 2>/dev/null || true)"
[ -z "$UV" ] && [ -x "/home/$OWNER/.local/bin/uv" ] && UV="/home/$OWNER/.local/bin/uv"
[ -n "$UV" ] || { echo "uv not found (looked on PATH and /home/$OWNER/.local/bin)"; exit 1; }

if [ "$(id -u)" = "0" ]; then
    ASUSER=(runuser -u "$OWNER" --)
    SUDO=""
else
    ASUSER=()
    SUDO="sudo"
fi

cd "$REPO"
echo "== tests =="
"${ASUSER[@]}" "$UV" run --project "$REPO" pytest tests/ -q

echo "== install -> $PROD_VENV =="
[ -d "$PROD_VENV" ] || "${ASUSER[@]}" "$UV" venv "$PROD_VENV"
"${ASUSER[@]}" "$UV" pip install --quiet --reinstall-package sgpu --python "$PROD_VENV/bin/python" "$REPO"
"${ASUSER[@]}" "$PROD_VENV/bin/python" -c "import sgpu; print('sgpu', sgpu.__name__, 'ok')"

echo "== restart service =="
$SUDO systemctl restart sgpu-collector
sleep 3
systemctl status sgpu-collector --no-pager -n 5
