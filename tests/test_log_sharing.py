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
    monkeypatch.setattr(collector, "_log_status", {})
    monkeypatch.setattr(collector, "_log_seed_tokens", {})
    monkeypatch.setattr(collector, "_log_next_check", {})
    monkeypatch.setattr(collector, "_log_live", {"42"})
    monkeypatch.setattr(collector, "_log_inflight", set())
    return d


def _job(jid="42", uid=None):
    from sgpu.common import JobInfo
    return JobInfo(
        jobid=jid, user="alice",
        uid=os.geteuid() if uid is None else uid,
    )


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


def test_disabled_sharing_cleans_previous_world_readable_tails(
        spool, monkeypatch):
    (spool / "42.out").write_text("old shared output\n")
    monkeypatch.setattr(collector, "SHARE_LOGS", False)

    assert collector._prepare_log_spool() is True
    assert not (spool / "42.out").exists()


def test_state_marker_only_claims_a_directory_created_by_sgpu(
        tmp_path, monkeypatch):
    existing = tmp_path / "existing-parent"
    existing.mkdir()
    monkeypatch.setattr(collector, "STATE_DIR", existing)
    monkeypatch.setattr(collector, "STATE_MARKER_FILE", existing / ".sgpu-state")

    collector._prepare_state_dir()

    assert not (existing / ".sgpu-state").exists()

    created = tmp_path / "created-by-sgpu"
    monkeypatch.setattr(collector, "STATE_DIR", created)
    monkeypatch.setattr(collector, "STATE_MARKER_FILE", created / ".sgpu-state")

    collector._prepare_state_dir()

    assert (created / ".sgpu-state").read_text() == "sgpu\n"


