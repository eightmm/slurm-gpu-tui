"""Shared SSH helpers, data models, and collection logic."""
from __future__ import annotations

import atexit
import json
import os
import pwd
import re
import shlex
import subprocess
import tempfile
import threading
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import MISSING, dataclass, field, fields, replace
from enum import Enum
from functools import lru_cache
from itertools import product
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
    mem: str = ""  # requested memory from squeue %m (e.g. "128G", "4000Mc")
    script: str = ""  # batch script when SHARE_SCRIPTS collector publishes it
    # world-readable mirrors of the job's stdout/stderr tail, published by a
    # SHARE_LOGS collector; "" when sharing is off or nothing was readable
    log_out: str = ""
    log_err: str = ""
    # Bounded scheduler detail published by a privileged collector. Readers
    # use it when their own Slurm account cannot inspect another user's job.
    detail: str = ""
    log_status: Dict[str, str] = field(default_factory=dict)
    uid: int = -1  # validated scheduler owner; authorizes log mirroring


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
    detail: str = ""  # collector-published scontrol detail for cross-user UI


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
    cpu_power: str = ""  # W, CPU package via RAPL (agent payload only)
    ram_power: str = ""  # W, DRAM via RAPL (Intel only)
    sys_power: str = ""  # W, whole-node wall power from the BMC
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


# ── data.json codec ───────────────────────────────────────────────────────
# One decoder for the snapshot, shared by the TUI and the CLI. Hand-written
# per-reader parsers drifted: both silently dropped fields the collector had
# been publishing for releases (GPU clocks), and the two `sgpu --json` code
# paths emitted different shapes for the same command.

@lru_cache(maxsize=32)
def _dataclass_field_types(cls) -> tuple[tuple[str, type | None], ...]:
    """Precompute the defensive decoder schema for a dataclass.

    Only the expected *type* is retained.  In particular, values produced by
    mutable default factories are discarded here; every decoded instance is
    still constructed by the dataclass and receives fresh list/dict defaults.
    """
    schema = []
    for f in fields(cls):
        if f.default is not MISSING:
            default = f.default
        elif f.default_factory is not MISSING:  # type: ignore[misc]
            default = f.default_factory()       # type: ignore[misc]
        else:
            default = None
        schema.append((f.name, type(default) if default is not None else None))
    return tuple(schema)


def from_dict(cls, raw: object):
    """Rebuild a flat dataclass from its JSON form, defensively.

    Unknown keys are ignored — a newer collector may publish fields this
    reader has never heard of. Missing keys keep the dataclass default — an
    older collector may not publish them yet. A value whose JSON type does not
    match the field's default is dropped rather than propagated, so one
    malformed entry degrades a single field instead of killing the refresh.
    """
    if not isinstance(raw, dict):
        return cls()
    kwargs = {}
    for name, expected_type in _dataclass_field_types(cls):
        if name not in raw:
            continue
        value = raw[name]
        if expected_type is not None and not isinstance(value, expected_type):
            continue
        kwargs[name] = value
    return cls(**kwargs)


def node_from_dict(raw: object) -> "NodeInfo":
    """NodeInfo plus its nested GPU and job lists."""
    node = from_dict(NodeInfo, raw)
    if isinstance(raw, dict):
        node.gpus = [from_dict(GpuInfo, g) for g in raw.get("gpus") or []]
        node.jobs = [from_dict(JobInfo, j) for j in raw.get("jobs") or []]
    return node


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

_SQUEUE_COMBINED_CMD = (
    'env SLURM_BITSTR_LEN=0 squeue -h -t R,PD,CG -o '
    # JobName is intentionally absent. Slurm prints it verbatim, including
    # embedded newlines, so it cannot safely share this line-oriented protocol.
    # %U is the numeric owner UID. It also anchors the hardened compatibility
    # backend on Slurm releases that predate scontrol --json.
    '"%T|%i|%u|%U|%P|%M|%N|%b|%l|%C|%m|%r|%Q|%S"'
)

_SLURM_JOB_ID_RE = re.compile(
    r"[0-9]+(?:_[0-9]+|_\[[0-9,:%-]+\]|\+[0-9]+)?"
)


def valid_slurm_job_id(value: object) -> bool:
    """Whether ``value`` is a safe canonical squeue job/array/het ID."""
    return isinstance(value, str) and len(value) <= 8192 \
        and bool(_SLURM_JOB_ID_RE.fullmatch(value))


@dataclass(frozen=True)
class _QueueAnchor:
    """Scheduler-owned identity used to validate legacy scontrol records."""

    user: str
    uid: int
    state: str
    nodes: tuple[str, ...]


_QUEUE_USER_RE = re.compile(r"[A-Za-z0-9_.@-]{1,256}")
_NODE_NAME_RE = re.compile(r"[A-Za-z0-9_.-]{1,255}")
_LEGACY_MAX_NODES = 4096
_LEGACY_MAX_GPU_INDICES = 1024
_LEGACY_MAX_GPU_INDEX = 65535
_LEGACY_MAX_LINE_CHARS = 128 * 1024
_LEGACY_MAX_OUTPUT_CHARS = 32 * 1024 * 1024


def _strict_range_values(spec: str, limit: int) -> List[str] | None:
    values: List[str] = []
    for part in spec.split(","):
        if not part:
            return None
        start, sep, end = part.partition("-")
        if not start.isascii() or not start.isdigit():
            return None
        if sep:
            if not end.isascii() or not end.isdigit():
                return None
            lo, hi = int(start), int(end)
            if lo > hi or hi - lo + 1 > limit - len(values):
                return None
            values.extend(str(i).zfill(len(start)) for i in range(lo, hi + 1))
        else:
            values.append(start)
        if len(values) > limit:
            return None
    return values


def _strict_expand_nodes(expr: str) -> tuple[str, ...] | None:
    """Bounded expansion of a scheduler nodelist, rejecting odd spellings."""
    if not expr or expr in {"(null)", "N/A"}:
        return ()
    if len(expr) > 8192 or any(ord(ch) < 32 for ch in expr):
        return None
    hosts: List[str] = []
    for name in _split_top_level(expr):
        groups: List[List[str]] = []
        cursor = 0
        for match in _NODE_TOKEN_RE.finditer(name):
            if match.start() != cursor:
                return None
            cursor = match.end()
            spec, literal = match.group(1), match.group(2)
            if spec is None:
                if not literal or not re.fullmatch(r"[A-Za-z0-9_.-]+", literal):
                    return None
                values = [literal]
            else:
                values = _strict_range_values(spec, _LEGACY_MAX_NODES)
                if not values:
                    return None
            groups.append(values)
        if cursor != len(name) or not groups:
            return None
        total = 1
        for group in groups:
            total *= len(group)
            if total > _LEGACY_MAX_NODES - len(hosts):
                return None
        hosts.extend("".join(parts) for parts in product(*groups))
    if not hosts or len(set(hosts)) != len(hosts) \
            or any(not _NODE_NAME_RE.fullmatch(host) for host in hosts):
        return None
    return tuple(sorted(hosts))


