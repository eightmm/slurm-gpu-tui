"""Modal screens, help text, and the Korean-IME key map."""
from __future__ import annotations

import os
import re
from typing import List, Tuple

from rich.syntax import Syntax
from rich.text import Text
from textual import work
from textual.app import ComposeResult
from textual.containers import Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import DataTable, OptionList, Static, TabbedContent, TabPane
from textual.widgets.option_list import Option

from .cells import _waste_thr, fmt_span
from .common import run_cmd, tail_file

# ── Modal screens ─────────────────────────────────────────────────────────

# Lines that usually mean "this is why your job died". Case-sensitive on
# purpose — training logs are full of benign lowercase "error" metrics.
_LOG_ERR_RE = re.compile(
    r"Traceback \(most recent call last\)"
    r"|CUDA out of memory|CUDA error"
    r"|(?:srun|slurmstepd): error"
    r"|Segmentation fault|\(core dumped\)|Killed"
    r"|\w*(?:Error|Exception)\b|ERROR|error:"
    r"|\bFAILED\b|[Oo]ut of memory"
)


def _log_text(text: str) -> Text:
    """Log tail as Text; error-looking lines in red."""
    out = Text()
    for line in text.splitlines(keepends=True):
        out.append(line, style="bold red" if _LOG_ERR_RE.search(line) else "")
    return out

class DetailScreen(ModalScreen):
    """Modal for a job or node: scontrol info, plus Script/StdOut/StdErr tabs for jobs."""

    BINDINGS = [
        ("escape", "close", "Close"),
        ("q", "close", "Close"),
        ("enter", "close", "Close"),
        ("tab", "switch_tab", "Next tab"),
    ]
    CSS = """
    DetailScreen { align: center middle; }
    #detail-box {
        width: 90%; height: 85%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    #detail-title { text-style: bold; color: $accent; height: 1; }
    DetailScreen TabbedContent { height: 1fr; }
    DetailScreen VerticalScroll { height: 1fr; }
    """

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def __init__(self, title: str, body: str, script: str = "", script_src: str = "",
                 stdout_text: str = "", stdout_path: str = "",
                 stderr_text: str = "", stderr_path: str = "") -> None:
        super().__init__()
        self._title = title
        self._body = body
        self._stdout_path = stdout_path
        self._stderr_path = stderr_path
        self._log_scrolled: set = set()  # log tabs already auto-scrolled to end
        # (tab id, tab title, renderable) — Text() so shell scripts and logs
        # with [brackets] aren't parsed as markup
        self._tabs: List[Tuple[str, str, object]] = [("tab-info", "Job Info", Text(body))]
        if script:
            self._tabs.append(("tab-script", f"Script ({script_src})",
                               Syntax(script, "bash", line_numbers=True, word_wrap=True)))
        if stdout_path:
            self._tabs.append(("tab-stdout", "StdOut", self._log_render(stdout_path, stdout_text)))
        if stderr_path:
            self._tabs.append(("tab-stderr", "StdErr", self._log_render(stderr_path, stderr_text)))

    @staticmethod
    def _log_render(path: str, text: str) -> Text:
        log = Text(f"{path}\n\n", style="dim")
        log.append(_log_text(text))
        return log

    def compose(self) -> ComposeResult:
        with Vertical(id="detail-box"):
            yield Static(self._title, id="detail-title")
            if len(self._tabs) > 1:
                with TabbedContent(initial="tab-info"):
                    for tab_id, tab_title, content in self._tabs:
                        with TabPane(tab_title, id=tab_id):
                            with VerticalScroll(id=f"scroll-{tab_id}"):
                                yield Static(content, id=f"body-{tab_id}")
            else:
                with VerticalScroll():
                    yield Static(Text(self._body))

    def on_mount(self) -> None:
        if self._stdout_path or self._stderr_path:
            self.set_interval(3.0, self._poll_logs)

    def on_tabbed_content_tab_activated(self, event: TabbedContent.TabActivated) -> None:
        # first visit to a log tab jumps to the end, where the fresh output is
        tab_id = event.pane.id or ""
        if tab_id in ("tab-stdout", "tab-stderr") and tab_id not in self._log_scrolled:
            self._log_scrolled.add(tab_id)
            try:
                self.query_one(f"#scroll-{tab_id}", VerticalScroll).scroll_end(animate=False)
            except Exception:
                pass

    @work(thread=True, exclusive=True)
    def _poll_logs(self) -> None:
        """Re-read log tails so open modals behave like tail -f."""
        updates = []
        for path, tab_id in ((self._stdout_path, "tab-stdout"),
                             (self._stderr_path, "tab-stderr")):
            if path:
                updates.append((tab_id, self._log_render(path, tail_file(path))))
        if updates:
            self.app.call_from_thread(self._apply_log_updates, updates)

    def _apply_log_updates(self, updates: List[Tuple[str, Text]]) -> None:
        for tab_id, content in updates:
            try:
                vs = self.query_one(f"#scroll-{tab_id}", VerticalScroll)
                at_end = vs.scroll_offset.y >= vs.max_scroll_y - 1
                self.query_one(f"#body-{tab_id}", Static).update(content)
                if at_end:
                    vs.scroll_end(animate=False)
            except Exception:
                pass

    def action_switch_tab(self) -> None:
        try:
            tc = self.query_one(TabbedContent)
        except Exception:
            return
        ids = [t[0] for t in self._tabs]
        try:
            i = ids.index(tc.active)
        except ValueError:
            i = -1
        tc.active = ids[(i + 1) % len(ids)]

    def action_close(self) -> None:
        self.app.pop_screen()


