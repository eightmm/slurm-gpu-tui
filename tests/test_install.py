"""Install-time service template contracts."""
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def test_cpu_agent_unit_renders_without_placeholder_collisions():
    rendered = (
        (ROOT / "sgpu-cpu-agent.service").read_text()
        .replace("@SGPU_AGENT_BIN@", "/shared/sgpu/.venv/bin/sgpu-agent")
        .replace("@SGPU_AGENT_DIR@", "/shared/sgpu-nodes")
        .replace("@CPU_AGENT_SEC@", "20")
    )

    assert "@" not in rendered
    assert 'Environment="SLURM_GPU_TUI_CPU_AGENT_SEC=20"' in rendered
    assert "ExecStart=/shared/sgpu/.venv/bin/sgpu-agent --mode cpu" in rendered
    assert "Restart=always" in rendered
    assert "StartLimitIntervalSec=60" in rendered
    assert "StartLimitBurst=6" in rendered


def test_installer_has_cpu_push_opt_out():
    installer = (ROOT / "install.sh").read_text()

    assert 'CPU_PUSH_REQUEST="${SGPU_ENABLE_CPU_PUSH:-auto}"' in installer
    assert "SLURM_GPU_TUI_AGENT_DISABLE is set" in installer


def test_installer_replaces_legacy_cpu_agent_before_restart():
    installer = (ROOT / "install.sh").read_text()
    remote = installer.split("REMOTE_CPU_INSTALL='", 1)[1].split("'\n", 1)[0]

    stop = remote.index("systemctl stop sgpu-cpu-agent.service")
    kill = remote.index('pkill -f "bin/[s]gpu-agent"')
    restart = remote.index("systemctl restart sgpu-cpu-agent.service")

    assert stop < kill < restart
    assert "legacy sgpu-agent did not stop" in remote
    assert "_stop_legacy_cpu_agent_local" in installer