def _parse_queue_output(
    out: str,
) -> Tuple[List[JobInfo], List[PendingJob], Dict[str, _QueueAnchor]]:
    jobs: List[JobInfo] = []
    pending: List[PendingJob] = []
    anchors_seen: Dict[str, List[_QueueAnchor]] = defaultdict(list)
    job_rows: List[tuple[str, JobInfo]] = []
    pending_rows: List[tuple[str, PendingJob]] = []
    for line in out.splitlines():
        p = line.split("|")
        if len(p) != 14:
            continue
        state = p[0].strip().upper()
        jobid = p[1].strip()
        user = p[2].strip()
        uid_text = p[3].strip()
        if state not in {"RUNNING", "PENDING", "COMPLETING"} \
                or not valid_slurm_job_id(jobid) \
                or not _QUEUE_USER_RE.fullmatch(user) \
                or not uid_text.isascii() or not uid_text.isdigit():
            continue
        uid = int(uid_text)
        if uid > 2 ** 32 - 1:
            continue
        nodes = () if state == "PENDING" else _strict_expand_nodes(p[6].strip())
        if nodes is None or (state in {"RUNNING", "COMPLETING"} and not nodes):
            continue
        anchor = _QueueAnchor(user=user, uid=uid, state=state, nodes=nodes)
        anchors_seen[jobid].append(anchor)
        if state == "RUNNING":
            gres = p[7].strip()
            try:
                cpu_count = int(p[9].strip())
            except ValueError:
                cpu_count = 0
            job_rows.append((jobid, JobInfo(
                jobid=jobid, user=user, uid=uid, partition=p[4].strip(),
                elapsed=p[5].strip(), node=p[6].strip(),
                gpu_count=_gpu_count_from_gres(gres),
                cpu_count=cpu_count, gres_raw=gres, time_limit=p[8].strip(),
                mem=p[10].strip(),
            )))
        elif state == "PENDING":
            gres = p[7].strip()
            pending_rows.append((jobid, PendingJob(
                jobid=jobid, user=user, partition=p[4].strip(),
                time_limit=p[8].strip(),
                gpu_count=_gpu_count_from_gres(gres), reason=p[11].strip(),
                priority=p[12].strip(), start_time=p[13].strip(),
            )))
    anchors = {
        jobid: values[0] for jobid, values in anchors_seen.items()
        if len(values) == 1
    }
    jobs.extend(job for jobid, job in job_rows if jobid in anchors)
    pending.extend(job for jobid, job in pending_rows if jobid in anchors)
    return jobs, pending, anchors


def _collect_queue_snapshot() -> Tuple[
    List[JobInfo], List[PendingJob], Dict[str, _QueueAnchor], str,
]:
    """Fetch UI rows plus a collision-free scheduler identity roster."""
    ok, out = run_cmd(_SQUEUE_COMBINED_CMD)
    if not ok:
        return [], [], {}, out
    jobs, pending, anchors = _parse_queue_output(out)
    return jobs, pending, anchors, ""


def _collect_queue() -> Tuple[List[JobInfo], List[PendingJob], str]:
    """Compatibility wrapper for callers that only need visible queue rows."""
    jobs, pending, _anchors, error = _collect_queue_snapshot()
    return jobs, pending, error


def collect_jobs() -> Tuple[List[JobInfo], str]:
    jobs, _pending, error = _collect_queue()
    if error:
        return [], f"squeue failed: {error}"
    return jobs, ""


def mem_to_mib(s: str, cpus: int = 1) -> float:
    """squeue %m value in MiB. Handles K/M/G/T suffixes and Slurm's trailing
    n (per node) / c (per CPU — multiplied by the job's CPU count).
    0.0 when empty or unparsable."""
    s = (s or "").strip()
    per_cpu = s.endswith(("c", "C"))
    if s.endswith(("n", "N", "c", "C")):
        s = s[:-1]
    m = re.match(r"^([\d.]+)([KMGT]?)$", s, re.I)
    if not m:
        return 0.0
    scale = {"K": 1 / 1024, "M": 1.0, "G": 1024.0, "T": 1024.0 * 1024, "": 1.0}
    val = float(m.group(1)) * scale[m.group(2).upper()]
    return val * (cpus if per_cpu and cpus > 0 else 1)


def collect_pending_jobs() -> Tuple[List[PendingJob], str]:
    _jobs, pending, error = _collect_queue()
    if error:
        return [], f"squeue PD failed: {error}"
    return pending, ""


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


# Guard against a malformed or hostile nodelist turning into a memory bomb
# ('gpu[1-99999999]'): far above any real cluster, far below trouble.
_MAX_EXPANSION = 100_000
_NODE_TOKEN_RE = re.compile(r"\[([^\]]*)\]|([^\[\]]+)")


def _split_top_level(expr: str) -> List[str]:
    """Split on commas that are not inside a bracket group."""
    out: List[str] = []
    depth = 0
    cur: List[str] = []
    for ch in expr:
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth = max(0, depth - 1)
        if ch == "," and depth == 0:
            out.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    out.append("".join(cur))
    return [s for s in (p.strip() for p in out) if s]


def _expand_range_spec(spec: str) -> List[str]:
    """'1-3,5' -> ['1','2','3','5'], preserving zero padding ('01-03')."""
    out: List[str] = []
    for r in (p.strip() for p in spec.split(",")):
        if not r:
            continue
        a, sep, b = r.partition("-")
        if sep and a.isdigit() and b.isdigit():
            lo, hi = int(a), int(b)
            if lo <= hi and hi - lo < _MAX_EXPANSION:
                out.extend(str(i).zfill(len(a)) for i in range(lo, hi + 1))
                continue
        out.append(r)  # not a numeric range — keep verbatim rather than raise
    return out


def expand_nodelist(expr: str) -> List[str]:
    """Expand a SLURM nodelist like 'gpu[1-3,5],node7' into hostnames.

    Handles several bracket groups in one name ('rack[1-2]node[3-4]') and a
    literal tail after a group ('gpu[1-2]-ib'). The previous version restarted
    parsing at every bracket, so both of those silently became extra bogus
    hostnames — and a non-numeric range ('gpu[a-b]') raised ValueError out of
    the collect loop instead of degrading.
    """
    hosts: List[str] = []
    for name in _split_top_level(expr):
        groups: List[List[str]] = []
        for m in _NODE_TOKEN_RE.finditer(name):
            spec, literal = m.group(1), m.group(2)
            groups.append([literal] if spec is None else _expand_range_spec(spec))
        if not groups:
            continue
        total = 1
        for g in groups:
            total *= max(1, len(g))
        if total > _MAX_EXPANSION:
            hosts.append(name)  # implausible; pass through rather than expand
            continue
        hosts.extend("".join(combo) for combo in product(*groups))
    return hosts


