"""Fast-path behavior for UI-only redraws and stable table models."""

import asyncio
from copy import deepcopy

from textual.widgets import TabbedContent

from sgpu.common import GpuInfo, JobInfo, NodeInfo, PendingJob
from sgpu.tui import SlurmGpuTui


class _FakeTui:
    def __init__(self, snapshot):
        self._last_applied = snapshot
        self._force_render = False
        self.applied = []
        self.refreshes = 0

    def _apply(self, *snapshot):
        self.applied.append(snapshot)

    def refresh_all(self):
        self.refreshes += 1


class _FakeTimer:
    def __init__(self, callback):
        self.callback = callback
        self.stopped = False

    def stop(self):
        self.stopped = True


class _SearchTui:
    _cancel_search_rerender = SlurmGpuTui._cancel_search_rerender
    _finish_search_rerender = SlurmGpuTui._finish_search_rerender
    _schedule_search_rerender = SlurmGpuTui._schedule_search_rerender

    def __init__(self):
        self._search_timer = None
        self.timers = []
        self.renders = 0

    def set_timer(self, delay, callback):
        timer = _FakeTimer(callback)
        self.timers.append((delay, timer))
        return timer

    def _rerender(self):
        self.renders += 1


def test_ui_rerender_reuses_last_parsed_snapshot():
    snapshot = ([], [], [], "")
    tui = _FakeTui(snapshot)

    SlurmGpuTui._rerender(tui)

    assert tui.applied == [snapshot]
    assert tui.refreshes == 0
    assert tui._force_render is False


def test_ui_rerender_collects_when_no_snapshot_exists():
    tui = _FakeTui(None)

    SlurmGpuTui._rerender(tui)

    assert tui.applied == []
    assert tui.refreshes == 1
    assert tui._force_render is True


def test_manual_refresh_still_forces_fresh_collection():
    tui = _FakeTui(([], [], [], ""))

    SlurmGpuTui.action_refresh(tui)

    assert tui.applied == []
    assert tui.refreshes == 1
    assert tui._force_render is True


def test_search_rerender_coalesces_rapid_changes():
    tui = _SearchTui()

    tui._schedule_search_rerender()
    tui._schedule_search_rerender()

    assert tui.timers[0][1].stopped is True
    assert tui.timers[1][0] == 0.12
    tui.timers[1][1].callback()
    assert tui.renders == 1
    assert tui._search_timer is None


class _MountedTui(SlurmGpuTui):
    """Real Textual widgets without starting a collector worker."""

    def refresh_all(self):
        pass


def _snapshot():
    jobs = [
        JobInfo(
            jobid="101", user="alice", partition="gpu", jobname="train-a",
            elapsed="01:00", node="gpu1", gpu_count=1, cpu_count=8,
            time_limit="1-00:00:00",
        ),
        JobInfo(
            jobid="102", user="bob", partition="gpu", jobname="train-b",
            elapsed="02:00", node="gpu2", gpu_count=1, cpu_count=4,
            time_limit="1-00:00:00",
        ),
    ]
    nodes = [
        NodeInfo(
            name="gpu1", state="mix", partition="gpu", has_gpu=True,
            cpus="64", cpu_alloc="8", cpu_load="2", mem_total="1000",
            mem_avail="700", jobs=[jobs[0]],
            gpus=[GpuInfo(
                index="0", name="H100", util="60", mem_used="100",
                mem_total="1000", users=["alice"], alloc_jobid="101",
                alloc_user="alice",
            )],
        ),
        NodeInfo(
            name="gpu2", state="mix", partition="gpu", has_gpu=True,
            cpus="64", cpu_alloc="4", cpu_load="1", mem_total="1000",
            mem_avail="800", jobs=[jobs[1]],
            gpus=[GpuInfo(
                index="0", name="H100", util="70", mem_used="200",
                mem_total="1000", users=["bob"], alloc_jobid="102",
                alloc_user="bob",
            )],
        ),
    ]
    pending = [PendingJob(
        jobid="201", user="alice", partition="gpu", jobname="next",
        gpu_count=1, reason="Priority", priority="10",
        start_time="2099-01-01T12:00:00",
    )]
    return nodes, jobs, pending, ""


