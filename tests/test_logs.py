"""Job log helpers: path resolution, tail reading, error highlighting."""
import os

from sgpu.common import JobInfo, job_log_paths, job_log_spec, tail_file
from sgpu.screens import DetailScreen, _LOG_ERR_RE, _fmt_sacct_detail
from sgpu.tui import _collector_job_detail, _job_log_views, _slurm_control_jobid


# ── job_log_paths ─────────────────────────────────────────────────────────

SCONTROL_OUT = """JobId=123 JobName=train
   UserId=alice(1001) GroupId=alice(1001)
   Command=/home/alice/run.sh
   WorkDir=/home/alice/proj
   StdErr=/home/alice/proj/err.log
   StdIn=/dev/null
   StdOut=/home/alice/proj/out.log
"""


def test_paths_absolute():
    so, se = job_log_paths(SCONTROL_OUT)
    assert so == "/home/alice/proj/out.log"
    assert se == "/home/alice/proj/err.log"


def test_paths_merged_stderr_dropped():
    out = SCONTROL_OUT.replace("err.log", "out.log")
    so, se = job_log_paths(out)
    assert so == "/home/alice/proj/out.log"
    assert se == ""
    assert job_log_spec(out) == ("/home/alice/proj/out.log", "", True)


def test_paths_relative_resolved_against_workdir():
    out = SCONTROL_OUT.replace("/home/alice/proj/out.log", "rel.out")
    so, _ = job_log_paths(out)
    assert so == "/home/alice/proj/rel.out"


def test_paths_with_spaces_are_not_truncated():
    detail = (
        "JobId=1 WorkDir=/home/alice/my project "
        "StdErr=logs/error file.log StdIn=/dev/null "
        "StdOut=logs/output file.log Power= TresPerTask=cpu:1"
    )

    assert job_log_spec(detail) == (
        "/home/alice/my project/logs/output file.log",
        "/home/alice/my project/logs/error file.log",
        False,
    )


def test_paths_null_and_missing():
    assert job_log_paths("JobId=1 StdOut=(null) StdErr=(null)") == ("", "")
    assert job_log_paths("JobId=1") == ("", "")
    assert job_log_spec("JobId=1 StdOut=/tmp/a StdErr=(null)") == (
        "/tmp/a", "", False,
    )


# ── tail_file ─────────────────────────────────────────────────────────────

def test_tail_small_file(tmp_path):
    p = tmp_path / "a.log"
    p.write_text("hello\nworld\n")
    assert tail_file(str(p)) == "hello\nworld\n"


def test_tail_truncates_large_file(tmp_path):
    p = tmp_path / "big.log"
    p.write_text("x" * 200_000)
    text = tail_file(str(p))
    assert text.startswith("… showing last 64KB")
    assert len(text) < 70_000


def test_tail_missing_empty_unreadable(tmp_path):
    assert "no file yet" in tail_file(str(tmp_path / "nope.log"))
    empty = tmp_path / "empty.log"
    empty.touch()
    assert tail_file(str(empty)) == "(empty)"
    if os.getuid() != 0:
        secret = tmp_path / "secret.log"
        secret.write_text("hidden")
        secret.chmod(0)
        assert "not readable" in tail_file(str(secret))


def test_detail_log_poll_skips_unchanged_files(tmp_path, monkeypatch):
    import sgpu.screens as screens

    log = tmp_path / "job.log"
    log.write_text("step 1\n")
    reads = []
    real_tail = screens.tail_file

    def counted_tail(path):
        reads.append(path)
        return real_tail(path)

    monkeypatch.setattr(screens, "tail_file", counted_tail)
    detail = DetailScreen("job 1", "body", stdout_path=str(log))

    assert len(detail._collect_log_updates()) == 1
    assert detail._collect_log_updates() == []
    before = log.stat()
    log.write_text("step 2\n")  # same size
    os.utime(log, ns=(before.st_atime_ns, before.st_mtime_ns))
    assert len(detail._collect_log_updates()) == 1
    assert reads == [str(log), str(log)]


