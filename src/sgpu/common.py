"""Shared SSH helpers, data models, and collection logic."""
from __future__ import annotations

import atexit
import os
import re
import shlex
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, List, Tuple


class NodeErrorKind(str, Enum):
    OK = "ok"
    SSH_TIMEOUT = "ssh_timeout"
    SSH_UNREACHABLE = "ssh_unreachable"
    SSH_AUTH = "ssh_auth"
    NVIDIA_SMI_MISSING = "nvidia_smi_missing"
    NVIDIA_SMI_FAILED = "nvidia_smi_failed"
    PARSE_ERROR = "parse_error"
    SLURM_DOWN = "slurm_down"
    STALE_CACHED = "stale_cached"


# ── Shell helpers ─────────────────────────────────────────────────────────

def run_cmd(cmd: str, timeout: int = 12) -> Tuple[bool, str]:
    try:
        out = subprocess.check_output(
            shlex.split(cmd), stderr=subprocess.STDOUT, timeout=timeout, text=True
        )
        return True, out.strip()
    except Exception as e:
        return False, str(e)


# ── SSH ControlMaster pool ────────────────────────────────────────────────

_SSH_CONTROL_DIR: str = ""
_SSH_BASE_OPTS: str = ""


def init_ssh_pool() -> None:
    """Initialize SSH ControlMaster socket directory."""
    global _SSH_CONTROL_DIR, _SSH_BASE_OPTS
    if _SSH_CONTROL_DIR:
        return
    _SSH_CONTROL_DIR = tempfile.mkdtemp(prefix="sgpu-ssh-")
    _SSH_BASE_OPTS = (
        f"-o ControlPath={_SSH_CONTROL_DIR}/%r@%h "
        "-o StrictHostKeyChecking=no -o BatchMode=yes"
    )
    atexit.register(cleanup_ssh_pool)


def cleanup_ssh_pool() -> None:
    """Remove SSH socket directory."""
    import shutil
    if _SSH_CONTROL_DIR:
        shutil.rmtree(_SSH_CONTROL_DIR, ignore_errors=True)


def ssh_ensure_master(node: str) -> None:
    """Start a ControlMaster connection to a node if not already running."""
    init_ssh_pool()
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
    init_ssh_pool()
    ssh_ensure_master(node)
    wrapped = (
        f"ssh -o ControlMaster=no {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=3 {node} {shlex.quote(inner_cmd)}"
    )
    return run_cmd(wrapped, timeout=timeout)


def warmup_ssh(nodes: List[str], max_workers: int = 8) -> None:
    """Pre-establish SSH ControlMaster connections in parallel."""
    init_ssh_pool()
    with ThreadPoolExecutor(max_workers=min(max_workers, max(len(nodes), 1))) as ex:
        list(ex.map(ssh_ensure_master, nodes))


# ── Data models ───────────────────────────────────────────────────────────

@dataclass
class GpuInfo:
    index: str = ""
    name: str = ""
    util: str = ""       # %
    mem_used: str = ""   # MiB
    mem_total: str = ""  # MiB
    temp: str = ""       # C
    power: str = ""      # W
    power_cap: str = ""  # W
    pids: List[str] = field(default_factory=list)
    users: List[str] = field(default_factory=list)


@dataclass
class JobInfo:
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
class PendingJob:
    jobid: str = ""
    user: str = ""
    partition: str = ""
    jobname: str = ""
    time_limit: str = ""
    gpu_count: int = 0
    reason: str = ""
    priority: str = ""


@dataclass
class NodeMemInfo:
    total: str = ""   # MB
    used: str = ""    # MB
    avail: str = ""   # MB


@dataclass
class NodeInfo:
    name: str = ""
    state: str = ""
    cpus: str = ""
    cpu_alloc: str = ""
    cpu_load: str = ""
    mem_total: str = ""
    mem_free: str = ""
    gres: str = ""
    gpus: List[GpuInfo] = field(default_factory=list)
    jobs: List[JobInfo] = field(default_factory=list)
    error: str = ""
    mem_used: str = ""
    mem_avail: str = ""
    stale: bool = False
    error_kind: str = ""  # NodeErrorKind value as string


@dataclass
class NodeSSHResult:
    gpus: List[GpuInfo] = field(default_factory=list)
    mem: NodeMemInfo = field(default_factory=NodeMemInfo)
    error: str = ""
    error_kind: NodeErrorKind = NodeErrorKind.OK
    consecutive_failures: int = 0
    last_ok_ts: float = 0.0


# ── GPU name shortening ──────────────────────────────────────────────────

def shorten_gpu_name(name: str) -> str:
    name = name.replace("NVIDIA ", "").replace("GeForce ", "")
    name = name.replace(" Generation", "").replace(" Workstation Edition", "")
    name = name.replace(" Max-Q", "").replace(" PCIe", "")
    name = name.replace("RTX PRO 6000 Blackwell", "RTX PRO 6000")
    return name.strip()


