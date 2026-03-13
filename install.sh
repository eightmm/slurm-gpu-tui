#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$INSTALL_DIR/.venv"

echo "=== sgpu installer ==="
echo ""

# 1. Create venv and install
echo "[1/3] Creating venv and installing..."
python3 -m venv "$VENV_DIR"
"$VENV_DIR/bin/pip" install --quiet -e "$INSTALL_DIR"

# 2. Generate wrapper scripts
echo "[2/3] Generating wrapper scripts..."
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

# 3. System-wide install (optional, needs root)
echo "[3/3] Checking system-wide install..."
if [ "$(id -u)" -eq 0 ]; then
    cp "$INSTALL_DIR/bin/sgpu" /usr/local/bin/sgpu
    cp "$INSTALL_DIR/bin/sgpu-collector" /usr/local/bin/sgpu-collector
    chmod +x /usr/local/bin/sgpu /usr/local/bin/sgpu-collector
    echo "  -> Installed to /usr/local/bin/ (all users can use sgpu)"
else
    echo "  -> Skipped (run as root to install system-wide)"
    echo "     sudo cp $INSTALL_DIR/bin/sgpu /usr/local/bin/"
    echo "     sudo cp $INSTALL_DIR/bin/sgpu-collector /usr/local/bin/"
fi

echo ""
echo "=== Done! ==="
echo ""
echo "  sgpu                       # Launch TUI"
echo "  sgpu-collector --daemon    # Start background collector (recommended)"
echo "  sgpu-collector --stop      # Stop collector"
echo "  sgpu-collector --status    # Check collector status"
echo ""
echo "For all users, start the collector as root once:"
echo "  sudo sgpu-collector --daemon"
