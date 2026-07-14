"""Push-agent payload validation and delivery-mode tests."""
import json
import time

from sgpu import collector
from sgpu.agent import AGENT_PAYLOAD_VERSION


def _payload(hostname="gpu1"):
    return {
        "agent_version": AGENT_PAYLOAD_VERSION,
        "agent_build": collector._expected_agent_build(),
        "release": "1.3.0",
        "ts": time.time(),
        "hostname": hostname,
        "gpus": [{
            "index": "0", "minor": "0", "name": "H100",
            "mem_total": "81920", "pids": [], "users": [],
        }],
        "mem": {"total": "250000", "used": "1000", "avail": "249000"},
    }


def test_valid_agent_payload_accepts_expected_shape():
    assert collector._valid_agent_payload("gpu1", _payload())


def test_valid_agent_payload_rejects_wrong_host_and_malformed_gpu():
    assert not collector._valid_agent_payload("gpu1", _payload("gpu2"))
    payload = _payload()
    payload["gpus"][0].pop("users")
    assert not collector._valid_agent_payload("gpu1", payload)


def test_read_agent_payload_rejects_oversized_and_symlink(tmp_path, monkeypatch):
    monkeypatch.setattr(collector, "AGENT_DIR", tmp_path)
    monkeypatch.setattr(collector, "AGENT_PAYLOAD_MAX_BYTES", 100)
    collector._agent_payload_cache.clear()

    path = tmp_path / "gpu1.json"
    path.write_text(json.dumps(_payload()))
    assert collector._read_agent_payload("gpu1") is None

    path.unlink()
    target = tmp_path / "target.json"
    target.write_text("{}")
    path.symlink_to(target)
    assert collector._read_agent_payload("gpu1") is None


def test_collect_all_does_not_repair_cpu_only_nodes(monkeypatch):
    nodes = [{
        "name": "cpu1", "state": "idle", "partition": "cpu",
        "has_gpu": False, "cpus": "64", "cpu_alloc": "0",
        "cpu_load": "0", "mem_total": "250000", "mem_free": "240000",
        "mem_alloc": "0", "gres": "(null)",
    }]
    monkeypatch.setattr(
        collector, "collect_basic",
        lambda: (nodes, [], [], {}, {}, {}, ""),
    )
    monkeypatch.setattr(collector, "_should_poll_node", lambda name: False)
    repaired = []
    monkeypatch.setattr(collector, "_maybe_repair_agent", repaired.append)
    monkeypatch.setattr(collector, "_accumulate_usage", lambda *args: None)
    monkeypatch.setattr(collector, "_fetch_scripts", lambda jobs: {})
    collector._node_results.clear()

    data = collector.collect_all()

    assert repaired == []
    assert data["nodes"][0]["has_gpu"] is False
