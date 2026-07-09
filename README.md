# sgpu - SLURM GPU Monitor

A real-time TUI tool for monitoring GPU usage across your SLURM cluster, right from the terminal.

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

- Per-node GPU status (utilization, VRAM, temperature, power)
- CPU allocation & memory usage per node
- Who's using which GPU (matched to SLURM jobs, correct even when the driver's
  probe order differs from `/dev/nvidiaN`)
- Pending job queue with reason codes and estimated start times
- Per-user GPU-hours, efficiency, and wasted (idle/parked) hours, with a
  per-day cluster trend — backfilled from slurmdbd so figures survive
  collector downtime
- Job history and monthly reports (outcomes, GPU-hours, queue waits) — `--jobs`, `--report`
- Cancel your own jobs from the TUI (`x`)
- Collapsible nodes, idle-only filter, real-time search
- Collector daemon for instant startup — no SSH wait on launch
- Slack alerts: node down/recovered, GPU health (temp/ECC), wasted/rogue GPUs,
  and lost collection — grouped into a daily thread, in English or Korean

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
| `1` `2` `3` | Switch tabs: GPU / CPU / Usage — the CPU tab includes CPU-only nodes, with a cluster core summary and per-user core TOP |
| `r` | Refresh now |
| `s` | Cycle sort: Node → Utilization → User → Free |
| `u` | Filter by user — pick from a list (me first); press again to clear |
| `i` | Free-GPU filter (only nodes with truly free GPUs) |
| `d` | Toggle detail columns (Temp / Power / JobID / JobName) |
| `Space` | Collapse / expand node (cursor on node header row) |
| `/` | Search by node name or username — `Esc` to clear |
| `j` / `k` | Move cursor down / up (vim-style) |
| `Enter` | Job / node details popup (`scontrol show`) |
| `w` | Wasted GPUs popup (idle / parked, worst first) |
| `x` | Cancel the job under the cursor (own jobs only, asks first) |
| `g` | Open the Usage tab (GPU-hours by user) |
| `e` | Export current snapshot as JSON |
| `?` | Help overlay |
| `q` | Quit |

While the TUI is open it also pops toast notifications: your jobs starting/finishing, and nodes going down or recovering. (For alerts without a TUI open, see the webhook config in Environment Variables.)

### One-shot CLI mode

```bash
sgpu --once          # plain-text snapshot (for quick checks / logs)
sgpu --json          # JSON snapshot (for scripts: sgpu --json | jq ...)
sgpu --waste [-v]    # idle/parked/rogue GPUs; exit 1 if any — -v adds Command/WorkDir
sgpu doctor          # self-diagnosis: data freshness, agents, slurm, sacct, webhook, sharing
sgpu --usage [days]  # per-user GPU-hours + efficiency + waste (default 7 days)
sgpu --usage 7 --daily                 # adds per-day cluster GPU-hours trend bars
sgpu --jobs [days] [--user U]          # job history: outcomes, GPU-hours sunk, queue waits
sgpu --report [YYYY-MM]                # markdown monthly report (users, trend, outcomes, waits)
sgpu --wait-free 2 --partition heavy   # block until 2 GPUs are free, then exit 0
chkgpu               # classic one-shot user x node GPU/CPU matrix with per-node next-free ETA
```

`--waste` in a daily cron + mail is a zero-setup GPU-hoarding digest.
`--wait-free` lets scripts submit the moment capacity opens.

### Reading the Display

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
▼ node02   ○ alloc  heavy       12/64   ██░░░░░░  48/256G
               0   H100    ████████░  91%   ███████  64/80G   78C   400W   jaemin  67890   10:15h
```

- **Node header row** (dark green): node name, state, partition, CPU alloc/total, RAM bar, plus a per-GPU glyph strip (`█` busy · `▅` parked · `▂` reserved-idle · `▁` free · `!` rogue) with busy/free/waste counts — collapse nodes (`Space`) for a one-line-per-node cluster overview
- **`user !gres` / `user !slurm` markers (red)**: GPU process with **no SLURM allocation for that GPU**. `!gres` = the user has a job on the node that skipped `--gres` (jobid linked in the `w` popup); `!slurm` = raw process outside SLURM entirely. Both raise the red ROGUE chip and top the `--waste` list. System daemons are ignored (`SLURM_GPU_TUI_ROGUE_IGNORE`, default `root,gdm,xdm`)
- **FREE chip** (summary bar): total free GPUs and which nodes have them
- **`parked` badge**: VRAM held at ~0% utilization (memory hog, no compute)
- **GPU rows**: indented — utilization bar, VRAM, temperature, power, user, job, time remaining
- **State symbols**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **`user idle 3.2h` marker**: GPU allocated to that user's job but no process running on it, with how long it has sat idle (bold yellow after 1h — reclaim candidates)
- **Stale nodes**: specific error label (e.g., `~timeout`, `~unreachable`, `~smi_err`)

---

## Slack Alerts

The collector can push cluster alerts to Slack. Config lives in
`~/.sgpu/webhook.json` (root's `/root/.sgpu/webhook.json` for a system
service) and is **hot-reloaded** — edit and save, no restart needed. The
installer offers to set it up; `sgpu doctor` shows the active mode.

**Setup.** Two delivery modes:

- **Incoming webhook** (simplest): create a Slack incoming webhook, paste the
  `https://hooks.slack.com/services/...` URL when the installer asks. Alerts
  post as individual messages.
- **Bot mode** (recommended): create a Slack app with the `chat:write` scope,
  install it, invite it to the channel (`/invite @your-bot`), and give the
  installer the bot token (`xoxb-...`) and channel. Alerts then post as
  replies under **one parent message per day** (`📅 GPU cluster alerts — date`),
  keeping the channel tidy. Incoming webhooks cannot thread.

