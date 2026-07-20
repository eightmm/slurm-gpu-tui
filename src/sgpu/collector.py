"""Background collector daemon for sgpu."""
from __future__ import annotations

import fcntl
import json
import os
import re
import shlex
import signal
import stat
import sys
import threading
import time
from datetime import datetime, timedelta
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Dict, List

from .common import (
    GpuInfo, JobInfo, NodeErrorKind, NodeMemInfo, PendingJob, ROGUE_IGNORE,
    collect_basic, collect_node_data, parse_gres_models, reconcile_gpu_alloc,
    resolve_user, run_cmd, ssh_cmd, _classify_error,
)
from .agent import AGENT_PAYLOAD_VERSION
from . import agent as _agent_module
from .notify import Notifier
from . import __build__, __version__

# ── Config ────────────────────────────────────────────────────────────────

DATA_DIR = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))
# Persistent state (usage history, waste ages, inventory) must survive
# reboots, so it lives under the home dir — NOT in /tmp like the live data
STATE_DIR = Path(os.getenv("SLURM_GPU_TUI_STATE_DIR", str(Path.home() / ".sgpu" / "state")))
DATA_FILE = DATA_DIR / "data.json"
PID_FILE = DATA_DIR / "collector.pid"
LOCK_FILE = DATA_DIR / "collector.lock"
REFRESH_SEC = int(os.getenv("SLURM_GPU_TUI_COLLECTOR_SEC", "3"))
NODE_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
MAX_WORKERS = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))
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
SHARE_SCRIPTS = bool(os.getenv("SLURM_GPU_TUI_SHARE_SCRIPTS", ""))
SCRIPT_MAX_BYTES = 16384

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
    """Load a state file, falling back to its pre-STATE_DIR /tmp location."""
    for p in (path, DATA_DIR / path.name):
        try:
            return json.loads(p.read_text())
        except Exception:
            continue
    return None


_state_write_warned: set = set()
_state_write_lock = threading.Lock()


def _write_state_json(path: Path, text: str) -> None:
    """Atomic + fsync'd write for files that must survive a reboot.
    Failures (disk full, permissions) are logged once, not every cycle."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        with open(tmp, "w") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        tmp.rename(path)
        _state_write_warned.discard(str(path))
    except OSError as e:
        if str(path) not in _state_write_warned:
            _state_write_warned.add(str(path))
            print(f"[collector] state write failed for {path}: {e}", flush=True)


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
    with _state_write_lock:
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
# one background worker: a burst of new jobs (array submit) would otherwise
# serialize N scontrol calls inside the 3s collect cycle
_script_executor = ThreadPoolExecutor(max_workers=1)


def _fetch_one_script(jid: str) -> None:
    try:
        cmd = f"scontrol write batch_script {jid} -"
        if os.geteuid() != 0:
            # install.sh provisions a sudoers rule for exactly this command
            cmd = "sudo -n " + cmd
        ok, out = run_cmd(cmd)
        out = out.strip()
        good = ok and out and not out.startswith("job script retrieval failed")
        _script_cache[jid] = out[:SCRIPT_MAX_BYTES] if good else ""
    finally:
        _script_inflight.discard(jid)


def _fetch_scripts(jobs: List[JobInfo]) -> Dict[str, str]:
    """Return cached batch scripts; fetch missing ones in the background.
    A job's script appears one or two cycles after the job does."""
    if not SHARE_SCRIPTS:
        return {}
    live = {j.jobid for j in jobs}
    for jid in [j for j in _script_cache if j not in live]:
        del _script_cache[jid]
    for j in jobs:
        if j.jobid in _script_cache or j.jobid in _script_inflight:
            continue
        _script_inflight.add(j.jobid)
        _script_executor.submit(_fetch_one_script, j.jobid)
    return dict(_script_cache)


# ── Per-user GPU-hour accounting ──────────────────────────────────────────
# Daily buckets: {"days": {"YYYY-MM-DD": {user: {"alloc": sec, "busy": sec}}}}
# alloc = GPU allocated to the user's job; busy = that GPU actually computing.
# Rolling window, persisted each cycle.
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
# 0 disables slurmdbd backfill
SACCT_BACKFILL_SEC = int(os.getenv("SLURM_GPU_TUI_SACCT_SEC", "3600"))
_usage: Dict[str, dict] = {"days": {}}
_last_usage_ts: float | None = None
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


