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

### Option 1: Already set up by your admin

If your sysadmin already installed it, just run:

```bash
sgpu
```

That's it.

### Option 2: Install it yourself

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui

# Create a venv and install
python3 -m venv .venv
source .venv/bin/activate
pip install -e .

# Run
sgpu
```

> **Note**: You need to activate the venv (`source .venv/bin/activate`) before using `sgpu`.

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

## Admin: System-wide Setup

```bash
# 1. Clone and run the installer
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
sudo bash install.sh

# 2. Start the daemon once (root, shared by all users)
sudo sgpu-collector --daemon
```

The installer automatically creates a venv, installs the package, generates wrapper scripts,
and copies them to `/usr/local/bin` when run as root.

After this, every user can simply run `sgpu`.

> **Note**: If you move the install directory, re-run `sudo bash install.sh` to regenerate the wrapper scripts.

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
