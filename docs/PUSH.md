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
SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash install.sh --role master

# Each GPU node, locally
SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash install.sh --role node --mode gpu

# Each CPU-only node, locally
SLURM_GPU_TUI_AGENT_DIR=/nfs/sgpu-nodes bash install.sh --role node --mode cpu
```

Use `--no-build` and `SGPU_BIN_DIR` for prebuilt binaries. Match the node's
architecture and C library. Root system units and nonroot user units use
`sgpu-agent.service`; systems without a service manager use `--daemon`.
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
`setup-node-power.sh --check` locally for a read-only check, or `--apply` as root
for local package/module setup. Missing hardware stays unavailable.

## Diagnosis and removal

Run `sgpu doctor` on the master. On a stale node, locally inspect
`systemctl status sgpu-agent`, its journal, mount visibility and write permissions.
For user units add `--user`. Restart or reinstall on that node after fixing the
cause. The collector does not coordinate recovery.

Uninstall the local role with `uninstall.sh --role node`; remove the collector
separately with `--role master`. Usage, shared payloads and previous releases
remain. Do not delete shared binaries still used by another host's service.

References: [cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[PSI](https://docs.kernel.org/accounting/psi.html),
[Slurm cgroup paths](https://slurm.schedmd.com/cgroup_v2.html),
[NVIDIA SMI](https://docs.nvidia.com/deploy/nvidia-smi/index.html).