def _save_usage() -> None:
    _write_state_json(USAGE_FILE, json.dumps(_usage))


def _accumulate_usage(result_nodes: List[dict], now: float) -> None:
    global _last_usage_ts
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
            u["alloc"] += dt
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


def _sacct_backfill(now: float) -> None:
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
        return
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
    _usage["sacct_days"] = days
    _usage["sacct_ts"] = now
    print(f"[collector] sacct backfill: {len(days)} day(s), "
          f"{sum(len(u) for u in days.values())} user-day rows", flush=True)


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
            _sacct_backfill(time.time())
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


def _gpu_to_dict(gpu: GpuInfo) -> dict:
    return {
        "index": gpu.index, "minor": gpu.minor, "uuid": gpu.uuid,
        "pci_bus": gpu.pci_bus, "slot": gpu.slot, "serial": gpu.serial,
        "name": gpu.name, "util": gpu.util,
        "mem_used": gpu.mem_used, "mem_total": gpu.mem_total,
        "temp": gpu.temp, "power": gpu.power, "power_cap": gpu.power_cap,
        "ecc": gpu.ecc, "sm_clock": gpu.sm_clock, "mem_clock": gpu.mem_clock,
        "pids": gpu.pids, "users": gpu.users,
    }


def _job_to_dict(job: JobInfo) -> dict:
    return {
        "jobid": job.jobid, "user": job.user, "partition": job.partition,
        "jobname": job.jobname, "elapsed": job.elapsed, "node": job.node,
        "gpu_count": job.gpu_count, "cpu_count": job.cpu_count,
        "gres_raw": job.gres_raw,
        "time_limit": job.time_limit,
    }


def _pending_to_dict(pj: PendingJob) -> dict:
    return {
        "jobid": pj.jobid, "user": pj.user, "partition": pj.partition,
        "jobname": pj.jobname, "time_limit": pj.time_limit,
        "gpu_count": pj.gpu_count, "reason": pj.reason, "priority": pj.priority,
        "start_time": pj.start_time,
    }


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
    return True


def _read_agent_payload(name: str, expected_kind: str = "gpu") -> dict | None:
    """Return a node's push-agent payload if fresh and version-compatible.
    Parsed payloads are cached by mtime — agents rewrite at their configured interval,
    so most 3s cycles can skip the read+parse."""
    p = AGENT_DIR / f"{name}.json"
    try:
        file_stat = p.lstat()
        if not stat.S_ISREG(file_stat.st_mode):
            return None
        if not 0 < file_stat.st_size <= AGENT_PAYLOAD_MAX_BYTES:
            return None
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


def _poll_node_bg(n: dict, has_jobs: bool) -> None:
    """Submit a background SSH poll for one node unless one is already in flight."""
    name, slurm_state = n["name"], n["state"]
    with _results_lock:
        if name in _inflight:
            return
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
                }
                _update_inventory(name, gpu_dicts)
                node_is_cold = (
                    all(g.util in ("0", "", "N/A") for g in gpus) and not has_jobs
                )
            _update_poll_state(name, success=not err, node_is_cold=node_is_cold, slurm_state=slurm_state)
            _inflight.discard(name)

    _node_executor.submit(_run)


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


def collect_all() -> dict:
    """One collection cycle: fast local data + latest async node results.

    Node SSH polls run in the background and never block this cycle — a dead
    node only goes stale, it cannot stall data for healthy nodes.
    """
    nodes_raw, jobs, pending, node_jobs_from_basic, gpu_alloc, alloc_user_map, basic_err = collect_basic()

    node_jobs: Dict[str, List[dict]] = {
        k: [_job_to_dict(j) for j in v] for k, v in node_jobs_from_basic.items()
    }
    # scontrol's UserId map wins over squeue: it resolves array-task jobids
    # (38182_0 in squeue vs the real 38192 in the alloc) and carries the name
    jobid_user = {j.jobid: j.user for j in jobs}
    jobid_user.update({k: v for k, v in alloc_user_map.items() if v})

    # Prefer push-agent payloads (local NFS read, every cycle). GPU agents are
    # collector-repaired; CPU agents are systemd-managed on their node. Either
    # kind falls back to async SSH when its payload is absent or stale.
    agent_nodes: set = set()
    for n in nodes_raw:
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
                    "error": "", "error_kind": NodeErrorKind.OK.value, "stale": False,
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
            _poll_node_bg(n, has_jobs=has_jobs)
        if has_gpu:
            _maybe_repair_agent(name)

    with _results_lock:
        results = {name: dict(r) for name, r in _node_results.items()}

    stale_nodes: List[str] = []
    result_nodes = []
    for n in nodes_raw:
        name = n["name"]
        r = results.get(name, {"gpus": [], "mem": {}, "error": "", "error_kind": NodeErrorKind.OK.value, "stale": False})
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
        now = time.time()
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
        for g, (jid, _user) in zip(gpus, alloc_pairs):
            g["alloc_jobid"] = jid
            g["alloc_user"] = _user
            if skeleton_mode:
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
        })

    _accumulate_usage(result_nodes, time.time())

    scripts = _fetch_scripts(jobs)
    return {
        "version": 1,
        "release": __version__,
        "build": __build__,
        "ts": datetime.now().isoformat(),
        "nodes": result_nodes,
        "jobs": [dict(_job_to_dict(j), script=scripts.get(j.jobid, "")) for j in jobs],
        "pending": [_pending_to_dict(p) for p in pending],
        "stale_nodes": stale_nodes,
        "errors": basic_err,
    }


