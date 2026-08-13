"""Job stdout/stderr sharing.

A job's log lives under the submitter's home, so only the owner can read it
and everyone else's log tabs are empty. A root collector mirrors a bounded
tail somewhere world-readable; readers prefer the real file and fall back.
"""
import os

import pytest

from sgpu import collector
from sgpu.common import read_job_log


# ── reader fallback ───────────────────────────────────────────────────────

def test_real_file_wins_when_readable(tmp_path):
    real, shared = tmp_path / "job.out", tmp_path / "job.out.shared"
    real.write_text("live and complete\n")
    shared.write_text("stale mirror\n")
    text, used = read_job_log(str(real), str(shared))
    assert "live and complete" in text and used == str(real)


def test_falls_back_to_the_mirror_when_unreadable(tmp_path):
    real, shared = tmp_path / "job.out", tmp_path / "job.out.shared"
    real.write_text("secret\n")
    real.chmod(0o000)
    shared.write_text("mirrored tail\n")
    try:
        text, used = read_job_log(str(real), str(shared))
        assert "mirrored tail" in text and used == str(shared)
    finally:
        real.chmod(0o600)


def test_reports_the_real_error_when_neither_is_readable(tmp_path):
    real = tmp_path / "job.out"
    text, used = read_job_log(str(real), "")
    assert used == str(real) and "no file yet" in text


def test_no_paths_at_all(tmp_path):
    assert read_job_log("", "") == ("", "")


def test_owner_is_unaffected_when_sharing_is_off(tmp_path):
    real = tmp_path / "job.out"
    real.write_text("mine\n")
    text, used = read_job_log(str(real), "")
    assert "mine" in text and used == str(real)


# ── collector mirroring ───────────────────────────────────────────────────

@pytest.fixture
def spool(tmp_path, monkeypatch):
    d = tmp_path / "spool"
    d.mkdir()
    monkeypatch.setattr(collector, "SHARE_LOGS", True)
    monkeypatch.setattr(collector, "LOG_SPOOL_DIR", d)
    monkeypatch.setattr(collector, "_log_paths", {})
    monkeypatch.setattr(collector, "_log_owner_uids", {"42": os.geteuid()})
    monkeypatch.setattr(collector, "_log_fingerprint", {})
    monkeypatch.setattr(collector, "_log_published", {})
    monkeypatch.setattr(collector, "_log_next_check", {})
    monkeypatch.setattr(collector, "_log_live", {"42"})
    monkeypatch.setattr(collector, "_log_inflight", set())
    return d


def _job(jid="42"):
    from sgpu.common import JobInfo
    return JobInfo(jobid=jid, user="alice")


