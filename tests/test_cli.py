"""Small CLI contract tests."""
import sys
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from sgpu import __build__, __version__
from sgpu import cli
from sgpu.common import NodeInfo
from sgpu.tui import _node_source_counts


def test_version_flag(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["sgpu", "--version"])

    cli.main()

    assert capsys.readouterr().out.strip() == f"sgpu {__version__} (build {__build__})"


def test_unit_env_enabled():
    unit = "[Service]\nEnvironment=OTHER=1\nEnvironment=SLURM_GPU_TUI_SHARE_SCRIPTS=1\n"
    assert cli._unit_env_enabled(unit, "SLURM_GPU_TUI_SHARE_SCRIPTS")
    assert not cli._unit_env_enabled(unit, "MISSING")


@pytest.mark.parametrize(("override", "enabled"), [
    ('Environment="SLURM_GPU_TUI_SHARE_LOGS=0"', False),
    ("Environment='SLURM_GPU_TUI_SHARE_LOGS=0'", False),
    ("Environment=OTHER=1 SLURM_GPU_TUI_SHARE_LOGS=0", False),
    ("Environment=SLURM_GPU_TUI_SHARE_LOGS=0 OTHER=1", False),
    ("Environment=", False),
    ("Environment=\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=1", True),
    ("Environment=SLURM_GPU_TUI_SHARE_LOGS=0 SLURM_GPU_TUI_SHARE_LOGS=1", True),
    ("Environment=OTHER=0", True),
    (" Environment = SLURM_GPU_TUI_SHARE_LOGS=0 ", False),
    ('Environment="unterminated', True),
    ("[Unit]\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=0", True),
])
def test_unit_env_enabled_applies_overrides(override, enabled):
    unit = "[Service]\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=1\n"

    assert cli._unit_env_enabled(unit + override + "\n", "SLURM_GPU_TUI_SHARE_LOGS") is enabled


@pytest.mark.parametrize(("restart", "environment", "enabled"), [
    ("no", 'Environment="SLURM_GPU_TUI_SHARE_LOGS=0"', False),
    ("always", "Environment=SLURM_GPU_TUI_SHARE_LOGS=0 SLURM_GPU_TUI_SHARE_LOGS=1", True),
    ("no", "Environment=", False),
])
def test_doctor_applies_dropins_in_order(monkeypatch, tmp_path, capsys,
                                       restart, environment, enabled):
    from sgpu import notify

    data = tmp_path / "data.json"
    data.write_text('{"nodes": [], "jobs": [], "scheduler": {"job_backend": "structured-json"}}')
    unit = tmp_path / "sgpu-collector.service"
    unit.write_text("[Service]\nRestart=on-failure\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=1\n")
    dropins = tmp_path / "sgpu-collector.service.d"
    dropins.mkdir()
    (dropins / "90-override.conf").write_text(
        f"[Service]\nRestart={restart}\n{environment}\n")
    (dropins / "10-defaults.conf").write_text(
        "[Service]\nRestart=always\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=1\n")
    real_path = cli.Path
    paths = Mock(side_effect=lambda value: unit if value ==
                 "/etc/systemd/system/sgpu-collector.service" else real_path(value))
    paths.home.return_value = tmp_path
    monkeypatch.setattr(cli, "Path", paths)
    monkeypatch.setattr(cli, "_DAEMON_DATA_FILE", data)
    monkeypatch.setattr(cli, "state_dir_candidates", lambda: [tmp_path])
    monkeypatch.setattr(cli, "run_cmd", lambda *_a, **_kw: (True, "stubbed"))
    monkeypatch.setattr(notify, "cfg_search_paths", lambda: [tmp_path / "slack.json"])
    monkeypatch.setattr(notify, "Notifier", lambda *_a, **_kw: SimpleNamespace(enabled=False))

    assert cli._cli_doctor() == 0

    lines = capsys.readouterr().out.splitlines()
    unit_line = next(line for line in lines if "collector unit" in line)
    logs_line = next(line for line in lines if "job log sharing" in line)
    assert unit_line.split(" — ", 1)[0].endswith(f" Restart={restart}")
    assert ("enabled in unit" if enabled else "off —") in logs_line


