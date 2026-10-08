"""Background collector daemon for sgpu."""
from __future__ import annotations

import fcntl
import glob
import hashlib
import json
import os
import pwd
import re
import shlex
import signal
import stat
import subprocess
import sys
import threading
import time
from dataclasses import asdict, replace
from datetime import datetime, timedelta
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Dict, List

from .common import (
    GpuInfo, JobInfo, NodeErrorKind, NodeMemInfo, PendingJob, ROGUE_IGNORE,
    collect_basic, collect_node_data, mem_to_mib,
    parse_gres_models, reconcile_gpu_alloc, resolve_user, run_cmd, ssh_cmd,
    valid_slurm_job_id,
    assign_node_jobs,
    _classify_error,
)
from .agent import AGENT_PAYLOAD_VERSION
from . import agent as _agent_module
from .notify import Notifier
from .runtime import (
    UnsafeRuntimeDir, atomic_write, atomic_write_with_signature,
    default_data_dir, default_state_dir, ensure_secure_dir, open_append,
    open_lock, read_regular_file_with_signature, state_dir_candidates,
    trusted_payload_uids,
)
from . import __build__, __version__
from .telemetry import command_snapshot, metric_number
from .node_telemetry import clean_telemetry, clean_sample, TELEMETRY_MAX_AGE

# ── Config ────────────────────────────────────────────────────────────────

DATA_DIR = default_data_dir()
# Persistent state (usage history, waste ages, inventory) must survive
# reboots, so it lives outside /tmp — see runtime.default_state_dir for why a
# root collector cannot keep it under ~/.sgpu
STATE_DIR = default_state_dir()
STATE_MARKER_FILE = STATE_DIR / ".sgpu-state"
DATA_FILE = DATA_DIR / "data.json"
PID_FILE = DATA_DIR / "collector.pid"
LOCK_FILE = DATA_DIR / "collector.lock"
REFRESH_SEC = int(os.getenv("SLURM_GPU_TUI_COLLECTOR_SEC", "3"))
NODE_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
MAX_WORKERS = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))
MAX_PENDING_POLLS = max(MAX_WORKERS, int(os.getenv("SLURM_GPU_TUI_MAX_PENDING_POLLS", str(MAX_WORKERS * 2))))
SCHEDULER_SEC = max(1.0, float(os.getenv("SLURM_GPU_TUI_SCHEDULER_SEC", str(REFRESH_SEC))))
SCHEDULER_MAX_AGE = max(SCHEDULER_SEC, float(os.getenv("SLURM_GPU_TUI_SCHEDULER_MAX_AGE_SEC", "30")))
MEM_REFRESH_SEC = max(1.0, float(os.getenv("SLURM_GPU_TUI_MEM_REFRESH_SEC", "15")))
LOG_MAX_BYTES = int(os.getenv("SLURM_GPU_TUI_LOG_MAX_BYTES", str(5 * 1024 * 1024)))

# Push-mode agents: nodes write their own payloads to this shared-FS dir
# (master is the NFS server, so reads here are local and cache-free).
AGENT_DIR = Path(os.getenv("SLURM_GPU_TUI_AGENT_DIR", str(Path.home() / ".sgpu" / "nodes")))
# Generous: nvidia-smi pmon on a busy node can stretch one agent cycle to ~20s
AGENT_MAX_AGE = int(os.getenv("SLURM_GPU_TUI_AGENT_MAX_AGE_SEC", "45"))
AGENT_REPAIR_SEC = int(os.getenv("SLURM_GPU_TUI_AGENT_REPAIR_SEC", "180"))
AGENT_DISABLE = bool(os.getenv("SLURM_GPU_TUI_AGENT_DISABLE", ""))
AGENT_PAYLOAD_MAX_BYTES = int(os.getenv("SLURM_GPU_TUI_AGENT_MAX_BYTES", str(1024 * 1024)))

# Opt-in: publish every running job's batch script in data.json so all users
# can view them in the TUI. Requires a collector that may read them (root).
# OFF by default — scripts can contain secrets; enabling shares them with
# everyone who can read data.json.
def _env_enabled(name: str) -> bool:
    return os.getenv(name, "").strip().lower() not in (
        "", "0", "false", "no", "off",
    )


SHARE_SCRIPTS = _env_enabled("SLURM_GPU_TUI_SHARE_SCRIPTS")
SCRIPT_MAX_BYTES = 16384
# data.json is world-readable. This gate exposes only allowlisted operational
# fields from `scontrol show job`; free-form text and private paths stay out.
SHARE_JOB_DETAILS = _env_enabled("SLURM_GPU_TUI_SHARE_JOB_DETAILS")

# ── Long-lived executors / shared node results ───────────────────────────

_node_executor = ThreadPoolExecutor(max_workers=MAX_WORKERS)
# Repairs (~40s each: pkill + sleep + launch over SSH) get their own small
# pool so a batch of dead nodes can't starve the polling executor
_repair_executor = ThreadPoolExecutor(max_workers=2)

# Latest per-node SSH results, updated by background pollers.
# name -> {"gpus": [dict], "mem": dict, "power": dict, "error": str, "error_kind": str, "stale": bool}
_results_lock = threading.Lock()
_node_results: Dict[str, dict] = {}
_inflight: set = set()
_node_absent_cycles: Dict[str, int] = {}
_NODE_CACHE_PRUNE_AFTER = 2
_poll_cursor = 0
_notification_log_sources: dict = {}
_cycle_stats: dict = {}


class SchedulerSampler:
    """Single in-flight scheduler generation, independent of telemetry publishing."""

    def __init__(self):
        self.executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="sgpu-scheduler")
        self.future = None
        self.snapshot = None
        self.next_poll = 0.0
        self.error = ""

    def read(self):
        now = time.monotonic()
        if self.future is not None and self.future.done():
            try:
                result = self.future.result()
                self.error = result[-1]
                if not self.error or self.snapshot is None:
                    self.snapshot = result
            except Exception as exc:
                self.error = f"scheduler collect failed: {exc}"
            self.future = None
            self.next_poll = now + SCHEDULER_SEC
        if self.future is None and now >= self.next_poll:
            self.future = self.executor.submit(collect_basic, mem_refresh_sec=MEM_REFRESH_SEC)
        if self.snapshot is None:
            error = self.error or "scheduler initializing"
            return ([], [], [], {}, {}, {}, {"job_backend": "unavailable", "age_sec": -1.0, "error": error}, error)
        nodes, jobs, pending, node_jobs, alloc, users, status, error = self.snapshot
        age = max(0.0, time.time() - float(status.get("observed_at", 0)))
        status = dict(status, age_sec=age)
        if self.error or age > SCHEDULER_MAX_AGE:
            error = self.error or "scheduler snapshot stale"
            jobs = [replace(job, uid=-1, detail="") for job in jobs]
            pending = [replace(job, detail="") for job in pending]
            node_jobs = assign_node_jobs(jobs)
            alloc, users = {}, {}
            status.update(job_backend="unavailable", error=error)
        return nodes, jobs, pending, node_jobs, alloc, users, status, error


_scheduler_sampler = SchedulerSampler()

# ── Waste-age tracking (idle / parked) ────────────────────────────────────
# "node:gpu_index" -> {"jobid"/"owner": str, "since": float}.
# idle   = allocated with no GPU process.
# parked = VRAM held (>=30%) at ~0% utilization by someone.
# Persisted so collector restarts don't reset ages.

IDLE_STATE_FILE = STATE_DIR / "idle_state.json"
_idle_since: Dict[str, dict] = {}
_parked_since: Dict[str, dict] = {}

# ── GPU inventory ─────────────────────────────────────────────────────────
# Hardware per node barely changes: remember index/name/mem_total from the
# last successful poll so the full GPU layout renders even when a node is
# cold-starting or unreachable. Auto-refreshes on every successful poll.

INVENTORY_FILE = STATE_DIR / "inventory.json"
_inventory: Dict[str, List[dict]] = {}


def _read_state_json(path: Path):
    """Load a state file, falling back to every earlier STATE_DIR layout.

    Covers the pre-STATE_DIR /tmp location and the pre-/var/lib ~/.sgpu/state
    one, so switching the collector to root keeps the accumulated history.
    """
    seen = set()
    for d in [path.parent] + state_dir_candidates():
        p = d / path.name
        if p in seen:
            continue
        seen.add(p)
        try:
            return json.loads(p.read_text())
        except Exception:
            continue
    return None


_state_write_warned: set = set()
# path -> (last successful digest, file identity). Keeping the identity means
# an external replacement/deletion is repaired instead of being mistaken for
# an unchanged state file.
_state_write_cache: Dict[
    Path, tuple[bytes, tuple[int, int, int, int, int, int]]
] = {}
_state_write_lock = threading.Lock()
_state_path_locks: Dict[Path, threading.Lock] = {}
_inventory_lock = threading.Lock()


def _state_path_lock(path: Path) -> threading.Lock:
    with _state_write_lock:
        return _state_path_locks.setdefault(path, threading.Lock())


def _state_file_signature(path: Path) -> tuple[int, int, int, int, int, int]:
    st = path.lstat()
    return (
        st.st_mode, st.st_dev, st.st_ino, st.st_size,
        st.st_mtime_ns, st.st_ctime_ns,
    )


def _write_state_json(path: Path, text: str) -> bool:
    """Atomic + fsync'd write for files that must survive a reboot.

    World-readable on purpose: the published TUI runs as an ordinary user and
    reads the usage history from here. Failures (disk full, permissions) are
    logged once, not every cycle.
    """
    try:
        ensure_secure_dir(path.parent)
        with _state_path_lock(path):
            digest = hashlib.blake2b(text.encode(), digest_size=16).digest()
            with _state_write_lock:
                cached = _state_write_cache.get(path)
            if cached is not None and cached[0] == digest:
                try:
                    signature = _state_file_signature(path)
                    if signature == cached[1]:
                        return False
                    # Some filesystems expose a different identity for the
                    # temp fd and its post-rename path. Verify content through
                    # a no-follow fd, then refresh metadata instead of writing
                    # forever. Wrong modes and non-regular files are repaired.
                    current = read_regular_file_with_signature(path, mode=0o644)
                    if current is None:
                        raise OSError("state file type or mode changed")
                    content, signature = current
                    disk_digest = hashlib.blake2b(content, digest_size=16).digest()
                    if disk_digest == digest:
                        with _state_write_lock:
                            _state_write_cache[path] = (digest, signature)
                        return False
                except OSError:
                    pass
            signature = atomic_write_with_signature(
                path, text, mode=0o644, fsync=True,
            )
            with _state_write_lock:
                _state_write_cache[path] = (digest, signature)
                _state_write_warned.discard(str(path))
            return True
    except (OSError, UnsafeRuntimeDir) as e:
        with _state_write_lock:
            first_failure = str(path) not in _state_write_warned
            _state_write_warned.add(str(path))
        if first_failure:
            print(f"[collector] state write failed for {path}: {e}", flush=True)
        return False


def _load_inventory() -> None:
    raw = _read_state_json(INVENTORY_FILE)
    if isinstance(raw, dict):
        _inventory.update(raw)


def _update_inventory(name: str, gpu_dicts: List[dict]) -> None:
    """Refresh a node's static GPU info; persist when changed or file missing."""
    static = [
        {"index": g["index"], "minor": g.get("minor", ""),
         "name": g["name"], "mem_total": g["mem_total"]}
        for g in gpu_dicts
    ]
    if not static:
        return
    # called from poller threads and the main loop — serialize the check-
    # mutate-write sequence so two threads can't interleave on the tmp file
    with _inventory_lock:
        if _inventory.get(name) == static and INVENTORY_FILE.exists():
            return
        _inventory[name] = static
        _write_state_json(INVENTORY_FILE, json.dumps(_inventory, ensure_ascii=False))


def _skeleton_gpus(name: str, gres: str) -> List[dict]:
    """Placeholder GPU rows: learned inventory, else sinfo GRES models."""
    inv = _inventory.get(name)
    if inv:
        base = [dict(g) for g in inv]
    else:
        base = [
            {"index": str(i), "name": model, "mem_total": ""}
            for i, model in enumerate(parse_gres_models(gres))
        ]
    for g in base:
        g.setdefault("mem_total", "")
        g.update(util="", mem_used="", temp="", power="", power_cap="",
                 pids=[], users=[])
    return base


def _load_idle_state() -> None:
    raw = _read_state_json(IDLE_STATE_FILE)
    if not isinstance(raw, dict):
        return
    if "idle" in raw or "parked" in raw:
        _idle_since.update(raw.get("idle", {}))
        _parked_since.update(raw.get("parked", {}))
    else:  # pre-parked flat format
        _idle_since.update(raw)


