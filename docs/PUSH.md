# Node Delivery: Push vs SSH-pull

`sgpu doctor` shows which mode is active (`node delivery`). Both are fine —
push just scales better and keeps SSH out of the hot path.

- **Push mode:** GPU agents write `nvidia-smi` stats every 3 seconds and
  CPU-only agents write `/proc/meminfo` every 20 seconds to a shared-filesystem
  directory; the collector reads those files locally. No SSH in the hot path.
- **SSH-pull:** the collector SSHes into each node per cycle. Automatic
  fallback when push isn't possible — a valid mode, not an error.

GPU agents are launched and repaired by the collector. A root/shared-FS
install provisions CPU-only agents as `sgpu-cpu-agent.service` with
`Restart=always`, so an overloaded node does not need to accept a new SSH
session to keep publishing RAM telemetry. Either kind falls back to SSH when
its payload is missing or stale.

## Push turns on automatically when two conditions hold

1. The **install directory is on a shared filesystem** that compute nodes
   mount at the same path (so they can execute the venv directly).
2. The **agent output directory** (`SLURM_GPU_TUI_AGENT_DIR`) is on that shared
   filesystem too (so agents write and the collector reads the same files).

If the install lives on the master's local disk (e.g. `/opt/sgpu`), nodes can't
see the venv → SSH-pull is used automatically.

## Enabling push mode

A **root install picks push-friendly defaults by itself**: when `/home/shared`
exists, the installer uses `/home/shared/sgpu` as the install dir and
`/home/shared/sgpu-nodes` as the agent dir (pre-created mode 1777), so

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | sudo bash
```

is all it takes. For a different shared-FS layout, point both paths there —
they must be visible to master and every compute node at the same location:

```bash
SGPU_INSTALL_DIR=/nfs/apps/sgpu \
SLURM_GPU_TUI_AGENT_DIR=/nfs/apps/sgpu-nodes \
  bash <(curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh)
```

Set both variables **on the same command line** as the installer — a systemd
service doesn't inherit your shell's environment, so the installer bakes these
paths into the unit for you. After ~2 collector cycles:

```bash
sgpu doctor        # node delivery → "push mode (N nodes via agent)"
```

## Requirements & gotchas

- Master needs **passwordless SSH to every compute node** (the collector
  launches agents over SSH). As a root system service that means root SSH.
- If the shared FS is exported with **NFS `root_squash`** and the collector
  runs as root, node-side agents (also root) write as `nobody`. The installer
  pre-creates the agent dir mode 1777 so this works out of the box; if you
  created the dir by hand, `chmod 1777` it (or export `no_root_squash`).
- CPU-only nodes use the systemd push agent after a root/shared-FS install.
  Set `SGPU_ENABLE_CPU_PUSH=0` to keep them SSH-pull-only.

## Who is allowed to publish a payload

The agent dir is mode 1777, so any user on the shared FS can create a
`<node>.json`. Shape validation cannot tell a real agent from a forgery, and a
forged payload feeds Slack alerts, the waste view, and per-user GPU-hour
accounting — so the collector also checks **who wrote the file** and ignores
payloads from anyone else, falling back to SSH polling for that node.

Trusted by default: `root`, the collector's own uid, the agent dir's owner, and
`nobody` (the `root_squash` case above). If your agents run under some other
account, list its uid:

```bash
SLURM_GPU_TUI_AGENT_TRUSTED_UIDS=1234,5678   # replaces the default set
```

Rejections are logged once per node and reported by `sgpu doctor` under
`agent payload trust`.

Sticky-bit side effect worth knowing: because nobody can replace a file they
do not own, a user who creates `<node>.json` first *blocks* the real agent
instead of impersonating it. The agent says so explicitly in its log
(`cannot publish ... push mode is blocked for this node`) rather than
degrading silently to SSH.

## Operational checks

```bash
sgpu doctor
systemctl status sgpu-collector          # system service
systemctl --user status sgpu-collector   # user service
ssh <cpu-node> systemctl status sgpu-cpu-agent
```

Healthy push mode usually shows:

- fresh collector data
- `node delivery -> GPU push mode (...)` with `CPU push: ...`, or a mixed
  push/SSH fallback state
- writable `SLURM_GPU_TUI_AGENT_DIR`
- no stale agent payloads older than `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC`

If a node is stale, wait one repair interval
(`SLURM_GPU_TUI_AGENT_REPAIR_SEC`, default 180s) before assuming manual repair
is needed. The collector rate-limits per-node relaunches.

## Wiping and reinstalling (shared FS)

Files on a shared FS are held open by the collector **and by every node's
agent** — a plain `rm -rf` will hang or leave `.nfsXXXX` files. Stop the
holders first:

```bash
# 1. stop the collector
sudo systemctl stop sgpu-collector

# 2. kill agents on every node (they hold the shared venv open over NFS)
for n in $(sinfo -h -N -o %N | sort -u); do
  timeout 8 ssh -o BatchMode=yes -o StrictHostKeyChecking=no "$n" \
    'sudo systemctl stop sgpu-cpu-agent 2>/dev/null || true;
     pkill -f "bin/[s]gpu-agent"' 2>/dev/null
done
sleep 2

# 3. now the directory deletes cleanly
sudo rm -rf /home/shared/sgpu /home/shared/sgpu-nodes

# 4. reinstall (do NOT ^C mid-build — a half-built venv breaks the interpreter)
SGPU_INSTALL_DIR=/home/shared/sgpu \
SLURM_GPU_TUI_AGENT_DIR=/home/shared/sgpu-nodes \
  bash <(curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh)
```

If `rm` still hangs, something still holds the files — check `fuser -v
/home/shared/sgpu` and confirm `systemctl status sgpu-collector` is stopped.
Leftover `.nfs*` files clear themselves once the open handles close.
