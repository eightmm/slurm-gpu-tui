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

### With sudo (System-wide, recommended for admins)

Installs the collector as a systemd service so it starts automatically on boot, and makes `sgpu` available to every user on the system.

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

`install.sh` handles everything automatically:
1. Creates a Python 3.12 venv via [uv](https://github.com/astral-sh/uv) (auto-installed if missing)
2. Installs the package into the venv
3. Generates wrapper scripts in `bin/`
4. Installs and starts `sgpu-collector` as a systemd service (requires sudo)

After that, make `sgpu` available system-wide:

```bash
sudo ln -sf $(pwd)/bin/sgpu /usr/local/bin/sgpu
```

Every user can now run `sgpu` directly — no venv activation needed.

> **Note:** If you move the install directory, re-run `bash install.sh` and recreate the symlink.

---

### Without sudo (Personal install)

If you don't have sudo access, install for your own account only.

#### Step 1 — Install

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui

# Install uv if not available
curl -LsSf https://astral.sh/uv/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"

# Create venv and install
uv venv --python 3.12 .venv
uv pip install --python .venv/bin/python -e .
```

#### Step 2 — Add to PATH

```bash
# Add to your shell config (~/.bashrc or ~/.zshrc)
echo 'export PATH="$HOME/slurm-gpu-tui/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

Replace `$HOME/slurm-gpu-tui` with your actual clone path.

#### Step 3 — Start the collector daemon

**Option A: Run manually in the background**

```bash
nohup bin/sgpu-collector > /tmp/sgpu-collector.log 2>&1 &
```

Add this to your `~/.bashrc` or a startup script so it persists across logins.

**Option B: Use systemd user service** (if your system supports it)

```bash
# Edit the service file to set the correct ExecStart path
sed "s|ExecStart=.*|ExecStart=$(pwd)/.venv/bin/sgpu-collector|" sgpu-collector.service \
  > ~/.config/systemd/user/sgpu-collector.service

systemctl --user daemon-reload
systemctl --user enable sgpu-collector
systemctl --user start sgpu-collector

# Check status
systemctl --user status sgpu-collector
```

> **Without the daemon:** `sgpu` still works — it falls back to direct SSH collection. The first load will be slower (a few seconds per node).

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
| `Space` | Collapse / expand node (cursor must be on node header) |
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

- **Node header row** (dark green): shows node name, state, partition, CPU alloc/total, RAM
- **GPU rows**: indented under the node header — utilization bar, VRAM, temperature, power, user, job
- **State symbols**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **Stale nodes**: show error label instead of `~stale` (e.g., `~timeout`, `~unreachable`, `~smi_err`)

---

## Architecture

```
[sgpu-collector]  ──→  /tmp/slurm-gpu-tui/data.json
                              ↑
[sgpu TUI]        ──reads──┘   (instant, no SSH on launch)
```

The collector daemon runs continuously in the background, polling SLURM and all GPU nodes via SSH. It writes a fresh JSON snapshot every few seconds. The TUI reads this file on each refresh — startup is instant regardless of cluster size.

If the collector is not running, the TUI falls back to direct SSH collection (slower first load).

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
