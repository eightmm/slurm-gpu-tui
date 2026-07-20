"""SLURM GPU Monitor TUI application."""
from __future__ import annotations

import getpass
import json
import os
import re
import threading
import time
from dataclasses import asdict
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rich.text import Text
from textual import work
from textual.app import App, ComposeResult
from textual.containers import Vertical, VerticalScroll
from textual.coordinate import Coordinate
from textual.timer import Timer
from textual.widgets import (
    DataTable, Footer, Header, Input, Static, TabbedContent, TabPane,
)

from .cells import (
    WASTE_MIN_SEC, classify_gpu, collect_waste, ellipsize, fmt_idle_age,
    fmt_span, fmt_start_time, gpu_strip, highlight_row, make_bar, mem_cell,
    pct_color, power_cell, remaining_cell, state_cell, temp_cell, util_cell,
    vram_cell,
)
from .common import (
    GpuInfo, JobInfo, NodeInfo, NodeSSHResult, PendingJob,
    apply_gpu_alloc, build_nodes, cleanup_ssh_pool, collect_basic,
    collect_node_data_parallel, run_cmd,
)
from .screens import (
    _JAMO_ACTIONS, ConfirmScreen, DetailScreen, HelpScreen, UserSelectScreen,
    WasteScreen,
)
from .usage import render_usage


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
        return _parse_daemon_data(raw)
    except Exception:
        # any malformed entry (wrong type, non-dict item) falls back to
        # direct collection instead of killing the refresh worker
        return None


def _parse_daemon_data(raw: dict) -> Tuple[List[NodeInfo], List[JobInfo], List[PendingJob], str]:
    jobs: List[JobInfo] = []
    for j in raw.get("jobs", []):
        jobs.append(JobInfo(
            jobid=j.get("jobid", ""), user=j.get("user", ""),
            partition=j.get("partition", ""), jobname=j.get("jobname", ""),
            elapsed=j.get("elapsed", ""), node=j.get("node", ""),
            gpu_count=j.get("gpu_count", 0), gres_raw=j.get("gres_raw", ""),
            time_limit=j.get("time_limit", ""), script=j.get("script", ""),
        ))

    pending: List[PendingJob] = []
    for p in raw.get("pending", []):
        pending.append(PendingJob(
            jobid=p.get("jobid", ""), user=p.get("user", ""),
            partition=p.get("partition", ""), jobname=p.get("jobname", ""),
            time_limit=p.get("time_limit", ""), gpu_count=p.get("gpu_count", 0),
            reason=p.get("reason", ""), priority=p.get("priority", ""),
            start_time=p.get("start_time", ""),
        ))

    stale_nodes = set(raw.get("stale_nodes", []))

    nodes: List[NodeInfo] = []
    for n in raw.get("nodes", []):
        gpus = [
            GpuInfo(
                index=g.get("index", ""), minor=g.get("minor", ""),
                uuid=g.get("uuid", ""), pci_bus=g.get("pci_bus", ""),
                slot=g.get("slot", ""),
                serial=g.get("serial", ""), name=g.get("name", ""),
                util=g.get("util", ""), mem_used=g.get("mem_used", ""),
                mem_total=g.get("mem_total", ""), temp=g.get("temp", ""),
                power=g.get("power", ""), power_cap=g.get("power_cap", ""),
                ecc=g.get("ecc", ""),
                pids=g.get("pids", []), users=g.get("users", []),
                pid_mem=g.get("pid_mem", {}) or {},
                pid_jobid=g.get("pid_jobid", {}) or {},
                alloc_jobid=g.get("alloc_jobid", ""), alloc_user=g.get("alloc_user", ""),
                idle_sec=g.get("idle_sec", 0), parked_sec=g.get("parked_sec", 0),
            )
            for g in n.get("gpus", [])
        ]
        node_jobs = [
            JobInfo(
                jobid=j.get("jobid", ""), user=j.get("user", ""),
                partition=j.get("partition", ""), jobname=j.get("jobname", ""),
                elapsed=j.get("elapsed", ""), node=j.get("node", ""),
                gpu_count=j.get("gpu_count", 0), cpu_count=j.get("cpu_count", 0),
                gres_raw=j.get("gres_raw", ""),
                time_limit=j.get("time_limit", ""),
            )
            for j in n.get("jobs", [])
        ]
        nodes.append(NodeInfo(
            name=n.get("name", ""), state=n.get("state", ""),
            partition=n.get("partition", ""), source=n.get("source", ""),
            has_gpu=n.get("has_gpu", True),
            cpus=n.get("cpus", ""), cpu_alloc=n.get("cpu_alloc", ""),
            cpu_load=n.get("cpu_load", ""), mem_total=n.get("mem_total", ""),
            mem_free=n.get("mem_free", ""), mem_alloc=n.get("mem_alloc", ""),
            gres=n.get("gres", ""),
            gpus=gpus, jobs=node_jobs, error=n.get("error", ""),
            mem_used=n.get("mem_used", ""), mem_avail=n.get("mem_avail", ""),
            stale=n.get("name", "") in stale_nodes,
            error_kind=n.get("error_kind", ""),
        ))

    daemon_errors = raw.get("errors", "")
    return nodes, jobs, pending, daemon_errors


def _node_source_counts(nodes: List[NodeInfo]) -> Tuple[int, int, int, int, int]:
    """GPU push/fallback, CPU telemetry polling, and all stale nodes."""
    agent = sum(1 for n in nodes if n.has_gpu and n.source == "agent")
    gpu_fallback = sum(1 for n in nodes if n.has_gpu and n.source == "ssh")
    cpu_push = sum(1 for n in nodes if not n.has_gpu and n.source == "agent")
    cpu_poll = sum(1 for n in nodes if not n.has_gpu and n.source == "ssh")
    stale = sum(1 for n in nodes if n.source == "stale" or (n.stale and not n.source))
    return agent, gpu_fallback, cpu_push, cpu_poll, stale



# ── TUI App ───────────────────────────────────────────────────────────────

