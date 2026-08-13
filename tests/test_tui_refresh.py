"""Fast-path behavior for UI-only redraws."""

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
