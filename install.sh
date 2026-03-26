#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$INSTALL_DIR/.venv"

echo "=== sgpu installer ==="
echo ""

# 0. Ensure uv is available (install if missing)
if ! command -v uv &>/dev/null; then
    echo "[0/2] Installing uv..."
    curl -LsSf https://astral.sh/uv/install.sh | sh
    export PATH="$HOME/.local/bin:$PATH"
fi

# 1. Create venv and install
echo "[1/2] Creating venv and installing..."
uv venv --python 3.12 "$VENV_DIR"
uv pip install --python "$VENV_DIR/bin/python" -e "$INSTALL_DIR"

# Make venv readable/executable by all users
chmod -R a+rX "$VENV_DIR"

# 2. Generate wrapper scripts
echo "[2/2] Generating wrapper scripts..."
mkdir -p "$INSTALL_DIR/bin"

cat > "$INSTALL_DIR/bin/sgpu" << EOF
#!/bin/bash
exec "$VENV_DIR/bin/sgpu" "\$@"
EOF

cat > "$INSTALL_DIR/bin/sgpu-collector" << EOF
#!/bin/bash
exec "$VENV_DIR/bin/sgpu-collector" "\$@"
EOF

chmod +x "$INSTALL_DIR/bin/sgpu" "$INSTALL_DIR/bin/sgpu-collector"

# 3. Install systemd service
echo "[3/3] Installing systemd service..."
SERVICE_FILE="$INSTALL_DIR/sgpu-collector.service"

# Rewrite ExecStart to use actual venv path
sed "s|ExecStart=.*|ExecStart=$VENV_DIR/bin/sgpu-collector|" "$SERVICE_FILE" \
    | sudo tee /etc/systemd/system/sgpu-collector.service > /dev/null

sudo systemctl daemon-reload
sudo systemctl enable sgpu-collector
sudo systemctl restart sgpu-collector

echo ""
echo "=== Done! ==="
echo ""
echo "Collector daemon status:"
sudo systemctl status sgpu-collector --no-pager -l || true
echo ""
echo "To make sgpu available system-wide, run:"
echo "  sudo ln -sf $INSTALL_DIR/bin/sgpu /usr/local/bin/sgpu"
echo ""
echo "All users can now run: sgpu"
echo "  (daemon auto-starts on boot via systemd)"