class SlurmGpuTui(App):
    TITLE = "SLURM GPU Monitor"
    CSS = """
    Screen { layout: vertical; }
    #status { height: 1; background: $surface; color: $text-muted; padding: 0 1; }
    #summary { height: 3; padding: 0 1; background: $surface; }
    #main-tabs { height: 1fr; }
    #cpu-summary { height: 2; padding: 0 1; background: $surface; }
    #cpu-tbl { height: 1fr; }
    #usage-scroll { height: 1fr; padding: 1 2; }
    #tbl-container { height: 1fr; layout: vertical; }
    #tbl { height: 1fr; }
    #pending-container { height: auto; max-height: 12; border-top: solid $primary; }
    #pending-tbl { height: auto; max-height: 10; }
    #pending-label { background: $primary; color: $text; padding: 0 1; }
    #search-input { display: none; height: 1; border: none; padding: 0 1; background: $surface; }
    """

    BINDINGS = [
        ("r", "refresh", "Refresh"),
        ("s", "toggle_sort", "Sort"),
        ("S", "reverse_sort", "Rev-sort"),
        ("z", "collapse_all", "Fold all"),
        ("u", "toggle_user_filter", "User"),
        ("p", "toggle_partition_filter", "Partition"),
        ("m", "toggle_my_filter", "Mine"),
        ("i", "toggle_idle_filter", "Free GPUs"),
        ("space", "toggle_collapse", "Collapse"),
        ("d", "toggle_details", "Details"),
        ("j", "cursor_down", "↓"),
        ("k", "cursor_up", "↑"),
        ("slash", "start_search", "Search"),
        ("w", "show_waste", "Waste"),
        ("x", "cancel_job", "Cancel job"),
        ("g", "show_usage", "Usage"),
        ("1", "tab_gpu", "GPU"),
        ("2", "tab_cpu", "CPU"),
        ("3", "tab_usage", "Usage"),
        ("e", "export_json", "Export JSON"),
        ("question_mark", "help", "Help"),
        ("q", "quit", "Quit"),
    ]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static("loading...", id="summary")
        with TabbedContent(initial="pane-gpu", id="main-tabs"):
            with TabPane("GPU [1]", id="pane-gpu"):
                with Vertical(id="tbl-container"):
                    yield DataTable(id="tbl")
                    with Vertical(id="pending-container"):
                        yield Static(" PENDING JOBS ", id="pending-label")
                        yield DataTable(id="pending-tbl")
            with TabPane("CPU [2]", id="pane-cpu"):
                with Vertical():
                    yield Static("", id="cpu-summary")
                    yield DataTable(id="cpu-tbl")
            with TabPane("Usage [3]", id="pane-usage"):
                with VerticalScroll(id="usage-scroll"):
                    yield Static("", id="usage-view")
        yield Input(placeholder="/ filter: node or user (Esc to clear)", id="search-input")
        yield Static("", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.tbl = self.query_one("#tbl", DataTable)
        self.pending_tbl = self.query_one("#pending-tbl", DataTable)
        self.cpu_tbl = self.query_one("#cpu-tbl", DataTable)
        self.cpu_summary = self.query_one("#cpu-summary", Static)
        self.usage_view = self.query_one("#usage-view", Static)
        self.summary_w = self.query_one("#summary", Static)
        self.status_w = self.query_one("#status", Static)

        self.cpu_tbl.add_column("Node", key="c_node", width=12)
        self.cpu_tbl.add_column("State", key="c_state", width=10)
        self.cpu_tbl.add_column("Partition", key="c_part", width=14)
        self.cpu_tbl.add_column("CPU alloc", key="c_cpu", width=34)
        self.cpu_tbl.add_column("Load", key="c_load", width=8)
        self.cpu_tbl.add_column("RAM", key="c_ram", width=22)
        self.cpu_tbl.add_column("CPU users (cores)", key="c_users")
        self.cpu_tbl.cursor_type = "row"
        self.cpu_tbl.zebra_stripes = True
        # NOT os.getlogin(): it raises OSError without a controlling TTY
        # (tmux detach, systemd, nohup)
        try:
            self.current_user = os.environ.get("USER") or getpass.getuser()
        except Exception:
            self.current_user = "user"
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
        self.pending_tbl.add_column("Est.Start", key="p_start")
        self.pending_tbl.cursor_type = "row"
        self.pending_tbl.zebra_stripes = True

        self.refresh_sec = int(os.getenv("SLURM_GPU_TUI_REFRESH_SEC", "3"))
        self.node_timeout = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
        self.max_workers = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))

        self.sort_by = "node"  # "node", "util", "user", "free"
        self.sort_reverse = False
        self.filter_user = ""  # show only this user's jobs ("" = everyone)
        self.filter_partition = ""  # show only nodes in this partition
        self._user_gpu_count: Dict[str, int] = {}
        self._collapsed: set = set()  # node names that are collapsed
        self._last_data_mtime: float | None = None
        self._force_render = False
        self._row_job: Dict[str, str] = {}  # table row key -> jobid for detail popup
        self._pending_user: Dict[str, str] = {}  # pending jobid -> user (for cancel)
        # toast baselines (None = no refresh seen yet)
        self._toast_jobs: Optional[Dict[str, JobInfo]] = None
        self._toast_pending: set = set()
        self._toast_down: Dict[str, bool] = {}
        self._nodes_cache: List[NodeInfo] = []  # last applied nodes (waste view)
        self._jobs_by_id: Dict[str, JobInfo] = {}  # for detail popup scripts
        self._auto_collapsed = False  # big clusters start collapsed, once
        self._last_applied: Optional[Tuple[List[NodeInfo], List[JobInfo], List[PendingJob], str]] = None
        # one collection at a time: exclusive=True only cancels cooperatively,
        # so without this a slow SSH-fallback sweep piles up parallel sweeps
        self._refresh_lock = threading.Lock()

        self._timer: Timer | None = None
        self._reset_timer(self.refresh_sec)
        self.refresh_all()

    def _reset_timer(self, sec: int) -> None:
        if self._timer is not None:
            self._timer.stop()
        self._timer = self.set_interval(sec, self.refresh_all)

    def action_quit(self) -> None:
        cleanup_ssh_pool()
        self.exit()

    def _rerender(self) -> None:
        """Force a re-render even if daemon data is unchanged (UI state changed)."""
        self._force_render = True
        self.refresh_all()

    def action_refresh(self) -> None:
        self._rerender()

    def action_export_json(self) -> None:
        if self._last_applied is None:
            self.status_w.update(Text(" No data to export yet ", style="dim"))
            return
        nodes, jobs, pending, _ = self._last_applied
        snapshot = {
            "ts": datetime.now().isoformat(),
            "nodes": [asdict(n) for n in nodes],
            "jobs": [asdict(j) for j in jobs],
            "pending": [asdict(p) for p in pending],
        }
        out = Path(f"sgpu-export-{datetime.now().strftime('%Y%m%d-%H%M%S')}.json")
        out.write_text(json.dumps(snapshot, indent=2, ensure_ascii=False))
        self.status_w.update(Text(f" Exported → {out} ", style="bold green"))

    def action_toggle_sort(self) -> None:
        choices = ["node", "util", "user", "free"]
        idx = choices.index(self.sort_by)
        self.sort_by = choices[(idx + 1) % len(choices)]
        self.status_w.update(f"Sort by: {self.sort_by}")
        self._rerender()

    def action_reverse_sort(self) -> None:
        self.sort_reverse = not self.sort_reverse
        self.status_w.update(f"Sort by: {self.sort_by}"
                             + (" (reversed)" if self.sort_reverse else ""))
        self._rerender()

    def action_collapse_all(self) -> None:
        gpu_nodes = {n.name for n in self._nodes_cache if n.has_gpu}
        if self._collapsed >= gpu_nodes:
            self._collapsed.clear()
        else:
            self._collapsed = set(gpu_nodes)
        self._rerender()

    def action_toggle_user_filter(self) -> None:
        if self.filter_user:
            self.filter_user = ""
            self.status_w.update("All Jobs")
            self._rerender()
            return
        # Me first, then heaviest GPU users
        entries = [(self.current_user, self._user_gpu_count.get(self.current_user, 0))]
        entries += sorted(
            ((u, c) for u, c in self._user_gpu_count.items() if u != self.current_user),
            key=lambda x: -x[1],
        )

        def _apply_filter(sel: Optional[str]) -> None:
            if sel:
                self.filter_user = sel
                self.status_w.update(f"Filter: {sel}'s jobs (u to clear)")
                self._rerender()

        self.push_screen(UserSelectScreen(entries), _apply_filter)

    def action_toggle_partition_filter(self) -> None:
        """Cycle: all -> partition A -> partition B -> ... -> all."""
        parts: List[str] = []
        for n in self._nodes_cache:
            if not n.has_gpu:
                continue
            for p in (n.partition or "").split(","):
                if p and p not in parts:
                    parts.append(p)
        if not parts:
            return
        order = [""] + sorted(parts)
        cur = order.index(self.filter_partition) if self.filter_partition in order else 0
        self.filter_partition = order[(cur + 1) % len(order)]
        self.status_w.update(f"Partition: {self.filter_partition or 'all'}"
                             + (" (p to cycle)" if self.filter_partition else ""))
        self._rerender()

    def action_toggle_my_filter(self) -> None:
        """Shortcut: filter to my own jobs (same as picking myself under u)."""
        if self.filter_user == self.current_user:
            self.filter_user = ""
            self.status_w.update("All Jobs")
        else:
            self.filter_user = self.current_user
            self.status_w.update(f"My jobs ({self.current_user}) — m to clear")
        self._rerender()

    def _job_under_cursor(self) -> str:
        """jobid of the row under the cursor, in whichever table has focus."""
        tables = [t for t in (self.tbl, self.pending_tbl) if t.has_focus] or [self.tbl]
        for tbl in tables:
            if tbl.row_count == 0:
                continue
            try:
                cell_key = tbl.coordinate_to_cell_key(Coordinate(tbl.cursor_row, 0))
                key = str(cell_key.row_key.value)
            except Exception:
                continue
            if key.startswith("pend_"):
                return key[5:]
            if key.startswith("hdr_"):
                return ""
            return self._row_job.get(key, "")
        return ""

    def action_cancel_job(self) -> None:
        jid = self._job_under_cursor()
        if not jid:
            self.status_w.update(Text(" Move cursor to a job row to cancel ", style="dim"))
            return
        j = self._jobs_by_id.get(jid)
        owner = j.user if j else self._pending_user.get(jid, "")
        name = j.jobname if j else ""
        if owner != self.current_user:
            self.status_w.update(Text(
                f" job {jid} belongs to {owner or '?'} — you can only cancel your own ",
                style="bold red"))
            return

        def _do(confirmed: Optional[bool]) -> None:
            if confirmed:
                self._do_scancel(jid)

        label = f"{jid} ({name})" if name else jid
        self.push_screen(ConfirmScreen(f"Cancel your job {label}?"), _do)

    @work(thread=True)
    def _do_scancel(self, jid: str) -> None:
        # off the UI thread — a slow slurmctld would otherwise freeze the app
        ok, out = run_cmd(f"scancel {jid}")
        if ok:
            self.call_from_thread(self.status_w.update,
                                  Text(f" job {jid} cancelled ", style="bold green"))
            self.call_from_thread(self.action_refresh)
        else:
            self.call_from_thread(self.status_w.update,
                                  Text(f" scancel failed: {out.strip()[:60]} ", style="bold red"))

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
        self._rerender()

    def _setup_columns(self) -> None:
        self.tbl.clear(columns=True)
        self.tbl.add_column("Node", key="node", width=12)
        self.tbl.add_column("State", key="state", width=10)
        self.tbl.add_column("Part", key="part", width=9)
        if not self.show_details:
            # details mode trades the CPU/RAM columns for Temp/Power/Job info
            self.tbl.add_column("CPU a/t", key="cpu", width=8)
            self.tbl.add_column("RAM", key="ram", width=20)
        self.tbl.add_column("GPU#", key="gpu_idx")
        self.tbl.add_column("GPU Name", key="gpu_name")
        self.tbl.add_column("Util", key="util")
        self.tbl.add_column("VRAM", key="vram")
        if self.show_details:
            self.tbl.add_column("T", key="temp")
            self.tbl.add_column("Power", key="power")
        self.tbl.add_column("User", key="user", width=14 if self.show_details else 17)
        if self.show_details:
            self.tbl.add_column("JobID", key="jobid")
            self.tbl.add_column("JobName", key="jobname")
        self.tbl.add_column("Remaining", key="remaining")

    def action_toggle_idle_filter(self) -> None:
        self.idle_filter_only = not self.idle_filter_only
        status = "Nodes with free GPUs only" if self.idle_filter_only else "All Nodes"
        self.status_w.update(status)
        self._rerender()

    def action_toggle_details(self) -> None:
        self.show_details = not self.show_details
        status = "Details: ON" if self.show_details else "Details: OFF"
        self.status_w.update(status)
        self._setup_columns()
        self._rerender()

    def action_help(self) -> None:
        self.push_screen(HelpScreen())

    def action_show_waste(self) -> None:
        self.push_screen(WasteScreen(collect_waste(self._nodes_cache, WASTE_MIN_SEC)))

    def _set_tab(self, pane: str) -> None:
        self.query_one("#main-tabs", TabbedContent).active = pane
        if pane == "pane-gpu":
            self.tbl.focus()
        elif pane == "pane-cpu":
            self.cpu_tbl.focus()

    def action_tab_gpu(self) -> None:
        self._set_tab("pane-gpu")

    def action_tab_cpu(self) -> None:
        self._set_tab("pane-cpu")

    def action_tab_usage(self) -> None:
        self._set_tab("pane-usage")

    def action_show_usage(self) -> None:
        self._set_tab("pane-usage")

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        key = str(event.row_key.value)
        if key.startswith("hdr_"):
            self._show_detail("node", key[4:])
        elif key.startswith("pend_"):
            self._show_detail("job", key[5:])
        else:
            jid = self._row_job.get(key, "")
            if jid:
                self._show_detail("job", jid)

    def _gpu_proc_table(self, node_name: str) -> str:
        """Per-GPU process lines (pid/user/VRAM/job) for the node detail modal."""
        node = next((n for n in self._nodes_cache if n.name == node_name), None)
        if node is None or not node.gpus:
            return ""
        lines = ["", "GPU processes:"]
        for g in node.gpus:
            if not g.pids:
                lines.append(f"  GPU{g.index} ({g.name})  —")
                continue
            for pid in g.pids:
                jid = g.pid_jobid.get(pid, "")
                j = self._jobs_by_id.get(jid)
                # users is a de-duped list, not pid-aligned — the job's owner
                # is exact; fall back to the sole user when unambiguous
                user = j.user if j else (g.users[0] if len(g.users) == 1 else "?")
                vram = g.pid_mem.get(pid, "")
                vram = f"{float(vram) / 1024:.1f}G" if vram.isdigit() else "?"
                job = f"  job {jid}" if jid else ""
                lines.append(f"  GPU{g.index} ({g.name})  pid {pid}  {user}  VRAM {vram}{job}")
        return "\n".join(lines)

    @work(thread=True)
    def _show_detail(self, kind: str, name: str) -> None:
        ok, out = run_cmd(f"scontrol show {kind} {name}")
        if not ok:
            out = f"scontrol failed: {out}"
        if kind == "node":
            out += self._gpu_proc_table(name)
        if kind == "job":
            script, src = "", ""
            # 1) collector-shared script (SHARE_SCRIPTS on a privileged collector)
            j = self._jobs_by_id.get(name)
            if j is not None and j.script:
                script, src = j.script, "shared by collector"
            # 2) own job via scontrol (slurm reports failure as text, exit 0)
            if not script:
                ok2, s = run_cmd(f"scontrol write batch_script {name} -")
                s = s.strip()
                if ok2 and s and not s.startswith("job script retrieval failed"):
                    script, src = s, "scontrol"
            # 3) the submitted file itself, if its permissions allow
            if not script:
                m = re.search(r"Command=(\S+)", out)
                if m and m.group(1) != "(null)":
                    try:
                        script = Path(m.group(1)).read_text(errors="replace")[:16384]
                        src = m.group(1)
                    except OSError:
                        out += "\n\n(batch script not readable: not your job and file permissions deny it)"
            self.call_from_thread(
                self.push_screen,
                DetailScreen(f"{kind} {name}", out, script=script, script_src=src),
            )
            return
        self.call_from_thread(self.push_screen, DetailScreen(f"{kind} {name}", out))

    def _active_scrollable(self):
        """Widget j/k should drive: the focused table, else the active tab's."""
        if self.pending_tbl.has_focus:
            return self.pending_tbl
        active = self.query_one("#main-tabs", TabbedContent).active
        if active == "pane-cpu":
            return self.cpu_tbl
        if active == "pane-usage":
            return self.query_one("#usage-scroll", VerticalScroll)
        return self.tbl

    def action_cursor_down(self) -> None:
        w = self._active_scrollable()
        if isinstance(w, DataTable):
            w.action_scroll_down()
        else:
            w.scroll_down()

    def action_cursor_up(self) -> None:
        w = self._active_scrollable()
        if isinstance(w, DataTable):
            w.action_scroll_up()
        else:
            w.scroll_up()

    def action_start_search(self) -> None:
        w = self.query_one("#search-input", Input)
        w.display = True
        w.focus()

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "search-input":
            self.search_text = event.value.strip().lower()
            self._rerender()

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
                self._rerender()
            return
        ch = getattr(event, "character", None)
        if ch in _JAMO_ACTIONS and not self.query_one("#search-input", Input).has_focus:
            getattr(self, f"action_{_JAMO_ACTIONS[ch]}")()
            event.stop()

    @work(exclusive=True, thread=True)
    def refresh_all(self) -> None:
        if not self._refresh_lock.acquire(blocking=False):
            return  # a collection is still running; this tick just skips
        try:
            self._refresh_all_locked()
        finally:
            self._refresh_lock.release()

    def _refresh_all_locked(self) -> None:
        force = self._force_render
        self._force_render = False

        # Try daemon data first (instant)
        try:
            mtime = _DAEMON_DATA_FILE.stat().st_mtime
        except OSError:
            mtime = None
        if mtime is not None and (time.time() - mtime) <= _DAEMON_MAX_AGE:
            if not force and mtime == self._last_data_mtime:
                return  # data unchanged, keep current render
            daemon_data = read_daemon_data()
            if daemon_data is not None:
                self._last_data_mtime = mtime
                nodes, jobs, pending, daemon_err = daemon_data
                self.call_from_thread(self._apply, nodes, jobs, pending, daemon_err)
                return

        # Fallback: direct collection (2-phase)
        nodes_raw, jobs, pending, node_jobs, gpu_alloc, alloc_user_map, err1 = collect_basic()
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
        apply_gpu_alloc(phase1_nodes, gpu_alloc, jobs, alloc_user_map)
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
            apply_gpu_alloc(phase2_nodes, gpu_alloc, jobs, alloc_user_map)
            self.call_from_thread(self._apply, phase2_nodes, jobs, pending, " | ".join(all_errors) if all_errors else "")

    def _toast_check(self, nodes: List[NodeInfo], jobs: List[JobInfo],
                     pending: List[PendingJob], err: str) -> None:
        """In-TUI toasts: my job started/finished, node down/recovered.
        First refresh only records the baseline."""
        mine_run = {j.jobid: j for j in jobs if j.user == self.current_user}
        mine_pend = {pj.jobid for pj in pending if pj.user == self.current_user}
        # state-string only (not staleness): SSH-fallback renders would
        # otherwise false-alarm "down" while a node is merely slow to poll
        down_now = {n.name: any(s in n.state for s in ("down", "drain", "fail"))
                    for n in nodes}
        # a failed/partial collection yields an empty job list — diffing
        # against it would toast "finished" for every running job
        jobs_ok = not err
        if self._toast_jobs is not None:
            if jobs_ok:
                for jid, j in self._toast_jobs.items():
                    if jid not in mine_run and jid not in mine_pend:
                        self.notify(f"{jid} ({j.jobname}) finished after {j.elapsed}",
                                    title="job done", severity="information", timeout=10)
                for jid in self._toast_pending:
                    if jid in mine_run:
                        self.notify(f"{jid} ({mine_run[jid].jobname}) started",
                                    title="job started", severity="information", timeout=8)
            for name, down in down_now.items():
                was = self._toast_down.get(name)
                if was is not None and down != was:
                    if down:
                        self.notify(f"{name}: {next(n.state for n in nodes if n.name == name)}",
                                    title="node down", severity="error", timeout=15)
                    else:
                        self.notify(f"{name} back in service",
                                    title="node recovered", severity="information", timeout=8)
        if jobs_ok:
            self._toast_jobs = mine_run
            self._toast_pending = mine_pend
        self._toast_down = down_now

    def _node_visible(self, node: NodeInfo, node_classes: Dict[str, List[str]]) -> bool:
        """GPU-tab filters: user/partition filters, free-GPU filter, live search."""
        if self.filter_partition:
            node_parts = {p for p in (node.partition or "").split(",") if p}
            node_parts.update(j.partition for j in node.jobs if j.partition)
            if self.filter_partition not in node_parts:
                return False
        if self.filter_user:
            fu = self.filter_user
            has_user = any(fu in g.users or fu == g.alloc_user for g in node.gpus)
            has_user = has_user or any(j.user == fu for j in node.jobs)
            if not has_user:
                return False
        if self.idle_filter_only and "free" not in node_classes[node.name]:
            return False
        if self.search_text:
            node_users = set()
            for g in node.gpus:
                node_users.update(g.users)
                if g.alloc_user:
                    node_users.add(g.alloc_user)
            for j in node.jobs:
                node_users.add(j.user)
            if (self.search_text not in node.name.lower() and
                    not any(self.search_text in u.lower() for u in node_users)):
                return False
        return True

    def on_tabbed_content_activated(self, event) -> None:
        # tabs render lazily — repaint the newly shown pane with current data
        if getattr(self, "_timer", None) is not None:  # fires during compose too
            self._rerender()

    def _apply(self, nodes: List[NodeInfo], jobs: List[JobInfo], pending: List[PendingJob], err: str) -> None:
        self._toast_check(nodes, jobs, pending, err)

        self._nodes_cache = nodes
        self._jobs_by_id = {j.jobid: j for j in jobs}
        self._last_applied = (nodes, jobs, pending, err)

        # Big clusters: start with every node collapsed (one line per node),
        # Space expands. Only on first data, never after user interaction.
        if not self._auto_collapsed:
            self._auto_collapsed = True
            limit = int(os.getenv("SLURM_GPU_TUI_AUTO_COLLAPSE_NODES", "12"))
            gpu_nodes_n = sum(1 for n in nodes if n.has_gpu)
            if gpu_nodes_n >= limit:
                self._collapsed = {n.name for n in nodes if n.has_gpu}

        # Pre-classify every GPU (header strips, FREE chip, sorting, filters)
        node_classes: Dict[str, List[str]] = {
            n.name: [classify_gpu(g) for g in n.gpus] for n in nodes
        }

        # Sorting nodes logic (simplistic)
        if self.sort_by == "util":
            # Sort by max util on node
            nodes.sort(key=lambda n: max([float(g.util or 0) for g in n.gpus] + [0]), reverse=True)
        elif self.sort_by == "user":
            # Nodes with current user first
            nodes.sort(key=lambda n: any(self.current_user in g.users for g in n.gpus), reverse=True)
        elif self.sort_by == "free":
            nodes.sort(key=lambda n: node_classes[n.name].count("free"), reverse=True)
        else:
            nodes.sort(key=lambda n: n.name)
        if self.sort_reverse:
            nodes.reverse()

        visible = [n for n in nodes if n.has_gpu and self._node_visible(n, node_classes)]

        # Summary stats over every visible GPU (collapse-independent —
        # row building below skips collapsed nodes' GPU rows)
        total_gpus = 0
        busy_gpus = 0
        partition_gpu_stats: Dict[str, List[int]] = {}  # partition -> [busy, total]
        for node in visible:
            node_partition = node.partition or (node.jobs[0].partition if node.jobs else "")
            parts_sorted = sorted(
                (p for p in node_partition.split(",") if p),
                key=lambda p: ("cpu" in p.lower(), p),
            )
            stat_key = node.jobs[0].partition if node.jobs else (parts_sorted[0] if parts_sorted else "")
            for gpu in node.gpus:
                total_gpus += 1
                stats = partition_gpu_stats.setdefault(stat_key, [0, 0])
                stats[1] += 1
                try:
                    if float(gpu.util) > 5:
                        busy_gpus += 1
                        stats[0] += 1
                except (ValueError, TypeError):
                    pass

        active_pane = self.query_one("#main-tabs", TabbedContent).active
        if active_pane == "pane-gpu":
            self._apply_gpu_tab(visible, node_classes, pending)
        elif active_pane == "pane-cpu":
            self._apply_cpu_tab(nodes)
        elif active_pane == "pane-usage":
            self.usage_view.update(render_usage())

        self._apply_summary(nodes, jobs, pending, err, node_classes,
                            total_gpus, busy_gpus, partition_gpu_stats)

    def _apply_gpu_tab(self, visible: List[NodeInfo], node_classes: Dict[str, List[str]],
                       pending: List[PendingJob]) -> None:
        saved_row = self.tbl.cursor_row
        saved_col = self.tbl.cursor_column
        saved_scroll_x = self.tbl.scroll_x
        saved_scroll_y = self.tbl.scroll_y
        saved_key = None
        try:
            cell_key = self.tbl.coordinate_to_cell_key(Coordinate(saved_row, 0))
            saved_key = cell_key.row_key.value
        except Exception:
            pass

        self.tbl.clear()
        self.pending_tbl.clear()
        self._row_job.clear()

        _HDR_BG = "on #0d1f0d"  # dark green tint for node header rows

        for node in visible:
            node_partition = node.partition or (node.jobs[0].partition if node.jobs else "")
            # GPU-ish partitions first so "cpu_only" doesn't hog the display
            parts_sorted = sorted(
                (p for p in node_partition.split(",") if p),
                key=lambda p: ("cpu" in p.lower(), p),
            )

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
                "unknown": "~err",
            }
            nname = Text(f"{arrow} {node.name}", style="bold white")
            if node.stale:
                label = _ERROR_LABELS.get(node.error_kind, "~stale")
                nname.append(f" {label}", style="dim yellow")

            # Partition cell: first (GPU-ish) partition + "+n" beats truncation
            part_disp = parts_sorted[0] + (f"+{len(parts_sorted) - 1}" if len(parts_sorted) > 1 else "") if parts_sorted else ""

            # Per-GPU glyph strip + waste/free counts — the node summary
            # stays informative even when the node is collapsed
            classes = node_classes[node.name]
            strip = gpu_strip(classes)
            n_busy, n_free = classes.count("busy"), classes.count("free")
            n_parked, n_rsv = classes.count("parked"), classes.count("idle")
            use_txt = Text()
            if n_busy:
                use_txt.append(f"{n_busy} busy", style="green")
            if n_free:
                use_txt.append("  " if n_busy else "")
                use_txt.append(f"{n_free} free", style="bold cyan")
            waste_txt = Text()
            n_rogue = classes.count("rogue")
            if n_rogue:
                waste_txt.append(f"{n_rogue} rogue", style="bold red")
            if n_parked:
                waste_txt.append("  " if waste_txt else "")
                waste_txt.append(f"{n_parked} parked", style="blue")
            if n_rsv:
                waste_txt.append("  " if waste_txt else "")
                waste_txt.append(f"{n_rsv} idle", style="yellow")

            if self.show_details:
                hdr_cells = [
                    nname, state_cell(node.state), Text(part_disp, style="cyan"),
                    Text(""), strip, use_txt, waste_txt,
                    Text(""), Text(""), Text(""), Text(""),
                    Text(""), Text(""),
                ]
            else:
                hdr_cells = [
                    nname, state_cell(node.state), Text(part_disp, style="cyan"),
                    cpu_text, mem_cell(node),
                    Text(""), strip, use_txt, waste_txt, Text(""), Text(""),
                ]
            for cell in hdr_cells:
                cell.stylize(_HDR_BG)
            self.tbl.add_row(*hdr_cells, key=f"hdr_{node.name}")

            if is_collapsed:
                pass
            elif node.gpus:
                for gpu_i, gpu in enumerate(node.gpus):
                    user = ""
                    jobid = ""
                    jobname = ""
                    elapsed = ""
                    is_me = False
                    reserved_idle = False
                    matched_job = None
                    if gpu.users:
                        user = ",".join(gpu.users)
                        is_me = self.current_user in gpu.users
                    elif gpu.alloc_user:
                        # Allocated by SLURM but no GPU process — reserved, sitting
                        # idle. On stale placeholder rows process info is unknown,
                        # so only claim idle when a tracked age says so.
                        user = gpu.alloc_user
                        reserved_idle = gpu.idle_sec > 0 or not node.stale
                        is_me = gpu.alloc_user == self.current_user
                    # Exact job match via SLURM allocation, fallback to user match
                    if gpu.alloc_jobid:
                        for j in node.jobs:
                            if j.jobid == gpu.alloc_jobid:
                                matched_job = j
                                break
                    if matched_job is None and gpu.users:
                        for j in node.jobs:
                            if j.user in gpu.users:
                                matched_job = j
                                break
                    if matched_job:
                        jobid, jobname, elapsed = matched_job.jobid, matched_job.jobname, matched_job.elapsed

                    vcell = vram_cell(gpu.mem_used, gpu.mem_total)
                    if classes[gpu_i] == "parked":
                        age = fmt_span(gpu.parked_sec)
                        vcell = vcell + Text(f" parked {age}".rstrip(), style="bold blue")
                    gutter = [Text(""), Text(""), Text("")]  # node, state, part
                    if not self.show_details:
                        gutter += [Text(""), Text("")]       # cpu, ram
                    row_cells = gutter + [
                        Text(f"  {gpu.index}", style="bold"),
                        Text(gpu.name) if gpu.name else Text("?", style="bright_black"),
                        util_cell(gpu.util),
                        vcell,
                    ]
                    if self.show_details:
                        row_cells.append(temp_cell(gpu.temp))
                        row_cells.append(power_cell(gpu.power, gpu.power_cap))
                    if user and classes[gpu_i] == "rogue":
                        # A same-user job on this node with no gres means the
                        # job simply skipped --gres; otherwise it's a raw
                        # process outside SLURM entirely
                        tag = "!gres" if (matched_job and matched_job.gpu_count == 0) else "!slurm"
                        user_cell = Text(user, style="bold red")
                        user_cell.append(f" {tag}", style="bold red reverse")
                        row_cells.append(user_cell)
                    elif user and reserved_idle:
                        user_cell = Text(f"{user} ", style="magenta")
                        age = fmt_idle_age(gpu.idle_sec)
                        user_cell.append(age, style="bold yellow" if gpu.idle_sec >= 3600 else "dim yellow")
                        row_cells.append(user_cell)
                    elif user:
                        row_cells.append(Text(user, style="bold magenta"))
                    else:
                        row_cells.append(Text(""))
                    if self.show_details:
                        row_cells.append(Text(jobid, style="dim") if jobid else Text(""))
                        row_cells.append(Text(ellipsize(jobname, 16)) if jobname else Text(""))
                    row_cells.append(remaining_cell(elapsed, matched_job.time_limit) if matched_job else Text("", style="dim"))
                    if is_me:
                        highlight_row(row_cells)
                    gpu_key = f"gpu_{node.name}_{gpu.index}"
                    detail_jid = jobid or gpu.alloc_jobid
                    if detail_jid:
                        self._row_job[gpu_key] = detail_jid
                    self.tbl.add_row(*row_cells, key=gpu_key)

            elif node.jobs:
                for j in node.jobs:
                    is_me = (j.user == self.current_user)
                    if self.filter_user and j.user != self.filter_user:
                        continue
                    gutter = [Text(""), Text(""), Text("")]
                    if not self.show_details:
                        gutter += [Text(""), Text("")]
                    row_cells = gutter + [
                        Text(""), Text("-", style="dim"),
                        Text("-", style="dim"), Text("-", style="dim"),
                    ]
                    if self.show_details:
                        row_cells.append(Text("-", style="dim"))
                        row_cells.append(Text("-", style="dim"))
                    row_cells.append(Text(j.user, style="bold magenta"))
                    if self.show_details:
                        row_cells.append(Text(j.jobid, style="dim"))
                        row_cells.append(Text(ellipsize(j.jobname, 16)))
                    row_cells.append(remaining_cell(j.elapsed, j.time_limit))
                    if is_me:
                        highlight_row(row_cells)
                    job_key = f"job_{node.name}_{j.jobid}"
                    self._row_job[job_key] = j.jobid
                    self.tbl.add_row(*row_cells, key=job_key)

        # ── Pending Jobs Table (lives in the GPU pane) ──
        self.query_one("#pending-container").display = bool(pending)
        self._pending_user = {pj.jobid: pj.user for pj in pending}
        for pj in pending:
            is_me = (pj.user == self.current_user)
            if self.filter_user and pj.user != self.filter_user:
                continue
            reason_style = "bold red" if pj.reason == "Resources" else "yellow" if pj.reason == "Priority" else "dim"
            gpu_txt = f"x{pj.gpu_count}" if pj.gpu_count else "-"
            row_cells = [
                Text(pj.jobid, style="dim"),
                Text(pj.user, style="bold magenta"),
                Text(gpu_txt, style="bold"),
                Text(pj.partition, style="dim"),
                Text(ellipsize(pj.jobname, 24)),
                Text(pj.reason, style=reason_style),
                Text(pj.priority, style="dim"),
                Text(fmt_start_time(pj.start_time), style="cyan"),
            ]
            if is_me:
                highlight_row(row_cells)
            self.pending_tbl.add_row(*row_cells, key=f"pend_{pj.jobid}")

        if self.tbl.row_count > 0:
            row = None
            if saved_key is not None:
                try:
                    row = self.tbl.get_row_index(saved_key)
                except Exception:
                    row = None
            if row is None:
                row = min(saved_row, self.tbl.row_count - 1)
            self.tbl.move_cursor(row=row, column=saved_col, animate=False)
        self.tbl.scroll_to(x=saved_scroll_x, y=saved_scroll_y, animate=False)

    def _apply_cpu_tab(self, nodes: List[NodeInfo]) -> None:
        # ── CPU tab (all nodes, CPU-only included) ──
        self.cpu_tbl.clear()
        cpu_rows = []
        cluster_cores = cluster_alloc = 0
        all_user_cores: Dict[str, int] = {}
        for node in nodes:
            try:
                total_c = float(node.cpus)
                alloc_c = float(node.cpu_alloc or 0)
                cpct = alloc_c / total_c if total_c > 0 else 0.0
            except (ValueError, TypeError):
                total_c, alloc_c, cpct = 0.0, 0.0, 0.0
            cluster_cores += int(total_c)
            cluster_alloc += int(alloc_c)
            user_cores: Dict[str, int] = {}
            for j in node.jobs:
                if j.cpu_count:
                    user_cores[j.user] = user_cores.get(j.user, 0) + j.cpu_count
                    all_user_cores[j.user] = all_user_cores.get(j.user, 0) + j.cpu_count
            cpu_rows.append((cpct, node, user_cores, total_c))

        for cpct, node, user_cores, total_c in sorted(cpu_rows, key=lambda r: (-r[0], r[1].name)):
            cbar = make_bar(cpct, width=20)
            cbar.append(f" {node.cpu_alloc or 0}/{node.cpus} {cpct:.0%}", style=f"bold {pct_color(cpct)}")
            try:
                load = float(node.cpu_load)
                load_style = "red" if total_c and load > total_c else "yellow" if total_c and load > total_c * 0.8 else "green"
                load_cell = Text(f"{load:.1f}", style=load_style)
            except (ValueError, TypeError):
                load_cell = Text(node.cpu_load or "-", style="bright_black")
            users_txt = Text()
            for u, c in sorted(user_cores.items(), key=lambda x: -x[1]):
                users_txt.append(f" {u}", style="bold magenta" if u == self.current_user else "magenta")
                users_txt.append(f":{c}", style="bold")
            cparts = sorted((p for p in node.partition.split(",") if p),
                            key=lambda p: ("cpu" in p.lower(), p))
            marker = "" if node.has_gpu else " ·cpu"
            nname_cell = Text(node.name, style="bold cyan")
            if marker:
                nname_cell.append(marker, style="dim")
            self.cpu_tbl.add_row(
                nname_cell, state_cell(node.state),
                Text(ellipsize(",".join(cparts), 14), style="cyan"),
                cbar, load_cell, mem_cell(node), users_txt,
                key=f"hdr_{node.name}",
            )

        cpu_sum = Text()
        cpu_sum.append(" CPU ", style="bold white on dark_green")
        cpct_all = cluster_alloc / cluster_cores if cluster_cores else 0
        cpu_sum.append(f" {cluster_alloc}/{cluster_cores} cores ", style="bold")
        cpu_sum.append_text(make_bar(cpct_all, width=20))
        cpu_sum.append(f" {cpct_all:.0%}\n", style=f"bold {pct_color(cpct_all)}")
        cpu_sum.append(" TOP ", style="bold white on purple")
        for u, c in sorted(all_user_cores.items(), key=lambda x: -x[1])[:10]:
            cpu_sum.append(f" {u}", style="bold yellow underline" if u == self.current_user else "bold magenta")
            cpu_sum.append(f":{c}", style="bold")
        self.cpu_summary.update(cpu_sum)

    def _apply_summary(self, nodes: List[NodeInfo], jobs: List[JobInfo],
                       pending: List[PendingJob], err: str,
                       node_classes: Dict[str, List[str]],
                       total_gpus: int, busy_gpus: int,
                       partition_gpu_stats: Dict[str, List[int]]) -> None:
        user_gpu_count: Dict[str, int] = {}
        for j in jobs:
            if j.gpu_count > 0:
                user_gpu_count[j.user] = user_gpu_count.get(j.user, 0) + j.gpu_count
        self._user_gpu_count = user_gpu_count

        ts = datetime.now().strftime("%H:%M:%S")
        sort_label = Text(f" SORT:{self.sort_by.upper()}{'↑' if self.sort_reverse else ''} ",
                          style="bold white on #444444")

        summary = Text()
        summary.append(" GPU ", style="bold white on dark_green")
        summary.append(f" {busy_gpus}/{total_gpus} active  ", style="bold")
        # FREE chip answers "where can I submit" without scanning rows.
        # Computed over ALL nodes (pre-filter) so filters don't hide capacity.
        free_by_node = sorted(
            ((name, cl.count("free")) for name, cl in node_classes.items() if cl.count("free")),
            key=lambda x: -x[1],
        )
        total_free = sum(c for _, c in free_by_node)
        summary.append(" FREE ", style="bold black on cyan")
        summary.append(f" {total_free} ", style="bold cyan")
        for nn, cnt in free_by_node[:4]:
            summary.append(f" {nn}×{cnt}", style="cyan")
        if len(free_by_node) > 4:
            summary.append(" …", style="dim cyan")
        summary.append("  ")
        # Data-source health. CPU-only SSH is normal telemetry for live RAM;
        # only a GPU node on SSH means the push agent fell back.
        (n_agent, n_gpu_fallback, n_cpu_push,
         n_cpu_poll, n_stale) = _node_source_counts(nodes)
        n_rogue_total = sum(cl.count("rogue") for cl in node_classes.values())
        if n_rogue_total:
            summary.append(" ROGUE ", style="bold white on red")
            summary.append(f" {n_rogue_total} ", style="bold red")
        if n_agent or n_gpu_fallback or n_cpu_push or n_cpu_poll or n_stale:
            summary.append(" SRC ", style="bold white on grey37")
            summary.append(f" agent:{n_agent}", style="green" if n_agent else "dim")
            if n_gpu_fallback:
                summary.append(f" fallback:{n_gpu_fallback}", style="yellow")
            if n_cpu_push:
                summary.append(f" cpu-push:{n_cpu_push}", style="green")
            if n_cpu_poll:
                summary.append(f" cpu-poll:{n_cpu_poll}", style="cyan")
            if n_stale:
                summary.append(f" stale:{n_stale}", style="bold red")
            summary.append("  ")
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
        summary.append_text(sort_label)
        if self.filter_user:
            summary.append(f" USER:{self.filter_user} ", style="bold white on #666600")
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
        # compact legend for the glyph/marker vocabulary
        summary.append("\n ")
        for glyph, label, style in (
            ("█", "busy", "green"), ("▅", "parked", "blue"), ("▂", "rsv-idle", "yellow"),
            ("▁", "free", "bold cyan"), ("!", "rogue", "bold red"), ("?", "no-data", "bright_black"),
        ):
            summary.append(glyph, style=style)
            summary.append(f" {label}  ", style="dim")

        self.summary_w.update(summary)

        if err:
            self.status_w.update(Text(f" WARN: {err} ", style="bold yellow on dark_red"))
        else:
            self.status_w.update(Text(f" OK [{ts}] ", style="dim"))




def main():
    """Entry-point shim: real CLI lives in sgpu.cli (kept for old venvs whose
    console script still imports sgpu.tui:main)."""
    from .cli import main as _main
    _main()
