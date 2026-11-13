# sgpu

A Rust Slurm GPU operations monitor with a terminal UI, usage accounting,
wasted-GPU detection, Slack alerts and Prometheus metrics.

[한국어](docs/README.ko.md)

## Build and install

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo bash
```

Install on the master as root. Linux, git and a C compiler are required; the
installer prepares Rust when needed. The first build may take several minutes.
Run the same command to upgrade. Existing state and Slack configuration are
preserved. After installation, users simply run `sgpu`.

Configure each node's publisher and the shared directory using the
[node setup guide](docs/PUSH.md). Nodes publish results to a shared filesystem;
the master reads those files. It never requests node collection, installation
or repair.

## Usage

```bash
sgpu                    # GPU / CPU / Usage / Jobs tabs
sgpu once               # Current snapshot
sgpu me                 # My jobs, waste and usage
sgpu waste -v           # Wasted GPUs
sgpu usage 7 --daily     # GPU-hours
sgpu jobs 7              # Job history
sgpu logs 12345 -e -f    # Follow stderr
sgpu doctor             # Collection and configuration checks
chkgpu                  # User × node allocations
```

| Key | Action |
|---|---|
| `1`–`4` | GPU / CPU / Usage / Jobs |
| `u` / `p` / `m` / `i` | User / partition / mine / free GPU filters |
| `/` / `s` / `S` | Search / sort / reverse |
| Enter / Tab | Details / switch information, script and log tabs |
| `w` / `f` / `h` | Waste / capacity / job history |
| `n` / `x` | Watch a job / confirm cancellation of my job |
| `?` / `q` | Full help / quit |

`?` means unavailable and `~` means partial coverage. Stale data never counts
as free GPUs. Capacity and estimated start times do not guarantee admission
by the scheduler.

## Alerts and operations

[Slack alerts](docs/ALERTS.md) include daily threads, node outages/recovery,
waste, GPU health and job-failure DMs sent only to the owner. Tokens belong in
private server configuration, never in the repository or service units.

The root collector shares job-log tails and sanitized details by default.
Set `SGPU_SHARE_LOGS=0` and `SGPU_SHARE_JOB_DETAILS=0` to disable them according
to cluster policy. Script sharing is opt-in. Logs and scripts may contain
sensitive output; confirm who may read them before enabling sharing.

- [Node installation, configuration and upgrades](docs/PUSH.md)
- [Slack configuration and alert types](docs/ALERTS.md)
- [Prometheus metrics](docs/METRICS.md)

Operational scripts live in `scripts/`, implementation in `src/` and checks
in `tests/`. Run `bash scripts/check.sh` for formatting, clippy, tests and
shell checks.
