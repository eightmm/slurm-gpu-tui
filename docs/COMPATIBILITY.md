# Mixed clusters

Build and install natively on each kind of Linux host. A shared filesystem can
carry telemetry across different distributions, architectures and hardware; a
single executable is not guaranteed to run on every host.

| Layer | Behavior | Boundary |
|---|---|---|
| Linux installation | Bash bootstrap, git, C compiler and Rust 1.93+; systemd or a local background service | Build on the target OS/architecture/libc, or supply matching prebuilt binaries |
| Slurm jobs | Prefer structured JSON; unsupported options or missing JSON/data-parser plugins select text records | Text identity/allocation still requires matching before/after numeric-UID rosters and falls back to TRES GPU counts on untyped GRES; malformed JSON and controller outages do not silently downgrade |
| Accounting | Slurm 19.05-compatible query without `sacct --array`; `sgpu logs` falls back to owner-verified `sacct` `WorkDir` when active controller records expire | Local array expansion is bounded; missing accounting stays unavailable and log reads still reject symlinks (`O_NOFOLLOW`) and foreign UIDs |
| NVIDIA measurements | Try rich fields; explicit unsupported-field errors retry a basic identity/utilization/VRAM query | Missing temperature, power, ECC, serial and clocks remain unknown, never zero |
| NVIDIA processes | Retry a basic process view if the memory view is unsupported; select GPUs in groups of sixteen | Every selected GPU needs a process-view row; unavailable or incomplete process data cannot count as empty activity |
| CPU-only nodes | `--mode cpu` samples CPU/RAM/cgroups without NVIDIA commands; power uses RAPL and `hwmon` (`amd_energy`, `acpi_power_meter`, `power_meter`, `ibmpowernv`) or `ipmitool` | GPU samplers currently target NVIDIA; AMD/Intel GPU utilization backends are not implemented |
| Job cgroups | Per-job CPU/RAM/I/O observations use cgroup v2 | Missing v2 files, including v1-only hosts, leave those job metrics unknown; host memory and scheduler allocation are separate observations |
| Agent updates | Check architecture, ownership, loader and version; retain the previous worker and roll back failed activation | Different libc versions may reject a candidate; local native installation remains the fallback |
| Legacy node bootstrap | Master installer uses existing administrative SSH and tests a native bundle before migration | Matching architecture/libc, shared publication and existing root SSH or remote sudo are required; failed nodes are reported |
| Login display | Cached snapshot, shared/personal scope, optional site hints | Debian/Ubuntu dynamic MOTD and interactive shell hooks are separate integration choices |

NVIDIA's [process-monitoring documentation](https://docs.nvidia.com/deploy/nvidia-smi/#process-monitoring)
describes the hardware and device-count limits of `pmon`. Where process tracking
is entirely unavailable, GPU-mode publication remains unavailable rather than
reporting a GPU as free. Unsupported or `N/A` utilization, including some MIG
metrics, stays unknown. This is not a claim of complete MIG support.

## Deployment checks

- Keep the agent publication directory at the same absolute shared path on all
  hosts. Do not use matching local directories as if they were shared storage.
- Use the CPU or GPU role explicitly. Different GPU models and optional IPMI,
  RAPL, quota, filesystem or link-speed sources may expose different fields.
- On a different architecture or libc, prefer a native bootstrap over copying the
  controller's binary. Read loader rejection/rollback messages before replacing
  a working baseline. An architecture match alone does not prove ABI compatibility.
- Use `sgpu doctor` to distinguish scheduler collection from node delivery.
  `sgpu-agent --version` identifies the installed baseline; payload `release`
  identifies its current worker. They may legitimately differ.
- Keep site-specific paths, modules and policy hints in the optional public
  login configuration. Lmod, `quota` and `seff` are not login-renderer dependencies.

## Verification scope

The local Rust gate covers legacy and modern Slurm response fixtures, missing
JSON plugins, unsupported NVIDIA fields/process-memory options, eighteen-device
process selection, incomplete process observations and CPU-only publication.
Those checks verify the parser and fallback contracts; they do not certify every
Ubuntu/RHEL/Alpine, driver, GPU or MIG combination. Validate an isolated local
agent sample and its snapshot on the target platform before rollout.
