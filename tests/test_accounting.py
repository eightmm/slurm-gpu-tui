"""Collector accounting (_track_waste, _accumulate_usage, sacct backfill)
and notify state-machine tests. All pure-logic with fake clocks/outputs."""
import os
import stat
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
    collector._state_write_cache.clear()
    collector._state_path_locks.clear()
    collector._node_absent_cycles.clear()
    collector._last_usage_ts = None
    yield


def test_state_write_skips_identical_payload_but_repairs_replacement(
        tmp_path, monkeypatch):
    path = tmp_path / "idle_state.json"
    writes = []
    real_atomic_write = collector.atomic_write_with_signature

    def counted_write(*args, **kwargs):
        writes.append(args[1])
        return real_atomic_write(*args, **kwargs)

    monkeypatch.setattr(collector, "atomic_write_with_signature", counted_write)

    payload = "expected"
    assert collector._write_state_json(path, payload) is True
    first_inode = path.stat().st_ino
    assert collector._write_state_json(path, payload) is False
    assert path.stat().st_ino == first_inode
    assert len(writes) == 1

    # Same-inode/same-size edits with a restored mtime are still detected from
    # the on-disk digest; external changes cannot hide behind metadata.
    before = path.stat()
    time.sleep(0.01)  # ensure ctime advances on coarse timestamp filesystems
    path.write_text("tampered")
    os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
    assert collector._write_state_json(path, payload) is True
    assert path.read_text() == payload
    assert len(writes) == 2


def test_state_write_repairs_a_replacement_racing_after_rename(
        tmp_path, monkeypatch):
    path = tmp_path / "idle_state.json"
    real_atomic_write = collector.atomic_write_with_signature
    calls = 0

    def replaced_after_write(*args, **kwargs):
        nonlocal calls
        calls += 1
        signature = real_atomic_write(*args, **kwargs)
        if calls == 1:
            path.write_text("external race")
        return signature

    monkeypatch.setattr(
        collector, "atomic_write_with_signature", replaced_after_write,
    )

    assert collector._write_state_json(path, "expected") is True
    assert path.read_text() == "external race"
    assert collector._write_state_json(path, "expected") is True
    assert path.read_text() == "expected"


def test_state_write_retries_after_failure(tmp_path, monkeypatch):
    path = tmp_path / "usage.json"
    real_atomic_write = collector.atomic_write_with_signature
    calls = 0

    def fail_once(*args, **kwargs):
        nonlocal calls
        calls += 1
        if calls == 1:
            raise OSError("disk full")
        return real_atomic_write(*args, **kwargs)

    monkeypatch.setattr(collector, "atomic_write_with_signature", fail_once)

    assert collector._write_state_json(path, "expected") is False
    assert collector._write_state_json(path, "expected") is True
    assert path.read_text() == "expected"


def test_state_write_skips_when_live_file_identity_is_unstable(
        tmp_path, monkeypatch):
    path = tmp_path / "usage.json"
    real_signature = collector._state_file_signature
    sequence = iter(range(1, 10))

    def unstable_signature(target):
        mode, dev, ino, size, mtime_ns, ctime_ns = real_signature(target)
        return mode, dev, ino + next(sequence), size, mtime_ns, ctime_ns

    monkeypatch.setattr(collector, "_state_file_signature", unstable_signature)

    results = [collector._write_state_json(path, "same") for _ in range(3)]
    assert results == [True, False, False]


def test_state_write_repairs_mode_and_symlink(tmp_path):
    path = tmp_path / "usage.json"
    victim = tmp_path / "victim"
    victim.write_text("same")

    assert collector._write_state_json(path, "same") is True
    path.chmod(0o600)
    assert collector._write_state_json(path, "same") is True
    assert stat.S_IMODE(path.stat().st_mode) == 0o644

    path.unlink()
    path.symlink_to(victim)
    assert collector._write_state_json(path, "same") is True
    assert not path.is_symlink()
    assert path.read_text() == "same"
    assert victim.read_text() == "same"


def test_prune_node_caches_uses_grace_and_keeps_repair_rate_limit(monkeypatch):
    monkeypatch.setattr(collector, "_node_results", {"live": {}, "old": {}})
    monkeypatch.setattr(collector, "_node_poll_state", {"live": {}, "old": {}})
    monkeypatch.setattr(collector, "_agent_payload_cache", {"live": (), "old": ()})
    monkeypatch.setattr(
        collector, "_agent_repair_ts",
        {"live": 1.0, "old": 2.0},
    )
    monkeypatch.setattr(collector, "_untrusted_payloads", {"live": 1, "old": 2})
    monkeypatch.setattr(collector, "_untrusted_warned", {"live", "old"})
    monkeypatch.setattr(collector, "_node_absent_cycles", {})

    collector._prune_node_caches({"live"})
    assert "old" in collector._node_results  # one partial roster is tolerated
    collector._prune_node_caches({"live"})

    for cache in (
        collector._node_results, collector._node_poll_state,
        collector._agent_payload_cache,
        collector._untrusted_payloads, collector._untrusted_warned,
    ):
        assert set(cache) == {"live"}
    # Deleting this while a repair worker is still active can schedule a
    # second repair that kills the first one's freshly launched agent.
    assert collector._agent_repair_ts == {"live": 1.0, "old": 2.0}


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
