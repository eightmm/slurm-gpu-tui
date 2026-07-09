"""Shared SSH helpers, data models, and collection logic."""
from __future__ import annotations

import atexit
import os
import re
import shlex
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field, replace
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
    UNKNOWN = "unknown"


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
        try:
            alive = subprocess.call(
                ["ssh", "-o", f"ControlPath={sock}", "-O", "check", node],
                stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=5,
            ) == 0
        except Exception:
            alive = False
        if alive:
            return
        # Dead master leaves a stale socket behind; remove it so we reconnect
        try:
            os.unlink(sock)
        except OSError:
            pass
    cmd = (
        f"ssh -o ControlMaster=yes {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=10 -o ControlPersist=1800 -fN {node}"
    )
    try:
        subprocess.call(
            shlex.split(cmd), stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=20,
        )
    except Exception:
        pass  # per-command ssh will surface the real error


def ssh_cmd(node: str, inner_cmd: str, timeout: int = 10) -> Tuple[bool, str]:
    init_ssh_pool()
    ssh_ensure_master(node)
    wrapped = (
        f"ssh -o ControlMaster=no {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=3 {node} {shlex.quote(inner_cmd)}"
    )
    return run_cmd(wrapped, timeout=timeout)


# ── Data models ───────────────────────────────────────────────────────────

@dataclass
class GpuInfo:
    index: str = ""      # nvidia-smi enumeration order (PCI bus order)
    minor: str = ""      # /dev/nvidiaN number — what SLURM GRES IDX refers to.
                         # Can differ from index (probe order != PCI order)!
    name: str = ""
    util: str = ""       # %
    mem_used: str = ""   # MiB
    mem_total: str = ""  # MiB
    temp: str = ""       # C
    power: str = ""      # W
    power_cap: str = ""  # W
    pids: List[str] = field(default_factory=list)
    users: List[str] = field(default_factory=list)
    alloc_jobid: str = ""  # job holding this GPU per SLURM allocation
    alloc_user: str = ""
    idle_sec: int = 0    # how long allocated with no GPU process (collector only)
    parked_sec: int = 0  # how long VRAM held at ~0% util (collector only)


@dataclass
class JobInfo:
    jobid: str = ""
    user: str = ""
    partition: str = ""
    jobname: str = ""
    elapsed: str = ""
    node: str = ""
    gpu_count: int = 0
    cpu_count: int = 0
    gres_raw: str = ""
    time_limit: str = ""
    script: str = ""  # batch script when SHARE_SCRIPTS collector publishes it


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
    start_time: str = ""  # scheduler's estimated start (squeue %S)


@dataclass
class NodeMemInfo:
    total: str = ""   # MB
    used: str = ""    # MB
    avail: str = ""   # MB


@dataclass
class NodeInfo:
    name: str = ""
    state: str = ""
    partition: str = ""  # comma-joined partitions from sinfo
    source: str = ""     # data origin: agent / ssh / stale (collector only)
    has_gpu: bool = True  # False for CPU-only nodes (shown on the CPU tab only)
    cpus: str = ""
    cpu_alloc: str = ""
    cpu_load: str = ""
    mem_total: str = ""
    mem_free: str = ""
    mem_alloc: str = ""  # slurm AllocMem (MB) — works without node access
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
    return NodeErrorKind.UNKNOWN


# ── Data collection ──────────────────────────────────────────────────────

def collect_jobs() -> Tuple[List[JobInfo], str]:
    cmd = 'squeue -h -t R -o "%i|%u|%P|%j|%M|%N|%b|%l|%C"'
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
        try:
            cc = int(p[8].strip()) if len(p) > 8 else 0
        except ValueError:
            cc = 0
        rows.append(JobInfo(p[0], p[1], p[2], p[3], p[4], p[5], gc, cc, gres, tlimit))
    return rows, ""


