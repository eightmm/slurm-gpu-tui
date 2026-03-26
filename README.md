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

### One-line install (auto-detects sudo)

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

`install.sh` detects your environment and handles everything automatically:

| Situation | What install.sh does |
|-----------|---------------------|
| **sudo available** | systemd system service + `/usr/local/bin/sgpu` symlink for all users |
| **no sudo, systemd --user works** | systemd user service (auto-starts on login) + PATH added to shell config |
| **no sudo, no systemd** | background process + PATH added to shell config |

After install, apply PATH changes if prompted:

```bash
source ~/.bashrc   # or open a new terminal
sgpu
```

If sudo was available, the symlink is created automatically — no PATH change needed.

> **Moving the install directory?** Re-run `bash install.sh`.

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
| `s` | Cycle sort: Node → Utilization → User |
| `u` | Toggle "My Jobs" filter (highlight your jobs only) |
| `i` | Toggle idle filter (show only nodes with free GPUs) |
| `d` | Toggle detail columns (Temp / Power / JobID / JobName) |
| `Space` | Collapse / expand node (cursor on node header row) |
| `/` | Search by node name or username — `Esc` to clear |
| `j` / `k` | Move cursor down / up (vim-style) |
| `e` | Export current snapshot as JSON |
| `q` | Quit |

### Reading the Display

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   hklee   12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
▼ node02   ○ alloc  heavy       12/64   ██░░░░░░  48/256G
               0   H100    ████████░  91%   ███████  64/80G   78C   400W   jaemin  67890   10:15h
```

- **Node header row** (dark green): node name, state symbol, partition, CPU alloc/total, RAM bar
- **GPU rows**: indented — utilization bar, VRAM, temperature, power, user, job, time remaining
- **State symbols**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **Stale nodes**: specific error label (e.g., `~timeout`, `~unreachable`, `~smi_err`)

---

## Architecture

```
[sgpu-collector]  ──→  /tmp/slurm-gpu-tui/data.json
                              ↑
[sgpu TUI]        ──reads──┘   (instant, no SSH on launch)
```

The collector daemon runs continuously in the background, polling SLURM and GPU nodes via SSH. The TUI reads its JSON output on each refresh — startup is instant regardless of cluster size.

Without the daemon, the TUI falls back to direct SSH collection (slower first load).

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

> The exact commands are printed at the end of `install.sh` — copy them then.

### With sudo (system service)

```bash
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
sudo rm -f /etc/systemd/system/sgpu-collector.service
sudo rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector
sudo systemctl daemon-reload
rm -rf /path/to/slurm-gpu-tui
```

### Without sudo (user service)

```bash
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
rm -f ~/.config/systemd/user/sgpu-collector.service
systemctl --user daemon-reload
# Remove the PATH line from ~/.bashrc
rm -rf /path/to/slurm-gpu-tui
```

### Without sudo (background process)

```bash
pkill -f sgpu-collector
# Remove the nohup and PATH lines from ~/.bashrc
rm -rf /path/to/slurm-gpu-tui
```

---

## Troubleshooting

**`sgpu` not found**
```bash
ls ~/slurm-gpu-tui/bin/sgpu        # check wrapper exists
export PATH="$HOME/slurm-gpu-tui/bin:$PATH"   # apply manually
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
bash install.sh    # safe to re-run, overwrites venv and service
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