def _expand_idx(spec: str) -> List[str]:
    """Expand a GPU index spec like '0-1,3' into ['0','1','3']."""
    out: List[str] = []
    for r in spec.split(","):
        r = r.strip()
        if not r or r.upper() == "N/A":
            continue
        a, sep, b = r.partition("-")
        if sep and a.isdigit() and b.isdigit() and int(a) <= int(b):
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
        r"(?:^|,)(?:gres/)?gpu(?::[^:,()]+)?:(\d+)(?=\(|,|$)", gres,
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
        m_id = re.match(r"JobId=(\d+)\b", line)
        if not m_id:
            continue
        jobid = m_id.group(1)
        m_u = re.search(r"(?:^|\s)UserId=([^(\s]+)\(", line)
        if m_u:
            jobid_user[jobid] = m_u.group(1)
        for m in re.finditer(
            r"(?:^|\s)Nodes=(\S+)\s+CPU_IDs=\S+\s+Mem=\S+\s+GRES=(\S+)",
            line,
        ):
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


JOB_DETAIL_MAX_CHARS = 16 * 1024

# The collector snapshot is mode 0644. Keep root-only/free-form/path-bearing
# fields (AdminComment, Comment, Extra, MailUser, Command, WorkDir, Std*) out
# of it and expose only operational scheduler state already useful in the UI.
_PUBLIC_JOB_DETAIL_FIELDS = frozenset({
    "JobId", "ArrayJobId", "ArrayTaskId", "JobName", "UserId",
    "Priority", "Nice", "Account", "QOS", "JobState", "Reason",
    "Dependency", "Requeue", "Restarts", "BatchFlag", "ExitCode",
    "RunTime", "TimeLimit", "TimeMin", "SubmitTime", "EligibleTime",
    "AccrueTime", "StartTime", "EndTime", "Deadline", "SuspendTime",
    "SecsPreSuspend", "LastSchedEval", "Scheduler", "Partition",
    "ReqNodeList", "ExcNodeList", "NodeList", "BatchHost", "NumNodes",
    "NumCPUs", "NumTasks", "CPUs/Task", "ReqB:S:C:T", "ReqTRES",
    "AllocTRES", "Socks/Node", "NtasksPerN:B:S:C", "CoreSpec",
    "MinCPUsNode", "MinMemoryNode", "MinTmpDiskNode", "Features",
    "DelayBoot", "OverSubscribe", "Contiguous", "Licenses", "Network",
    "Power", "TresPerNode", "TresPerTask", "TresPerSocket", "TresPerJob",
})

JobLogMetadata = Tuple[str, str, bool]


def _json_number(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, dict):
        number = value.get("number")
        if isinstance(number, int) and not isinstance(number, bool):
            return number
    return None


def _json_job_ids(record: dict) -> List[str]:
    job_id = _json_number(record.get("job_id"))
    if job_id is None or job_id <= 0:
        return []
    ids = [str(job_id)]
    array_id = _json_number(record.get("array_job_id")) or 0
    task_id = record.get("array_task_id")
    task_number = _json_number(task_id)
    task_is_set = isinstance(task_id, dict) and task_id.get("set") is True
    task_string = record.get("array_task_string")
    if array_id > 0 and task_is_set and task_number is not None:
        ids.append(f"{array_id}_{task_number}")
    elif array_id > 0 and isinstance(task_string, str) and task_string:
        candidate = f"{array_id}_[{task_string}]"
        if valid_slurm_job_id(candidate):
            ids.append(candidate)
    het_id = _json_number(record.get("het_job_id")) or 0
    het_offset = _json_number(record.get("het_job_offset"))
    if het_id > 0 and het_offset is not None:
        ids.append(f"{het_id}+{het_offset}")
    return list(dict.fromkeys(ids))


def _public_json_scalar(value: object, limit: int = 4096) -> str:
    """One display-safe line from a structured Slurm JSON scalar."""
    if isinstance(value, bool):
        text = "Yes" if value else "No"
    elif isinstance(value, (str, int, float)) and not isinstance(value, bool):
        text = str(value)
    elif isinstance(value, list):
        text = ",".join(
            part for item in value
            if (part := _public_json_scalar(item, limit))
        )
    else:
        return ""
    # JobName and several other strings are submitter-controlled. Keep each
    # value on one line so it cannot impersonate a neighboring detail field.
    return " ".join(text.split())[:limit]


def _json_wrapped_scalar(value: object) -> str:
    if not isinstance(value, dict):
        return _public_json_scalar(value)
    if value.get("infinite") is True:
        return "UNLIMITED"
    if value.get("set") is False:
        return ""
    return _public_json_scalar(value.get("number"))


def _json_timestamp(value: object) -> str:
    number = _json_number(value)
    if not isinstance(value, dict) or value.get("set") is False \
            or number is None or number <= 0:
        return ""
    try:
        return time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(number))
    except (OverflowError, OSError, ValueError):
        return ""


def _json_duration_minutes(value: object) -> str:
    if isinstance(value, dict) and value.get("infinite") is True:
        return "UNLIMITED"
    minutes = _json_number(value)
    if minutes is None or minutes < 0:
        return ""
    seconds = minutes * 60
    days, seconds = divmod(seconds, 86400)
    hours, seconds = divmod(seconds, 3600)
    mins, seconds = divmod(seconds, 60)
    prefix = f"{days}-" if days else ""
    return f"{prefix}{hours:02d}:{mins:02d}:{seconds:02d}"


