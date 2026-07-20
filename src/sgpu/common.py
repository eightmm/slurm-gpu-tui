"""Shared SSH helpers, data models, and collection logic."""
from __future__ import annotations

import atexit
import os
import pwd
import re
import shlex
import subprocess
import tempfile
import threading
import time
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


# GPU processes by these users never count as rogue (system daemons)
ROGUE_IGNORE = {
    u for u in os.getenv("SLURM_GPU_TUI_ROGUE_IGNORE", "root,gdm,xdm").split(",") if u
}


_uid_name_cache: Dict[str, str] = {}


def resolve_user(name: str) -> str:
    """Map a bare numeric UID to a login name via the master's name service.

    Compute nodes often lack a passwd entry for cluster users (home is NFS but
    the account DB isn't shared), so node-side `ps` reports the UID number.
    This runs on the master, where the name resolves. Non-numeric names (already
    resolved) and unknown UIDs pass through unchanged. Call only on the master
    — a compute node would fail the same lookup.
    """
    if not name or not name.isdigit():
        return name
    cached = _uid_name_cache.get(name)
    if cached is not None:
        return cached
    try:
        resolved = pwd.getpwuid(int(name)).pw_name
    except (KeyError, ValueError, OverflowError):
        resolved = name
    _uid_name_cache[name] = resolved
    return resolved


# ── Shell helpers ─────────────────────────────────────────────────────────

def run_cmd(cmd: str, timeout: int = 12) -> Tuple[bool, str]:
    try:
        out = subprocess.check_output(
            shlex.split(cmd), stderr=subprocess.STDOUT, timeout=timeout, text=True
        )
        return True, out.strip()
    except subprocess.CalledProcessError as e:
        # the captured stderr is what error classification and logs need,
        # not "returned non-zero exit status 1"
        out = (e.output or "").strip()
        return False, out or str(e)
    except Exception as e:
        return False, str(e)


# ── SSH ControlMaster pool ────────────────────────────────────────────────

_SSH_CONTROL_DIR: str = ""
_SSH_BASE_OPTS: str = ""
# node -> monotonic ts of the last confirmed-alive master check; skips the
# extra `ssh -O check` subprocess per command while the master is trusted
_MASTER_ALIVE_TTL = 60.0
_master_alive: Dict[str, float] = {}
_master_locks: Dict[str, threading.Lock] = {}
_master_locks_guard = threading.Lock()


def init_ssh_pool() -> None:
    """Initialize SSH ControlMaster socket directory."""
    global _SSH_CONTROL_DIR, _SSH_BASE_OPTS
    if _SSH_CONTROL_DIR:
        return
    _SSH_CONTROL_DIR = tempfile.mkdtemp(prefix="sgpu-ssh-")
    # %h only: the check path below must match what ssh resolves, and %r
    # (remote user) is not knowable here when USER differs from it
    _SSH_BASE_OPTS = (
        f"-o ControlPath={_SSH_CONTROL_DIR}/%h "
        "-o StrictHostKeyChecking=no -o BatchMode=yes"
    )
    atexit.register(cleanup_ssh_pool)


def cleanup_ssh_pool() -> None:
    """Remove SSH socket directory."""
    import shutil
    if _SSH_CONTROL_DIR:
        shutil.rmtree(_SSH_CONTROL_DIR, ignore_errors=True)


def _node_lock(node: str) -> threading.Lock:
    with _master_locks_guard:
        return _master_locks.setdefault(node, threading.Lock())


def ssh_ensure_master(node: str) -> None:
    """Start a ControlMaster connection to a node if not already running.
    Per-node locked so parallel pollers don't spawn duplicate masters."""
    init_ssh_pool()
    if time.monotonic() - _master_alive.get(node, 0.0) < _MASTER_ALIVE_TTL:
        return
    with _node_lock(node):
        if time.monotonic() - _master_alive.get(node, 0.0) < _MASTER_ALIVE_TTL:
            return
        sock = f"{_SSH_CONTROL_DIR}/{node}"
        if os.path.exists(sock):
            try:
                alive = subprocess.call(
                    ["ssh", "-o", f"ControlPath={sock}", "-O", "check", node],
                    stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=5,
                ) == 0
            except Exception:
                alive = False
            if alive:
                _master_alive[node] = time.monotonic()
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
            rc = subprocess.call(
                shlex.split(cmd), stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=20,
            )
            if rc == 0:
                _master_alive[node] = time.monotonic()
        except Exception:
            pass  # per-command ssh will surface the real error