def test_detail_log_poll_retries_a_missing_file(tmp_path):
    log = tmp_path / "later.log"
    detail = DetailScreen("job 1", "body", stdout_path=str(log))

    assert len(detail._collect_log_updates()) == 1
    assert len(detail._collect_log_updates()) == 1
    log.write_text("started\n")
    updates = detail._collect_log_updates()
    assert len(updates) == 1 and "started" in updates[0][1].plain


def test_tui_prefers_collector_detail_for_another_user():
    from sgpu.common import PendingJob

    running = JobInfo(jobid="1", user="alice", detail="JobId=1 JobState=RUNNING")
    pending = PendingJob(jobid="2", user="bob", detail="JobId=2 JobState=PENDING")

    assert "JobState=RUNNING" in _collector_job_detail(running, None)
    assert "JobState=PENDING" in _collector_job_detail(None, pending)
    assert _collector_job_detail(None, None) == ""


def test_tui_detail_policy_keeps_owner_live_paths_and_cross_user_snapshot():
    running = JobInfo(jobid="1", user="alice", detail="JobId=1\nJobState=RUNNING")
    shared = _collector_job_detail(running, None)

    # Mirrors the branch in SlurmGpuTui._show_detail: the owner queries live
    # detail; another account uses the sanitized collector record.
    assert bool(shared and running.user != "alice") is False
    assert bool(shared and running.user != "bob") is True


def test_tui_normalizes_compressed_pending_array_for_scontrol():
    assert _slurm_control_jobid("51317_[0-159%16]") == "51317"
    assert _slurm_control_jobid("53552_43") == "53552_43"
    assert _slurm_control_jobid("123+1") == "123+1"


def test_tui_merged_stderr_uses_shared_stdout_without_private_paths(tmp_path):
    shared = tmp_path / "42.out"
    shared.write_text("combined output\n")
    job = JobInfo(
        jobid="42", log_out=str(shared),
        log_status={"out": "mirrored", "err": "merged"},
    )

    stdout_text, stdout_path, stderr_text, stderr_path = _job_log_views(
        "JobId=42\nJobState=RUNNING", job,
    )

    assert stdout_path == stderr_path == str(shared)
    assert stdout_text == "combined output\n"
    assert "stderr is merged" in stderr_text
    assert "combined output" in stderr_text


def test_cli_logs_uses_shared_mirror_when_scontrol_is_denied(
        tmp_path, monkeypatch, capsys):
    import sgpu.cli as cli

    shared = tmp_path / "42.out"
    shared.write_text("cross-user output\n")
    monkeypatch.setattr(cli, "_collector_snapshot", lambda: {"jobs": [{
        "jobid": "42", "detail": "JobId=42\nJobState=RUNNING",
        "log_out": str(shared), "log_err": "", "log_status": {"out": "mirrored"},
    }]})
    monkeypatch.setattr(cli, "run_cmd", lambda _cmd: (False, "Access denied"))

    assert cli._cli_logs("42") == 0
    assert "cross-user output" in capsys.readouterr().out


def test_cli_merged_stderr_uses_shared_stdout(tmp_path, monkeypatch, capsys):
    import sgpu.cli as cli

    shared = tmp_path / "43.out"
    shared.write_text("merged stream\n")
    detail = "JobId=43 WorkDir=/private StdOut=job.log StdErr=job.log"
    monkeypatch.setattr(cli, "_collector_snapshot", lambda: {"jobs": [{
        "jobid": "43", "detail": detail, "log_out": str(shared), "log_err": "",
        "log_status": {"out": "mirrored", "err": "merged"},
    }]})
    monkeypatch.setattr(cli, "run_cmd", lambda _cmd: (True, detail))

    assert cli._cli_logs("43", want_err=True) == 0
    captured = capsys.readouterr()
    assert "merged stream" in captured.out
    assert "stderr is merged" in captured.err


