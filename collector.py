#!/usr/bin/env python3
"""
Background collector daemon for slurm-gpu-tui.
Continuously collects SLURM + GPU data via SSH and writes to a shared JSON file.

Usage:
    python3 collector.py                # foreground
    python3 collector.py --daemon       # background (daemonize)
    python3 collector.py --stop         # stop running daemon
    python3 collector.py --status       # check if running

The TUI reads from the data file for instant startup.
"""
from __future__ import annotations

import atexit
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Tuple

# ── Config ─────────────────────────────────────────────────────────────────

DATA_DIR = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))
DATA_FILE = DATA_DIR / "data.json"
PID_FILE = DATA_DIR / "collector.pid"
REFRESH_SEC = int(os.getenv("SLURM_GPU_TUI_COLLECTOR_SEC", "3"))
NODE_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
MAX_WORKERS = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))


# ── Shell helpers ──────────────────────────────────────────────────────────

def run_cmd(cmd: str, timeout: int = 12) -> Tuple[bool, str]:
    try:
        out = subprocess.check_output(
            shlex.split(cmd), stderr=subprocess.STDOUT, timeout=timeout, text=True
        )
        return True, out.strip()
    except Exception as e:
        return False, str(e)


# ── SSH ControlMaster ──────────────────────────────────────────────────────

_SSH_CONTROL_DIR = tempfile.mkdtemp(prefix="slurm-collector-ssh-")
_SSH_BASE_OPTS = (
    f"-o ControlPath={_SSH_CONTROL_DIR}/%r@%h "
    "-o StrictHostKeyChecking=no -o BatchMode=yes"
)


def _cleanup_ssh():
    import shutil
    shutil.rmtree(_SSH_CONTROL_DIR, ignore_errors=True)

atexit.register(_cleanup_ssh)


def ssh_ensure_master(node: str) -> None:
    sock = f"{_SSH_CONTROL_DIR}/{os.getenv('USER', 'user')}@{node}"
    if os.path.exists(sock):
        return
    cmd = (
        f"ssh -o ControlMaster=yes {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=10 -o ControlPersist=1800 -fN {node}"
    )
    subprocess.call(
        shlex.split(cmd), stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=20,
    )


def ssh_cmd(node: str, inner_cmd: str, timeout: int = 10) -> Tuple[bool, str]:
    ssh_ensure_master(node)
    wrapped = (
        f"ssh -o ControlMaster=no {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=3 {node} {shlex.quote(inner_cmd)}"
    )
    return run_cmd(wrapped, timeout=timeout)


def warmup_ssh(nodes: List[str], max_workers: int = 8) -> None:
    with ThreadPoolExecutor(max_workers=min(max_workers, max(len(nodes), 1))) as ex:
        list(ex.map(ssh_ensure_master, nodes))


# ── Data models (JSON-serializable) ───────────────────────────────────────

@dataclass
class GpuData:
    index: str = ""
    name: str = ""
    util: str = ""
    mem_used: str = ""
    mem_total: str = ""
    temp: str = ""
    power: str = ""
    power_cap: str = ""
    pids: List[str] = field(default_factory=list)
    users: List[str] = field(default_factory=list)


@dataclass
class JobData:
    jobid: str = ""
    user: str = ""
    partition: str = ""
    jobname: str = ""
    elapsed: str = ""
    node: str = ""
    gpu_count: int = 0
    gres_raw: str = ""
    time_limit: str = ""


@dataclass
class PendingJobData:
    jobid: str = ""
    user: str = ""
    partition: str = ""
    jobname: str = ""
    time_limit: str = ""
    gpu_count: int = 0
    reason: str = ""
    priority: str = ""


@dataclass
class NodeData:
    name: str = ""
    state: str = ""
    cpus: str = ""
    cpu_alloc: str = ""
    cpu_load: str = ""
    mem_total: str = ""
    mem_free: str = ""
    gres: str = ""
    mem_used: str = ""
    mem_avail: str = ""
    gpus: List[dict] = field(default_factory=list)
    jobs: List[dict] = field(default_factory=list)
    error: str = ""
    stale: bool = False


# ── Collection logic ──────────────────────────────────────────────────────

def _has_gpu(gres: str) -> bool:
    if not gres or gres == "(null)":
        return False
    return "gpu" in gres.lower()


def _shorten_gpu_name(name: str) -> str:
    name = name.replace("NVIDIA ", "").replace("GeForce ", "")
    name = name.replace(" Generation", "").replace(" Workstation Edition", "")
    name = name.replace(" Max-Q", "").replace(" PCIe", "")
    name = name.replace("RTX PRO 6000 Blackwell", "RTX PRO 6000")
    return name.strip()


def collect_jobs() -> List[dict]:
    cmd = 'squeue -h -t R -o "%i|%u|%P|%j|%M|%R|%b|%l"'
    ok, out = run_cmd(cmd)
    if not ok:
        return []
    rows = []
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 7:
            continue
        gres = p[6].strip()
        gc = 0
        m = re.search(r"gpu(?::[^:,]+)?:(\d+)", gres)
        if m:
            gc = int(m.group(1))
        rows.append(asdict(JobData(
            p[0], p[1], p[2], p[3], p[4], p[5], gc, gres,
            p[7].strip() if len(p) > 7 else "",
        )))
    return rows


