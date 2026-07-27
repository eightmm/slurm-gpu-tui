"""usage.json merge rules and state-file discovery.

alloc has two sources — 3s sampling and a slurmdbd backfill — and the rule for
combining them (plus which of busy/waste is sampling-only) used to be written
out three separate times. This is the accounting the monthly report and the
Usage tab both bill people from.
"""
import json
from datetime import datetime, timedelta

import pytest

from sgpu import usage as usage_mod
from sgpu.usage import load_usage_daily, load_usage_totals, merge_usage_window


def day(offset=0):
    return (datetime.now() - timedelta(days=offset)).strftime("%Y-%m-%d")


ALL = lambda _d: True  # noqa: E731


@pytest.fixture(autouse=True)
def _clear_cache():
    usage_mod._usage_cache = (None, None)
    yield
    usage_mod._usage_cache = (None, None)


def write_usage(dir_, payload):
    dir_.mkdir(parents=True, exist_ok=True)
    (dir_ / "usage.json").write_text(json.dumps(payload))


# ── merge rule ────────────────────────────────────────────────────────────

def test_sacct_wins_when_it_saw_more_than_sampling():
    # slurmdbd keeps counting while the collector is down.
    users, _ = merge_usage_window({
        "days": {"2026-07-01": {"alice": {"alloc": 100, "busy": 60}}},
        "sacct_days": {"2026-07-01": {"alice": 900.0}},
    }, ALL)
    alloc, busy, sampled_alloc, _waste = users["alice"]
    assert (alloc, busy, sampled_alloc) == (900.0, 60, 100)


def test_sampling_wins_when_slurmdbd_has_not_flushed_yet():
    users, _ = merge_usage_window({
        "days": {"2026-07-01": {"alice": {"alloc": 900, "busy": 60}}},
        "sacct_days": {"2026-07-01": {"alice": 100.0}},
    }, ALL)
    assert users["alice"][0] == 900


def test_efficiency_uses_sampled_alloc_not_merged_alloc():
    # busy is sampling-only, so dividing it by a backfilled alloc would
    # under-report everyone whose jobs ran while the collector was down.
    users, _ = merge_usage_window({
        "days": {"2026-07-01": {"alice": {"alloc": 100, "busy": 100}}},
        "sacct_days": {"2026-07-01": {"alice": 1000.0}},
    }, ALL)
    alloc, busy, sampled_alloc, _ = users["alice"]
    assert busy / sampled_alloc == 1.0
    assert busy / alloc == 0.1


def test_user_present_only_in_sacct_is_still_counted():
    users, _ = merge_usage_window(
        {"days": {}, "sacct_days": {"2026-07-01": {"bob": 500.0}}}, ALL)
    assert users["bob"] == [500.0, 0.0, 0.0, 0.0]


def test_waste_accumulates_across_days():
    users, _ = merge_usage_window({"days": {
        "2026-07-01": {"alice": {"alloc": 10, "busy": 1, "waste": 4}},
        "2026-07-02": {"alice": {"alloc": 10, "busy": 1, "waste": 6}},
    }}, ALL)
    assert users["alice"][3] == 10


def test_window_predicate_excludes_days():
    users, daily = merge_usage_window({"days": {
        "2026-07-01": {"alice": {"alloc": 10}},
        "2026-08-01": {"alice": {"alloc": 20}},
    }}, lambda d: d.startswith("2026-08"))
    assert users["alice"][0] == 20 and set(daily) == {"2026-08-01"}


def test_daily_totals_sum_all_users():
    _users, daily = merge_usage_window({"days": {"2026-07-01": {
        "alice": {"alloc": 10, "busy": 5}, "bob": {"alloc": 20, "busy": 1},
    }}}, ALL)
    assert daily["2026-07-01"] == [30, 6]


def test_malformed_sections_do_not_raise():
    assert merge_usage_window({"days": None, "sacct_days": "nope"}, ALL) == ({}, {})


