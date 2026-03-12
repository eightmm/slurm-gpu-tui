#!/usr/bin/env python3
from __future__ import annotations

import atexit
import json
import os
import re
import shlex
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rich.text import Text
from textual import work
from textual.app import App, ComposeResult
from textual.timer import Timer
from textual.widgets import DataTable, Footer, Header, Static


# ── shell helpers ──────────────────────────────────────────────────────────

def run_cmd(cmd: str, timeout: int = 12) -> Tuple[bool, str]:
    try:
        out = subprocess.check_output(
            shlex.split(cmd), stderr=subprocess.STDOUT, timeout=timeout, text=True
        )
        return True, out.strip()
    except Exception as e:
        return False, str(e)


# ── SSH ControlMaster pool ────────────────────────────────────────────────
# Keeps persistent SSH connections to avoid 5s+ handshake per call.

_SSH_CONTROL_DIR = tempfile.mkdtemp(prefix="slurm-tui-ssh-")
_SSH_BASE_OPTS = (
    f"-o ControlPath={_SSH_CONTROL_DIR}/%r@%h "
    "-o StrictHostKeyChecking=no -o BatchMode=yes"
)


def _cleanup_ssh_pool() -> None:
    """Kill all ControlMaster connections on exit."""
    import shutil
    # Just nuke the socket dir — ControlPersist masters detect dead sockets and exit
    shutil.rmtree(_SSH_CONTROL_DIR, ignore_errors=True)


atexit.register(_cleanup_ssh_pool)


def ssh_ensure_master(node: str) -> None:
    """Start a ControlMaster connection to a node if not already running."""
    sock = f"{_SSH_CONTROL_DIR}/{os.getenv('USER', 'user')}@{node}"
    if os.path.exists(sock):
        return
    # Start background master (-fN = fork to background, no command)
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


# ── data models ────────────────────────────────────────────────────────────

@dataclass
class GpuInfo:
    index: str
    name: str
    util: str       # %
    mem_used: str   # MiB
    mem_total: str  # MiB
    temp: str       # C
    power: str      # W
    power_cap: str  # W
    pids: List[str]
    users: List[str]  # resolved from PID via ps


@dataclass
class JobInfo:
    jobid: str
    user: str
    partition: str
    jobname: str
    elapsed: str
    node: str
    gpu_count: int
    gres_raw: str
    time_limit: str = ""


@dataclass
class PendingJob:
    jobid: str
    user: str
    partition: str
    jobname: str
    time_limit: str
    gpu_count: int
    reason: str
    priority: str


@dataclass
class NodeInfo:
    name: str
    state: str
    cpus: str        # total cores
    cpu_alloc: str   # SLURM allocated cores
    cpu_load: str    # load average
    mem_total: str   # MB (from sinfo)
    mem_free: str    # MB (from sinfo - unreliable, use mem_avail if available)
    gres: str
    gpus: List[GpuInfo]
    jobs: List[JobInfo]
    error: str
    mem_used: str = ""   # MB (from /proc/meminfo via SSH)
    mem_avail: str = ""  # MB (from /proc/meminfo via SSH)
    stale: bool = False


# ── data collection ────────────────────────────────────────────────────────

def collect_jobs() -> Tuple[List[JobInfo], str]:
    cmd = 'squeue -h -t R -o "%i|%u|%P|%j|%M|%R|%b|%l"'
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
    # %C = A/I/O/T (Allocated/Idle/Other/Total CPUs)
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
        # Parse %C: "A/I/O/T"
        cpus_aiot = p[7].strip()
        cpu_alloc = ""
        parts = cpus_aiot.split("/")
        if len(parts) >= 4:
            cpu_alloc = parts[0]  # allocated
        rows.append({
            "name": name, "state": p[1].strip(), "cpus": p[2].strip(),
            "cpu_load": p[3].strip(), "mem_total": p[4].strip(),
            "mem_free": p[5].strip(), "gres": p[6].strip(),
            "cpu_alloc": cpu_alloc,
        })
    return rows, ""


def _has_gpu(gres: str) -> bool:
    if not gres or gres == "(null)":
        return False
    return "gpu" in gres.lower()


