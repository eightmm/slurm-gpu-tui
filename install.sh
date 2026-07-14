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
# Keep the old venv restorable: a failed build (network, disk) would otherwise
# leave the still-enabled collector service pointing at a destroyed venv.
# The venv must be rebuilt at its real path (console-script shebangs bake it in).
if [ -d "$VENV_DIR" ]; then
    rm -rf "$VENV_DIR.bak"
    mv "$VENV_DIR" "$VENV_DIR.bak"
fi
_restore_venv() {
    if [ -d "$VENV_DIR.bak" ]; then
        echo "install failed — restoring previous venv" >&2
        rm -rf "$VENV_DIR"
        mv "$VENV_DIR.bak" "$VENV_DIR"
    fi
}
trap _restore_venv ERR
uv venv --python 3.12 "$VENV_DIR"
uv pip install --python "$VENV_DIR/bin/python" -e "$INSTALL_DIR"
trap - ERR
rm -rf "$VENV_DIR.bak"
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
        SUDOERS_TMP="$(mktemp)"
        echo "$(id -un) ALL=(root) NOPASSWD: $SCONTROL_BIN write batch_script *" > "$SUDOERS_TMP"
        # validate before installing — a malformed sudoers.d file breaks sudo host-wide
        if ! command -v visudo >/dev/null || visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
            $SUDO install -m 440 "$SUDOERS_TMP" /etc/sudoers.d/sgpu
            echo "[3a] Script sharing enabled (sudoers.d/sgpu)"
        else
            echo "WARNING: generated sudoers rule failed visudo check — script sharing skipped"
            SHARE=""
        fi
        rm -f "$SUDOERS_TMP"
    else
        echo "NOTE: script sharing needs sudo to provision — skipping."
        SHARE=""
    fi
fi

# Slack alerts config (~/.sgpu/webhook.json). Prompts run on both fresh
# installs and re-installs; every field defaults to its existing value, so
# each answer only overrides what you type (empty = keep). This lets an
# existing install gain a bot token / language without a hand-edit.
# Skip a question with: SGPU_WEBHOOK_URL / _SENDER / _LANG,
# SGPU_SLACK_BOT_TOKEN / _CHANNEL. Non-interactive with none set = no change.
WEBHOOK_CFG="$HOME/.sgpu/webhook.json"
CFG_EXISTS=false; [ -f "$WEBHOOK_CFG" ] && CFG_EXISTS=true
_tty() { [ -r /dev/tty ] && [ -w /dev/tty ]; }

WEBHOOK_URL="${SGPU_WEBHOOK_URL-__ask__}"
if [ "$WEBHOOK_URL" = "__ask__" ]; then
    WEBHOOK_URL=""
    if _tty; then
        if $CFG_EXISTS; then
            printf "Slack webhook URL — Enter to keep existing: " > /dev/tty
        else
            printf "Slack webhook URL for cluster alerts (node down/up) — Enter to skip: " > /dev/tty
        fi
        read -r WEBHOOK_URL < /dev/tty || WEBHOOK_URL=""
    fi
fi

if $CFG_EXISTS || [ -n "$WEBHOOK_URL" ]; then
    SENDER="${SGPU_WEBHOOK_SENDER-__ask__}"
    if [ "$SENDER" = "__ask__" ]; then
        SENDER=""
        _tty && { printf "Sender name shown in alerts (Enter to keep/default AI-master): " > /dev/tty; read -r SENDER < /dev/tty || SENDER=""; }
    fi
    # Optional bot token -> daily-thread grouping (needs chat:write in channel)
    BOT_TOKEN="${SGPU_SLACK_BOT_TOKEN-__ask__}"
    if [ "$BOT_TOKEN" = "__ask__" ]; then
        BOT_TOKEN=""
        _tty && { printf "Slack bot token for daily-thread grouping (xoxb-…, Enter to keep/skip): " > /dev/tty; read -rs BOT_TOKEN < /dev/tty || BOT_TOKEN=""; echo > /dev/tty; }
    fi
    CHANNEL="${SGPU_SLACK_CHANNEL-__ask__}"
    if [ "$CHANNEL" = "__ask__" ]; then
        CHANNEL=""
        _tty && { printf "Channel for threaded alerts (e.g. #gpu-cluster, Enter to keep): " > /dev/tty; read -r CHANNEL < /dev/tty || CHANNEL=""; }
    fi
    LANG_SEL="${SGPU_WEBHOOK_LANG-__ask__}"
    if [ "$LANG_SEL" = "__ask__" ]; then
        LANG_SEL=""
        _tty && { printf "Alert language en/ko (Enter to keep/default en): " > /dev/tty; read -r LANG_SEL < /dev/tty || LANG_SEL=""; }
    fi
    mkdir -p "$HOME/.sgpu"
    # Merge into existing config in Python: only non-empty answers override;
    # unknown/tuned keys (node_health, thresholds, …) are preserved. Seeds
    # sensible defaults for keys absent on a fresh file.
    SGPU_CFG="$WEBHOOK_CFG" NEW_URL="$WEBHOOK_URL" NEW_SENDER="$SENDER" \
    NEW_BOT="$BOT_TOKEN" NEW_CHANNEL="$CHANNEL" NEW_LANG="$LANG_SEL" \
    "$VENV_DIR/bin/python" - << 'PYEOF'
