"""data.json codec: one decoder, no silent field drift.

The readers used to be hand-written per call site, so fields the collector had
been publishing for releases (GPU clocks, node power) never reached the TUI,
and `sgpu --json` emitted a different shape depending on whether a collector
happened to be running.
"""
from dataclasses import asdict, fields

from sgpu.collector import _gpu_to_dict, _job_to_dict, _pending_to_dict
from sgpu.common import (
    GpuInfo, JobInfo, NodeInfo, PendingJob, from_dict, node_from_dict,
)


def test_from_dict_ignores_unknown_keys():
    g = from_dict(GpuInfo, {"index": "0", "field_from_a_newer_collector": 1})
    assert g.index == "0"


def test_from_dict_keeps_defaults_for_missing_keys():
    g = from_dict(GpuInfo, {"index": "0"})
    assert g.name == "" and g.pids == [] and g.idle_sec == 0


def test_from_dict_cached_schema_does_not_share_mutable_defaults():
    first = from_dict(GpuInfo, {"index": "0"})
    second = from_dict(GpuInfo, {"index": "1"})
    first.pids.append("123")
    first.pid_mem["123"] = "10"
    assert second.pids == [] and second.pid_mem == {}


def test_from_dict_drops_type_mismatches_instead_of_propagating():
    # A malformed entry should cost one field, not kill the refresh worker.
    g = from_dict(GpuInfo, {"index": "0", "pids": "not-a-list", "idle_sec": "x"})
    assert g.pids == [] and g.idle_sec == 0


def test_from_dict_survives_a_non_dict():
    assert from_dict(GpuInfo, ["nonsense"]) == GpuInfo()


def test_node_from_dict_builds_nested_gpus_and_jobs():
    node = node_from_dict({
        "name": "gpu1",
        "gpus": [{"index": "0", "sm_clock": "2100"}],
        "jobs": [{"jobid": "42", "user": "alice", "cpu_count": 8}],
    })
    assert node.name == "gpu1"
    assert node.gpus[0].sm_clock == "2100"
    assert node.jobs[0].cpu_count == 8


def test_node_round_trips_through_json_form():
    node = NodeInfo(
        name="gpu1", state="mixed", source="agent", has_gpu=True,
        cpu_power="120", ram_power="15", sys_power="900",
        gpus=[GpuInfo(index="0", sm_clock="2100", mem_clock="1500",
                      pid_jobid={"111": "42"}, pid_mem={"111": "8192"})],
        jobs=[JobInfo(jobid="42", user="alice")],
    )
    assert node_from_dict(asdict(node)) == node


def test_clocks_and_power_survive_decoding():
    # Regression: both readers silently dropped these.
    node = node_from_dict({
        "name": "gpu1", "sys_power": "900",
        "gpus": [{"index": "0", "sm_clock": "2100", "mem_clock": "1500"}],
    })
    assert node.sys_power == "900"
    assert (node.gpus[0].sm_clock, node.gpus[0].mem_clock) == ("2100", "1500")


def test_collector_publishes_every_dataclass_field():
    # The hand-listed writer omitted pid_mem and pid_jobid, which meant
    # SSH-polled nodes reached reconcile_gpu_alloc with no cgroup jobids and
    # never got exact GPU->job attribution.
    assert set(_gpu_to_dict(GpuInfo())) == {f.name for f in fields(GpuInfo)}
    assert set(_job_to_dict(JobInfo())) == {f.name for f in fields(JobInfo)}
    assert set(_pending_to_dict(PendingJob())) == {f.name for f in fields(PendingJob)}


def test_gpu_payload_carries_cgroup_attribution():
    g = GpuInfo(index="0", pid_jobid={"111": "42"}, pid_mem={"111": "8192"})
    published = _gpu_to_dict(g)
    assert published["pid_jobid"] == {"111": "42"}
    assert published["pid_mem"] == {"111": "8192"}


def test_node_info_exposes_the_power_fields_the_collector_publishes():
    names = {f.name for f in fields(NodeInfo)}
    assert {"cpu_power", "ram_power", "sys_power", "source", "has_gpu"} <= names