def _public_json_job_detail(record: dict, ids: List[str]) -> str:
    """Render an allowlisted scheduler detail from structured JSON."""
    if not ids:
        return ""
    uid = _json_number(record.get("user_id"))
    user = _public_json_scalar(record.get("user_name"))
    user_id = f"{user}({uid})" if user and uid is not None else user
    state = _public_json_scalar(record.get("job_state"))
    array_id = _json_number(record.get("array_job_id")) or 0
    array_task = ""
    if array_id > 0:
        array_task = _json_wrapped_scalar(record.get("array_task_id")) \
            or _public_json_scalar(record.get("array_task_string"))
    fields_out = [
        ("JobId", ids[0]),
        ("ArrayJobId", str(array_id) if array_id > 0 else ""),
        ("ArrayTaskId", array_task),
        ("JobName", _public_json_scalar(record.get("name"))),
        ("UserId", user_id),
        ("Priority", _json_wrapped_scalar(record.get("priority"))),
        ("Nice", _public_json_scalar(record.get("nice"))),
        ("Account", _public_json_scalar(record.get("account"))),
        ("QOS", _public_json_scalar(record.get("qos"))),
        ("JobState", state),
        ("Reason", _public_json_scalar(record.get("state_reason"))),
        ("Dependency", _public_json_scalar(record.get("dependency"))),
        ("Requeue", _public_json_scalar(record.get("requeue"))),
        ("Restarts", _public_json_scalar(record.get("restart_cnt"))),
        ("BatchFlag", _public_json_scalar(record.get("batch_flag"))),
        ("TimeLimit", _json_duration_minutes(record.get("time_limit"))),
        ("SubmitTime", _json_timestamp(record.get("submit_time"))),
        ("EligibleTime", _json_timestamp(record.get("eligible_time"))),
        ("StartTime", _json_timestamp(record.get("start_time"))),
        ("EndTime", _json_timestamp(record.get("end_time"))),
        ("Deadline", _json_timestamp(record.get("deadline"))),
        ("SuspendTime", _json_timestamp(record.get("suspend_time"))),
        ("LastSchedEval", _json_timestamp(record.get("last_sched_evaluation"))),
        ("Partition", _public_json_scalar(record.get("partition"))),
        ("ReqNodeList", _public_json_scalar(record.get("required_nodes"))),
        ("ExcNodeList", _public_json_scalar(record.get("excluded_nodes"))),
        ("NodeList", _public_json_scalar(record.get("nodes"))),
        ("BatchHost", _public_json_scalar(record.get("batch_host"))),
        ("NumNodes", _json_wrapped_scalar(record.get("node_count"))),
        ("NumCPUs", _json_wrapped_scalar(record.get("cpus"))),
        ("NumTasks", _json_wrapped_scalar(record.get("tasks"))),
        ("CPUs/Task", _json_wrapped_scalar(record.get("cpus_per_task"))),
        ("ReqTRES", _public_json_scalar(record.get("tres_req_str"))),
        ("AllocTRES", _public_json_scalar(record.get("tres_alloc_str"))),
        ("MinCPUsNode", _json_wrapped_scalar(
            record.get("minimum_cpus_per_node"))),
        ("MinMemoryNode", _json_wrapped_scalar(record.get("memory_per_node"))),
        ("MinTmpDiskNode", _json_wrapped_scalar(
            record.get("minimum_tmp_disk_per_node"))),
        ("Features", _public_json_scalar(record.get("features"))),
        ("OverSubscribe", _public_json_scalar(record.get("oversubscribe"))),
        ("Contiguous", _public_json_scalar(record.get("contiguous"))),
        ("Licenses", _public_json_scalar(record.get("licenses"))),
        ("Network", _public_json_scalar(record.get("network"))),
        ("TresPerNode", _public_json_scalar(record.get("tres_per_node"))),
        ("TresPerTask", _public_json_scalar(record.get("tres_per_task"))),
        ("TresPerSocket", _public_json_scalar(record.get("tres_per_socket"))),
        ("TresPerJob", _public_json_scalar(record.get("tres_per_job"))),
    ]
    return "\n".join(f"{key}={value}" for key, value in fields_out if value)[
        :JOB_DETAIL_MAX_CHARS
    ]


def _json_job_log_spec(record: dict) -> JobLogMetadata:
    workdir = record.get("current_working_directory")
    workdir = workdir if isinstance(workdir, str) else ""

    def one(name: str) -> str:
        value = record.get(name)
        if not isinstance(value, str) or not value or value == "(null)":
            return ""
        if not os.path.isabs(value) and workdir:
            value = os.path.join(workdir, value)
        return os.path.normpath(value)

    stdout_path = one("standard_output")
    stderr_path = one("standard_error")
    merged = bool(stdout_path and stderr_path and stdout_path == stderr_path)
    return stdout_path, "" if merged else stderr_path, merged


def _gpu_indices(gres: object) -> List[str]:
    if not isinstance(gres, str):
        return []
    groups = re.findall(r"(?:^|,)gpu(?::[^,()]*)?\(IDX:([^)]+)\)", gres)
    return [idx for group in groups for idx in _expand_idx(group)]


def parse_job_json(
    out: str,
) -> Tuple[Dict[str, Dict[str, str]], Dict[str, str], Dict[str, int],
           Dict[str, str], Dict[str, JobLogMetadata]]:
    """Parse structured ``scontrol --json show jobs`` output.

    Unlike the legacy one-line formatter, JSON keeps embedded newlines inside
    their owning string and therefore cannot forge a second job record.
    """
    payload = json.loads(out)
    records = payload.get("jobs") if isinstance(payload, dict) else None
    if not isinstance(records, list):
        raise ValueError("scontrol JSON has no jobs list")
    alloc: Dict[str, Dict[str, str]] = {}
    users: Dict[str, str] = {}
    owner_uids: Dict[str, int] = {}
    details: Dict[str, str] = {}
    metadata: Dict[str, JobLogMetadata] = {}
    for record in records:
        if not isinstance(record, dict):
            continue
        ids = _json_job_ids(record)
        if not ids:
            continue
        detail = _public_json_job_detail(record, ids)
        log_spec = _json_job_log_spec(record)
        for jobid in ids:
            details[jobid] = detail
            metadata[jobid] = log_spec
        user = _public_json_scalar(record.get("user_name"))
        owner_uid = _json_number(record.get("user_id"))
        for jobid in ids:
            if user:
                users[jobid] = user
            if owner_uid is not None and owner_uid >= 0:
                owner_uids[jobid] = owner_uid

        states = record.get("job_state")
        state_set = {
            str(item).upper() for item in states
        } if isinstance(states, list) else {str(states).upper()}
        if not state_set.intersection({"RUNNING", "COMPLETING"}):
            continue
        jobid = ids[0]
        resources = record.get("job_resources")
        allocated_nodes = resources.get("allocated_nodes") \
            if isinstance(resources, dict) else None
        node_names = []
        if isinstance(allocated_nodes, list):
            for node in allocated_nodes:
                name = node.get("nodename") if isinstance(node, dict) else ""
                if isinstance(name, str) and 0 < len(name) <= 255 \
                        and re.fullmatch(r"[A-Za-z0-9_.-]+", name):
                    node_names.append(name)
        gres_details = record.get("gres_detail")
        if not isinstance(gres_details, list):
            continue
        # Slurm's gres_detail array follows the job allocation's node order.
        # On any cardinality ambiguity, fail closed rather than attributing a
        # GPU to the wrong node/user.
        if len(node_names) != len(gres_details):
            continue
        for node, gres in zip(node_names, gres_details, strict=True):
            for index in _gpu_indices(gres):
                alloc.setdefault(node, {})[index] = jobid
    return alloc, users, owner_uids, details, metadata


_LEGACY_FIELD_RE = re.compile(
    r"(?:^|[ \t]+)([A-Za-z][A-Za-z0-9_/:.-]*)="
)
_LEGACY_IDENTITY_FIELDS = frozenset({
    "JobId", "ArrayJobId", "ArrayTaskId", "HetJobId", "HetJobOffset",
    "UserId", "JobState", "NodeList",
})
_LEGACY_PATH_FIELDS = frozenset({"WorkDir", "StdOut", "StdErr"})


