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
    WASTE_MIN_SEC, collect_waste, ellipsize, fmt_idle_age,
    fmt_span, fmt_start_time, gpu_strip, highlight_row,
    power_cell, remaining_cell, state_cell, temp_cell, util_cell,
    vram_cell,
    gpu_health_status, node_gpu_classes, job_resource_summary,
    pending_reason, pending_wait_seconds, pending_explanation,
)
from .common import (
    JobInfo, NodeInfo, NodeSSHResult, PendingJob,
    apply_gpu_alloc, build_nodes, cleanup_ssh_pool, collect_basic,
    collect_node_data_parallel, from_dict, job_log_spec, node_from_dict,
    read_job_log, run_cmd,
)
from .runtime import default_data_dir
from .screens import (
    _JAMO_ACTIONS, ConfirmScreen, DetailScreen, HelpScreen, HistoryScreen,
    UserSelectScreen, WasteScreen,
)
from .usage import render_usage
from .telemetry import ObservationHistory, metric_number as _metric_number
from .node_telemetry import TELEMETRY_MAX_AGE


# ── Daemon data reader ────────────────────────────────────────────────────

_DAEMON_DATA_FILE = default_data_dir() / "data.json"
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
    jobs = [from_dict(JobInfo, j) for j in raw.get("jobs") or []]
    pending = [from_dict(PendingJob, p) for p in raw.get("pending") or []]
    stale_nodes = set(raw.get("stale_nodes") or [])
    nodes = [node_from_dict(n) for n in raw.get("nodes") or []]
    for n in nodes:
        # stale is published both per-node and as a roster; keep honouring both
        n.stale = n.stale or n.name in stale_nodes
    return nodes, jobs, pending, raw.get("errors", "")


def _node_source_counts(nodes: List[NodeInfo]) -> Tuple[int, int, int, int, int]:
    """GPU push/fallback, CPU telemetry polling, and all stale nodes."""
    agent = sum(1 for n in nodes if n.has_gpu and n.source == "agent")
    gpu_fallback = sum(1 for n in nodes if n.has_gpu and n.source == "ssh")
    cpu_push = sum(1 for n in nodes if not n.has_gpu and n.source == "agent")
    cpu_poll = sum(1 for n in nodes if not n.has_gpu and n.source == "ssh")
    stale = sum(1 for n in nodes if n.source == "stale" or (n.stale and not n.source))
    return agent, gpu_fallback, cpu_push, cpu_poll, stale


def _collector_job_detail(
    job: JobInfo | None, pending: PendingJob | None,
) -> str:
    """Scheduler detail published by the root collector, when available."""
    detail = job.detail if job else pending.detail if pending else ""
    return detail + "\n\n(scheduler detail shared by root collector)" if detail else ""


def _slurm_control_jobid(jobid: str) -> str:
    """Normalize a compressed pending-array display ID for scontrol."""
    return jobid.split("_", 1)[0] if "_[" in jobid else jobid


def _job_log_views(
    detail: str, job: JobInfo | None,
) -> Tuple[str, str, str, str]:
    """Resolve stdout/stderr views, including a shared merged stream."""
    real_out, real_err, merged = job_log_spec(detail)
    shared_out = job.log_out if job else ""
    shared_err = job.log_err if job else ""
    if job and job.log_status.get("err") == "merged":
        merged = True
    stdout_text, stdout_path = read_job_log(real_out, shared_out)
    if merged:
        stderr_text, stderr_path = read_job_log(real_out, shared_out)
        if stderr_text:
            stderr_text = "(stderr is merged into stdout)\n\n" + stderr_text
    else:
        stderr_text, stderr_path = read_job_log(real_err, shared_err)
    return stdout_text, stdout_path, stderr_text, stderr_path



# ── TUI App ───────────────────────────────────────────────────────────────