import json, os
p = os.environ["SGPU_CFG"]
try:
    cfg = json.load(open(p))
except Exception:
    cfg = {}
def setval(key, val):
    if val:
        cfg[key] = val
setval("url", os.environ.get("NEW_URL"))
setval("bot_token", os.environ.get("NEW_BOT"))
setval("channel", os.environ.get("NEW_CHANNEL"))
setval("sender_name", os.environ.get("NEW_SENDER"))
lang = os.environ.get("NEW_LANG")
if lang in ("en", "ko"):
    cfg["lang"] = lang
# seed defaults only when absent (don't clobber tuned values)
defaults = {"sender_name": "AI-master", "lang": "en", "node_health": True,
            "down_grace_sec": 180, "collect_alert": True, "collect_grace_sec": 600,
            "waste_alert_hours": 2, "rogue_alert": True,
            "ecc_alert": True, "temp_alert_c": 0,
            "job_done_users": [], "free_gpus_min": 0}
for k, v in defaults.items():
    cfg.setdefault(k, v)
if not cfg.get("url") and not (cfg.get("bot_token") and cfg.get("channel")):
    print("SKIP")  # no delivery configured -> leave file as-is/absent
else:
    json.dump(cfg, open(p, "w"), indent=2)
    print("OK")
PYEOF
    if [ -f "$WEBHOOK_CFG" ]; then
        chmod 600 "$WEBHOOK_CFG"
        echo "[3b] Webhook alerts configured ($WEBHOOK_CFG) — edit it any time; the collector hot-reloads it"
    fi
fi

SERVICE_FILE="$INSTALL_DIR/sgpu-collector.service"
GENERATED_SERVICE="$(mktemp)"
sed -e "s|ExecStart=.*|ExecStart=$VENV_DIR/bin/sgpu-collector|" \
    -e "s|User=.*|User=$(id -un)|" "$SERVICE_FILE" > "$GENERATED_SERVICE"
if [ -n "$SHARE" ] && [ "$SHARE" != "0" ]; then
    sed -i "/^User=/a Environment=SLURM_GPU_TUI_SHARE_SCRIPTS=1" "$GENERATED_SERVICE"
fi
# Root install: default the agent dir to a sibling of the install dir
# (/home/shared/sgpu -> /home/shared/sgpu-nodes) so a shared-FS install gets
# push mode with zero configuration. Root's own default (~root/.sgpu/nodes)
# is useless — mode-700 home, not on the shared FS.
if [ "$(id -u)" = "0" ] && [ -z "${SLURM_GPU_TUI_AGENT_DIR:-}" ]; then
    export SLURM_GPU_TUI_AGENT_DIR="${INSTALL_DIR}-nodes"
fi
# Pre-create it world-writable+sticky: node agents run as root, and under an
# NFS export with root_squash they write as nobody — 1777 keeps push working
# either way (the agent only mkdirs it when missing, it never re-chmods).
if [ -n "${SLURM_GPU_TUI_AGENT_DIR:-}" ]; then
    mkdir -p "$SLURM_GPU_TUI_AGENT_DIR"
    chmod 1777 "$SLURM_GPU_TUI_AGENT_DIR"
fi

# Propagate path overrides into the unit: a systemd service does NOT inherit
# the installing shell's env, so push mode (agents on a shared FS) needs
# SLURM_GPU_TUI_AGENT_DIR baked into the unit, else the collector falls back
# to the local default (~/.sgpu/nodes) and agents can't be reached.
for var in SLURM_GPU_TUI_AGENT_DIR SLURM_GPU_TUI_STATE_DIR SLURM_GPU_TUI_DATA_DIR; do
    val="$(eval "printf '%s' \"\${$var:-}\"")"
    [ -n "$val" ] && sed -i "/^User=/a Environment=$var=$val" "$GENERATED_SERVICE"
done

