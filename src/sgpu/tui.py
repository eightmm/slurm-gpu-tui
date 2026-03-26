"""SLURM GPU Monitor TUI application."""
from __future__ import annotations

import json
import os
import time
from dataclasses import asdict
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rich.text import Text
from textual import work
from textual.app import App, ComposeResult
from textual.containers import Vertical
from textual.coordinate import Coordinate
from textual.timer import Timer
from textual.widgets import DataTable, Footer, Header, Input, Static

from .common import (
    GpuInfo, JobInfo, NodeInfo, NodeMemInfo, NodeSSHResult, PendingJob,
    build_nodes, cleanup_ssh_pool, collect_basic, collect_node_data_parallel,
)


# ── Daemon data reader ────────────────────────────────────────────────────

_DAEMON_DATA_FILE = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui")) / "data.json"
_DAEMON_MAX_AGE = 30


def read_daemon_data(max_age: float = _DAEMON_MAX_AGE) -> Optional[Tuple[List[NodeInfo], List[JobInfo], List[PendingJob], str]]:
    """Try to read fresh data from collector daemon's JSON file."""
    try:
        if not _DAEMON_DATA_FILE.exists():
            return None
        age = time.time() - _DAEMON_DATA_FILE.stat().st_mtime
        if age > max_age:
            return None
        raw = json.loads(_DAEMON_DATA_FILE.read_text(encoding="utf-8"))
    except Exception:
        return None

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
            error_kind=n.get("error_kind", ""),
        ))

    daemon_errors = raw.get("errors", "")
    return nodes, jobs, pending, daemon_errors


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
        return Text(f"● {state}", style="bold green")
    if "mix" in s:
        return Text(f"◐ {state}", style="bold yellow")
    if "alloc" in s:
        return Text(f"○ {state}", style="bold blue")
    if "down" in s or "drain" in s:
        return Text(f"✖ {state}", style="bold bright_black")
    return Text(state)


def highlight_row(cells: list) -> list:
    """Apply 'My Jobs' background highlight to all cells in a row."""
    for cell in cells:
        if isinstance(cell, Text):
            cell.stylize("on #1a1a3a")
    return cells


def node_cell(name: str) -> Text:
    return Text(name, style="bold cyan")


def mem_cell(node: NodeInfo) -> Text:
    """Show memory: used = total - avail."""
    try:
        total_mb = float(node.mem_total)
        if node.mem_avail:
            used_mb = total_mb - float(node.mem_avail)
        else:
            used_mb = total_mb - float(node.mem_free)
        pct = used_mb / total_mb if total_mb > 0 else 0
        used_gb = used_mb / 1024
        total_gb = total_mb / 1024
    except (ValueError, TypeError):
        return Text("?")
    bar = make_bar(pct, width=8)
    label = Text(f" {used_gb:.0f}/{total_gb:.0f}G {pct:.0%}", style=f"{pct_color(pct)}")
    return bar + label


def parse_slurm_duration(s: str) -> int:
    """Parse SLURM duration string to total seconds. Returns -1 on failure."""
    if not s or s in ("N/A", "UNLIMITED", "Partition_Limit"):
        return -1
    try:
        days = 0
        if "-" in s:
            d, s = s.split("-", 1)
            days = int(d)
        parts = s.split(":")
        if len(parts) == 3:
            h, m, sec = int(parts[0]), int(parts[1]), int(parts[2])
        elif len(parts) == 2:
            h, m, sec = 0, int(parts[0]), int(parts[1])
        else:
            return -1
        return days * 86400 + h * 3600 + m * 60 + sec
    except (ValueError, IndexError):
        return -1


def remaining_cell(elapsed: str, time_limit: str) -> Text:
    """Show time remaining in job. Red <1h, yellow <12h, green otherwise."""
    elapsed_s = parse_slurm_duration(elapsed)
    limit_s = parse_slurm_duration(time_limit)
    if elapsed_s < 0 or limit_s < 0:
        return Text(elapsed or "", style="dim")
    remaining_s = limit_s - elapsed_s
    if remaining_s < 0:
        remaining_s = 0
    h = remaining_s // 3600
    m = (remaining_s % 3600) // 60
    label = f"{h}:{m:02d}h" if h > 0 else f"{m}m"
    if remaining_s < 3600:
        return Text(label, style="bold red")
    elif remaining_s < 43200:
        return Text(label, style="yellow")
    else:
        return Text(label, style="dim green")


