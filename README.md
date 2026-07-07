# sgpu - SLURM GPU Monitor

A real-time TUI tool for monitoring GPU usage across your SLURM cluster, right from the terminal.

![Python](https://img.shields.io/badge/python-3.10+-blue)

[한국어 README](README_ko.md)

## What You Get

- Per-node GPU status (utilization, VRAM, temperature, power)
- CPU allocation & memory usage per node
- Who's using which GPU (matched to SLURM jobs)
- Pending job queue with reason codes
- Per-user GPU allocation summary
- Collapsible nodes, idle-only filter, real-time search
- Collector daemon for instant startup — no SSH wait on launch

---

## Installation

> **Already installed on your server?** Just run `sgpu`.

### One-line install / upgrade

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

> **Recommended: run as root (or with passwordless sudo).** That installs a
> system-wide service and `/usr/local/bin/sgpu` for every user on the login
> node. A non-root install works too, but only sets up your own user.

Run the same command again anytime to **upgrade in place**: it resets to the
latest release, rebuilds the venv, restarts the collector, and running node
agents are restarted automatically on the next collector cycle.

Install dir defaults to `~/.sgpu/app` (as root: `/opt/sgpu`, since `/root` is
not readable by other users). To override, put the variable on the `bash` side
of the pipe — pick a shared-filesystem path if you want push-mode agents
(compute nodes must be able to run the venv from it; otherwise SSH-pull mode
is used automatically):

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | SGPU_INSTALL_DIR=/shared/path/sgpu bash
```

The installer detects your environment and handles everything automatically:

| Situation | What the installer does |
|-----------|---------------------|
| **root or sudo** | systemd system service + `/usr/local/bin/sgpu` symlink for all users |
| **no sudo, systemd --user works** | systemd user service (auto-starts on login) + PATH added to shell config |
| **no sudo, no systemd** | background process + PATH added to shell config |

After install, apply PATH changes if prompted:

```bash
source ~/.bashrc   # or open a new terminal
sgpu
```

If sudo was available, the symlink is created automatically — no PATH change needed.

> **Moving the install directory?** Re-run the install command above.

---

## Usage

```bash
sgpu        # Launch the GPU monitor
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `r` | Refresh now |
| `f` | Toggle Fast (1s) / Normal (3s) refresh |
| `s` | Cycle sort: Node → Utilization → User → Free |
| `u` | Toggle "My Jobs" filter (highlight your jobs only) |
| `i` | Toggle idle filter (show only nodes with free GPUs) |
| `d` | Toggle detail columns (Temp / Power / JobID / JobName) |
| `Space` | Collapse / expand node (cursor on node header row) |
| `/` | Search by node name or username — `Esc` to clear |
| `j` / `k` | Move cursor down / up (vim-style) |
| `Enter` | Job / node details popup (`scontrol show`) |
| `e` | Export current snapshot as JSON |
| `?` | Help overlay |
| `q` | Quit |

### One-shot CLI mode

```bash
sgpu --once   # plain-text snapshot (for quick checks / logs)
sgpu --json   # JSON snapshot (for scripts: sgpu --json | jq ...)
```

### Reading the Display

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
▼ node02   ○ alloc  heavy       12/64   ██░░░░░░  48/256G
               0   H100    ████████░  91%   ███████  64/80G   78C   400W   jaemin  67890   10:15h
```

- **Node header row** (dark green): node name, state, partition, CPU alloc/total, RAM bar, plus a per-GPU glyph strip (`█` busy · `▅` parked · `▂` reserved-idle · `▁` free) with busy/free/waste counts — collapse nodes (`Space`) for a one-line-per-node cluster overview
- **FREE chip** (summary bar): total free GPUs and which nodes have them
- **`parked` badge**: VRAM held at ~0% utilization (memory hog, no compute)
- **GPU rows**: indented — utilization bar, VRAM, temperature, power, user, job, time remaining
- **State symbols**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **`user idle 3.2h` marker**: GPU allocated to that user's job but no process running on it, with how long it has sat idle (bold yellow after 1h — reclaim candidates)
- **Stale nodes**: specific error label (e.g., `~timeout`, `~unreachable`, `~smi_err`)

---

## Architecture

```
[sgpu-agent @ each node]  ──3s──→  ~/.sgpu/nodes/<node>.json   (shared FS push)
                                          │
[sgpu-collector @ master] ──merge──→  /tmp/slurm-gpu-tui/data.json
                                          ↑
[sgpu TUI]                ──reads──┘   (instant, no SSH on launch)
```

**Push mode (preferred):** each GPU node runs a tiny resident `sgpu-agent` that writes its own stats to a shared-filesystem directory every few seconds. The collector on the master reads those files locally — no SSH in the hot path, so a flaky sshd or busy node can't stall collection.

**Self-healing:** the collector deploys and repairs agents automatically. If a node's file goes stale (agent died, node rebooted, old agent version), the collector re-launches the agent over SSH — rate-limited per node. No per-node installation needed; the shared venv is executed directly.

**SSH pull fallback:** nodes without a live agent are polled via SSH (ControlMaster-pooled, async per node) exactly as before. The two modes mix freely during migration.

The TUI reads the merged JSON on each refresh — startup is instant regardless of cluster size. Without the collector, the TUI falls back to direct SSH collection (slower first load).

The collector also writes `/tmp/slurm-gpu-tui/metrics.prom` (Prometheus textfile format: GPU util/memory/temp/power, allocation, idle seconds, node health) — point node_exporter's textfile collector or any scraper at it for Grafana dashboards.

---

## Managing the Collector Daemon

### With sudo (system service)

```bash
# Status
sudo systemctl status sgpu-collector

# Restart
sudo systemctl restart sgpu-collector

# Live logs
sudo journalctl -u sgpu-collector -f

# Recent logs
sudo journalctl -u sgpu-collector --since "10 minutes ago"

# Stop / disable
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
```

### Without sudo (user service)

```bash
# Status
systemctl --user status sgpu-collector

# Restart
systemctl --user restart sgpu-collector

# Live logs
journalctl --user -u sgpu-collector -f

# Stop / disable
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
```

### Without sudo (background process)

```bash
# Check if running
pgrep -a -f sgpu-collector

# Live log
tail -f /tmp/sgpu-collector.log

# Stop
pkill -f sgpu-collector
```

---

## Uninstall

> The exact commands are printed at the end of the install run — copy them then.

### With sudo (system service)

```bash
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
sudo rm -f /etc/systemd/system/sgpu-collector.service
sudo rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector
sudo systemctl daemon-reload
rm -rf ~/.sgpu/app    # or your SGPU_INSTALL_DIR
```

### Without sudo (user service)

```bash
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
rm -f ~/.config/systemd/user/sgpu-collector.service
systemctl --user daemon-reload
# Remove the PATH line from ~/.bashrc
rm -rf ~/.sgpu/app    # or your SGPU_INSTALL_DIR
```

### Without sudo (background process)

```bash
pkill -f sgpu-collector
# Remove the nohup and PATH lines from ~/.bashrc
rm -rf ~/.sgpu/app    # or your SGPU_INSTALL_DIR
```

---

## Troubleshooting

**`sgpu` not found**
```bash
ls ~/.sgpu/app/bin/sgpu            # check wrapper exists
export PATH="$HOME/.sgpu/app/bin:$PATH"   # apply manually
```

**Slow startup / "loading GPUs..." on every launch**

The collector daemon is not running. Check its status and restart:
```bash
sudo systemctl status sgpu-collector       # system service
systemctl --user status sgpu-collector    # user service
pgrep -a -f sgpu-collector                # background process
```

**Node shows `~timeout` or `~unreachable`**

SSH from the master node to that compute node is failing:
```bash
ssh <node-name>       # test manually
ssh -v <node-name>    # verbose output
```

**Node shows `~smi_err` or `~no_smi`**

`nvidia-smi` is not working on that node:
```bash
ssh <node-name> nvidia-smi
```

**Collector keeps crashing**
```bash
sudo journalctl -u sgpu-collector -n 50 --no-pager    # system service
journalctl --user -u sgpu-collector -n 50 --no-pager   # user service
cat /tmp/sgpu-collector.log                             # background process
```

**Reinstall cleanly**
```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI refresh interval (seconds) |
| `SLURM_GPU_TUI_FAST_REFRESH_SEC` | `1` | Fast mode refresh interval |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | SSH timeout per node |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | Parallel SSH workers (fallback mode) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | Daemon JSON output directory |

---

## Requirements

- Python 3.10+
- SLURM cluster with `sinfo` / `squeue` available on the master node
- SSH access from the master node to compute nodes (passwordless)
- `nvidia-smi` installed on GPU nodes