# Root installs provision persistence mode on actual GPU nodes. Keeping the
# driver initialized avoids multi-second nvidia-smi startup costs on otherwise
# idle/headless nodes. Slurm GRES narrows the candidates; nvidia-smi -L is the
# hardware check, so a stale/misconfigured GRES cannot receive the unit.
#
# The NVIDIA-packaged unit is deliberately left untouched: distributions use
# different flags (often --no-persistence-mode). Our oneshot runs after it and
# reapplies `nvidia-smi -pm 1` on every boot. A node failure is non-fatal to the
# master install. Set SGPU_ENABLE_PERSISTENCE=0 to skip this remote change.
PERSISTENCE_REQUEST="${SGPU_ENABLE_PERSISTENCE:-auto}"
PERSISTENCE_SERVICE="$INSTALL_DIR/sgpu-gpu-persistence.service"
_persistence_requested=false
case "${PERSISTENCE_REQUEST,,}" in
    0|false|no|off) ;;
    auto) [ "$(id -u)" = "0" ] && _persistence_requested=true ;;
    *) _persistence_requested=true ;;
esac

_install_persistence_local() {
    local modes
    if ! command -v nvidia-smi >/dev/null 2>&1 || ! nvidia-smi -L >/dev/null 2>&1; then
        echo "nvidia-smi GPU probe failed"
        return 1
    fi
    if [ "$(id -u)" != "0" ] && ! sudo -n true 2>/dev/null; then
        echo "root or passwordless sudo required"
        return 77
    fi
    $SUDO install -m 0644 "$PERSISTENCE_SERVICE" \
        /etc/systemd/system/sgpu-gpu-persistence.service
    $SUDO systemctl daemon-reload
    if $SUDO systemctl cat nvidia-persistenced.service >/dev/null 2>&1; then
        $SUDO systemctl start nvidia-persistenced.service >/dev/null 2>&1 || true
        $SUDO systemctl enable nvidia-persistenced.service >/dev/null 2>&1 || true
    fi
    $SUDO systemctl enable --now sgpu-gpu-persistence.service >/dev/null
    modes="$(nvidia-smi --query-gpu=persistence_mode --format=csv,noheader 2>/dev/null)" || return
    printf '%s\n' "$modes" | awk '
        BEGIN { count=0; bad=0 }
        { gsub(/^[[:space:]]+|[[:space:]]+$/, ""); count++; if ($0 != "Enabled") bad=1 }
        END { exit !(count > 0 && bad == 0) }
    '
}

if $_persistence_requested; then
    if [ ! -r "$PERSISTENCE_SERVICE" ]; then
        echo "[3c] WARNING: persistence unit template missing: $PERSISTENCE_SERVICE"
    elif ! command -v sinfo >/dev/null 2>&1; then
        echo "[3c] WARNING: sinfo unavailable — GPU persistence provisioning skipped"
    else
        mapfile -t GPU_NODES < <(
            sinfo -h -N -o '%N|%G' 2>/dev/null \
                | awk -F'|' '$2 ~ /(^|,)gpu(:|=|$)/ { print $1 }' \
                | sort -u
        )
        if [ "${#GPU_NODES[@]}" -eq 0 ]; then
            echo "[3c] No GPU nodes found in Slurm GRES — persistence provisioning skipped"
        else
            echo "[3c] Enabling GPU persistence on ${#GPU_NODES[@]} detected node(s)..."
            PERSIST_OK=0
            PERSIST_FAIL=0
            LOCAL_HOST="$(hostname -s)"
            REMOTE_INSTALL_SCRIPT='set -e
as_root() {
    if [ "$(id -u)" = "0" ]; then
        "$@"
    else
        sudo -n "$@"
    fi
}
if [ "$(id -u)" != "0" ] && ! sudo -n true 2>/dev/null; then
    echo "root or passwordless sudo required"
    exit 77
fi
as_root install -m 0644 /dev/stdin /etc/systemd/system/sgpu-gpu-persistence.service
as_root systemctl daemon-reload
if as_root systemctl cat nvidia-persistenced.service >/dev/null 2>&1; then
    as_root systemctl start nvidia-persistenced.service >/dev/null 2>&1 || true
    as_root systemctl enable nvidia-persistenced.service >/dev/null 2>&1 || true
fi
as_root systemctl enable --now sgpu-gpu-persistence.service >/dev/null
modes="$(nvidia-smi --query-gpu=persistence_mode --format=csv,noheader 2>/dev/null)"
printf "%s\n" "$modes" | awk '\''
    BEGIN { count=0; bad=0 }
    { gsub(/^[[:space:]]+|[[:space:]]+$/, ""); count++; if ($0 != "Enabled") bad=1 }
    END { exit !(count > 0 && bad == 0) }