def collect_pending_jobs() -> Tuple[List[PendingJob], str]:
    cmd = 'squeue -h -t PD -o "%i|%u|%P|%j|%l|%b|%r|%Q|%S"'
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
        start = p[8].strip() if len(p) > 8 else ""
        rows.append(PendingJob(p[0], p[1], p[2], p[3], p[4], gc, p[6].strip(), p[7].strip(), start))
    return rows, ""


def collect_nodes_basic() -> Tuple[List[dict], str]:
    cmd = 'sinfo -N -h -o "%N|%T|%c|%O|%m|%e|%G|%C|%P"'
    ok, out = run_cmd(cmd)
    if not ok:
        return [], f"sinfo failed: {out}"
    rows: List[dict] = []
    by_name: Dict[str, dict] = {}
    for line in out.splitlines():
        p = line.split("|")
        if len(p) < 9:
            continue
        name = p[0].strip()
        partition = p[8].strip().rstrip("*")
        if name in by_name:
            # Node listed once per partition — accumulate partitions
            row = by_name[name]
            if partition and partition not in row["partition"].split(","):
                row["partition"] = f"{row['partition']},{partition}" if row["partition"] else partition
            continue
        gres = p[6].strip()
        cpus_aiot = p[7].strip()
        cpu_alloc = ""
        parts = cpus_aiot.split("/")
        if len(parts) >= 4:
            cpu_alloc = parts[0]
        row = {
            "name": name, "state": p[1].strip(), "cpus": p[2].strip(),
            "cpu_load": p[3].strip(), "mem_total": p[4].strip(),
            "mem_free": p[5].strip(), "gres": gres,
            "has_gpu": "gpu" in gres.lower(),
            "cpu_alloc": cpu_alloc, "partition": partition,
        }
        by_name[name] = row
        rows.append(row)
    return rows, ""


def expand_nodelist(expr: str) -> List[str]:
    """Expand a SLURM nodelist like 'gpu[1-3,5],node7' into hostnames."""
    hosts: List[str] = []
    for part in re.findall(r"[^,\[\]]+(?:\[[^\]]*\])?", expr):
        m = re.match(r"^(.*?)\[([^\]]+)\]$", part)
        if not m:
            if part:
                hosts.append(part)
            continue
        prefix, ranges = m.group(1), m.group(2)
        for r in ranges.split(","):
            if "-" in r:
                a, b = r.split("-", 1)
                width = len(a)
                for i in range(int(a), int(b) + 1):
                    hosts.append(f"{prefix}{str(i).zfill(width)}")
            else:
                hosts.append(f"{prefix}{r}")
    return hosts


def _expand_idx(spec: str) -> List[str]:
    """Expand a GPU index spec like '0-1,3' into ['0','1','3']."""
    out: List[str] = []
    for r in spec.split(","):
        r = r.strip()
        if not r or r.upper() == "N/A":
            continue
        if "-" in r:
            a, b = r.split("-", 1)
            out.extend(str(i) for i in range(int(a), int(b) + 1))
        else:
            out.append(r)
    return out


def parse_gres_models(gres: str) -> List[str]:
    """Expand sinfo GRES like 'gpu:h100:1(S:0-1),gpu:3' into per-GPU model names."""
    out: List[str] = []
    for part in gres.split(","):
        m = re.match(r"gpu(?::([^:(]+))?:(\d+)", part.strip())
        if m:
            model = (m.group(1) or "").strip()
            out.extend([model] * int(m.group(2)))
    return out


def parse_gpu_alloc(out: str) -> Dict[str, Dict[str, str]]:
    """Parse `scontrol -o show job -d` output: node -> gpu index -> jobid.

    Reads per-node detail segments like 'Nodes=gpu4 CPU_IDs=... Mem=... GRES=gpu:1(IDX:0)'.
    """
    alloc: Dict[str, Dict[str, str]] = {}
    for line in out.splitlines():
        if "JobState=RUNNING" not in line:
            continue
        m_id = re.search(r"JobId=(\d+)", line)
        if not m_id:
            continue
        jobid = m_id.group(1)
        for m in re.finditer(r"Nodes=(\S+)\s+CPU_IDs=\S+\s+Mem=\S+\s+GRES=(\S+)", line):
            nodes_expr, gres = m.group(1), m.group(2)
            gm = re.search(r"gpu[^(]*\(IDX:([^)]+)\)", gres)
            if not gm:
                continue
            idxs = _expand_idx(gm.group(1))
            for node in expand_nodelist(nodes_expr):
                d = alloc.setdefault(node, {})
                for i in idxs:
                    d[i] = jobid
    return alloc


