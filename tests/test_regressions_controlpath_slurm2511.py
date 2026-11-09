"""Regressions: ssh ControlPath alias, Slurm 25.11 job JSON node layout, doctor drop-ins."""
import json

from sgpu import cli, common
from sgpu.common import parse_job_json


def _job(resources):
    return {"jobs": [{
        "job_id": 487,
        "array_job_id": {"set": True, "number": 0},
        "array_task_id": {"set": False, "number": 0},
        "array_task_string": "",
        "het_job_id": {"set": True, "number": 0},
        "het_job_offset": {"set": True, "number": 0},
        "name": "train",
        "user_id": 2000,
        "user_name": "alice",
        "job_state": ["RUNNING"],
        "partition": "gpu",
        "nodes": "server3",
        "gres_detail": ["gpu:nvidia_geforce_rtx_5070_ti:1(IDX:0)"],
        "job_resources": resources,
    }]}


def test_job_json_slurm_25_11_nodes_allocation_layout():
    # Slurm 25.11 dropped job_resources.allocated_nodes[].nodename in favour of
    # job_resources.nodes.allocation[].name; the GPU binding was silently empty.
    payload = _job({"nodes": {"count": 1, "list": "server3", "allocation": [
        {"index": 0, "name": "server3", "cpus": {"count": 4, "used": 0}},
    ]}})
    alloc, *_ = parse_job_json(json.dumps(payload))
    assert alloc == {"server3": {"0": "487"}}


def test_job_json_allocation_layout_keeps_gres_detail_node_order():
    payload = _job({"nodes": {"count": 2, "list": "server[3-4]", "allocation": [
        {"index": 1, "name": "server4"}, {"index": 0, "name": "server3"},
    ]}})
    payload["jobs"][0]["gres_detail"] = ["gpu:x:1(IDX:0)", "gpu:x:1(IDX:1)"]
    alloc, *_ = parse_job_json(json.dumps(payload))
    assert alloc == {"server3": {"0": "487"}, "server4": {"1": "487"}}


def test_job_json_legacy_allocated_nodes_layout_still_supported():
    payload = _job({"allocated_nodes": [{"nodename": "server3"}]})
    alloc, *_ = parse_job_json(json.dumps(payload))
    assert alloc == {"server3": {"0": "487"}}


def test_control_path_uses_alias_not_hostname(monkeypatch, tmp_path):
    # With "Host server1 / HostName 10.10.0.101" in ssh_config, %h expands to the
    # IP while ssh_ensure_master() checks <dir>/server1 -> a new never-exiting
    # `ssh -fN` every TTL. %n is the alias exactly as passed on the command line.
    monkeypatch.setattr(common, "_SSH_CONTROL_DIR", "")
    monkeypatch.setattr(common.tempfile, "mkdtemp", lambda prefix: str(tmp_path))
    common.init_ssh_pool()
    assert f"ControlPath={tmp_path}/%n " in common._SSH_BASE_OPTS
    assert "/%h" not in common._SSH_BASE_OPTS


def test_unit_env_enabled_respects_dropin_override():
    unit = ("[Service]\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=1\n"
            "\n[Service]\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=0\n")
    assert not cli._unit_env_enabled(unit, "SLURM_GPU_TUI_SHARE_LOGS")
    assert cli._unit_env_enabled(unit + "Environment=SLURM_GPU_TUI_SHARE_LOGS=1\n",
                                 "SLURM_GPU_TUI_SHARE_LOGS")
