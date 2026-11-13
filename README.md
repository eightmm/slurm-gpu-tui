<div align="center">

# sgpu

**Your Slurm GPU cluster, at a glance.**

A Rust terminal monitor for jobs, GPU waste, usage and alerts.

[![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml)
[![Rust](https://img.shields.io/badge/Rust-1.93%2B-CE422B?logo=rust)](https://github.com/eightmm/slurm-gpu-tui/blob/main/Cargo.toml)
[![Linux](https://img.shields.io/badge/platform-Linux-2D6A4F?logo=linux&logoColor=white)](https://github.com/eightmm/slurm-gpu-tui#build-and-install)
[![GitHub stars](https://img.shields.io/github/stars/eightmm/slurm-gpu-tui?style=social)](https://github.com/eightmm/slurm-gpu-tui/stargazers)

[Install](#build-and-install) · [Usage](#usage) · [Slack alerts](docs/ALERTS.md) · [한국어](docs/README.ko.md)

</div>

## What you get

| Feature | What you get |
|---|---|
| **Cluster visibility** | Live GPU/CPU/RAM meters, per-node GPU state maps and keyboard-driven job tables |
| **People and controller** | Per-user job resources, sustained-waste groups, controller I/O and storage quotas when available |
| **Less wasted capacity** | Idle, parked and rogue GPU detection, plus per-user GPU-hours |
| **Job inspection** | History, scripts, log tails and confirmed cancellation of your own jobs |
| **Alerts and metrics** | Slack daily threads, owner-only failure DMs and Prometheus textfiles |
| **Push-only collection** | Independent node publishers; missing results stay visibly unavailable |

## Build and install

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo bash
```

Install on the master as root. Linux, git and a C compiler are required; the
installer prepares Rust when needed. The first build may take several minutes.
Run the same command to upgrade. Existing installation paths, service settings,
state and Slack configuration are preserved. Check `sgpu --version` after
installation; the current version is **2.1.0**. Users simply run `sgpu`.

Configure each node's publisher and the shared directory using the
[node setup guide](docs/PUSH.md). Nodes publish results to a shared filesystem;
the master reads those files. It never requests node collection, installation
or repair.

## Usage

```bash
sgpu                    # GPU / CPU / Usage / Jobs / Attention / Users / Controller
sgpu --highlight-user alice  # Highlight an account while viewing as an administrator
sgpu once               # Current snapshot
sgpu me                 # My jobs, waste and usage
sgpu waste -v           # Wasted GPUs
sgpu usage 7 --daily     # GPU-hours
sgpu jobs 7              # Job history
sgpu logs 12345 -e -f    # Follow stderr
sgpu doctor             # Collection and configuration checks
chkgpu                  # Colored user × node allocation matrix
```

| Key | Action |
|---|---|
| `1`–`7` | GPU / CPU / Usage / Jobs / Attention / Users / Controller |
| `u` / `p` / `m` / `i` | User / partition / highlighted user / free GPU filters |
| `[` / `]` / `0` | Previous node / next node / reset filters |
| `/` / `s` / `S` | Search / sort / reverse |
| Enter / Tab | Details / switch information, script and log tabs |
| `w` / `f` / `h` | Waste / capacity / job history |
| `n` / `x` | Watch a job / confirm cancellation of my job |
| `?` / `q` | Full help / quit |

`sgpu` opens a live dashboard with resource meters, GPU state symbols and sortable
tables. `chkgpu` automatically uses color in terminals; pipes stay plain. Use
`chkgpu --color always` or `--color never` to override, or set `NO_COLOR`.

Your rows use a teal background and `◆`, separate from the selection cursor.
Utilization goes from dim through blue/cyan to green as activity increases;
VRAM goes through blue/cyan/yellow/orange/red as it fills. Large terminals use
thicker resource bars. The highlight override changes display and filtering;
cancellation and private logs still use the executing account.

**Users** separates attributed job CPU/RAM/block I/O from shared-node network
and load. **Controller** shows local CPU/RAM, load, physical disk/NIC rates and
filesystem capacity; user-quota records appear when the filesystem exposes them.
**Attention** groups current idle/parked and unallocated GPU observations by user
and shows the evidence. Existing Slack thresholds continue to control delivery.
See [sampling and attribution](docs/PUSH.md#resource-views) before interpreting
shared counters. Update each node publisher locally to enable its new I/O fields.

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

## Support sgpu

If sgpu saves you time on your cluster, [give it a star](https://github.com/eightmm/slurm-gpu-tui)
so you can find it again. Bug reports and focused improvements are welcome through
[issues](https://github.com/eightmm/slurm-gpu-tui/issues) and
[pull requests](https://github.com/eightmm/slurm-gpu-tui/pulls).
