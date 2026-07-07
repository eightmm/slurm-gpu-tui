"""Background collector daemon for sgpu."""
from __future__ import annotations

import fcntl
import json
import os
import signal
import sys
import threading
import time
from datetime import datetime, timedelta
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Dict, List

from .common import (
    GpuInfo, JobInfo, NodeErrorKind, NodeMemInfo, PendingJob,
    collect_basic, collect_node_data, parse_gres_models, run_cmd, ssh_cmd,
    _classify_error,
)
from .agent import AGENT_PAYLOAD_VERSION
from . import agent as _agent_module

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

# Opt-in: publish every running job's batch script in data.json so all users
# can view them in the TUI. Requires a collector that may read them (root).
# OFF by default — scripts can contain secrets; enabling shares them with
# everyone who can read data.json.
SHARE_SCRIPTS = bool(os.getenv("SLURM_GPU_TUI_SHARE_SCRIPTS", ""))
SCRIPT_MAX_BYTES = 16384

# ── Long-lived executors / shared node results ───────────────────────────

_node_executor = ThreadPoolExecutor(max_workers=16)

# Latest per-node SSH results, updated by background pollers.
# name -> {"gpus": [dict], "mem": dict, "error": str, "error_kind": str, "stale": bool}
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


def _load_inventory() -> None:
    raw = _read_state_json(INVENTORY_FILE)
    if isinstance(raw, dict):
        _inventory.update(raw)


def _update_inventory(name: str, gpu_dicts: List[dict]) -> None:
    """Refresh a node's static GPU info; persist when changed or file missing."""
    static = [
        {"index": g["index"], "name": g["name"], "mem_total": g["mem_total"]}
        for g in gpu_dicts
    ]
    if not static:
        return
    if _inventory.get(name) == static and INVENTORY_FILE.exists():
        return
    _inventory[name] = static
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        tmp = INVENTORY_FILE.with_suffix(".tmp")
        tmp.write_text(json.dumps(_inventory, ensure_ascii=False))
        tmp.rename(INVENTORY_FILE)
    except Exception:
        pass


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
    try:
        tmp = IDLE_STATE_FILE.with_suffix(".tmp")
        tmp.write_text(json.dumps({"idle": _idle_since, "parked": _parked_since}))
        tmp.rename(IDLE_STATE_FILE)
    except Exception:
        pass


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


def _fetch_scripts(jobs: List[JobInfo]) -> Dict[str, str]:
    """Fetch each running job's batch script once (needs privileges)."""
    if not SHARE_SCRIPTS:
        return {}
    live = {j.jobid for j in jobs}
    for jid in [j for j in _script_cache if j not in live]:
        del _script_cache[jid]
    for j in jobs:
        if j.jobid in _script_cache:
            continue
        cmd = f"scontrol write batch_script {j.jobid} -"
        if os.geteuid() != 0:
            # install.sh provisions a sudoers rule for exactly this command
            cmd = "sudo -n " + cmd
        ok, out = run_cmd(cmd)
        out = out.strip()
        good = ok and out and not out.startswith("job script retrieval failed")
        _script_cache[j.jobid] = out[:SCRIPT_MAX_BYTES] if good else ""
    return _script_cache


# ── Per-user GPU-hour accounting ──────────────────────────────────────────
# Daily buckets: {"days": {"YYYY-MM-DD": {user: {"alloc": sec, "busy": sec}}}}
# alloc = GPU allocated to the user's job; busy = that GPU actually computing.
# Rolling window, persisted each cycle. No slurmdbd needed.

USAGE_FILE = STATE_DIR / "usage.json"
USAGE_KEEP_DAYS = int(os.getenv("SLURM_GPU_TUI_USAGE_KEEP_DAYS", "30"))
_usage: Dict[str, dict] = {"days": {}}
_last_usage_ts: float | None = None


def _load_usage() -> None:
    raw = _read_state_json(USAGE_FILE)
    if isinstance(raw, dict) and isinstance(raw.get("days"), dict):
        _usage["days"] = raw["days"]


def _save_usage() -> None:
    try:
        tmp = USAGE_FILE.with_suffix(".tmp")
        tmp.write_text(json.dumps(_usage))
        tmp.rename(USAGE_FILE)
    except Exception:
        pass


