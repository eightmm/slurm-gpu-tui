"""Job log helpers: path resolution, tail reading, error highlighting."""
import os

from sgpu.common import job_log_paths, tail_file
from sgpu.screens import _LOG_ERR_RE, _fmt_sacct_detail


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


def test_paths_relative_resolved_against_workdir():
    out = SCONTROL_OUT.replace("/home/alice/proj/out.log", "rel.out")
    so, _ = job_log_paths(out)
    assert so == "/home/alice/proj/rel.out"


def test_paths_null_and_missing():
    assert job_log_paths("JobId=1 StdOut=(null) StdErr=(null)") == ("", "")
    assert job_log_paths("JobId=1") == ("", "")


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
