# sgpu

## Status
- State: confirmed

## Project
- Goal: Replace the entire Python project with Rust, preserving the monitor's features while removing every master-to-compute-node request path.
- Authority: The user's explicit 2026-11-13 instruction authorizes the complete language migration, deletion of the Python implementation and tests after replacement, and removal of SSH pull, agent repair, and remote node provisioning. Production deployment and publication are separate actions.
- Publication authority: The user subsequently requested the Rust-only version on main with installation through curl. This authorizes committing and pushing the migration to origin/main, plus the curl bootstrap/docs/tests needed to make that installation work. It does not authorize deployment onto production hosts.
- Users/workflow: Slurm operators and users view GPU/CPU/jobs/usage, inspect logs and history, receive alerts, and consume Prometheus metrics.
- Scope: Rust TUI, CLI, collector, node agent, usage accounting, safe log helper, alerting, Prometheus metrics, tests, installation, services, CI and documentation.
- Scope update: The user explicitly requested removal of Grafana. Remove its dashboards, generators, installer, dedicated bridge/services and documentation; keep Prometheus textfile metrics.

## Interface and Data
- Transport: Nodes autonomously publish bounded JSON payloads through the existing shared filesystem. The master reads published files and local Slurm CLI output. It must never SSH to, poll, restart, configure, or otherwise request telemetry from compute nodes. Missing/stale node results stay explicitly unavailable.
- Public API/CLI/config: Preserve `sgpu`, `sgpu-collector`, `sgpu-agent`, `chkgpu`, existing user commands and relevant `SLURM_GPU_TUI_*` variables. Remove pull/repair-only options. Node installation/configuration runs locally on each node.
- Persistence/schema: Keep published `data.json` v1, agent payload v9, existing usage/idle/inventory JSON and metric names compatible. Missing observations are distinct from zero. Preserve prior persistent data; do not delete production state during migration.
- Security: Preserve numeric scheduler UID validation, legacy Slurm fail-closed parsing, node payload authorship/hostname/size/version checks, regular-file/no-symlink reads, atomic publication, private log paths and privilege-dropped bounded log reads. No secrets in snapshots or logs.

## Commands
- Setup: `cargo build --locked`
- Test: `cargo test --locked --all-targets`
- Run: `cargo run --bin sgpu -- --help`

## Acceptance criteria
- [id:RUST-1] All application implementation is Rust. No tracked Python sources, Python manifest/lock, Python test runner, venv entrypoint, or runtime Python dependency remains.
- [id:PUSH-1] No application, installer, service, fallback or maintenance path initiates a compute-node request. The independently installed node agent publishes results; stale/missing payloads never trigger repair or polling.
- [id:FEATURE-1] GPU/CPU/Usage/Jobs TUI, filtering/sorting/search/collapse/details/history/watch/cancel/export/help, CLI snapshots/waste/usage/jobs/report/wait-free/fit/bench/me/logs/doctor and chkgpu are implemented in Rust.
- [id:DATA-1] Agent sampling includes GPU process attribution/topology/health, RAM, CPU, PSI, bounded cgroup-v2 job telemetry and node power. Collector validates and merges scheduler allocations and telemetry, tracks waste, persists usage and backfills sacct.
- [id:OPS-1] Slack configuration/reload/debounce/daily threads/job outcome DM behavior and Prometheus outputs are retained with bounded work queues and explicit failures.
- [id:NO-GRAFANA-1] Grafana dashboards, generators, installer, dedicated bridge/services and documentation are removed, with no stale installation or runtime references.
- [id:SAFE-1] Local negative-path tests cover untrusted/stale/malformed payloads, UID mismatches, symlinks, unsafe runtime directories, forged scheduler metadata, unavailable measurements, and failed durable writes.
- [id:INSTALL-1] Local master/node installation, root/user/no-systemd lifecycle, safe binary upgrades/rollback, deployment, uninstall, services and docs use Rust binaries and preserve persistent data.
- [id:VERIFY-1] Format, clippy, Rust unit/integration tests, shell checks, offline benchmarks and terminal rendering checks pass; current sources are audited against every criterion. Production cluster behavior is not claimed without an authorized live deployment.

## Verification
- Success criteria: Every acceptance criterion has inspected source and current executable evidence; the goal remains active until all are satisfied.
- Affected checks: `cargo test --locked --all-targets`
- Always-run checks: Rust format/clippy/tests, shell syntax and no-Python/no-master-to-node architecture checks.
- Required checks: `bash scripts/check.sh`
- Required check files: scripts/check.sh
- CI scope/runtime: One Linux Rust 1.93 job runs format/clippy/tests and shell checks on PRs or manual dispatch. No duplicate push matrix or publication workflow. The warm local gate took 23s before the final service case; the final run took 146s including concurrent-build lock waits and cold rebuilds.

## Decisions
- Confirmed: Full Rust end state and push-only node telemetry come directly from the user. Existing shared-FS transport and data formats minimize operational changes. Local scheduler queries and user-requested scheduler job cancellation remain supported.
- Open: None blocking implementation. Performance gains and production-node ABI compatibility require measurement and are not assumed.

## Migration evidence