def _shorten_gpu_name(name: str) -> str:
    """Shorten verbose GPU names for display."""
    name = name.replace("NVIDIA ", "").replace("GeForce ", "")
    name = name.replace(" Generation", "").replace(" Workstation Edition", "")
    name = name.replace(" Max-Q", "").replace(" PCIe", "")
    name = name.replace("RTX PRO 6000 Blackwell", "RTX PRO 6000")
    return name.strip()


@dataclass
class NodeMemInfo:
    total: str = ""   # MB
    used: str = ""    # MB
    avail: str = ""   # MB
    cpu_usage: str = ""  # % (from /proc/stat snapshot)


def _collect_node_data(node: str, timeout: int = 30) -> Tuple[List[GpuInfo], NodeMemInfo, str]:
    # Single nvidia-smi call for GPU metrics + meminfo (user info comes from SLURM squeue)
    combined = (
        "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,"
        "temperature.gpu,power.draw,power.limit --format=csv,noheader,nounits 2>/dev/null; "
        "echo '---SEP---'; "
        "awk '/^MemTotal:/{ t=$2 } /^MemAvailable:/{ a=$2 } END{ printf \"%d %d %d\", t/1024, (t-a)/1024, a/1024 }' /proc/meminfo"
    )
    ok, out = ssh_cmd(node, combined, timeout=timeout)
    if not ok:
        return [], NodeMemInfo(), "ssh failed"

    sections = out.split("---SEP---")
    metrics_raw = sections[0].strip() if len(sections) > 0 else ""
    mem_raw = sections[1].strip() if len(sections) > 1 else ""

    # Parse memory
    mem_info = NodeMemInfo()
    mem_parts = mem_raw.split()
    if len(mem_parts) >= 3:
        mem_info = NodeMemInfo(total=mem_parts[0], used=mem_parts[1], avail=mem_parts[2])

    gpus: List[GpuInfo] = []
    for line in metrics_raw.splitlines():
        p = [x.strip() for x in line.split(",")]
        if len(p) < 9:
            continue
        gpus.append(GpuInfo(
            index=p[0], name=_shorten_gpu_name(p[2]), util=p[3], mem_used=p[4], mem_total=p[5],
            temp=p[6], power=p[7], power_cap=p[8], pids=[], users=[],
        ))
    return gpus, mem_info, ""


def warmup_ssh(nodes: List[str], max_workers: int = 8) -> None:
    """Pre-establish SSH ControlMaster connections in parallel."""
    with ThreadPoolExecutor(max_workers=min(max_workers, max(len(nodes), 1))) as ex:
        list(ex.map(ssh_ensure_master, nodes))


# Per-node cache: if SSH fails, serve last successful result
_node_cache: Dict[str, Tuple[List[GpuInfo], NodeMemInfo]] = {}


def collect_basic() -> Tuple[List[dict], List[JobInfo], List[PendingJob], Dict[str, List[JobInfo]], str]:
    """Phase 1: fast local commands only (sinfo + squeue)."""
    nodes_raw, e1 = collect_nodes_basic()
    jobs, e2 = collect_jobs()
    pending, e3 = collect_pending_jobs()
    err = " | ".join(x for x in [e1, e2, e3] if x)
    node_jobs: Dict[str, List[JobInfo]] = {}
    for j in jobs:
        node_jobs.setdefault(j.node, []).append(j)
    return nodes_raw, jobs, pending, node_jobs, err


@dataclass
class NodeSSHResult:
    gpus: List[GpuInfo]
    mem: NodeMemInfo
    error: str


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
        ))
    return result


def collect_node_data_parallel(
    gpu_node_names: List[str], node_timeout: int = 15, max_workers: int = 8,
) -> Tuple[Dict[str, NodeSSHResult], List[str], List[str]]:
    """Phase 2: SSH to GPU nodes (slow). Returns results, stale_nodes, errors."""
    warmup_ssh(gpu_node_names, max_workers=max_workers)

    ssh_results: Dict[str, NodeSSHResult] = {}
    stale_nodes: List[str] = []
    errors: List[str] = []

    with ThreadPoolExecutor(max_workers=min(max_workers, len(gpu_node_names))) as ex:
        futs = {ex.submit(_collect_node_data, n, node_timeout): n for n in gpu_node_names}
        for fut in as_completed(futs):
            name = futs[fut]
            gpus, mem, err = fut.result()
            if err and name in _node_cache:
                cached_gpus, cached_mem = _node_cache[name]
                ssh_results[name] = NodeSSHResult(cached_gpus, cached_mem, "")
                stale_nodes.append(name)
            else:
                ssh_results[name] = NodeSSHResult(gpus, mem, err)
                if gpus or mem.total:
                    _node_cache[name] = (gpus, mem)
                if err:
                    errors.append(f"{name}: {err}")

    if stale_nodes:
        errors.append(f"cached: {','.join(stale_nodes)}")

    return ssh_results, stale_nodes, errors


