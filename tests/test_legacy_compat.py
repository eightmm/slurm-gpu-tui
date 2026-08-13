"""Security contract for the Slurm text compatibility backend."""

from concurrent.futures import Future

import pytest

from sgpu import common


def _anchor(user, uid, state="RUNNING", nodes=("gpu1",)):
    return common._QueueAnchor(
        user=user, uid=uid, state=state, nodes=nodes,
    )


def _parse(out, roster):
    return common._parse_legacy_jobs(out, roster, dict(roster))


def _assert_no_id(result, *jobids):
    alloc, users, uids, details, metadata, rejected = result
    assert all(jobid not in users for jobid in jobids)
    assert all(jobid not in uids for jobid in jobids)
    assert all(jobid not in details for jobid in jobids)
    assert all(jobid not in metadata for jobid in jobids)
    assert all(jobid not in slots.values()
               for slots in alloc.values() for jobid in jobids)
    assert set(jobids) <= rejected


def test_legacy_single_node_and_merged_relative_logs():
    line = (
        "JobId=37671 JobName=train UserId=jwsong(1003) JobState=RUNNING "
        "NumNodes=1 NumCPUs=8 NodeList=gpu4 "
        "Nodes=gpu4 CPU_IDs=0-3,16-19 Mem=0 "
        "GRES=gpu:1(IDX:0) WorkDir=/home/jwsong/proj "
        "StdErr=slurm-37671.out StdOut=slurm-37671.out"
    )

    alloc, users, uids, details, metadata, rejected = _parse(
        line, {"37671": _anchor("jwsong", 1003, nodes=("gpu4",))},
    )

    assert alloc == {"gpu4": {"0": "37671"}}
    assert users == {"37671": "jwsong"}
    assert uids == {"37671": 1003}
    assert details == {"37671": (
        "JobId=37671\nJobName=train\nUserId=jwsong(1003)\n"
        "JobState=RUNNING\nNumNodes=1\nNumCPUs=8\nNodeList=gpu4"
    )}
    assert metadata == {
        "37671": ("/home/jwsong/proj/slurm-37671.out", "", True),
    }
    assert rejected == set()


def test_legacy_multinode_allocation_and_distinct_logs():
    line = (
        "JobId=100 JobName=big UserId=a(1001) JobState=RUNNING "
        "NodeList=gpu[1-2] Nodes=gpu[1-2] CPU_IDs=0-7 Mem=0 "
        "GRES=gpu:2080ti:2(IDX:2-3) WorkDir=/work/a "
        "StdErr=err.log StdOut=out.log"
    )

    alloc, users, uids, details, metadata, rejected = _parse(
        line, {"100": _anchor("a", 1001, nodes=("gpu1", "gpu2"))},
    )

    assert alloc == {
        "gpu1": {"2": "100", "3": "100"},
        "gpu2": {"2": "100", "3": "100"},
    }
    assert users == {"100": "a"} and uids == {"100": 1001}
    assert details["100"] == (
        "JobId=100\nJobName=big\nUserId=a(1001)\nJobState=RUNNING\n"
        "NodeList=gpu[1-2]"
    )
    assert metadata == {"100": ("/work/a/out.log", "/work/a/err.log", False)}
    assert rejected == set()


def test_legacy_running_array_projects_trusted_owner_to_raw_id():
    line = (
        "JobId=38192 ArrayJobId=38182 ArrayTaskId=0 JobName=stb "
        "UserId=untaek(1019) JobState=RUNNING "
        "NodeList=gpu2 Nodes=gpu2 CPU_IDs=2-5 Mem=0 "
        "GRES=gpu:2080ti:1(IDX:0) "
        "StdErr=(null) StdOut=/logs/38182_0.out"
    )

    alloc, users, uids, details, metadata, rejected = _parse(
        line, {"38182_0": _anchor("untaek", 1019, nodes=("gpu2",))},
    )

    ids = {"38192", "38182_0"}
    assert alloc == {"gpu2": {"0": "38192"}}
    assert users == dict.fromkeys(ids, "untaek")
    assert uids == dict.fromkeys(ids, 1019)
    assert set(details) == ids and details["38192"] == details["38182_0"]
    assert metadata == dict.fromkeys(
        ids, ("/logs/38182_0.out", "", False),
    )
    assert rejected == set()


