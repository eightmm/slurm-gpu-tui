#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$INSTALL_DIR/.venv"

echo "=== sgpu installer ==="
echo ""

# ── Detect privileges (root or passwordless sudo) ──────────────────────────
HAS_SUDO=false
SUDO="sudo"
if [ "$(id -u)" = "0" ]; then
    HAS_SUDO=true
    SUDO=""
elif sudo -n true 2>/dev/null; then
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
# Keep the uv-managed interpreter inside the install dir: the default
# (~/.local/share/uv) is unreadable by other users when installing as root,
# and invisible to compute nodes when the home dir isn't the shared FS.
export UV_PYTHON_INSTALL_DIR="$INSTALL_DIR/python"
uv venv --clear --python 3.12 "$VENV_DIR"
uv pip install --python "$VENV_DIR/bin/python" -e "$INSTALL_DIR"
chmod -R a+rX "$INSTALL_DIR"

# Every path component must be world-traversable or other users can't run
# the /usr/local/bin symlinks (classic trap: install under /root)
p="$INSTALL_DIR"
while [ "$p" != "/" ]; do
    if [ ! -x "$p" ] || ! stat -c %A "$p" | grep -q "x$"; then
        echo ""
        echo "WARNING: $p is not world-traversable — other users won't be able"
        echo "         to run sgpu from here. Reinstall with e.g.:"
        echo "         SGPU_INSTALL_DIR=/opt/sgpu bash install.sh"
        break
    fi
    p="$(dirname "$p")"
done

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

# chkgpu: bundled one-shot user x node GPU/CPU matrix (stdlib only)
cat > "$INSTALL_DIR/bin/chkgpu" << EOF
#!/bin/bash
exec "$VENV_DIR/bin/python" "$INSTALL_DIR/chkgpu" "\$@"
EOF

chmod +x "$INSTALL_DIR/bin/sgpu" "$INSTALL_DIR/bin/sgpu-collector" "$INSTALL_DIR/bin/chkgpu"

# ── Step 3: Collector daemon ───────────────────────────────────────────────

# Batch-script sharing: with a root collector, every user can read every
# job's submit script in the TUI (Enter popup). Asked interactively; set
# SGPU_SHARE_SCRIPTS=1/0 to skip the question. Headless runs default to no.
SHARE="${SGPU_SHARE_SCRIPTS:-}"
if [ -z "$SHARE" ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
    printf "Share all jobs' batch scripts with every user in the TUI? (needs root collector) [Y/n] " > /dev/tty
    read -r ans < /dev/tty || ans=""
    case "$ans" in n|N|no) SHARE="" ;; *) SHARE=1 ;; esac
fi

if [ -n "$SHARE" ] && [ "$SHARE" != "0" ]; then
    if $HAS_SUDO; then
        # Narrow sudoers grant: the collector user may run exactly
        # 'scontrol write batch_script' as root — nothing else. This keeps
        # the collector (and its push agents / state paths) non-root.
        SCONTROL_BIN="$(command -v scontrol || echo /usr/bin/scontrol)"
        echo "$(id -un) ALL=(root) NOPASSWD: $SCONTROL_BIN write batch_script *" \
            | $SUDO tee /etc/sudoers.d/sgpu >/dev/null
        $SUDO chmod 440 /etc/sudoers.d/sgpu
        echo "[3a] Script sharing enabled (sudoers.d/sgpu)"
    else
        echo "NOTE: script sharing needs sudo to provision — skipping."
        SHARE=""
    fi
fi

# Slack-compatible webhook for cluster alerts (node down/recovered).
# Asked interactively; set SGPU_WEBHOOK_URL to skip the question
# (SGPU_WEBHOOK_URL="" skips with no webhook). Existing config is kept
# unless a new URL is entered.
WEBHOOK_CFG="$HOME/.sgpu/webhook.json"
WEBHOOK_URL="${SGPU_WEBHOOK_URL-__ask__}"
if [ "$WEBHOOK_URL" = "__ask__" ]; then
    WEBHOOK_URL=""
    if [ -r /dev/tty ] && [ -w /dev/tty ]; then
        if [ -f "$WEBHOOK_CFG" ]; then
            printf "Slack webhook for cluster alerts: config exists — new URL to replace, Enter to keep: " > /dev/tty
        else
            printf "Slack webhook URL for cluster alerts (node down/up) — Enter to skip: " > /dev/tty
        fi
        read -r WEBHOOK_URL < /dev/tty || WEBHOOK_URL=""
    fi
fi
if [ -n "$WEBHOOK_URL" ]; then
    mkdir -p "$HOME/.sgpu"
    cat > "$WEBHOOK_CFG" << WEOF
{
  "url": "$WEBHOOK_URL",
  "sender_name": "AI-master",
  "node_health": true,
  "job_done_users": [],
  "free_gpus_min": 0
}
WEOF
    chmod 600 "$WEBHOOK_CFG"
    echo "[3b] Webhook alerts configured ($WEBHOOK_CFG) — edit it any time; the collector hot-reloads it"
elif [ -f "$WEBHOOK_CFG" ]; then
    echo "[3b] Keeping existing webhook config ($WEBHOOK_CFG)"
fi

SERVICE_FILE="$INSTALL_DIR/sgpu-collector.service"
GENERATED_SERVICE="$(mktemp)"
sed -e "s|ExecStart=.*|ExecStart=$VENV_DIR/bin/sgpu-collector|" \
    -e "s|User=.*|User=$(id -un)|" "$SERVICE_FILE" > "$GENERATED_SERVICE"
if [ -n "$SHARE" ] && [ "$SHARE" != "0" ]; then
    sed -i "/^User=/a Environment=SLURM_GPU_TUI_SHARE_SCRIPTS=1" "$GENERATED_SERVICE"
fi

SYSTEMD_MODE="none"

if $HAS_SUDO; then
    echo "[3] Installing systemd service (system-wide)..."
    $SUDO cp "$GENERATED_SERVICE" /etc/systemd/system/sgpu-collector.service
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable sgpu-collector
    $SUDO systemctl restart sgpu-collector
    SYSTEMD_MODE="system"
else
    USER_SERVICE_DIR="$HOME/.config/systemd/user"
    mkdir -p "$USER_SERVICE_DIR"
    cp "$GENERATED_SERVICE" "$USER_SERVICE_DIR/sgpu-collector.service"
    if systemctl --user daemon-reload 2>/dev/null && \
       systemctl --user enable sgpu-collector 2>/dev/null && \
       systemctl --user restart sgpu-collector 2>/dev/null; then
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
    # System-wide symlinks — available to all users immediately
    $SUDO ln -sf "$INSTALL_DIR/bin/sgpu" /usr/local/bin/sgpu
    $SUDO ln -sf "$INSTALL_DIR/bin/sgpu-collector" /usr/local/bin/sgpu-collector
    $SUDO ln -sf "$INSTALL_DIR/bin/chkgpu" /usr/local/bin/chkgpu
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
    $SUDO systemctl status sgpu-collector --no-pager -l || true
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
echo "--- Uninstall (one line) ---"
echo "  curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash"