# ── Error classification ─────────────────────────────────────────────────

def _classify_error(error_str: str, exc: Exception = None) -> NodeErrorKind:
    if exc is not None and hasattr(exc, '__class__'):
        if 'TimeoutExpired' in type(exc).__name__ or 'Timeout' in type(exc).__name__:
            return NodeErrorKind.SSH_TIMEOUT
    s = str(error_str).lower()
    if "timed out" in s or "timeout" in s:
        return NodeErrorKind.SSH_TIMEOUT
    if "connection refused" in s or "no route to host" in s or "network is unreachable" in s:
        return NodeErrorKind.SSH_UNREACHABLE
    if "permission denied" in s or "publickey" in s:
        return NodeErrorKind.SSH_AUTH
    if "command not found" in s and "nvidia" in s:
        return NodeErrorKind.NVIDIA_SMI_MISSING
    if "nvidia-smi" in s and ("failed" in s or "error" in s):
        return NodeErrorKind.NVIDIA_SMI_FAILED
    return NodeErrorKind.SSH_UNREACHABLE  # default for unknown SSH errors


# ── Data collection ──────────────────────────────────────────────────────

def collect_jobs() -> Tuple[List[JobInfo], str]:
    cmd = 'squeue -h -t R -o "%i|%u|%P|%j|%M|%N|%b|%l"'
    ok, out = run_cmd(cmd)
    if not ok:
        return [], f"squeue failed: {out}"
    rows: List[JobInfo] = []
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 7:
            continue
        gres = p[6].strip()
        gc = 0
        m = re.search(r"gpu(?::[^:,]+)?:(\d+)", gres)
        if m:
            gc = int(m.group(1))
        tlimit = p[7].strip() if len(p) > 7 else ""
        rows.append(JobInfo(p[0], p[1], p[2], p[3], p[4], p[5], gc, gres, tlimit))
    return rows, ""


def collect_pending_jobs() -> Tuple[List[PendingJob], str]:
    cmd = 'squeue -h -t PD -o "%i|%u|%P|%j|%l|%b|%r|%Q"'
    ok, out = run_cmd(cmd)
    if not ok:
        return [], f"squeue PD failed: {out}"
    rows: List[PendingJob] = []
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 8:
            continue
        gres = p[5].strip()
        gc = 0
        m = re.search(r"gpu(?::[^:,]+)?:(\d+)", gres)
        if m:
            gc = int(m.group(1))
        rows.append(PendingJob(p[0], p[1], p[2], p[3], p[4], gc, p[6].strip(), p[7].strip()))
    return rows, ""


def collect_nodes_basic() -> Tuple[List[dict], str]:
    cmd = 'sinfo -N -h -o "%N|%T|%c|%O|%m|%e|%G|%C"'
    ok, out = run_cmd(cmd)
    if not ok:
        return [], f"sinfo failed: {out}"
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
        gres = p[6].strip()
        if "gpu" not in gres.lower():
            continue
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
    return rows, ""


def collect_node_data(node: str, timeout: int = 30) -> Tuple[List[GpuInfo], NodeMemInfo, str]:
    """SSH to node: nvidia-smi metrics + pmon (PID→GPU) + ps (PID→user) + meminfo."""
    combined = (
        "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
        "temperature.gpu,power.draw,power.limit --format=csv,noheader,nounits 2>/dev/null; "
        "echo '---SEP---'; "
        "nvidia-smi pmon -c 1 -s m 2>/dev/null; "
        "echo '---SEP---'; "
        "awk '/^MemTotal:/{ t=$2 } /^MemAvailable:/{ a=$2 } "
        "END{ printf \"%d %d %d\", t/1024, (t-a)/1024, a/1024 }' /proc/meminfo; "
        "echo '---SEP---'; "
        "PIDS=$(nvidia-smi pmon -c 1 -s u 2>/dev/null | awk 'NR>2 && $2!= \"-\" {print $2}' | tr '\\n' ','); "
        "if [ -n \"$PIDS\" ]; then ps -p ${PIDS%,} -o pid=,user= 2>/dev/null; fi"
    )
    ok, out = ssh_cmd(node, combined, timeout=timeout)
    if not ok:
        return [], NodeMemInfo(), out if out else "ssh failed"

    sections = out.split("---SEP---")
    metrics_raw = sections[0].strip() if len(sections) > 0 else ""
    pmon_raw = sections[1].strip() if len(sections) > 1 else ""
    mem_raw = sections[2].strip() if len(sections) > 2 else ""
    ps_raw = sections[3].strip() if len(sections) > 3 else ""

    mem_info = NodeMemInfo()
    mem_parts = mem_raw.split()
    if len(mem_parts) >= 3:
        mem_info = NodeMemInfo(total=mem_parts[0], used=mem_parts[1], avail=mem_parts[2])

    # Parse pmon: gpu_idx -> list of PIDs
    gpu_pids: Dict[str, List[str]] = {}
    for line in pmon_raw.splitlines():
        line = line.strip()
        if line.startswith("#") or not line:
            continue
        parts = line.split()
        if len(parts) >= 2 and parts[1] != "-":
            gpu_pids.setdefault(parts[0], []).append(parts[1])

    # Resolve PIDs to usernames via ps output from combined SSH call
    pid_to_user: Dict[str, str] = {}
    for line in ps_raw.splitlines():
        ps_parts = line.split()
        if len(ps_parts) >= 2:
            pid_to_user[ps_parts[0]] = ps_parts[1]

    gpus: List[GpuInfo] = []
    for line in metrics_raw.splitlines():
        p = [x.strip() for x in line.split(",")]
        if len(p) < 9:
            continue
        idx = p[0]
        pids = gpu_pids.get(idx, [])
        users = list(dict.fromkeys(
            pid_to_user[pid] for pid in pids if pid in pid_to_user
        ))
        gpus.append(GpuInfo(
            index=idx, name=shorten_gpu_name(p[2]), util=p[3],
            mem_used=p[4], mem_total=p[5], temp=p[6], power=p[7], power_cap=p[8],
            pids=pids, users=users,
        ))
    return gpus, mem_info, ""