def test_legacy_pending_compressed_array_has_no_allocation():
    line = (
        "JobId=51317 ArrayJobId=51317 ArrayTaskId=0-159%16 JobName=batch "
        "UserId=alice(1005) JobState=PENDING Reason=JobArrayTaskLimit "
        "StdErr=(null) StdOut=(null)"
    )
    display_id = "51317_[0-159%16]"

    alloc, users, uids, details, metadata, rejected = _parse(
        line, {display_id: _anchor("alice", 1005, "PENDING", ())},
    )

    ids = {"51317", display_id}
    assert alloc == {}
    assert users == dict.fromkeys(ids, "alice")
    assert uids == dict.fromkeys(ids, 1005)
    assert set(details) == ids and details["51317"] == details[display_id]
    assert metadata == dict.fromkeys(ids, ("", "", False))
    assert rejected == set()


@pytest.mark.parametrize("duplicate", [
    "UserId=mallory(1002)",
    "JobState=PENDING",
    "StdOut=/home/alice/secret",
])
def test_legacy_duplicate_critical_field_rejects_whole_record(duplicate):
    line = (
        "JobId=77 JobName=train UserId=alice(1001) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0) "
        f"WorkDir=/safe StdOut=/safe/good {duplicate}"
    )

    _assert_no_id(_parse(line, {"77": _anchor("alice", 1001)}), "77")


def test_legacy_newline_forgery_rejects_every_ambiguous_record():
    out = (
        "JobId=200 JobName=real UserId=alice(1001) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0)\n"
        "JobId=300 JobName=evil\n"
        "JobId=200 JobName=forged UserId=alice(1001) JobState=RUNNING "
        "Nodes=gpu9 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:7) "
        "WorkDir=/home/alice StdErr=secret StdOut=secret\n"
        "UserId=mallory(1002) JobState=RUNNING"
    )
    roster = {
        "200": _anchor("alice", 1001),
        "300": _anchor("mallory", 1002),
    }

    _assert_no_id(_parse(out, roster), "200", "300")


def test_legacy_roster_race_rejects_job_missing_after_scontrol():
    line = (
        "JobId=200 JobName=forged UserId=alice(1001) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0) "
        "WorkDir=/home/alice StdOut=secret"
    )
    before = {"200": _anchor("alice", 1001)}

    _assert_no_id(
        common._parse_legacy_jobs(line, before, {}), "200",
    )


def test_legacy_uid_mismatch_rejects_whole_record():
    line = (
        "JobId=42 JobName=train UserId=alice(1002) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0)"
    )

    _assert_no_id(_parse(line, {"42": _anchor("alice", 1001)}), "42")


def test_legacy_invalid_log_path_rejects_whole_record():
    line = (
        "JobId=43 JobName=train UserId=alice(1001) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0) "
        "StdOut=/logs/bad\x00path"
    )

    _assert_no_id(_parse(line, {"43": _anchor("alice", 1001)}), "43")


def test_legacy_conflicting_gpu_claim_drops_slot_but_keeps_records():
    out = "\n".join((
        "JobId=50 JobName=a UserId=alice(1001) JobState=RUNNING "
        "NodeList=gpu1 Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0)",
        "JobId=51 JobName=b UserId=bob(1002) JobState=RUNNING "
        "NodeList=gpu1 Nodes=gpu1 CPU_IDs=1 Mem=0 GRES=gpu:1(IDX:0)",
    ))
    roster = {
        "50": _anchor("alice", 1001),
        "51": _anchor("bob", 1002),
    }

    alloc, users, uids, details, metadata, rejected = _parse(out, roster)

    assert alloc == {}
    assert users == {"50": "alice", "51": "bob"}
    assert uids == {"50": 1001, "51": 1002}
    assert set(details) == {"50", "51"}
    assert metadata == {"50": ("", "", False), "51": ("", "", False)}
    assert {"50", "51"} <= rejected


