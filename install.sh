#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$INSTALL_DIR/.venv"

echo "=== sgpu installer ==="
echo ""

# ── Detect sudo availability ───────────────────────────────────────────────
HAS_SUDO=false
if sudo -n true 2>/dev/null; then
    HAS_SUDO=true
fi

# ── Step 0: Ensure uv is available ────────────────────────────────────────
if ! command -v uv &>/dev/null; then
    echo "[0] Installing uv..."
    curl -LsSf https://astral.sh/uv/install.sh | sh
    export PATH="$HOME/.local/bin:$PATH"
fi

# ── Step 1: Create venv and install package ────────────────────────────────
echo "[1] Creating venv and installing..."
uv venv --python 3.12 "$VENV_DIR"
uv pip install --python "$VENV_DIR/bin/python" -e "$INSTALL_DIR"
chmod -R a+rX "$VENV_DIR"

# ── Step 2: Generate wrapper scripts ──────────────────────────────────────
echo "[2] Generating wrapper scripts..."
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

# ── Step 3: Collector daemon ───────────────────────────────────────────────
SERVICE_FILE="$INSTALL_DIR/sgpu-collector.service"
GENERATED_SERVICE="$(mktemp)"
sed "s|ExecStart=.*|ExecStart=$VENV_DIR/bin/sgpu-collector|" "$SERVICE_FILE" > "$GENERATED_SERVICE"

SYSTEMD_MODE="none"

if $HAS_SUDO; then
    echo "[3] Installing systemd service (system-wide)..."
    sudo cp "$GENERATED_SERVICE" /etc/systemd/system/sgpu-collector.service
    sudo systemctl daemon-reload
    sudo systemctl enable sgpu-collector
    sudo systemctl restart sgpu-collector
    SYSTEMD_MODE="system"
else
    USER_SERVICE_DIR="$HOME/.config/systemd/user"
    mkdir -p "$USER_SERVICE_DIR"
    cp "$GENERATED_SERVICE" "$USER_SERVICE_DIR/sgpu-collector.service"
    if systemctl --user daemon-reload 2>/dev/null && \
       systemctl --user enable sgpu-collector 2>/dev/null && \
       systemctl --user start sgpu-collector 2>/dev/null; then
        SYSTEMD_MODE="user"
    else
        rm -f "$USER_SERVICE_DIR/sgpu-collector.service"
        pkill -f sgpu-collector 2>/dev/null || true
        nohup "$VENV_DIR/bin/sgpu-collector" > /tmp/sgpu-collector.log 2>&1 &
        SYSTEMD_MODE="none"
    fi
fi
rm -f "$GENERATED_SERVICE"

# ── Step 4: Make sgpu available in PATH ───────────────────────────────────
echo "[4] Setting up PATH..."

PATH_ADDED=false

if $HAS_SUDO; then
    # System-wide symlink — available to all users immediately
    sudo ln -sf "$INSTALL_DIR/bin/sgpu" /usr/local/bin/sgpu
    sudo ln -sf "$INSTALL_DIR/bin/sgpu-collector" /usr/local/bin/sgpu-collector
else
    # Add bin/ to user's shell config if not already present
    SHELL_RC=""
    if [ -n "$ZSH_VERSION" ] || [ "$(basename "$SHELL")" = "zsh" ]; then
        SHELL_RC="$HOME/.zshrc"
    else
        SHELL_RC="$HOME/.bashrc"
    fi

    PATH_LINE="export PATH=\"$INSTALL_DIR/bin:\$PATH\""
    if ! grep -qF "$INSTALL_DIR/bin" "$SHELL_RC" 2>/dev/null; then
        echo "" >> "$SHELL_RC"
        echo "# sgpu" >> "$SHELL_RC"
        echo "$PATH_LINE" >> "$SHELL_RC"
        PATH_ADDED=true
    fi
    # Also export for the current shell session
    export PATH="$INSTALL_DIR/bin:$PATH"
fi

# ── Done ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Done! ==="
echo ""

# Daemon status
if [ "$SYSTEMD_MODE" = "system" ]; then
    echo "Collector daemon (system service):"
    sudo systemctl status sgpu-collector --no-pager -l || true
elif [ "$SYSTEMD_MODE" = "user" ]; then
    echo "Collector daemon (user service):"
    systemctl --user status sgpu-collector --no-pager -l || true
else
    echo "Collector daemon (background process, PID $(pgrep -f sgpu-collector | head -1)):"
    echo "  Log: /tmp/sgpu-collector.log"
    echo ""
    echo "  NOTE: Add this to $SHELL_RC to auto-start on login:"
    echo "    nohup $VENV_DIR/bin/sgpu-collector > /tmp/sgpu-collector.log 2>&1 &"
fi

echo ""

if $PATH_ADDED; then
    echo "PATH updated in $SHELL_RC."
    echo "Run the following to apply now (or open a new terminal):"
    echo ""
    echo "  source $SHELL_RC && sgpu"
else
    echo "Run: sgpu"
fi

echo ""

# Uninstall instructions
echo "--- Uninstall ---"
if [ "$SYSTEMD_MODE" = "system" ]; then
    echo "  sudo systemctl stop sgpu-collector"
    echo "  sudo systemctl disable sgpu-collector"
    echo "  sudo rm /etc/systemd/system/sgpu-collector.service"
    echo "  sudo rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector"
    echo "  sudo systemctl daemon-reload"
    echo "  rm -rf $INSTALL_DIR"
elif [ "$SYSTEMD_MODE" = "user" ]; then
    echo "  systemctl --user stop sgpu-collector"
    echo "  systemctl --user disable sgpu-collector"
    echo "  rm ~/.config/systemd/user/sgpu-collector.service"
    echo "  systemctl --user daemon-reload"
    echo "  rm -rf $INSTALL_DIR"
else
    echo "  pkill -f sgpu-collector"
    echo "  rm -rf $INSTALL_DIR"
fi