# ── Daemon data reader ────────────────────────────────────────────────────

_DAEMON_DATA_FILE = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui")) / "data.json"
_DAEMON_MAX_AGE = 30  # seconds: consider data stale if older than this


def read_daemon_data(max_age: float = _DAEMON_MAX_AGE) -> Optional[Tuple[List[NodeInfo], List[JobInfo], List[PendingJob]]]:
    """Try to read fresh data from collector daemon's JSON file.
    Returns (nodes, jobs, pending) or None if unavailable/stale."""
    try:
        if not _DAEMON_DATA_FILE.exists():
            return None
        age = time.time() - _DAEMON_DATA_FILE.stat().st_mtime
        if age > max_age:
            return None
        raw = json.loads(_DAEMON_DATA_FILE.read_text(encoding="utf-8"))
    except Exception:
        return None

    # Convert JSON dicts back to dataclass objects
    jobs: List[JobInfo] = []
    for j in raw.get("jobs", []):
        jobs.append(JobInfo(
            jobid=j.get("jobid", ""), user=j.get("user", ""),
            partition=j.get("partition", ""), jobname=j.get("jobname", ""),
            elapsed=j.get("elapsed", ""), node=j.get("node", ""),
            gpu_count=j.get("gpu_count", 0), gres_raw=j.get("gres_raw", ""),
            time_limit=j.get("time_limit", ""),
        ))

    pending: List[PendingJob] = []
    for p in raw.get("pending", []):
        pending.append(PendingJob(
            jobid=p.get("jobid", ""), user=p.get("user", ""),
            partition=p.get("partition", ""), jobname=p.get("jobname", ""),
            time_limit=p.get("time_limit", ""), gpu_count=p.get("gpu_count", 0),
            reason=p.get("reason", ""), priority=p.get("priority", ""),
        ))

    stale_nodes = set(raw.get("stale_nodes", []))

    nodes: List[NodeInfo] = []
    for n in raw.get("nodes", []):
        gpus = [
            GpuInfo(
                index=g.get("index", ""), name=g.get("name", ""),
                util=g.get("util", ""), mem_used=g.get("mem_used", ""),
                mem_total=g.get("mem_total", ""), temp=g.get("temp", ""),
                power=g.get("power", ""), power_cap=g.get("power_cap", ""),
                pids=g.get("pids", []), users=g.get("users", []),
            )
            for g in n.get("gpus", [])
        ]
        node_jobs = [
            JobInfo(
                jobid=j.get("jobid", ""), user=j.get("user", ""),
                partition=j.get("partition", ""), jobname=j.get("jobname", ""),
                elapsed=j.get("elapsed", ""), node=j.get("node", ""),
                gpu_count=j.get("gpu_count", 0), gres_raw=j.get("gres_raw", ""),
                time_limit=j.get("time_limit", ""),
            )
            for j in n.get("jobs", [])
        ]
        nodes.append(NodeInfo(
            name=n.get("name", ""), state=n.get("state", ""),
            cpus=n.get("cpus", ""), cpu_alloc=n.get("cpu_alloc", ""),
            cpu_load=n.get("cpu_load", ""), mem_total=n.get("mem_total", ""),
            mem_free=n.get("mem_free", ""), gres=n.get("gres", ""),
            gpus=gpus, jobs=node_jobs, error=n.get("error", ""),
            mem_used=n.get("mem_used", ""), mem_avail=n.get("mem_avail", ""),
            stale=n.get("name", "") in stale_nodes,
        ))

    return nodes, jobs, pending


# ── Rich visual helpers ───────────────────────────────────────────────────