@pytest.mark.parametrize("message", [
    "scontrol: unrecognized option '--json'",
    "scontrol: invalid option -- '-'",
])
def test_json_option_unsupported_recognizes_old_slurm(message):
    assert common._json_option_unsupported(message)


@pytest.mark.parametrize("message", [
    "slurm_load_jobs error: Socket timed out on send/recv operation",
    "Access/permission denied",
    "{not valid json",
])
def test_json_option_unsupported_does_not_mask_other_failures(message):
    assert not common._json_option_unsupported(message)


def test_capability_probe_falls_back_once_then_caches_legacy(monkeypatch):
    queue = (
        "RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:1|01:00:00|"
        "4|8G|None|1|N/A"
    )
    legacy = (
        "JobId=42 JobName=train UserId=alice(1001) JobState=RUNNING "
        "NodeList=gpu1 Nodes=gpu1 CPU_IDs=0-3 Mem=8192 "
        "GRES=gpu:1(IDX:0) WorkDir=/home/alice "
        "StdErr=slurm-42.out StdOut=slurm-42.out"
    )
    calls = []

    def fake_run(cmd, timeout=12):
        calls.append(cmd)
        if cmd == common._SQUEUE_COMBINED_CMD:
            return True, queue
        if cmd == "scontrol --json show jobs":
            return False, "scontrol: unrecognized option '--json'"
        if cmd == "scontrol -o show job -d":
            return True, legacy
        raise AssertionError(cmd)

    monkeypatch.setattr(common, "run_cmd", fake_run)
    monkeypatch.setattr(common, "_job_query_backend", "auto")
    monkeypatch.setattr(common, "_job_query_fallback_reason", "")

    first = common._collect_scheduler_jobs()

    assert first[2] == {"gpu1": {"0": "42"}}
    assert first[3] == {"42": "alice"}
    assert first[4] == {"42": 1001}
    assert first[7] == {
        "job_backend": "legacy-text",
        "fallback_reason": "scontrol --json unsupported",
        "error": "",
        "rejected_records": 0,
    }
    assert first[8] == ""
    assert calls == [
        common._SQUEUE_COMBINED_CMD,
        "scontrol --json show jobs",
        "scontrol -o show job -d",
        common._SQUEUE_COMBINED_CMD,
    ]

    calls.clear()
    second = common._collect_scheduler_jobs()

    assert second[2] == first[2]
    assert second[7]["job_backend"] == "legacy-text"
    assert calls == [
        common._SQUEUE_COMBINED_CMD,
        "scontrol -o show job -d",
        common._SQUEUE_COMBINED_CMD,
    ]


@pytest.mark.parametrize("json_result", [
    (False, "slurm_load_jobs error: Socket timed out"),
    (True, "{not-json"),
])
def test_non_capability_json_failure_never_uses_text(monkeypatch, json_result):
    queue = (
        "RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:1|01:00:00|"
        "4|8G|None|1|N/A"
    )
    calls = []

    def fake_run(cmd, timeout=12):
        calls.append(cmd)
        if cmd == common._SQUEUE_COMBINED_CMD:
            return True, queue
        if cmd == "scontrol --json show jobs":
            return json_result
        raise AssertionError("unsafe legacy fallback attempted")

    monkeypatch.setattr(common, "run_cmd", fake_run)
    monkeypatch.setattr(common, "_job_query_backend", "auto")
    monkeypatch.setattr(common, "_job_query_fallback_reason", "")

    result = common._collect_scheduler_jobs()

    assert result[2:7] == ({}, {}, {}, {}, {})
    assert result[7]["job_backend"] == "unavailable"
    assert result[7]["error"]
    assert result[8] == result[7]["error"]
    assert calls == [common._SQUEUE_COMBINED_CMD, "scontrol --json show jobs"]