'\'''
            for node in "${GPU_NODES[@]}"; do
                if [ "${node%%.*}" = "$LOCAL_HOST" ]; then
                    if out="$(_install_persistence_local 2>&1)"; then
                        echo "     $node: enabled (local)"
                        PERSIST_OK=$((PERSIST_OK + 1))
                    else
                        echo "     $node: WARNING: ${out:-provisioning failed}"
                        PERSIST_FAIL=$((PERSIST_FAIL + 1))
                    fi
                    continue
                fi
                SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=3)
                if ! probe="$(timeout 8 ssh "${SSH_OPTS[@]}" "$node" \
                        'command -v nvidia-smi >/dev/null && nvidia-smi -L' 2>&1)"; then
                    echo "     $node: WARNING: GPU probe failed: ${probe%%$'\n'*}"
                    PERSIST_FAIL=$((PERSIST_FAIL + 1))
                    continue
                fi
                if out="$(timeout 30 ssh "${SSH_OPTS[@]}" "$node" \
                        "$REMOTE_INSTALL_SCRIPT" < "$PERSISTENCE_SERVICE" 2>&1)"; then
                    echo "     $node: enabled"
                    PERSIST_OK=$((PERSIST_OK + 1))
                else
                    echo "     $node: WARNING: ${out%%$'\n'*}"
                    PERSIST_FAIL=$((PERSIST_FAIL + 1))
                fi
            done
            echo "     persistence summary: $PERSIST_OK enabled, $PERSIST_FAIL skipped/failed"
        fi
    fi
elif [ "${PERSISTENCE_REQUEST,,}" = "auto" ]; then
    echo "[3c] GPU persistence provisioning skipped (automatic only for root installs)"
else
    echo "[3c] GPU persistence provisioning disabled (SGPU_ENABLE_PERSISTENCE=$PERSISTENCE_REQUEST)"
fi

# CPU-only push agents avoid creating a new SSH session when a busy node is
# hardest to reach. They read only /proc/meminfo at a slow interval and are
# kept alive locally by systemd; stale/missing payloads still fall back to the
# collector's existing SSH poll. Automatic provisioning requires a root
# install whose venv and AGENT_DIR are visible at the same path on the node.
# Set SGPU_ENABLE_CPU_PUSH=0 to keep CPU nodes on SSH-only telemetry.
CPU_PUSH_REQUEST="${SGPU_ENABLE_CPU_PUSH:-auto}"
CPU_AGENT_SEC="${SLURM_GPU_TUI_CPU_AGENT_SEC:-20}"
case "$CPU_AGENT_SEC" in
    ''|*[!0-9]*|0) echo "WARNING: invalid SLURM_GPU_TUI_CPU_AGENT_SEC=$CPU_AGENT_SEC; using 20"; CPU_AGENT_SEC=20 ;;
esac
_cpu_push_requested=false
case "${CPU_PUSH_REQUEST,,}" in
    0|false|no|off) ;;
    auto) [ "$(id -u)" = "0" ] && _cpu_push_requested=true ;;
    *) _cpu_push_requested=true ;;
esac

CPU_AGENT_TEMPLATE="$INSTALL_DIR/sgpu-cpu-agent.service"
GENERATED_CPU_AGENT_SERVICE=""
if $_cpu_push_requested; then
    if [ -n "${SLURM_GPU_TUI_AGENT_DISABLE:-}" ]; then
        echo "[3d] CPU push provisioning skipped (SLURM_GPU_TUI_AGENT_DISABLE is set)"
    elif [ -z "${SLURM_GPU_TUI_AGENT_DIR:-}" ]; then
        echo "[3d] WARNING: no shared agent dir — CPU push provisioning skipped"
    elif [ ! -r "$CPU_AGENT_TEMPLATE" ]; then
        echo "[3d] WARNING: CPU agent unit template missing: $CPU_AGENT_TEMPLATE"
    elif ! command -v sinfo >/dev/null 2>&1; then
        echo "[3d] WARNING: sinfo unavailable — CPU push provisioning skipped"
    else
        GENERATED_CPU_AGENT_SERVICE="$(mktemp)"
        sed -e "s|@SGPU_AGENT_BIN@|$VENV_DIR/bin/sgpu-agent|g" \
            -e "s|@SGPU_AGENT_DIR@|$SLURM_GPU_TUI_AGENT_DIR|g" \
            -e "s|@CPU_AGENT_SEC@|$CPU_AGENT_SEC|g" \
            "$CPU_AGENT_TEMPLATE" > "$GENERATED_CPU_AGENT_SERVICE"
        mapfile -t CPU_NODES < <(
            sinfo -h -N -o '%N|%G' 2>/dev/null \
                | awk -F'|' '$2 !~ /(^|,)gpu(:|=|$)/ { print $1 }' \
                | sort -u
        )
        if [ "${#CPU_NODES[@]}" -eq 0 ]; then
            echo "[3d] No CPU-only nodes found — CPU push provisioning skipped"
        else
            echo "[3d] Installing CPU push agents on ${#CPU_NODES[@]} detected node(s)..."
            CPU_PUSH_OK=0
            CPU_PUSH_FAIL=0
            LOCAL_HOST="$(hostname -s)"
            printf -v CPU_AGENT_BIN_Q '%q' "$VENV_DIR/bin/sgpu-agent"
            printf -v CPU_AGENT_DIR_Q '%q' "$SLURM_GPU_TUI_AGENT_DIR"
            REMOTE_CPU_INSTALL='set -e
