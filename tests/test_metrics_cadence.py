"""Prometheus textfile generation cadence."""

from sgpu import collector


def test_metrics_write_is_rate_limited_and_forceable(tmp_path, monkeypatch):
    writes = []
    monkeypatch.setattr(collector, "METRICS_FILE", tmp_path / "metrics.prom")
    monkeypatch.setattr(collector, "METRICS_REFRESH_SEC", 15.0)
    monkeypatch.setattr(collector, "_metrics_last_write", 0.0)
    monkeypatch.setattr(collector, "_format_metrics", lambda _data: "metric 1\n")
    monkeypatch.setattr(collector, "_master_host_lines", lambda: [])
    monkeypatch.setattr(
        collector, "atomic_write",
        lambda path, text, mode=0o644: writes.append((path, text, mode)),
    )

    assert collector._write_metrics({}, monotonic_now=100.0) is True
    assert collector._write_metrics({}, monotonic_now=114.9) is False
    assert collector._write_metrics({}, monotonic_now=115.0) is True
    assert collector._write_metrics({}, force=True, monotonic_now=116.0) is True
    assert len(writes) == 3


def test_metrics_failure_retries_without_waiting(tmp_path, monkeypatch):
    calls = []
    monkeypatch.setattr(collector, "METRICS_FILE", tmp_path / "metrics.prom")
    monkeypatch.setattr(collector, "_metrics_last_write", 0.0)
    monkeypatch.setattr(collector, "_format_metrics", lambda _data: "metric 1\n")
    monkeypatch.setattr(collector, "_master_host_lines", lambda: [])

    def fail_once(*args, **kwargs):
        calls.append(args)
        if len(calls) == 1:
            raise OSError("disk full")

    monkeypatch.setattr(collector, "atomic_write", fail_once)

    assert collector._write_metrics({}, monotonic_now=100.0) is False
    assert collector._write_metrics({}, monotonic_now=101.0) is True
    assert len(calls) == 2


def test_cycle_measures_storage_and_notify(monkeypatch):
    from types import SimpleNamespace
    from queue import Queue
    clock = [100.0]
    monkeypatch.setattr(collector.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(collector, "_last_cycle_started", 90)
    monkeypatch.setattr(collector, "_cycle_stats", {})
    monkeypatch.setattr(collector, "_notification_log_sources", {"42": (123, ("private", ""))})
    def delay(seconds, result=None):
        clock[0] += seconds
        return result
    monkeypatch.setattr(collector, "collect_all", lambda **kw: delay(1, {}))
    monkeypatch.setattr(collector, "atomic_write", lambda *a, **kw: delay(2))
    monkeypatch.setattr(collector, "_save_idle_state", lambda: delay(3))
    monkeypatch.setattr(collector, "_maybe_backfill_sacct", lambda *a: None)
    monkeypatch.setattr(collector, "_save_usage", lambda: delay(4))
    monkeypatch.setattr(collector, "_write_metrics", lambda *a: delay(5))
    seen = []
    notifier = SimpleNamespace(_queue=Queue(), _outcome_queue=Queue(),
                               process=lambda data, **kw: (seen.append(kw), delay(6)))
    _, diagnostics = collector._collect_cycle(notifier)
    assert diagnostics["cycle_seconds"] == 21
    assert diagnostics["interval_seconds"] == 10
    assert diagnostics["phase_seconds"] == {"collect": 1, "write": 2, "idle": 3, "usage": 4, "metrics": 5, "notify": 6}
    assert seen == [{"job_log_sources": {"42": (123, ("private", ""))}}]


def test_self_metrics_have_bounded_labels():
    data = {"nodes": [], "collector": {"phase_seconds": {"notify": 2, "private-path": 9},
            "ssh_inflight": 4, "rpc": {"squeue": {"calls": 3, "failures": 1, "seconds": 2, "last_seconds": 1}}}}
    text = collector._format_metrics(data)
    assert 'sgpu_collector_phase_seconds{phase="notify"} 2' in text
    assert "private-path" not in text
    assert 'sgpu_command_calls_total{command="squeue"} 3' in text
    assert "sgpu_collector_ssh_inflight 4" in text
