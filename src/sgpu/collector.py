"""Background collector daemon for sgpu."""
from __future__ import annotations

import json
import os
import signal
import sys
import time
from dataclasses import asdict
from datetime import datetime
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any, Dict, List, Tuple

from .common import (
    GpuInfo, JobInfo, NodeErrorKind, NodeMemInfo, PendingJob,
    collect_basic, collect_node_data, _classify_error,
)

# ── Config ────────────────────────────────────────────────────────────────

DATA_DIR = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))
DATA_FILE = DATA_DIR / "data.json"
PID_FILE = DATA_DIR / "collector.pid"
REFRESH_SEC = int(os.getenv("SLURM_GPU_TUI_COLLECTOR_SEC", "3"))
NODE_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
MAX_WORKERS = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))

_collector_cache: Dict[str, Any] = {}

# ── Long-lived executors ──────────────────────────────────────────────────

_node_executor = ThreadPoolExecutor(max_workers=16)
_basic_executor = ThreadPoolExecutor(max_workers=3)

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
        "gpu_count": job.gpu_count, "gres_raw": job.gres_raw,
        "time_limit": job.time_limit,
    }


def _pending_to_dict(pj: PendingJob) -> dict:
    return {
        "jobid": pj.jobid, "user": pj.user, "partition": pj.partition,
        "jobname": pj.jobname, "time_limit": pj.time_limit,
        "gpu_count": pj.gpu_count, "reason": pj.reason, "priority": pj.priority,
    }


def collect_all() -> dict:
    """Full collection cycle. Returns JSON-serializable dict."""
    nodes_raw, jobs, pending, node_jobs_from_basic, basic_err = collect_basic()

    node_jobs: Dict[str, List[dict]] = {
        k: [_job_to_dict(j) for j in v] for k, v in node_jobs_from_basic.items()
    }

    stale_nodes: List[str] = []

    # SSH collection — skip nodes that don't need polling yet, use cache instead
    ssh_results: Dict[str, Tuple[List[dict], dict, str]] = {}
    nodes_to_poll = [n for n in nodes_raw if _should_poll_node(n["name"], n["state"])]
    nodes_cached = [n for n in nodes_raw if not _should_poll_node(n["name"], n["state"])]

    # Inject cached results for nodes we're skipping this cycle
    for n in nodes_cached:
        name = n["name"]
        if name in _collector_cache:
            cached_gpus, cached_mem = _collector_cache[name]
            ssh_results[name] = (cached_gpus, cached_mem, "", NodeErrorKind.OK.value)

    if nodes_to_poll:
        futs = {_node_executor.submit(collect_node_data, n["name"], NODE_TIMEOUT): n for n in nodes_to_poll}
        for fut in as_completed(futs):
            n = futs[fut]
            name = n["name"]
            slurm_state = n["state"]
            gpus, mem, err = fut.result()
            gpu_dicts = [_gpu_to_dict(g) for g in gpus]
            mem_dict = {"total": mem.total, "used": mem.used, "avail": mem.avail}
            if err and name in _collector_cache:
                cached_gpus, cached_mem = _collector_cache[name]
                ssh_results[name] = (cached_gpus, cached_mem, "", NodeErrorKind.STALE_CACHED.value)
                stale_nodes.append(name)
                _update_poll_state(name, success=False, node_is_cold=False, slurm_state=slurm_state)
            else:
                kind = _classify_error(err).value if err else NodeErrorKind.OK.value
                ssh_results[name] = (gpu_dicts, mem_dict, err, kind)
                if gpu_dicts or mem.total:
                    _collector_cache[name] = (gpu_dicts, mem_dict)
                node_is_cold = (
                    all(g.util in ("0", "", "N/A") for g in gpus)
                    and name not in node_jobs
                )
                _update_poll_state(name, success=not err, node_is_cold=node_is_cold, slurm_state=slurm_state)

    # Build final node list
    result_nodes = []
    for n in nodes_raw:
        name = n["name"]
        raw = ssh_results.get(name, ([], {}, "", NodeErrorKind.OK.value))
        gpus, mem, err, error_kind = raw if len(raw) == 4 else (*raw, NodeErrorKind.OK.value)
        result_nodes.append({
            "name": name, "state": n["state"], "cpus": n["cpus"],
            "cpu_alloc": n.get("cpu_alloc", ""), "cpu_load": n["cpu_load"],
            "mem_total": n["mem_total"], "mem_free": n["mem_free"], "gres": n["gres"],
            "mem_used": mem.get("used", ""), "mem_avail": mem.get("avail", ""),
            "gpus": gpus, "jobs": node_jobs.get(name, []),
            "error": err, "stale": name in stale_nodes,
            "error_kind": error_kind,
        })

    return {
        "ts": datetime.now().isoformat(),
        "nodes": result_nodes,
        "jobs": [_job_to_dict(j) for j in jobs],
        "pending": [_pending_to_dict(p) for p in pending],
        "stale_nodes": stale_nodes,
        "errors": basic_err,
    }


# ── Daemon ────────────────────────────────────────────────────────────────

_running = True


def _handle_signal(signum, frame):
    global _running
    _running = False


def run_collector():
    """Main loop: collect and write data file every REFRESH_SEC."""
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    PID_FILE.write_text(str(os.getpid()))

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)

    print(f"[collector] started (pid={os.getpid()}, interval={REFRESH_SEC}s, data={DATA_FILE})")

    while _running:
        try:
            t0 = time.time()
            data = collect_all()
            elapsed = time.time() - t0

            tmp = DATA_FILE.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
            tmp.rename(DATA_FILE)

            n_gpus = sum(len(n.get("gpus", [])) for n in data["nodes"])
            print(f"[collector] {data['ts']} nodes={len(data['nodes'])} "
                  f"gpus={n_gpus} jobs={len(data['jobs'])} pending={len(data['pending'])} "
                  f"({elapsed:.1f}s)")
        except Exception as e:
            print(f"[collector] error: {e}")

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
