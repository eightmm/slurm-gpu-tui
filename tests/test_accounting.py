"""Collector accounting (_track_waste, _accumulate_usage, sacct backfill)
and notify state-machine tests. All pure-logic with fake clocks/outputs."""
import json
import time
from datetime import datetime, timedelta

import pytest

from sgpu import collector


@pytest.fixture(autouse=True)
def _reset_collector_state():
    collector._idle_since.clear()
    collector._parked_since.clear()
    collector._usage.clear()
    collector._usage["days"] = {}
    collector._last_usage_ts = None
    yield


# ── _track_waste ──────────────────────────────────────────────────────────

def _gpu(jid="", users=(), util="0", mem_used="0", mem_total="81920"):
    return {"index": "0", "alloc_jobid": jid, "users": list(users),
            "util": util, "mem_used": mem_used, "mem_total": mem_total}


def test_track_waste_idle_accumulates():
    t0 = 1000.0
    g = _gpu(jid="42")
    collector._track_waste("n1", g, t0)
    assert g["idle_sec"] == 0
    g2 = _gpu(jid="42")
    collector._track_waste("n1", g2, t0 + 3600)
    assert g2["idle_sec"] == 3600


def test_track_waste_idle_resets_on_new_job():
    t0 = 1000.0
    collector._track_waste("n1", _gpu(jid="42"), t0)
    g = _gpu(jid="43")  # different job took over
    collector._track_waste("n1", g, t0 + 3600)
    assert g["idle_sec"] == 0


def test_track_waste_idle_clears_when_process_appears():
    t0 = 1000.0
    collector._track_waste("n1", _gpu(jid="42"), t0)
    g = _gpu(jid="42", users=["alice"], util="90")
    collector._track_waste("n1", g, t0 + 3600)
    assert g["idle_sec"] == 0
    assert "n1:0" not in collector._idle_since


def test_track_waste_parked_needs_vram_and_owner():
    t0 = 1000.0
    # VRAM held at 40%, util 0, allocated -> parked clock starts
    g = _gpu(jid="42", util="0", mem_used="32768")
    collector._track_waste("n1", g, t0)
    assert g["parked_sec"] == 0
    g2 = _gpu(jid="42", util="0", mem_used="32768")
    collector._track_waste("n1", g2, t0 + 1800)
    assert g2["parked_sec"] == 1800
    # compute resumes -> parked resets
    g3 = _gpu(jid="42", util="80", mem_used="32768")
    collector._track_waste("n1", g3, t0 + 3600)
    assert g3["parked_sec"] == 0


def test_track_waste_parked_ignores_low_vram():
    g = _gpu(jid="42", util="0", mem_used="100")  # ~0.1% VRAM
    collector._track_waste("n1", g, 1000.0)
    assert "n1:0" not in collector._parked_since


# ── _accumulate_usage ─────────────────────────────────────────────────────

def _node(gpus):
    return [{"gpus": gpus}]


def test_accumulate_usage_first_call_records_baseline_only():
    collector._accumulate_usage(_node([_gpu(jid="1")]), 1000.0)
    assert collector._usage["days"] == {}


def test_accumulate_usage_credits_alloc_and_busy():
    g = dict(_gpu(jid="1", util="90"), alloc_user="alice",
             idle_sec=0, parked_sec=0)
    collector._accumulate_usage(_node([g]), 1000.0)
    collector._accumulate_usage(_node([g]), 1003.0)
    day = datetime.now().strftime("%Y-%m-%d")
    u = collector._usage["days"][day]["alice"]
    assert u["alloc"] == 3.0
    assert u["busy"] == 3.0
    assert collector._usage["meta"][day] == 3.0


def test_accumulate_usage_skips_long_gap():
    g = dict(_gpu(jid="1"), alloc_user="alice")
    collector._accumulate_usage(_node([g]), 1000.0)
    collector._accumulate_usage(_node([g]), 1000.0 + 3600)  # collector was down
    assert collector._usage["days"] == {}