def test_cli_merged_stderr_uses_status_when_private_detail_is_denied(
        tmp_path, monkeypatch, capsys):
    import sgpu.cli as cli

    shared = tmp_path / "44.out"
    shared.write_text("merged from collector\n")
    monkeypatch.setattr(cli, "_collector_snapshot", lambda: {"jobs": [{
        "jobid": "44", "detail": "JobId=44\nJobState=RUNNING",
        "log_out": str(shared), "log_err": "",
        "log_status": {"out": "mirrored", "err": "merged"},
    }]})
    monkeypatch.setattr(cli, "run_cmd", lambda _cmd: (False, "Access denied"))

    assert cli._cli_logs("44", want_err=True) == 0
    captured = capsys.readouterr()
    assert "merged from collector" in captured.out
    assert "stderr is merged" in captured.err


def test_cli_readable_own_log_does_not_load_collector_snapshot(
        tmp_path, monkeypatch, capsys):
    import sgpu.cli as cli

    real = tmp_path / "45.out"
    real.write_text("owner output\n")
    detail = f"JobId=45 StdOut={real} StdErr=(null)"
    monkeypatch.setattr(cli, "run_cmd", lambda _cmd: (True, detail))
    monkeypatch.setattr(
        cli, "_collector_snapshot",
        lambda: (_ for _ in ()).throw(AssertionError("snapshot loaded")),
    )

    assert cli._cli_logs("45") == 0
    assert "owner output" in capsys.readouterr().out


def test_shared_follow_overlap_emits_only_new_suffix():
    from sgpu.cli import _suffix_prefix_overlap

    previous = b"old line\nkept line\n"
    current = b"kept line\nnew line\n"

    overlap = _suffix_prefix_overlap(previous, current)

    assert current[overlap:] == b"new line\n"
    assert _suffix_prefix_overlap(b"abc", b"xyz") == 0
    assert _suffix_prefix_overlap(b"same", b"same") == 4


# ── error highlighting ────────────────────────────────────────────────────

def test_err_patterns_match():
    for line in [
        "Traceback (most recent call last):",
        "RuntimeError: CUDA out of memory. Tried to allocate 2GiB",
        "srun: error: node1: task 0: Exited with exit code 1",
        "slurmstepd: error: Detected 1 oom-kill event",
        "Segmentation fault (core dumped)",
        "ValueError: bad shape",
        "FAILED",
    ]:
        assert _LOG_ERR_RE.search(line), line


def test_err_patterns_skip_benign():
    for line in [
        "epoch 3 val_error 0.123",   # lowercase metric name
        "loading weights",
        "step 100/1000 loss=0.5",
    ]:
        assert not _LOG_ERR_RE.search(line), line


# ── sacct detail formatting ───────────────────────────────────────────────

def test_fmt_sacct_detail_blocks():
    raw = ("JobID|State|ExitCode|MaxRSS\n"
           "123|FAILED|1:0|\n"
           "123.batch|FAILED|1:0|12345K\n")
    out = _fmt_sacct_detail(raw)
    blocks = out.split("\n\n")
    assert len(blocks) == 2
    assert "JobID     123" in blocks[0]
    assert "MaxRSS" not in blocks[0]      # empty values dropped
    assert "MaxRSS    12345K" in blocks[1]


def test_fmt_sacct_detail_passthrough_on_junk():
    assert _fmt_sacct_detail("sacct: error") == "sacct: error"


# ── notify: failed-job stderr tail ────────────────────────────────────────

def test_fail_log_tail_from_scontrol(monkeypatch, tmp_path):
    import sgpu.notify as notify
    err = tmp_path / "e.log"
    err.write_text("line\n" * 30 + "RuntimeError: boom\n")

    def fake_run_cmd(cmd, timeout=10):
        if cmd.startswith("scontrol"):
            return True, f"JobId=5 WorkDir={tmp_path} StdOut={tmp_path}/o.log StdErr={err}"
        raise AssertionError(f"unexpected cmd {cmd}")

    monkeypatch.setattr(notify, "run_cmd", fake_run_cmd)
    tail = notify.Notifier._fail_log_tail(None, "5")
    assert tail.endswith("RuntimeError: boom")
    assert len(tail.splitlines()) == 15  # capped