def _accumulate_usage(result_nodes: List[dict], now: float) -> None:
    global _last_usage_ts
    prev, _last_usage_ts = _last_usage_ts, now
    if prev is None:
        return
    dt = now - prev
    if not (0 < dt <= 60):
        return  # collector was paused; don't credit the gap
    day = datetime.now().strftime("%Y-%m-%d")
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
    cutoff = (datetime.now() - timedelta(days=USAGE_KEEP_DAYS)).strftime("%Y-%m-%d")
    for d in [d for d in _usage["days"] if d < cutoff]:
        del _usage["days"][d]


# ── Adaptive polling state ────────────────────────────────────────────────

_node_poll_state: Dict[str, Dict] = {}
_INTERVAL_HOT = 5    # active node: poll every 5s
_INTERVAL_COLD = 20  # idle node: poll every 20s
_INTERVAL_DOWN = 60  # down/drain node: poll every 60s


def _should_poll_node(name: str, slurm_state: str) -> bool:
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
        "index": gpu.index, "name": gpu.name, "util": gpu.util,
        "mem_used": gpu.mem_used, "mem_total": gpu.mem_total,
        "temp": gpu.temp, "power": gpu.power, "power_cap": gpu.power_cap,
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


def _expected_agent_build() -> str:
    """Current agent.py fingerprint, read live so upgrades are noticed even
    if this collector predates them (relaunched agents then match again)."""
    try:
        return str(int(os.path.getmtime(_agent_module.__file__)))
    except OSError:
        return "0"


def _read_agent_payload(name: str) -> dict | None:
    """Return a node's push-agent payload if fresh and version-compatible."""
    p = AGENT_DIR / f"{name}.json"
    try:
        # mtime is stamped by the NFS server (= this host), so no clock skew
        if time.time() - p.stat().st_mtime > AGENT_MAX_AGE:
            return None
        payload = json.loads(p.read_text())
        if payload.get("agent_version") != AGENT_PAYLOAD_VERSION:
            return None  # old agent — treated as stale, repair will upgrade it
        if not AGENT_DISABLE and payload.get("agent_build") != _expected_agent_build():
            return None  # agent runs outdated code — repair restarts it
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
        ok, out = ssh_cmd(name, f"{_AGENT_BIN} --daemon", timeout=25)
        if not ok and "No such file" in out:
            # Install dir isn't visible from this node (not a shared FS):
            # push mode can't work there — stop retrying, SSH pull covers it
            _agent_repair_ts[name] = float("inf")
            print(f"[collector] agent repair {name}: venv not on node, "
                  "push disabled for this node (SSH pull fallback)", flush=True)
            return
        print(f"[collector] agent repair {name}: {'ok' if ok else out}", flush=True)

    _node_executor.submit(_run)


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


def collect_all() -> dict:
    """One collection cycle: fast local data + latest async node results.

    Node SSH polls run in the background and never block this cycle — a dead
    node only goes stale, it cannot stall data for healthy nodes.
    """
    nodes_raw, jobs, pending, node_jobs_from_basic, gpu_alloc, basic_err = collect_basic()

    node_jobs: Dict[str, List[dict]] = {
        k: [_job_to_dict(j) for j in v] for k, v in node_jobs_from_basic.items()
    }
    jobid_user = {j.jobid: j.user for j in jobs}

    # Prefer push-agent payloads (local NFS read, every cycle). Nodes without
    # a live agent fall back to async SSH polls + agent repair.
    agent_nodes: set = set()
    for n in nodes_raw:
        name = n["name"]
        has_jobs = name in node_jobs_from_basic
        payload = _read_agent_payload(name)
        if payload is not None:
            agent_nodes.add(name)
            gpu_dicts = payload.get("gpus", [])
            with _results_lock:
                _node_results[name] = {
                    "gpus": gpu_dicts, "mem": payload.get("mem", {}),
                    "error": "", "error_kind": NodeErrorKind.OK.value, "stale": False,
                }
                node_is_cold = (
                    all(g.get("util") in ("0", "", "N/A") for g in gpu_dicts)
                    and not has_jobs
                )
                # Mark polled so the SSH path stays quiet while the agent lives
                _update_poll_state(name, success=True, node_is_cold=node_is_cold, slurm_state=n["state"])
            _update_inventory(name, gpu_dicts)
            continue
        if _should_poll_node(name, n["state"]):
            _poll_node_bg(n, has_jobs=has_jobs)
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
        gpus = []
        for g in r["gpus"]:
            g = dict(g)
            jid = node_alloc.get(g.get("index", ""), "")
            g["alloc_jobid"] = jid
            g["alloc_user"] = jobid_user.get(jid, "")
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
            gpus.append(g)
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
            "mem_total": n["mem_total"], "mem_free": n["mem_free"], "gres": n["gres"],
            "mem_used": mem.get("used", ""), "mem_avail": mem.get("avail", ""),
            "gpus": gpus, "jobs": node_jobs.get(name, []),
            "error": r["error"], "stale": r["stale"],
            "error_kind": r["error_kind"],
        })

    _accumulate_usage(result_nodes, time.time())

    scripts = _fetch_scripts(jobs)
    return {
        "version": 1,
        "ts": datetime.now().isoformat(),
        "nodes": result_nodes,
        "jobs": [dict(_job_to_dict(j), script=scripts.get(j.jobid, "")) for j in jobs],
        "pending": [_pending_to_dict(p) for p in pending],
        "stale_nodes": stale_nodes,
        "errors": basic_err,
    }


