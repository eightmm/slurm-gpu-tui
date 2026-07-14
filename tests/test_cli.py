"""Small CLI contract tests."""
import sys

from sgpu import __build__, __version__
from sgpu import cli


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