def collect_pending() -> List[dict]:
    cmd = 'squeue -h -t PD -o "%i|%u|%P|%j|%l|%b|%r|%Q"'
    ok, out = run_cmd(cmd)
    if not ok:
        return []
    rows = []
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 8:
            continue
        gres = p[5].strip()
        gc = 0
        m = re.search(r"gpu(?::[^:,]+)?:(\d+)", gres)
        if m:
            gc = int(m.group(1))
        rows.append(asdict(PendingJobData(
            p[0], p[1], p[2], p[3], p[4], gc, p[6].strip(), p[7].strip()
        )))
    return rows


def collect_nodes_basic() -> List[dict]:
    cmd = 'sinfo -N -h -o "%N|%T|%c|%O|%m|%e|%G|%C"'
    ok, out = run_cmd(cmd)
    if not ok:
        return []
    rows = []
    seen = set()
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 8:
            continue
        name = p[0].strip()
        if name in seen:
            continue
        seen.add(name)
        cpus_aiot = p[7].strip()
        cpu_alloc = ""
        parts = cpus_aiot.split("/")
        if len(parts) >= 4:
            cpu_alloc = parts[0]
        rows.append({
            "name": name, "state": p[1].strip(), "cpus": p[2].strip(),
            "cpu_load": p[3].strip(), "mem_total": p[4].strip(),
            "mem_free": p[5].strip(), "gres": p[6].strip(),
            "cpu_alloc": cpu_alloc,
        })
    return rows


def collect_node_ssh(node: str, timeout: int = 30) -> Tuple[List[dict], dict, str]:
    """SSH to a node: GPU metrics + meminfo. User info comes from SLURM squeue."""
    combined = (
        "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
        "temperature.gpu,power.draw,power.limit --format=csv,noheader,nounits 2>/dev/null; "
        "echo '---SEP---'; "
        "awk '/^MemTotal:/{ t=$2 } /^MemAvailable:/{ a=$2 } END{ printf \"%d %d %d\", t/1024, (t-a)/1024, a/1024 }' /proc/meminfo"
    )
    ok, out = ssh_cmd(node, combined, timeout=timeout)
    if not ok:
        return [], {}, "ssh failed"

    sections = out.split("---SEP---")
    metrics_raw = sections[0].strip() if len(sections) > 0 else ""
    mem_raw = sections[1].strip() if len(sections) > 1 else ""

    mem = {}
    mem_parts = mem_raw.split()
    if len(mem_parts) >= 3:
        mem = {"total": mem_parts[0], "used": mem_parts[1], "avail": mem_parts[2]}

    gpus = []
    for line in metrics_raw.splitlines():
        p = [x.strip() for x in line.split(",")]
        if len(p) < 9:
            continue
        gpus.append(asdict(GpuData(
            index=p[0], name=_shorten_gpu_name(p[2]), util=p[3],
            mem_used=p[4], mem_total=p[5], temp=p[6], power=p[7], power_cap=p[8],
            pids=[], users=[],
        )))
    return gpus, mem, ""


# Cache for fallback
_cache: Dict[str, Tuple[List[dict], dict]] = {}


def collect_all() -> dict:
    """Full collection cycle. Returns JSON-serializable dict."""
    nodes_raw = collect_nodes_basic()
    jobs = collect_jobs()
    pending = collect_pending()

    # Map node -> jobs
    node_jobs: Dict[str, List[dict]] = {}
    for j in jobs:
        node_jobs.setdefault(j["node"], []).append(j)

    gpu_node_names = [n["name"] for n in nodes_raw]

    # SSH warmup + parallel collection
    if gpu_node_names:
        warmup_ssh(gpu_node_names, max_workers=MAX_WORKERS)

    ssh_results: Dict[str, Tuple[List[dict], dict, str]] = {}
    stale_nodes: List[str] = []

    if gpu_node_names:
        with ThreadPoolExecutor(max_workers=min(MAX_WORKERS, len(gpu_node_names))) as ex:
            futs = {ex.submit(collect_node_ssh, n, NODE_TIMEOUT): n for n in gpu_node_names}
            for fut in as_completed(futs):
                name = futs[fut]
                gpus, mem, err = fut.result()
                if err and name in _cache:
                    cached_gpus, cached_mem = _cache[name]
                    ssh_results[name] = (cached_gpus, cached_mem, "")
                    stale_nodes.append(name)
                else:
                    ssh_results[name] = (gpus, mem, err)
                    if gpus or mem:
                        _cache[name] = (gpus, mem)

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
        "jobs": jobs,
        "pending": pending,
        "stale_nodes": stale_nodes,
    }


# ── Daemon loop ───────────────────────────────────────────────────────────

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

            # Atomic write: write to temp then rename
            tmp = DATA_FILE.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
            tmp.rename(DATA_FILE)

            n_gpus = sum(len(n.get("gpus", [])) for n in data["nodes"])
            print(f"[collector] {data['ts']} nodes={len(data['nodes'])} "
                  f"gpus={n_gpus} jobs={len(data['jobs'])} pending={len(data['pending'])} "
                  f"({elapsed:.1f}s)")
        except Exception as e:
            print(f"[collector] error: {e}")

        # Sleep in small increments so we can respond to signals
        deadline = time.time() + REFRESH_SEC
        while _running and time.time() < deadline:
            time.sleep(0.5)

    # Cleanup
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
    # Redirect stdio
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


if __name__ == "__main__":
    if "--daemon" in sys.argv:
        daemonize()
    elif "--stop" in sys.argv:
        stop_daemon()
    elif "--status" in sys.argv:
        check_status()
    else:
        run_collector()