def mb_to_gb(val: str) -> str:
    try:
        return f"{float(val) / 1024:.1f}"
    except (ValueError, TypeError):
        return val


def pct_color(pct: float) -> str:
    if pct >= 0.90:
        return "red"
    if pct >= 0.60:
        return "yellow"
    return "green"


def make_bar(pct: float, width: int = 20) -> Text:
    filled = int(pct * width)
    empty = width - filled
    color = pct_color(pct)
    bar = Text()
    bar.append("█" * filled, style=f"bold {color}")
    bar.append("░" * empty, style="bright_black")
    return bar


def util_cell(util_str: str) -> Text:
    try:
        val = float(util_str)
        pct = val / 100.0
    except (ValueError, TypeError):
        return Text(f"{util_str}%", style="bright_black")
    bar = make_bar(pct, width=10)
    label = Text(f" {val:3.0f}%", style=f"bold {pct_color(pct)}")
    return bar + label


def vram_cell(used_str: str, total_str: str) -> Text:
    try:
        u_gb = float(used_str) / 1024
        t_gb = float(total_str) / 1024
        if t_gb <= 0:
            return Text("?")
        pct = u_gb / t_gb
    except (ValueError, TypeError):
        return Text("?")
    bar = make_bar(pct, width=12)
    label = Text(f" {u_gb:.1f}/{t_gb:.0f}G {pct:.0%}", style=f"bold {pct_color(pct)}")
    return bar + label


def temp_cell(temp_str: str) -> Text:
    try:
        t = float(temp_str)
    except (ValueError, TypeError):
        return Text(f"{temp_str}C", style="bright_black")
    if t >= 85:
        return Text(f"{t:.0f}C HOT", style="bold red")
    if t >= 70:
        return Text(f"{t:.0f}C", style="bold yellow")
    return Text(f"{t:.0f}C", style="green")


def power_cell(power_str: str, cap_str: str) -> Text:
    try:
        pw = float(power_str)
        cap = float(cap_str)
        pct = pw / cap if cap > 0 else 0
    except (ValueError, TypeError):
        return Text(f"{power_str}W", style="bright_black")
    bar = make_bar(pct, width=6)
    label = Text(f" {pw:.0f}/{cap:.0f}W", style=f"{pct_color(pct)}")
    return bar + label


def state_cell(state: str) -> Text:
    s = state.lower()
    if "idle" in s:
        return Text(state, style="bold green")
    if "mix" in s:
        return Text(state, style="bold yellow")
    if "alloc" in s:
        return Text(state, style="bold red")
    if "down" in s or "drain" in s:
        return Text(state, style="bold bright_black strike")
    return Text(state)


def node_cell(name: str) -> Text:
    return Text(name, style="bold cyan")


def mem_cell(node: NodeInfo) -> Text:
    """Show memory: used = total - avail. Prefer SSH /proc/meminfo, fallback sinfo."""
    try:
        total_mb = float(node.mem_total)
        if node.mem_avail:
            # SSH data: avail from /proc/meminfo (accurate)
            used_mb = total_mb - float(node.mem_avail)
        else:
            # Fallback: sinfo free (less accurate)
            used_mb = total_mb - float(node.mem_free)
        pct = used_mb / total_mb if total_mb > 0 else 0
        used_gb = used_mb / 1024
        total_gb = total_mb / 1024
    except (ValueError, TypeError):
        return Text("?")
    bar = make_bar(pct, width=8)
    label = Text(f" {used_gb:.0f}/{total_gb:.0f}G {pct:.0%}", style=f"{pct_color(pct)}")
    return bar + label


# ── TUI ────────────────────────────────────────────────────────────────────

