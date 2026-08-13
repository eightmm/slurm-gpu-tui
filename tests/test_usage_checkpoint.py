"""Durable usage checkpoint cadence and retry behavior."""

from sgpu import collector


def _reset_usage_state(monkeypatch):
    monkeypatch.setattr(collector, "_usage", {"days": {}})
    monkeypatch.setattr(collector, "_last_usage_ts", None)
    monkeypatch.setattr(collector, "_usage_dirty", False)
    monkeypatch.setattr(collector, "_usage_last_save", 0.0)


def _node():
    return [{"gpus": [{
        "alloc_user": "alice", "users": [], "util": "90",
        "idle_sec": 0, "parked_sec": 0,
    }]}]


def test_usage_sampling_stays_frequent_but_checkpoints_are_bounded(monkeypatch):
    _reset_usage_state(monkeypatch)
    writes = []
    monkeypatch.setattr(collector, "USAGE_SAVE_SEC", 30.0)
    monkeypatch.setattr(
        collector, "_write_state_json",
        lambda _path, payload: writes.append(payload) or True,
    )

    for tick in range(21):
        now = 1000.0 + tick * 3
        collector._accumulate_usage(_node(), now)
        collector._save_usage(monotonic_now=now)

    assert len(writes) == 2  # first sample after baseline, then every 30s
    day = next(iter(collector._usage["days"]))
    assert collector._usage["days"][day]["alice"]["alloc"] == 60.0


def test_usage_force_checkpoint_and_failed_write_retry(monkeypatch):
    _reset_usage_state(monkeypatch)
    results = iter((False, True))
    writes = []
    monkeypatch.setattr(
        collector, "_write_state_json",
        lambda _path, payload: writes.append(payload) or next(results),
    )

    collector._accumulate_usage(_node(), 1000.0)
    collector._accumulate_usage(_node(), 1003.0)
    assert collector._save_usage(force=True, monotonic_now=1003.0) is False
    assert collector._usage_dirty is True
    assert collector._save_usage(force=True, monotonic_now=1006.0) is True
    assert collector._usage_dirty is False
    assert len(writes) == 2


def test_sacct_publish_marks_usage_dirty(monkeypatch):
    _reset_usage_state(monkeypatch)
    monkeypatch.setattr(
        collector, "run_cmd",
        lambda *args, **kwargs: (
            True,
            "alice|gres/gpu=1|2026-01-01T00:00:00|2026-01-01T00:01:00",
        ),
    )

    assert collector._sacct_backfill(1767225660.0) is True
    assert collector._usage_dirty is True
