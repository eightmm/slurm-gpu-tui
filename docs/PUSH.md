# Node Delivery: Push vs SSH-pull

`sgpu doctor` shows which mode is active (`node delivery`). Both are fine —
push just scales better and keeps SSH out of the hot path.

- **Push mode:** each GPU node runs a small resident `sgpu-agent` that writes
  its stats to a shared-filesystem directory every few seconds; the collector
  reads those files locally. No SSH in the hot path.
- **SSH-pull:** the collector SSHes into each node per cycle. Automatic
  fallback when push isn't possible — a valid mode, not an error.

## Push turns on automatically when two conditions hold

1. The **install directory is on a shared filesystem** that compute nodes
   mount at the same path (so they can execute the venv directly).
2. The **agent output directory** (`SLURM_GPU_TUI_AGENT_DIR`) is on that shared
   filesystem too (so agents write and the collector reads the same files).

If the install lives on the master's local disk (e.g. `/opt/sgpu`), nodes can't
see the venv → SSH-pull is used automatically.

## Enabling push mode

Install onto the shared FS and point the agent dir there — both paths must be
visible to master and every compute node at the same location:

```bash
# example: /home is NFS-shared to all nodes
SGPU_INSTALL_DIR=/home/shared/sgpu \
SLURM_GPU_TUI_AGENT_DIR=/home/shared/sgpu-nodes \
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
  runs as root, node-side agents (also root) can't write to the shared agent
  dir. Either export that path `no_root_squash`, make the agent dir
  world-writable (`chmod 1777`), or install under a normal user's shared home.
- CPU-only / GPU-less nodes are never agent targets; SSH-pull-only, and they
  never raise alerts for it.

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
    'pkill -f "bin/[s]gpu-agent"' 2>/dev/null
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