as_root() {
    if [ "$(id -u)" = "0" ]; then
        "$@"
    else
        sudo -n "$@"
    fi
}
if [ "$(id -u)" != "0" ] && ! sudo -n true 2>/dev/null; then
    echo "root or passwordless sudo required"
    exit 77
fi
as_root install -m 0644 /dev/stdin /etc/systemd/system/sgpu-cpu-agent.service
as_root systemctl daemon-reload
as_root systemctl enable sgpu-cpu-agent.service >/dev/null
as_root systemctl restart sgpu-cpu-agent.service
as_root systemctl is-active --quiet sgpu-cpu-agent.service'
            for node in "${CPU_NODES[@]}"; do
                if [ "${node%%.*}" = "$LOCAL_HOST" ]; then
                    if [ ! -x "$VENV_DIR/bin/sgpu-agent" ] || [ ! -d "$SLURM_GPU_TUI_AGENT_DIR" ]; then
                        echo "     $node: WARNING: shared agent paths unavailable"
                        CPU_PUSH_FAIL=$((CPU_PUSH_FAIL + 1))
                    elif $SUDO install -m 0644 "$GENERATED_CPU_AGENT_SERVICE" \
                            /etc/systemd/system/sgpu-cpu-agent.service \
                            && $SUDO systemctl daemon-reload \
                            && $SUDO systemctl enable sgpu-cpu-agent.service >/dev/null \
                            && $SUDO systemctl restart sgpu-cpu-agent.service \
                            && $SUDO systemctl is-active --quiet sgpu-cpu-agent.service; then
                        echo "     $node: CPU push active (local)"
                        CPU_PUSH_OK=$((CPU_PUSH_OK + 1))
                    else
                        echo "     $node: WARNING: CPU agent service failed"
                        CPU_PUSH_FAIL=$((CPU_PUSH_FAIL + 1))
                    fi
                    continue
                fi
                SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=3)
                if ! probe="$(timeout 8 ssh "${SSH_OPTS[@]}" "$node" \
                        "test -x $CPU_AGENT_BIN_Q && test -d $CPU_AGENT_DIR_Q" 2>&1)"; then
                    echo "     $node: WARNING: shared agent paths unavailable: ${probe%%$'\n'*}"
                    CPU_PUSH_FAIL=$((CPU_PUSH_FAIL + 1))
                    continue
                fi
                if out="$(timeout 30 ssh "${SSH_OPTS[@]}" "$node" \
                        "$REMOTE_CPU_INSTALL" < "$GENERATED_CPU_AGENT_SERVICE" 2>&1)"; then
                    echo "     $node: CPU push active"
                    CPU_PUSH_OK=$((CPU_PUSH_OK + 1))
                else
                    echo "     $node: WARNING: ${out%%$'\n'*}"
                    CPU_PUSH_FAIL=$((CPU_PUSH_FAIL + 1))
                fi
            done
            echo "     CPU push summary: $CPU_PUSH_OK active, $CPU_PUSH_FAIL fallback/skipped"
        fi
        rm -f "$GENERATED_CPU_AGENT_SERVICE"
    fi
elif [ "${CPU_PUSH_REQUEST,,}" = "auto" ]; then
    echo "[3d] CPU push provisioning skipped (automatic only for root installs)"
else
    echo "[3d] CPU push provisioning disabled (SGPU_ENABLE_CPU_PUSH=$CPU_PUSH_REQUEST)"
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
        pkill -f "bin/[s]gpu-collector" 2>/dev/null || true
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
    echo "Collector daemon (background process, PID $(pgrep -f 'bin/[s]gpu-collector' | head -1)):"
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