def _save_idle_state() -> None:
    _write_state_json(IDLE_STATE_FILE,
                      json.dumps({"idle": _idle_since, "parked": _parked_since}))


def _track_waste(node: str, gpu: dict, now: float) -> None:
    """Set gpu['idle_sec'] and gpu['parked_sec'] waste durations."""
    key = f"{node}:{gpu.get('index', '')}"
    jid = gpu.get("alloc_jobid", "")

    if jid and not gpu.get("users"):
        st = _idle_since.get(key)
        if not st or st.get("jobid") != jid:
            st = {"jobid": jid, "since": now}
            _idle_since[key] = st
        gpu["idle_sec"] = int(now - st["since"])
    else:
        _idle_since.pop(key, None)
        gpu["idle_sec"] = 0

    try:
        util = float(gpu.get("util") or -1)
    except (ValueError, TypeError):
        util = -1.0
    try:
        total = float(gpu.get("mem_total") or 0)
        vram_pct = float(gpu.get("mem_used") or 0) / total if total > 0 else 0.0
    except (ValueError, TypeError):
        vram_pct = 0.0
    owner = jid or ",".join(gpu.get("users") or [])
    if 0 <= util <= 5 and vram_pct >= 0.3 and owner:
        st = _parked_since.get(key)
        if not st or st.get("owner") != owner:
            st = {"owner": owner, "since": now}
            _parked_since[key] = st
        gpu["parked_sec"] = int(now - st["since"])
    else:
        _parked_since.pop(key, None)
        gpu["parked_sec"] = 0

# ── Batch-script sharing (opt-in) ─────────────────────────────────────────

_script_cache: Dict[str, str] = {}  # jobid -> script text ("" = unreadable)
_script_inflight: set = set()
# Both are touched by the collect loop and by the fetch worker below. Without
# this lock the loop's "drop finished jobs" pass could iterate the dict while
# the worker inserts into it — RuntimeError, swallowed by run_collector's
# handler, costing a whole collect cycle.
_script_lock = threading.Lock()
# one background worker: a burst of new jobs (array submit) would otherwise
# serialize N scontrol calls inside the 3s collect cycle
_script_executor = ThreadPoolExecutor(max_workers=1)


def _fetch_one_script(jid: str) -> None:
    try:
        cmd = f"scontrol write batch_script {jid} -"
        if os.geteuid() != 0:
            # install.sh provisions a sudoers rule for exactly this command
            cmd = "sudo -n " + cmd
        ok, out = run_cmd(cmd)  # outside the lock: this is a subprocess call
        out = out.strip()
        good = ok and out and not out.startswith("job script retrieval failed")
        with _script_lock:
            _script_cache[jid] = out[:SCRIPT_MAX_BYTES] if good else ""
    finally:
        with _script_lock:
            _script_inflight.discard(jid)


def _fetch_scripts(jobs: List[JobInfo]) -> Dict[str, str]:
    """Return cached batch scripts; fetch missing ones in the background.
    A job's script appears one or two cycles after the job does."""
    if not SHARE_SCRIPTS:
        return {}
    live = {j.jobid for j in jobs}
    with _script_lock:
        for jid in [j for j in _script_cache if j not in live]:
            del _script_cache[jid]
        todo = [j.jobid for j in jobs
                if j.jobid not in _script_cache and j.jobid not in _script_inflight]
        _script_inflight.update(todo)
        snapshot = dict(_script_cache)
    for jid in todo:
        _script_executor.submit(_fetch_one_script, jid)
    return snapshot


# ── Job stdout/stderr sharing ─────────────────────────────────────────────
# A job's logs live wherever the submitter pointed --output, normally under
# their home: another user cannot read them, so the TUI's log tabs are empty
# for everyone but the owner. A root collector can read them, so it mirrors a
# bounded tail into a world-readable spool and readers fall back to that.
#
# Gated by the unit environment; root installs enable it by default and offer
# SGPU_SHARE_LOGS=0 as an install-time opt-out. It is more sensitive than
# SHARE_SCRIPTS: tokens echoed by a framework, connection strings, and
# environment dumps inside tracebacks can all end up here, and this publishes
# them to everyone who can read the state directory.

SHARE_LOGS = _env_enabled("SLURM_GPU_TUI_SHARE_LOGS")
LOG_SPOOL_DIR = Path(
    os.getenv("SLURM_GPU_TUI_LOG_SPOOL_DIR", str(STATE_DIR / "logs"))
)
# Matches tail_file's default, so a shared log shows exactly what the owner
# would see in the same pane.
LOG_TAIL_BYTES = int(os.getenv("SLURM_GPU_TUI_LOG_TAIL_BYTES", str(64 * 1024)))
LOG_MIRROR_SEC = max(
    1.0, float(os.getenv("SLURM_GPU_TUI_LOG_MIRROR_SEC", "10")),
)
LOG_MIRROR_MAX_SEC = max(LOG_MIRROR_SEC, float(os.getenv("SLURM_GPU_TUI_LOG_MIRROR_MAX_SEC", "60")))
_log_unchanged: dict[str, int] = {}

_log_paths: Dict[str, tuple] = {}      # jobid -> (stdout src, stderr src)
_log_owner_uids: Dict[str, int] = {}   # jobid -> scheduler-reported owner uid
_log_fingerprint: Dict[str, tuple] = {}  # spool path -> (size, mtime) mirrored
_log_published: Dict[str, Dict[str, str]] = {}
_log_status: Dict[str, Dict[str, str]] = {}
_log_seed_tokens: Dict[str, object] = {}  # invalidates stale async workers
_log_next_check: Dict[str, float] = {}
_log_live: set[str] = set()
_log_inflight: set = set()
_log_lock = threading.Lock()
# two workers: a mirror pass is file I/O over NFS, and one slow home directory
# should not hold up every other job's logs
_log_executor = ThreadPoolExecutor(max_workers=2)


def _owner_identity(uid: int) -> tuple[int, List[int]] | None:
    """Primary/supplementary groups for a scheduler-reported local UID."""
    try:
        pw = pwd.getpwuid(uid)
        groups = [g for g in os.getgrouplist(pw.pw_name, pw.pw_gid)
                  if g != pw.pw_gid]
        return pw.pw_gid, groups
    except (KeyError, OSError):
        return None


