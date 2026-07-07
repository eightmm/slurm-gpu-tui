"""SLURM GPU Monitor TUI application."""
from __future__ import annotations

import json
import os
import sys
import time
from dataclasses import asdict
from datetime import datetime, timedelta
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rich.text import Text
from textual import work
from textual.app import App, ComposeResult
from textual.containers import Vertical, VerticalScroll
from textual.coordinate import Coordinate
from textual.screen import ModalScreen
from textual.timer import Timer
from textual.widgets import DataTable, Footer, Header, Input, OptionList, Static
from textual.widgets.option_list import Option

from .common import (
    GpuInfo, JobInfo, NodeInfo, NodeSSHResult, PendingJob,
    apply_gpu_alloc, build_nodes, cleanup_ssh_pool, collect_basic,
    collect_node_data_parallel, run_cmd,
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
            start_time=p.get("start_time", ""),
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
                gpu_count=j.get("gpu_count", 0), gres_raw=j.get("gres_raw", ""),
                time_limit=j.get("time_limit", ""),
            )
            for j in n.get("jobs", [])
        ]
        nodes.append(NodeInfo(
            name=n.get("name", ""), state=n.get("state", ""),
            partition=n.get("partition", ""), source=n.get("source", ""),
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
    if not util_str:
        return Text("-", style="bright_black")
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
        # sinfo free_mem can exceed configured total; clamp to sane range
        used_mb = min(max(used_mb, 0.0), total_mb)
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


# GPU processes by these users never count as rogue (system daemons)
_ROGUE_IGNORE = {
    u for u in os.getenv("SLURM_GPU_TUI_ROGUE_IGNORE", "root,gdm,xdm").split(",") if u
}


def classify_gpu(g: GpuInfo) -> str:
    """One of: rogue (GPU process outside any SLURM allocation) / busy /
    parked (VRAM held, no compute) / idle (reserved, no process) / free /
    unknown (no data yet)."""
    real_users = [u for u in g.users if u not in _ROGUE_IGNORE]
    if real_users and not g.alloc_jobid:
        return "rogue"
    try:
        util = float(g.util)
    except (ValueError, TypeError):
        return "unknown"
    if util > 5:
        return "busy"
    try:
        total = float(g.mem_total)
        vram_pct = float(g.mem_used) / total if total > 0 else 0.0
    except (ValueError, TypeError):
        vram_pct = 0.0
    if vram_pct >= 0.3:
        return "parked"
    if g.alloc_jobid or real_users:
        return "idle"
    return "free"


_CLASS_GLYPH = {
    "busy": ("█", "green"),
    "parked": ("▅", "blue"),
    "idle": ("▂", "yellow"),
    "free": ("▁", "bold cyan"),
    "rogue": ("!", "bold red"),
    "unknown": ("?", "bright_black"),
}


def gpu_strip(classes: List[str]) -> Text:
    """One glyph per GPU: at-a-glance node state, useful when collapsed."""
    strip = Text()
    for c in classes:
        ch, style = _CLASS_GLYPH.get(c, ("?", "bright_black"))
        strip.append(ch, style=style)
    return strip


def fmt_span(sec: int) -> str:
    """'3.2h' / '12m' / '' for sub-minute durations."""
    if sec >= 3600:
        return f"{sec / 3600:.1f}h"
    if sec >= 60:
        return f"{sec // 60}m"
    return ""


def fmt_idle_age(sec: int) -> str:
    """'idle 3.2h' / 'idle 12m' / 'idle' for short/unknown durations."""
    span = fmt_span(sec)
    return f"idle {span}" if span else "idle"


def collect_waste(nodes: List[NodeInfo], min_sec: int) -> List[dict]:
    """Idle/parked offenders over the threshold, worst first."""
    rows: List[dict] = []
    for n in nodes:
        for g in n.gpus:
            real_users = [u for u in g.users if u not in _ROGUE_IGNORE]
            if real_users and not g.alloc_jobid:
                rows.append({
                    "node": n.name, "gpu": g.index, "kind": "rogue",
                    "sec": 0, "user": ",".join(real_users), "jobid": "",
                })
            elif g.idle_sec >= min_sec:
                rows.append({
                    "node": n.name, "gpu": g.index, "kind": "idle",
                    "sec": g.idle_sec, "user": g.alloc_user,
                    "jobid": g.alloc_jobid,
                })
            elif g.parked_sec >= min_sec:
                rows.append({
                    "node": n.name, "gpu": g.index, "kind": "parked",
                    "sec": g.parked_sec,
                    "user": g.alloc_user or ",".join(g.users),
                    "jobid": g.alloc_jobid,
                })
    # rogue first (GPU use outside SLURM), then worst waste
    rows.sort(key=lambda r: (r["kind"] != "rogue", -r["sec"]))
    return rows


WASTE_MIN_SEC = int(os.getenv("SLURM_GPU_TUI_WASTE_MIN_SEC", "600"))


def fmt_start_time(s: str) -> str:
    """Compact squeue %S estimate: '15:00' today, '07-08 15:00' otherwise."""
    if not s or s in ("N/A", "(null)"):
        return ""
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return s
    if dt.date() == datetime.now().date():
        return dt.strftime("%H:%M")
    return dt.strftime("%m-%d %H:%M")


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


# ── Modal screens ─────────────────────────────────────────────────────────

class DetailScreen(ModalScreen):
    """Scrollable modal showing scontrol output for a job or node."""

    BINDINGS = [
        ("escape", "close", "Close"),
        ("q", "close", "Close"),
        ("enter", "close", "Close"),
    ]
    CSS = """
    DetailScreen { align: center middle; }
    #detail-box {
        width: 90%; max-height: 85%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    #detail-title { text-style: bold; color: $accent; }
    """

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def __init__(self, title: str, body: str) -> None:
        super().__init__()
        self._title = title
        self._body = body

    def compose(self) -> ComposeResult:
        with VerticalScroll(id="detail-box"):
            yield Static(self._title, id="detail-title")
            # Text() so shell scripts with [brackets] aren't parsed as markup
            yield Static(Text(self._body))

    def action_close(self) -> None:
        self.app.pop_screen()


HELP_TEXT = """\
 r        Refresh now
 f        Fast (1s) / Normal (3s) refresh
 s        Cycle sort: Node → Utilization → User → Free
 u        Filter by user (pick from list; u again clears)
 i        Idle filter (truly free GPUs only)
 d        Detail columns (Temp / Power / JobID / JobName)
 Space    Collapse / expand node (on header row)
 Enter    Job / node details (scontrol)
 /        Search node or user (Esc clears)
 w        Wasted GPUs (idle / parked, worst first)
 g        GPU-hours by user (last 7 days)
 j / k    Cursor down / up
 e        Export snapshot JSON
 ?        This help
 q        Quit

 Node header GPU strip (one glyph per GPU):
   █ busy   ▅ parked (VRAM held, no compute)
   ▂ idle (reserved, no process)   ▁ free
   ! rogue (GPU use outside SLURM)   ? no data

 User column markers:
   bold user        process running on GPU
   user idle 3.2h   allocated but no process (wasted)
   parked           VRAM held at ~0% util
   user !slurm      GPU process with no SLURM allocation

 Korean IME: same physical keys work without switching
 (ㅂ=q, ㄱ=r, ㄴ=s, ㅇ=d, ㅑ=i, ㅕ=u, ㄹ=f, ㄷ=e, ㅈ=w, ㅎ=g, ㅓ=j, ㅏ=k)
"""


class WasteScreen(ModalScreen):
    """GPUs wasting away: idle (allocated, no process) and parked (VRAM held)."""

    BINDINGS = [("escape", "close", "Close"), ("q", "close", "Close"), ("w", "close", "Close")]
    CSS = """
    WasteScreen { align: center middle; }
    #waste-box {
        width: 76; max-height: 85%;
        border: round $warning; background: $surface; padding: 1 2;
    }
    """

    def __init__(self, rows: List[dict]) -> None:
        super().__init__()
        self._rows = rows

    def compose(self) -> ComposeResult:
        body = Text()
        if not self._rows:
            body.append("No wasted GPUs over threshold. Nice cluster.", style="green")
        for r in self._rows:
            body.append(f" {r['node']}/{r['gpu']:<3}", style="bold cyan")
            style = {"idle": "yellow", "parked": "blue", "rogue": "red"}.get(r["kind"], "white")
            body.append(f" {r['kind']:<7}", style=f"bold {style}")
            span = "-" if r["kind"] == "rogue" else (fmt_span(r["sec"]) or "<1m")
            body.append(f" {span:>7} ", style="bold")
            body.append(f" {r['user']:<12}", style="magenta")
            if r["jobid"]:
                body.append(f" job {r['jobid']}", style="dim")
            body.append("\n")
        with VerticalScroll(id="waste-box"):
            yield Static(Text(f"Wasted GPUs (≥{fmt_span(WASTE_MIN_SEC) or WASTE_MIN_SEC}) — idle: reserved w/o process · parked: VRAM held at 0% util", style="bold"))
            yield Static(body)

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def action_close(self) -> None:
        self.app.pop_screen()


class UsageScreen(ModalScreen):
    """Per-user GPU-hours over the last N days (from collector's usage.json)."""

    BINDINGS = [("escape", "close", "Close"), ("q", "close", "Close"), ("g", "close", "Close")]
    CSS = """
    UsageScreen { align: center middle; }
    #usage-box {
        width: 64; max-height: 85%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    """

    def __init__(self, days: int = 7) -> None:
        super().__init__()
        self._days = days

    def compose(self) -> ComposeResult:
        body = Text()
        totals = load_usage_totals(self._days)
        if totals is None:
            body.append("No usage data (collector not running or too new).", style="dim")
        elif not totals:
            body.append("No GPU usage recorded in this window.", style="dim")
        else:
            body.append(f" {'user':<14}{'alloc':>9}{'busy':>9}{'eff':>6}\n", style="bold underline")
            for user, alloc, busy in totals:
                eff = busy / alloc if alloc > 0 else 0
                eff_style = "green" if eff >= 0.7 else "yellow" if eff >= 0.4 else "red"
                body.append(f" {user:<14}", style="magenta")
                body.append(f"{alloc / 3600:>8.1f}h{busy / 3600:>8.1f}h", style="bold")
                body.append(f"{eff:>6.0%}\n", style=eff_style)
        with VerticalScroll(id="usage-box"):
            yield Static(Text(f"GPU-hours by user — last {self._days} days", style="bold"))
            yield Static(body)

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def action_close(self) -> None:
        self.app.pop_screen()


def load_usage_totals(days: int) -> Optional[List[Tuple[str, float, float]]]:
    """Sum usage.json daily buckets: [(user, alloc_sec, busy_sec)], alloc desc."""
    state_dir = Path(os.getenv("SLURM_GPU_TUI_STATE_DIR", str(Path.home() / ".sgpu" / "state")))
    raw = None
    for p in (state_dir / "usage.json", _DAEMON_DATA_FILE.parent / "usage.json"):
        try:
            raw = json.loads(p.read_text())
            break
        except (OSError, ValueError):
            continue
    if raw is None:
        return None
    cutoff = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%d")
    totals: Dict[str, List[float]] = {}
    for day, users in raw.get("days", {}).items():
        if day < cutoff:
            continue
        for user, u in users.items():
            t = totals.setdefault(user, [0.0, 0.0])
            t[0] += u.get("alloc", 0)
            t[1] += u.get("busy", 0)
    return sorted(((u, a, b) for u, (a, b) in totals.items()), key=lambda x: -x[1])


class UserSelectScreen(ModalScreen):
    """Pick a user to filter the view by (Enter selects, Esc cancels)."""

    BINDINGS = [("escape", "cancel", "Cancel"), ("q", "cancel", "Cancel")]
    CSS = """
    UserSelectScreen { align: center middle; }
    #user-box {
        width: 44; max-height: 80%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    """

    def __init__(self, users: List[Tuple[str, int]]) -> None:
        super().__init__()
        self._users = users

    def compose(self) -> ComposeResult:
        with Vertical(id="user-box"):
            yield Static(Text("Filter by user", style="bold"))
            yield OptionList(*[
                Option(f"{u:<16} {c} GPU", id=u) for u, c in self._users
            ])

    def on_option_list_option_selected(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(str(event.option.id))

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_cancel()

    def action_cancel(self) -> None:
        self.dismiss(None)


class HelpScreen(ModalScreen):
    BINDINGS = [
        ("escape", "close", "Close"),
        ("q", "close", "Close"),
        ("question_mark", "close", "Close"),
    ]
    CSS = """
    HelpScreen { align: center middle; }
    #help-box {
        width: 60; max-height: 90%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    """

    def compose(self) -> ComposeResult:
        with VerticalScroll(id="help-box"):
            yield Static(Text("Keyboard Shortcuts", style="bold"))
            yield Static(HELP_TEXT)

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def action_close(self) -> None:
        self.app.pop_screen()


# Korean 2-set layout: jamo typed where the latin binding key sits.
# Lets every shortcut work without switching IME back to English.
_JAMO_ACTIONS = {
    "ㅂ": "quit",          # q
    "ㄱ": "refresh",       # r
    "ㄹ": "toggle_fast",   # f
    "ㄴ": "toggle_sort",   # s
    "ㅕ": "toggle_user_filter",  # u
    "ㅑ": "toggle_idle_filter",  # i
    "ㅇ": "toggle_details",      # d
    "ㄷ": "export_json",         # e
    "ㅈ": "show_waste",          # w
    "ㅎ": "show_usage",          # g
    "ㅓ": "cursor_down",         # j
    "ㅏ": "cursor_up",           # k
}


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
        ("w", "show_waste", "Waste"),
        ("g", "show_usage", "GPU-hours"),
        ("e", "export_json", "Export JSON"),
        ("question_mark", "help", "Help"),
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
        self.pending_tbl.add_column("Est.Start", key="p_start")
        self.pending_tbl.cursor_type = "row"
        self.pending_tbl.zebra_stripes = True

        self.refresh_sec_normal = int(os.getenv("SLURM_GPU_TUI_REFRESH_SEC", "3"))
        self.refresh_sec_fast = int(os.getenv("SLURM_GPU_TUI_FAST_REFRESH_SEC", "1"))
        self.refresh_sec = self.refresh_sec_normal
        self.node_timeout = int(os.getenv("SLURM_GPU_TUI_NODE_TIMEOUT_SEC", "30"))
        self.max_workers = int(os.getenv("SLURM_GPU_TUI_MAX_WORKERS", "8"))
        self.snapshot: dict = {}

        self.sort_by = "node"  # "node", "util", "user", "free"
        self.filter_user = ""  # show only this user's jobs ("" = everyone)
        self._user_gpu_count: Dict[str, int] = {}
        self._collapsed: set = set()  # node names that are collapsed
        self._last_data_mtime: float | None = None
        self._force_render = False
        self._row_job: Dict[str, str] = {}  # table row key -> jobid for detail popup
        self._nodes_cache: List[NodeInfo] = []  # last applied nodes (waste view)

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

    def _rerender(self) -> None:
        """Force a re-render even if daemon data is unchanged (UI state changed)."""
        self._force_render = True
        self.refresh_all()

    def action_refresh(self) -> None:
        self._rerender()

    def action_toggle_fast(self) -> None:
        if self.refresh_sec == self.refresh_sec_normal:
            self.refresh_sec = self.refresh_sec_fast
        else:
            self.refresh_sec = self.refresh_sec_normal
        self._reset_timer(self.refresh_sec)
        self._rerender()

    def action_export_json(self) -> None:
        import json as _json
        out = Path(f"sgpu-export-{datetime.now().strftime('%Y%m%d-%H%M%S')}.json")
        out.write_text(_json.dumps(self.snapshot, indent=2, ensure_ascii=False))
        self.status_w.update(Text(f" Exported → {out} ", style="bold green"))

    def action_toggle_sort(self) -> None:
        choices = ["node", "util", "user", "free"]
        idx = choices.index(self.sort_by)
        self.sort_by = choices[(idx + 1) % len(choices)]
        self.status_w.update(f"Sort by: {self.sort_by}")
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
        self.tbl.add_column("CPU a/t", key="cpu", width=8)
        self.tbl.add_column("RAM", key="ram", width=18)
        self.tbl.add_column("GPU#", key="gpu_idx")
        self.tbl.add_column("GPU Name", key="gpu_name")
        self.tbl.add_column("Util", key="util")
        self.tbl.add_column("VRAM", key="vram")
        if self.show_details:
            self.tbl.add_column("T", key="temp")
            self.tbl.add_column("Power", key="power")
        self.tbl.add_column("User", key="user", width=17)
        if self.show_details:
            self.tbl.add_column("JobID", key="jobid")
            self.tbl.add_column("JobName", key="jobname")
        self.tbl.add_column("Remaining", key="remaining")

    def action_toggle_idle_filter(self) -> None:
        self.idle_filter_only = not self.idle_filter_only
        status = "Idle Nodes Only" if self.idle_filter_only else "All Nodes"
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

    def action_show_usage(self) -> None:
        self.push_screen(UsageScreen())

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

    @work(thread=True)
    def _show_detail(self, kind: str, name: str) -> None:
        ok, out = run_cmd(f"scontrol show {kind} {name}")
        if not ok:
            out = f"scontrol failed: {out}"
        if kind == "job":
            # Batch script is only readable for your own jobs; slurm reports
            # the failure as text with exit 0, so filter by content
            ok2, script = run_cmd(f"scontrol write batch_script {name} -")
            script = script.strip()
            if ok2 and script and not script.startswith("job script retrieval failed"):
                out += "\n\n─── batch script " + "─" * 43 + "\n" + script
        self.call_from_thread(self.push_screen, DetailScreen(f"{kind} {name}", out))

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
        nodes_raw, jobs, pending, node_jobs, gpu_alloc, err1 = collect_basic()
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
        apply_gpu_alloc(phase1_nodes, gpu_alloc, jobs)
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
            apply_gpu_alloc(phase2_nodes, gpu_alloc, jobs)
            self.call_from_thread(self._apply, phase2_nodes, jobs, pending, " | ".join(all_errors) if all_errors else "")

    def _apply(self, nodes: List[NodeInfo], jobs: List[JobInfo], pending: List[PendingJob], err: str) -> None:
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

        self._nodes_cache = nodes

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

        _HDR_BG = "on #0d1f0d"  # dark green tint for node header rows

        total_gpus = 0
        busy_gpus = 0
        user_gpu_count: Dict[str, int] = {}
        partition_gpu_stats: Dict[str, List[int]] = {}  # partition -> [busy, total]

        for node in nodes:
            # Filter logic
            if self.filter_user:
                fu = self.filter_user
                has_user = any(fu in g.users or fu == g.alloc_user for g in node.gpus)
                has_user = has_user or any(j.user == fu for j in node.jobs)
                if not has_user:
                    continue

            if self.idle_filter_only:
                if "free" not in node_classes[node.name]:
                    continue

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
                    continue

            node_partition = node.partition or (node.jobs[0].partition if node.jobs else "")
            # Stats: prefer the partition jobs actually run in over sinfo's first listing
            partition_stat_key = node.jobs[0].partition if node.jobs else node_partition.split(",")[0]

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

            # Partition cell: first partition + "+n" beats a hard truncation
            parts = [p for p in node_partition.split(",") if p]
            part_disp = parts[0] + (f"+{len(parts) - 1}" if len(parts) > 1 else "") if parts else ""

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
                    cpu_text, mem_cell(node),
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
                    total_gpus += 1
                    gpu_busy = False
                    try:
                        if float(gpu.util) > 5:
                            busy_gpus += 1
                            gpu_busy = True
                    except (ValueError, TypeError):
                        pass

                    if partition_stat_key not in partition_gpu_stats:
                        partition_gpu_stats[partition_stat_key] = [0, 0]
                    partition_gpu_stats[partition_stat_key][1] += 1
                    if gpu_busy:
                        partition_gpu_stats[partition_stat_key][0] += 1

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
                    row_cells = [
                        Text(""), Text(""), Text(""),  # node, state, part
                        Text(""), Text(""),             # cpu, ram
                        Text(f"  {gpu.index}", style="bold"),
                        Text(gpu.name),
                        util_cell(gpu.util),
                        vcell,
                    ]
                    if self.show_details:
                        row_cells.append(temp_cell(gpu.temp))
                        row_cells.append(power_cell(gpu.power, gpu.power_cap))
                    if user and classes[gpu_i] == "rogue":
                        user_cell = Text(user, style="bold red")
                        user_cell.append(" !slurm", style="bold red reverse")
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
                        row_cells.append(Text(jobname) if jobname else Text(""))
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
                    job_key = f"job_{node.name}_{j.jobid}"
                    self._row_job[job_key] = j.jobid
                    self.tbl.add_row(*row_cells, key=job_key)

        # ── Pending Jobs Table ──
        self.query_one("#pending-container").display = bool(pending)
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
                Text(pj.jobname),
                Text(pj.reason, style=reason_style),
                Text(pj.priority, style="dim"),
                Text(fmt_start_time(pj.start_time), style="cyan"),
            ]
            if is_me:
                highlight_row(row_cells)
            self.pending_tbl.add_row(*row_cells, key=f"pend_{pj.jobid}")

        # Update Summary
        for j in jobs:
            if j.gpu_count > 0:
                user_gpu_count[j.user] = user_gpu_count.get(j.user, 0) + j.gpu_count
        self._user_gpu_count = user_gpu_count

        ts = datetime.now().strftime("%H:%M:%S")
        mode_label = Text(" FAST ", style="bold white on red") if self.refresh_sec == self.refresh_sec_fast else Text(" NORM ", style="bold white on blue")
        sort_label = Text(f" SORT:{self.sort_by.upper()} ", style="bold white on #444444")

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
        # Data-source health: how each node's stats arrived this cycle
        n_agent = sum(1 for n in nodes if n.source == "agent")
        n_ssh = sum(1 for n in nodes if n.source == "ssh")
        n_stale = sum(1 for n in nodes if n.source == "stale" or (n.stale and not n.source))
        n_rogue_total = sum(cl.count("rogue") for cl in node_classes.values())
        if n_rogue_total:
            summary.append(" ROGUE ", style="bold white on red")
            summary.append(f" {n_rogue_total} ", style="bold red")
        if n_agent or n_ssh or n_stale:
            summary.append(" SRC ", style="bold white on grey37")
            summary.append(f" agent:{n_agent}", style="green" if n_agent else "dim")
            if n_ssh:
                summary.append(f" ssh:{n_ssh}", style="yellow")
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
        summary.append_text(mode_label)
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

        self.summary_w.update(summary)

        if err:
            self.status_w.update(Text(f" WARN: {err} ", style="bold yellow on dark_red"))
        else:
            self.status_w.update(Text(f" OK [{ts}] ", style="dim"))

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

        self.snapshot = {
            "ts": datetime.now().isoformat(),
            "nodes": [asdict(n) for n in nodes],
            "jobs": [asdict(j) for j in jobs],
            "pending": [asdict(p) for p in pending],
        }


# ── One-shot CLI mode ─────────────────────────────────────────────────────

def _oneshot_snapshot() -> dict:
    """Fresh snapshot dict: daemon file if recent, else direct collection."""
    try:
        age = time.time() - _DAEMON_DATA_FILE.stat().st_mtime
        if age <= _DAEMON_MAX_AGE:
            return json.loads(_DAEMON_DATA_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        pass
    nodes_raw, jobs, pending, node_jobs, gpu_alloc, err = collect_basic()
    node_names = [n["name"] for n in nodes_raw]
    ssh_results, stale_nodes, ssh_errors = collect_node_data_parallel(node_names)
    nodes = build_nodes(nodes_raw, node_jobs, ssh_results, stale_nodes)
    apply_gpu_alloc(nodes, gpu_alloc, jobs)
    return {
        "version": 1,
        "ts": datetime.now().isoformat(),
        "nodes": [asdict(n) for n in nodes],
        "jobs": [asdict(j) for j in jobs],
        "pending": [asdict(p) for p in pending],
        "stale_nodes": stale_nodes,
        "errors": " | ".join(x for x in [err] + ssh_errors if x),
    }


def _print_once(data: dict) -> None:
    print(f"sgpu {data.get('ts', '')}")
    for n in data.get("nodes", []):
        stale = " ~stale" if n.get("stale") else ""
        err = f" ERROR: {n['error']}" if n.get("error") else ""
        print(f"\n{n['name']}  {n['state']}  [{n.get('partition', '')}]  "
              f"CPU {n.get('cpu_alloc') or '0'}/{n.get('cpus', '?')}{stale}{err}")
        for g in n.get("gpus", []):
            user = ",".join(g.get("users", []))
            note = ""
            if not user and g.get("alloc_user"):
                user = g["alloc_user"]
                note = f"  [{fmt_idle_age(g.get('idle_sec', 0)).upper()}]"
            job = f"  job {g['alloc_jobid']}" if g.get("alloc_jobid") else ""
            print(f"  GPU{g.get('index', '?')}  {g.get('name', ''):<14} "
                  f"util {g.get('util', '?'):>3}%  "
                  f"{mb_to_gb(g.get('mem_used', '')):>6}/{mb_to_gb(g.get('mem_total', ''))}G"
                  f"  {user}{note}{job}")
    pending = data.get("pending", [])
    if pending:
        print(f"\nPENDING ({len(pending)}):")
        for p in pending:
            start = fmt_start_time(p.get("start_time", ""))
            start = f"  est.start {start}" if start else ""
            print(f"  {p['jobid']}  {p['user']}  x{p.get('gpu_count', 0)}  "
                  f"{p.get('partition', '')}  {p.get('reason', '')}{start}")


def _snapshot_nodes() -> List[NodeInfo]:
    """Snapshot as NodeInfo list (daemon file or direct collection)."""
    data = _oneshot_snapshot()
    nodes: List[NodeInfo] = []
    for n in data.get("nodes", []):
        gpus = [
            GpuInfo(**{k: g.get(k, d) for k, d in (
                ("index", ""), ("name", ""), ("util", ""), ("mem_used", ""),
                ("mem_total", ""), ("temp", ""), ("power", ""), ("power_cap", ""),
                ("pids", []), ("users", []), ("alloc_jobid", ""),
                ("alloc_user", ""), ("idle_sec", 0), ("parked_sec", 0),
            )})
            for g in n.get("gpus", [])
        ]
        nodes.append(NodeInfo(
            name=n.get("name", ""), state=n.get("state", ""),
            partition=n.get("partition", ""), gpus=gpus,
        ))
    return nodes


def _cli_waste() -> int:
    rows = collect_waste(_snapshot_nodes(), WASTE_MIN_SEC)
    if not rows:
        print("no wasted GPUs over threshold")
        return 0
    for r in rows:
        job = f"  job {r['jobid']}" if r["jobid"] else ""
        span = "-" if r["kind"] == "rogue" else (fmt_span(r["sec"]) or "<1m")
        print(f"{r['node']}/{r['gpu']}  {r['kind']:<7} {span:>7}  {r['user']}{job}")
    return 1  # non-zero so cron/scripts can alert on it


def _cli_usage(days: int) -> int:
    totals = load_usage_totals(days)
    if totals is None:
        print("no usage data (collector not running or too new)")
        return 1
    print(f"{'user':<14}{'alloc':>9}{'busy':>9}{'eff':>6}   (last {days}d)")
    for user, alloc, busy in totals:
        eff = busy / alloc if alloc > 0 else 0
        print(f"{user:<14}{alloc / 3600:>8.1f}h{busy / 3600:>8.1f}h{eff:>6.0%}")
    return 0


def _cli_wait_free(want: int, partition: str, interval: int) -> int:
    while True:
        free = 0
        for n in _snapshot_nodes():
            if partition and partition not in n.partition.split(","):
                continue
            free += sum(1 for g in n.gpus if classify_gpu(g) == "free")
        if free >= want:
            print(f"{free} free GPU(s) available" + (f" in {partition}" if partition else ""))
            return 0
        time.sleep(interval)


def _arg_value(argv: List[str], flag: str, default: str) -> str:
    if flag in argv:
        i = argv.index(flag)
        if i + 1 < len(argv):
            return argv[i + 1]
    return default


def main():
    argv = sys.argv[1:]
    try:
        if "--json" in argv or "--once" in argv:
            data = _oneshot_snapshot()
            if "--json" in argv:
                print(json.dumps(data, ensure_ascii=False, indent=2))
            else:
                _print_once(data)
            return
        if "--waste" in argv:
            sys.exit(_cli_waste())
        if "--usage" in argv:
            v = _arg_value(argv, "--usage", "7")
            sys.exit(_cli_usage(int(v) if v.isdigit() else 7))
        if "--wait-free" in argv:
            want = int(_arg_value(argv, "--wait-free", "1"))
            part = _arg_value(argv, "--partition", "")
            interval = int(_arg_value(argv, "--interval", "10"))
            sys.exit(_cli_wait_free(want, part, interval))
        if argv and argv[0] in ("-h", "--help"):
            print("usage: sgpu [--json | --once | --waste | --usage [days] | --wait-free N]\n"
                  "  (no args)      interactive TUI\n"
                  "  --json         print snapshot as JSON and exit\n"
                  "  --once         print snapshot as plain text and exit\n"
                  "  --waste        list idle/parked GPUs; exit 1 if any (cron-friendly)\n"
                  "  --usage [N]    per-user GPU-hours over last N days (default 7)\n"
                  "  --wait-free N  block until N GPUs are free\n"
                  "                 [--partition P] [--interval sec]")
            return
    finally:
        if argv:
            cleanup_ssh_pool()
    SlurmGpuTui().run()