# ── TUI App ───────────────────────────────────────────────────────────────

class SlurmGpuTui(App):
    TITLE = "SLURM GPU Monitor"
    CSS = """
    Screen { layout: vertical; }
    #status { height: 1; background: $surface; color: $text-muted; padding: 0 1; }
    #summary { height: 3; padding: 0 1; background: $surface; }
    #tbl-container { height: 1fr; layout: vertical; }
    #tbl { height: 1fr; }
    #pending-container { height: auto; max-height: 12; border-top: solid $primary; }
    #pending-tbl { height: auto; max-height: 10; }
    #pending-label { background: $primary; color: $text; padding: 0 1; }
    #search-input { display: none; height: 1; border: none; padding: 0 1; background: $surface; }
    """

    BINDINGS = [
        ("r", "refresh", "Refresh"),
        ("f", "toggle_fast", "Fast/Normal"),
        ("s", "toggle_sort", "Sort"),
        ("u", "toggle_user_filter", "My Jobs"),
        ("i", "toggle_idle_filter", "Idle Only"),
        ("space", "toggle_collapse", "Collapse"),
        ("d", "toggle_details", "Details"),
        ("j", "cursor_down", "↓"),
        ("k", "cursor_up", "↑"),
        ("slash", "start_search", "Search"),
        ("e", "export_json", "Export JSON"),
        ("q", "quit", "Quit"),
    ]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static("loading...", id="summary")
        with Vertical(id="tbl-container"):
            yield DataTable(id="tbl")
            with Vertical(id="pending-container"):
                yield Static(" PENDING JOBS ", id="pending-label")
                yield DataTable(id="pending-tbl")
        yield Input(placeholder="/ filter: node or user (Esc to clear)", id="search-input")
        yield Static("", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.tbl = self.query_one("#tbl", DataTable)
        self.pending_tbl = self.query_one("#pending-tbl", DataTable)
        self.summary_w = self.query_one("#summary", Static)
        self.status_w = self.query_one("#status", Static)
        self.current_user = os.environ.get("USER", os.getlogin() if hasattr(os, "getlogin") else "user")
        self._node_cache: dict = {}

        self.show_details = False
        self.idle_filter_only = False
        self.search_text = ""

        self._setup_columns()
        self.tbl.cursor_type = "row"
        self.tbl.zebra_stripes = True

        self.pending_tbl.add_column("JobID", key="p_jobid")
        self.pending_tbl.add_column("User", key="p_user")
        self.pending_tbl.add_column("GPUs", key="p_gpu")
        self.pending_tbl.add_column("Partition", key="p_part")
        self.pending_tbl.add_column("JobName", key="p_name")
        self.pending_tbl.add_column("Reason", key="p_reason")
        self.pending_tbl.add_column("Priority", key="p_pri")
        self.pending_tbl.cursor_type = "row"
        self.pending_tbl.zebra_stripes = True

        self.refresh_sec_normal = int(os.getenv("SLURM_GPU_TUI_REFRESH_SEC", "3"))
        self.refresh_sec_fast = int(os.getenv("SLURM_GPU_TUI_FAST_REFRESH_SEC", "1"))
        self.refresh_sec = self.refresh_sec_normal
        self.node_timeout = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
        self.max_workers = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))
        self.snapshot: dict = {}

        self.sort_by = "node"  # "node", "util", "user"
        self.user_filter_only = False
        self._collapsed: set = set()  # node names that are collapsed

        self._timer: Timer | None = None
        self._reset_timer(self.refresh_sec)
        self.refresh_all()

    def _reset_timer(self, sec: int) -> None:
        if self._timer is not None:
            self._timer.stop()
        self._timer = self.set_interval(sec, self.refresh_all)

    def action_quit(self) -> None:
        from .common import cleanup_ssh_pool
        cleanup_ssh_pool()
        self.exit()

    def action_refresh(self) -> None:
        self.refresh_all()

    def action_toggle_fast(self) -> None:
        if self.refresh_sec == self.refresh_sec_normal:
            self.refresh_sec = self.refresh_sec_fast
        else:
            self.refresh_sec = self.refresh_sec_normal
        self._reset_timer(self.refresh_sec)
        self.refresh_all()

    def action_export_json(self) -> None:
        import json as _json
        out = Path(f"sgpu-export-{datetime.now().strftime('%Y%m%d-%H%M%S')}.json")
        out.write_text(_json.dumps(self.snapshot, indent=2, ensure_ascii=False))
        self.status_w.update(Text(f" Exported → {out} ", style="bold green"))

    def action_toggle_sort(self) -> None:
        choices = ["node", "util", "user"]
        idx = choices.index(self.sort_by)
        self.sort_by = choices[(idx + 1) % len(choices)]
        self.status_w.update(f"Sort by: {self.sort_by}")
        self.refresh_all()

    def action_toggle_user_filter(self) -> None:
        self.user_filter_only = not self.user_filter_only
        status = "My Jobs Only" if self.user_filter_only else "All Jobs"
        self.status_w.update(status)
        self.refresh_all()

    def action_toggle_collapse(self) -> None:
        if self.tbl.row_count == 0:
            return
        try:
            cell_key = self.tbl.coordinate_to_cell_key(Coordinate(self.tbl.cursor_row, 0))
            row_key = str(cell_key.row_key.value)
        except Exception:
            return
        if not row_key.startswith("hdr_"):
            self.status_w.update(Text(" Move cursor to a node header row to collapse ", style="dim"))
            return
        node_name = row_key[4:]
        if node_name in self._collapsed:
            self._collapsed.discard(node_name)
        else:
            self._collapsed.add(node_name)
        self.refresh_all()

    def _setup_columns(self) -> None:
        self.tbl.clear(columns=True)
        self.tbl.add_column("Node", key="node", width=12)
        self.tbl.add_column("State", key="state", width=10)
        self.tbl.add_column("Part", key="part", width=9)
        self.tbl.add_column("CPU a/t", key="cpu", width=8)
        self.tbl.add_column("RAM", key="ram", width=18)
        self.tbl.add_column("GPU#", key="gpu_idx")
        self.tbl.add_column("GPU Name", key="gpu_name")
        self.tbl.add_column("Util", key="util")
        self.tbl.add_column("VRAM", key="vram")
        if self.show_details:
            self.tbl.add_column("T", key="temp")
            self.tbl.add_column("Power", key="power")
        self.tbl.add_column("User", key="user", width=12)
        if self.show_details:
            self.tbl.add_column("JobID", key="jobid")
            self.tbl.add_column("JobName", key="jobname")
        self.tbl.add_column("Remaining", key="remaining")

    def action_toggle_idle_filter(self) -> None:
        self.idle_filter_only = not self.idle_filter_only
        status = "Idle Nodes Only" if self.idle_filter_only else "All Nodes"
        self.status_w.update(status)
        self.refresh_all()

    def action_toggle_details(self) -> None:
        self.show_details = not self.show_details
        status = "Details: ON" if self.show_details else "Details: OFF"
        self.status_w.update(status)
        self._setup_columns()
        self.refresh_all()

    def action_cursor_down(self) -> None:
        self.tbl.action_scroll_down()

    def action_cursor_up(self) -> None:
        self.tbl.action_scroll_up()

    def action_start_search(self) -> None:
        w = self.query_one("#search-input", Input)
        w.display = True
        w.focus()

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "search-input":
            self.search_text = event.value.strip().lower()
            self.refresh_all()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input.id == "search-input":
            self.tbl.focus()

    def on_key(self, event) -> None:
        if event.key == "escape":
            w = self.query_one("#search-input", Input)
            if w.display:
                w.clear()
                w.display = False
                self.search_text = ""
                self.tbl.focus()
                self.refresh_all()

    @work(exclusive=True, thread=True)
    def refresh_all(self) -> None:
        # Try daemon data first (instant)
        daemon_data = read_daemon_data()
        if daemon_data is not None:
            nodes, jobs, pending, daemon_err = daemon_data
            self.call_from_thread(self._apply, nodes, jobs, pending, daemon_err)
            return

        # Fallback: direct collection (2-phase)
        nodes_raw, jobs, pending, node_jobs, err1 = collect_basic()
        node_names = [n["name"] for n in nodes_raw]

        # Show basic data immediately (use cache or empty)
        cached_results: Dict[str, NodeSSHResult] = {}
        stale_now: List[str] = []
        for name in node_names:
            if name in self._node_cache:
                gpus, mem = self._node_cache[name]
                cached_results[name] = NodeSSHResult(gpus, mem, "")
                stale_now.append(name)
        phase1_nodes = build_nodes(nodes_raw, node_jobs, cached_results, stale_now)
        loading_msg = f"loading GPUs from {len(node_names)} nodes..."
        self.call_from_thread(self._apply, phase1_nodes, jobs, pending, loading_msg if node_names else err1)

        # Phase 2: SSH to nodes (slow on first run)
        if node_names:
            ssh_results, stale_nodes, ssh_errors = collect_node_data_parallel(
                node_names, node_timeout=self.node_timeout, max_workers=self.max_workers,
                cache=self._node_cache,
            )
            all_errors = [x for x in [err1] + ssh_errors if x]
            phase2_nodes = build_nodes(nodes_raw, node_jobs, ssh_results, stale_nodes)
            self.call_from_thread(self._apply, phase2_nodes, jobs, pending, " | ".join(all_errors) if all_errors else "")

    def _apply(self, nodes: List[NodeInfo], jobs: List[JobInfo], pending: List[PendingJob], err: str) -> None:
        saved_row = self.tbl.cursor_row
        saved_col = self.tbl.cursor_column
        saved_scroll_x = self.tbl.scroll_x
        saved_scroll_y = self.tbl.scroll_y

        self.tbl.clear()
        self.pending_tbl.clear()

        # Sorting nodes logic (simplistic)
        if self.sort_by == "util":
            # Sort by max util on node
            nodes.sort(key=lambda n: max([float(g.util or 0) for g in n.gpus] + [0]), reverse=True)
        elif self.sort_by == "user":
            # Nodes with current user first
            nodes.sort(key=lambda n: any(self.current_user in g.users for g in n.gpus), reverse=True)
        else:
            nodes.sort(key=lambda n: n.name)

        _HDR_BG = "on #0d1f0d"  # dark green tint for node header rows

        total_gpus = 0
        busy_gpus = 0
        user_gpu_count: Dict[str, int] = {}
        partition_gpu_stats: Dict[str, List[int]] = {}  # partition -> [busy, total]

        for node in nodes:
            # Filter logic
            if self.user_filter_only:
                has_my_job = any(self.current_user in g.users for g in node.gpus)
                has_my_job = has_my_job or any(j.user == self.current_user for j in node.jobs)
                if not has_my_job:
                    continue

            if self.idle_filter_only:
                has_idle = False
                for g in node.gpus:
                    try:
                        if float(g.util) <= 5:
                            has_idle = True
                            break
                    except (ValueError, TypeError):
                        pass
                if not has_idle:
                    continue

            if self.search_text:
                node_users = set()
                for g in node.gpus:
                    node_users.update(g.users)
                for j in node.jobs:
                    node_users.add(j.user)
                if (self.search_text not in node.name.lower() and
                        not any(self.search_text in u.lower() for u in node_users)):
                    continue

            node_partition = node.jobs[0].partition if node.jobs else ""

            alloc = node.cpu_alloc or "0"
            cpu_text = Text()
            cpu_text.append(f"{alloc}", style="bold")
            cpu_text.append(f"/{node.cpus}", style="dim")

            # ── Node header row ──────────────────────────────────────────────
            is_collapsed = node.name in self._collapsed
            arrow = "▶" if is_collapsed else "▼"
            _ERROR_LABELS = {
                "ssh_timeout": "~timeout",
                "ssh_unreachable": "~unreachable",
                "ssh_auth": "~auth_err",
                "nvidia_smi_missing": "~no_smi",
                "nvidia_smi_failed": "~smi_err",
                "parse_error": "~parse_err",
                "slurm_down": "~down",
                "stale_cached": "~stale",
            }
            nname = Text(f"{arrow} {node.name}", style="bold white")
            if node.stale:
                label = _ERROR_LABELS.get(node.error_kind, "~stale")
                nname.append(f" {label}", style="dim yellow")
            if self.show_details:
                hdr_cells = [
                    nname, state_cell(node.state), Text(node_partition, style="cyan"),
                    cpu_text, mem_cell(node),
                    Text(""), Text(""), Text(""), Text(""),
                    Text(""), Text(""), Text(""), Text(""),
                    Text(""), Text(""),
                ]
            else:
                hdr_cells = [
                    nname, state_cell(node.state), Text(node_partition, style="cyan"),
                    cpu_text, mem_cell(node),
                    Text(""), Text(""), Text(""), Text(""), Text(""), Text(""),
                ]
            for cell in hdr_cells:
                cell.stylize(_HDR_BG)
            self.tbl.add_row(*hdr_cells, key=f"hdr_{node.name}")

            if is_collapsed:
                pass
            elif node.gpus:
                for gpu in node.gpus:
                    total_gpus += 1
                    gpu_busy = False
                    try:
                        if float(gpu.util) > 5:
                            busy_gpus += 1
                            gpu_busy = True
                    except (ValueError, TypeError):
                        pass

                    if node_partition not in partition_gpu_stats:
                        partition_gpu_stats[node_partition] = [0, 0]
                    partition_gpu_stats[node_partition][1] += 1
                    if gpu_busy:
                        partition_gpu_stats[node_partition][0] += 1

                    user = ""
                    jobid = ""
                    jobname = ""
                    elapsed = ""
                    is_me = False
                    matched_job = None
                    if gpu.users:
                        user = ",".join(gpu.users)
                        is_me = self.current_user in gpu.users
                        for j in node.jobs:
                            if j.user in gpu.users:
                                jobid, jobname, elapsed = j.jobid, j.jobname, j.elapsed
                                matched_job = j
                                break

                    row_cells = [
                        Text(""), Text(""), Text(""),  # node, state, part
                        Text(""), Text(""),             # cpu, ram
                        Text(f"  {gpu.index}", style="bold"),
                        Text(gpu.name),
                        util_cell(gpu.util),
                        vram_cell(gpu.mem_used, gpu.mem_total),
                    ]
                    if self.show_details:
                        row_cells.append(temp_cell(gpu.temp))
                        row_cells.append(power_cell(gpu.power, gpu.power_cap))
                    row_cells.append(Text(user, style="bold magenta") if user else Text(""))
                    if self.show_details:
                        row_cells.append(Text(jobid, style="dim") if jobid else Text(""))
                        row_cells.append(Text(jobname) if jobname else Text(""))
                    row_cells.append(remaining_cell(elapsed, matched_job.time_limit) if matched_job else Text("", style="dim"))
                    if is_me:
                        highlight_row(row_cells)
                    self.tbl.add_row(*row_cells)

            elif node.jobs:
                for j in node.jobs:
                    is_me = (j.user == self.current_user)
                    if self.user_filter_only and not is_me:
                        continue
                    row_cells = [
                        Text(""), Text(""), Text(""),
                        Text(""), Text(""),
                        Text(""), Text("-", style="dim"),
                        Text("-", style="dim"), Text("-", style="dim"),
                    ]
                    if self.show_details:
                        row_cells.append(Text("-", style="dim"))
                        row_cells.append(Text("-", style="dim"))
                    row_cells.append(Text(j.user, style="bold magenta"))
                    if self.show_details:
                        row_cells.append(Text(j.jobid, style="dim"))
                        row_cells.append(Text(j.jobname))
                    row_cells.append(remaining_cell(j.elapsed, j.time_limit))
                    if is_me:
                        highlight_row(row_cells)
                    self.tbl.add_row(*row_cells)

        # ── Pending Jobs Table ──
        self.query_one("#pending-container").display = bool(pending)
        for pj in pending:
            is_me = (pj.user == self.current_user)
            if self.user_filter_only and not is_me:
                continue
            reason_style = "bold red" if pj.reason == "Resources" else "yellow" if pj.reason == "Priority" else "dim"
            gpu_txt = f"x{pj.gpu_count}" if pj.gpu_count else "-"
            row_cells = [
                Text(pj.jobid, style="dim"),
                Text(pj.user, style="bold magenta"),
                Text(gpu_txt, style="bold"),
                Text(pj.partition, style="dim"),
                Text(pj.jobname),
                Text(pj.reason, style=reason_style),
                Text(pj.priority, style="dim"),
            ]
            if is_me:
                highlight_row(row_cells)
            self.pending_tbl.add_row(*row_cells)

        # Update Summary
        for j in jobs:
            if j.gpu_count > 0:
                user_gpu_count[j.user] = user_gpu_count.get(j.user, 0) + j.gpu_count

        ts = datetime.now().strftime("%H:%M:%S")
        mode_label = Text(" FAST ", style="bold white on red") if self.refresh_sec == self.refresh_sec_fast else Text(" NORM ", style="bold white on blue")
        sort_label = Text(f" SORT:{self.sort_by.upper()} ", style="bold white on #444444")

        summary = Text()
        summary.append(" GPU ", style="bold white on dark_green")
        summary.append(f" {busy_gpus}/{total_gpus} active  ", style="bold")
        # Per-partition GPU breakdown
        if partition_gpu_stats:
            for part, (pbsy, ptot) in sorted(partition_gpu_stats.items()):
                summary.append(f" [{part} {pbsy}/{ptot}]", style="dim cyan")
            summary.append("  ")
        summary.append(" JOBS ", style="bold white on dark_blue")
        summary.append(f" {len(jobs)} run  ", style="bold")
        if pending:
            summary.append(" WAIT ", style="bold white on dark_orange3")
            summary.append(f" {len(pending)}  ", style="bold")
        summary.append_text(mode_label)
        summary.append_text(sort_label)
        if self.user_filter_only:
            summary.append(" MY JOBS ", style="bold white on #666600")
        if self.idle_filter_only:
            summary.append(" IDLE ", style="bold white on dark_cyan")
        if self.search_text:
            summary.append(f" /{self.search_text} ", style="bold white on #664400")
        summary.append(f" {self.refresh_sec}s  ")
        summary.append(f"[{ts}]\n", style="dim")

        summary.append(" USER/GPU ", style="bold white on purple")
        summary.append(" ")
        for u, g in sorted(user_gpu_count.items(), key=lambda x: -x[1])[:10]: # Top 10
            style = "bold yellow underline" if u == self.current_user else "bold magenta"
            summary.append(f" {u}", style=style)
            summary.append(f":{g} ", style="bold")

        self.summary_w.update(summary)

        if err:
            self.status_w.update(Text(f" WARN: {err} ", style="bold yellow on dark_red"))
        else:
            self.status_w.update(Text(f" OK [{ts}] ", style="dim"))

        if self.tbl.row_count > 0:
            self.tbl.move_cursor(row=min(saved_row, self.tbl.row_count - 1), column=saved_col, animate=False)
        self.tbl.scroll_to(x=saved_scroll_x, y=saved_scroll_y, animate=False)

        self.snapshot = {
            "ts": datetime.now().isoformat(),
            "nodes": [asdict(n) for n in nodes],
            "jobs": [asdict(j) for j in jobs],
            "pending": [asdict(p) for p in pending],
        }


def main():
    SlurmGpuTui().run()
