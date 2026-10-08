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

            # A metric-only change updates cells and retains the keyed cursor.
            app.tbl.move_cursor(row=1, column=0, animate=False)
            changed = deepcopy(snapshot)
            changed[0][0].gpus[0].util = "90"
            app._apply(*changed)
            await pilot.pause()
            assert gpu_clears == pending_clears == []
            assert app.tbl.get_cell("gpu_gpu1_0", "util").plain.endswith("90%")
            assert app.tbl.cursor_row == 1

            # Each table rebuilds only for its own row structure changes.
            app.sort_reverse = True
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 1
            changed[2].append(PendingJob(jobid="202", user="bob"))
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 1
            assert len(pending_clears) == 1
            app._collapsed.add("gpu1")
            app._apply(*deepcopy(changed))
            await pilot.pause()
            assert len(gpu_clears) == 2

            # Hidden live metrics do not invalidate a collapsed row while its
            # aggregate class remains the same.
            hidden_change = deepcopy(changed)
            hidden_change[0][0].gpus[0].util = "80"
            app._apply(*hidden_change)
            await pilot.pause()
            assert len(gpu_clears) == 2

            # User filtering, details columns, and node removal all change the
            # visible structure and must invalidate the fast path.
            app.filter_user = "alice"
            app._apply(*deepcopy(hidden_change))
            await pilot.pause()
            assert len(gpu_clears) == 3
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
            assert clears == []

            # Input order and job order are irrelevant because the CPU view
            # sorts nodes and aggregates cores per user.
            reordered = deepcopy(changed)
            reordered[0].reverse()
            app._apply(*reordered)
            await pilot.pause()
            assert clears == []

    asyncio.run(scenario())


def test_hidden_columns_pending_cursor_and_missing_util():
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)):
            app._auto_collapsed = True
            snapshot = _snapshot()
            app._apply(*deepcopy(snapshot))
            clears = _track_clear(app.tbl)
            snapshot[0][0].gpus[0].temp = "90"
            snapshot[0][0].gpus[0].power = "400"
            snapshot[1][0].jobname = "hidden-name"
            app._apply(*deepcopy(snapshot))
            assert clears == []
            app.pending_tbl.move_cursor(row=0, column=0, animate=False)
            snapshot[2][0].priority = "999"
            app._apply(*deepcopy(snapshot))
            assert app.pending_tbl.get_cell("pend_201", "p_pri").plain == "999"
            assert app.pending_tbl.cursor_row == 0
            assert clears == []
            snapshot[0][0].gpus[0].util = "N/A"
            app.sort_by = "util"
            app._apply(*deepcopy(snapshot))
            assert app.tbl.get_cell("gpu_gpu1_0", "util").plain == "N/A%"
            app.show_details = True
            app._setup_columns()
            app._apply(*deepcopy(snapshot))
            assert "90C" in app.tbl.get_cell("gpu_gpu1_0", "temp").plain
    asyncio.run(scenario())


def test_observation_history_deduplicates_ui_changes_and_preserves_gaps():
    from sgpu.telemetry import ObservationHistory
    nodes = _snapshot()[0]
    for node in nodes:
        node.observed_at = 100.0
    history = ObservationHistory(window=10, max_samples=3)
    history.observe(nodes, 100)
    history.observe(list(reversed(nodes)), 101)
    assert len(history.samples) == 1
    nodes[0].observed_at = 102
    nodes[0].gpus[0].util = "N/A"
    history.observe(nodes, 102)
    assert history.samples[-1][1] == 70
    for node in nodes:
        node.stale = True
    history.observe(nodes, 103)
    assert history.samples[-1][1] is None
    assert history.sparkline(1, 103).endswith("·")
    assert history.sparkline(1, 120) == "·"
    history = ObservationHistory()
    history.samples.extend((stamp, 100.0, None, None) for stamp in range(0, 301, 3))
    assert len(history.sparkline(1, 300)) == 30


def test_offline_benchmark_and_replay_never_collect(monkeypatch, tmp_path):
    import json
    from sgpu import common
    from sgpu.benchmark import run_benchmark
    monkeypatch.setattr(common.subprocess, "check_output", lambda *a, **kw: (_ for _ in ()).throw(AssertionError("cluster call")))
    result = run_benchmark(nodes=2, repeat=2)
    assert {item["case"] for item in result} >= {"missing_metric", "stale_node"}
    for item in result:
        assert item["gpu_clear_calls"] == item["pending_clear_calls"] == 0
    replay = tmp_path / "replay.json"
    replay.write_text(json.dumps({"snapshots": [{"nodes": [], "jobs": [], "pending": []}]}))
    assert run_benchmark(replay_path=str(replay), repeat=1)[0]["case"] == "replay"


def test_compact_layout_pending_trend_and_resize_keep_gpu_space():
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(80, 24)) as pilot:
            app._auto_collapsed = True
            app._apply(*_snapshot())
            await pilot.pause()
            assert app.tbl.size.height >= 8
            assert not app.pending_tbl.display and not app.trend_w.display
            assert sum(column.width + 2 for column in app.tbl.columns.values()) <= 78
            assert app._compact
            app.action_toggle_pending()
            app.action_toggle_trend()
            await pilot.pause()
            assert app.pending_tbl.display and app.trend_w.display
            app.tbl.move_cursor(row=1, column=0, animate=False)
            await pilot.resize_terminal(160, 50)
            await pilot.pause()
            assert not app._compact
            assert app.tbl.cursor_row == 1
            assert app.tbl.get_cell('gpu_gpu1_0', 'gpu_name').plain == 'H100'
    asyncio.run(scenario())


