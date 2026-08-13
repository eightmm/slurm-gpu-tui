"""Push-agent payload validation and delivery-mode tests."""
import json
import os
import subprocess
import time

from sgpu import collector
from sgpu import agent
from sgpu.agent import AGENT_PAYLOAD_VERSION
from sgpu.common import (
    GpuInfo, NODE_DYNAMIC_PAYLOAD_CMD, NODE_PAYLOAD_CMD, NodeMemInfo,
    parse_node_payload,
)


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


def test_node_payload_reuses_one_pmon_sample(tmp_path):
    """PID attribution and pmon metrics must come from one driver query."""
    calls = tmp_path / "nvidia-calls"
    ps_args = tmp_path / "ps-args"
    grep_args = tmp_path / "grep-args"
    nvidia_smi = tmp_path / "nvidia-smi"
    nvidia_smi.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" >> \"$SGPU_TEST_NVIDIA_CALLS\"\n"
        "case \"$1\" in\n"
        "  --query-gpu=*) printf '%s\\n' "
        "'0, GPU-test, NVIDIA H100, 10, 20, 100, 30, 40, 50, "
        "00000000:01:00.0, 0, S, 1000, 2000' ;;\n"
        "  pmon) printf '%s\\n' '# gpu pid type fb ccpm command' "
        "'# Idx # C/G MB MB name' '0 4242 C 20 0 python' "
        "'0 4343 C 10 0 python' ;;\n"
        "esac\n"
    )
    nvidia_smi.chmod(0o755)
    ps = tmp_path / "ps"
    ps.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" >> \"$SGPU_TEST_PS_ARGS\"\n"
        "printf '%s\\n' '4242 alice' '4343 bob'\n"
    )
    ps.chmod(0o755)
    grep = tmp_path / "grep"
    grep.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" >> \"$SGPU_TEST_GREP_ARGS\"\n"
        "printf '%s\\n' '/proc/4242/cgroup:job_77' '/proc/4343/cgroup:job_88'\n"
    )
    grep.chmod(0o755)
    env = dict(os.environ, PATH=f"{tmp_path}:{os.environ['PATH']}",
               SGPU_TEST_NVIDIA_CALLS=str(calls), SGPU_TEST_PS_ARGS=str(ps_args),
               SGPU_TEST_GREP_ARGS=str(grep_args))

    proc = subprocess.run(
        ["bash", "-c", NODE_PAYLOAD_CMD], env=env, text=True,
        capture_output=True, timeout=10, check=True,
    )

    invocations = calls.read_text().splitlines()
    assert len(invocations) == 2
    assert sum(line.startswith("pmon ") for line in invocations) == 1
    assert "-p 4242,4343 -o pid=,user=" in ps_args.read_text()
    grep_call = grep_args.read_text()
    assert grep_call.count("/proc/4242/cgroup") == 1
    assert grep_call.count("/proc/4343/cgroup") == 1
    gpus, _mem = parse_node_payload(proc.stdout)
    assert gpus[0].pids == ["4242", "4343"]
    assert gpus[0].users == ["alice", "bob"]
    assert gpus[0].pid_jobid == {"4242": "77", "4343": "88"}


def test_cgroup_probe_uses_one_grep_for_all_pids():
    assert NODE_PAYLOAD_CMD.count("grep -H -m1") == 1
    assert "for p in" in NODE_PAYLOAD_CMD
    assert NODE_DYNAMIC_PAYLOAD_CMD.count("echo '---SEP---'") == 6


def test_agent_caches_static_gpu_topology_until_ttl(monkeypatch):
    agent._gpu_topology_cache.update(expires=0.0, gpu_set=(), values={})
    calls = []

    def run(command):
        calls.append(command)
        if command == NODE_PAYLOAD_CMD:
            return ([GpuInfo(index="0", uuid="u0", pci_bus="b0",
                             minor="3", slot="9")], NodeMemInfo(total="1"))
        return ([GpuInfo(index="0", uuid="u0", pci_bus="b0")],
                NodeMemInfo(total="1"))

    monkeypatch.setattr(agent, "_run_gpu_payload", run)
    monkeypatch.setattr(agent, "GPU_TOPOLOGY_TTL", 300)

    first, _ = agent._collect_gpu_payload(now=10.0)
    second, _ = agent._collect_gpu_payload(now=11.0)
    third, _ = agent._collect_gpu_payload(now=311.0)

    assert calls == [NODE_PAYLOAD_CMD, NODE_DYNAMIC_PAYLOAD_CMD, NODE_PAYLOAD_CMD]
    assert (first[0].minor, second[0].minor, third[0].minor) == ("3", "3", "3")
    assert second[0].slot == "9"


def test_agent_refreshes_topology_when_gpu_set_changes(monkeypatch):
    agent._gpu_topology_cache.update(
        expires=999.0, gpu_set=(("0", "old", "b0"),),
        values={"0": ("0", "1")},
    )
    calls = []

    def run(command):
        calls.append(command)
        if len(calls) == 1:
            return ([GpuInfo(index="0", uuid="new", pci_bus="b1")], NodeMemInfo())
        return ([GpuInfo(index="0", uuid="new", pci_bus="b1",
                         minor="4", slot="8")], NodeMemInfo())

    monkeypatch.setattr(agent, "_run_gpu_payload", run)
    gpus, _ = agent._collect_gpu_payload(now=10.0)

    assert calls == [NODE_DYNAMIC_PAYLOAD_CMD, NODE_PAYLOAD_CMD]
    assert (gpus[0].minor, gpus[0].slot) == ("4", "8")
    assert agent._gpu_topology_cache["gpu_set"] == (("0", "new", "b1"),)


def test_agent_retries_incomplete_static_topology_next_cycle(monkeypatch):
    agent._gpu_topology_cache.update(expires=0.0, gpu_set=(), values={})
    calls = []

    def run(command):
        calls.append(command)
        minor = "" if len(calls) == 1 else "3"
        return ([GpuInfo(index="0", uuid="u0", pci_bus="b0", minor=minor)],
                NodeMemInfo())

    monkeypatch.setattr(agent, "_run_gpu_payload", run)
    first, _ = agent._collect_gpu_payload(now=10.0)
    second, _ = agent._collect_gpu_payload(now=11.0)

    assert calls == [NODE_PAYLOAD_CMD, NODE_PAYLOAD_CMD]
    assert first[0].minor == "" and second[0].minor == "3"
    assert agent._gpu_topology_cache["gpu_set"] == (("0", "u0", "b0"),)


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