def _legacy_field_items(line: str) -> List[tuple[str, str]]:
    matches = list(_LEGACY_FIELD_RE.finditer(line))
    return [
        (
            match.group(1),
            line[match.end():matches[index + 1].start()].rstrip(),
        )
        for index, match in enumerate(matches)
        if index + 1 < len(matches)
    ] + ([
        (matches[-1].group(1), line[matches[-1].end():].rstrip())
    ] if matches else [])


def _legacy_one(
    grouped: Dict[str, List[str]], name: str, required: bool = False,
) -> str | None:
    values = grouped.get(name, [])
    if len(values) > 1 or (required and len(values) != 1):
        return None
    return values[0] if values else ""


def _legacy_safe_path(value: str, workdir: str = "") -> str | None:
    if not value or value == "(null)":
        return ""
    # Oneline output has no quoting contract that lets us distinguish spaces
    # inside a path from the next key. Reject ambiguous paths in legacy mode.
    if len(value) > 4096 or any(ch.isspace() or ord(ch) < 32 for ch in value):
        return None
    if not os.path.isabs(value):
        if not workdir:
            return None
        value = os.path.join(workdir, value)
    normalized = os.path.normpath(value)
    if not os.path.isabs(normalized) or len(normalized) > 4096:
        return None
    return normalized


def _strict_gpu_indices(gres: str) -> List[str] | None:
    groups = re.findall(r"(?:^|,)gpu(?::[^,()]*)?\(IDX:([^)]+)\)", gres)
    if gres.count("IDX:") != len(groups):
        return None
    indices: List[str] = []
    for group in groups:
        if group.upper() == "N/A":
            continue
        values = _strict_range_values(
            group, _LEGACY_MAX_GPU_INDICES - len(indices),
        )
        if values is None:
            return None
        for value in values:
            if int(value) > _LEGACY_MAX_GPU_INDEX:
                return None
        indices.extend(str(int(value)) for value in values)
    return indices


def _legacy_gpu_claims(
    line: str, anchor_nodes: tuple[str, ...], raw_jobid: str,
) -> List[tuple[str, str, str]] | None:
    matches = list(re.finditer(
        r"(?:^|\s)Nodes=(\S+)\s+CPU_IDs=\S+\s+Mem=\S+\s+GRES=(\S+)",
        line,
    ))
    if not matches:
        return []
    expected = set(anchor_nodes)
    seen_nodes: set[str] = set()
    claims: List[tuple[str, str, str]] = []
    for match in matches:
        nodes = _strict_expand_nodes(match.group(1))
        indices = _strict_gpu_indices(match.group(2))
        if nodes is None or indices is None or not set(nodes) <= expected \
                or seen_nodes.intersection(nodes):
            return None
        seen_nodes.update(nodes)
        claims.extend(
            (node, index, raw_jobid) for node in nodes for index in indices
        )
    # A forged segment cannot supplement or replace the scheduler's real node
    # segment: the validated segments must cover the stable queue placement.
    if seen_nodes != expected:
        return None
    return claims


def _legacy_public_detail(items: List[tuple[str, str]]) -> str:
    return "\n".join(
        f"{key}={_public_json_scalar(value)}"
        for key, value in items
        if key in _PUBLIC_JOB_DETAIL_FIELDS and value and value != "(null)"
    )[:JOB_DETAIL_MAX_CHARS]


def _parse_legacy_jobs(
    out: str,
    before: Dict[str, _QueueAnchor],
    after: Dict[str, _QueueAnchor],
) -> Tuple[
    Dict[str, Dict[str, str]], Dict[str, str], Dict[str, int],
    Dict[str, str], Dict[str, JobLogMetadata], set[str],
]:
    """Parse old ``scontrol -o`` only after bookend queue authorization.

    Slurm 19.05 prints submitter-controlled strings verbatim, including
    newlines. Every physical JobId claim is counted before content validation;
    a duplicate, owner/placement race, ambiguous field, or unsafe path rejects
    that whole identity. The caller supplies two numeric-UID queue rosters,
    captured immediately before and after this output.
    """
    if len(out) > _LEGACY_MAX_OUTPUT_CHARS:
        raise ValueError("legacy scontrol output exceeds safety limit")
    candidates: List[tuple[str, tuple[str, ...]]] = []
    claim_counts: Counter[str] = Counter()
    for line in out.splitlines():
        match = re.match(r"JobId=(\d+)\b", line)
        if not match:
            continue
        ids = tuple(dict.fromkeys(
            jobid for jobid in _job_record_ids(line)
            if valid_slurm_job_id(jobid)
        ))
        if not ids:
            continue
        candidates.append((line, ids))
        claim_counts.update(ids)

    stable = {
        jobid: anchor for jobid, anchor in before.items()
        if after.get(jobid) == anchor
    }
    known = set(before) | set(after)
    users: Dict[str, str] = {}
    owner_uids: Dict[str, int] = {}
    details: Dict[str, str] = {}
    metadata: Dict[str, JobLogMetadata] = {}
    rejected: set[str] = set()
    gpu_claims: Dict[tuple[str, str], List[tuple[str, tuple[str, ...]]]] = (
        defaultdict(list)
    )

    for line, ids in candidates:
        relevant = set(ids) & known
        if not relevant:
            continue
        if any(claim_counts[jobid] != 1 for jobid in ids):
            rejected.update(ids)
            continue
        anchored = [(jobid, stable[jobid]) for jobid in ids if jobid in stable]
        if not anchored or any(anchor != anchored[0][1] for _, anchor in anchored):
            rejected.update(ids)
            continue
        anchor = anchored[0][1]
        if len(line) > _LEGACY_MAX_LINE_CHARS \
                or any(ord(ch) < 32 and ch != "\t" for ch in line):
            rejected.update(ids)
            continue

        items = _legacy_field_items(line)
        grouped: Dict[str, List[str]] = defaultdict(list)
        for key, value in items:
            grouped[key].append(value)
        unique_fields = _LEGACY_IDENTITY_FIELDS | _LEGACY_PATH_FIELDS \
            | _PUBLIC_JOB_DETAIL_FIELDS
        if any(len(grouped.get(key, [])) > 1 for key in unique_fields):
            rejected.update(ids)
            continue
        raw_jobid = _legacy_one(grouped, "JobId", required=True)
        text_owner = _legacy_one(grouped, "UserId", required=True)
        state = _legacy_one(grouped, "JobState", required=True)
        node_expr = _legacy_one(grouped, "NodeList")
        if None in (raw_jobid, text_owner, state, node_expr) \
                or raw_jobid != ids[0]:
            rejected.update(ids)
            continue
        owner_match = re.fullmatch(
            r"([A-Za-z0-9_.@-]{1,256})\(([0-9]{1,10})\)", text_owner,
        )
        nodes = _strict_expand_nodes(node_expr)
        if owner_match is None or nodes is None \
                or owner_match.group(1) != anchor.user \
                or int(owner_match.group(2)) != anchor.uid \
                or state != anchor.state or nodes != anchor.nodes:
            rejected.update(ids)
            continue

        workdir_value = _legacy_one(grouped, "WorkDir")
        stdout_value = _legacy_one(grouped, "StdOut")
        stderr_value = _legacy_one(grouped, "StdErr")
        if None in (workdir_value, stdout_value, stderr_value):
            rejected.update(ids)
            continue
        workdir = _legacy_safe_path(workdir_value or "")
        stdout_path = _legacy_safe_path(stdout_value or "", workdir or "") \
            if workdir is not None else None
        stderr_path = _legacy_safe_path(stderr_value or "", workdir or "") \
            if workdir is not None else None
        if workdir is None or stdout_path is None or stderr_path is None:
            rejected.update(ids)
            continue
        merged = bool(stdout_path and stderr_path and stdout_path == stderr_path)
        log_spec = (stdout_path, "" if merged else stderr_path, merged)
        detail = _legacy_public_detail(items)
        for jobid in ids:
            users[jobid] = anchor.user
            owner_uids[jobid] = anchor.uid
            details[jobid] = detail
            metadata[jobid] = log_spec

        if state in {"RUNNING", "COMPLETING"}:
            claims = _legacy_gpu_claims(line, anchor.nodes, raw_jobid)
            if claims is None:
                rejected.update(ids)
            else:
                for node, index, allocation_jobid in claims:
                    gpu_claims[(node, index)].append((allocation_jobid, ids))

    alloc: Dict[str, Dict[str, str]] = {}
    for (node, index), claims in gpu_claims.items():
        if len(claims) != 1:
            for _jobid, ids in claims:
                rejected.update(ids)
            continue
        alloc.setdefault(node, {})[index] = claims[0][0]
    return alloc, users, owner_uids, details, metadata, rejected