def test_scheduler_reuses_parallel_structured_result(monkeypatch):
    queue = (
        "RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:1|01:00:00|"
        "4|8G|None|1|N/A"
    )
    monkeypatch.setattr(
        common, "run_cmd",
        lambda cmd, timeout=12: (True, queue)
        if cmd == common._SQUEUE_COMBINED_CMD else (_ for _ in ()).throw(
            AssertionError(cmd)
        ),
    )
    monkeypatch.setattr(
        common, "collect_gpu_alloc",
        lambda: (_ for _ in ()).throw(AssertionError("second JSON query")),
    )
    monkeypatch.setattr(common, "_job_query_backend", "auto")
    monkeypatch.setattr(common, "_job_query_fallback_reason", "")
    future = Future()
    future.set_result(({}, {"42": "alice"}, {"42": 1001}, {}, {}, ""))

    result = common._collect_scheduler_jobs(future)

    assert result[3] == {"42": "alice"}
    assert result[7]["job_backend"] == "structured-json"


def test_legacy_postcheck_failure_discards_privileged_metadata(monkeypatch):
    queue = (
        "RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:1|01:00:00|"
        "4|8G|None|1|N/A"
    )
    queue_calls = 0

    def fake_run(cmd, timeout=12):
        nonlocal queue_calls
        if cmd == common._SQUEUE_COMBINED_CMD:
            queue_calls += 1
            return (True, queue) if queue_calls == 1 else (False, "timeout")
        if cmd == "scontrol --json show jobs":
            return False, "scontrol: unrecognized option '--json'"
        if cmd == "scontrol -o show job -d":
            return True, (
                "JobId=42 UserId=alice(1001) JobState=RUNNING "
                "NodeList=gpu1 Nodes=gpu1 CPU_IDs=0 Mem=0 "
                "GRES=gpu:1(IDX:0) StdOut=/logs/42.out"
            )
        raise AssertionError(cmd)

    monkeypatch.setattr(common, "run_cmd", fake_run)
    monkeypatch.setattr(common, "_job_query_backend", "auto")
    monkeypatch.setattr(common, "_job_query_fallback_reason", "")

    result = common._collect_scheduler_jobs()

    assert len(result[0]) == 1
    assert result[2:7] == ({}, {}, {}, {}, {})
    assert result[7]["job_backend"] == "unavailable"
    assert "post-check" in result[8]


def test_legacy_node_mismatch_rejects_whole_record():
    line = (
        "JobId=60 JobName=train UserId=alice(1001) JobState=RUNNING "
        "NodeList=gpu2 Nodes=gpu2 CPU_IDs=0 Mem=0 "
        "GRES=gpu:1(IDX:0) StdOut=/logs/60.out"
    )

    _assert_no_id(_parse(line, {"60": _anchor("alice", 1001)}), "60")


def test_legacy_oversized_gpu_range_keeps_identity_but_drops_allocation():
    line = (
        "JobId=61 JobName=train UserId=alice(1001) JobState=RUNNING "
        "NodeList=gpu1 Nodes=gpu1 CPU_IDs=0 Mem=0 "
        "GRES=gpu:1(IDX:0-999999) StdOut=/logs/61.out"
    )

    alloc, users, uids, details, metadata, rejected = _parse(
        line, {"61": _anchor("alice", 1001)},
    )

    assert alloc == {}
    assert users == {"61": "alice"} and uids == {"61": 1001}
    assert "61" in details and metadata["61"] == ("/logs/61.out", "", False)
    assert rejected == {"61"}