def _read_log_as_owner(src: str, owner_uid: int, limit: int | None = None) -> tuple[bytes | None, str]:
    """Read a bounded tail in a privilege-dropped child.

    This is the NFS root-squash fallback. The helper itself opens with
    O_NOFOLLOW and accepts only a regular file owned by its effective UID.
    """
    if os.geteuid() != 0:
        return None, "unreadable"
    identity = _owner_identity(owner_uid)
    if identity is None:
        return None, "unreadable"
    gid, groups = identity
    try:
        result = subprocess.run(
            [sys.executable, "-I", "-m", "sgpu.log_reader", src,
             str(LOG_TAIL_BYTES if limit is None else limit)],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            timeout=8, check=False, cwd="/", env={"LANG": "C.UTF-8"},
            user=owner_uid, group=gid, extra_groups=groups,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None, "unreadable"
    if result.returncode == 0:
        return result.stdout[:LOG_TAIL_BYTES if limit is None else limit], "mirrored"
    if result.returncode == 4:
        return None, "unsafe"
    return None, "unreadable"


def _read_log_source(
    src: str, owner_uid: int, known: tuple | None, *, limit: int | None = None,
) -> tuple[bytes | None, tuple | None, str]:
    """Read one safe tail; ``None`` data with a fingerprint means unchanged."""
    try:
        before = os.lstat(src)
        if not stat.S_ISREG(before.st_mode) or before.st_uid != owner_uid:
            return None, None, "unsafe"
        flags = os.O_RDONLY | os.O_NONBLOCK
        flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        fd = os.open(src, flags)
    except FileNotFoundError:
        return None, None, "waiting"
    except PermissionError:
        data, status = _read_log_as_owner(src, owner_uid) if limit is None else _read_log_as_owner(src, owner_uid, limit)
        if data is None:
            return None, None, status
        fp = ("owner-tail", len(data), hashlib.blake2b(data, digest_size=16).digest())
        return (None if fp == known else data), fp, status
    except OSError:
        return None, None, "unreadable"
    try:
        with os.fdopen(fd, "rb") as source:
            st = os.fstat(source.fileno())
            if (
                not stat.S_ISREG(st.st_mode)
                or st.st_uid != owner_uid
                or (st.st_dev, st.st_ino) != (before.st_dev, before.st_ino)
            ):
                return None, None, "unsafe"
            fp = (
                st.st_dev, st.st_ino, st.st_size,
                st.st_mtime_ns, st.st_ctime_ns,
            )
            if fp == known:
                return None, fp, "mirrored"
            cap = LOG_TAIL_BYTES if limit is None else limit
            if st.st_size > cap:
                source.seek(st.st_size - cap)
            return source.read(cap), fp, "mirrored"
    except OSError:
        return None, None, "unreadable"


def _set_log_status(
    jid: str, suffix: str, status: str, token: object | None = None,
) -> None:
    with _log_lock:
        if jid in _log_live and (
            token is None or _log_seed_tokens.get(jid) is token
        ):
            _log_status.setdefault(jid, {})[suffix] = status


def _log_spool_path(jid: str, suffix: str) -> Path | None:
    """A validated direct child of the spool, never a caller-shaped path."""
    if not valid_slurm_job_id(jid) or suffix not in ("out", "err"):
        return None
    return LOG_SPOOL_DIR / f"{jid}.{suffix}"


def _unpublish_log_stream(
    jid: str, suffix: str, token: object | None = None,
) -> None:
    """Remove a mirror that is no longer backed by a safe source."""
    dst = _log_spool_path(jid, suffix)
    if dst is None:
        return
    with _log_lock:
        if token is not None and _log_seed_tokens.get(jid) is not token:
            return
        published = _log_published.get(jid)
        if published is not None:
            published.pop(suffix, None)
            if not published:
                _log_published.pop(jid, None)
        _log_fingerprint.pop(str(dst), None)
        try:
            dst.unlink()
        except OSError:
            pass


def _mirror_one_job_log(jid: str) -> None:
    """Copy the tail of one job's stdout/stderr into the shared spool."""
    try:
        if not valid_slurm_job_id(jid):
            return
        with _log_lock:
            paths = _log_paths.get(jid)
            owner_uid = _log_owner_uids.get(jid)
            token = _log_seed_tokens.get(jid)
            if token is None:
                token = object()
                _log_seed_tokens[jid] = token
        if owner_uid is None or owner_uid < 0:
            _set_log_status(jid, "out", "untrusted-owner", token)
            _set_log_status(jid, "err", "untrusted-owner", token)
            return
        if paths is None:
            # Never recover privileged paths from scontrol's line-oriented
            # output: JobName is printed verbatim and can forge another line.
            _set_log_status(jid, "out", "metadata-unavailable", token)
            _set_log_status(jid, "err", "metadata-unavailable", token)
            return
        unchanged = True
        for src, suffix in ((paths[0], "out"), (paths[1], "err")):
            if not src:
                with _log_lock:
                    prior = _log_status.get(jid, {}).get(suffix)
                _set_log_status(
                    jid, suffix,
                    prior if suffix == "err" and prior == "merged"
                    else "not-configured",
                    token,
                )
                continue
            dst = _log_spool_path(jid, suffix)
            if dst is None:  # defense in depth; jid/suffix were checked above
                continue
            with _log_lock:
                known = _log_fingerprint.get(str(dst)) if dst.is_file() else None
            data, fp, status = _read_log_source(src, owner_uid, known)
            unchanged = unchanged and fp is not None and fp == known and data is None
            _set_log_status(jid, suffix, status, token)
            if fp is None:
                # Do not keep serving a stale world-readable tail after its
                # source vanished, became unreadable, or failed validation.
                _unpublish_log_stream(jid, suffix, token)
                continue
            if data is None and dst.is_file():
                with _log_lock:
                    if jid in _log_live \
                            and _log_seed_tokens.get(jid) is token:
                        _log_published.setdefault(jid, {})[suffix] = str(dst)
                continue
            if data is None:
                continue
            # raw bytes, not tail_file's decorated text: readers run their own
            # tail_file over this and `sgpu logs -f` tracks byte offsets
            with _log_lock:
                if jid not in _log_live \
                        or _log_seed_tokens.get(jid) is not token:
                    continue
                # Serialize the publish with live-set removal. If a job exits
                # during a mirror, either this finishes before cleanup (which
                # then unlinks it) or cleanup wins and this write is skipped.
                atomic_write(dst, data, mode=0o644)
                _log_fingerprint[str(dst)] = fp
                _log_published.setdefault(jid, {})[suffix] = str(dst)
        with _log_lock:
            if jid in _log_live and _log_seed_tokens.get(jid) is token:
                count = min(6, _log_unchanged.get(jid, 0) + 1) if unchanged else 0
                _log_unchanged[jid] = count
                _log_next_check[jid] = time.monotonic() + min(LOG_MIRROR_MAX_SEC, LOG_MIRROR_SEC * 2 ** count)
    except Exception as e:
        print(f"[collector] log mirror {jid} failed: {e}", flush=True)
    finally:
        with _log_lock:
            _log_inflight.discard(jid)


def _drop_log_spool(jid: str) -> None:
    with _log_lock:
        _log_published.pop(jid, None)
        _log_next_check.pop(jid, None)
        _log_unchanged.pop(jid, None)
        _log_owner_uids.pop(jid, None)
        _log_status.pop(jid, None)
        _log_seed_tokens.pop(jid, None)
        _log_live.discard(jid)
    for suffix in ("out", "err"):
        p = _log_spool_path(jid, suffix)
        if p is None:
            continue
        with _log_lock:
            _log_fingerprint.pop(str(p), None)
        try:
            p.unlink()
        except OSError:
            pass


def _clear_stale_log_spool() -> None:
    """Remove bounded-tail files left by a crashed/aborted collector."""
    for pattern in ("*.out", "*.err"):
        for path in LOG_SPOOL_DIR.glob(pattern):
            try:
                path.unlink()  # unlinks a planted symlink; never follows it
            except OSError:
                pass


def _prepare_log_spool() -> bool:
    """Secure/clean the spool, including remnants after sharing is disabled."""
    if not SHARE_LOGS and not LOG_SPOOL_DIR.exists():
        return True
    try:
        ensure_secure_dir(LOG_SPOOL_DIR)
    except UnsafeRuntimeDir as exc:
        print(f"[collector] {exc}", flush=True)
        return False
    _clear_stale_log_spool()
    return True


def _prepare_state_dir() -> None:
    """Secure persistent state and mark only a directory created by sgpu."""
    existed = STATE_DIR.exists()
    ensure_secure_dir(STATE_DIR)
    if not existed:
        atomic_write(STATE_MARKER_FILE, "sgpu\n", mode=0o644)


def _share_logs(jobs: List[JobInfo]) -> Dict[str, dict]:
    """Refresh the shared log spool; return jobid -> {"out"/"err": path}."""
    if not SHARE_LOGS:
        return {}
    # Reuse private metadata parsed from the root collector's combined
    # scontrol result instead of issuing one scheduler RPC per job. These
    # paths/UIDs are dynamic attributes and cannot leak through asdict.
    seeds = []
    invalid_seeds = []
    for job in jobs:
        paths = getattr(job, "_log_paths", None)
        owner_uid = job.uid
        if valid_slurm_job_id(job.jobid) \
                and paths is not None and owner_uid >= 0:
            seeds.append((
                job.jobid, paths, owner_uid,
                bool(getattr(job, "_log_stderr_merged", False)),
            ))
        elif valid_slurm_job_id(job.jobid):
            invalid_seeds.append(job.jobid)
    live = {j.jobid for j in jobs if valid_slurm_job_id(j.jobid)}
    now = time.monotonic()
    reset_streams = []
    with _log_lock:
        # A live queue row without a freshly validated owner/path record must
        # invalidate the previous cycle. Otherwise a transient scheduler
        # failure would keep publishing and refreshing a stale privileged path.
        for jid in invalid_seeds:
            _log_unchanged.pop(jid, None)
            _log_seed_tokens[jid] = object()
            token = _log_seed_tokens[jid]
            reset_streams.extend(
                (jid, suffix, token) for suffix in ("out", "err")
            )
            _log_paths.pop(jid, None)
            _log_owner_uids.pop(jid, None)
            _log_next_check.pop(jid, None)
            _log_status[jid] = {
                "out": "metadata-unavailable", "err": "metadata-unavailable",
            }
        for jid, paths, owner_uid, stderr_merged in seeds:
            previous_paths = _log_paths.get(jid)
            previous_owner = _log_owner_uids.get(jid)
            seed_changed = previous_paths is not None and (
                previous_paths != paths or previous_owner != owner_uid
            )
            if seed_changed:
                _log_unchanged.pop(jid, None)
                _log_seed_tokens[jid] = object()
                token = _log_seed_tokens[jid]
                reset_streams.extend(
                    (jid, suffix, token) for suffix in ("out", "err")
                )
                _log_status.pop(jid, None)
            elif jid not in _log_seed_tokens:
                _log_seed_tokens[jid] = object()
            _log_paths[jid] = paths
            _log_owner_uids[jid] = owner_uid
            statuses = _log_status.setdefault(jid, {})
            if not paths[0]:
                statuses["out"] = "not-configured"
            if not paths[1]:
                statuses["err"] = (
                    "merged" if stderr_merged else "not-configured"
                )
        known = set(_log_paths) | set(_log_owner_uids) | set(_log_published) \
            | set(_log_status) | set(_log_next_check) | set(_log_live)
        gone = [jid for jid in known if jid not in live]
        _log_live.clear()
        _log_live.update(live)
        trusted = {jid for jid, *_rest in seeds}
        todo = [
            jid for jid in trusted
            if jid not in _log_inflight
            and now >= _log_next_check.get(jid, 0.0)
        ]
        _log_inflight.update(todo)
        for jid in todo:
            _log_next_check[jid] = now + LOG_MIRROR_SEC
    for jid, suffix, token in reset_streams:
        _unpublish_log_stream(jid, suffix, token)
    for jid in gone:
        with _log_lock:
            _log_paths.pop(jid, None)
        _drop_log_spool(jid)
    for jid in todo:
        _log_executor.submit(_mirror_one_job_log, jid)
    with _log_lock:
        return {
            j.jobid: dict(
                _log_published.get(j.jobid, {}),
                status=dict(_log_status.get(j.jobid, {})),
            )
            for j in jobs
            if valid_slurm_job_id(j.jobid)
            and (_log_published.get(j.jobid) or _log_status.get(j.jobid))
        }


# ── Per-user GPU-hour accounting ──────────────────────────────────────────
# Daily buckets: {"days": {"YYYY-MM-DD": {user: {"alloc": sec, "busy": sec}}}}
# alloc = GPU allocated to the user's job; busy = that GPU actually computing.
# Rolling window, sampled each cycle and checkpointed on a bounded cadence.
#
# Two alloc sources:
#   days       — 3s sampling (loses time whenever the collector is down)
#   sacct_days — {"YYYY-MM-DD": {user: alloc_sec}} rebuilt from slurmdbd,
#                which records jobs even while the collector is dead.
# Readers take max(sampled, sacct) per user-day. busy has no slurmdbd
# equivalent here (needs an acct_gather GPU plugin), so it stays sampled.

USAGE_FILE = STATE_DIR / "usage.json"
USAGE_KEEP_DAYS = int(os.getenv("SLURM_GPU_TUI_USAGE_KEEP_DAYS", "30"))
WASTE_MIN_SEC = int(os.getenv("SLURM_GPU_TUI_WASTE_MIN_SEC", "600"))
# Sampling remains per collector cycle, but a durable full-history checkpoint
# every cycle needlessly fsyncs the same growing file thousands of times/day.
USAGE_SAVE_SEC = max(1.0, float(os.getenv("SLURM_GPU_TUI_USAGE_SAVE_SEC", "30")))
# 0 disables slurmdbd backfill
SACCT_BACKFILL_SEC = int(os.getenv("SLURM_GPU_TUI_SACCT_SEC", "3600"))
_usage: Dict[str, dict] = {"days": {}}
_last_usage_ts: float | None = None
_usage_lock = threading.Lock()
_usage_dirty = False
_usage_last_save = 0.0
_sacct_inflight = False
_sacct_last_attempt = 0.0
_sacct_failures = 0  # consecutive; disables backfill on clusters without slurmdbd
_SACCT_MAX_FAILURES = 3


def _load_usage() -> None:
    raw = _read_state_json(USAGE_FILE)
    if not isinstance(raw, dict):
        return
    if isinstance(raw.get("days"), dict):
        _usage["days"] = raw["days"]
    if isinstance(raw.get("meta"), dict):
        _usage["meta"] = raw["meta"]
    if isinstance(raw.get("sacct_days"), dict):
        _usage["sacct_days"] = raw["sacct_days"]
        _usage["sacct_ts"] = raw.get("sacct_ts")


def _save_usage(force: bool = False, monotonic_now: float | None = None) -> bool:
    """Checkpoint sampled usage, normally at a lower cadence than sampling."""
    global _usage_dirty, _usage_last_save
    now = time.monotonic() if monotonic_now is None else monotonic_now
    with _usage_lock:
        if not _usage_dirty:
            return False
        if not force and _usage_last_save and now - _usage_last_save < USAGE_SAVE_SEC:
            return False
        # Serialize while holding the lock: the hourly sacct worker replaces
        # nested history concurrently, and a torn snapshot is not acceptable.
        payload = json.dumps(_usage)
        if not _write_state_json(USAGE_FILE, payload):
            return False  # retry next collector cycle
        _usage_dirty = False
        _usage_last_save = now
        return True


def _accumulate_usage(result_nodes: List[dict], now: float) -> None:
    global _last_usage_ts, _usage_dirty
    with _usage_lock:
        prev, _last_usage_ts = _last_usage_ts, now
        if prev is None:
            return
        dt = now - prev
        if not (0 < dt <= 60):
            return  # collector was paused; don't credit the gap
        day = datetime.now().strftime("%Y-%m-%d")
        # coverage meta: how many seconds this sampling accounting actually saw
        meta = _usage.setdefault("meta", {})
        meta[day] = meta.get(day, 0) + dt
        bucket = _usage["days"].setdefault(day, {})
        for n in result_nodes:
            for g in n.get("gpus", []):
                user = g.get("alloc_user") or (g.get("users") or [""])[0]
                if not user:
                    continue
                u = bucket.setdefault(user, {"alloc": 0, "busy": 0})
                u.setdefault("observed_alloc", u.get("alloc", 0))
                u["alloc"] += dt
                coverage = _usage.setdefault("telemetry", {}).setdefault(day, {"fresh_gpu_sec": 0.0, "stale_gpu_sec": 0.0})
                fresh = not n.get("stale", False)
                coverage["fresh_gpu_sec" if fresh else "stale_gpu_sec"] += dt
                if not fresh:
                    continue
                u["observed_alloc"] += dt
                try:
                    if float(g.get("util") or 0) > 5:
                        u["busy"] += dt
                except (ValueError, TypeError):
                    pass
                # waste = allocated but idle (no process) or parked (VRAM held,
                # no compute). Same threshold as the TUI waste view, so short
                # startup/data-loading lulls don't count.
                if max(g.get("idle_sec", 0), g.get("parked_sec", 0)) >= WASTE_MIN_SEC:
                    u["waste"] = u.get("waste", 0) + dt
        cutoff = (datetime.now() - timedelta(days=USAGE_KEEP_DAYS)).strftime("%Y-%m-%d")
        for d in [d for d in _usage["days"] if d < cutoff]:
            del _usage["days"][d]
        for d in [d for d in _usage.get("meta", {}) if d < cutoff]:
            del _usage["meta"][d]
        for d in [d for d in _usage.get("telemetry", {}) if d < cutoff]:
            del _usage["telemetry"][d]
        _usage_dirty = True


def _parse_sacct_time(s: str) -> float | None:
    if not s or s in ("Unknown", "None", "N/A"):
        return None
    try:
        return datetime.strptime(s, "%Y-%m-%dT%H:%M:%S").timestamp()
    except ValueError:
        return None


def _gpu_count_from_tres(tres: str) -> int:
    m = re.search(r"(?:^|,)gres/gpu=(\d+)", tres)
    if m:
        return int(m.group(1))
    # some setups only record typed GRES (gres/gpu:a6000=2)
    return sum(int(n) for n in re.findall(r"(?:^|,)gres/gpu:[^=,]+=(\d+)", tres))


def _sacct_backfill(now: float) -> bool:
    """Rebuild per-day alloc GPU-seconds from slurmdbd (authoritative)."""
    global _sacct_failures
    start_dt = datetime.now() - timedelta(days=USAGE_KEEP_DAYS)
    cutoff = start_dt.strftime("%Y-%m-%d")
    ok, out = run_cmd(
        "sacct -a -X --noheader --parsable2 --format=User,AllocTRES,Start,End "
        f"-S {start_dt.strftime('%Y-%m-%dT00:00:00')}", timeout=60)
    if not ok:
        _sacct_failures += 1
        print(f"[collector] sacct backfill failed ({_sacct_failures}/{_SACCT_MAX_FAILURES}): "
              f"{out.splitlines()[0][:100] if out else 'no output'}", flush=True)
        if _sacct_failures >= _SACCT_MAX_FAILURES:
            print("[collector] disabling sacct backfill (no slurmdbd/accounting?) — "
                  "alloc stays sampling-based", flush=True)
        return False
    _sacct_failures = 0
    days: Dict[str, Dict[str, float]] = {}
    for line in out.splitlines():
        parts = line.split("|")
        if len(parts) != 4:
            continue
        user, tres, s_start, s_end = parts
        ngpu = _gpu_count_from_tres(tres)
        if not user or ngpu <= 0:
            continue
        t0 = _parse_sacct_time(s_start)
        t1 = _parse_sacct_time(s_end) or now  # still running
        t1 = min(t1, now)
        if t0 is None or t1 <= t0:
            continue
        # split the job's [t0, t1) across day boundaries
        cur = t0
        while cur < t1:
            d = datetime.fromtimestamp(cur)
            day_end = datetime(d.year, d.month, d.day).timestamp() + 86400
            day_key = d.strftime("%Y-%m-%d")
            seg = min(t1, day_end) - cur
            if day_key >= cutoff:
                bucket = days.setdefault(day_key, {})
                bucket[user] = bucket.get(user, 0.0) + ngpu * seg
            cur = day_end
    global _usage_dirty
    with _usage_lock:
        _usage["sacct_days"] = days
        _usage["sacct_ts"] = now
        _usage_dirty = True
    print(f"[collector] sacct backfill: {len(days)} day(s), "
          f"{sum(len(u) for u in days.values())} user-day rows", flush=True)
    return True


def _maybe_backfill_sacct(now: float) -> None:
    """Spawn a background sacct refresh when the last one is old enough."""
    global _sacct_inflight, _sacct_last_attempt
    if SACCT_BACKFILL_SEC <= 0 or _sacct_inflight:
        return
    if _sacct_failures >= _SACCT_MAX_FAILURES:
        return
    if now - float(_usage.get("sacct_ts") or 0) < SACCT_BACKFILL_SEC:
        return
    # failed attempts don't advance sacct_ts; don't hammer a broken sacct
    if now - _sacct_last_attempt < min(SACCT_BACKFILL_SEC, 900):
        return
    _sacct_last_attempt = now
    _sacct_inflight = True

    def worker() -> None:
        global _sacct_inflight
        try:
            if _sacct_backfill(time.time()):
                _save_usage(force=True)
        except Exception as e:
            print(f"[collector] sacct backfill error: {e}", flush=True)
        finally:
            _sacct_inflight = False

    threading.Thread(target=worker, daemon=True, name="sacct-backfill").start()


# ── Adaptive polling state ────────────────────────────────────────────────

_node_poll_state: Dict[str, Dict] = {}
_INTERVAL_HOT = 5    # active node: poll every 5s
_INTERVAL_COLD = 20  # idle node: poll every 20s
_INTERVAL_DOWN = 60  # down/drain node: poll every 60s


def _should_poll_node(name: str) -> bool:
    now = time.monotonic()
    state = _node_poll_state.get(name, {"last_poll": 0.0, "interval": _INTERVAL_HOT})
    return (now - state["last_poll"]) >= state["interval"]


def _update_poll_state(name: str, success: bool, node_is_cold: bool, slurm_state: str) -> None:
    now = time.monotonic()
    state = _node_poll_state.setdefault(name, {
        "last_poll": 0.0, "interval": _INTERVAL_HOT,
        "consecutive_failures": 0, "last_ok": 0.0
    })
    state["last_poll"] = now
    s = slurm_state.lower()
    if "down" in s or "drain" in s:
        state["interval"] = _INTERVAL_DOWN
    elif not success:
        state["consecutive_failures"] = state.get("consecutive_failures", 0) + 1
        state["interval"] = min(_INTERVAL_DOWN, _INTERVAL_HOT * (2 ** state["consecutive_failures"]))
    elif node_is_cold:
        state["consecutive_failures"] = 0
        state["last_ok"] = now
        state["interval"] = _INTERVAL_COLD
    else:
        state["consecutive_failures"] = 0
        state["last_ok"] = now
        state["interval"] = _INTERVAL_HOT


# asdict, not a hand-listed subset. The old hand-written version omitted
# pid_mem and pid_jobid, so SSH-polled nodes reached reconcile_gpu_alloc with
# no cgroup jobids at all — the exact-attribution step silently never ran for
# them, and `sgpu doctor`'s gpu-job binding check skipped them too.
def _gpu_to_dict(gpu: GpuInfo) -> dict:
    return asdict(gpu)


def _job_to_dict(job: JobInfo) -> dict:
    return asdict(job)


def _node_job_to_dict(job: JobInfo) -> dict:
    """Compact copy for per-node rows; full detail lives in top-level jobs."""
    out = asdict(job)
    for name in ("detail", "script", "log_out", "log_err", "log_status", "telemetry"):
        out.pop(name, None)
    return out


def _published_job_to_dict(job: JobInfo, script: str, logs: dict) -> dict:
    """World-readable top-level job record, behind explicit share gates."""
    return dict(
        _job_to_dict(job),
        script=script,
        detail=job.detail if SHARE_JOB_DETAILS else "",
        log_out=logs.get("out", ""),
        log_err=logs.get("err", ""),
        log_status=logs.get("status", {}),
    )


def _pending_to_dict(pj: PendingJob) -> dict:
    return dict(
        asdict(pj),
        detail=pj.detail if SHARE_JOB_DETAILS else "",
    )


_agent_build_cache: tuple = (0.0, "0")  # (checked monotonic ts, value)


def _expected_agent_build() -> str:
    """Current agent.py fingerprint, read live so upgrades are noticed even
    if this collector predates them (relaunched agents then match again).
    Cached 60s — this ran one stat per node per 3s cycle."""
    global _agent_build_cache
    now = time.monotonic()
    if now - _agent_build_cache[0] > 60:
        try:
            v = str(int(os.path.getmtime(_agent_module.__file__)))
        except OSError:
            v = "0"
        _agent_build_cache = (now, v)
    return _agent_build_cache[1]


_agent_payload_cache: Dict[str, tuple] = {}  # name -> (mtime, expected kind, payload or None)
# Nodes whose payload was rejected for authorship; also reported by `sgpu doctor`
_untrusted_payloads: Dict[str, int] = {}  # node -> writing uid
_untrusted_warned: set = set()


def _payload_author_trusted(name: str, uid: int) -> bool:
    """Is `uid` allowed to speak for node `name`?

    AGENT_DIR is mode 1777, so any user can create <node>.json. Shape
    validation cannot tell a real agent from a forgery, and a forged payload
    drives Slack alerts, the waste view, and GPU-hour accounting.
    """
    if uid in trusted_payload_uids(AGENT_DIR):
        return True
    _untrusted_payloads[name] = uid
    if name not in _untrusted_warned:
        _untrusted_warned.add(name)
        print(f"[collector] ignoring {name}.json: written by uid {uid}, not a "
              f"trusted agent account — falling back to SSH poll. Set "
              f"SLURM_GPU_TUI_AGENT_TRUSTED_UIDS if this uid is legitimate.",
              flush=True)
    return False


def _valid_agent_payload(name: str, payload: object, expected_kind: str | None = None) -> bool:
    """Validate the push payload shape before it reaches the merge loop."""
    if not isinstance(payload, dict) or payload.get("hostname") != name:
        return False
    kind = payload.get("node_kind")
    if kind not in ("gpu", "cpu") or (expected_kind and kind != expected_kind):
        return False
    if not isinstance(payload.get("agent_build"), str):
        return False
    if not isinstance(payload.get("ts"), (int, float)):
        return False
    mem = payload.get("mem")
    if not isinstance(mem, dict) or not all(k in mem for k in ("total", "used", "avail")):
        return False
    gpus = payload.get("gpus")
    if not isinstance(gpus, list) or len(gpus) > 64:
        return False
    if (kind == "gpu" and not gpus) or (kind == "cpu" and gpus):
        return False
    seen = set()
    for gpu in gpus:
        if not isinstance(gpu, dict):
            return False
        if not all(k in gpu for k in ("index", "name", "mem_total", "pids", "users")):
            return False
        index = str(gpu["index"])
        if not index or index in seen:
            return False
        seen.add(index)
        if not isinstance(gpu["pids"], list) or not isinstance(gpu["users"], list):
            return False
        if any(not isinstance(gpu.get(key, ""), str) or len(gpu.get(key, "")) > 128
               for key in ("clock_reasons", "recovery_action")):
            return False
        if "health_observed_at" in gpu:
            stamp = clean_sample({"observed_at": gpu["health_observed_at"]}).get("observed_at")
            if stamp is None:
                return False
    return True


def _read_agent_payload(name: str, expected_kind: str = "gpu") -> dict | None:
    """Return a node's push-agent payload if fresh and version-compatible.
    Parsed payloads are cached by mtime — agents rewrite at their configured interval,
    so most 3s cycles can skip the read+parse."""
    p = AGENT_DIR / f"{name}.json"
    try:
        file_stat = p.lstat()
        if not stat.S_ISREG(file_stat.st_mode):
            return None  # symlink or fifo planted in the 1777 agent dir
        if not 0 < file_stat.st_size <= AGENT_PAYLOAD_MAX_BYTES:
            return None
        if not _payload_author_trusted(name, file_stat.st_uid):
            return None
        _untrusted_payloads.pop(name, None)
        mtime = file_stat.st_mtime
        # mtime is stamped by the NFS server (= this host), so no clock skew
        if time.time() - mtime > AGENT_MAX_AGE:
            return None
        cached = _agent_payload_cache.get(name)
        if cached is not None and cached[0] == mtime and cached[1] == expected_kind:
            return cached[2]
        payload = json.loads(p.read_text())
        if not _valid_agent_payload(name, payload, expected_kind):
            payload = None
        elif payload.get("agent_version") != AGENT_PAYLOAD_VERSION:
            payload = None  # old agent — treated as stale, repair will upgrade it
        elif not AGENT_DISABLE and payload.get("agent_build") != _expected_agent_build():
            payload = None  # agent runs outdated code — repair restarts it
        _agent_payload_cache[name] = (mtime, expected_kind, payload)
        return payload
    except Exception:
        return None


_agent_repair_ts: Dict[str, float] = {}
_AGENT_BIN = Path(sys.executable).parent / "sgpu-agent"


def _maybe_repair_agent(name: str) -> None:
    """(Re)launch the push agent on a node via SSH, rate-limited per node.

    The venv lives on the shared FS, so nodes exec the same binary path.
    Also upgrades agents left running with an old payload version.
    """
    if AGENT_DISABLE or not _AGENT_BIN.exists():
        return
    now = time.monotonic()
    if now - _agent_repair_ts.get(name, 0.0) < AGENT_REPAIR_SEC:
        return
    _agent_repair_ts[name] = now

    def _run() -> None:
        # Kill and launch MUST be separate ssh commands: combined, the launch
        # path 'bin/sgpu-agent' appears in the shell's own cmdline and pkill
        # kills the shell (and with it the relaunch). The [s] bracket keeps
        # the kill command itself from self-matching.
        ssh_cmd(name, 'pkill -f "bin/[s]gpu-agent" 2>/dev/null || true', timeout=15)
        time.sleep(1)
        # Pass our AGENT_DIR to the remote agent: an SSH launch does NOT inherit
        # the collector's env, and the agent's own default (~/.sgpu/nodes) is
        # relative to the SSH user's home — which differs from the collector's
        # when it runs as a system service. Both sides must use the same shared
        # dir or the collector never sees the payloads (silent SSH-pull).
        launch = f"SLURM_GPU_TUI_AGENT_DIR={shlex.quote(str(AGENT_DIR))} {_AGENT_BIN} --daemon"
        ok, out = ssh_cmd(name, launch, timeout=25)
        if not ok and "No such file" in out:
            # Install dir isn't visible from this node (not a shared FS):
            # push mode can't work there — stop retrying, SSH pull covers it
            _agent_repair_ts[name] = float("inf")
            print(f"[collector] agent repair {name}: venv not on node, "
                  "push disabled for this node (SSH pull fallback)", flush=True)
            return
        print(f"[collector] agent repair {name}: {'ok' if ok else out}", flush=True)

    def _run_logged() -> None:
        # An exception escaping into the executor is swallowed silently (the
        # Future is never inspected) — the repair just stops happening with no
        # trace in the journal. Surface it instead.
        try:
            _run()
        except Exception as e:
            print(f"[collector] agent repair {name} crashed: {e!r}", flush=True)

    _repair_executor.submit(_run_logged)


def _poll_node_bg(n: dict, has_jobs: bool) -> bool:
    """Submit a background SSH poll for one node unless one is already in flight."""
    name, slurm_state = n["name"], n["state"]
    with _results_lock:
        if name in _inflight or len(_inflight) >= MAX_PENDING_POLLS:
            return False
        _inflight.add(name)

    def _run() -> None:
        try:
            gpus, mem, err = collect_node_data(name, NODE_TIMEOUT)
        except Exception as e:
            gpus, mem, err = [], NodeMemInfo(), f"collect failed: {e}"
        gpu_dicts = [_gpu_to_dict(g) for g in gpus]
        mem_dict = {"total": mem.total, "used": mem.used, "avail": mem.avail}
        node_is_cold = False
        with _results_lock:
            prev = _node_results.get(name)
            if err and prev and not prev.get("error"):
                # Keep last good data, mark stale
                prev["stale"] = True
                prev["error_kind"] = NodeErrorKind.STALE_CACHED.value
            elif err:
                _node_results[name] = {
                    "gpus": [], "mem": {}, "error": err,
                    "error_kind": _classify_error(err).value, "stale": False,
                }
            else:
                _node_results[name] = {
                    "gpus": gpu_dicts, "mem": mem_dict, "error": "",
                    "error_kind": NodeErrorKind.OK.value, "stale": False,
                    "observed_at": time.time(),
                    "telemetry": {"pressure": mem.pressure, "observed_at": time.time()},
                }
                node_is_cold = (
                    all(g.util in ("0", "", "N/A") for g in gpus) and not has_jobs
                )
            _update_poll_state(name, success=not err, node_is_cold=node_is_cold, slurm_state=slurm_state)
            _inflight.discard(name)
        if not err:
            _update_inventory(name, gpu_dicts)

    try:
        _node_executor.submit(_run)
    except Exception:
        with _results_lock:
            _inflight.discard(name)
        raise
    return True


def _effective_mem_total(mem: object, slurm_total: str) -> str:
    """Prefer live OS RAM over Slurm RealMemory when the payload has it."""
    if isinstance(mem, dict):
        live_total = mem.get("total")
        try:
            if float(live_total) > 0:
                return str(live_total)
        except (TypeError, ValueError):
            pass
    return slurm_total


def _prune_node_caches(live_names: set[str]) -> None:
    """Drop transient data after a node misses consecutive Slurm rosters.

    Inventory is intentionally retained so a temporarily missing node keeps
    its learned GPU layout. A one-cycle grace also prevents a partial sinfo
    result from immediately discarding last-good telemetry.
    """
    with _results_lock:
        tracked = set(_node_results) | set(_node_poll_state) | set(_inflight)
    tracked.update(_agent_payload_cache)
    tracked.update(_untrusted_payloads)
    tracked.update(_untrusted_warned)
    tracked.update(_node_absent_cycles)

    for name in live_names:
        _node_absent_cycles.pop(name, None)
    for name in tracked - live_names:
        _node_absent_cycles[name] = _node_absent_cycles.get(name, 0) + 1
    retired = {
        name for name, count in _node_absent_cycles.items()
        if count >= _NODE_CACHE_PRUNE_AFTER
    }
    if not retired:
        return

    with _results_lock:
        for name in retired:
            _node_results.pop(name, None)
        for name in retired:
            _node_poll_state.pop(name, None)
    for cache in (_agent_payload_cache, _untrusted_payloads):
        for name in retired:
            cache.pop(name, None)
    for name in retired:
        _untrusted_warned.discard(name)
    for name in retired:
        _node_absent_cycles.pop(name, None)


def _attach_telemetry(jobs, node_jobs, nodes, results, scheduler_error="") -> None:
    now = time.time()
    for job in jobs:
        job.telemetry = {}
    for node in nodes:
        name = node["name"]
        node.update(cpu_util="", pressure={}, telemetry_observed_at=0.0, jobs_truncated=False)
        telemetry = clean_telemetry(results.get(name, {}).get("telemetry"))
        observed = telemetry.get("observed_at", 0)
        fresh = not node["stale"] and observed > 0 and -5 <= now - observed <= TELEMETRY_MAX_AGE
        if fresh:
            node.update(cpu_util=telemetry.get("cpu_util", ""), pressure=telemetry.get("pressure", {}),
                        telemetry_observed_at=observed, jobs_truncated=telemetry.get("jobs_truncated", False))
        if scheduler_error or node["stale"]:
            continue
        for job in node_jobs.get(name, []):
            if job.uid < 0:
                continue
            aliases = {job.jobid}
            from .common import _published_detail_field
            canonical = _published_detail_field(job.detail, "JobId")
            if canonical.isascii() and canonical.isdigit():
                aliases.add(canonical)
            candidates = [telemetry.get("jobs", {}).get(jid) for jid in aliases]
            sample = next((clean_sample(value) for value in candidates if value), {}) if fresh else {}
            stamp = sample.get("observed_at", 0)
            if not stamp or not -5 <= now - stamp <= TELEMETRY_MAX_AGE:
                sample = {}
            amounts = [float(amount) for gpu in node["gpus"] for pid, amount in gpu.get("pid_mem", {}).items()
                       if gpu.get("pid_jobid", {}).get(pid) in aliases and re.fullmatch(r"[0-9]{1,12}(?:\.[0-9]+)?", str(amount))]
            if amounts and node.get("observed_at", 0) > 0:
                sample["vram_mib"] = sum(amounts)
                sample["observed_at"] = min(sample.get("observed_at", node["observed_at"]), node["observed_at"])
            if len(sample) > 1:
                job.telemetry[name] = sample


def collect_all(*, background_basic: bool = False) -> dict:
    """One collection cycle: fast local data + latest async node results.

    Node SSH polls run in the background and never block this cycle — a dead
    node only goes stale, it cannot stall data for healthy nodes.
    """
    (
        nodes_raw, jobs, pending, node_jobs_from_basic, gpu_alloc,
        alloc_user_map, scheduler_status, basic_err,
    ) = _scheduler_sampler.read() if background_basic else collect_basic()
    # An empty roster can mean sinfo failed, so retain caches in that case.
    # Repeated non-empty snapshots retire renamed or decommissioned nodes;
    # one partial snapshot keeps the last-good telemetry.
    if nodes_raw:
        _prune_node_caches({n["name"] for n in nodes_raw})

    node_jobs: Dict[str, List[dict]] = {
        k: [_node_job_to_dict(j) for j in v]
        for k, v in node_jobs_from_basic.items()
    }
    # The validated scheduler owner map resolves array-task jobids (38182_0 in
    # squeue vs the real 38192 in the alloc) and carries the canonical name.
    jobid_user = {j.jobid: j.user for j in jobs}
    jobid_user.update({k: v for k, v in alloc_user_map.items() if v})

    # Prefer push-agent payloads (local NFS read, every cycle). GPU agents are
    # collector-repaired; CPU agents are systemd-managed on their node. Either
    # kind falls back to async SSH when its payload is absent or stale.
    agent_nodes: set = set()
    global _poll_cursor, _notification_log_sources
    start = _poll_cursor % len(nodes_raw) if nodes_raw else 0
    poll_order = nodes_raw[start:] + nodes_raw[:start]
    _poll_cursor = start
    for offset, n in enumerate(poll_order):
        name = n["name"]
        has_jobs = name in node_jobs_from_basic
        has_gpu = n.get("has_gpu", True)
        payload = _read_agent_payload(name, "gpu" if has_gpu else "cpu")
        if payload is not None:
            agent_nodes.add(name)
            gpu_dicts = payload.get("gpus", [])
            with _results_lock:
                _node_results[name] = {
                    "gpus": gpu_dicts, "mem": payload.get("mem", {}),
                    "power": payload.get("power", {}),
                    "telemetry": clean_telemetry(payload.get("telemetry")),
                    "error": "", "error_kind": NodeErrorKind.OK.value, "stale": False,
                    "observed_at": float(_agent_payload_cache.get(name, (0,))[0]),
                }
                node_is_cold = not has_jobs and (
                    not has_gpu
                    or all(g.get("util") in ("0", "", "N/A") for g in gpu_dicts)
                )
                # Mark polled so the SSH path stays quiet while the agent lives
                _update_poll_state(name, success=True, node_is_cold=node_is_cold, slurm_state=n["state"])
            _update_inventory(name, gpu_dicts)
            continue
        if _should_poll_node(name):
            if _poll_node_bg(n, has_jobs=has_jobs):
                _poll_cursor = start + offset + 1
        if has_gpu:
            _maybe_repair_agent(name)

    with _results_lock:
        results = {name: dict(r) for name, r in _node_results.items()}

    stale_nodes: List[str] = []
    result_nodes = []
    for n in nodes_raw:
        name = n["name"]
        r = results.get(name, {"gpus": [], "mem": {}, "error": "", "error_kind": NodeErrorKind.OK.value, "stale": False})
        now = time.time()
        observed_at = float(r.get("observed_at", 0))
        freshness_limit = max(AGENT_MAX_AGE, NODE_TIMEOUT + _node_poll_state.get(name, {}).get("interval", _INTERVAL_HOT))
        if observed_at and now - observed_at > freshness_limit:
            r = dict(r, stale=True, error_kind=NodeErrorKind.STALE_CACHED.value)
        skeleton_mode = False
        if not r["gpus"]:
            # No live data (cold start or unreachable node): render the known
            # GPU layout as placeholders instead of dropping the rows.
            skeleton = _skeleton_gpus(name, n.get("gres", ""))
            if skeleton:
                r = dict(r, gpus=skeleton, stale=True)
                skeleton_mode = True
                if r["error_kind"] == NodeErrorKind.OK.value:
                    r["error_kind"] = NodeErrorKind.STALE_CACHED.value
        if r["stale"]:
            stale_nodes.append(name)
        node_alloc = gpu_alloc.get(name, {})
        gpus = [dict(g) for g in r["gpus"]]
        for g in gpus:
            # node-side ps reports a bare UID when the node lacks the account;
            # resolve it here on the master, where the name service knows it
            if g.get("users"):
                g["users"] = [resolve_user(u) for u in g["users"]]
        # bind allocations to the cards their processes actually run on —
        # SLURM's IDX hint misplaces jobs on heterogeneous nodes
        alloc_pairs = reconcile_gpu_alloc(node_alloc, jobid_user, [
            ([u for u in g.get("users", []) if u not in ROGUE_IGNORE],
             g.get("minor") or g.get("index", ""),
             list(dict.fromkeys((g.get("pid_jobid") or {}).values())))
            for g in gpus])
        # strict: reconcile_gpu_alloc returns exactly one pair per GPU, and a
        # length mismatch would silently leave trailing cards unattributed
        for g, (jid, _user) in zip(gpus, alloc_pairs, strict=True):
            g["alloc_jobid"] = jid
            g["alloc_user"] = _user
            if skeleton_mode or r["stale"]:
                # Placeholder rows carry no process info — show previously
                # tracked waste ages but never start or reset the timers.
                key = f"{name}:{g.get('index', '')}"
                st = _idle_since.get(key)
                g["idle_sec"] = int(now - st["since"]) if st and jid and st.get("jobid") == jid else 0
                st = _parked_since.get(key)
                g["parked_sec"] = int(now - st["since"]) if st else 0
            else:
                _track_waste(name, g, now)
        mem = r["mem"]
        if name in agent_nodes:
            source = "agent"
        elif r["stale"]:
            source = "stale"
        else:
            source = "ssh"
        result_nodes.append({
            "name": name, "state": n["state"], "partition": n.get("partition", ""),
            "source": source, "has_gpu": n.get("has_gpu", True),
            "cpus": n["cpus"],
            "cpu_alloc": n.get("cpu_alloc", ""), "cpu_load": n["cpu_load"],
            "mem_total": _effective_mem_total(mem, n["mem_total"]),
            "mem_free": n["mem_free"],
            "mem_alloc": n.get("mem_alloc", ""), "gres": n["gres"],
            "mem_used": mem.get("used", ""), "mem_avail": mem.get("avail", ""),
            "cpu_power": r.get("power", {}).get("cpu", ""),
            "ram_power": r.get("power", {}).get("ram", ""),
            "sys_power": r.get("power", {}).get("sys", ""),
            "gpus": gpus, "jobs": node_jobs.get(name, []),
            "error": r["error"], "stale": r["stale"],
            "error_kind": r["error_kind"],
            "observed_at": float(r.get("observed_at", 0)),
            "data_age_sec": max(0.0, now - float(r["observed_at"])) if r.get("observed_at") else -1.0,
            "scheduler_age_sec": float(scheduler_status.get("age_sec", 0)),
            "scheduler_mem_total": n["mem_total"],
            "scheduler_available": not basic_err and scheduler_status.get("job_backend") != "unavailable",
            "cpu_util": "", "pressure": {}, "telemetry_observed_at": 0.0,
            "jobs_truncated": False,
        })

    _attach_telemetry(jobs, node_jobs_from_basic, result_nodes, results, basic_err)
    _accumulate_usage(result_nodes, time.time())

    scripts = _fetch_scripts(jobs)
    logs = _share_logs(jobs)
    _notification_log_sources = {
        job.jobid: (job.uid, tuple(job._log_paths)) for job in jobs
        if job.uid >= 0 and getattr(job, "_log_paths", None) is not None
    }
    return {
        "version": 1,
        "release": __version__,
        "build": __build__,
        "ts": datetime.now().isoformat(),
        "nodes": result_nodes,
        "jobs": [
            _published_job_to_dict(
                j, scripts.get(j.jobid, ""), logs.get(j.jobid, {}),
            )
            for j in jobs
        ],
        "pending": [_pending_to_dict(p) for p in pending],
        "stale_nodes": stale_nodes,
        # node -> uid of a rejected <node>.json, so `sgpu doctor` can name it
        "untrusted_payloads": dict(_untrusted_payloads),
        "scheduler": scheduler_status,
        "errors": basic_err,
        "collector": dict(_cycle_stats, ssh_inflight=len(_inflight),
                          log_inflight=len(_log_inflight), rpc=command_snapshot()),
    }


# ── Prometheus textfile exporter ──────────────────────────────────────────

# Prometheus textfile. Default sits next to data.json in DATA_DIR (/tmp),
# but node_exporter units often ship PrivateTmp=yes and then never see a
# /tmp file — point this at a shared path node_exporter can read in that case.
METRICS_FILE = Path(
    os.getenv("SLURM_GPU_TUI_METRICS_FILE", str(DATA_DIR / "metrics.prom"))
)
METRICS_REFRESH_SEC = max(
    float(REFRESH_SEC),
    float(os.getenv("SLURM_GPU_TUI_METRICS_SEC", "15")),
)
_metrics_last_write = 0.0
# Per-job pending series carry the jobid label: a deep queue (thousands of
# array rows whose compressed ids change as tasks start) would dominate the
# textfile and churn Prometheus series. sgpu_jobs_pending keeps the full count.
PENDING_METRICS_MAX = max(0, int(os.getenv("SLURM_GPU_TUI_METRICS_PENDING_MAX", "100")))


def _prom_escape(s: str) -> str:
    return str(s).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "")


