"""Read-only GPU capacity audit: consistent accounting, privacy, and uncertainty."""
import json
from datetime import date

import pytest

from sgpu.audit import build_audit, render_audit_markdown
from sgpu.cli import _build_parser


AS_OF = date(2026, 11, 12)


def _sample():
    return {
        "days": {
            "2026-11-11": {
                "alice": {"alloc": 7200, "observed_alloc": 7200, "busy": 3600, "waste": 1800},
                "bob": {"alloc": 3600, "observed_alloc": 3600, "busy": 1800, "waste": 900},
            },
            "2026-10-01": {
                "old_user": {"alloc": 86400, "busy": 0, "waste": 86400},
            },
        },
        "sacct_days": {
            "2026-11-11": {"alice": 10800, "bob": 3600, "charlie": 1800},
        },
        "meta": {"2026-11-11": 3600},
        "telemetry": {
            "2026-11-11": {"fresh_gpu_sec": 10800, "stale_gpu_sec": 3600},
        },
    }


def test_audit_does_not_equate_backfilled_alloc_with_observed_gpu_time():
    report = build_audit(
        _sample(), days=2, as_of=AS_OF, rate=2.0, reclaimable=0.5,
    )
    summary = report["summary"]
    assert report["window"] == {"start": "2026-11-11", "end": "2026-11-12", "days": 2}
    assert summary["allocated_gpu_hours"] == 4.5  # includes sacct-only charlie
    assert summary["observed_allocated_gpu_hours"] == 3.0
    assert summary["observed_busy_gpu_hours"] == 1.5
    assert summary["sampled_efficiency"] == 0.5
    assert summary["detected_waste_gpu_hours"] == 0.75
    assert summary["telemetry_fresh_ratio"] == 0.75
    assert report["scenario"]["modeled_reclaimable_gpu_hours"] == 0.375
    assert report["scenario"]["modeled_capacity_value"] == 0.75
    assert "users" not in report
    assert "alice" not in json.dumps(report)
    assert "realized cash savings" in render_audit_markdown(report)


def test_no_default_cost_assumption_or_user_identifiers():
    report = build_audit(_sample(), days=2, as_of=AS_OF)
    assert report["scenario"]["modeled_capacity_value"] is None
    assert report["scenario"]["gpu_hour_rate"] is None
    rendered = render_audit_markdown(report)
    assert "Not calculated" in rendered
    assert "alice" not in rendered
    assert "old_user" not in rendered


def test_explicit_user_details_dont_leak_outside_window():
    report = build_audit(_sample(), days=1, as_of=AS_OF, include_users=True)
    assert [x["user"] for x in report["users"]] == ["alice", "bob", "charlie"]
    assert "old_user" not in render_audit_markdown(report)


def test_only_sacct_no_sampling_does_not_claim_zero_efficiency():
    report = build_audit({
        "sacct_days": {"2026-11-12": {"alice": 7200}},
    }, days=1, as_of=AS_OF)
    assert report["summary"]["allocated_gpu_hours"] == 2
    assert report["summary"]["sampled_efficiency"] is None
    assert report["summary"]["telemetry_fresh_ratio"] is None
    assert any("No observed allocation" in x for x in report["limitations"])


@pytest.mark.parametrize("kwargs", [
    {"days": 0},
    {"days": 367},
    {"rate": 2.0},
    {"reclaimable": 0.5},
    {"rate": -1.0, "reclaimable": 0.5},
    {"rate": float("nan"), "reclaimable": 0.5},
    {"rate": 2.0, "reclaimable": 1.1},
    {"rate": 2.0, "reclaimable": float("inf")},
    {"currency": "USD|oops"},
])
def test_bad_assumptions_rejected(kwargs):
    with pytest.raises(ValueError):
        build_audit(_sample(), as_of=AS_OF, **kwargs)


def test_parser_accepts_read_only_audit_command():
    args = _build_parser().parse_args([
        "audit", "--days", "30", "--rate", "3", "--reclaimable", ".25",
        "--currency", "KRW", "--format", "json",
    ])
    assert args.cmd == "audit"
    assert args.days == 30
    assert args.rate == 3.0
    assert args.reclaimable == .25
    assert args.currency == "KRW"
    assert args.format == "json"


def test_cli_audit_offline_never_needs_collector_or_ssh(tmp_path, monkeypatch, capsys):
    from sgpu.cli import main
    path = tmp_path / "usage.json"
    path.write_text(json.dumps(_sample()), encoding="utf-8")
    monkeypatch.setattr("sys.argv", [
        "sgpu", "audit", "--input", str(path), "--days", "40", "--format", "json",
    ])
    monkeypatch.setattr("sgpu.cli._oneshot_snapshot", lambda: pytest.fail("SSH sweep"))
    main()
    report = json.loads(capsys.readouterr().out)
    assert report["report_type"] == "historical_gpu_usage_audit"
    assert report["window"]["days"] == 40
    assert "users" not in report
