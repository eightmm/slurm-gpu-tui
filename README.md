# sgpu - SLURM GPU Monitor

A real-time TUI tool for monitoring GPU usage across your SLURM cluster, right from the terminal.

![Python](https://img.shields.io/badge/python-3.10+-blue)

[한국어 README](README_ko.md)

## What You Get

- Per-node GPU status (utilization, VRAM, temperature, power)
- CPU load & memory usage (accurate values from `/proc/meminfo`)
- Who's using which GPU
- Pending job queue
- Per-user GPU allocation summary

---

## Install

### Already set up by your admin?

```bash
sgpu
```

That's it. If not, follow the steps below.

### Personal Install

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

The installer uses [uv](https://github.com/astral-sh/uv) (auto-installed if not present) to set up a venv and install the package. No `python3-venv` package required.

After installation, activate the venv to use:

```bash
source .venv/bin/activate
sgpu
```

### System-wide Setup (for all users)

Install as a regular user first, then create symlinks with sudo:

```bash
# 1. Install as your user
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh

# 2. Create system-wide symlinks (requires sudo)
sudo ln -sf $(pwd)/bin/sgpu /usr/local/bin/sgpu
sudo ln -sf $(pwd)/bin/sgpu-collector /usr/local/bin/sgpu-collector

# 3. Start the collector daemon (shared by all users)
sudo sgpu-collector --daemon
```

After this, every user can simply run `sgpu` — no venv activation needed.

> **Note**: If you move the install directory, re-run `bash install.sh` and re-create the symlinks.

---

## Usage

```bash
sgpu                # Launch GPU monitor
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `r` | Refresh now |
| `f` | Toggle Fast (1s) / Normal (3s) refresh |
| `e` | Export current snapshot as JSON |
| `q` | Quit |

---

## Collector Daemon (Optional)

Running the collector daemon in the background makes `sgpu` load data instantly.
Without it, `sgpu` still works but the first load may be slower.

```bash
sgpu-collector --daemon    # Start in background
sgpu-collector --status    # Check if running
sgpu-collector --stop      # Stop daemon
```

---

## Environment Variables (Advanced)

Defaults work fine, but you can tweak if needed:

| Variable | Default | Description |
|----------|---------|-------------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI refresh interval (seconds) |
| `SLURM_GPU_TUI_FAST_REFRESH_SEC` | `1` | Fast mode refresh interval |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector daemon interval |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | SSH timeout per node |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | Parallel SSH workers |

---

## Requirements

- Python 3.10+
- SLURM cluster with `sinfo` / `squeue` commands available
- SSH access from the master node to compute nodes (passwordless)
- `nvidia-smi` installed on GPU nodes
