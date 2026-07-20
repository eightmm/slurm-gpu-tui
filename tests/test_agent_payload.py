"""Push-agent payload validation and delivery-mode tests."""
import json
import time

from sgpu import collector
from sgpu import agent
from sgpu.agent import AGENT_PAYLOAD_VERSION


def _payload(hostname="gpu1", kind="gpu"):
    return {
        "agent_version": AGENT_PAYLOAD_VERSION,
        "agent_build": collector._expected_agent_build(),
        "release": "1.3.0",
        "ts": time.time(),
        "hostname": hostname,
        "node_kind": kind,
        "gpus": [{
            "index": "0", "minor": "0", "name": "H100",
            "mem_total": "81920", "pids": [], "users": [],
        }] if kind == "gpu" else [],
        "mem": {"total": "250000", "used": "1000", "avail": "249000"},
    }


def test_valid_agent_payload_accepts_expected_shape():
    assert collector._valid_agent_payload("gpu1", _payload(), "gpu")
    assert collector._valid_agent_payload("cpu1", _payload("cpu1", "cpu"), "cpu")


def test_valid_agent_payload_rejects_node_kind_mismatch():
    assert not collector._valid_agent_payload("gpu1", _payload(), "cpu")
    assert not collector._valid_agent_payload("cpu1", _payload("cpu1", "cpu"), "gpu")
    bad_cpu = _payload("cpu1", "cpu")
    bad_cpu["gpus"] = _payload()["gpus"]
    assert not collector._valid_agent_payload("cpu1", bad_cpu, "cpu")


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
    monkeypatch.setattr(collector, "_read_agent_payload", lambda *args: None)
    repaired = []
    monkeypatch.setattr(collector, "_maybe_repair_agent", repaired.append)
    monkeypatch.setattr(collector, "_accumulate_usage", lambda *args: None)
    monkeypatch.setattr(collector, "_fetch_scripts", lambda jobs: {})
    collector._node_results.clear()

    data = collector.collect_all()

    assert repaired == []
    assert data["nodes"][0]["has_gpu"] is False


def test_collect_all_prefers_cpu_agent_over_ssh(monkeypatch):
    nodes = [{
        "name": "cpu1", "state": "alloc", "partition": "cpu",
        "has_gpu": False, "cpus": "64", "cpu_alloc": "64",
        "cpu_load": "60", "mem_total": "1", "mem_free": "20000",
        "mem_alloc": "200000", "gres": "(null)",
    }]
    payload = _payload("cpu1", "cpu")
    monkeypatch.setattr(
        collector, "collect_basic",
        lambda: (nodes, [], [], {}, {}, {}, ""),
    )
    monkeypatch.setattr(collector, "_read_agent_payload", lambda name, kind: payload)
    polled = []
    monkeypatch.setattr(collector, "_poll_node_bg", lambda *args, **kwargs: polled.append(args))
    monkeypatch.setattr(collector, "_accumulate_usage", lambda *args: None)
    monkeypatch.setattr(collector, "_fetch_scripts", lambda jobs: {})
    collector._node_results.clear()

    data = collector.collect_all()

    assert polled == []
    assert data["nodes"][0]["source"] == "agent"
    assert data["nodes"][0]["mem_total"] == "250000"
    assert data["nodes"][0]["mem_avail"] == "249000"


def test_effective_mem_total_falls_back_for_invalid_live_value():
    assert collector._effective_mem_total({}, "64000") == "64000"
    assert collector._effective_mem_total({"total": "0"}, "64000") == "64000"
    assert collector._effective_mem_total({"total": "N/A"}, "64000") == "64000"


def test_cpu_agent_collects_meminfo_without_gpus(tmp_path, monkeypatch):
    meminfo = tmp_path / "meminfo"
    meminfo.write_text("MemTotal:       1048576 kB\nMemAvailable:    262144 kB\n")
    read_meminfo = agent._read_meminfo
    monkeypatch.setattr(agent, "_read_meminfo", lambda: read_meminfo(meminfo))

    payload = agent.collect_local("cpu")

    assert payload["node_kind"] == "cpu"
    assert payload["gpus"] == []
    assert payload["mem"] == {"total": "1024", "used": "768", "avail": "256"}


def test_read_rapl_power_deltas_and_domains(tmp_path):
    # package-0 (cpu) + dram subdomain (ram) + core subdomain (must be skipped)
    def domain(name, dirname):
        d = tmp_path / dirname
        d.mkdir()
        (d / "name").write_text(name + "\n")
        return d
    pkg = domain("package-0", "intel-rapl:0")
    dram = domain("dram", "intel-rapl:0:0")
    core = domain("core", "intel-rapl:0:1")
    pkg_uj, dram_uj, core_uj = 1_000_000_000, 500_000_000, 400_000_000
    for d, uj in ((pkg, pkg_uj), (dram, dram_uj), (core, core_uj)):
        (d / "energy_uj").write_text(str(uj))

    agent._rapl_prev.clear()
    assert agent._read_rapl_power(tmp_path, now=100.0) == {}  # first sample: no delta

    # +120 J cpu, +12 J ram over 2s -> 60 W / 6 W; core grows too but is ignored
    (pkg / "energy_uj").write_text(str(pkg_uj + 120_000_000))
    (dram / "energy_uj").write_text(str(dram_uj + 12_000_000))
    (core / "energy_uj").write_text(str(core_uj + 99_000_000))
    assert agent._read_rapl_power(tmp_path, now=102.0) == {"cpu": "60.0", "ram": "6.0"}

    # counter wrap (delta < 0) drops that domain for one cycle
    (pkg / "energy_uj").write_text("5")
    (dram / "energy_uj").write_text(str(dram_uj + 24_000_000))
    assert agent._read_rapl_power(tmp_path, now=104.0) == {"ram": "6.0"}
    agent._rapl_prev.clear()


def test_read_rapl_power_missing_root_returns_empty(tmp_path):
    agent._rapl_prev.clear()
    assert agent._read_rapl_power(tmp_path / "nope", now=1.0) == {}


def test_parse_ipmi_power():
    out = (
        "    Instantaneous power reading:                   612 Watts\n"
        "    Minimum during sampling period:                 24 Watts\n"
        "    IPMI timestamp:                           Mon Jul 20 07:00:00 2026\n"
    )
    assert agent._parse_ipmi_power(out) == "612"
    assert agent._parse_ipmi_power("") == ""
    assert agent._parse_ipmi_power("Instantaneous power reading: N/A Watts") == ""