def test_mirror_copies_the_tail_world_readable(spool, tmp_path, monkeypatch):
    src = tmp_path / "slurm-42.out"
    src.write_text("hello from the job\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    dst = spool / "42.out"
    assert dst.read_text() == "hello from the job\n"
    assert oct(dst.stat().st_mode)[-3:] == "644"


def test_mirror_is_capped_to_the_tail(spool, tmp_path, monkeypatch):
    src = tmp_path / "slurm-42.out"
    src.write_text("A" * 5000 + "TAIL")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    monkeypatch.setattr(collector, "LOG_TAIL_BYTES", 100)
    collector._mirror_one_job_log("42")
    data = (spool / "42.out").read_bytes()
    assert len(data) == 100 and data.endswith(b"TAIL")


def test_unchanged_source_is_not_recopied(spool, tmp_path, monkeypatch):
    src = tmp_path / "slurm-42.out"
    src.write_text("one\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    first = (spool / "42.out").stat().st_mtime_ns
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").stat().st_mtime_ns == first


def test_growing_source_is_recopied(spool, tmp_path, monkeypatch):
    src = tmp_path / "slurm-42.out"
    src.write_text("one\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    src.write_text("one\ntwo\n")
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").read_text() == "one\ntwo\n"


def test_stderr_is_mirrored_separately(spool, tmp_path, monkeypatch):
    out, err = tmp_path / "j.out", tmp_path / "j.err"
    out.write_text("stdout\n")
    err.write_text("stderr\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(out), str(err))})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").read_text() == "stdout\n"
    assert (spool / "42.err").read_text() == "stderr\n"


def test_missing_source_produces_no_spool_file(spool, tmp_path, monkeypatch):
    monkeypatch.setattr(collector, "_log_paths",
                        {"42": (str(tmp_path / "absent.out"), "")})
    collector._mirror_one_job_log("42")
    assert list(spool.iterdir()) == []


def test_finished_jobs_are_swept_from_the_spool(spool, tmp_path, monkeypatch):
    src = tmp_path / "j.out"
    src.write_text("x\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").exists()
    collector._share_logs([])  # job left the queue
    assert not (spool / "42.out").exists()
    assert "42" not in collector._log_paths


def test_stale_spool_files_are_cleared_on_startup(spool):
    (spool / "old.out").write_text("stale\n")
    (spool / "old.err").write_text("stale\n")
    keep = spool / "README"
    keep.write_text("not a mirror\n")

    collector._clear_stale_log_spool()

    assert not (spool / "old.out").exists()
    assert not (spool / "old.err").exists()
    assert keep.read_text() == "not a mirror\n"


def test_share_logs_publishes_paths_for_live_jobs(spool, tmp_path, monkeypatch):
    src = tmp_path / "j.out"
    src.write_text("x\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    published = collector._share_logs([_job("42")])
    assert published["42"]["out"] == str(spool / "42.out")


def test_disabled_sharing_publishes_nothing(spool, monkeypatch):
    monkeypatch.setattr(collector, "SHARE_LOGS", False)
    assert collector._share_logs([_job("42")]) == {}


def test_share_logs_rate_limits_worker_submissions(spool, monkeypatch):
    submitted = []
    now = [100.0]

    class _ImmediateExecutor:
        def submit(self, fn, jid):
            submitted.append(jid)
            collector._log_inflight.discard(jid)

    monkeypatch.setattr(collector, "_log_executor", _ImmediateExecutor())
    monkeypatch.setattr(collector.time, "monotonic", lambda: now[0])
    monkeypatch.setattr(collector, "LOG_MIRROR_SEC", 10.0)

    jobs = [_job(str(i)) for i in range(100)]
    for _ in range(10):
        collector._share_logs(jobs)
        now[0] += 1
    assert len(submitted) == 100

    collector._share_logs(jobs)
    assert len(submitted) == 200


def test_missing_published_spool_is_repaired_on_next_scan(
        spool, tmp_path, monkeypatch):
    src = tmp_path / "j.out"
    src.write_text("x\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    dst = spool / "42.out"
    dst.unlink()

    collector._mirror_one_job_log("42")

    assert dst.read_text() == "x\n"
    assert collector._log_published["42"]["out"] == str(dst)


def test_mirror_rejects_symlink_and_wrong_owner(spool, tmp_path, monkeypatch):
    victim = tmp_path / "victim"
    victim.write_text("must not publish\n")
    link = tmp_path / "job.out"
    link.symlink_to(victim)
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(link), "")})

    collector._mirror_one_job_log("42")
    assert list(spool.iterdir()) == []

    link.unlink()
    link.write_text("also private\n")
    monkeypatch.setattr(collector, "_log_owner_uids", {"42": os.geteuid() + 1})
    collector._mirror_one_job_log("42")
    assert list(spool.iterdir()) == []


def test_scontrol_lookup_failure_is_retried(spool, tmp_path, monkeypatch):
    src = tmp_path / "job.out"
    src.write_text("eventually mirrored\n")
    monkeypatch.setattr(collector, "_log_paths", {})
    monkeypatch.setattr(collector, "_log_owner_uids", {})
    calls = []

    def fake_run(*args, **kwargs):
        calls.append(args[0])
        if len(calls) == 1:
            return False, "controller busy"
        return True, (
            f"JobId=42 UserId=alice({os.geteuid()}) "
            f"WorkDir={tmp_path} StdOut={src} StdErr=(null)"
        )

    monkeypatch.setattr(collector, "run_cmd", fake_run)
    collector._mirror_one_job_log("42")
    assert "42" not in collector._log_paths

    collector._mirror_one_job_log("42")
    assert len(calls) == 2
    assert (spool / "42.out").read_text() == "eventually mirrored\n"


def test_finished_job_worker_cannot_republish_spool(spool, tmp_path, monkeypatch):
    src = tmp_path / "job.out"
    src.write_text("late worker\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    monkeypatch.setattr(collector, "_log_live", set())

    collector._mirror_one_job_log("42")

    assert list(spool.iterdir()) == []
    assert collector._log_published == {}


def test_unreadable_source_does_not_crash_the_worker(spool, tmp_path, monkeypatch):
    if os.geteuid() == 0:
        pytest.skip("root reads everything")
    src = tmp_path / "j.out"
    src.write_text("secret\n")
    src.chmod(0o000)
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    try:
        collector._mirror_one_job_log("42")
        assert not (spool / "42.out").exists()
    finally:
        src.chmod(0o600)