# ── loaders ───────────────────────────────────────────────────────────────

def test_load_usage_totals_sorts_by_alloc_desc(tmp_path, monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", str(tmp_path))
    write_usage(tmp_path, {"days": {day(): {
        "alice": {"alloc": 10, "busy": 5}, "bob": {"alloc": 99, "busy": 1},
    }}, "meta": {day(): 3600}})
    totals, covered, sacct_ts = load_usage_totals(7)
    assert [t[0] for t in totals] == ["bob", "alice"]
    assert covered == 3600 and sacct_ts is None


def test_load_usage_totals_reports_sacct_ts_only_when_backfilled(tmp_path, monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", str(tmp_path))
    write_usage(tmp_path, {"days": {}, "sacct_days": {day(): {"alice": 5.0}},
                           "sacct_ts": 1234.0})
    assert load_usage_totals(7)[2] == 1234.0


def test_load_usage_totals_is_none_without_a_file(tmp_path, monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", str(tmp_path / "absent"))
    assert load_usage_totals(7) is None


def test_days_outside_the_window_are_dropped(tmp_path, monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", str(tmp_path))
    write_usage(tmp_path, {"days": {
        day(1): {"alice": {"alloc": 10, "busy": 1}},
        day(90): {"alice": {"alloc": 999, "busy": 1}},
    }})
    assert load_usage_totals(7)[0][0][1] == 10


def test_load_usage_daily_is_oldest_first_with_coverage(tmp_path, monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", str(tmp_path))
    write_usage(tmp_path, {
        "days": {day(2): {"a": {"alloc": 1, "busy": 1}},
                 day(1): {"a": {"alloc": 2, "busy": 2}}},
        "meta": {day(1): 120},
    })
    rows = load_usage_daily(7)
    assert [r[0] for r in rows] == sorted(r[0] for r in rows)
    assert rows[-1][3] == 120  # covered seconds
    assert rows[0][3] == 0     # collector wasn't sampling: unknown, not zero


# ── discovery ─────────────────────────────────────────────────────────────

def test_freshest_state_file_wins_over_a_stale_leftover(tmp_path, monkeypatch):
    # A root collector publishes to /var/lib/sgpu, but an install that used to
    # run as a user leaves a copy in the data dir. Showing weeks-old GPU-hours
    # as if they were current is worse than showing none.
    published, leftover = tmp_path / "published", tmp_path / "leftover"
    write_usage(leftover, {"days": {day(): {"old": {"alloc": 1, "busy": 1}}}})
    write_usage(published, {"days": {day(): {"new": {"alloc": 2, "busy": 2}}}})
    import os
    os.utime(leftover / "usage.json", (1, 1))
    monkeypatch.delenv("SLURM_GPU_TUI_STATE_DIR", raising=False)
    monkeypatch.setattr(usage_mod, "state_dir_candidates",
                        lambda: [leftover, published])
    assert [t[0] for t in load_usage_totals(7)[0]] == ["new"]


def test_cache_is_keyed_on_path_as_well_as_mtime(tmp_path, monkeypatch):
    # Keying on mtime alone let two candidate paths return each other's cache.
    import os
    a, b = tmp_path / "a", tmp_path / "b"
    write_usage(a, {"days": {day(): {"alice": {"alloc": 1, "busy": 1}}}})
    write_usage(b, {"days": {day(): {"bob": {"alloc": 1, "busy": 1}}}})
    os.utime(a / "usage.json", (500, 500))
    os.utime(b / "usage.json", (500, 500))
    monkeypatch.delenv("SLURM_GPU_TUI_STATE_DIR", raising=False)
    monkeypatch.setattr(usage_mod, "state_dir_candidates", lambda: [a])
    assert [t[0] for t in load_usage_totals(7)[0]] == ["alice"]
    monkeypatch.setattr(usage_mod, "state_dir_candidates", lambda: [b])
    assert [t[0] for t in load_usage_totals(7)[0]] == ["bob"]