def _job_record_ids(line: str) -> List[str]:
    # `scontrol -o` starts each record with these canonical fields. Anchoring
    # prevents submitter-controlled JobName/comments from forging aliases.
    match = re.match(r"JobId=(\d+)\b", line)
    if not match:
        return []
    ids = [match.group(1)]
    rest = line[match.end():]
    array = re.match(
        r"\s+ArrayJobId=(\d+)\s+ArrayTaskId=([^\s]+)", rest,
    )
    if array:
        task = array.group(2)
        display_task = task if task.isdigit() else f"[{task}]"
        ids.append(f"{array.group(1)}_{display_task}")
    het = re.match(r"\s+HetJobId=(\d+)\s+HetJobOffset=(\d+)", rest)
    if het:
        ids.append(f"{het.group(1)}+{het.group(2)}")
    return ids


def _public_job_detail(line: str) -> str:
    """Sanitize a root ``scontrol -o`` record for the public snapshot."""
    try:
        tokens = shlex.split(line)
    except ValueError:
        tokens = line.split()
    fields_out = []
    seen = set()
    for token in tokens:
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        if key in _PUBLIC_JOB_DETAIL_FIELDS:
            # Canonical scontrol fields occur once. A duplicate can only make
            # the record ambiguous (for example embedded free-form text), so
            # fail closed for that field instead of choosing attacker order.
            if key in seen:
                fields_out = [item for item in fields_out
                              if not item.startswith(f"{key}=")]
                continue
            seen.add(key)
            fields_out.append(f"{key}={value}")
    return "\n".join(fields_out)[:JOB_DETAIL_MAX_CHARS]


def parse_job_records(
    out: str,
) -> Tuple[Dict[str, str], Dict[str, JobLogMetadata]]:
    """Legacy text parser used for compatibility tests and unprivileged views.

    Never use this permissive line-oriented helper as input to privileged log
    sharing: submitter-controlled Slurm strings can contain newlines. The
    collector uses :func:`parse_job_json` or :func:`_parse_legacy_jobs`; the
    latter adds duplicate-claim rejection and two numeric-UID queue anchors.
    """
    details: Dict[str, str] = {}
    log_metadata: Dict[str, JobLogMetadata] = {}
    for raw_line in out.splitlines():
        line = raw_line.strip()
        ids = _job_record_ids(line)
        if not ids:
            continue
        detail = _public_job_detail(line)
        stdout_path, stderr_path, merged = job_log_spec(line)
        metadata = (stdout_path, stderr_path, merged)
        for jobid in ids:
            details[jobid] = detail
            log_metadata[jobid] = metadata
    return details, log_metadata


def parse_job_details(out: str) -> Dict[str, str]:
    """Map display job IDs to sanitized, bounded scheduler detail."""
    details, _log_metadata = parse_job_records(out)
    return details


def _published_detail_field(detail: str, name: str) -> str:
    prefix = f"{name}="
    return next(
        (line[len(prefix):] for line in detail.splitlines()
         if line.startswith(prefix)),
        "",
    )


def collect_gpu_alloc() -> Tuple[
    Dict[str, Dict[str, str]], Dict[str, str], Dict[str, int],
    Dict[str, str], Dict[str, JobLogMetadata], str,
]:
    """GPU allocation plus public detail and private per-job log metadata."""
    ok, out = run_cmd("scontrol --json show jobs")
    if ok:
        try:
            alloc, users, owner_uids, details, log_metadata = parse_job_json(out)
            return alloc, users, owner_uids, details, log_metadata, ""
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            json_error = f"scontrol JSON parse failed: {exc}"
    else:
        json_error = f"scontrol JSON failed: {out}"
    # Text-mode scontrol prints JobName verbatim and cannot safely delimit
    # records. Never let that representation drive privileged file access,
    # allocation attribution, accounting, or public scheduler details.
    return {}, {}, {}, {}, {}, json_error


_job_query_backend = "auto"
_job_query_fallback_reason = ""
_job_query_lock = threading.Lock()


def _json_option_unsupported(message: str) -> bool:
    """Narrowly recognize old scontrol's rejection of the --json option."""
    text = " ".join(str(message).lower().split())
    return (
        ("unrecognized option" in text and "json" in text)
        or ("unknown option" in text and "json" in text)
        # Old getopt reports a leading '-' as the invalid short option when it
        # receives any unsupported GNU-style long option. This classifier is
        # called only for the known `scontrol --json` command.
        or "invalid option -- '-'" in text
    )


def _scheduler_status(
    backend: str, *, fallback_reason: str = "", error: str = "",
    rejected: int = 0,
) -> Dict[str, object]:
    return {
        "job_backend": backend,
        "fallback_reason": fallback_reason,
        "error": error,
        "rejected_records": max(0, int(rejected)),
    }