HELP_TEXT = """\
 1/2/3    Tabs: GPU / CPU / Usage  (g also opens Usage)
 r        Refresh now
 s        Cycle sort: Node → Utilization → User → Free
 S        Reverse current sort order
 z        Collapse / expand ALL nodes
 u        Filter by user (pick from list; u again clears)
 p        Cycle partition filter (all → each partition)
 m        My jobs only (m again clears)
 i        Idle filter (truly free GPUs only)
 d        Detail columns (Temp / Power / JobID / JobName)
 Space    Collapse / expand node (on header row)
 Enter    Job / node details — Tab cycles Info/Script/StdOut/StdErr
 /        Search node or user (Esc clears)
 w        Wasted GPUs (idle / parked, worst first)
 h        My job history (7d) — Enter: state, exit code, logs
 n        Watch job under cursor — toast when it starts/ends
 x        Cancel job under cursor (your own only, asks first)
 j / k    Cursor down / up
 e        Export snapshot JSON
 ?        This help
 q        Quit

 Toasts appear when your jobs start/finish and when a
 node goes down or recovers (while the TUI is open).

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
 (ㅂ=q, ㄱ=r, ㄴ=s, ㅇ=d, ㅑ=i, ㅕ=u, ㅔ=p, ㅡ=m, ㄷ=e, ㅈ=w,
  ㅎ=g, ㅗ=h, ㅜ=n, ㅓ=j, ㅏ=k)
"""


_STATE_STYLE = {
    "COMPLETED": "green", "RUNNING": "cyan", "PENDING": "blue",
    "FAILED": "bold red", "OUT_OF_MEMORY": "bold red", "NODE_FAIL": "bold red",
    "CANCELLED": "yellow", "TIMEOUT": "magenta", "PREEMPTED": "magenta",
}

_SACCT_DETAIL_FMT = ("JobID,JobName,State,ExitCode,Reason,Submit,Start,End,Elapsed,"
                     "Timelimit,Partition,NodeList,AllocTRES,ReqMem,MaxRSS,MaxVMSize,"
                     "TotalCPU,WorkDir,SubmitLine")


def _fmt_sacct_detail(raw: str) -> str:
    """--parsable2 sacct rows as aligned key/value blocks (one per step)."""
    lines = raw.strip().splitlines()
    if len(lines) < 2:
        return raw
    hdr = lines[0].split("|")
    blocks = []
    for line in lines[1:]:
        pairs = [(h, v) for h, v in zip(hdr, line.split("|")) if v]
        if not pairs:
            continue
        w = max(len(h) for h, _ in pairs)
        blocks.append("\n".join(f"  {h:<{w}}  {v}" for h, v in pairs))
    return "\n\n".join(blocks)