def test_parse_persistence_status():
    modes, unit = cli._parse_persistence_status(
        "modes=Enabled, Enabled,\nunit=active\n"
    )

    assert modes == ["Enabled", "Enabled"]
    assert unit == "active"


def test_parse_persistence_status_handles_missing_gpu_output():
    modes, unit = cli._parse_persistence_status("modes=\nunit=inactive\n")

    assert modes == []
    assert unit == "inactive"


def test_persistence_probe_allows_slow_nvidia_initialization():
    assert cli._PERSISTENCE_PROBE_TIMEOUT_SEC == 20


def test_scheduler_diagnostic_reports_selected_backend():
    ok, detail, backend = cli._scheduler_diagnostic({
        "job_backend": "legacy-text",
        "fallback_reason": "scontrol --json unsupported",
        "error": "",
        "rejected_records": 2,
    })

    assert ok is None
    assert backend == "legacy-text"
    assert "legacy text compatibility" in detail
    assert "2 ambiguous/changed" in detail


def test_scheduler_diagnostic_fails_closed_and_sanitizes_error():
    ok, detail, backend = cli._scheduler_diagnostic({
        "job_backend": "unavailable",
        "error": "timeout\nforged-line",
    })

    assert ok is False
    assert backend == "unavailable"
    assert detail == "timeout forged-line"


def test_scheduler_diagnostic_handles_precompat_snapshot():
    assert cli._scheduler_diagnostic(None) == (
        None, "backend unknown — restart/deploy collector", "",
    )


def test_split_node_sources_separates_cpu_poll_from_gpu_fallback():
    gpu, cpu = cli._split_node_sources([
        {"name": "cpu1", "has_gpu": False, "source": "ssh", "gpus": []},
        {"name": "cpu2", "has_gpu": False, "source": "ssh", "gpus": []},
        {"name": "gpu1", "has_gpu": True, "source": "agent", "gpus": [{}]},
        {"name": "gpu2", "has_gpu": True, "source": "ssh", "gpus": [{}]},
    ])

    assert gpu == {"agent": 1, "ssh": 1}
    assert cpu == {"ssh": 2}


def test_split_node_sources_infers_legacy_gpu_rows():
    gpu, cpu = cli._split_node_sources([
        {"source": "agent", "gres": "gpu:a5000:2", "gpus": []},
        {"source": "ssh", "gres": "(null)", "gpus": []},
    ])

    assert gpu == {"agent": 1}
    assert cpu == {"ssh": 1}


def test_tui_source_counts_do_not_call_cpu_poll_gpu_fallback():
    counts = _node_source_counts([
        NodeInfo(name="cpu1", has_gpu=False, source="ssh"),
        NodeInfo(name="cpu2", has_gpu=False, source="agent"),
        NodeInfo(name="gpu1", has_gpu=True, source="agent"),
        NodeInfo(name="gpu2", has_gpu=True, source="ssh"),
        NodeInfo(name="gpu3", has_gpu=True, source="stale", stale=True),
    ])

    assert counts == (1, 1, 1, 1, 1)


def test_fit_checks_scheduler_resources_model_and_reasons(monkeypatch, capsys):
    from dataclasses import replace
    from sgpu.common import GpuInfo
    node = NodeInfo(name="gpu1", state="idle", partition="gpu", gres="gpu:h100:2",
                    cpus="64", cpu_alloc="48", mem_total="999999", scheduler_mem_total="131072", mem_alloc="65536",
                    gpus=[GpuInfo(index=str(i), name="H100", util="0", mem_used="0", mem_total="81920") for i in range(2)])
    monkeypatch.setattr(cli, "_snapshot_nodes", lambda: [node, replace(node, name="gpu2", stale=True)])
    assert cli._cli_fit(2, model="h100", cpus=16, ram_gb=64, explain=True) == 0
    output = capsys.readouterr().out
    assert "--gres=gpu:h100:2" in output and "--cpus-per-task=16" in output
    assert "--mem=65536M" in output and "skip gpu2: stale data" in output
    assert cli._cli_fit(2, model="h100", ram_gb=65) == 1
    assert "scheduler RAM insufficient" in capsys.readouterr().out
    assert cli._cli_fit(1, model="a100") == 1
    assert "matching GPUs 0/1" in capsys.readouterr().out