def _remember_job_backend(backend: str, reason: str = "") -> None:
    global _job_query_backend, _job_query_fallback_reason
    with _job_query_lock:
        _job_query_backend = backend
        _job_query_fallback_reason = reason


def _known_job_backend() -> tuple[str, str]:
    with _job_query_lock:
        return _job_query_backend, _job_query_fallback_reason


def _legacy_scheduler_jobs(
    jobs: List[JobInfo], pending: List[PendingJob],
    before: Dict[str, _QueueAnchor], fallback_reason: str,
) -> tuple:
    ok, out = run_cmd("scontrol -o show job -d")
    if not ok:
        error = f"scontrol legacy failed: {out}"
        return (
            jobs, pending, {}, {}, {}, {}, {},
            _scheduler_status("unavailable", error=error), error,
        )
    _after_jobs, _after_pending, after, after_error = _collect_queue_snapshot()
    if after_error:
        error = f"legacy post-check squeue failed: {after_error}"
        return (
            jobs, pending, {}, {}, {}, {}, {},
            _scheduler_status("unavailable", error=error), error,
        )
    try:
        alloc, users, uids, details, metadata, rejected = _parse_legacy_jobs(
            out, before, after,
        )
    except ValueError as exc:
        error = f"scontrol legacy parse failed: {exc}"
        return (
            jobs, pending, {}, {}, {}, {}, {},
            _scheduler_status("unavailable", error=error), error,
        )
    status = _scheduler_status(
        "legacy-text", fallback_reason=fallback_reason,
        rejected=len(rejected),
    )
    return jobs, pending, alloc, users, uids, details, metadata, status, ""


def _collect_scheduler_jobs(json_future=None) -> tuple:
    """Queue plus one safe job-detail backend selected by capability.

    The legacy backend deliberately bookends its single text scontrol RPC with
    numeric-UID queue snapshots. Modern Slurm keeps the faster structured JSON
    path. Only a definite unsupported-option response is cached as legacy;
    timeouts, permission errors, and malformed JSON remain fail-closed.
    """
    jobs, pending, before, queue_error = _collect_queue_snapshot()
    if queue_error:
        # The modern query may have been started in parallel. Observe it so
        # executor exceptions never become detached, but scheduler identity
        # still fails closed when the queue anchor is unavailable.
        if json_future is not None:
            try:
                json_future.result()
            except Exception:
                pass
        error = (
            f"squeue failed: {queue_error} | "
            f"squeue PD failed: {queue_error}"
        )
        return (
            jobs, pending, {}, {}, {}, {}, {},
            _scheduler_status("unavailable", error=error), error,
        )
    backend, fallback_reason = _known_job_backend()
    if backend == "legacy-text":
        return _legacy_scheduler_jobs(
            jobs, pending, before,
            fallback_reason or "scontrol --json unsupported",
        )

    json_result = json_future.result() if json_future is not None \
        else collect_gpu_alloc()
    alloc, users, uids, details, metadata, json_error = json_result
    if not json_error:
        _remember_job_backend("structured-json")
        return (
            jobs, pending, alloc, users, uids, details, metadata,
            _scheduler_status("structured-json"), "",
        )
    if _json_option_unsupported(json_error):
        fallback_reason = "scontrol --json unsupported"
        _remember_job_backend("legacy-text", fallback_reason)
        return _legacy_scheduler_jobs(
            jobs, pending, before, fallback_reason,
        )
    return (
        jobs, pending, {}, {}, {}, {}, {},
        _scheduler_status("unavailable", error=json_error), json_error,
    )


# Combined node-side payload commands: nvidia-smi metrics + pmon (PID→GPU)
# + meminfo + ps (PID→user). SSH pull uses the full command. The resident
# agent normally uses the dynamic form and periodically refreshes the static
# PCI minor/slot sections.
_NODE_PAYLOAD_PREFIX = (
    # NOTE: pci.bus_id must stay at index 9 (minor mapping reads p[9]); new
    # columns append after. Consumer GPUs report ecc/serial as [N/A].
    "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
    "temperature.gpu,power.draw,power.limit,pci.bus_id,"
    "ecc.errors.uncorrected.aggregate.total,serial,clocks.sm,clocks.mem "
    "--format=csv,noheader,nounits 2>/dev/null; "
    "echo '---SEP---'; "
    # One pmon sample supplies both the payload section and the PID list.
    # Running pmon again doubled its process/driver-query cost on every node.
    "PMON=$(nvidia-smi pmon -c 1 -s m 2>/dev/null); "
    "printf '%s\n' \"$PMON\"; "
    "echo '---SEP---'; "
    "awk '/^MemTotal:/{ t=$2 } /^MemAvailable:/{ a=$2 } "
    "END{ printf \"%d %d %d\", t/1024, (t-a)/1024, a/1024 }' /proc/meminfo; "
    "echo '---SEP---'; "
    "PIDS=$(printf '%s\n' \"$PMON\" | awk 'NR>2 && $2!= \"-\" {print $2}' | tr '\\n' ','); "
    "if [ -n \"$PIDS\" ]; then ps -p ${PIDS%,} -o pid=,user= 2>/dev/null; fi; "
    "echo '---SEP---'; "
)

_NODE_CGROUP_CMD = (
    # PID -> SLURM jobid from the process's cgroup path (job_<id> under the
    # slurmstepd scope). One grep handles all PIDs instead of one fork per PID.
    "if [ -n \"$PIDS\" ]; then set --; "
    "for p in $(printf '%s\\n' \"${PIDS%,}\" | tr ',' ' '); do "
    "set -- \"$@\" \"/proc/$p/cgroup\"; done; "
    "grep -H -m1 -oE 'job_[0-9]+' \"$@\" 2>/dev/null | "
    "sed -E 's#^/proc/([0-9]+)/cgroup:job_#\\1 #'; fi; "
)

NODE_DYNAMIC_PAYLOAD_CMD = (
    _NODE_PAYLOAD_PREFIX
    # Empty minor section; the agent merges its cached topology after parse.
    + "echo '---SEP---'; "
    + _NODE_CGROUP_CMD
    # Empty slot section.
    + "echo '---SEP---'; true"
)

_NODE_MINOR_CMD = (
    # PCI bus -> /dev/nvidiaN minor. SLURM's GRES IDX means the minor, and
    # minor order can differ from nvidia-smi (PCI) order on some boards.
    "for d in /proc/driver/nvidia/gpus/*/information; do "
    "awk '/Bus Location/{b=$NF} /Device Minor/{m=$NF} END{print b, m}' \"$d\" 2>/dev/null; "
    "done; "
)