def _format_metrics(data: dict) -> str:
    """Return a Prometheus textfile snapshot for the merged cluster state."""
    from .cells import node_gpu_classes, gpu_health_status, pending_wait_seconds
    from .common import node_from_dict, from_dict
    num = metric_number

    lines = [
        "# HELP sgpu_jobs_running Running Slurm jobs visible to sgpu",
        "# TYPE sgpu_jobs_running gauge",
        "# HELP sgpu_jobs_pending Pending Slurm jobs visible to sgpu",
        "# TYPE sgpu_jobs_pending gauge",
        "# HELP sgpu_nodes_total Nodes visible to sgpu",
        "# TYPE sgpu_nodes_total gauge",
        "# HELP sgpu_nodes_up Nodes without an sgpu collection error",
        "# TYPE sgpu_nodes_up gauge",
        "# HELP sgpu_nodes_stale Nodes with stale sgpu data",
        "# TYPE sgpu_nodes_stale gauge",
        "# HELP sgpu_node_up Node collection state by node",
        "# TYPE sgpu_node_up gauge",
        "# HELP sgpu_node_stale Node stale-data state by node",
        "# TYPE sgpu_node_stale gauge",
        "# HELP sgpu_node_cpus_total CPU cores on the node per Slurm",
        "# TYPE sgpu_node_cpus_total gauge",
        "# HELP sgpu_node_cpus_alloc CPU cores allocated by Slurm",
        "# TYPE sgpu_node_cpus_alloc gauge",
        "# HELP sgpu_node_cpu_load Node load average",
        "# TYPE sgpu_node_cpu_load gauge",
        "# HELP sgpu_node_mem_total_mib Node memory total in MiB",
        "# TYPE sgpu_node_mem_total_mib gauge",
        "# HELP sgpu_node_mem_used_mib Node memory used in MiB",
        "# TYPE sgpu_node_mem_used_mib gauge",
        "# HELP sgpu_node_mem_alloc_mib Node memory allocated by Slurm in MiB",
        "# TYPE sgpu_node_mem_alloc_mib gauge",
        "# HELP sgpu_node_mem_avail_mib Node memory available in MiB",
        "# TYPE sgpu_node_mem_avail_mib gauge",
        "# HELP sgpu_node_cpu_power_watts CPU package power via RAPL (not full system)",
        "# TYPE sgpu_node_cpu_power_watts gauge",
        "# HELP sgpu_node_ram_power_watts DRAM power via RAPL (Intel only)",
        "# TYPE sgpu_node_ram_power_watts gauge",
        "# HELP sgpu_node_sys_power_watts Whole-node wall power from the BMC (ipmitool dcmi)",
        "# TYPE sgpu_node_sys_power_watts gauge",
        "# HELP sgpu_gpus_total GPUs visible to sgpu",
        "# TYPE sgpu_gpus_total gauge",
        "# HELP sgpu_gpus_allocated GPUs allocated by Slurm",
        "# TYPE sgpu_gpus_allocated gauge",
        "# HELP sgpu_gpus_free GPUs with no Slurm allocation and no process",
        "# TYPE sgpu_gpus_free gauge",
        "# HELP sgpu_gpus_idle GPUs allocated by Slurm with no process",
        "# TYPE sgpu_gpus_idle gauge",
        "# HELP sgpu_gpus_parked GPUs holding VRAM with near-zero utilization",
        "# TYPE sgpu_gpus_parked gauge",
        "# HELP sgpu_gpus_rogue GPUs with a process but no Slurm GPU allocation",
        "# TYPE sgpu_gpus_rogue gauge",
        "# HELP sgpu_gpu_util GPU utilization percent",
        "# TYPE sgpu_gpu_util gauge",
        "# HELP sgpu_gpu_mem_used_mib GPU memory used in MiB",
        "# TYPE sgpu_gpu_mem_used_mib gauge",
        "# HELP sgpu_gpu_mem_total_mib GPU memory total in MiB",
        "# TYPE sgpu_gpu_mem_total_mib gauge",
        "# HELP sgpu_gpu_mem_used_percent GPU memory used percent",
        "# TYPE sgpu_gpu_mem_used_percent gauge",
        "# HELP sgpu_gpu_temp_celsius GPU temperature in Celsius",
        "# TYPE sgpu_gpu_temp_celsius gauge",
        "# HELP sgpu_gpu_power_watts GPU power draw in watts",
        "# TYPE sgpu_gpu_power_watts gauge",
        "# HELP sgpu_gpu_allocated GPU allocation state by Slurm user",
        "# TYPE sgpu_gpu_allocated gauge",
        "# HELP sgpu_gpu_job_info Slurm job holding this GPU",
        "# TYPE sgpu_gpu_job_info gauge",
        "# HELP sgpu_gpu_idle_seconds Seconds GPU has been allocated with no process",
        "# TYPE sgpu_gpu_idle_seconds gauge",
        "# HELP sgpu_gpu_parked_seconds Seconds GPU has held VRAM with near-zero utilization",
        "# TYPE sgpu_gpu_parked_seconds gauge",
        "# HELP sgpu_gpu_ecc_errors Uncorrectable ECC error count (aggregate)",
        "# TYPE sgpu_gpu_ecc_errors gauge",
        "# HELP sgpu_gpu_sm_clock_mhz Current SM clock in MHz",
        "# TYPE sgpu_gpu_sm_clock_mhz gauge",
        "# HELP sgpu_gpu_mem_clock_mhz Current memory clock in MHz",
        "# TYPE sgpu_gpu_mem_clock_mhz gauge",
        "# HELP sgpu_pending_job_info Slurm job waiting in the queue",
        "# TYPE sgpu_pending_job_info gauge",
        "# HELP sgpu_job_mem_mib RAM requested by a running GPU job in MiB",
        "# TYPE sgpu_job_mem_mib gauge",
        "# HELP sgpu_job_mem_fair_ratio Job RAM over its GPU fair share (node RAM x job GPUs / node GPUs)",
        "# TYPE sgpu_job_mem_fair_ratio gauge",
        "# HELP sgpu_gpu_info Static GPU identity labels",
        "# TYPE sgpu_gpu_info gauge",
        "# HELP sgpu_node_info Static node identity labels",
        "# TYPE sgpu_node_info gauge",
        "# HELP sgpu_collector_last_success_timestamp_seconds Unix time of this snapshot",
        "# TYPE sgpu_collector_last_success_timestamp_seconds gauge",
        "# HELP sgpu_node_data_age_seconds Age of the last node observation; -1 means unknown",
        "# TYPE sgpu_node_data_age_seconds gauge",
        "# HELP sgpu_node_scheduler_age_seconds Age of the scheduler generation; -1 means unknown",
        "# TYPE sgpu_node_scheduler_age_seconds gauge",
        "# HELP sgpu_collector_phase_seconds Wall time of each phase in the previous completed cycle",
        "# TYPE sgpu_collector_phase_seconds gauge",
        "# HELP sgpu_collector_cycle_seconds Wall time of the previous completed collector cycle",
        "# TYPE sgpu_collector_cycle_seconds gauge",
        "# HELP sgpu_collector_interval_seconds Time between the last two cycle starts",
        "# TYPE sgpu_collector_interval_seconds gauge",
        "# HELP sgpu_collector_ssh_inflight Admitted SSH polls running or queued",
        "# TYPE sgpu_collector_ssh_inflight gauge",
        "# HELP sgpu_collector_log_inflight Log mirrors running or queued",
        "# TYPE sgpu_collector_log_inflight gauge",
        "# HELP sgpu_collector_notify_backlog Unfinished outcome lookup and delivery work",
        "# TYPE sgpu_collector_notify_backlog gauge",
        "# HELP sgpu_command_calls_total Local command calls since collector start",
        "# TYPE sgpu_command_calls_total counter",
        "# HELP sgpu_command_failures_total Failed local command calls since collector start",
        "# TYPE sgpu_command_failures_total counter",
        "# HELP sgpu_command_seconds_total Cumulative local command wall time since collector start",
        "# TYPE sgpu_command_seconds_total counter",
        "# HELP sgpu_command_last_seconds Wall time of the last local command call",
        "# TYPE sgpu_command_last_seconds gauge",
        "# HELP sgpu_build_info sgpu collector release information",
        "# TYPE sgpu_build_info gauge",
    ]
    lines.append(f"sgpu_collector_last_success_timestamp_seconds {time.time():.0f}")
    lines.append(
        f'sgpu_build_info{{version="{_prom_escape(data.get("release", __version__))}"'
        f',build="{_prom_escape(data.get("build", __build__))}"}} 1'
    )
    n_run = len(data.get("jobs", []))
    n_pend = len(data.get("pending", []))
    nodes = data.get("nodes", [])
    total_nodes = len(nodes)
    up_nodes = sum(1 for n in nodes if not n.get("error"))
    stale_nodes = sum(1 for n in nodes if n.get("stale"))
    total_gpus = allocated_gpus = free_gpus = idle_gpus = parked_gpus = rogue_gpus = 0
    for n in nodes:
        free_gpus += node_gpu_classes(node_from_dict(n)).count("free")
        for g in n.get("gpus", []):
            total_gpus += 1
            allocated = bool(g.get("alloc_jobid") or g.get("alloc_user"))
            has_process = bool(g.get("users"))
            if allocated:
                allocated_gpus += 1
            if g.get("idle_sec", 0) > 0:
                idle_gpus += 1
            if g.get("parked_sec", 0) > 0:
                parked_gpus += 1
            if has_process and not allocated:
                rogue_gpus += 1
    lines.append(f"sgpu_jobs_running {n_run}")
    lines.append(f"sgpu_jobs_pending {n_pend}")
    lines.append(f"sgpu_nodes_total {total_nodes}")
    lines.append(f"sgpu_nodes_up {up_nodes}")
    lines.append(f"sgpu_nodes_stale {stale_nodes}")
    lines.append(f"sgpu_gpus_total {total_gpus}")
    lines.append(f"sgpu_gpus_allocated {allocated_gpus}")
    lines.append(f"sgpu_gpus_free {free_gpus}")
    lines.append(f"sgpu_gpus_idle {idle_gpus}")
    lines.append(f"sgpu_gpus_parked {parked_gpus}")
    lines.append(f"sgpu_gpus_rogue {rogue_gpus}")
    for pj in data.get("pending", [])[:PENDING_METRICS_MAX]:
        lines.append(
            "sgpu_pending_job_info{"
            f'jobid="{_prom_escape(str(pj.get("jobid", "")))}"'
            f',user="{_prom_escape(pj.get("user", ""))}"'
            f',partition="{_prom_escape(pj.get("partition", ""))}"'
            f',jobname="{_prom_escape(pj.get("jobname", ""))}"'
            f',reason="{_prom_escape(pj.get("reason", ""))}"'
            f',gpus="{_prom_escape(str(pj.get("gpu_count", "")))}"'
            "} 1"
        )
    # per-job RAM vs GPU fair share (node RAM × job GPUs / node GPUs) —
    # ratio > 1 means the job holds more memory than its GPU count entitles
    node_ram = {n["name"]: num(n.get("mem_total")) for n in nodes}
    node_gpus = {n["name"]: len(n.get("gpus", [])) for n in nodes}
    for j in data.get("jobs", []):
        gpus = j.get("gpu_count", 0)
        node = j.get("node", "")
        mem_mib = mem_to_mib(j.get("mem", ""), int(j.get("cpu_count") or 1))
        if not gpus or not mem_mib or not node_ram.get(node) or not node_gpus.get(node):
            continue
        share = node_ram[node] * gpus / node_gpus[node]
        lbl = (f'jobid="{_prom_escape(str(j.get("jobid", "")))}"'
               f',user="{_prom_escape(j.get("user", ""))}"'
               f',node="{_prom_escape(node)}"'
               f',gpus="{gpus}"')
        lines.append(f"sgpu_job_mem_mib{{{lbl}}} {mem_mib:.0f}")
        lines.append(f"sgpu_job_mem_fair_ratio{{{lbl}}} {mem_mib / share:.3f}")
    jobs_by_id = {str(j.get("jobid", "")): j for j in data.get("jobs", [])}
    for n in nodes:
        node = _prom_escape(n["name"])
        partition = _prom_escape(n.get("partition", ""))
        source = _prom_escape(n.get("source", ""))
        up = 0 if n.get("error") else 1
        lines.append(
            f'sgpu_node_info{{node="{node}",partition="{partition}",source="{source}"}} 1'
        )
        lines.append(f'sgpu_node_up{{node="{node}"}} {up}')
        lines.append(f'sgpu_node_stale{{node="{node}"}} {1 if n.get("stale") else 0}')
        for metric, key in (
            ("sgpu_node_cpus_total", "cpus"),
            ("sgpu_node_cpus_alloc", "cpu_alloc"),
            ("sgpu_node_cpu_load", "cpu_load"),
            ("sgpu_node_mem_total_mib", "mem_total"),
            ("sgpu_node_mem_used_mib", "mem_used"),
            ("sgpu_node_mem_alloc_mib", "mem_alloc"),
            ("sgpu_node_mem_avail_mib", "mem_avail"),
            ("sgpu_node_cpu_power_watts", "cpu_power"),
            ("sgpu_node_ram_power_watts", "ram_power"),
            ("sgpu_node_sys_power_watts", "sys_power"),
            ("sgpu_node_data_age_seconds", "data_age_sec"),
            ("sgpu_node_scheduler_age_seconds", "scheduler_age_sec"),
        ):
            v = num(n.get(key))
            if v is not None:
                lines.append(f'{metric}{{node="{node}"}} {v:g}')
        for g in n.get("gpus", []):
            lbl = f'node="{node}",gpu="{_prom_escape(g.get("index", ""))}"'
            lines.append(
                f'sgpu_gpu_info{{{lbl},name="{_prom_escape(g.get("name", ""))}"'
                f',uuid="{_prom_escape(g.get("uuid", ""))}"}} 1'
            )
            for metric, key in (
                ("sgpu_gpu_util", "util"),
                ("sgpu_gpu_mem_used_mib", "mem_used"),
                ("sgpu_gpu_mem_total_mib", "mem_total"),
                ("sgpu_gpu_temp_celsius", "temp"),
                ("sgpu_gpu_power_watts", "power"),
                ("sgpu_gpu_ecc_errors", "ecc"),
                ("sgpu_gpu_sm_clock_mhz", "sm_clock"),
                ("sgpu_gpu_mem_clock_mhz", "mem_clock"),
            ):
                v = num(g.get(key))
                if v is not None:
                    lines.append(f"{metric}{{{lbl}}} {v:g}")
            mem_used = num(g.get("mem_used"))
            mem_total = num(g.get("mem_total"))
            if mem_used is not None and mem_total and mem_total > 0:
                lines.append(f"sgpu_gpu_mem_used_percent{{{lbl}}} {mem_used / mem_total * 100:g}")
            user = _prom_escape(g.get("alloc_user", ""))
            allocated = 1 if (g.get("alloc_jobid") or g.get("alloc_user")) else 0
            lines.append(f'sgpu_gpu_allocated{{{lbl},user="{user}"}} {allocated}')
            if allocated:
                jid = str(g.get("alloc_jobid", ""))
                job = jobs_by_id.get(jid, {})
                lines.append(
                    f'sgpu_gpu_job_info{{{lbl},user="{user}"'
                    f',jobid="{_prom_escape(jid)}"'
                    f',jobname="{_prom_escape(job.get("jobname", ""))}"}} 1'
                )
            lines.append(f"sgpu_gpu_idle_seconds{{{lbl}}} {g.get('idle_sec', 0)}")
            lines.append(f"sgpu_gpu_parked_seconds{{{lbl}}} {g.get('parked_sec', 0)}")
    diagnostics = data.get("collector", {})
    for phase, seconds in diagnostics.get("phase_seconds", {}).items():
        if phase in ("collect", "write", "idle", "usage", "metrics", "notify"):
            lines.append(f'sgpu_collector_phase_seconds{{phase="{phase}"}} {seconds:g}')
    for field in ("cycle_seconds", "interval_seconds", "ssh_inflight", "log_inflight", "notify_backlog"):
        if field in diagnostics:
            lines.append(f'sgpu_collector_{field} {diagnostics[field]:g}')
    for command, values in diagnostics.get("rpc", {}).items():
        if command in ("sinfo", "squeue", "scontrol_jobs", "scontrol_nodes", "sacct", "other"):
            for field in ("calls", "failures", "seconds"):
                lines.append(f'sgpu_command_{field}_total{{command="{command}"}} {values[field]:g}')
            lines.append(f'sgpu_command_last_seconds{{command="{command}"}} {values["last_seconds"]:g}')
    optional = {
        "sgpu_node_cpu_util_percent": "Node CPU busy percent from counter deltas",
        "sgpu_node_pressure_percent": "Node PSI some avg10 stall percent by resource",
        "sgpu_node_telemetry_timestamp_seconds": "Last CPU and PSI sample timestamp",
        "sgpu_job_cpu_cores": "Job cgroup CPU cores used over the sample interval",
        "sgpu_job_mem_current_mib": "Job cgroup memory including cache in MiB",
        "sgpu_job_mem_peak_mib": "Job cgroup local memory peak in MiB",
        "sgpu_job_mem_limit_mib": "Finite job cgroup memory limit in MiB",
        "sgpu_job_oom_kills": "Job cgroup lifetime OOM kills",
        "sgpu_job_pid_vram_mib": "VRAM attributed to job GPU processes in MiB",
        "sgpu_job_telemetry_timestamp_seconds": "Last validated job sample timestamp",
        "sgpu_pending_wait_seconds": "Pending age since submission in seconds",
        "sgpu_gpu_health_severity": "GPU health severity: zero normal, one caution, two action needed",
    }
    for metric, description in optional.items():
        lines.extend((f"# HELP {metric} {description}", f"# TYPE {metric} gauge"))
    now = time.time()
    live_nodes = {node["name"] for node in nodes if not node.get("stale") and not node.get("error")
                  and node.get("scheduler_available", True) and (num(node.get("scheduler_age_sec")) or 0) <= 30}
    for node in nodes:
        label = f'node="{_prom_escape(node["name"])}"'
        stamp = num(node.get("telemetry_observed_at")) or 0
        if not node.get("stale") and stamp > 0 and -5 <= now - stamp <= TELEMETRY_MAX_AGE:
            lines.append(f"sgpu_node_telemetry_timestamp_seconds{{{label}}} {stamp:g}")
            cpu = num(node.get("cpu_util"))
            if cpu is not None and 0 <= cpu <= 100:
                lines.append(f"sgpu_node_cpu_util_percent{{{label}}} {cpu:g}")
            for resource, value in node.get("pressure", {}).items():
                pressure = num(value)
                if resource in ("cpu", "memory", "io") and pressure is not None and 0 <= pressure <= 100:
                    lines.append(f'sgpu_node_pressure_percent{{{label},resource="{resource}"}} {pressure:g}')
        if not node.get("stale"):
            for gpu in node.get("gpus", []):
                status, severity = gpu_health_status(from_dict(GpuInfo, gpu), now)
                if status != "—":
                    lines.append(f'sgpu_gpu_health_severity{{{label},gpu="{_prom_escape(gpu.get("index", ""))}"}} {severity}')
    mapping = {"cpu_cores": "cpu_cores", "mem_current_mib": "mem_current_mib", "mem_peak_mib": "mem_peak_mib",
               "mem_limit_mib": "mem_limit_mib", "oom_kill": "oom_kills", "vram_mib": "pid_vram_mib", "observed_at": "telemetry_timestamp_seconds"}
    for job in data.get("jobs", []):
        for node, raw in job.get("telemetry", {}).items():
            sample = clean_sample(raw)
            stamp = sample.get("observed_at", 0)
            if node not in live_nodes or not stamp or not -5 <= now - stamp <= TELEMETRY_MAX_AGE:
                continue
            label = f'jobid="{_prom_escape(job.get("jobid", ""))}",user="{_prom_escape(job.get("user", ""))}",node="{_prom_escape(node)}"'
            for key, suffix in mapping.items():
                if key in sample:
                    lines.append(f"sgpu_job_{suffix}{{{label}}} {sample[key]:g}")
    for job in data.get("pending", [])[:PENDING_METRICS_MAX]:
        wait = pending_wait_seconds(from_dict(PendingJob, job), now)
        if wait is not None:
            lines.append(f'sgpu_pending_wait_seconds{{jobid="{_prom_escape(job.get("jobid", ""))}"}} {wait:g}')
    return "\n".join(lines) + "\n"


