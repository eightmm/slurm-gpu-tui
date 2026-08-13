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