def test_accumulate_usage_credits_waste_over_threshold():
    g = dict(_gpu(jid="1", util="0"), alloc_user="alice",
             idle_sec=collector.WASTE_MIN_SEC + 1, parked_sec=0)
    collector._accumulate_usage(_node([g]), 1000.0)
    collector._accumulate_usage(_node([g]), 1003.0)
    day = datetime.now().strftime("%Y-%m-%d")
    assert collector._usage["days"][day]["alice"]["waste"] == 3.0


def test_accumulate_usage_prunes_old_days():
    old_day = (datetime.now() - timedelta(days=collector.USAGE_KEEP_DAYS + 5)).strftime("%Y-%m-%d")
    collector._usage["days"][old_day] = {"bob": {"alloc": 1, "busy": 0}}
    g = dict(_gpu(jid="1"), alloc_user="alice")
    collector._accumulate_usage(_node([g]), 1000.0)
    collector._accumulate_usage(_node([g]), 1003.0)
    assert old_day not in collector._usage["days"]


# ── sacct parsing / day-split ─────────────────────────────────────────────

def test_gpu_count_from_tres():
    f = collector._gpu_count_from_tres
    assert f("billing=8,cpu=8,gres/gpu=2,mem=32G,node=1") == 2
    assert f("cpu=8,gres/gpu:a6000=3,mem=32G") == 3
    assert f("gres/gpu:a100=1,gres/gpu:h100=2") == 3
    assert f("cpu=8,mem=32G") == 0


def test_parse_sacct_time():
    f = collector._parse_sacct_time
    assert f("Unknown") is None
    assert f("") is None
    ts = f("2026-07-10T12:00:00")
    assert ts == datetime(2026, 7, 10, 12, 0, 0).timestamp()


def test_sacct_backfill_splits_across_midnight(monkeypatch):
    now = time.time()
    yesterday = datetime.now() - timedelta(days=1)
    d0 = datetime(yesterday.year, yesterday.month, yesterday.day)
    # job runs 22:00 yesterday -> 02:00 today with 2 GPUs
    start = (d0 + timedelta(hours=22)).strftime("%Y-%m-%dT%H:%M:%S")
    end = (d0 + timedelta(hours=26)).strftime("%Y-%m-%dT%H:%M:%S")
    line = f"alice|billing=8,gres/gpu=2|{start}|{end}"
    monkeypatch.setattr(collector, "run_cmd", lambda *a, **k: (True, line))
    collector._sacct_backfill(now)
    day1 = d0.strftime("%Y-%m-%d")
    day2 = (d0 + timedelta(days=1)).strftime("%Y-%m-%d")
    days = collector._usage["sacct_days"]
    assert days[day1]["alice"] == pytest.approx(2 * 2 * 3600)  # 2h x 2 GPUs
    assert days[day2]["alice"] == pytest.approx(2 * 2 * 3600)


def test_sacct_backfill_running_job_clamped_to_now(monkeypatch):
    now = time.time()
    start_dt = datetime.now() - timedelta(hours=1)
    line = f"bob|gres/gpu=1|{start_dt.strftime('%Y-%m-%dT%H:%M:%S')}|Unknown"
    monkeypatch.setattr(collector, "run_cmd", lambda *a, **k: (True, line))
    collector._sacct_backfill(now)
    total = sum(sum(u.values()) for u in collector._usage["sacct_days"].values())
    assert total == pytest.approx(3600, abs=2)


def test_sacct_backfill_failure_counts(monkeypatch):
    monkeypatch.setattr(collector, "run_cmd", lambda *a, **k: (False, "sacct: error"))
    monkeypatch.setattr(collector, "_sacct_failures", 0)
    collector._sacct_backfill(time.time())
    assert collector._sacct_failures == 1
    assert "sacct_days" not in collector._usage
