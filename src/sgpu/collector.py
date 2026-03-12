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
from typing import Dict, List, Tuple

from .common import (
    GpuInfo, JobInfo, NodeMemInfo, PendingJob,
    collect_jobs, collect_pending_jobs, collect_nodes_basic,
    collect_node_data, warmup_ssh, shorten_gpu_name,
)

# ── Config ────────────────────────────────────────────────────────────────

DATA_DIR = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))
DATA_FILE = DATA_DIR / "data.json"
PID_FILE = DATA_DIR / "collector.pid"
REFRESH_SEC = int(os.getenv("SLURM_GPU_TUI_COLLECTOR_SEC", "3"))
NODE_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
MAX_WORKERS = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))

# Cache for fallback
_cache: Dict[str, Tuple[List[dict], dict]] = {}


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
    nodes_raw, _ = collect_nodes_basic()
    jobs, _ = collect_jobs()
    pending, _ = collect_pending_jobs()

    node_jobs: Dict[str, List[dict]] = {}
    for j in jobs:
        node_jobs.setdefault(j.node, []).append(_job_to_dict(j))

    node_names = [n["name"] for n in nodes_raw]
    stale_nodes: List[str] = []

    # SSH collection
    ssh_results: Dict[str, Tuple[List[dict], dict, str]] = {}
    if node_names:
        warmup_ssh(node_names, max_workers=MAX_WORKERS)
        with ThreadPoolExecutor(max_workers=min(MAX_WORKERS, len(node_names))) as ex:
            futs = {ex.submit(collect_node_data, n, NODE_TIMEOUT): n for n in node_names}
            for fut in as_completed(futs):
                name = futs[fut]
                gpus, mem, err = fut.result()
                gpu_dicts = [_gpu_to_dict(g) for g in gpus]
                mem_dict = {"total": mem.total, "used": mem.used, "avail": mem.avail}
                if err and name in _cache:
                    cached_gpus, cached_mem = _cache[name]
                    ssh_results[name] = (cached_gpus, cached_mem, "")
                    stale_nodes.append(name)
                else:
                    ssh_results[name] = (gpu_dicts, mem_dict, err)
                    if gpu_dicts or mem.total:
                        _cache[name] = (gpu_dicts, mem_dict)

    # Build final node list
    result_nodes = []
    for n in nodes_raw:
        name = n["name"]
        gpus, mem, err = ssh_results.get(name, ([], {}, ""))
        result_nodes.append({
            "name": name, "state": n["state"], "cpus": n["cpus"],
            "cpu_alloc": n.get("cpu_alloc", ""), "cpu_load": n["cpu_load"],
            "mem_total": n["mem_total"], "mem_free": n["mem_free"], "gres": n["gres"],
            "mem_used": mem.get("used", ""), "mem_avail": mem.get("avail", ""),
            "gpus": gpus, "jobs": node_jobs.get(name, []),
            "error": err, "stale": name in stale_nodes,
        })

    return {
        "ts": datetime.now().isoformat(),
        "nodes": result_nodes,
        "jobs": [_job_to_dict(j) for j in jobs],
        "pending": [_pending_to_dict(p) for p in pending],
        "stale_nodes": stale_nodes,
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