def collect_basic() -> Tuple[List[dict], List[JobInfo], List[PendingJob], Dict[str, List[JobInfo]], str]:
    """Phase 1: fast local commands only (sinfo + squeue)."""
    with ThreadPoolExecutor(max_workers=3) as ex:
        f_nodes = ex.submit(collect_nodes_basic)
        f_jobs = ex.submit(collect_jobs)
        f_pending = ex.submit(collect_pending_jobs)
        nodes_raw, e1 = f_nodes.result()
        jobs, e2 = f_jobs.result()
        pending, e3 = f_pending.result()
    err = " | ".join(x for x in [e1, e2, e3] if x)
    node_jobs: Dict[str, List[JobInfo]] = {}
    for j in jobs:
        node_jobs.setdefault(j.node, []).append(j)
    return nodes_raw, jobs, pending, node_jobs, err


def build_nodes(
    nodes_raw: List[dict],
    node_jobs: Dict[str, List[JobInfo]],
    ssh_results: Dict[str, NodeSSHResult],
    stale_nodes: List[str],
) -> List[NodeInfo]:
    result: List[NodeInfo] = []
    for n in nodes_raw:
        name = n["name"]
        r = ssh_results.get(name)
        gpus = r.gpus if r else []
        gerr = r.error if r else ""
        mem = r.mem if r else NodeMemInfo()
        result.append(NodeInfo(
            name=name, state=n["state"], cpus=n["cpus"],
            cpu_alloc=n.get("cpu_alloc", ""), cpu_load=n["cpu_load"],
            mem_total=n["mem_total"], mem_free=n["mem_free"], gres=n["gres"],
            gpus=gpus, jobs=node_jobs.get(name, []), error=gerr,
            mem_used=mem.used, mem_avail=mem.avail,
            stale=(name in stale_nodes),
            error_kind=r.error_kind.value if r and hasattr(r, 'error_kind') else "",
        ))
    return result


# Per-node cache for fallback
_node_cache: Dict[str, Tuple[List[GpuInfo], NodeMemInfo]] = {}


def collect_node_data_parallel(
    node_names: List[str], node_timeout: int = 30, max_workers: int = 8, cache=None,
) -> Tuple[Dict[str, NodeSSHResult], List[str], List[str]]:
    """Phase 2: SSH to nodes. Returns results, stale_nodes, errors."""
    active_cache = cache if cache is not None else _node_cache

    ssh_results: Dict[str, NodeSSHResult] = {}
    stale_nodes: List[str] = []
    errors: List[str] = []

    with ThreadPoolExecutor(max_workers=min(max_workers, len(node_names))) as ex:
        futs = {ex.submit(collect_node_data, n, node_timeout): n for n in node_names}
        for fut in as_completed(futs):
            name = futs[fut]
            gpus, mem, err = fut.result()
            if err:
                gpus2, mem2, err2 = collect_node_data(name, node_timeout)
                if not err2:
                    gpus, mem, err = gpus2, mem2, err2
            if err and name in active_cache:
                cached_gpus, cached_mem = active_cache[name]
                ssh_results[name] = NodeSSHResult(cached_gpus, cached_mem, "", error_kind=NodeErrorKind.STALE_CACHED)
                stale_nodes.append(name)
            else:
                kind = _classify_error(err) if err else NodeErrorKind.OK
                ssh_results[name] = NodeSSHResult(gpus, mem, err, error_kind=kind)
                if gpus or mem.total:
                    active_cache[name] = (gpus, mem)
                if err:
                    errors.append(f"{name}: {err}")

    if stale_nodes:
        errors.append(f"cached: {','.join(stale_nodes)}")

    return ssh_results, stale_nodes, errors