# ── Master host stats (the collector's own machine) ──────────────────────
# The dashboard's "master (login/collector node)" row needs first-party data
# even on clusters that run no node_exporter — a remote Grafana can then read
# everything from this one textfile. Metric suffixes mirror node_exporter's
# so consumers translate mechanically (node_X -> sgpu_master_X).

# local filesystems only: statvfs on a dead NFS mount would hang the loop
_MASTER_FS_TYPES = {"ext2", "ext3", "ext4", "xfs", "btrfs", "zfs"}
_MASTER_DISK_RE = re.compile(r"^(sd[a-z]+|vd[a-z]+|nvme\d+n\d+)$")


def _master_host_lines(proc: str = "/proc", sys_dir: str = "/sys") -> List[str]:
    """This host's CPU/RAM/load/fs/net/disk/temp as sgpu_master_* lines.
    Every section is best-effort — a missing file just drops its metrics."""
    lines: List[str] = []

    def read(path: str) -> str:
        with open(path) as f:
            return f.read()

    try:
        for ln in read(f"{proc}/stat").splitlines():
            if ln.startswith("cpu") and len(ln) > 3 and ln[3].isdigit():
                f_ = ln.split()
                lines.append(f'sgpu_master_cpu_seconds_total{{cpu="{f_[0][3:]}",mode="idle"}} '
                             f"{int(f_[4]) / 100:.2f}")
            elif ln.startswith("btime "):
                lines.append(f"sgpu_master_boot_time_seconds {ln.split()[1]}")
    except Exception:
        pass
    try:
        for ln in read(f"{proc}/meminfo").splitlines():
            k = ln.split(":")[0]
            if k in ("MemTotal", "MemAvailable"):
                lines.append(f"sgpu_master_memory_{k}_bytes {int(ln.split()[1]) * 1024}")
    except Exception:
        pass
    try:
        lines.append(f"sgpu_master_load1 {read(f'{proc}/loadavg').split()[0]}")
    except Exception:
        pass
    try:
        seen = set()
        for ln in read(f"{proc}/mounts").splitlines():
            parts = ln.split()
            if len(parts) < 3 or parts[2] not in _MASTER_FS_TYPES or parts[1] in seen:
                continue
            seen.add(parts[1])
            try:
                st = os.statvfs(parts[1])
            except OSError:
                continue
            mp = _prom_escape(parts[1])
            lines.append(f'sgpu_master_filesystem_size_bytes{{mountpoint="{mp}"}} '
                         f"{st.f_blocks * st.f_frsize}")
            lines.append(f'sgpu_master_filesystem_avail_bytes{{mountpoint="{mp}"}} '
                         f"{st.f_bavail * st.f_frsize}")
    except Exception:
        pass
    try:
        for ln in read(f"{proc}/net/dev").splitlines()[2:]:
            if ":" not in ln:
                continue
            dev, rest = ln.split(":", 1)
            dev = dev.strip()
            if dev == "lo":
                continue
            f_ = rest.split()
            lines.append(f'sgpu_master_network_receive_bytes_total{{device="{dev}"}} {f_[0]}')
            lines.append(f'sgpu_master_network_transmit_bytes_total{{device="{dev}"}} {f_[8]}')
    except Exception:
        pass
    try:
        for ln in read(f"{proc}/diskstats").splitlines():
            f_ = ln.split()
            if len(f_) > 9 and _MASTER_DISK_RE.match(f_[2]):
                lines.append(f'sgpu_master_disk_read_bytes_total{{device="{f_[2]}"}} '
                             f"{int(f_[5]) * 512}")
                lines.append(f'sgpu_master_disk_written_bytes_total{{device="{f_[2]}"}} '
                             f"{int(f_[9]) * 512}")
    except Exception:
        pass
    try:
        coretemp_seen = 0
        for h in sorted(glob.glob(f"{sys_dir}/class/hwmon/hwmon*")):
            try:
                name = read(f"{h}/name").strip()
            except OSError:
                continue
            if name == "coretemp":
                # one hwmon per CPU package: label each by its platform device
                # (coretemp.0, coretemp.1, ...) or the sockets collide
                dev = os.path.basename(os.path.realpath(f"{h}/device"))
                chip = _prom_escape(f"platform_{dev}" if dev.startswith("coretemp") else f"platform_coretemp.{coretemp_seen}")
                coretemp_seen += 1
                for t in sorted(glob.glob(f"{h}/temp*_input")):
                    sensor = os.path.basename(t)[:-len("_input")]
                    try:
                        val = int(read(t)) / 1000
                    except (OSError, ValueError):
                        continue
                    lines.append(f'sgpu_master_hwmon_temp_celsius{{chip="{chip}",'
                                 f'sensor="{sensor}"}} {val:.1f}')
            for pf in sorted(glob.glob(f"{h}/power*_average")):
                sensor = os.path.basename(pf)[:-len("_average")]
                try:
                    val = int(read(pf)) / 1e6
                except (OSError, ValueError):
                    continue
                lines.append(f'sgpu_master_hwmon_power_average_watt{{chip="{_prom_escape(name)}",'
                             f'sensor="{sensor}"}} {val:.1f}')
    except Exception:
        pass
    return lines