def test_jobs_tab_cpu_jobs_partial_metrics_oom_and_cursor_actions(monkeypatch):
    import time
    now = time.time()
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)) as pilot:
            snapshot = _snapshot()
            job = JobInfo(jobid='300', user='alice', node='cpu[1-2]', cpu_count=16, mem='16G',
                          elapsed='01:00', time_limit='02:00', telemetry={
                              'cpu1': {'observed_at': now, 'cpu_cores': 2.4, 'mem_current_mib': 1024,
                                       'mem_peak_mib': 2048, 'oom_kill': 1},
                              'cpu2': {'observed_at': now - 100, 'cpu_cores': 999}})
            snapshot[1].append(job)
            snapshot[0].extend([NodeInfo(name='cpu1', has_gpu=False, jobs=[job]),
                                NodeInfo(name='cpu2', has_gpu=False, jobs=[job])])
            app.current_user = 'alice'
            app._apply(*snapshot)
            await pilot.pause()
            await pilot.press('4')
            await pilot.pause()
            assert app.jobs_tbl.row_count == 4
            assert app.jobs_tbl.get_cell('job_300', 'j_cpu').plain == '~2.4/16'
            assert app.jobs_tbl.get_cell('job_300', 'j_mem').plain == '~1.0G/16G'
            assert app.jobs_tbl.get_cell('job_300', 'j_note').plain.startswith('OOM 1')
            app.jobs_tbl.move_cursor(row=2, column=0, animate=False)
            assert app._job_under_cursor() == '300'
            app.action_watch_job()
            assert app._watched['300']['state'] == 'running'
            assert '2.0G' in app._job_telemetry_detail(job)
            opened = []
            app._show_detail = lambda kind, name: opened.append((kind, name))
            await pilot.press('enter')
            await pilot.pause()
            assert opened == [('job', '300')]
            from sgpu.screens import ConfirmScreen
            await pilot.press('x')
            await pilot.pause()
            assert isinstance(app.screen, ConfirmScreen)
            await pilot.press('escape')
            await pilot.pause()
            assert app.jobs_tbl.has_focus
            app.filter_partition = 'gpu'
            app._rerender()
            assert app.jobs_tbl.row_count == 3
    asyncio.run(scenario())


def test_free_summary_excludes_stale_drain_and_recovery_nodes(monkeypatch):
    import time
    from sgpu.cells import node_gpu_classes, gpu_health_status
    now = time.time()
    def node(name, **kwargs):
        return NodeInfo(name=name, state='idle', has_gpu=True,
                        gpus=[GpuInfo(index='0', name='H100', util='0', mem_used='0', mem_total='81920')], **kwargs)
    good = node('good')
    stale = node('stale', stale=True)
    drained = node('drained')
    drained.state = 'drain'
    reset = node('reset')
    reset.gpus[0].recovery_action = 'Reset'
    reset.gpus[0].health_observed_at = now
    assert node_gpu_classes(good) == ['free']
    assert all(node_gpu_classes(other) == ['unknown'] for other in (stale, drained, reset))
    reset.gpus[0].health_observed_at = now - 100
    assert gpu_health_status(reset.gpus[0])[0] == '—'
    reset.gpus[0].health_observed_at = now
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)):
            app._apply([good, stale, drained, reset], [], [], '')
            summary = str(app.summary_w.render())
            assert 'FREE 1 (node max 1)' in summary
            assert 'H100/80G×1' in summary
    asyncio.run(scenario())


def test_pending_reason_wait_and_filters_are_shared_with_jobs_tab():
    from datetime import datetime, timedelta
    pending = PendingJob(jobid='202', user='bob', partition='other', reason='Dependency',
                         dependency='afterok:123', submit_time=(datetime.now() - timedelta(hours=2)).isoformat(),
                         start_time='2099-01-01T12:00:00')
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(160, 50)):
            snapshot = _snapshot()
            snapshot[2].append(pending)
            app._apply(*snapshot)
            assert app.pending_tbl.get_cell('pend_202', 'p_wait').plain == '2.0h'
            assert 'afterok:123' in app.pending_tbl.get_cell('pend_202', 'p_reason').plain
            app.filter_partition = 'gpu'
            app._rerender()
            assert app.pending_tbl.row_count == 1
            assert app._pending_user['202'] == 'bob'
    asyncio.run(scenario())


def test_cpu_actual_pressure_age_and_pending_click():
    import time
    async def scenario():
        app = _MountedTui()
        async with app.run_test(size=(80, 24)) as pilot:
            snapshot = _snapshot()
            node = snapshot[0][0]
            node.cpu_util = '25.0'
            node.pressure = {'cpu': '12', 'memory': '0', 'io': '2'}
            node.telemetry_observed_at = time.time()
            app._apply(*snapshot)
            await pilot.pause()
            await pilot.click('#pending-label')
            await pilot.pause()
            assert app.pending_tbl.display
            await pilot.press('2')
            await pilot.pause()
            assert app.cpu_tbl.get_cell('hdr_gpu1', 'c_actual').plain == '25'
            assert app.cpu_tbl.get_cell('hdr_gpu1', 'c_psi').plain == '12/0/2'
            node.telemetry_observed_at -= 100
            app._rerender()
            assert app.cpu_tbl.get_cell('hdr_gpu1', 'c_actual').plain == '?'
            assert app.cpu_tbl.get_cell('hdr_gpu1', 'c_psi').plain == '?/?/?'
            await pilot.press('4')
            await pilot.pause()
            assert app.jobs_tbl.row_count == 3
    asyncio.run(scenario())
