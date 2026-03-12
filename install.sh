#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$INSTALL_DIR/.venv"

# Create venv and install dependencies
python3 -m venv "$VENV_DIR"
"$VENV_DIR/bin/pip" install -r "$INSTALL_DIR/requirements.txt"

# Create wrapper scripts
mkdir -p "$INSTALL_DIR/bin"

cat > "$INSTALL_DIR/bin/sgpu" << EOF
#!/bin/bash
exec "$VENV_DIR/bin/python3" "$INSTALL_DIR/app.py" "\$@"
EOF

cat > "$INSTALL_DIR/bin/sgpu-collector" << EOF
#!/bin/bash
exec "$VENV_DIR/bin/python3" "$INSTALL_DIR/collector.py" "\$@"
EOF

chmod +x "$INSTALL_DIR/bin/sgpu" "$INSTALL_DIR/bin/sgpu-collector"

echo ""
echo "Install complete! To make sgpu available system-wide (requires root):"
echo "  sudo cp $INSTALL_DIR/bin/sgpu /usr/local/bin/"
echo "  sudo cp $INSTALL_DIR/bin/sgpu-collector /usr/local/bin/"
echo ""
echo "Or add to your PATH:"
echo "  export PATH=\"$INSTALL_DIR/bin:\$PATH\""
echo ""
echo "Usage:"
echo "  sgpu-collector --daemon   # Start background collector"
echo "  sgpu                      # Launch TUI"