def _write_metrics(
    data: dict, *, force: bool = False, monotonic_now: float | None = None,
) -> bool:
    """Write a rate-limited Prometheus textfile snapshot."""
    global _metrics_last_write
    now = time.monotonic() if monotonic_now is None else monotonic_now
    if not force and _metrics_last_write \
            and now - _metrics_last_write < METRICS_REFRESH_SEC:
        return False
    try:
        text = _format_metrics(data)
        host = _master_host_lines()
        if host:
            text += "\n".join(host) + "\n"
        # 0644: node_exporter's textfile collector usually runs as its own user
        atomic_write(METRICS_FILE, text, mode=0o644)
        _metrics_last_write = now
        return True
    except Exception as e:
        print(f"[collector] metrics write error: {e}", flush=True)
        return False


# ── Daemon ────────────────────────────────────────────────────────────────

_running = True
_lock_fd = None
_log_path: Path | None = None


def _handle_signal(signum, frame):
    global _running
    _running = False


def _rotate_log_if_big() -> None:
    """Rotate collector.log to collector.log.1 when it exceeds LOG_MAX_BYTES."""
    if _log_path is None:
        return
    try:
        if not _log_path.exists() or _log_path.stat().st_size <= LOG_MAX_BYTES:
            return
        reopen = sys.stdout is not sys.__stdout__
        if reopen:
            sys.stdout.close()
        _log_path.rename(_log_path.with_name("collector.log.1"))
        if reopen:
            sys.stdout = os.fdopen(open_append(_log_path), "a")
            sys.stderr = sys.stdout
    except Exception:
        pass