def ssh_cmd(node: str, inner_cmd: str, timeout: int = 10) -> Tuple[bool, str]:
    init_ssh_pool()
    ssh_ensure_master(node)
    wrapped = (
        f"ssh -o ControlMaster=no {_SSH_BASE_OPTS} "
        f"-o ConnectTimeout=3 {node} {shlex.quote(inner_cmd)}"
    )
    ok, out = run_cmd(wrapped, timeout=timeout)
    if not ok:
        # connection-level failure: distrust the cached master so the next
        # call re-checks (command-level failures also drop it — cheap re-check)
        _master_alive.pop(node, None)
    return ok, out


# ── Data models ───────────────────────────────────────────────────────────

@dataclass
class GpuInfo:
    index: str = ""      # nvidia-smi enumeration order (PCI bus order)
    minor: str = ""      # /dev/nvidiaN number — what SLURM GRES IDX refers to.
                         # Can differ from index (probe order != PCI order)!
    uuid: str = ""       # durable hardware id (RMA / physical identification)
    pci_bus: str = ""    # PCI bus address
    slot: str = ""       # physical PCIe slot (SMBIOS number; "" when unknown)
    serial: str = ""     # board serial ("" / N/A on consumer GPUs)
    name: str = ""
    util: str = ""       # %
    mem_used: str = ""   # MiB
    mem_total: str = ""  # MiB
    temp: str = ""       # C
    power: str = ""      # W
    power_cap: str = ""  # W
    ecc: str = ""        # uncorrectable ECC error count ("" / N/A on consumer GPUs)
    sm_clock: str = ""   # current SM clock MHz
    mem_clock: str = ""  # current memory clock MHz
    pids: List[str] = field(default_factory=list)
    users: List[str] = field(default_factory=list)
    pid_mem: Dict[str, str] = field(default_factory=dict)    # pid -> FB MiB (pmon)
    pid_jobid: Dict[str, str] = field(default_factory=dict)  # pid -> SLURM job (cgroup)
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
        gc = _gpu_count_from_gres(gres)
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
        gc = _gpu_count_from_gres(gres)
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


def _gpu_count_from_gres(gres: str) -> int:
    """Total GPUs in an squeue ``%b`` value, including mixed typed GRES.

    Examples: ``gpu:2`` and ``gpu:h100:1,gpu:a6000:2``.  The previous
    first-match parser under-counted jobs requesting more than one GPU type.
    """
    return sum(int(n) for n in re.findall(
        r"(?:^|,)gpu(?::[^:,()]+)?:(\d+)(?=\(|,|$)", gres,
    ))


def parse_gres_models(gres: str) -> List[str]:
    """Expand sinfo GRES like 'gpu:h100:1(S:0-1),gpu:3' into per-GPU model names."""
    out: List[str] = []
    for part in gres.split(","):
        m = re.match(r"gpu(?::([^:(]+))?:(\d+)", part.strip())
        if m:
            model = (m.group(1) or "").strip()
            out.extend([model] * int(m.group(2)))
    return out