class SlurmGpuTui(App):
    TITLE = "SLURM GPU Monitor"
    CSS = """
    Screen { layout: vertical; }
    #status { height: 1; background: $surface; color: $text-muted; padding: 0 1; }
    #summary { height: 2; padding: 0 1; background: $surface; }
    #main-tabs { height: 1fr; }
    #cpu-summary { height: 2; padding: 0 1; background: $surface; }
    #cpu-tbl { height: 1fr; }
    #jobs-tbl { height: 1fr; }
    #usage-scroll { height: 1fr; padding: 1 2; }
    #tbl-container { height: 1fr; layout: vertical; }
    #tbl { height: 1fr; }
    #trend { height: 2; padding: 0 1; }
    #pending-container { height: auto; max-height: 9; border-top: solid $primary; }
    #pending-tbl { height: auto; max-height: 7; }
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
        ("f", "show_free", "Capacity"),
        ("space", "toggle_collapse", "Collapse"),
        ("d", "toggle_details", "Details"),
        ("j", "cursor_down", "↓"),
        ("k", "cursor_up", "↑"),
        ("slash", "start_search", "Search"),
        ("w", "show_waste", "Waste"),
        ("h", "show_history", "History"),
        ("n", "watch_job", "Watch job"),
        ("x", "cancel_job", "Cancel job"),
        ("g", "show_usage", "Usage"),
        ("1", "tab_gpu", "GPU"),
        ("2", "tab_cpu", "CPU"),
        ("3", "tab_usage", "Usage"),
        ("4", "tab_jobs", "Jobs"),
        ("v", "toggle_pending", "Pending"),
        ("t", "toggle_trend", "Trend"),
        ("e", "export_json", "Export JSON"),
        ("question_mark", "help", "Help"),
        ("q", "quit", "Quit"),
    ]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static("loading...", id="summary")
        with TabbedContent(initial="pane-gpu", id="main-tabs"):
            with TabPane("GPU [1]", id="pane-gpu"):
                yield Static("", id="trend")
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
            with TabPane("Jobs [4]", id="pane-jobs"):
                yield DataTable(id="jobs-tbl")
        yield Input(placeholder="/ filter: node or user (Esc to clear)", id="search-input")
        yield Static("", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.tbl = self.query_one("#tbl", DataTable)
        self.pending_tbl = self.query_one("#pending-tbl", DataTable)
        self.cpu_tbl = self.query_one("#cpu-tbl", DataTable)
        self.jobs_tbl = self.query_one("#jobs-tbl", DataTable)
        self.cpu_summary = self.query_one("#cpu-summary", Static)
        self.usage_view = self.query_one("#usage-view", Static)
        self.summary_w = self.query_one("#summary", Static)
        self.status_w = self.query_one("#status", Static)
        self.trend_w = self.query_one("#trend", Static)
        self._history = ObservationHistory()
        self._compact = self.size.width < 130
        self._pending_expanded = False
        self._show_trend = False
        self.trend_w.display = False
        self.pending_tbl.display = False

        self.cpu_tbl.cursor_type = "row"
        self.cpu_tbl.zebra_stripes = True
        self.jobs_tbl.cursor_type = "row"
        self.jobs_tbl.zebra_stripes = True
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
        self._jobs_row_job: Dict[str, str] = {}
        self._pending_user: Dict[str, str] = {}  # pending jobid -> user (for cancel)
        # toast baselines (None = no refresh seen yet)
        self._toast_jobs: Optional[Dict[str, JobInfo]] = None
        self._toast_pending: set = set()
        self._toast_down: Dict[str, bool] = {}
        # jobs watched with `n`: jobid -> {user, jobname, state} (in-session)
        self._watched: Dict[str, Dict[str, str]] = {}
        self._nodes_cache: List[NodeInfo] = []  # last applied nodes (waste view)
        self._jobs_by_id: Dict[str, JobInfo] = {}  # for detail popup scripts
        self._pending_by_id: Dict[str, PendingJob] = {}
        # DataTable.clear()+add_row() is substantially more expensive than the
        # parsing and summary work on large clusters.  Keep a value-only model
        # of the last rendered pane so the common case (a fresh collector file
        # whose visible values did not change) can leave the table untouched.
        # Reuse unchanged node rows; update cells when row/column keys match.
        # Structural or ordering changes rebuild while retaining cursor/scroll.
        self._last_gpu_view_signature: tuple | None = None
        self._table_models: dict = {}
        self._gpu_node_rows_cache: dict = {}
        self._last_cpu_view_signature: tuple | None = None
        self._auto_collapsed = False  # big clusters start collapsed, once
        self._last_applied: Optional[Tuple[List[NodeInfo], List[JobInfo], List[PendingJob], str]] = None
        # one collection at a time: exclusive=True only cancels cooperatively,
        # so without this a slow SSH-fallback sweep piles up parallel sweeps
        self._refresh_lock = threading.Lock()

        self._timer: Timer | None = None
        self._search_timer: Timer | None = None
        self._reset_timer(self.refresh_sec)
        self.refresh_all()
        # Initial TabPane layout can leave the hidden search Input focused.
        self.call_after_refresh(self._focus_initial_table)

    def _focus_initial_table(self) -> None:
        search = self.query_one("#search-input", Input)
        if search.has_focus and not search.display:
            self._focus_tab()

    def _reset_timer(self, sec: int) -> None:
        if self._timer is not None:
            self._timer.stop()
        self._timer = self.set_interval(sec, self.refresh_all)

    def action_quit(self) -> None:
        cleanup_ssh_pool()
        self.exit()

    def _rerender(self) -> None:
        """Repaint cached data after a UI-only state change."""
        if self._last_applied is not None:
            self._apply(*self._last_applied)
            return
        self._force_render = True
        self.refresh_all()

    def action_refresh(self) -> None:
        self._force_render = True
        self.refresh_all()

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
        tables = [t for t in (self.tbl, self.pending_tbl, self.jobs_tbl) if t.has_focus] or [self._active_scrollable()]
        for tbl in tables:
            if not isinstance(tbl, DataTable):
                continue
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
            return self._jobs_row_job.get(key, self._row_job.get(key, ""))
        return ""

    def action_show_history(self) -> None:
        self.status_w.update(Text(" loading job history… ", style="dim"))
        self._open_history()

    @work(thread=True)
    def _open_history(self) -> None:
        days = 7
        ok, out = run_cmd(
            f"sacct -u {self.current_user} -X --noheader --parsable2 "
            "--format=JobID,JobName,State,ExitCode,Elapsed,End,Partition,AllocTRES "
            f"-S now-{days}days", timeout=30)
        rows: List[dict] = []
        if ok:
            for line in out.splitlines():
                p = line.split("|")
                if len(p) != 8:
                    continue
                mg = re.search(r"gres/gpu[^=]*=(\d+)", p[7])
                rows.append({
                    "jobid": p[0], "name": p[1], "state": p[2].split()[0],
                    "exit": p[3], "elapsed": p[4],
                    "end": p[5].replace("T", " "), "part": p[6],
                    "gpus": int(mg.group(1)) if mg else 0,
                })
            rows.reverse()  # newest first
        self.call_from_thread(self.push_screen,
                              HistoryScreen(self.current_user, days, rows,
                                            error="" if ok else out.strip()[:120]))
        self.call_from_thread(self.status_w.update, Text(""))

    def action_watch_job(self) -> None:
        """Watch any job: toast when it starts / ends (n again unwatches)."""
        jid = self._job_under_cursor()
        if not jid:
            self.status_w.update(Text(" Move cursor to a job row to watch ", style="dim"))
            return
        if jid in self._watched:
            del self._watched[jid]
            self.status_w.update(Text(f" stopped watching job {jid} ", style="dim"))
            return
        j = self._jobs_by_id.get(jid)
        if j is not None:
            user, name, state = j.user, j.jobname, "running"
        else:
            user, name, state = self._pending_user.get(jid, "?"), "", "pending"
        self._watched[jid] = {"user": user, "jobname": name, "state": state}
        label = f"{jid} ({name})" if name else jid
        self.status_w.update(Text(
            f" watching {state} job {label} of {user} — toast when it ends ",
            style="bold cyan"))

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
        compact = self._compact
        columns = [("Node / GPU", "node", 10 if compact else 14)]
        if not compact:
            columns.append(("Model", "gpu_name", 13))
        columns += [("Util", "util", 5 if compact else 15),
                    ("VRAM", "vram", 11 if compact else 26),
                    ("User / job", "user", 18), ("Left", "remaining", 8),
                    ("Health", "health", 10)]
        if self.show_details:
            columns += [("Temp", "temp", 9), ("Power", "power", 18),
                        ("JobID", "jobid", 10), ("JobName", "jobname", 16)]
        cpu = [("Node", "c_node", 12), ("Alloc C", "c_cpu", 9),
               ("CPU%", "c_actual", 5), ("Load", "c_load", 5),
               ("RAM", "c_mem", 11), ("PSI C/M/I", "c_psi", 11),
               ("Users", "c_users", 9 if compact else 20)]
        if not compact:
            cpu.insert(1, ("State", "c_state", 10))
            cpu.insert(2, ("Part", "c_part", 14))
        pend = [("JobID", "p_jobid", 10), ("User", "p_user", 9),
                ("GPU", "p_gpu", 3), ("Reason", "p_reason", 19 if compact else 26),
                ("Wait", "p_wait", 6), ("Est.Start", "p_start", 15)]
        if not compact:
            pend[3:3] = [("Part", "p_part", 10), ("Name", "p_name", 16)]
            pend.insert(-2, ("Priority", "p_pri", 8))
        jobs = [("JobID", "j_id", 9), ("User", "j_user", 8),
                ("State", "j_state", 4), ("GPU", "j_gpu", 3),
                ("CPU a/r", "j_cpu", 8), ("RAM a/r", "j_mem", 11),
                ("Left/wait", "j_span", 8), ("Coverage / reason", "j_note", 10 if compact else 22)]
        if not compact:
            jobs.insert(-2, ("PID VRAM", "j_vram", 10))
        if self.show_details:
            jobs += [("Nodes", "j_nodes", 18), ("Name", "j_name", 20)]
        for table, specs in ((self.tbl, columns), (self.cpu_tbl, cpu),
                             (self.pending_tbl, pend), (self.jobs_tbl, jobs)):
            if table.row_count:
                views = getattr(self, "_column_views", {})
                key = table.coordinate_to_cell_key(Coordinate(table.cursor_row, 0)).row_key
                views[table.id] = (table.cursor_row, table.cursor_column, table.scroll_x, table.scroll_y, key)
                self._column_views = views
            table.clear(columns=True)
            for label, key, width in specs:
                table.add_column(label, key=key, width=width)
        self._last_gpu_view_signature = self._last_cpu_view_signature = None
        self._gpu_node_rows_cache = {}

    def on_resize(self, event) -> None:
        if not hasattr(self, "_last_applied"):
            return
        compact = event.size.width < 130
        if compact != self._compact:
            self._compact = compact
            self._setup_columns()
        if self._last_applied is not None:
            self._rerender()

    def action_toggle_pending(self) -> None:
        self._pending_expanded = not self._pending_expanded
        self.pending_tbl.display = self._pending_expanded
        if not self._pending_expanded and self.pending_tbl.has_focus:
            self.tbl.focus()
        self._rerender()

    def action_toggle_trend(self) -> None:
        self._show_trend = not self._show_trend
        self.trend_w.display = self._show_trend
        self._rerender()

    def on_click(self, event) -> None:
        if getattr(event.widget, "id", None) == "pending-label":
            self.action_toggle_pending()

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

    def action_show_free(self) -> None:
        from collections import defaultdict
        grouped = defaultdict(lambda: defaultdict(int))
        classes = {node.name: node_gpu_classes(node) for node in self._nodes_cache}
        for node in self._nodes_cache:
            if not self._node_visible(node, classes):
                continue
            for gpu, kind in zip(node.gpus, classes[node.name], strict=True):
                if kind == "free":
                    capacity = _metric_number(gpu.mem_total)
                    label = f"{gpu.name or '?'} / {capacity / 1024:.0f}GiB" if capacity else f"{gpu.name or '?'} / ?GiB"
                    grouped[label][node.name] += 1
        lines = ["Current GPU view: user, partition, search and free-GPU filters apply.",
                 "Stale/unavailable nodes and GPUs needing recovery are excluded.", ""]
        for model, counts in sorted(grouped.items()):
            lines += [f"{model}: {sum(counts.values())} free; single-node max {max(counts.values())}",
                      "  " + ", ".join(f"{name} × {count}" for name, count in sorted(counts.items(), key=lambda item: (-item[1], item[0])))]
        if not grouped:
            lines.append("No observed free capacity in this view.")
        lines.append("\nObserved capacity is not a reservation or scheduler admission guarantee.")
        self.push_screen(DetailScreen("Free GPU capacity", "\n".join(lines)))

    def _set_tab(self, pane: str) -> None:
        self.query_one("#main-tabs", TabbedContent).active = pane
        self._focus_tab()

    def _focus_tab(self) -> None:
        pane = self.query_one("#main-tabs", TabbedContent).active
        if pane == "pane-gpu":
            self.tbl.focus()
        elif pane == "pane-cpu":
            self.cpu_tbl.focus()
        elif pane == "pane-jobs":
            self.jobs_tbl.focus()
        elif pane == "pane-usage":
            self.query_one("#usage-scroll", VerticalScroll).focus()

    def action_tab_gpu(self) -> None:
        self._set_tab("pane-gpu")

    def action_tab_cpu(self) -> None:
        self._set_tab("pane-cpu")

    def action_tab_usage(self) -> None:
        self._set_tab("pane-usage")

    def action_tab_jobs(self) -> None:
        self._set_tab("pane-jobs")

    def action_show_usage(self) -> None:
        self._set_tab("pane-usage")

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        key = str(event.row_key.value)
        if key.startswith("hdr_"):
            self._show_detail("node", key[4:])
        elif key.startswith("pend_"):
            self._show_detail("job", key[5:])
        else:
            jid = self._jobs_row_job.get(key, self._row_job.get(key, ""))
            if jid:
                self._show_detail("job", jid)

    def _gpu_proc_table(self, node_name: str) -> str:
        """Per-GPU process lines (pid/user/VRAM/job) for the node detail modal."""
        node = next((n for n in self._nodes_cache if n.name == node_name), None)
        if node is None:
            return ""
        fresh = self._node_telemetry_fresh(node)
        lines = ["", "Node telemetry:",
                 f"  CPU actual: {node.cpu_util + '%' if fresh and node.cpu_util else '?'}; allocated: {node.cpu_alloc}/{node.cpus}",
                 "  PSI some avg10 (%): " + ", ".join(f"{resource}={node.pressure.get(resource, '?') if fresh else '?'}" for resource in ("cpu", "memory", "io")),
                 f"  Telemetry observed: {node.telemetry_observed_at:.0f}; node data age: {node.data_age_sec:.0f}s",
                 f"  Scheduler state: {node.state}; partition: {node.partition}; RAM: {self._ram_brief(node)}"]
        lines.append(f"  Source: {node.source or '?'}; scheduler age: {node.scheduler_age_sec:.0f}s")
        if node.jobs_truncated:
            lines.append("  Job cgroup scan reached its limit; some samples are unavailable.")
        lines.append("GPU health / processes:")
        for g in node.gpus:
            status = "STALE" if node.stale else gpu_health_status(g)[0]
            health_fresh = g.health_observed_at > 0 and -5 <= time.time() - g.health_observed_at <= TELEMETRY_MAX_AGE
            lines += [f"  GPU{g.index} ({g.name}): {status}",
                      f"    Temp {g.temp or '?'}C; power {g.power or '?'}/{g.power_cap or '?'}W; SM/memory clocks {g.sm_clock or '?'}/{g.mem_clock or '?'} MHz",
                      f"    ECC uncorrectable aggregate: {g.ecc or '?'} (cumulative, not new faults)",
                      f"    Clock events: {g.clock_reasons or '?' if health_fresh else '?'}; recovery: {g.recovery_action or '?' if health_fresh else '?'}"]
            if not g.pids:
                lines.append("    processes: —")
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

    def _job_telemetry_detail(self, job: JobInfo) -> str:
        summary = job_resource_summary(job, self._nodes_cache)
        def value(key, unit="", memory=False):
            return self._resource_actual(summary, key, memory) + unit
        detail = (f"\n\nJob telemetry: {summary['sampled_nodes']}/{summary['expected_nodes']} nodes sampled"
                f"\n  Requested: {job.cpu_count} CPUs; RAM {job.mem or '?'}; {job.gpu_count} GPUs"
                f"\n  CPU actual: {value('cpu_cores', ' cores')}"
                f"\n  Cgroup memory current / peak / finite limit: {value('mem_current_mib', memory=True)} / {value('mem_peak_mib', memory=True)} / {value('mem_limit_mib', memory=True)}"
                f"\n  OOM kills (cgroup lifetime): {value('oom_kill')}"
                f"\n  PID-attributed GPU VRAM: {value('vram_mib', memory=True)}"
                "\n  ? = unavailable; ~ = incomplete node coverage. Cgroup memory includes cache."
                "\n  Peak is a sum of node-local peaks; they may occur at different times.")
        for key, label in (("cpu_cores", "CPU"), ("mem_current_mib", "RAM"), ("mem_peak_mib", "peak"),
                           ("mem_limit_mib", "finite limit"), ("oom_kill", "OOM"), ("vram_mib", "PID VRAM")):
            detail += f"\n  {label} coverage: {summary.get(key + '_nodes', 0)}/{summary['expected_nodes']} nodes"
        return detail

    @work(thread=True)
    def _show_detail(self, kind: str, name: str) -> None:
        j = self._jobs_by_id.get(name) if kind == "job" else None
        pending = self._pending_by_id.get(name) if kind == "job" else None
        shared_detail = _collector_job_detail(j, pending)
        owner = j.user if j is not None else pending.user if pending is not None else ""
        control_name = _slurm_control_jobid(name) if kind == "job" else name
        # Keep the owner's full live record (especially private log paths).
        # Other users get the collector's sanitized detail without depending
        # on their Slurm RPC privileges.
        if shared_detail and owner != self.current_user:
            out = shared_detail
        else:
            ok, out = run_cmd(f"scontrol show {kind} {control_name}")
            if not ok:
                out = shared_detail or f"scontrol failed: {out}"
        if kind == "node":
            out += self._gpu_proc_table(name)
        if kind == "job":
            if j is not None:
                out += self._job_telemetry_detail(j)
            elif pending is not None:
                wait = pending_wait_seconds(pending)
                out += (f"\n\nPending: {pending_reason(pending)}; wait {int(wait)}s" if wait is not None else f"\n\nPending: {pending_reason(pending)}; wait ?")
                out += f"\n  Submitted: {pending.submit_time or '?'}; eligible: {pending.eligible_time or '?'}; QOS: {pending.qos or '?'}"
                out += f"\n  Estimated start: {pending.start_time or '?'} (scheduler estimate)"
                out += f"\n  {pending_explanation(pending)}"
            script, src = "", ""
            # 1) collector-shared script (SHARE_SCRIPTS on a privileged collector)
            if j is not None and j.script:
                script, src = j.script, "shared by collector"
            # 2) own job via scontrol (slurm reports failure as text, exit 0)
            if not script:
                ok2, s = run_cmd(f"scontrol write batch_script {control_name} -")
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
            # live step usage — sstat only answers for your own running jobs,
            # so a silent skip is the common case for everything else
            if j is not None and j.user == self.current_user:
                ok3, s3 = run_cmd(f"sstat -a -j {control_name} "
                                  "--format=JobID%16,AveCPU,MaxRSS,MaxVMSize,MaxDiskRead,MaxDiskWrite")
                if ok3 and len(s3.strip().splitlines()) > 2:
                    out += "\n\nLive usage (sstat, per step; MaxRSS is the largest single task):\n" + s3
            # Fall back to the collector's shared mirror for jobs whose logs
            # our account cannot read — without it these tabs are blank for
            # everyone but the owner.
            stdout_text, stdout_path, stderr_text, stderr_path = (
                _job_log_views(out, j)
            )
            if j is not None and j.log_status:
                status = ", ".join(
                    f"{stream}={value}" for stream, value in sorted(j.log_status.items())
                )
                out += f"\n\nShared log status: {status}"
            self.call_from_thread(
                self.push_screen,
                DetailScreen(f"{kind} {name}", out, script=script, script_src=src,
                             stdout_text=stdout_text, stdout_path=stdout_path,
                             stderr_text=stderr_text, stderr_path=stderr_path),
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
        if active == "pane-jobs":
            return self.jobs_tbl
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

    def _cancel_search_rerender(self) -> None:
        if self._search_timer is not None:
            self._search_timer.stop()
            self._search_timer = None

    def _finish_search_rerender(self) -> None:
        self._search_timer = None
        self._rerender()

    def _schedule_search_rerender(self) -> None:
        # A full DataTable rebuild is still substantial on large clusters.
        # Coalesce rapid keystrokes while keeping the filter responsive.
        self._cancel_search_rerender()
        self._search_timer = self.set_timer(0.12, self._finish_search_rerender)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "search-input":
            self.search_text = event.value.strip().lower()
            self._schedule_search_rerender()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input.id == "search-input":
            self._cancel_search_rerender()
            self._rerender()
            self._focus_tab()

    def on_key(self, event) -> None:
        if event.key == "escape":
            w = self.query_one("#search-input", Input)
            if w.display:
                w.clear()
                w.display = False
                self.search_text = ""
                self._focus_tab()
                self._cancel_search_rerender()
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
        (
            nodes_raw, jobs, pending, node_jobs, gpu_alloc, alloc_user_map,
            scheduler_status, err1,
        ) = collect_basic()
        node_names = [n["name"] for n in nodes_raw]

        # Show basic data immediately (use cache or empty)
        cached_results: Dict[str, NodeSSHResult] = {}
        stale_now: List[str] = []
        for name in node_names:
            if name in self._node_cache:
                gpus, mem = self._node_cache[name]
                cached_results[name] = NodeSSHResult(gpus, mem, "")
                stale_now.append(name)
        phase1_nodes = build_nodes(nodes_raw, node_jobs, cached_results, stale_now,
                                   scheduler_status=scheduler_status, scheduler_error=err1)
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
            phase2_nodes = build_nodes(nodes_raw, node_jobs, ssh_results, stale_nodes,
                                       scheduler_status=scheduler_status, scheduler_error=err1)
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
        if jobs_ok and self._watched:
            run_ids = {j.jobid for j in jobs}
            pend_ids = {pj.jobid for pj in pending}
            for jid in list(self._watched):
                w = self._watched[jid]
                label = f"{jid} ({w['jobname']})" if w["jobname"] else jid
                if jid in run_ids:
                    if w["state"] == "pending":
                        w["state"] = "running"
                        self.notify(f"watched {label} of {w['user']} started",
                                    title="watch", severity="information", timeout=10)
                elif jid not in pend_ids:
                    j = self._jobs_by_id.get(jid)
                    gpus = f", {j.gpu_count} GPU freed" if j and j.gpu_count else ""
                    self.notify(f"watched {label} of {w['user']} ended{gpus}",
                                title="watch", severity="warning", timeout=15)
                    del self._watched[jid]
        if jobs_ok:
            self._toast_jobs = mine_run
            self._toast_pending = mine_pend
        self._toast_down = down_now

    def _node_visible(self, node: NodeInfo, node_classes: Dict[str, List[str]], gpu_filter: bool = True) -> bool:
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
        if gpu_filter and self.idle_filter_only and "free" not in node_classes[node.name]:
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
                    not any(self.search_text in u.lower() for u in node_users) and
                    not any(self.search_text in value.lower() for job in node.jobs for value in (job.jobid, job.jobname))):
                return False
        return True

    def on_tabbed_content_tab_activated(self, event) -> None:
        if event.tabbed_content.id != "main-tabs":
            return
        # tabs render lazily — repaint the newly shown pane with current data
        if getattr(self, "_timer", None) is not None:  # fires during compose too
            self._rerender()
            if not self.query_one("#search-input", Input).display:
                self._focus_tab()

    def _apply(self, nodes: List[NodeInfo], jobs: List[JobInfo], pending: List[PendingJob], err: str) -> None:
        self._toast_check(nodes, jobs, pending, err)

        self._nodes_cache = nodes
        self._jobs_by_id = {j.jobid: j for j in jobs}
        self._pending_by_id = {j.jobid: j for j in pending}
        self._last_applied = (nodes, jobs, pending, err)
        nodes = list(nodes)
        now = time.time()
        self._history.observe(nodes, now)
        self.trend_w.update(Text(
            f"Cluster 5m GPU% {self._history.sparkline(1, now)}  "
            f"VRAM% {self._history.sparkline(2, now)}\n"
            f"GPU W {self._history.sparkline(3, now)}  · = no fresh observation",
            style="dim cyan",
        ))

        # Big clusters: start with every node collapsed (one line per node),
        # Space expands. Only on first data, never after user interaction.
        if not self._auto_collapsed and nodes:
            self._auto_collapsed = True
            limit = int(os.getenv("SLURM_GPU_TUI_AUTO_COLLAPSE_NODES", "12"))
            gpu_nodes_n = sum(1 for n in nodes if n.has_gpu)
            if gpu_nodes_n >= limit:
                self._collapsed = {n.name for n in nodes if n.has_gpu}

        # Pre-classify every GPU (header strips, FREE chip, sorting, filters)
        node_classes: Dict[str, List[str]] = {
            n.name: node_gpu_classes(n) for n in nodes
        }

        # Sorting nodes logic (simplistic)
        if self.sort_by == "util":
            # Sort by max util on node
            nodes.sort(key=lambda n: max([_metric_number(g.util) or 0 for g in n.gpus] + [0]), reverse=True)
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
                if not node.stale and (_metric_number(gpu.util) or 0) > 5:
                    busy_gpus += 1
                    stats[0] += 1

        active_pane = self.query_one("#main-tabs", TabbedContent).active
        if active_pane == "pane-gpu":
            self._apply_gpu_tab(visible, node_classes, pending)
        elif active_pane == "pane-cpu":
            self._apply_cpu_tab(nodes)
        elif active_pane == "pane-jobs":
            self._pending_user = {job.jobid: job.user for job in pending}
            self._apply_jobs_tab(jobs, pending, nodes)
        elif active_pane == "pane-usage":
            self.usage_view.update(render_usage())

        self._apply_summary(nodes, jobs, pending, err, node_classes,
                            total_gpus, busy_gpus, partition_gpu_stats)

    def _sync_table(self, table: DataTable, rows: list) -> None:
        columns = tuple(table.columns)
        keys = tuple(key for key, _ in rows)
        previous = self._table_models.get(table.id)
        if previous is not None and previous[0] == columns and previous[1] == keys and table.row_count == len(rows):
            for (key, cells), (_old_key, old_cells) in zip(rows, previous[2], strict=True):
                if cells is old_cells:
                    continue
                for column, old, new in zip(columns, old_cells, cells, strict=True):
                    # Rich Text equality omits its base style.
                    if old != new or old.style != new.style:
                        table.update_cell(key, column, new, update_width=True)
        else:
            row, col = table.cursor_row, table.cursor_column
            scroll_x, scroll_y = table.scroll_x, table.scroll_y
            key = None
            try:
                key = table.coordinate_to_cell_key(Coordinate(row, 0)).row_key
            except Exception:
                pass
            saved = getattr(self, "_column_views", {}).pop(table.id, None)
            if saved is not None:
                row, col, scroll_x, scroll_y, key = saved
            table.clear()
            for row_key, cells in rows:
                table.add_row(*cells, key=row_key)
            if table.row_count:
                try:
                    target = table.get_row_index(key) if key is not None else row
                except Exception:
                    target = row
                table.move_cursor(row=min(target, table.row_count - 1), column=col, animate=False)
            table.scroll_to(x=scroll_x, y=scroll_y, animate=False)
        self._table_models[table.id] = (columns, keys, rows)

    def _row_cells(self, table: DataTable, values: dict) -> list:
        cells = []
        for key, column in table.columns.items():
            value = values.get(str(key.value), Text(""))
            cell = value.copy() if isinstance(value, Text) else Text(str(value))
            cell.truncate(column.width, overflow="ellipsis")
            cells.append(cell)
        return cells

    @staticmethod
    def _ram_brief(node: NodeInfo) -> str:
        total = _metric_number(node.mem_total)
        if total is None or total <= 0:
            return "?"
        available = _metric_number(node.mem_avail)
        if available is None:
            available = _metric_number(node.mem_free)
        allocated = _metric_number(node.mem_alloc)
        used = total - available if available is not None else allocated
        prefix = "~" if available is None else ""
        if used is None:
            return f"?/{total / 1024:.0f}G"
        return f"{prefix}{max(0, min(total, used)) / 1024:.0f}/{total / 1024:.0f}G"

    @staticmethod
    def _node_telemetry_fresh(node: NodeInfo) -> bool:
        return (not node.stale and not node.error and node.telemetry_observed_at > 0
                and -5 <= time.time() - node.telemetry_observed_at <= TELEMETRY_MAX_AGE)

    def _apply_gpu_tab(self, visible: List[NodeInfo], node_classes: Dict[str, List[str]],
                       pending: List[PendingJob]) -> None:
        self._apply_pending_tab(pending)
        view_signature = self._gpu_view_signature(visible, node_classes, pending)
        if view_signature == self._last_gpu_view_signature:
            return
        rows = []
        self._row_job.clear()
        for node, node_model in zip(visible, view_signature[3], strict=True):
            signature = (view_signature[:3], node_model)
            cached = self._gpu_node_rows_cache.get(node.name)
            if cached is not None and cached[0] == signature:
                rows.extend(cached[1])
                self._row_job.update(cached[2])
                continue
            first_row = len(rows)
            collapsed = node.name in self._collapsed
            classes = node_classes[node.name]
            health = [gpu_health_status(gpu) for gpu in node.gpus]
            warnings = sum(severity > 0 for _label, severity in health)
            severity = max((value for _label, value in health), default=0)
            status = "STALE" if node.stale else "ERROR" if node.error else f"!{warnings}" if warnings else ""
            nname = Text(("▶ " if collapsed else "▼ ") + node.name,
                         style="yellow" if node.stale else "bold red" if node.error else "bold")
            models = ",".join(dict.fromkeys(gpu.name or "?" for gpu in node.gpus))
            free = classes.count("free")
            values = {"node": nname, "gpu_name": state_cell(node.state),
                      "util": state_cell(node.state) if self._compact else Text(f"C {node.cpu_alloc or '0'}/{node.cpus}", style="dim"),
                      "vram": gpu_strip(classes) if self._compact else Text("RAM " + self._ram_brief(node), style="dim"),
                      "user": Text(models if self._compact else node.partition, style="dim"),
                      "remaining": Text(f"free {free}", style="cyan" if free else "dim"),
                      "health": Text(status, style="red" if node.error or not node.stale and severity == 2 else "yellow" if status else "dim")}
            header = self._row_cells(self.tbl, values)
            for cell in header:
                cell.stylize("on #17201e")
            rows.append((f"hdr_{node.name}", header))
            if not collapsed:
                by_id = {job.jobid: job for job in node.jobs}
                by_user = {}
                for job in node.jobs:
                    by_user.setdefault(job.user, job)
                for index, gpu in enumerate(node.gpus):
                    matched = by_id.get(gpu.alloc_jobid)
                    if matched is None:
                        matched = next((by_user[user] for user in gpu.users if user in by_user), None)
                    jid = matched.jobid if matched else gpu.alloc_jobid
                    user = ",".join(gpu.users) or gpu.alloc_user
                    label = user
                    if classes[index] == "rogue":
                        label += " !gres" if matched and matched.gpu_count == 0 else " !slurm"
                    elif user and not gpu.users and not node.stale:
                        label += " " + fmt_idle_age(gpu.idle_sec)
                    elif classes[index] == "parked":
                        label += " parked"
                    elif jid:
                        label += " #" + jid
                    user_style = "bold red" if classes[index] == "rogue" else "yellow" if classes[index] == "idle" else "dim"
                    if self.current_user in gpu.users or self.current_user == gpu.alloc_user:
                        user_style = "bold cyan"
                    vcell = vram_cell(gpu.mem_used, gpu.mem_total)
                    ucell = util_cell(gpu.util)
                    if self._compact:
                        util = _metric_number(gpu.util)
                        ucell = Text(f"{util:.0f}%" if util is not None else "?", style="dim")
                        used, total = _metric_number(gpu.mem_used), _metric_number(gpu.mem_total)
                        vcell = Text((f"{used / 1024:.1f}" if used is not None else "?") +
                                     (f"/{total / 1024:.0f}G" if total else "/?"), style="dim")
                    health_label, severity = health[index]
                    if node.stale:
                        health_label, severity = "STALE", 1
                    values = {"node": Text(f"  GPU{gpu.index}", style="dim"), "gpu_name": Text(gpu.name or "?", style="dim"),
                              "util": ucell, "vram": vcell, "user": Text(label, style=user_style),
                              "remaining": remaining_cell(matched.elapsed, matched.time_limit) if matched else Text(""),
                              "health": Text(health_label, style="bold red" if severity == 2 else "yellow" if severity else "dim"),
                              "temp": temp_cell(gpu.temp), "power": power_cell(gpu.power, gpu.power_cap),
                              "jobid": Text(jid, style="dim"), "jobname": Text(matched.jobname if matched else "")}
                    cells = self._row_cells(self.tbl, values)
                    if self.current_user in gpu.users or self.current_user == gpu.alloc_user:
                        highlight_row(cells)
                    key = f"gpu_{node.name}_{gpu.index}"
                    if jid:
                        self._row_job[key] = jid
                    rows.append((key, cells))
            node_rows = rows[first_row:]
            self._gpu_node_rows_cache[node.name] = (
                signature, node_rows, {key: self._row_job[key] for key, _ in node_rows if key in self._row_job})
        live = {node.name for node in visible}
        self._gpu_node_rows_cache = {name: value for name, value in self._gpu_node_rows_cache.items() if name in live}
        self._sync_table(self.tbl, rows)
        self._last_gpu_view_signature = view_signature

    def _pending_visible(self, job: PendingJob) -> bool:
        return (not self.filter_user or job.user == self.filter_user) and (
            not self.filter_partition or job.partition == self.filter_partition) and (
            not self.search_text or any(self.search_text in value.lower()
                                       for value in (job.jobid, job.user, job.jobname, job.partition, pending_reason(job))))

    def _apply_pending_tab(self, pending: List[PendingJob]) -> None:
        from collections import Counter
        self._pending_user = {job.jobid: job.user for job in pending}
        visible = [job for job in pending if self._pending_visible(job)]
        self.query_one("#pending-container").display = bool(visible)
        counts = Counter(job.reason for job in visible)
        reasons = " · ".join(f"{reason or '?'} {count}" for reason, count in counts.most_common(3))
        label = f"{'▼' if self._pending_expanded else '▶'} Pending {len(visible)} · {reasons} [v]"
        self.query_one("#pending-label", Static).update(ellipsize(label, max(10, self.size.width - 3)))
        rows = []
        for job in visible:
            wait = pending_wait_seconds(job)
            values = {"p_jobid": Text(job.jobid, style="dim"), "p_user": Text(job.user, style="dim"),
                      "p_gpu": Text(str(job.gpu_count) if job.gpu_count else "—"),
                      "p_part": Text(job.partition, style="dim"), "p_name": Text(job.jobname),
                      "p_reason": Text(pending_reason(job), style="yellow"), "p_pri": Text(job.priority, style="dim"),
                      "p_wait": Text((fmt_span(int(wait)) or f"{int(wait)}s") if wait is not None else "?", style="dim"),
                      "p_start": Text(fmt_start_time(job.start_time), style="dim")}
            cells = self._row_cells(self.pending_tbl, values)
            if job.user == self.current_user:
                highlight_row(cells)
            rows.append((f"pend_{job.jobid}", cells))
        self._sync_table(self.pending_tbl, rows)

    def _gpu_view_signature(self, visible, node_classes, pending) -> tuple:
        return ((self.show_details, self._compact), self.filter_user, self.current_user,
                tuple(self._gpu_node_signature(node, node_classes) for node in visible))

    def _gpu_node_signature(self, node, node_classes) -> tuple:
        collapsed = node.name in self._collapsed
        health = tuple(gpu_health_status(gpu) for gpu in node.gpus)
        detail_rows = ()
        if not collapsed:
            jobs = tuple((job.jobid, job.user, job.jobname if self.show_details else "", job.elapsed,
                          job.gpu_count, job.time_limit) for job in node.jobs)
            gpus = tuple((gpu.index, gpu.name, gpu.util, gpu.mem_used, gpu.mem_total,
                          (gpu.temp, gpu.power, gpu.power_cap) if self.show_details else (),
                          tuple(gpu.users), gpu.alloc_jobid, gpu.alloc_user, gpu.idle_sec, gpu.parked_sec)
                         for gpu in node.gpus)
            detail_rows = (jobs, gpus, health)
        return (node.name, node.state, node.partition, node.stale, bool(node.error),
                node.cpus, node.cpu_alloc, self._ram_brief(node),
                tuple(gpu.name for gpu in node.gpus) if self._compact else (),
                sum(severity > 0 for _label, severity in health),
                max((severity for _label, severity in health), default=0),
                collapsed, tuple(node_classes[node.name]), detail_rows)

    def _apply_cpu_tab(self, nodes: List[NodeInfo]) -> None:
        visible = [node for node in nodes if self._node_visible(node, {}, gpu_filter=False)]
        visible.sort(key=lambda node: (-(_metric_number(node.cpu_alloc) or 0) / max(1, _metric_number(node.cpus) or 1), node.name))
        signature = self._cpu_view_signature(visible)
        if signature == self._last_cpu_view_signature:
            return
        rows = []
        total_cores = alloc_cores = actual_cores = reporting_cores = 0
        for node in visible:
            total = _metric_number(node.cpus) or 0
            alloc = _metric_number(node.cpu_alloc) or 0
            total_cores += total
            alloc_cores += alloc
            fresh = self._node_telemetry_fresh(node)
            actual = _metric_number(node.cpu_util) if fresh else None
            if actual is not None:
                actual_cores += total * actual / 100
                reporting_cores += total
            pressure = [_metric_number(node.pressure.get(resource)) if fresh else None for resource in ("cpu", "memory", "io")]
            psi = "/".join(f"{value:.0f}" if value is not None else "?" for value in pressure)
            users = {}
            for job in node.jobs:
                if job.cpu_count:
                    users[job.user] = users.get(job.user, 0) + job.cpu_count
            users_text = Text()
            for user, cores in sorted(users.items(), key=lambda item: -item[1]):
                users_text.append((" " if users_text else "") + f"{user}:{cores}",
                                  style="bold cyan" if user == self.current_user else "dim")
            values = {"c_node": Text(node.name + (" !" if node.stale else ""), style="yellow" if node.stale else "bold"),
                      "c_state": state_cell(node.state), "c_part": Text(node.partition, style="dim"),
                      "c_cpu": Text(f"{alloc:.0f}/{total:.0f}", style="dim"),
                      "c_actual": Text(f"{actual:.0f}" if actual is not None else "?", style="dim"),
                      "c_load": Text(node.cpu_load or "?", style="dim"),
                      "c_mem": Text(self._ram_brief(node), style="dim"),
                      "c_psi": Text(psi, style="yellow" if any(value is not None and value >= 10 for value in pressure) else "dim"),
                      "c_users": users_text}
            rows.append((f"hdr_{node.name}", self._row_cells(self.cpu_tbl, values)))
        self._sync_table(self.cpu_tbl, rows)
        coverage = f"{reporting_cores:.0f}/{total_cores:.0f} cores sampled"
        self.cpu_summary.update(Text(
            f"VIEW CPU alloc {alloc_cores:.0f}/{total_cores:.0f} · actual {actual_cores:.1f} cores ({coverage})\n"
            "PSI C/M/I = CPU/memory/I/O some avg10 stall % · RAM ~ = scheduler allocation", style="dim"))
        self._last_cpu_view_signature = signature

    def _cpu_view_signature(self, nodes: List[NodeInfo]) -> tuple:
        rows = []
        for node in nodes:
            users = {}
            for job in node.jobs:
                users[job.user] = users.get(job.user, 0) + job.cpu_count
            rows.append((node.name, node.state, node.partition, node.stale, node.cpus,
                         node.cpu_alloc, node.cpu_load, self._ram_brief(node),
                         self._node_telemetry_fresh(node), node.cpu_util,
                         tuple(sorted(node.pressure.items())), tuple(sorted(users.items()))))
        return self._compact, self.current_user, tuple(rows)

    @staticmethod
    def _resource_actual(summary: dict, key: str, memory: bool = False) -> str:
        value = summary.get(key)
        if value is None:
            return "?"
        partial = summary.get(key + "_nodes", 0) < summary["expected_nodes"]
        return ("~" if partial else "") + (f"{value / 1024:.1f}G" if memory else f"{value:.1f}")

    def _job_visible(self, job: JobInfo) -> bool:
        from .common import _strict_expand_nodes
        targets = (job.jobid, job.user, job.jobname, job.partition, job.node)
        if self.search_text:
            targets += tuple(_strict_expand_nodes(job.node) or ())
        return (not self.filter_user or job.user == self.filter_user) and (
            not self.filter_partition or job.partition == self.filter_partition) and (
            not self.search_text or any(self.search_text in value.lower()
                                       for value in targets))

    def _apply_jobs_tab(self, jobs: List[JobInfo], pending: List[PendingJob], nodes: List[NodeInfo]) -> None:
        rows = []
        self._jobs_row_job.clear()
        for job in jobs:
            if not self._job_visible(job):
                continue
            summary = job_resource_summary(job, nodes)
            sampled, expected = summary["sampled_nodes"], summary["expected_nodes"]
            note = f"{sampled}/{expected} sampled" if sampled else "no sample"
            if summary.get("oom_kill", 0):
                note = f"OOM {summary['oom_kill']:.0f} · {note}"
            elif sampled and (sampled < expected or any(summary.get(key + "_nodes", 0) < expected for key in ("cpu_cores", "mem_current_mib"))):
                note = f"partial {sampled}/{expected}"
            state = "R"
            values = {"j_id": Text(job.jobid, style="dim"), "j_user": Text(job.user, style="bold cyan" if job.user == self.current_user else "dim"),
                      "j_state": Text(state, style="dim"), "j_gpu": Text(str(job.gpu_count) if job.gpu_count else "—"),
                      "j_cpu": Text(self._resource_actual(summary, "cpu_cores") + f"/{job.cpu_count}", style="dim"),
                      "j_mem": Text(self._resource_actual(summary, "mem_current_mib", True) + "/" + (job.mem or "?"), style="dim"),
                      "j_vram": Text(self._resource_actual(summary, "vram_mib", True), style="dim"),
                      "j_span": remaining_cell(job.elapsed, job.time_limit), "j_note": Text(note, style="bold red" if summary.get("oom_kill", 0) else "dim"),
                      "j_nodes": Text(job.node, style="dim"), "j_name": Text(job.jobname)}
            cells = self._row_cells(self.jobs_tbl, values)
            if job.user == self.current_user:
                highlight_row(cells)
            key = f"job_{job.jobid}"
            rows.append((key, cells))
            self._jobs_row_job[key] = job.jobid
        for job in pending:
            if not self._pending_visible(job):
                continue
            wait = pending_wait_seconds(job)
            values = {"j_id": Text(job.jobid, style="dim"), "j_user": Text(job.user, style="dim"),
                      "j_state": Text("PD", style="yellow"), "j_gpu": Text(str(job.gpu_count) if job.gpu_count else "—"),
                      "j_cpu": Text(f"?/{job.cpu_count}", style="dim"), "j_mem": Text("?/" + (job.mem or "?"), style="dim"),
                      "j_span": Text((fmt_span(int(wait)) or f"{int(wait)}s") if wait is not None else "?", style="dim"),
                      "j_note": Text(pending_reason(job), style="yellow"), "j_name": Text(job.jobname)}
            cells = self._row_cells(self.jobs_tbl, values)
            if job.user == self.current_user:
                highlight_row(cells)
            rows.append((f"pend_{job.jobid}", cells))
        self._sync_table(self.jobs_tbl, rows)

    def _apply_summary(self, nodes: List[NodeInfo], jobs: List[JobInfo],
                       pending: List[PendingJob], err: str,
                       node_classes: Dict[str, List[str]],
                       total_gpus: int, busy_gpus: int,
                       partition_gpu_stats: Dict[str, List[int]]) -> None:
        from collections import Counter
        user_gpu_count: Dict[str, int] = {}
        for job in jobs:
            user_gpu_count[job.user] = user_gpu_count.get(job.user, 0) + job.gpu_count
        self._user_gpu_count = user_gpu_count
        visible = [node for node in nodes if node.has_gpu and self._node_visible(node, node_classes)]
        free_nodes = [(node, node_classes[node.name].count("free")) for node in visible]
        free = sum(count for _node, count in free_nodes)
        maximum = max((count for _node, count in free_nodes), default=0)
        models = Counter()
        for node in visible:
            for gpu, kind in zip(node.gpus, node_classes[node.name], strict=True):
                if kind == "free":
                    memory = _metric_number(gpu.mem_total)
                    capacity = f"{memory / 1024:.0f}G" if memory else "?G"
                    model = gpu.name.removeprefix("NVIDIA ") or "?"
                    models[(model, capacity)] += 1
        running = sum(self._job_visible(job) for job in jobs)
        waiting = sum(self._pending_visible(job) for job in pending)
        unknown = sum(node_classes[node.name].count("unknown") for node in visible)
        stale = sum(node.stale for node in nodes)
        first = Text(f"VIEW GPU {busy_gpus}/{total_gpus} active · FREE {free} (node max {maximum}) · JOBS {running}/{waiting} wait", style="bold")
        if unknown:
            first.append(f" · ? {unknown}", style="yellow")
        second = Text("FREE " + (" · ".join(f"{model}/{capacity}×{count}" for (model, capacity), count in models.most_common()) or "—"), style="dim")
        filters = [f"u:{self.filter_user}" if self.filter_user else "", f"p:{self.filter_partition}" if self.filter_partition else "",
                   "free" if self.idle_filter_only else "", f"/{self.search_text}" if self.search_text else ""]
        if any(filters):
            second.append(" · " + " ".join(filter(None, filters)), style="cyan")
        if stale:
            second.append(f" · ALL stale {stale}", style="yellow")
        width = max(10, self.size.width - 3)
        first.truncate(width, overflow="ellipsis")
        second.truncate(width, overflow="ellipsis")
        self.summary_w.update(first + Text("\n") + second)
        ts = datetime.now().strftime("%H:%M:%S")
        if err:
            self.status_w.update(Text(ellipsize(f"WARN: {err}", width), style="bold yellow"))
        else:
            age = max((node.scheduler_age_sec for node in nodes if node.scheduler_age_sec >= 0), default=-1)
            source = _node_source_counts(nodes)
            status = f"{ts} · {self.refresh_sec}s · sort:{self.sort_by}{'↑' if self.sort_reverse else ''} · ALL source {source[0]} GPU push/{source[1]} fallback"
            status += f" · CPU {source[2]} push/{source[3]} poll"
            if age >= 0:
                status += f" · scheduler {age:.0f}s"
            self.status_w.update(Text(ellipsize(status, width), style="dim"))





def main():
    """Entry-point shim: real CLI lives in sgpu.cli (kept for old venvs whose
    console script still imports sgpu.tui:main)."""
    from .cli import main as _main
    _main()