def test_share_logs_publishes_paths_for_live_jobs(spool, tmp_path, monkeypatch):
    src = tmp_path / "j.out"
    src.write_text("x\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    job = _job("42")
    job._log_paths = (str(src), "")
    published = collector._share_logs([job])
    assert published["42"]["out"] == str(spool / "42.out")


def test_disabled_sharing_publishes_nothing(spool, monkeypatch):
    monkeypatch.setattr(collector, "SHARE_LOGS", False)
    assert collector._share_logs([_job("42")]) == {}


def test_false_tokens_disable_sensitive_share_flags(monkeypatch):
    for value in ("0", "false", "no", "off", ""):
        monkeypatch.setenv("SGPU_FLAG_TEST", value)
        assert collector._env_enabled("SGPU_FLAG_TEST") is False
    monkeypatch.setenv("SGPU_FLAG_TEST", "1")
    assert collector._env_enabled("SGPU_FLAG_TEST") is True


def test_share_logs_seeds_private_metadata_without_scheduler_rpc(
        spool, monkeypatch):
    job = _job("42")
    job._log_paths = ("/private/alice/job.out", "")
    job.uid = 1001
    job._log_stderr_merged = True

    class _RecordingExecutor:
        def submit(self, _fn, _jid):
            return None

    monkeypatch.setattr(collector, "_log_executor", _RecordingExecutor())
    monkeypatch.setattr(
        collector, "run_cmd",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(
            AssertionError("unexpected scheduler RPC")
        ),
    )

    collector._share_logs([job])

    assert collector._log_paths["42"] == ("/private/alice/job.out", "")
    assert collector._log_owner_uids["42"] == 1001
    assert collector._log_status["42"]["err"] == "merged"


def test_share_logs_does_not_overwrite_status_for_configured_stderr(
        spool, monkeypatch):
    job = _job("42")
    job._log_paths = ("/logs/job.out", "/logs/job.err")

    class _NoopExecutor:
        def submit(self, _fn, _jid):
            return None

    monkeypatch.setattr(collector, "_log_executor", _NoopExecutor())
    monkeypatch.setattr(collector, "_log_status", {"42": {"err": "mirrored"}})

    collector._share_logs([job])

    assert collector._log_status["42"]["err"] == "mirrored"


def test_share_logs_invalidates_mirror_when_source_path_changes(
        spool, tmp_path, monkeypatch):
    old = tmp_path / "old.out"
    old.write_text("old output\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(old), "")})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").exists()

    job = _job("42")
    job._log_paths = (str(tmp_path / "new.out"), "")

    class _NoopExecutor:
        def submit(self, _fn, _jid):
            return None

    monkeypatch.setattr(collector, "_log_executor", _NoopExecutor())
    collector._share_logs([job])

    assert not (spool / "42.out").exists()
    assert collector._log_published == {}


def test_missing_fresh_metadata_removes_previous_cycle_mirror(
        spool, tmp_path, monkeypatch):
    src = tmp_path / "old.out"
    src.write_text("previously shared\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").exists()

    job = _job("42", uid=-1)
    job._log_paths = ("", "")

    class _NoopExecutor:
        def submit(self, _fn, _jid):
            raise AssertionError("untrusted metadata must not schedule a reader")

    monkeypatch.setattr(collector, "_log_executor", _NoopExecutor())

    published = collector._share_logs([job])

    assert not (spool / "42.out").exists()
    assert "42" not in collector._log_paths
    assert "42" not in collector._log_owner_uids
    assert published == {"42": {"status": {
        "out": "metadata-unavailable", "err": "metadata-unavailable",
    }}}


def test_stale_worker_cannot_republish_after_seed_changes(
        spool, tmp_path, monkeypatch):
    old = tmp_path / "old.out"
    old.write_text("old user's output\n")
    old_paths = (str(old), "")
    old_token = object()
    monkeypatch.setattr(collector, "_log_paths", {"42": old_paths})
    monkeypatch.setattr(collector, "_log_seed_tokens", {"42": old_token})

    real_read = collector._read_log_source

    def change_seed_during_read(src, owner_uid, known):
        result = real_read(src, owner_uid, known)
        with collector._log_lock:
            collector._log_paths["42"] = (str(tmp_path / "new.out"), "")
            collector._log_seed_tokens["42"] = object()
        return result

    monkeypatch.setattr(collector, "_read_log_source", change_seed_during_read)

    collector._mirror_one_job_log("42")

    assert not (spool / "42.out").exists()
    assert collector._log_published == {}


def test_public_job_records_gate_details_and_exclude_internal_metadata(monkeypatch):
    job = _job("42")
    job.detail = "JobId=42\nJobState=RUNNING"
    job._log_paths = ("/private/alice/job.out", "")
    job.uid = 1001
    job._log_stderr_merged = True

    monkeypatch.setattr(collector, "SHARE_JOB_DETAILS", False)
    hidden = collector._published_job_to_dict(job, "", {})
    assert hidden["detail"] == ""
    assert not any(key.startswith("_log_") for key in hidden)

    monkeypatch.setattr(collector, "SHARE_JOB_DETAILS", True)
    shared = collector._published_job_to_dict(job, "", {})
    assert shared["detail"] == "JobId=42\nJobState=RUNNING"
    assert not any(key.startswith("_log_") for key in shared)


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
    for job in jobs:
        job._log_paths = ("", "")
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


def test_safe_mirror_is_removed_if_source_becomes_unsafe(
        spool, tmp_path, monkeypatch):
    src = tmp_path / "job.out"
    src.write_text("previously public\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").exists()

    src.unlink()
    victim = tmp_path / "victim"
    victim.write_text("must remain private\n")
    src.symlink_to(victim)
    collector._mirror_one_job_log("42")

    assert not (spool / "42.out").exists()
    assert collector._log_published == {}
    assert collector._log_status["42"]["out"] == "unsafe"


def test_existing_mirror_is_removed_if_source_disappears(
        spool, tmp_path, monkeypatch):
    src = tmp_path / "job.out"
    src.write_text("old output\n")
    monkeypatch.setattr(collector, "_log_paths", {"42": (str(src), "")})
    collector._mirror_one_job_log("42")
    assert (spool / "42.out").exists()

    src.unlink()
    collector._mirror_one_job_log("42")

    assert not (spool / "42.out").exists()
    assert collector._log_published == {}
    assert collector._log_status["42"]["out"] == "waiting"


def test_root_squash_permission_error_uses_job_owner_reader(monkeypatch):
    monkeypatch.setattr(collector.os, "lstat", lambda _path: (_ for _ in ()).throw(
        PermissionError("root squashed")
    ))
    monkeypatch.setattr(
        collector, "_read_log_as_owner",
        lambda path, uid: (b"owner-readable tail\n", "mirrored"),
    )

    data, fingerprint, status = collector._read_log_source(
        "/home/alice/private/job.out", 1001, None,
    )

    assert data == b"owner-readable tail\n"
    assert fingerprint and fingerprint[0] == "owner-tail"
    assert status == "mirrored"


def test_owner_reader_child_drops_identity_and_environment(monkeypatch):
    from types import SimpleNamespace

    seen = {}
    monkeypatch.setattr(collector.os, "geteuid", lambda: 0)
    monkeypatch.setattr(collector, "_owner_identity", lambda uid: (2001, [3001]))
    monkeypatch.setenv("PYTHONPATH", "/untrusted")
    monkeypatch.setenv("SGPU_TEST_SECRET", "must-not-cross-uid-boundary")

    def fake_run(argv, **kwargs):
        seen.update(argv=argv, **kwargs)
        return SimpleNamespace(returncode=0, stdout=b"tail")

    monkeypatch.setattr(collector.subprocess, "run", fake_run)

    data, status = collector._read_log_as_owner("/logs/job.out", 1001)

    assert (data, status) == (b"tail", "mirrored")
    assert seen["argv"][1:3] == ["-I", "-m"]
    assert seen["user"] == 1001 and seen["group"] == 2001
    assert seen["extra_groups"] == [3001]
    assert seen["env"] == {"LANG": "C.UTF-8"}


def test_missing_structured_metadata_never_falls_back_to_text_scontrol(
        spool, monkeypatch):
    monkeypatch.setattr(collector, "_log_paths", {})
    monkeypatch.setattr(collector, "_log_owner_uids", {"42": os.geteuid()})
    monkeypatch.setattr(
        collector, "run_cmd",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(
            AssertionError("line-oriented metadata must not be queried")
        ),
    )

    collector._mirror_one_job_log("42")

    assert collector._log_status["42"] == {
        "out": "metadata-unavailable", "err": "metadata-unavailable",
    }


def test_invalid_jobid_cannot_unlink_outside_spool(tmp_path, monkeypatch):
    spool = tmp_path / "state" / "logs"
    spool.mkdir(parents=True)
    victim = tmp_path / "victim.out"
    victim.write_text("keep me\n")
    monkeypatch.setattr(collector, "LOG_SPOOL_DIR", spool)
    monkeypatch.setattr(collector, "_log_published", {
        "../../victim": {"out": str(victim)},
    })
    monkeypatch.setattr(collector, "_log_paths", {"../../victim": ("x", "")})
    monkeypatch.setattr(collector, "_log_owner_uids", {"../../victim": 0})

    collector._drop_log_spool("../../victim")

    assert victim.read_text() == "keep me\n"
    assert "../../victim" not in collector._log_published


def test_missing_numeric_queue_owner_fails_closed(spool, monkeypatch):
    monkeypatch.setattr(collector, "_log_paths", {})
    monkeypatch.setattr(collector, "_log_owner_uids", {"42": -1})
    monkeypatch.setattr(
        collector, "run_cmd",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(
            AssertionError("must not query paths without trusted owner")
        ),
    )

    collector._mirror_one_job_log("42")

    assert collector._log_status["42"] == {
        "out": "untrusted-owner", "err": "untrusted-owner",
    }


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