def collect_gpu_alloc() -> Tuple[Dict[str, Dict[str, str]], str]:
    """Exact GPU allocation from scontrol: node -> gpu index -> jobid."""
    ok, out = run_cmd("scontrol -o show job -d")
    if not ok:
        if "no jobs" in out.lower():
            return {}, ""
        return {}, f"scontrol failed: {out}"
    return parse_gpu_alloc(out), ""


# Combined node-side payload command: nvidia-smi metrics + pmon (PID→GPU)
# + meminfo + ps (PID→user). Run remotely via SSH (pull) or locally by the
# resident agent (push).
NODE_PAYLOAD_CMD = (
    "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
    "temperature.gpu,power.draw,power.limit,pci.bus_id "
    "--format=csv,noheader,nounits 2>/dev/null; "
    "echo '---SEP---'; "
    "nvidia-smi pmon -c 1 -s m 2>/dev/null; "
    "echo '---SEP---'; "
    "awk '/^MemTotal:/{ t=$2 } /^MemAvailable:/{ a=$2 } "
    "END{ printf \"%d %d %d\", t/1024, (t-a)/1024, a/1024 }' /proc/meminfo; "
    "echo '---SEP---'; "
    "PIDS=$(nvidia-smi pmon -c 1 -s u 2>/dev/null | awk 'NR>2 && $2!= \"-\" {print $2}' | tr '\\n' ','); "
    "if [ -n \"$PIDS\" ]; then ps -p ${PIDS%,} -o pid=,user= 2>/dev/null; fi; "
    "echo '---SEP---'; "
    # PCI bus -> /dev/nvidiaN minor. SLURM's GRES IDX means the minor, and
    # minor order can differ from nvidia-smi (PCI) order on some boards.
    "for d in /proc/driver/nvidia/gpus/*/information; do "
    "awk '/Bus Location/{b=$NF} /Device Minor/{m=$NF} END{print b, m}' \"$d\" 2>/dev/null; "
    "done"
)


def collect_node_data(node: str, timeout: int = 30) -> Tuple[List[GpuInfo], NodeMemInfo, str]:
    """SSH to node and run the combined payload command."""
    ok, out = ssh_cmd(node, NODE_PAYLOAD_CMD, timeout=timeout)
    if not ok:
        return [], NodeMemInfo(), out if out else "ssh failed"
    gpus, mem_info = parse_node_payload(out)
    return gpus, mem_info, ""


def parse_node_payload(out: str) -> Tuple[List[GpuInfo], NodeMemInfo]:
    """Parse the combined SSH payload (metrics/pmon/meminfo/ps sections)."""
    sections = out.split("---SEP---")
    metrics_raw = sections[0].strip() if len(sections) > 0 else ""
    pmon_raw = sections[1].strip() if len(sections) > 1 else ""
    mem_raw = sections[2].strip() if len(sections) > 2 else ""
    ps_raw = sections[3].strip() if len(sections) > 3 else ""
    minor_raw = sections[4].strip() if len(sections) > 4 else ""

    # "0000:06:00.0 2" -> {"06:00.0": "2"}; nvidia-smi prints the bus id with
    # a longer domain ("00000000:06:00.0"), so compare on the bus:dev.fn tail
    bus_to_minor: Dict[str, str] = {}
    for line in minor_raw.splitlines():
        parts = line.split()
        if len(parts) == 2 and ":" in parts[0]:
            bus_to_minor[parts[0].split(":", 1)[1].lower()] = parts[1]

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
        minor = ""
        if len(p) >= 10 and ":" in p[9]:
            minor = bus_to_minor.get(p[9].split(":", 1)[1].lower(), "")
        gpus.append(GpuInfo(
            index=idx, minor=minor, name=shorten_gpu_name(p[2]), util=p[3],
            mem_used=p[4], mem_total=p[5], temp=p[6], power=p[7], power_cap=p[8],
            pids=pids, users=users,
        ))
    return gpus, mem_info