# ── Prometheus textfile exporter ──────────────────────────────────────────

METRICS_FILE = DATA_DIR / "metrics.prom"


def _prom_escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "")


def _write_metrics(data: dict) -> None:
    """Write a Prometheus textfile snapshot next to data.json."""
    def num(v):
        try:
            return float(v)
        except (ValueError, TypeError):
            return None

    lines = [
        "# HELP sgpu_gpu_util GPU utilization percent",
        "# HELP sgpu_gpu_idle_seconds Seconds GPU has been allocated with no process",
    ]
    n_run = len(data.get("jobs", []))
    n_pend = len(data.get("pending", []))
    lines.append(f"sgpu_jobs_running {n_run}")
    lines.append(f"sgpu_jobs_pending {n_pend}")
    for n in data.get("nodes", []):
        node = _prom_escape(n["name"])
        up = 0 if n.get("error") else 1
        lines.append(f'sgpu_node_up{{node="{node}"}} {up}')
        lines.append(f'sgpu_node_stale{{node="{node}"}} {1 if n.get("stale") else 0}')
        for g in n.get("gpus", []):
            lbl = f'node="{node}",gpu="{_prom_escape(g.get("index", ""))}"'
            for metric, key in (
                ("sgpu_gpu_util", "util"),
                ("sgpu_gpu_mem_used_mib", "mem_used"),
                ("sgpu_gpu_mem_total_mib", "mem_total"),
                ("sgpu_gpu_temp_celsius", "temp"),
                ("sgpu_gpu_power_watts", "power"),
            ):
                v = num(g.get(key))
                if v is not None:
                    lines.append(f"{metric}{{{lbl}}} {v:g}")
            user = _prom_escape(g.get("alloc_user", ""))
            lines.append(f'sgpu_gpu_allocated{{{lbl},user="{user}"}} {1 if g.get("alloc_jobid") else 0}')
            lines.append(f"sgpu_gpu_idle_seconds{{{lbl}}} {g.get('idle_sec', 0)}")
            lines.append(f"sgpu_gpu_parked_seconds{{{lbl}}} {g.get('parked_sec', 0)}")
    try:
        tmp = METRICS_FILE.with_suffix(".tmp")
        tmp.write_text("\n".join(lines) + "\n")
        tmp.rename(METRICS_FILE)
    except Exception:
        pass


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
            _save_usage()
            _write_metrics(data)

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
    sys.stdin = open(os.devnull, "r")
    log = DATA_DIR / "collector.log"
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    global _log_path
    _log_path = log
    _rotate_log_if_big()
    sys.stdout = open(log, "a")
    sys.stderr = sys.stdout
    run_collector()


def stop_daemon():
    if not PID_FILE.exists():
        print("No collector running (no pid file)")
        return
    pid = int(PID_FILE.read_text().strip())
    try:
        os.kill(pid, signal.SIGTERM)
        print(f"Sent SIGTERM to collector (pid={pid})")
    except ProcessLookupError:
        print(f"Collector not running (stale pid={pid})")
        PID_FILE.unlink(missing_ok=True)


def check_status():
    if not PID_FILE.exists():
        print("Collector: not running")
        return
    pid = int(PID_FILE.read_text().strip())
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