def parse_gpu_alloc(out: str) -> Tuple[Dict[str, Dict[str, str]], Dict[str, str]]:
    """Parse `scontrol -o show job -d`: (node -> gpu IDX -> jobid, jobid -> user).

    Reads per-node detail segments like 'Nodes=gpu4 CPU_IDs=... Mem=... GRES=gpu:1(IDX:0)'.
    The IDX is SLURM's node GRES index. On single-type nodes it equals the
    device minor, but on heterogeneous nodes SLURM's per-type index order does
    NOT track /dev/nvidiaN minors, so this map is only a placement *hint* —
    apply_gpu_alloc reconciles it against the GPUs' real process owners.
    The jobid->user map comes from scontrol's own `UserId=name(uid)` field: it
    carries the login name even for array tasks (whose real jobid never appears
    in squeue's `38182_0` notation) and for users with no node-side passwd entry.
    """
    alloc: Dict[str, Dict[str, str]] = {}
    jobid_user: Dict[str, str] = {}
    for line in out.splitlines():
        # COMPLETING keeps its GRES detail while epilog/process teardown runs;
        # dropping it made every job's final seconds look like rogue GPU use
        if "JobState=RUNNING" not in line and "JobState=COMPLETING" not in line:
            continue
        m_id = re.search(r"JobId=(\d+)", line)
        if not m_id:
            continue
        jobid = m_id.group(1)
        m_u = re.search(r"UserId=([^(\s]+)\(", line)
        if m_u:
            jobid_user[jobid] = m_u.group(1)
        for m in re.finditer(r"Nodes=(\S+)\s+CPU_IDs=\S+\s+Mem=\S+\s+GRES=(\S+)", line):
            nodes_expr, gres = m.group(1), m.group(2)
            idx_groups = re.findall(
                r"(?:^|,)gpu(?::[^,()]*)?\(IDX:([^)]+)\)", gres,
            )
            if not idx_groups:
                continue
            idxs = [idx for group in idx_groups for idx in _expand_idx(group)]
            for node in expand_nodelist(nodes_expr):
                d = alloc.setdefault(node, {})
                for i in idxs:
                    d[i] = jobid
    return alloc, jobid_user


def collect_gpu_alloc() -> Tuple[Dict[str, Dict[str, str]], Dict[str, str], str]:
    """Exact GPU allocation from scontrol: (node->idx->jobid, jobid->user, err)."""
    ok, out = run_cmd("scontrol -o show job -d")
    if not ok:
        if "no jobs" in out.lower():
            return {}, {}, ""
        return {}, {}, f"scontrol failed: {out}"
    alloc, jobid_user = parse_gpu_alloc(out)
    return alloc, jobid_user, ""