def collect_mem_alloc() -> Tuple[Dict[str, str], str]:
    """Slurm-allocated memory (MB) per node from scontrol — no SSH needed."""
    ok, out = run_cmd("scontrol -o show node")
    if not ok:
        return {}, f"scontrol node failed: {out}"
    res: Dict[str, str] = {}
    for line in out.splitlines():
        m = re.search(r"NodeName=(\S+)", line)
        a = re.search(r"AllocMem=(\d+)", line)
        if m and a:
            res[m.group(1)] = a.group(1)
    return res, ""


def collect_basic() -> Tuple[List[dict], List[JobInfo], List[PendingJob], Dict[str, List[JobInfo]], Dict[str, Dict[str, str]], str]:
    """Phase 1: fast local commands only (sinfo + squeue + scontrol)."""
    with ThreadPoolExecutor(max_workers=5) as ex:
        f_nodes = ex.submit(collect_nodes_basic)
        f_jobs = ex.submit(collect_jobs)
        f_pending = ex.submit(collect_pending_jobs)
        f_alloc = ex.submit(collect_gpu_alloc)
        f_mem = ex.submit(collect_mem_alloc)
        nodes_raw, e1 = f_nodes.result()
        jobs, e2 = f_jobs.result()
        pending, e3 = f_pending.result()
        gpu_alloc, e4 = f_alloc.result()
        mem_alloc, e5 = f_mem.result()
    for n in nodes_raw:
        n["mem_alloc"] = mem_alloc.get(n["name"], "")
    err = " | ".join(x for x in [e1, e2, e3, e4, e5] if x)
    return nodes_raw, jobs, pending, assign_node_jobs(jobs), gpu_alloc, err


def assign_node_jobs(jobs: List[JobInfo]) -> Dict[str, List[JobInfo]]:
    """Per-node job map. Multi-node jobs arrive as compressed nodelists
    ('gpu[3-4]') — expand them, and split the job's total CPU count across
    its nodes so per-node core sums stay correct."""
    node_jobs: Dict[str, List[JobInfo]] = {}
    for j in jobs:
        nodes = expand_nodelist(j.node) or ([j.node] if j.node else [])
        n = len(nodes)
        if n <= 1:
            for node in nodes:
                node_jobs.setdefault(node, []).append(j)
            continue
        base, rem = divmod(j.cpu_count, n)
        for i, node in enumerate(nodes):
            node_jobs.setdefault(node, []).append(
                replace(j, cpu_count=base + (1 if i < rem else 0)))
    return node_jobs


def apply_gpu_alloc(
    nodes: List[NodeInfo], gpu_alloc: Dict[str, Dict[str, str]], jobs: List[JobInfo],
) -> None:
    """Annotate GPUs with the job/user that holds them per SLURM allocation."""
    jobid_user = {j.jobid: j.user for j in jobs}
    for node in nodes:
        node_alloc = gpu_alloc.get(node.name, {})
        for g in node.gpus:
            # SLURM GRES IDX = device minor, not nvidia-smi order
            jid = node_alloc.get(g.minor or g.index, "")
            g.alloc_jobid = jid
            g.alloc_user = jobid_user.get(jid, "")


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
            name=name, state=n["state"], partition=n.get("partition", ""), cpus=n["cpus"],
            cpu_alloc=n.get("cpu_alloc", ""), cpu_load=n["cpu_load"],
            mem_total=n["mem_total"], mem_free=n["mem_free"],
            mem_alloc=n.get("mem_alloc", ""), gres=n["gres"],
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
            try:
                gpus, mem, err = fut.result()
            except Exception as e:
                gpus, mem, err = [], NodeMemInfo(), f"collect failed: {e}"
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
