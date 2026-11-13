# Autonomous node publication

The only telemetry transport is node → shared filesystem → collector.
Install a supervised publisher locally on every node. The application never
opens a master-to-node session or repairs a publisher when data stops.

## Local setup

Choose one absolute shared `SLURM_GPU_TUI_AGENT_DIR`, visible at the same path
on the master and every node. Binaries may be installed on local disks; only
the published JSON files need the shared filesystem.

```bash
# Master host
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo env SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash -s -- --role master

# Each GPU node, locally
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo env SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash -s -- --role node --mode gpu

# Each CPU-only node, locally
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo env SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash -s -- --role node --mode cpu
```

Use `--no-build` and `SGPU_BIN_DIR` for prebuilt binaries. Match the node's
architecture and C library. Installation requires root and uses
`sgpu-agent.service`; systems without a service manager use a root background daemon.
The service waits for network/shared mounts, restarts after failure, and receives
60 seconds to stop. `sgpu-agent --stop` stops only the current user's local
publisher validated against its lock holder. Configure agent runtime files on
local storage with `SLURM_GPU_TUI_AGENT_RUNTIME_DIR` when needed.

The installer creates a missing publication directory mode 1777; an existing
directory retains its site-managed permissions. The sticky bit prevents one
writer from replacing another writer's file. Provision the directory and its
ACLs explicitly when NFS `root_squash` prevents directory creation. A malicious
first writer can block a node's filename; it cannot impersonate a trusted author.
Resolve such a collision as the directory owner or site administrator.

## Payload validation

Nodes atomically publish `<hostname>.json` (schema v9). The collector checks:

- bounded regular files, no final symlink, expected hostname and GPU/CPU kind;
- supported version, required fields, unique GPU indices and bounded lists;
- modification time (default maximum age 45s; at most 5s into the future);
- file author in the trusted numeric UID set.

Default trusted UIDs: root, collector UID, directory owner and 65534 (common
root-squash identity). Narrow this set for the site's actual publisher account:
`SLURM_GPU_TUI_AGENT_TRUSTED_UIDS=1234,5678` replaces defaults. Trust applies
to every node filename in this directory; filesystem identity is not per-node
cryptographic authentication. Do not trust accounts that untrusted users can use.

Missing, invalid, rejected or stale files retain known inventory with explicit
unavailable status. They do not count as free GPUs and never cause a node request.
Build timestamps do not trigger publisher updates. Update each node service
locally when upgrading the agent schema.

## Sampling

GPU publishers sample at 3s; CPU publishers at 20s. CPU/RAM, PSI and bounded
cgroup v2 job data are cached for 10s by default. CPU deltas need two samples.
`SLURM_GPU_TUI_TELEMETRY_MAX_JOBS` defaults to 256, clamped to 1–4096.

The cgroup scan examines Slurm-managed `slurm` or `[node_]slurmstepd.scope`
under the root or `system.slice`, including optional `uid_<id>` wrappers.
Numeric `job_<id>` paths are supported; SLUID directories require a root-owned
`slurmstepd: [job.step]` process. Unresolved/duplicate IDs are omitted. The scan
does not traverse `user.slice`, follow symlinks or read more than 8 KiB per
kernel record. Scan limits produce `jobs_truncated`.

CPU counter resets, unavailable cgroup v2 files, permissions and unlimited
memory produce missing values. `memory.current` includes descendants and cache;
`memory.peak` is node-local; `oom_kill` is cumulative. The collector attaches
finite numeric values only to validated scheduler jobs on allocated nodes.
GPU PID-attributed VRAM is reported separately, with per-metric coverage.

GPU health is capability-gated and cached for at least 10s; topology is cached
for 300s. Power uses local RAPL and optional `ipmitool`. Run
`scripts/setup-node-power.sh --check` locally for a read-only check, or `--apply` as root
for local package/module setup. Missing hardware stays unavailable.

## Diagnosis and removal

Run `sgpu doctor` on the master. On a stale node, locally inspect
`systemctl status sgpu-agent`, its journal, mount visibility and write permissions.
Restart or reinstall on that node after fixing the cause. The collector does not coordinate recovery.

Uninstall the local role with `sudo sgpu-install uninstall --role node`; remove
the collector separately with `sudo sgpu-install uninstall --role master`.
Usage, shared payloads and previous releases remain. Do not delete shared binaries still used by another host's service.