class HistoryScreen(ModalScreen):
    """My recent jobs from slurmdbd: what ran, what died, and why."""

    BINDINGS = [
        ("escape", "close", "Close"),
        ("q", "close", "Close"),
        ("h", "close", "Close"),
    ]
    CSS = """
    HistoryScreen { align: center middle; }
    #hist-box {
        width: 96%; height: 85%;
        border: round $primary; background: $surface; padding: 1 2;
    }
    #hist-title { text-style: bold; color: $accent; height: 1; }
    HistoryScreen DataTable { height: 1fr; }
    """

    def __init__(self, user: str, days: int, rows: List[dict], error: str = "") -> None:
        super().__init__()
        self._user = user
        self._days = days
        self._rows = rows
        self._error = error

    def compose(self) -> ComposeResult:
        with Vertical(id="hist-box"):
            yield Static(f"Job history — {self._user}, last {self._days}d "
                         f"({len(self._rows)} jobs) · Enter = details", id="hist-title")
            if self._error:
                yield Static(Text(f"sacct failed: {self._error}", style="bold red"))
            elif not self._rows:
                yield Static(Text(f"no jobs in the last {self._days} day(s)", style="dim"))
            yield DataTable(id="hist-tbl")

    def on_mount(self) -> None:
        t = self.query_one(DataTable)
        t.cursor_type = "row"
        t.add_columns("JobID", "State", "Exit", "Elapsed", "End", "GPUs", "Partition", "Name")
        for r in self._rows:
            style = _STATE_STYLE.get(r["state"], "white")
            t.add_row(
                Text(r["jobid"], style="bold"),
                Text(r["state"], style=style),
                Text(r["exit"], style="red" if r["exit"] not in ("0:0", "") else "dim"),
                r["elapsed"],
                Text(r["end"], style="dim"),
                str(r["gpus"]) if r["gpus"] else "-",
                r["part"],
                r["name"][:40],
                key=r["jobid"],
            )
        t.focus()

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        self._show_job(str(event.row_key.value))

    @work(thread=True)
    def _show_job(self, jid: str) -> None:
        """sacct detail (alloc + steps); default-named logs when still on disk."""
        ok, raw = run_cmd(f"sacct -j {jid} --parsable2 --format={_SACCT_DETAIL_FMT}",
                          timeout=20)
        body = _fmt_sacct_detail(raw) if ok else f"sacct failed: {raw}"
        # completed jobs: slurmdbd doesn't record stdout paths, but the default
        # name (slurm-<jobid>.out in WorkDir) covers jobs that didn't set -o/-e
        stdout_path = stderr_path = stdout_text = stderr_text = ""
        m = re.search(r"^\s*WorkDir\s+(\S+)", body, re.M)
        if m:
            for cand, is_err in ((f"slurm-{jid}.out", False), (f"slurm-{jid}.err", True)):
                p = os.path.join(m.group(1), cand)
                try:
                    exists = os.path.exists(p)
                except OSError:
                    exists = False
                if exists:
                    if is_err:
                        stderr_path, stderr_text = p, tail_file(p)
                    else:
                        stdout_path, stdout_text = p, tail_file(p)
        self.app.call_from_thread(
            self.app.push_screen,
            DetailScreen(f"job {jid} (history)", body,
                         stdout_text=stdout_text, stdout_path=stdout_path,
                         stderr_text=stderr_text, stderr_path=stderr_path),
        )

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def action_close(self) -> None:
        self.app.pop_screen()


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
            style = {"idle": "yellow", "parked": "blue", "rogue": "red", "no-gres": "red"}.get(r["kind"], "white")
            body.append(f" {r['kind']:<7}", style=f"bold {style}")
            span = "-" if r["kind"] in ("rogue", "no-gres") else (fmt_span(r["sec"]) or "<1m")
            body.append(f" {span:>7} ", style="bold")
            body.append(f" {r['user']:<12}", style="magenta")
            if r["jobid"]:
                body.append(f" job {r['jobid']}", style="dim")
            body.append("\n")
        with VerticalScroll(id="waste-box"):
            yield Static(Text(f"Wasted GPUs (≥{_waste_thr()}) — idle: reserved w/o process · parked: VRAM held at 0% util", style="bold"))
            yield Static(body)

    def on_key(self, event) -> None:
        if getattr(event, "character", None) == "ㅂ":
            event.stop()
            self.action_close()

    def action_close(self) -> None:
        self.app.pop_screen()


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


class ConfirmScreen(ModalScreen):
    """Yes/no confirmation; dismisses with a bool."""

    BINDINGS = [
        ("escape", "no", "No"), ("n", "no", "No"),
        ("y", "yes", "Yes"), ("enter", "yes", "Yes"),
    ]
    CSS = """
    ConfirmScreen { align: center middle; }
    #confirm-box {
        width: 60; height: auto;
        border: round $error; background: $surface; padding: 1 2;
    }
    """

    def __init__(self, message: str) -> None:
        super().__init__()
        self._message = message

    def compose(self) -> ComposeResult:
        with Vertical(id="confirm-box"):
            yield Static(Text(self._message, style="bold"))
            yield Static(Text("y/Enter = yes · n/Esc = no", style="dim"))

    def action_yes(self) -> None:
        self.dismiss(True)

    def action_no(self) -> None:
        self.dismiss(False)


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
    "ㄴ": "toggle_sort",   # s
    "ㅕ": "toggle_user_filter",  # u
    "ㅔ": "toggle_partition_filter",  # p
    "ㅡ": "toggle_my_filter",    # m
    "ㅑ": "toggle_idle_filter",  # i
    "ㅇ": "toggle_details",      # d
    "ㄷ": "export_json",         # e
    "ㅈ": "show_waste",          # w
    "ㅗ": "show_history",        # h
    "ㅜ": "watch_job",           # n
    "ㅎ": "show_usage",          # g
    "ㅓ": "cursor_down",         # j
    "ㅏ": "cursor_up",           # k
}