# Combined node-side payload command: nvidia-smi metrics + pmon (PID→GPU)
# + meminfo + ps (PID→user). Run remotely via SSH (pull) or locally by the
# resident agent (push).
NODE_PAYLOAD_CMD = (
    # NOTE: pci.bus_id must stay at index 9 (minor mapping reads p[9]); new
    # columns append after. Consumer GPUs report ecc/serial as [N/A].
    "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
    "temperature.gpu,power.draw,power.limit,pci.bus_id,"
    "ecc.errors.uncorrected.aggregate.total,serial,clocks.sm,clocks.mem "
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
    "done; "
    "echo '---SEP---'; "
    # PID -> SLURM jobid from the process's cgroup path (job_<id> under the
    # slurmstepd scope). World-readable, so this works for any user's PID —
    # gives exact GPU->job attribution with no user-name heuristics.
    "if [ -n \"$PIDS\" ]; then for p in $(echo ${PIDS%,} | tr ',' ' '); do "
    "j=$(grep -m1 -oE 'job_[0-9]+' /proc/$p/cgroup 2>/dev/null); "
    "[ -n \"$j\" ] && echo \"$p ${j#job_}\"; "
    "done; fi; "
    "echo '---SEP---'; "
    # PCI bus -> physical slot number (SMBIOS, via /sys/bus/pci/slots — no
    # root needed). A GPU behind a riser/PLX bridge has no slot entry of its
    # own, so walk the sysfs ancestor chain and take the deepest ancestor
    # whose bus address matches a slot.
    "SLOTS=$(for s in /sys/bus/pci/slots/*/address; do [ -e \"$s\" ] || continue; "
    "p=${s%/address}; printf '%s %s\\n' \"${p##*/}\" \"$(cat \"$s\")\"; done 2>/dev/null); "
    "for d in /proc/driver/nvidia/gpus/*/information; do "
    "b=$(awk '/Bus Location/{print $NF}' \"$d\" 2>/dev/null); "
    "if [ -n \"$b\" ]; then "
    "rp=$(readlink -f \"/sys/bus/pci/devices/$b\" 2>/dev/null); slot=; "
    "for c in $(printf '%s\\n' \"$rp\" | tr '/' ' '); do case \"$c\" in *:*.*) "
    "m=$(printf '%s\\n' \"$SLOTS\" | awk -v a=\"${c%.*}\" '$2==a{print $1; exit}'); "
    "[ -n \"$m\" ] && slot=$m;; esac; done; "
    "if [ -n \"$slot\" ]; then echo \"$b $slot\"; fi; fi; done; true"
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
    jobid_raw = sections[5].strip() if len(sections) > 5 else ""
    slot_raw = sections[6].strip() if len(sections) > 6 else ""

    # "0000:06:00.0 2" -> {"06:00.0": "2"}; nvidia-smi prints the bus id with
    # a longer domain ("00000000:06:00.0"), so compare on the bus:dev.fn tail
    bus_to_minor: Dict[str, str] = {}
    for line in minor_raw.splitlines():
        parts = line.split()
        if len(parts) == 2 and ":" in parts[0]:
            bus_to_minor[parts[0].split(":", 1)[1].lower()] = parts[1]

    # "0000:06:00.0 4" -> {"06:00.0": "4"} (physical slot; same tail-matching)
    bus_to_slot: Dict[str, str] = {}
    for line in slot_raw.splitlines():
        parts = line.split()
        if len(parts) == 2 and ":" in parts[0]:
            bus_to_slot[parts[0].split(":", 1)[1].lower()] = parts[1]

    mem_info = NodeMemInfo()
    mem_parts = mem_raw.split()
    if len(mem_parts) >= 3:
        mem_info = NodeMemInfo(total=mem_parts[0], used=mem_parts[1], avail=mem_parts[2])

    # Parse pmon (-s m: gpu pid type fb ccpm cmd): gpu_idx -> PIDs, pid -> FB MiB
    gpu_pids: Dict[str, List[str]] = {}
    pid_fb: Dict[str, str] = {}
    for line in pmon_raw.splitlines():
        line = line.strip()
        if line.startswith("#") or not line:
            continue
        parts = line.split()
        if len(parts) >= 2 and parts[1] != "-":
            gpu_pids.setdefault(parts[0], []).append(parts[1])
            if len(parts) >= 4 and parts[3] not in ("-", ""):
                pid_fb[parts[1]] = parts[3]

    # Resolve PIDs to usernames via ps output from combined SSH call
    pid_to_user: Dict[str, str] = {}
    for line in ps_raw.splitlines():
        ps_parts = line.split()
        if len(ps_parts) >= 2:
            pid_to_user[ps_parts[0]] = ps_parts[1]

    # PID -> SLURM jobid from the node-side cgroup probe (exact attribution)
    pid_jobid_all: Dict[str, str] = {}
    for line in jobid_raw.splitlines():
        jp = line.split()
        if len(jp) == 2 and jp[1].isdigit():
            pid_jobid_all[jp[0]] = jp[1]

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
        pci_bus = p[9] if len(p) >= 10 else ""
        minor = slot = ""
        if pci_bus and ":" in pci_bus:
            tail = pci_bus.split(":", 1)[1].lower()
            minor = bus_to_minor.get(tail, "")
            slot = bus_to_slot.get(tail, "")
        ecc = p[10] if len(p) >= 11 else ""
        serial = p[11] if len(p) >= 12 else ""
        sm_clock = p[12] if len(p) >= 13 else ""
        mem_clock = p[13] if len(p) >= 14 else ""
        gpus.append(GpuInfo(
            index=idx, minor=minor, uuid=p[1], pci_bus=pci_bus, slot=slot,
            serial=serial,
            name=shorten_gpu_name(p[2]), util=p[3],
            mem_used=p[4], mem_total=p[5], temp=p[6], power=p[7], power_cap=p[8],
            ecc=ecc, sm_clock=sm_clock, mem_clock=mem_clock,
            pids=pids, users=users,
            pid_mem={pid: pid_fb[pid] for pid in pids if pid in pid_fb},
            pid_jobid={pid: pid_jobid_all[pid] for pid in pids if pid in pid_jobid_all},
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


def collect_basic() -> Tuple[List[dict], List[JobInfo], List[PendingJob], Dict[str, List[JobInfo]], Dict[str, Dict[str, str]], Dict[str, str], str]:
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
        gpu_alloc, alloc_user_map, e4 = f_alloc.result()
        mem_alloc, e5 = f_mem.result()
    for n in nodes_raw:
        n["mem_alloc"] = mem_alloc.get(n["name"], "")
    err = " | ".join(x for x in [e1, e2, e3, e4, e5] if x)
    return nodes_raw, jobs, pending, assign_node_jobs(jobs), gpu_alloc, alloc_user_map, err


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


def reconcile_gpu_alloc(
    node_alloc: Dict[str, str], jobid_user: Dict[str, str],
    gpus: List[Tuple[List[str], str, List[str]]],
) -> List[Tuple[str, str]]:
    """Bind one node's SLURM allocations to physical GPUs: [(jobid, user)].

    ``gpus`` is one (real process users, minor-or-index key, process jobids
    from the node-side cgroup probe) triple per card.
    SLURM's GRES IDX only equals the device minor on single-type nodes; on
    heterogeneous nodes (e.g. an H100 alongside RTX-6000s) SLURM's per-type
    index order does not track /dev/nvidiaN, so keying purely on IDX paints
    the allocation onto the wrong physical card — a job shows up on an empty
    GPU while its process runs elsewhere. With task/cgroup + ConstrainDevices,
    a job's process can only touch its allocated GPU, so the GPU's real process
    owner is authoritative: bind by the process's own cgroup jobid first, then
    by process user, and place only genuinely idle reservations by the IDX hint.
    """
    # one entry per allocated GPU on this node (a job holding N GPUs
    # appears N times); consumed as we bind each to a physical card
    remaining = list(node_alloc.values())
    out: List[Tuple[str, str]] = [("", "")] * len(gpus)
    # 0) cgroup-exact: the process's own cgroup names its jobid — no
    #    heuristics, disambiguates same-user multi-job nodes.
    for i, (_users, _key, jobids) in enumerate(gpus):
        jid = next((j for j in jobids if j in remaining), "")
        if jid:
            out[i] = (jid, jobid_user.get(jid, ""))
            remaining.remove(jid)
    # 1) process-confirmed: a GPU running user U's process, where U holds
    #    an allocation here, belongs to that job. Covers payloads without
    #    the cgroup probe (old agents); self-correcting when the IDX->minor
    #    hint is wrong on mixed nodes.
    for i, (users, _key, _jobids) in enumerate(gpus):
        if out[i][0] or not users:
            continue
        jid = next((j for j in remaining if jobid_user.get(j, "") in users), "")
        if jid:
            out[i] = (jid, jobid_user.get(jid, ""))
            remaining.remove(jid)
    # 2) idle reservations: allocations with no observed process yet. Place
    #    each on an unbound, process-free card, preferring the one whose
    #    minor/index matches the raw IDX (exact on single-type nodes).
    for jid in remaining:
        pref = {k for k, j in node_alloc.items() if j == jid}
        free = [i for i, (users, _key, _jobids) in enumerate(gpus)
                if not out[i][0] and not users]
        tgt = next((i for i in free if gpus[i][1] in pref),
                   free[0] if free else None)
        if tgt is not None:
            out[tgt] = (jid, jobid_user.get(jid, ""))
    return out


def apply_gpu_alloc(
    nodes: List[NodeInfo], gpu_alloc: Dict[str, Dict[str, str]], jobs: List[JobInfo],
    alloc_user_map: Dict[str, str] | None = None,
) -> None:
    """Annotate GPUs with the job/user that holds them (see reconcile_gpu_alloc)."""
    # squeue's jobid can't be joined to an array task's real jobid; scontrol's
    # UserId map (alloc_user_map) can, so it wins where present.
    jobid_user = {j.jobid: j.user for j in jobs}
    if alloc_user_map:
        jobid_user.update({k: v for k, v in alloc_user_map.items() if v})
    for node in nodes:
        # node-side ps reports a bare UID when the node lacks the account
        for g in node.gpus:
            g.users = [resolve_user(u) for u in g.users]
        pairs = reconcile_gpu_alloc(
            gpu_alloc.get(node.name, {}), jobid_user,
            [([u for u in g.users if u not in ROGUE_IGNORE], g.minor or g.index,
              list(dict.fromkeys(g.pid_jobid.values())))
             for g in node.gpus])
        for g, (jid, user) in zip(node.gpus, pairs):
            g.alloc_jobid = jid
            g.alloc_user = user


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