# ── Prometheus textfile exporter ──────────────────────────────────────────

# Prometheus textfile. Default sits next to data.json in DATA_DIR (/tmp),
# but node_exporter units often ship PrivateTmp=yes and then never see a
# /tmp file — point this at a shared path node_exporter can read in that case.
METRICS_FILE = Path(
    os.getenv("SLURM_GPU_TUI_METRICS_FILE", str(DATA_DIR / "metrics.prom"))
)


def _prom_escape(s: str) -> str:
    return str(s).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "")


def _format_metrics(data: dict) -> str:
    """Return a Prometheus textfile snapshot for the merged cluster state."""
    def num(v):
        try:
            return float(v)
        except (ValueError, TypeError):
            return None

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
        "# HELP sgpu_gpu_info Static GPU identity labels",
        "# TYPE sgpu_gpu_info gauge",
        "# HELP sgpu_node_info Static node identity labels",
        "# TYPE sgpu_node_info gauge",
        "# HELP sgpu_collector_last_success_timestamp_seconds Unix time of this snapshot",
        "# TYPE sgpu_collector_last_success_timestamp_seconds gauge",
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
        for g in n.get("gpus", []):
            total_gpus += 1
            allocated = bool(g.get("alloc_jobid") or g.get("alloc_user"))
            has_process = bool(g.get("users"))
            if allocated:
                allocated_gpus += 1
            if not allocated and not has_process:
                free_gpus += 1
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
    for pj in data.get("pending", []):
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
    return "\n".join(lines) + "\n"


def _write_metrics(data: dict) -> None:
    """Write a Prometheus textfile snapshot next to data.json."""
    try:
        tmp = METRICS_FILE.with_suffix(".tmp")
        tmp.write_text(_format_metrics(data))
        tmp.rename(METRICS_FILE)
    except Exception as e:
        print(f"[collector] metrics write error: {e}", flush=True)


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
            sys.stdout = open(_log_path, "a")
            sys.stderr = sys.stdout
    except Exception:
        pass


def run_collector():
    """Main loop: collect and write data file every REFRESH_SEC."""
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    # Single-instance guard: two collectors would race on data.json.
    # Retry briefly so a restart can overlap the old instance's shutdown.
    global _lock_fd
    _lock_fd = open(LOCK_FILE, "w")
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

    PID_FILE.write_text(str(os.getpid()))
    STATE_DIR.mkdir(parents=True, exist_ok=True)
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
            t0 = time.time()
            data = collect_all()
            elapsed = time.time() - t0

            tmp = DATA_FILE.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
            tmp.rename(DATA_FILE)
            _save_idle_state()
            _maybe_backfill_sacct(time.time())
            _save_usage()
            _write_metrics(data)
            try:
                notifier.process(data)
            except Exception as e:
                print(f"[collector] notify error: {e}", flush=True)

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
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    global _log_path
    _log_path = log
    _rotate_log_if_big()
    # dup2 over the real fds — merely rebinding sys.stdout would leave the
    # inherited ssh/terminal pipe open (a remote `sgpu-collector --daemon`
    # launch would hang) and C-level writes to fd 2 would miss the log
    null_fd = os.open(os.devnull, os.O_RDONLY)
    log_fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND)
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