References: [cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[PSI](https://docs.kernel.org/accounting/psi.html),
[Slurm cgroup paths](https://slurm.schedmd.com/cgroup_v2.html),
[NVIDIA SMI](https://docs.nvidia.com/deploy/nvidia-smi/index.html).

## Configuration

Set public settings when installing; they are captured in the generated system
unit. Credentials stay in the private Slack configuration.

| `SLURM_GPU_TUI_` suffix | Default | Purpose |
|---|---|---|
| `DATA_DIR` | `/tmp/slurm-gpu-tui` | Merged snapshot, metrics and collector runtime |
| `STATE_DIR` | `/var/lib/sgpu` | Usage, inventory, idle and notification state |
| `AGENT_DIR` | `~/.sgpu/nodes` | Shared publisher directory; set explicitly across hosts |
| `AGENT_TRUSTED_UIDS` | root, own UID, directory owner, 65534 | Override accepted file authors with comma-separated numeric UIDs |
| `AGENT_RUNTIME_DIR` | `/run` | Optional private local agent lock/log directory |
| `REFRESH_SEC` / `COLLECTOR_SEC` | `3` / `3` | UI / collector cadence |
| `SCHEDULER_SEC` / `SCHEDULER_MAX_AGE_SEC` | collector cadence / `30` | Background scheduler cadence / maximum age |
| `MEM_REFRESH_SEC` | `15` | Scheduler AllocMem query cadence |
| `AGENT_SEC` / `CPU_AGENT_SEC` | `3` / `20` | GPU / CPU publisher cadence |
| `AGENT_MAX_AGE_SEC` / `AGENT_MAX_BYTES` | `45` / `1048576` | Payload freshness / byte limit |
| `AGENT_CMD_TIMEOUT_SEC` | `40` | Local agent command timeout |
| `TELEMETRY_SEC` / `TELEMETRY_MAX_JOBS` | `10` / `256` | CPU/PSI/cgroup sampling interval / job scan cap |
| `GPU_TOPOLOGY_TTL_SEC` | `300` | GPU identity cache |
| `WASTE_MIN_SEC` / `ROGUE_IGNORE` | `600` / `root,gdm,xdm` | Waste threshold / ignored process users |
| `USAGE_KEEP_DAYS` / `USAGE_SAVE_SEC` | `30` / `30` | Retention / durable checkpoint cadence |
| `SACCT_SEC` | `3600` | Usage backfill (`0` disables) |
| `METRICS_SEC` / `METRICS_PENDING_MAX` | `15` / `100` | Textfile cadence / per-job pending-series cap |
| `METRICS_FILE` | `<data>/metrics.prom` | Textfile path |
| `SHARE_LOGS` / `SHARE_JOB_DETAILS` / `SHARE_SCRIPTS` | off before installer defaults | Publish selected content to every snapshot reader |
| `LOG_TAIL_BYTES` / `LOG_MIRROR_SEC` / `LOG_MIRROR_MAX_SEC` | `65536` / `10` / `60` | Shared tail cap / scan and unchanged-file backoff |
| `SLACK_CONFIG` | `/etc/sgpu/slack.json` | Explicit override; legacy paths are also searched |
| `SLACK_DEBOUNCE_SEC` / `SLACK_NAG_SEC` | `1800` / `21600` | Event / standing-condition debounce |

For a source checkout, use `sudo bash scripts/install.sh`; `scripts/deploy.sh`
runs the required gate before the local installation. Both require root.
The installer reuses a recognized existing installation, including the previous
Python installation's directory and service settings. A fresh installation uses
`/opt/sgpu`; `SGPU_INSTALL_DIR` overrides it. Explicit settings in the installing
environment override inherited service settings. Existing Slack configuration,
state and the old environment are retained. Check `sgpu --version` to confirm
that the native command is active (currently **2.1.0**).

Source and prebuilt binaries must match the host architecture and C library.
Upgrades keep previous releases and persistent state; failed activation attempts
roll back the current release and service configuration. `--no-service` prepares
files only and still requires root.

## Resource views

The TUI adds Attention, Users and Controller tabs (`5`, `6`, `7`). Nodes are
spaced without inserting selectable blank rows. `◆` and a teal background mark
the highlighted account; `--highlight-user USER` sets that display preference
for an administrator. It does not change the OS identity used for cancellation
or private logs. `[`/`]` jump between nodes and `0` clears filters. Enter opens
full values when narrow tables hide auxiliary columns or abbreviate long names.

Users sums fresh, scheduler-validated job cgroup CPU cores, current RAM and
physical block read/write bytes per second. Block I/O comes from `io.stat`,
not filesystem ownership scans; cache hits and network-filesystem traffic are
outside that counter. Each metric keeps its own sampling coverage (`?` missing,
`~` partial). A readable empty `io.stat` can report a measured zero after two
samples. Missing files, counter resets and first samples remain unavailable.

Node network and load are **shared host values**, never divided into estimated
per-user traffic. An assigned node is counted once per user even when that user
has several jobs on it. Network is the sum of physical NIC counters; virtual
bridge/bond/VLAN interfaces are excluded to avoid double counting. These are
interface byte counters, not a complete accounting of offloaded RDMA traffic.
Load1 is the maximum 1-minute host load over the user's assigned nodes. Enter
shows per-node disk/network rates and 1/5/15-minute load values.

The controller samples its own `/proc` and `/sys` locally at 10-second intervals.
Disk counters use whole physical devices, excluding partitions and virtual
device stacks. Filesystem capacity and available VFS user-quota records are
refreshed by one background worker every 120 seconds, bounded to 16 local
filesystems and 128 known active/local accounts. Slow storage queries do not
block snapshot publication. Samples older than 300 seconds are unavailable;
sampling limits are marked partial. Quota limits are read, never configured. Unsupported
quota backends and missing records remain unavailable. Network mounts and
filesystem tree scans are excluded from capacity/quota sampling. No site paths,
module lists or policy limits are embedded in the application.

Update agents locally to publish these optional fields. Payload v9 and snapshot
v1 remain compatible; older agents show unavailable new metrics. Controller
telemetry comes from the collector host, not from the workstation running the UI.

Attention uses the configured `WASTE_MIN_SEC` threshold for current idle/parked
allocation evidence and separately shows unallocated processes. Timer age is
elapsed time, not proof of continuous observation through stale gaps. Inspect
the job before acting. New confirmed groups produce an in-app notice; existing
`waste_alert_hours` and Slack debounce settings still control Slack alerts.

Kernel references: [block counters](https://docs.kernel.org/block/stat.html),
[interface counters](https://docs.kernel.org/networking/statistics.html),
[cgroup v2 I/O](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[quota queries](https://man7.org/linux/man-pages/man2/quotactl.2.html).
