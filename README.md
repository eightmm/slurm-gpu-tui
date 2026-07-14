# sgpu - SLURM GPU Operations Monitor

Real-time SLURM GPU monitoring: a terminal TUI, a collector daemon, push agents
for compute nodes, usage/waste accounting, and Slack alerts.

![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg)
![Python](https://img.shields.io/badge/python-3.10+-blue)
![License](https://img.shields.io/badge/license-MIT-green)

[한국어 README](README_ko.md)

<p align="center"><img src="docs/tab-gpu.svg" alt="sgpu GPU tab" width="100%"></p>

**Wasted-GPU popup (`w`)** — idle / parked / rogue, worst first
<p><img src="docs/waste.svg" alt="waste popup" width="100%"></p>

**CPU tab (`2`)** — every node incl. CPU-only, core allocation, per-user cores
<p><img src="docs/tab-cpu.svg" alt="CPU tab" width="100%"></p>

**Detail columns (`d`)** — temperature, power, JobID, job name
<p><img src="docs/tab-gpu-details.svg" alt="details" width="100%"></p>

**GPU-hours by user (`3`)** — allocation vs actual compute, efficiency %
<p><img src="docs/tab-usage.svg" alt="usage tab" width="100%"></p>


## What You Get

- Per-node GPU status (utilization, VRAM, temperature, power) and CPU/memory
- Who holds which GPU, matched to SLURM jobs — correct even when the driver's
  probe order differs from `/dev/nvidiaN`
- Pending queue with reason codes and estimated start times
- Per-user GPU-hours, efficiency, and wasted (idle/parked) hours with a per-day
  trend — backfilled from slurmdbd so figures survive collector downtime
- Job history and monthly reports (`--jobs`, `--report`)
- Cancel your own jobs (`x`), collapsible nodes, idle filter, live search
- Collector daemon for instant startup (no SSH wait on launch)
- Slack alerts: node down/recovered, GPU health (temp/ECC), wasted/rogue GPUs,
  lost collection — daily thread, English or Korean

## How It Works

`sgpu` runs on a SLURM login/master node with `sinfo`/`squeue` (and optionally
`sacct`) and passwordless SSH to GPU nodes.

```
[sgpu-agent @ each node]  ──3s──→  <AGENT_DIR>/<node>.json   (shared FS push)
                                          │
[sgpu-collector @ master] ──merge──→  /tmp/slurm-gpu-tui/data.json
                                          ↑
[sgpu TUI]                ──reads──┘   (instant, no SSH on launch)
```

- **Push mode (preferred):** each GPU node runs a tiny resident `sgpu-agent`
  that writes stats to a shared-FS directory; the collector reads them locally
  — no SSH in the hot path. The collector deploys and repairs agents itself
  (rate-limited per node); no per-node install.
- **SSH-pull fallback:** nodes without a live agent are polled over SSH
  (ControlMaster-pooled, async). The two modes mix freely.
- CPU-only nodes use low-frequency SSH polling for live RAM telemetry; this is
  shown separately as `cpu-poll` and is not a GPU push fallback.
- The TUI reads the merged JSON, so startup is instant at any cluster size.
  Without a collector it falls back to direct SSH (slower first load).
- The collector also writes `/tmp/slurm-gpu-tui/metrics.prom` (Prometheus
  textfile) — point node_exporter at it, import the bundled dashboard, and load
  the dead-man alert rules for collector failure.
  **→ [docs/GRAFANA.md](docs/GRAFANA.md)**
- `sgpu doctor` is the first check after install or when data looks wrong.

---

## Installation

> **Already installed? Just run `sgpu`.**

One line to install or upgrade in place (resets to latest, rebuilds the venv,
restarts the collector; running agents restart on the next cycle):

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

> **Run as root (or passwordless sudo)** for a system-wide service and
> `/usr/local/bin/sgpu` for every user. A non-root install sets up only your
> own user.

On root installs, the installer detects GPU nodes from Slurm GRES, verifies
them with `nvidia-smi -L`, and installs `sgpu-gpu-persistence.service` on each
reachable node. The oneshot enables NVIDIA persistence mode now and after
reboots, reducing idle-node driver initialization latency. Node provisioning
failures are warnings and do not abort the master install. Set
`SGPU_ENABLE_PERSISTENCE=0` to skip this remote system change.

### Install location (`SGPU_INSTALL_DIR`)

Defaults: `~/.sgpu/app` for a user install; as root, `/home/shared/sgpu` when
`/home/shared` exists (shared FS → **push mode works out of the box**, agent
dir defaults to `/home/shared/sgpu-nodes`), else `/opt/sgpu` (local disk,
SSH-pull). Override by putting the variables on the **`bash` side** of the
pipe. For push mode both paths must be on a shared filesystem the compute
nodes mount at the same path (otherwise SSH-pull is used automatically):

```bash
# local install (SSH-pull mode)
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh \
  | SGPU_INSTALL_DIR=/opt/sgpu bash

# push mode with custom shared-FS paths (root default already does this
# with /home/shared/sgpu + /home/shared/sgpu-nodes)
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh \
  | SGPU_INSTALL_DIR=/nfs/apps/sgpu SLURM_GPU_TUI_AGENT_DIR=/nfs/apps/sgpu-nodes bash
```

The installer bakes those paths into the systemd unit and picks the right
service mode automatically:

| Environment | Service |
|-------------|---------|
| root / sudo | systemd system service + `/usr/local/bin/sgpu` for all users |
| no sudo, systemd `--user` | user service (auto-starts on login) + PATH line |
| no sudo, no systemd | background process + PATH line |

If prompted, apply PATH changes with `source ~/.bashrc` (or a new terminal).
With sudo the symlink is created automatically. Moving the install dir? Re-run
the command above.

> Push mode also needs root→node passwordless SSH and, on `root_squash` NFS, a
> writable agent dir. **→ [docs/PUSH.md](docs/PUSH.md)**

---

## Usage

```bash
sgpu        # launch the GPU monitor
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `1` `2` `3` | Tabs: GPU / CPU / Usage (CPU tab adds CPU-only nodes + per-user core TOP) |
| `r` | Refresh now |
| `s` | Cycle sort: Node → Utilization → User → Free |
| `u` | Filter by user (me first); press again to clear |
| `i` | Free-GPU filter |
| `d` | Toggle detail columns (Temp / Power / JobID / JobName) |
| `Space` | Collapse / expand node |
| `/` | Search by node or username (`Esc` clears) |
| `j` / `k` | Cursor down / up |
| `Enter` | Job / node details (`scontrol show`) |
| `w` | Wasted GPUs popup (idle / parked, worst first) |
| `x` | Cancel the job under the cursor (own jobs, asks first) |
| `e` | Export snapshot as JSON |
| `?` / `q` | Help / quit |

The TUI also pops toasts while open: your jobs starting/finishing, nodes going
down or recovering. (For alerts without a TUI, see Slack Alerts.)

### One-shot CLI

```bash
sgpu --once          # plain-text snapshot
sgpu --version       # installed CLI release
sgpu --json          # JSON snapshot (sgpu --json | jq ...)
sgpu --waste [-v]    # idle/parked/rogue GPUs; exit 1 if any (-v adds Command/WorkDir)
sgpu doctor          # self-diagnosis: data, agents, slurm, sacct, webhook, sharing
sgpu --usage [days]  # per-user GPU-hours + efficiency + waste (default 7d)
sgpu --usage 7 --daily                 # + per-day cluster trend bars
sgpu --jobs [days] [--user U]          # job history: outcomes, GPU-hours, queue waits
sgpu --report [YYYY-MM]                # markdown monthly report
sgpu --wait-free 2 --partition heavy   # block until 2 GPUs free, then exit 0
chkgpu               # one-shot user × node GPU/CPU matrix with next-free ETA
```

`--waste` in a daily cron + mail is a zero-setup hoarding digest;
`--wait-free` lets scripts submit the moment capacity opens.

### Reading the Display

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
```

- **Node header** (green): name, state, partition, CPU alloc/total, RAM bar, and
  a per-GPU glyph strip (`█` busy · `▅` parked · `▂` reserved-idle · `▁` free ·
  `!` rogue) with busy/free/waste counts. Collapse (`Space`) for one line/node.
- **`user !gres` / `user !slurm` (red)**: GPU process with no SLURM allocation
  for that GPU. `!gres` = a job on the node skipped `--gres` (jobid linked in
  the `w` popup); `!slurm` = raw process outside SLURM. Both raise the ROGUE
  chip and top `--waste`. Daemons ignored (`SLURM_GPU_TUI_ROGUE_IGNORE`).
- **`user idle 3.2h`**: GPU allocated but no process running, with idle age
  (bold yellow after 1h — reclaim candidate).
- **`parked` badge**: VRAM held at ~0% util. **FREE chip**: free GPUs + nodes.
- **State symbols**: `●` idle · `◐` mixed · `○` alloc · `✖` drain.
- **Stale nodes**: error label (`~timeout`, `~unreachable`, `~smi_err`).

---

## Slack Alerts

The collector pushes cluster alerts to Slack — node down/recovered, GPU health
(temp/ECC), wasted/rogue GPUs, lost collection — grouped into a daily thread,
English or Korean. Config is `~/.sgpu/webhook.json` (hot-reloaded); the
installer sets it up and `sgpu doctor` shows the active mode.

**→ Full setup, bot/thread mode, all config keys: [docs/ALERTS.md](docs/ALERTS.md)**

---

## Managing the Collector

```bash
# system service (root/sudo)
systemctl status|restart|stop sgpu-collector
journalctl -u sgpu-collector -f

# user service (no sudo)
systemctl --user status|restart|stop sgpu-collector
journalctl --user -u sgpu-collector -f

# background process (no systemd)
pgrep -a -f sgpu-collector      # check
tail -f /tmp/sgpu-collector.log # log
pkill -f sgpu-collector         # stop
```

Deploying from a dev checkout to a separate prod venv? See `deploy.sh`.

## Uninstall

One line — stops the collector and node agents, removes services, symlinks,
data, and the install dir:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
```

Node agents specifically:
```bash
for n in $(sinfo -N -h -o %N | sort -u); do ssh "$n" 'pkill -f "bin/[s]gpu-agent"' 2>/dev/null; done
rm -rf "$SLURM_GPU_TUI_AGENT_DIR"   # default ~/.sgpu/nodes
```

---

## Troubleshooting

| Symptom | Check |
|---------|-------|
| `sgpu` not found | `export PATH="$HOME/.sgpu/app/bin:$PATH"` (or your install dir) |
| Slow startup every launch | collector not running — `systemctl status sgpu-collector` |
| Node `~timeout` / `~unreachable` | `ssh <node>` from master fails — test with `ssh -v <node>` |
| Node `~smi_err` / `~no_smi` | `ssh <node> nvidia-smi` |
| Collector crashing | `journalctl -u sgpu-collector -n 50 --no-pager` (or `/tmp/sgpu-collector.log`) |
| Anything else | `sgpu doctor` |

Reinstall cleanly by re-running the one-line install.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI refresh interval |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector cycle interval |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | SSH timeout per node |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | Parallel SSH workers (fallback mode) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | Daemon JSON output dir |
| `SLURM_GPU_TUI_STATE_DIR` | `~/.sgpu/state` | Persistent state (usage, waste ages, inventory) |
| `SLURM_GPU_TUI_AGENT_DIR` | `~/.sgpu/nodes` | Push-agent payload dir (shared FS for push mode) |
| `SLURM_GPU_TUI_AGENT_SEC` | `3` | Agent collect interval on nodes |
| `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC` | `45` | Agent payload freshness limit |
| `SLURM_GPU_TUI_AGENT_MAX_BYTES` | `1048576` | Maximum accepted agent payload size |
| `SLURM_GPU_TUI_AGENT_REPAIR_SEC` | `180` | Min interval between agent repairs per node |
| `SLURM_GPU_TUI_AGENT_DISABLE` | (unset) | Disable push agents entirely |
| `SLURM_GPU_TUI_WASTE_MIN_SEC` | `600` | Threshold for the waste view / `--waste` |
| `SLURM_GPU_TUI_AUTO_COLLAPSE_NODES` | `12` | Collapse nodes when cluster has ≥ this many GPU nodes |
| `SLURM_GPU_TUI_USAGE_KEEP_DAYS` | `30` | GPU-hour history retention |
| `SLURM_GPU_TUI_SACCT_SEC` | `3600` | slurmdbd alloc backfill interval; `0` disables (auto-disables after 3 failures) |
| `SLURM_GPU_TUI_WEBHOOK_URL` | (unset) | Slack webhook URL shortcut (full config in `~/.sgpu/webhook.json`) |
| `SLURM_GPU_TUI_SLACK_BOT_TOKEN` | (unset) | Slack bot token for daily-thread mode |
| `SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC` | `1800` | Min interval between repeated alerts (same event key) |
| `SLURM_GPU_TUI_WEBHOOK_NAG_SEC` | `21600` | Re-alert interval for standing conditions (waste/rogue/temp/ECC) |
| `SLURM_GPU_TUI_ROGUE_IGNORE` | `root,gdm,xdm` | Users never flagged as rogue |
| `SLURM_GPU_TUI_SHARE_SCRIPTS` | (unset) | Publish every job's batch script to all users in the Enter popup. **Shares script contents (and secrets) with everyone** — installer asks (`SGPU_SHARE_SCRIPTS=0/1` skips) |

Install-time only: `SGPU_INSTALL_DIR` (repo + venv location) and
`SGPU_ENABLE_PERSISTENCE` (`auto`; root installs provision GPU nodes, `0`
disables).

---

## Requirements

- Python 3.10+
- SLURM with `sinfo` / `squeue` on the master node
- Passwordless SSH from master to compute nodes
- `nvidia-smi` on GPU nodes