def _track_clear(table):
    calls = []
    original = table.clear

    def tracked(*args, **kwargs):
        calls.append((args, kwargs))
        return original(*args, **kwargs)

    table.clear = tracked
    return calls


def test_gpu_table_skips_identical_view_and_rebuilds_edge_changes():
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)) as pilot:
            app._auto_collapsed = True
            snapshot = _snapshot()
            app._apply(*deepcopy(snapshot))
            await pilot.pause()
            gpu_clears = _track_clear(app.tbl)
            pending_clears = _track_clear(app.pending_tbl)

            # A newly parsed but value-identical snapshot must not touch either
            # DataTable, and detail/cancel lookup maps remain available.
            app._apply(*deepcopy(snapshot))
            await pilot.pause()
            assert gpu_clears == pending_clears == []
            assert app._row_job["gpu_gpu1_0"] == "101"
            assert app._pending_user == {"201": "alice"}

            # A displayed metric change uses the compatibility rebuild path and
            # retains the cursor's keyed row.
            app.tbl.move_cursor(row=1, column=0, animate=False)
            changed = deepcopy(snapshot)
            changed[0][0].gpus[0].util = "90"
            app._apply(*changed)
            await pilot.pause()
            assert len(gpu_clears) == len(pending_clears) == 1
            assert app.tbl.cursor_row == 1

            # Reordering, pending-row addition, and collapse are structural and
            # therefore intentionally fall back to a complete rebuild.
            app.sort_reverse = True
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 2
            changed[2].append(PendingJob(jobid="202", user="bob"))
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 3
            app._collapsed.add("gpu1")
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 4

            # Hidden live metrics do not invalidate a collapsed row while its
            # aggregate class remains the same.
            hidden_change = deepcopy(changed)
            hidden_change[0][0].gpus[0].util = "80"
            app._apply(*hidden_change)
            await pilot.pause()
            assert len(gpu_clears) == 4

            # User filtering, details columns, and node removal all change the
            # visible structure and must invalidate the fast path.
            app.filter_user = "alice"
            app._apply(*deepcopy(hidden_change))
            await pilot.pause()
            assert len(gpu_clears) == 5
            assert app.tbl.row_count == 1  # collapsed gpu1 header

            app.filter_user = ""
            app.show_details = True
            before_details = len(gpu_clears)
            app._setup_columns()
            app._apply(*deepcopy(hidden_change))
            await pilot.pause()
            # One clear replaces columns; the second rebuilds the new rows.
            assert len(gpu_clears) == before_details + 2

            removed_copy = deepcopy(hidden_change)
            removed = (
                [node for node in removed_copy[0] if node.name != "gpu2"],
                removed_copy[1], removed_copy[2], removed_copy[3],
            )
            before_remove = len(gpu_clears)
            app._apply(*removed)
            await pilot.pause()
            assert len(gpu_clears) == before_remove + 1
            assert all("gpu2" not in str(key.value) for key in app.tbl.rows)

    asyncio.run(scenario())


def test_cpu_table_skips_identical_view_and_rebuilds_display_change():
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)) as pilot:
            app._auto_collapsed = True
            app.query_one("#main-tabs", TabbedContent).active = "pane-cpu"
            snapshot = _snapshot()
            app._apply(*deepcopy(snapshot))
            await pilot.pause()
            clears = _track_clear(app.cpu_tbl)

            app._apply(*deepcopy(snapshot))
            await pilot.pause()
            assert clears == []

            changed = deepcopy(snapshot)
            changed[0][0].cpu_load = "40"
            app._apply(*changed)
            await pilot.pause()
            assert len(clears) == 1

            # Input order and job order are irrelevant because the CPU view
            # sorts nodes and aggregates cores per user.
            reordered = deepcopy(changed)
            reordered[0].reverse()
            app._apply(*reordered)
            await pilot.pause()
            assert len(clears) == 1

    asyncio.run(scenario())