class SlurmGpuTui(App):
    TITLE = "SLURM GPU Monitor"
    CSS = """
    Screen { layout: vertical; }
    #status { height: 1; background: $surface; color: $text-muted; padding: 0 1; }
    #summary { height: 3; padding: 0 1; background: $surface; }
    #tbl { height: 1fr; }
    """

    BINDINGS = [
        ("r", "refresh", "Refresh"),
        ("f", "toggle_fast", "Fast/Normal"),
        ("e", "export_json", "Export JSON"),
        ("q", "quit", "Quit"),
    ]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static("loading...", id="summary")
        yield DataTable(id="tbl")
        yield Static("", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.tbl = self.query_one("#tbl", DataTable)
        self.summary_w = self.query_one("#summary", Static)
        self.status_w = self.query_one("#status", Static)

        self.tbl.add_column("Node", width=6)
        self.tbl.add_column("State", width=7)
        self.tbl.add_column("CPU(l/a/t)", width=14)
        self.tbl.add_column("RAM(used/tot)", width=22)
        self.tbl.add_column("GPU#", width=4)
        self.tbl.add_column("GPU Name", width=14)
        self.tbl.add_column("Util", width=15)
        self.tbl.add_column("VRAM", width=22)
        self.tbl.add_column("T", width=7)
        self.tbl.add_column("Power", width=16)
        self.tbl.add_column("User", width=10)
        self.tbl.add_column("JobID", width=8)
        self.tbl.add_column("JobName", width=14)
        self.tbl.add_column("Elapsed", width=10)
        self.tbl.cursor_type = "row"
        self.tbl.zebra_stripes = True

        self.refresh_sec_normal = int(os.getenv("SLURM_GPU_TUI_REFRESH_SEC", "3"))
        self.refresh_sec_fast = int(os.getenv("SLURM_GPU_TUI_FAST_REFRESH_SEC", "1"))
        self.refresh_sec = self.refresh_sec_normal
        self.node_timeout = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
        self.max_workers = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))
        self.snapshot: dict = {}

        self._timer: Timer | None = None
        self._reset_timer(self.refresh_sec)
        self.refresh_all()

    def _reset_timer(self, sec: int) -> None:
        if self._timer is not None:
            self._timer.stop()
        self._timer = self.set_interval(sec, self.refresh_all)

    def action_quit(self) -> None:
        _cleanup_ssh_pool()
        self.exit()

    def action_refresh(self) -> None:
        self.refresh_all()

    def action_toggle_fast(self) -> None:
        if self.refresh_sec == self.refresh_sec_normal:
            self.refresh_sec = self.refresh_sec_fast
        else:
            self.refresh_sec = self.refresh_sec_normal
        self._reset_timer(self.refresh_sec)

    def action_export_json(self) -> None:
        out_dir = Path(os.getenv("SLURM_GPU_TUI_EXPORT_DIR", "./exports"))
        out_dir.mkdir(parents=True, exist_ok=True)
        ts = datetime.now().strftime("%Y%m%d_%H%M%S")
        p = out_dir / f"snapshot_{ts}.json"
        p.write_text(json.dumps(self.snapshot, ensure_ascii=False, indent=2), encoding="utf-8")
        self.status_w.update(f"Saved: {p}")

    @work(exclusive=True, thread=True)
    def refresh_all(self) -> None:
        # Try daemon data first (instant)
        daemon_data = read_daemon_data()
        if daemon_data is not None:
            nodes, jobs, pending = daemon_data
            self.call_from_thread(self._apply, nodes, jobs, pending, "")
            return

        # Fallback: direct collection (2-phase)
        # Phase 1: instant sinfo/squeue data
        nodes_raw, jobs, pending, node_jobs, err1 = collect_basic()
        gpu_node_names = [n["name"] for n in nodes_raw]

        # Show basic data immediately (GPU/mem columns use cache or empty)
        cached_results: Dict[str, NodeSSHResult] = {}
        stale_now: List[str] = []
        for name in gpu_node_names:
            if name in _node_cache:
                gpus, mem = _node_cache[name]
                cached_results[name] = NodeSSHResult(gpus, mem, "")
                stale_now.append(name)
        phase1_nodes = build_nodes(nodes_raw, node_jobs, cached_results, stale_now)
        loading_msg = f"loading GPUs from {len(gpu_node_names)} nodes..."
        self.call_from_thread(self._apply, phase1_nodes, jobs, pending, loading_msg if gpu_node_names else err1)

        # Phase 2: SSH to GPU nodes (slow on first run)
        if gpu_node_names:
            ssh_results, stale_nodes, ssh_errors = collect_node_data_parallel(
                gpu_node_names, node_timeout=self.node_timeout, max_workers=self.max_workers,
            )
            all_errors = [x for x in [err1] + ssh_errors if x]
            phase2_nodes = build_nodes(nodes_raw, node_jobs, ssh_results, stale_nodes)
            self.call_from_thread(self._apply, phase2_nodes, jobs, pending, " | ".join(all_errors) if all_errors else "")

    _NCOLS = 14  # number of table columns

    def _apply(self, nodes: List[NodeInfo], jobs: List[JobInfo], pending: List[PendingJob], err: str) -> None:
        # Save cursor/scroll position before clearing
        saved_row = self.tbl.cursor_row
        saved_col = self.tbl.cursor_column
        saved_scroll_x = self.tbl.scroll_x
        saved_scroll_y = self.tbl.scroll_y

        self.tbl.clear()
        empty = [Text("") for _ in range(self._NCOLS)]

        total_gpus = 0
        busy_gpus = 0
        user_gpu_count: Dict[str, int] = {}

        first_node = True
        for node in nodes:
            if not first_node:
                self.tbl.add_row(*list(empty))
            first_node = False

            alloc = node.cpu_alloc or "0"
            cpu_text = Text()
            cpu_text.append(f"{node.cpu_load}", style="bold")
            cpu_text.append(f"/{alloc}/{node.cpus}", style="dim")
            mem_text = mem_cell(node)
            if node.stale:
                nname = Text()
                nname.append(node.name, style="bold cyan")
                nname.append(" [stale]", style="dim yellow")
            else:
                nname = node_cell(node.name)
            nstate = state_cell(node.state)

            if node.gpus:
                for i, gpu in enumerate(node.gpus):
                    total_gpus += 1
                    try:
                        if float(gpu.util) > 5:
                            busy_gpus += 1
                    except (ValueError, TypeError):
                        pass

                    user = ""
                    jobid = ""
                    jobname = ""
                    elapsed = ""
                    # Map GPU index to job using SLURM GPU allocation counts
                    gpu_idx = int(gpu.index) if gpu.index.isdigit() else i
                    assigned = 0
                    for j in node.jobs:
                        if assigned + j.gpu_count > gpu_idx:
                            user, jobid, jobname, elapsed = j.user, j.jobid, j.jobname, j.elapsed
                            break
                        assigned += j.gpu_count

                    is_first = (i == 0)
                    self.tbl.add_row(
                        nname if is_first else Text(""),
                        nstate if is_first else Text(""),
                        cpu_text if is_first else Text(""),
                        mem_text if is_first else Text(""),
                        Text(gpu.index, style="bold"),
                        Text(gpu.name),
                        util_cell(gpu.util),
                        vram_cell(gpu.mem_used, gpu.mem_total),
                        temp_cell(gpu.temp),
                        power_cell(gpu.power, gpu.power_cap),
                        Text(user, style="bold magenta") if user else Text(""),
                        Text(jobid, style="dim") if jobid else Text(""),
                        Text(jobname) if jobname else Text(""),
                        Text(elapsed, style="dim") if elapsed else Text(""),
                    )
            elif node.jobs:
                for i, j in enumerate(node.jobs):
                    is_first = (i == 0)
                    self.tbl.add_row(
                        nname if is_first else Text(""),
                        nstate if is_first else Text(""),
                        cpu_text if is_first else Text(""),
                        mem_text if is_first else Text(""),
                        Text("-", style="dim"), Text("-", style="dim"),
                        Text("-", style="dim"), Text("-", style="dim"),
                        Text("-", style="dim"), Text("-", style="dim"),
                        Text(j.user, style="bold magenta"),
                        Text(j.jobid, style="dim"),
                        Text(j.jobname),
                        Text(j.elapsed, style="dim"),
                    )
            else:
                self.tbl.add_row(
                    nname, nstate, cpu_text, mem_text,
                    Text("-", style="dim"), Text("-", style="dim"),
                    Text("-", style="dim"), Text("-", style="dim"),
                    Text("-", style="dim"), Text("-", style="dim"),
                    Text(""), Text(""), Text(""), Text(""),
                )

        # ── Pending Jobs Section ──
        if pending:
            self.tbl.add_row(*list(empty))
            # Separator line
            sep = [Text("─" * 12, style="dim") for _ in range(self._NCOLS)]
            self.tbl.add_row(*sep)
            self.tbl.add_row(*list(empty))
            # Section header: reuse columns for pending-specific labels
            gpu_req = sum(p.gpu_count for p in pending)
            hdr = [Text("") for _ in range(self._NCOLS)]
            hdr[0] = Text(f" PENDING ({len(pending)}) ", style="bold white on dark_orange3")
            hdr[1] = Text("JobID", style="bold dim")
            hdr[2] = Text("User", style="bold dim")
            hdr[3] = Text("JobName", style="bold dim")
            hdr[4] = Text("GPU", style="bold dim")
            hdr[5] = Text("Partition", style="bold dim")
            hdr[6] = Text("Reason", style="bold dim")
            if gpu_req:
                hdr[7] = Text(f"({gpu_req} GPUs req)", style="bold yellow")
            self.tbl.add_row(*hdr)

            for pj in pending:
                reason_style = "bold red" if pj.reason == "Resources" else "yellow" if pj.reason == "Priority" else "dim"
                gpu_txt = f"x{pj.gpu_count}" if pj.gpu_count else "-"
                row = [Text("") for _ in range(self._NCOLS)]
                row[1] = Text(pj.jobid, style="dim")
                row[2] = Text(pj.user, style="bold magenta")
                row[3] = Text(pj.jobname)
                row[4] = Text(gpu_txt, style="bold")
                row[5] = Text(pj.partition, style="dim")
                row[6] = Text(pj.reason, style=reason_style)
                self.tbl.add_row(*row)

        for j in jobs:
            if j.gpu_count > 0:
                user_gpu_count[j.user] = user_gpu_count.get(j.user, 0) + j.gpu_count

        ts = datetime.now().strftime("%H:%M:%S")
        mode_label = Text(" FAST ", style="bold white on red") if self.refresh_sec == self.refresh_sec_fast else Text(" NORM ", style="bold white on blue")

        summary = Text()
        summary.append(" GPU ", style="bold white on dark_green")
        summary.append(f" {busy_gpus}/{total_gpus} active  ", style="bold")
        summary.append(" JOBS ", style="bold white on dark_blue")
        summary.append(f" {len(jobs)} run  ", style="bold")
        if pending:
            summary.append(" WAIT ", style="bold white on dark_orange3")
            summary.append(f" {len(pending)}  ", style="bold")
        summary.append_text(mode_label)
        summary.append(f" {self.refresh_sec}s  ")
        summary.append(f"[{ts}]\n", style="dim")

        summary.append(" USER/GPU ", style="bold white on purple")
        summary.append(" ")
        for u, g in sorted(user_gpu_count.items(), key=lambda x: -x[1]):
            summary.append(f" {u}", style="bold magenta")
            summary.append(f":{g} ", style="bold")

        self.summary_w.update(summary)

        if err:
            self.status_w.update(Text(f" WARN: {err} ", style="bold yellow on dark_red"))
        else:
            self.status_w.update(Text(f" OK [{ts}] ", style="dim"))

        # Restore cursor/scroll position
        row_count = self.tbl.row_count
        if row_count > 0:
            self.tbl.move_cursor(
                row=min(saved_row, row_count - 1),
                column=saved_col,
                animate=False,
            )
            self.tbl.scroll_x = saved_scroll_x
            self.tbl.scroll_y = saved_scroll_y

        self.snapshot = {
            "ts": datetime.now().isoformat(),
            "nodes": [
                {
                    "name": n.name, "state": n.state, "cpus": n.cpus,
                    "cpu_load": n.cpu_load,
                    "mem_total_gb": mb_to_gb(n.mem_total),
                    "mem_free_gb": mb_to_gb(n.mem_free),
                    "gres": n.gres,
                    "gpus": [{"index": g.index, "name": g.name, "util": g.util,
                              "vram_used_gb": mb_to_gb(g.mem_used),
                              "vram_total_gb": mb_to_gb(g.mem_total),
                              "temp": g.temp, "power": g.power, "power_cap": g.power_cap}
                             for g in n.gpus],
                    "jobs": [{"jobid": j.jobid, "user": j.user, "jobname": j.jobname,
                              "elapsed": j.elapsed} for j in n.jobs],
                }
                for n in nodes
            ],
        }


if __name__ == "__main__":
    SlurmGpuTui().run()
