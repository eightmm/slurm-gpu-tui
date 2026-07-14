"""Modal screens, help text, and the Korean-IME key map."""
from __future__ import annotations

from typing import List, Tuple

from rich.syntax import Syntax
from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import OptionList, Static, TabbedContent, TabPane
from textual.widgets.option_list import Option

from .cells import _waste_thr, fmt_span

# ── Modal screens ─────────────────────────────────────────────────────────

class DetailScreen(ModalScreen):
    """Modal for a job or node: scontrol info, plus a Script tab for jobs."""

    BINDINGS = [
        ("escape", "close", "Close"),
        ("q", "close", "Close"),
        ("enter", "close", "Close"),
        ("tab", "switch_tab", "Info/Script"),
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

    def __init__(self, title: str, body: str, script: str = "", script_src: str = "") -> None:
        super().__init__()
        self._title = title
        self._body = body
        self._script = script
        self._script_src = script_src

    def compose(self) -> ComposeResult:
        with Vertical(id="detail-box"):
            yield Static(self._title, id="detail-title")
            # Text() so shell scripts with [brackets] aren't parsed as markup
            if self._script:
                with TabbedContent(initial="tab-info"):
                    with TabPane("Job Info", id="tab-info"):
                        with VerticalScroll():
                            yield Static(Text(self._body))
                    with TabPane(f"Script ({self._script_src})", id="tab-script"):
                        with VerticalScroll():
                            yield Static(Syntax(self._script, "bash",
                                                line_numbers=True, word_wrap=True))
            else:
                with VerticalScroll():
                    yield Static(Text(self._body))

    def action_switch_tab(self) -> None:
        try:
            tc = self.query_one(TabbedContent)
        except Exception:
            return
        tc.active = "tab-script" if tc.active == "tab-info" else "tab-info"

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
 Enter    Job / node details — Tab switches Info/Script
 /        Search node or user (Esc clears)
 w        Wasted GPUs (idle / parked, worst first)
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
 (ㅂ=q, ㄱ=r, ㄴ=s, ㅇ=d, ㅑ=i, ㅕ=u, ㅔ=p, ㅡ=m, ㄷ=e, ㅈ=w, ㅎ=g, ㅓ=j, ㅏ=k)
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
    "ㅎ": "show_usage",          # g
    "ㅓ": "cursor_down",         # j
    "ㅏ": "cursor_up",           # k
}