Baseline: `b348fb900d7c785977ecf39dc8c1f5e5aa3d5677`, initially clean. All five native entrypoints execute Rust implementations. Python source/tests, manifest, lock and project environment/package metadata are removed. The behavioral reference remains in git history. Grafana dashboards, generators, installer, dedicated bridge/services/rules and documentation are removed; Prometheus output and local rules remain.

| Criterion | Inspected source and executable evidence |
|---|---|
| RUST-1 | Cargo package/lock, Rust application modules and five binaries. Architecture tests reject former source/manifests/entrypoints. Shell wrappers perform local build/OS setup. |
| PUSH-1 | Collector reads publications and local Slurm output. The process enum has no remote transport. Pipeline tests trap remote tools and verify missing publishers retain stale inventory. Install/uninstall/power setup operate locally. Cancellation uses `scancel --ctld`. |
| FEATURE-1 | `cli`, `ui`, `history`, `quickview`, `benchmark`. Integration covers all CLI commands, personal CPU jobs, invalid input and a real pseudo-terminal opening/tab navigation/terminal restoration. Rendering tests cover four tabs, filtered cards/summary and wrapped/Usage scrolling. |
| DATA-1 | `agent`, `node_telemetry`, `scheduler`, `collector`, `rules`, `usage`. Tests cover CPU deltas/resets, bounded cgroups, payload trust, Slurm 25.11 ordering, guarded legacy metadata, allocation attribution and durable usage retries. |
| OPS-1 | `notify`, `batch`, `logs`, `metrics`. Captured Slack verifies daily threads, owner-only failure DM, scheduler failure, zero-state migration and transition retry. Failed script requests back off. Stale measurements are omitted from metrics. |
| NO-GRAFANA-1 | Application-tree and maintained-document tests reject removed setup references. No dashboard/bridge runtime remains. |
| SAFE-1 | Tests reject incorrect numeric UID, unsafe ownership/modes, symlinks, oversized/malformed/stale data and forged/ambiguous scheduler records. Source log paths remain private; root reads through a bounded credential-dropped helper. Failed writes retain dirty state. |
| INSTALL-1 | `packaging` stages immutable native releases and records reversible file changes before switching `current`. Tests cover role upgrades/removal, missing artifacts, partial-link rollback, private config, a real nonroot background collector and simulated user-systemd failure/rollback. Persistent data and previous releases remain. |
| VERIFY-1 | `scripts/check.sh` passed on the minimum declared Rust 1.93.0: format, strict clippy, all-target tests, Bash syntax, ShellCheck and offline benchmark. Release build and final artifact checks are recorded below. |

### Verification record

- 2026-11-13: Migration required gate passed: **49 unit tests + 14 integration tests = 63**, zero failures or ignored tests. The source includes the user-service rollback test; root/user/no-systemd units are native. Curl bootstrap publication checks are recorded separately below.
- Tests isolate home/data/state/runtime paths and mock scheduler/service tools. The background test starts and stops a real native local collector. Slack uses captured transports; no live Slack requests or production cluster jobs were sent.
- `cargo build --locked --release --bins -j 2` passed (first build 6m31s, dependencies included). All five native ELF executables ran `--version` successfully. Release files-only install/version/uninstall passed in a temporary prefix. These checks do not deploy onto production hosts.
- Release offline benchmark: 128 nodes/1,024 GPUs, one-util update + terminal-buffer render median 7.83ms (collapsed 2.30ms); 512 nodes/4,096 GPUs, 30.38ms (collapsed 8.32ms). JSON decoding is excluded. These are local synthetic timings, not a direct comparison with the former update-only benchmark or a production claim.
- Root systemd, GPU hardware, deployed-node C-library compatibility and production NFS ownership/mount behavior remain site-validation limits; local tests do not prove live deployment behavior.

### Curl installation

The published bootstrap clones main, checks for a C compiler, reuses compatible Rust or prepares minimal Rust 1.93.0 through official rustup, and invokes the native local installer. Automatic Rust setup can be disabled; prebuilt installation skips build tools. Source updates require a fast-forward and preserve local edits by default. Master and node roles are installed on their respective hosts. README examples pass the shared publication directory on the bash side of the curl pipeline.

The publication gate passed with **68 tests** (49 unit, 19 integration), strict clippy, rustfmt, shell checks and offline benchmark. Five piped-bootstrap cases cover native installation, Rust setup, failed downloads/opt-out, prebuilt upgrades and preservation of local edits. The gate took 43s with warm dependencies. The repository is public, so the documented raw main URL is accessible without credentials.

### Compatibility decisions

Snapshot v1 and publisher v9 remain compatible. Structured allocations sort Slurm 25.11 node indices; legacy metadata requires stable before/after numeric UID/state/node rosters and fails closed. Notification state preserves its eight keys and accepts legacy boolean/zero node values. Private failure-log seeds stay in memory. Runtime waste/rogue defaults are 0/false; the explicit config writer seeds 2/true. Configuration search, hot reload, bounded queues, daily thread and owner-only DM are preserved; delivery is not a durable outbox.

Shared-FS publication is the sole node transport. Missing data never launches repair. Local services supervise both GPU and CPU publishers. Root GPU persistence setup is local and opt-out; uninstall does not disable the site's GPU persistence mode. Master uninstall cannot stop another host's publisher. Existing persistent state and unrelated exports are preserved.