_last_cycle_started = 0.0


def _collect_cycle(notifier) -> tuple[dict, dict]:
    global _cycle_stats, _last_cycle_started
    started = time.monotonic()
    interval = started - _last_cycle_started if _last_cycle_started else 0.0
    _last_cycle_started = started
    phases = {}

    def timed(name, callback):
        before = time.monotonic()
        try:
            return callback()
        finally:
            phases[name] = time.monotonic() - before

    data = timed("collect", lambda: collect_all(background_basic=True))
    timed("write", lambda: atomic_write(DATA_FILE, json.dumps(data, ensure_ascii=False), mode=0o644))
    timed("idle", _save_idle_state)
    _maybe_backfill_sacct(time.time())
    timed("usage", _save_usage)
    timed("metrics", lambda: _write_metrics(data))
    try:
        timed("notify", lambda: notifier.process(data, job_log_sources=dict(_notification_log_sources)))
    except Exception as exc:
        print(f"[collector] notify error: {exc}", flush=True)
    _cycle_stats = {
        "phase_seconds": phases, "cycle_seconds": time.monotonic() - started,
        "interval_seconds": interval,
        "notify_backlog": notifier._outcome_queue.unfinished_tasks + notifier._queue.unfinished_tasks,
    }
    return data, _cycle_stats