_NODE_SLOT_CMD = (
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

NODE_PAYLOAD_CMD = (
    _NODE_PAYLOAD_PREFIX
    + _NODE_MINOR_CMD
    + "echo '---SEP---'; "
    + _NODE_CGROUP_CMD
    + "echo '---SEP---'; "
    + _NODE_SLOT_CMD
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


_basic_executor = ThreadPoolExecutor(max_workers=4, thread_name_prefix="sgpu-basic")


def cleanup_basic_executor() -> None:
    """Release the long-lived local Slurm command pool at process exit."""
    _basic_executor.shutdown(wait=True, cancel_futures=True)


atexit.register(cleanup_basic_executor)


def collect_basic() -> Tuple[
    List[dict], List[JobInfo], List[PendingJob], Dict[str, List[JobInfo]],
    Dict[str, Dict[str, str]], Dict[str, str], Dict[str, object], str,
]:
    """Phase 1: fast local commands only (sinfo + squeue + scontrol)."""
    f_nodes = _basic_executor.submit(collect_nodes_basic)
    backend, _fallback_reason = _known_job_backend()
    # Keep modern Slurm's squeue and structured scontrol RPCs parallel. The
    # legacy path cannot overlap its text query with the queue anchors, but a
    # one-time capability probe can still run beside the first pre-check.
    f_json = _basic_executor.submit(collect_gpu_alloc) \
        if backend != "legacy-text" else None
    f_scheduler = _basic_executor.submit(_collect_scheduler_jobs, f_json)
    f_mem = _basic_executor.submit(collect_mem_alloc)
    nodes_raw, e1 = f_nodes.result()
    (
        jobs, pending, gpu_alloc, alloc_user_map, owner_uids, job_details,
        job_log_metadata, scheduler_status, scheduler_error,
    ) = f_scheduler.result()
    mem_alloc, e5 = f_mem.result()
    for n in nodes_raw:
        n["mem_alloc"] = mem_alloc.get(n["name"], "")
    for job in jobs:
        job.detail = job_details.get(job.jobid, "")
        job.jobname = _published_detail_field(job.detail, "JobName")
        trusted_user = alloc_user_map.get(job.jobid, "")
        if trusted_user:
            job.user = trusted_user
        job.uid = owner_uids.get(job.jobid, -1)
        log_out, log_err, merged = job_log_metadata.get(
            job.jobid, ("", "", False),
        )
        # Collector-only attributes: dataclasses.asdict deliberately cannot
        # serialize these into the world-readable snapshot.
        job._log_paths = (log_out, log_err)
        job._log_stderr_merged = merged
    for job in pending:
        # Pending array ranges use e.g. 51317_[0-159%16] in squeue while
        # scontrol keeps the parent record as JobId=51317.
        job.detail = job_details.get(
            job.jobid, job_details.get(job.jobid.split("_", 1)[0], ""),
        )
        job.jobname = _published_detail_field(job.detail, "JobName")
        trusted_user = alloc_user_map.get(
            job.jobid, alloc_user_map.get(job.jobid.split("_", 1)[0], ""),
        )
        if trusted_user:
            job.user = trusted_user
    err = " | ".join(x for x in [e1, scheduler_error, e5] if x)
    return (
        nodes_raw, jobs, pending, assign_node_jobs(jobs), gpu_alloc,
        alloc_user_map, scheduler_status, err,
    )


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
        for g, (jid, user) in zip(node.gpus, pairs, strict=True):
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
        stale = name in stale_nodes
        result.append(NodeInfo(
            name=name, state=n["state"], partition=n.get("partition", ""),
            # keep the direct-collection path's shape identical to the
            # collector's, so `sgpu --json` emits one schema either way
            source="stale" if stale else "ssh",
            has_gpu=n.get("has_gpu", True),
            cpus=n["cpus"],
            cpu_alloc=n.get("cpu_alloc", ""), cpu_load=n["cpu_load"],
            mem_total=n["mem_total"], mem_free=n["mem_free"],
            mem_alloc=n.get("mem_alloc", ""), gres=n["gres"],
            gpus=gpus, jobs=node_jobs.get(name, []), error=gerr,
            mem_used=mem.used, mem_avail=mem.avail,
            stale=stale,
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


# ── Job log files (stdout/stderr) ─────────────────────────────────────────

def tail_file(path: str, limit: int = 65536) -> str:
    """Last `limit` bytes of a job log, decoded leniently."""
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as f:
            if size > limit:
                f.seek(size - limit)
            text = f.read(limit).decode(errors="replace")
    except FileNotFoundError:
        return "(no file yet — job may not have started writing)"
    except OSError as e:
        return f"(not readable: {e.strerror or e})"
    if not text:
        return "(empty)"
    if size > limit:
        text = f"… showing last {limit // 1024}KB of {size / 1048576:.1f}MB …\n" + text
    return text


def read_job_log(path: str, shared: str = "", limit: int = 65536) -> Tuple[str, str]:
    """Tail a job log, preferring the real file. Returns (text, path read).

    The owner reads their own file directly, so they always get the live,
    complete thing. Everyone else usually cannot — job logs land in the
    submitter's home, behind a private directory or mode 0600 — and falls back
    to the collector's world-readable mirror, which only exists when the site
    turned SHARE_LOGS on.
    """
    if path and os.access(path, os.R_OK):
        return tail_file(path, limit), path
    if shared and os.access(shared, os.R_OK):
        return tail_file(shared, limit), shared
    if path:
        return tail_file(path, limit), path  # surface the real error
    return "", ""


def job_log_spec(scontrol_out: str) -> Tuple[str, str, bool]:
    """Return stdout, distinct stderr, and whether stderr is merged."""
    path_boundaries = (
        "Command", "WorkDir", "StdErr", "StdIn", "StdOut", "Power",
        "CpusPerTres", "TresPerNode", "TresPerTask", "TresPerSocket",
        "TresPerJob",
    )

    def one(field: str) -> str:
        boundary = "|".join(re.escape(name) for name in path_boundaries)
        matches = list(re.finditer(
            rf"(?:^|[ \t]){field}=(.*?)"
            rf"(?=[ \t]+(?:{boundary})=|$)",
            scontrol_out, re.M,
        ))
        # Canonical path fields are near the end of the record. Choosing the
        # last occurrence avoids earlier free-form fields spoofing a key.
        p = matches[-1].group(1).rstrip() if matches else ""
        if not p or p == "(null)":
            return ""
        if not os.path.isabs(p):
            wd = one("WorkDir")
            p = os.path.join(wd, p) if wd else p
        return os.path.normpath(p)

    stdout_path = one("StdOut")
    stderr_path = one("StdErr")
    merged = bool(stdout_path and stderr_path and stderr_path == stdout_path)
    return stdout_path, "" if merged else stderr_path, merged


def job_log_paths(scontrol_out: str) -> Tuple[str, str]:
    """(stdout, stderr) paths from `scontrol show job` output.

    scontrol reports resolved paths (%j etc. already expanded); relative
    paths are relative to WorkDir. stderr is "" when merged into stdout."""
    stdout_path, stderr_path, _merged = job_log_spec(scontrol_out)
    return stdout_path, stderr_path
