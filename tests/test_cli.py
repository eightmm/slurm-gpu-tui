"""Small CLI contract tests."""
import sys

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