def run_collector():
    """Main loop: collect and write data file every REFRESH_SEC."""
    # The snapshot is published world-readable, but the directory holding it
    # must not be writable by the audience: this process is usually root, and
    # a user-owned data dir turns every write into a root-write primitive.
    try:
        ensure_secure_dir(DATA_DIR)
        # docs/GRAFANA.md tells sites with a PrivateTmp node_exporter to point
        # METRICS_FILE outside DATA_DIR; that parent needs the same treatment,
        # and creating it here beats failing every cycle in _write_metrics
        if METRICS_FILE.parent != DATA_DIR:
            ensure_secure_dir(METRICS_FILE.parent)
    except UnsafeRuntimeDir as e:
        print(f"[collector] {e}", flush=True)
        sys.exit(1)

    # Single-instance guard: two collectors would race on data.json.
    # Retry briefly so a restart can overlap the old instance's shutdown.
    global _lock_fd
    _lock_fd = os.fdopen(open_lock(LOCK_FILE), "r+")
    lock_deadline = time.time() + 10
    while True:
        try:
            fcntl.flock(_lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except OSError:
            if time.time() >= lock_deadline:
                print("[collector] another collector is already running, exiting")
                sys.exit(1)
            time.sleep(0.5)

    atomic_write(PID_FILE, str(os.getpid()), mode=0o644)
    try:
        # A custom setting may point at an existing parent such as /srv/state;
        # never mark that parent as wholly owned by sgpu for recursive removal.
        _prepare_state_dir()
    except UnsafeRuntimeDir as e:
        print(f"[collector] {e}", flush=True)
        sys.exit(1)
    # 0755 while enabled: every user must traverse it. Cleanup also runs when
    # disabled so an opt-out removes old world-readable tails immediately.
    if not _prepare_log_spool() and SHARE_LOGS:
        sys.exit(1)
    if SHARE_LOGS:
        print(f"[collector] job log sharing on (spool={LOG_SPOOL_DIR}, "
              f"tail={LOG_TAIL_BYTES // 1024}KB) — every user can read every "
              "job's stdout/stderr", flush=True)
    if SHARE_JOB_DETAILS:
        print("[collector] job detail sharing on — every user can inspect "
              "running/pending scheduler records", flush=True)
    _load_idle_state()
    _load_inventory()
    _load_usage()
    notifier = Notifier(STATE_DIR)
    if notifier.enabled:
        print(f"[collector] Slack bot notifier on ({notifier.channel}, daily thread, "
              f"sender={notifier.sender}, "
              f"node_health={notifier.node_health}, grace={notifier.down_grace_sec:.0f}s, "
              f"collect_alert={notifier.collect_alert}, "
              f"waste_alert_hours={notifier.waste_alert_hours}, rogue={notifier.rogue_alert}, "
              f"temp_alert_c={notifier.temp_alert_c}, ecc={notifier.ecc_alert}, "
              f"job_done_users={notifier.job_done_users}, free_gpus_min={notifier.free_gpus_min})",
              flush=True)

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)

    print(f"[collector] started (pid={os.getpid()}, interval={REFRESH_SEC}s, data={DATA_FILE})")

    # First cycle writes immediately: skeleton GPU rows (inventory / sinfo
    # GRES) render the full layout while real polls land asynchronously.
    while _running:
        try:
            data, diagnostics = _collect_cycle(notifier)
            elapsed = diagnostics["cycle_seconds"]

            n_gpus = sum(len(n.get("gpus", [])) for n in data["nodes"])
            print(f"[collector] {data['ts']} nodes={len(data['nodes'])} "
                  f"gpus={n_gpus} jobs={len(data['jobs'])} pending={len(data['pending'])} "
                  f"({elapsed:.1f}s)", flush=True)
        except Exception as e:
            print(f"[collector] error: {e}", flush=True)

        _rotate_log_if_big()
        deadline = time.time() + REFRESH_SEC
        while _running and time.time() < deadline:
            time.sleep(0.5)

    try:
        _save_usage(force=True)
        PID_FILE.unlink(missing_ok=True)
    except Exception:
        pass
    print("[collector] stopped")


def daemonize():
    """Fork to background."""
    if os.fork() > 0:
        sys.exit(0)
    os.setsid()
    if os.fork() > 0:
        sys.exit(0)
    log = DATA_DIR / "collector.log"
    try:
        ensure_secure_dir(DATA_DIR)
    except UnsafeRuntimeDir as e:
        print(f"[collector] {e}", file=sys.stderr, flush=True)
        sys.exit(1)
    global _log_path
    _log_path = log
    _rotate_log_if_big()
    # dup2 over the real fds — merely rebinding sys.stdout would leave the
    # inherited ssh/terminal pipe open (a remote `sgpu-collector --daemon`
    # launch would hang) and C-level writes to fd 2 would miss the log
    null_fd = os.open(os.devnull, os.O_RDONLY)
    log_fd = open_append(log)
    os.dup2(null_fd, 0)
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    os.close(null_fd)
    os.close(log_fd)
    sys.stdin = os.fdopen(0, "r")
    sys.stdout = os.fdopen(1, "w", buffering=1)
    sys.stderr = os.fdopen(2, "w", buffering=1)
    run_collector()


def _read_pid() -> int | None:
    """PID file content, or None when absent/corrupt (crash mid-write)."""
    try:
        return int(PID_FILE.read_text().strip())
    except (OSError, ValueError):
        return None


def stop_daemon():
    pid = _read_pid()
    if pid is None:
        print("No collector running (no/invalid pid file)")
        PID_FILE.unlink(missing_ok=True)
        return
    try:
        os.kill(pid, signal.SIGTERM)
        print(f"Sent SIGTERM to collector (pid={pid})")
    except ProcessLookupError:
        print(f"Collector not running (stale pid={pid})")
        PID_FILE.unlink(missing_ok=True)


def check_status():
    pid = _read_pid()
    if pid is None:
        print("Collector: not running")
        return
    try:
        os.kill(pid, 0)
        age = ""
        if DATA_FILE.exists():
            age_sec = time.time() - DATA_FILE.stat().st_mtime
            age = f", data {age_sec:.0f}s old"
        print(f"Collector: running (pid={pid}{age})")
    except ProcessLookupError:
        print(f"Collector: not running (stale pid={pid})")
        PID_FILE.unlink(missing_ok=True)


def main():
    if "--daemon" in sys.argv:
        daemonize()
    elif "--stop" in sys.argv:
        stop_daemon()
    elif "--status" in sys.argv:
        check_status()
    else:
        run_collector()
