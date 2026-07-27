"""Push-agent payload authorship.

AGENT_DIR is mode 1777 so that node agents can publish under NFS root_squash.
That means file existence proves nothing about who wrote it, and shape
validation cannot tell a real agent from a forgery — but a forged payload
drives Slack alerts, the waste view, and per-user GPU-hour accounting.
"""
import json
import os

import pytest

from sgpu import collector


@pytest.fixture(autouse=True)
def _isolate(tmp_path, monkeypatch):
    monkeypatch.setattr(collector, "AGENT_DIR", tmp_path)
    monkeypatch.setattr(collector, "_agent_payload_cache", {})
    monkeypatch.setattr(collector, "_untrusted_payloads", {})
    monkeypatch.setattr(collector, "_untrusted_warned", set())
    monkeypatch.setattr(collector, "AGENT_DISABLE", True)  # skip build check
    yield


def valid_payload(host="gpu1"):
    return {
        "agent_version": collector.AGENT_PAYLOAD_VERSION,
        "agent_build": "1", "ts": 0, "hostname": host, "node_kind": "gpu",
        "mem": {"total": "1", "used": "1", "avail": "1"},
        "gpus": [{"index": "0", "name": "A", "mem_total": "1",
                  "pids": [], "users": []}],
    }


def publish(tmp_path, host="gpu1", payload=None):
    p = tmp_path / f"{host}.json"
    p.write_text(json.dumps(payload if payload is not None else valid_payload(host)))
    os.utime(p, None)
    return p


def test_payload_from_our_own_uid_is_accepted(tmp_path):
    publish(tmp_path)
    assert collector._read_agent_payload("gpu1") is not None


def test_payload_from_an_untrusted_uid_is_rejected(tmp_path, monkeypatch):
    publish(tmp_path)
    monkeypatch.setenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "999999")
    assert collector._read_agent_payload("gpu1") is None


def test_rejected_payload_is_recorded_for_doctor(tmp_path, monkeypatch):
    publish(tmp_path)
    monkeypatch.setenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "999999")
    collector._read_agent_payload("gpu1")
    assert collector._untrusted_payloads == {"gpu1": os.geteuid()}


def test_rejection_is_logged_once_per_node(tmp_path, monkeypatch, capsys):
    publish(tmp_path)
    monkeypatch.setenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "999999")
    for _ in range(3):
        collector._read_agent_payload("gpu1")
    assert capsys.readouterr().out.count("not a trusted agent account") == 1


def test_recovering_trust_clears_the_record(tmp_path, monkeypatch):
    publish(tmp_path)
    monkeypatch.setenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "999999")
    collector._read_agent_payload("gpu1")
    assert collector._untrusted_payloads
    monkeypatch.delenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS")
    collector._agent_payload_cache.clear()
    collector._read_agent_payload("gpu1")
    assert collector._untrusted_payloads == {}


def test_symlinked_payload_is_ignored(tmp_path):
    # A symlink in the 1777 dir would otherwise let a user point the collector
    # at a file they do not own.
    real = tmp_path / "elsewhere.json"
    real.write_text(json.dumps(valid_payload()))
    (tmp_path / "gpu1.json").symlink_to(real)
    assert collector._read_agent_payload("gpu1") is None


def test_oversized_payload_is_ignored(tmp_path, monkeypatch):
    monkeypatch.setattr(collector, "AGENT_PAYLOAD_MAX_BYTES", 10)
    publish(tmp_path)
    assert collector._read_agent_payload("gpu1") is None


def test_hostname_mismatch_is_ignored(tmp_path):
    publish(tmp_path, payload=valid_payload("someone-else"))
    assert collector._read_agent_payload("gpu1") is None