Non-interactive: `SGPU_WEBHOOK_URL`, `SGPU_WEBHOOK_SENDER`, `SGPU_WEBHOOK_LANG`,
`SGPU_SLACK_BOT_TOKEN`, `SGPU_SLACK_CHANNEL` skip the matching prompts.

**Config keys** (`~/.sgpu/webhook.json`):

| Key | Default | Meaning |
|-----|---------|---------|
| `url` | — | Slack incoming-webhook URL |
| `bot_token` / `channel` | — | Bot token + channel → daily-thread grouping |
| `sender_name` | `AI-master` | Identity shown in every alert (set per cluster to tell them apart) |
| `lang` | `en` | Alert language: `en` or `ko` |
| `node_health` | `true` | Node down/recovered, judged by **SLURM state** (not sgpu's own SSH/collection errors) |
| `down_grace_sec` | `180` | Down must persist this long before alerting (rides out cold starts / blips) |
| `collect_alert` | `true` | A GPU node that was reporting goes blind (dead agent / hung node) while SLURM still shows it up |
| `collect_grace_sec` | `600` | Blind must persist this long (longer than the agent auto-repair cycle) |
| `waste_alert_hours` | `2` | GPU idle/parked ≥ N hours (`0` = off) |
| `rogue_alert` | `true` | GPU used outside SLURM |
| `temp_alert_c` | `0` | GPU temperature ≥ N °C (`0` = off; ~90 typical) |
| `ecc_alert` | `true` | Uncorrectable ECC errors — silent hardware failure; alert carries UUID / PCI bus / serial for RMA |
| `job_done_users` | `[]` | Notify when these users' jobs finish |
| `free_gpus_min` | `0` | Notify when free-GPU count reaches N (`0` = off) |

Repeated conditions are debounced (30 min for events, 6 h for standing
conditions like waste/temp/ECC). Node-health and collection alerts are for
**GPU nodes and SLURM state only** — SSH-pull clusters routinely can't reach
CPU/GPU-less nodes, and those collection failures never raise a false "down".

The TUI shows the same job/node transitions as toasts while it's open, so you
don't need Slack for at-the-terminal awareness.

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

One line — stops the collector and node agents, removes services, symlinks,
data, and the install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
```

<details>
<summary>Manual steps (what the script does)</summary>

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

### Node agents (all modes)

```bash
# Stop push agents and remove their data
for n in $(sinfo -N -h -o %N | sort -u); do ssh "$n" 'pkill -f "bin/[s]gpu-agent"' 2>/dev/null; done
rm -rf ~/.sgpu/nodes
```

### Without sudo (background process)

```bash
pkill -f sgpu-collector
# Remove the nohup and PATH lines from ~/.bashrc
rm -rf ~/.sgpu/app    # or your SGPU_INSTALL_DIR
```

</details>

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
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector cycle interval |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | SSH timeout per node |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | Parallel SSH workers (fallback mode) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | Daemon JSON output directory |
| `SLURM_GPU_TUI_STATE_DIR` | `~/.sgpu/state` | Persistent state (usage history, waste ages, inventory) — survives reboots |
| `SLURM_GPU_TUI_AGENT_DIR` | `~/.sgpu/nodes` | Push-agent payload directory (shared FS) |
| `SLURM_GPU_TUI_AGENT_SEC` | `3` | Agent collect interval on nodes |
| `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC` | `45` | Agent payload freshness limit |
| `SLURM_GPU_TUI_AGENT_REPAIR_SEC` | `180` | Min interval between agent repairs per node |
| `SLURM_GPU_TUI_AGENT_DISABLE` | (unset) | Set to disable push agents entirely |
| `SLURM_GPU_TUI_WASTE_MIN_SEC` | `600` | Threshold for the waste view / `--waste` |
| `SLURM_GPU_TUI_AUTO_COLLAPSE_NODES` | `12` | Start with nodes collapsed when the cluster has at least this many GPU nodes |
| `SLURM_GPU_TUI_USAGE_KEEP_DAYS` | `30` | GPU-hour history retention |
| `SLURM_GPU_TUI_SACCT_SEC` | `3600` | slurmdbd (sacct) alloc backfill interval; `0` disables. Optional — without accounting it auto-disables after 3 failed tries and alloc stays sampling-based |
| `SLURM_GPU_TUI_WEBHOOK_URL` | (unset) | Slack webhook URL (URL-only shortcut). Full alert config lives in `~/.sgpu/webhook.json` — see the [Slack Alerts](#slack-alerts) section |
| `SLURM_GPU_TUI_SLACK_BOT_TOKEN` | (unset) | Slack bot token for daily-thread mode (alternative to putting it in `webhook.json`) |
| `SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC` | `1800` | Min interval between repeated alerts for the same event key |
| `SLURM_GPU_TUI_WEBHOOK_NAG_SEC` | `21600` | Re-alert interval for standing conditions (waste / rogue / temp / ECC) |
| `SLURM_GPU_TUI_ROGUE_IGNORE` | `root,gdm,xdm` | Users never flagged as rogue |
| `SLURM_GPU_TUI_SHARE_SCRIPTS` | (unset) | Collector publishes every job's batch script so all users see them in the Enter popup. **Shares script contents (and any secrets in them) with everyone** — the installer asks about this (`[Y/n]`); `SGPU_SHARE_SCRIPTS=0/1` skips the question |

---

## Requirements

- Python 3.10+
- SLURM cluster with `sinfo` / `squeue` available on the master node
- SSH access from the master node to compute nodes (passwordless)
- `nvidia-smi` installed on GPU nodes