def test_fail_log_tail_workdir_fallback(monkeypatch, tmp_path):
    import sgpu.notify as notify
    (tmp_path / "slurm-7.out").write_text("srun: error: died\n")

    def fake_run_cmd(cmd, timeout=10):
        if cmd.startswith("scontrol"):
            return False, "Invalid job id"
        if cmd.startswith("sacct"):
            return True, str(tmp_path)
        raise AssertionError(cmd)

    monkeypatch.setattr(notify, "run_cmd", fake_run_cmd)
    assert notify.Notifier._fail_log_tail(None, "7") == "srun: error: died"


def test_fail_log_tail_nothing_readable(monkeypatch, tmp_path):
    import sgpu.notify as notify
    monkeypatch.setattr(notify, "run_cmd", lambda cmd, timeout=10: (False, "nope"))
    assert notify.Notifier._fail_log_tail(None, "8") == ""


def test_fail_tail_dm_only(monkeypatch, tmp_path):
    """stderr tail must reach the owner's DM, never the shared channel."""
    import json as _json
    import sgpu.notify as notify_mod

    cfg = {"bot_token": "xoxb-test", "channel": "#gpu", "node_health": False,
           "collect_alert": False, "rogue_alert": False, "ecc_alert": False,
           "job_fail_users": ["*"], "dm_users": {"bob": "U01"}}
    p = tmp_path / "webhook.json"
    p.write_text(_json.dumps(cfg))
    n = notify_mod.Notifier(tmp_path, cfg_path=p)
    posts = []
    n._post = lambda text, key="", channel="": posts.append((channel, text))
    monkeypatch.setattr(notify_mod.Notifier, "_fail_log_tail",
                        lambda self, jid: "RuntimeError: boom")
    monkeypatch.setattr(notify_mod.Notifier, "_job_final_state",
                        lambda self, jid: "FAILED")

    base = {"nodes": [], "pending": [], "errors": ""}
    n.process(dict(base, jobs=[{"jobid": "9", "user": "bob",
                                "jobname": "t", "elapsed": "1:00"}]))
    n.process(dict(base, jobs=[]))

    channel_msgs = [t for c, t in posts if not c]
    dm_msgs = [t for c, t in posts if c == "U01"]
    assert channel_msgs and all("boom" not in t for t in channel_msgs)
    assert dm_msgs and "```RuntimeError: boom```" in dm_msgs[0]


def test_fail_tail_skipped_without_dm_target(monkeypatch, tmp_path):
    """No DM mapping -> log file is never even read."""
    import json as _json
    import sgpu.notify as notify_mod

    cfg = {"bot_token": "xoxb-test", "channel": "#gpu", "node_health": False,
           "collect_alert": False, "rogue_alert": False, "ecc_alert": False,
           "job_fail_users": ["*"]}
    p = tmp_path / "webhook.json"
    p.write_text(_json.dumps(cfg))
    n = notify_mod.Notifier(tmp_path, cfg_path=p)
    n._post = lambda text, key="", channel="": None

    def boom(self, jid):
        raise AssertionError("_fail_log_tail called without DM target")
    monkeypatch.setattr(notify_mod.Notifier, "_fail_log_tail", boom)
    monkeypatch.setattr(notify_mod.Notifier, "_job_final_state",
                        lambda self, jid: "FAILED")

    base = {"nodes": [], "pending": [], "errors": ""}
    n.process(dict(base, jobs=[{"jobid": "9", "user": "bob",
                                "jobname": "t", "elapsed": "1:00"}]))
    n.process(dict(base, jobs=[]))
