"""Install-time service template contracts."""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest


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


def test_collector_unit_ships_placeholders_not_a_real_path():
    # A committed absolute ExecStart leaked the maintainer's checkout into the
    # public repo and read as if the unit should point at a dev tree.
    unit = (ROOT / "sgpu-collector.service").read_text()

    assert "ExecStart=@SGPU_VENV@/bin/sgpu-collector" in unit
    assert "User=@SGPU_USER@" in unit
    assert "/home/" not in unit
    assert "Restart=always" in unit


def test_collector_unit_renders_without_leftover_placeholders():
    rendered = (
        (ROOT / "sgpu-collector.service").read_text()
        .replace("@SGPU_VENV@", "/shared/sgpu/.venv")
        .replace("@SGPU_USER@", "root")
    )

    assert "@SGPU" not in rendered
    assert "ExecStart=/shared/sgpu/.venv/bin/sgpu-collector" in rendered


def test_installer_substitutes_the_collector_unit_placeholders():
    installer = (ROOT / "install.sh").read_text()

    assert 's|@SGPU_VENV@|$VENV_DIR|g' in installer
    assert 's|@SGPU_USER@|$(id -un)|g' in installer
    # and refuses to install a half-rendered unit
    assert "unresolved placeholder in generated unit" in installer


def test_root_install_skips_the_pointless_sudoers_rule():
    # A root collector runs `scontrol write batch_script` directly, so the
    # grant would give root what root already has.
    installer = (ROOT / "install.sh").read_text()

    assert "root collector — no sudoers rule needed" in installer


def test_root_install_puts_slack_config_where_root_reads_it():
    # /root/.sgpu is mode 0700, so a config there is invisible to the admin
    # who has to maintain it — and to `sgpu doctor` run as a normal user.
    installer = (ROOT / "install.sh").read_text()

    assert 'SLACK_CFG="/etc/sgpu/slack.json"' in installer
    assert 'mkdir -p "$(dirname "$SLACK_CFG")"' in installer


def test_root_log_sharing_defaults_on_with_explicit_opt_out():
    # The chosen cluster policy defaults root installs on, while the installer
    # must still disclose the exposure and preserve a deterministic opt-out.
    installer = (ROOT / "install.sh").read_text()

    assert 'elif [ "$(id -u)" = "0" ]; then\n    SHARE_LOGS=1' in installer
    assert "Logs may contain secrets. [Y/n]" in installer
    assert 'SGPU_SHARE_LOGS=0' in installer
    assert "Environment=SLURM_GPU_TUI_SHARE_LOGS=1" in installer
    assert "job log sharing needs a root collector" in installer


def test_root_job_detail_sharing_defaults_on_with_explicit_opt_out():
    installer = (ROOT / "install.sh").read_text()

    assert 'elif [ "$(id -u)" = "0" ]; then\n    SHARE_JOB_DETAILS=1' in installer
    assert "SGPU_SHARE_JOB_DETAILS=0" in installer
    assert "Environment=SLURM_GPU_TUI_SHARE_JOB_DETAILS=1" in installer
    assert "all-user job details need a root collector" in installer


def test_uninstaller_removes_root_owned_and_unit_configured_shared_logs():
    uninstaller = (ROOT / "uninstall.sh").read_text()

    assert "SYSTEM_STATE_DIR=" in uninstaller
    assert "SLURM_GPU_TUI_STATE_DIR=" in uninstaller
    assert '_rm_data_dir "$SYSTEM_STATE_DIR"' in uninstaller
    assert '_rm_data_dir "$SLURM_GPU_TUI_STATE_DIR" .sgpu-state' in uninstaller
    assert (
        '_rm_data_dir "$SLURM_GPU_TUI_STATE_DIR" .sgpu-state usage.json'
        not in uninstaller
    )
    assert (
        '_rm_data_dir "$HOME/.sgpu/state" .sgpu-state usage.json '
        "idle_state.json inventory.json"
    ) in uninstaller
    assert "$SUDO rm -rf -- \"$d\"" in uninstaller
    assert "/var/lib/sgpu .sgpu-state usage.json idle_state.json inventory.json" in uninstaller
    assert 'inventory.json logs' not in uninstaller


def test_installer_restarts_persistence_oneshot_on_every_install():
    installer = (ROOT / "install.sh").read_text()
    local = installer.split("_install_persistence_local() {", 1)[1].split(
        "\n}\n\nif $_persistence_requested", 1
    )[0]
    remote = installer.split("REMOTE_INSTALL_SCRIPT='", 1)[1].split(
        "'\n            for node", 1
    )[0]

    for path in (local, remote):
        enable = path.index("systemctl enable sgpu-gpu-persistence.service")
        restart = path.index("systemctl restart sgpu-gpu-persistence.service")
        verify = path.index("nvidia-smi --query-gpu=persistence_mode")
        assert enable < restart < verify
        assert "failed to enable persistence unit" in path
        assert "failed to reapply GPU persistence mode" in path
        assert "failed to verify GPU persistence mode" in path
        assert "systemctl enable --now sgpu-gpu-persistence.service" not in path

    assert "GPU persistence could not be enabled" in installer
    assert "PERSISTENCE_PROBE_TIMEOUT_SEC=20" in installer
    assert 'timeout "$PERSISTENCE_PROBE_TIMEOUT_SEC"' in installer
    assert "PERSISTENCE_APPLY_TIMEOUT_SEC=60" in installer
    assert 'timeout "$PERSISTENCE_APPLY_TIMEOUT_SEC"' in installer


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


def test_installer_configures_slack_bot_only_and_shows_existing_values():
    installer = (ROOT / "install.sh").read_text()

    assert "Slack bot token" in installer
    assert 'Use this? [Y/n]' in installer
    assert "Existing Slack settings found in %s" in installer
    assert "_mask_token" in installer
    assert "visible while typing" in installer
    assert "read -rs BOT_TOKEN" not in installer
    assert 'cfg.pop("url", None)' in installer
    assert "SGPU_WEBHOOK_URL" not in installer
    assert "Slack webhook URL" not in installer


@pytest.mark.parametrize("tests_pass", [True, False])
def test_deploy_isolates_pytest_and_stops_before_install_on_failure(tmp_path, tests_pass):
    repo = tmp_path / "checkout"
    repo.mkdir()
    script = repo / "deploy.sh"
    script.write_bytes((ROOT / "deploy.sh").read_bytes())
    mock_bin = tmp_path / "bin"
    mock_bin.mkdir()
    temp_root = tmp_path / "deployment-temp"
    temp_root.mkdir(mode=0o700)
    # Reproduce pytest's inherited-name/different-uid collision without root.
    foreign_root = tmp_path / "foreign-temp"
    foreign_root.mkdir()
    (foreign_root / "pytest-of-fixture").mkdir(mode=0o700)
    smoke = repo / "test_smoke.py"
    smoke.write_text("import os\ndef test_temp(tmp_path):\n"
                     "    assert tmp_path.stat().st_uid == os.getuid()\n"
                     "    assert os.environ['SGPU_TEST_PASS'] == '1'\n")
    calls = tmp_path / "calls.jsonl"

    def executable(path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
        path.chmod(0o755)

    executable(mock_bin / "uv", f"#!{sys.executable}\n" + '''
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ['SGPU_TEST_CALLS'], 'a') as log:
    log.write(json.dumps(['uv', *args]) + '\\n')
if args[0] == 'run':
    import pytest
    import _pytest.tmpdir as tmpdir
    tmpdir.get_user = lambda: 'fixture'
    tmpdir.get_user_id = lambda: os.getuid() + 1
    options = ['-q', '-p', 'no:cacheprovider', os.environ['SGPU_TEST_SMOKE']]
    if '--basetemp' in args:
        base = Path(args[args.index('--basetemp') + 1])
        assert base.parent.stat().st_uid == os.getuid()
        assert base.parent.stat().st_mode & 0o077 == 0
        assert os.environ['PYTHONDONTWRITEBYTECODE'] == '1'
        options += ['--basetemp', str(base)]
    sys.exit(pytest.main(options))
''')
    executable(mock_bin / "id", "#!/bin/sh\nprintf '0\\n'\n")
    executable(mock_bin / "grep", "#!/bin/sh\nexit 0\n")
    executable(mock_bin / "sed", "#!/bin/sh\nexit 99\n")
    executable(mock_bin / "sleep", "#!/bin/sh\nexit 0\n")
    executable(mock_bin / "mktemp", f"#!{sys.executable}\n" + '''
import os, sys, tempfile
from pathlib import Path
assert sys.argv[1] == '-d'
assert Path(sys.argv[2]).name.startswith('sgpu-deploy-tests.')
print(tempfile.mkdtemp(prefix='sgpu-deploy-tests.', dir=os.environ['SGPU_TEST_TMPDIR']))
''')
    record = f"#!{sys.executable}\n" + '''
import json, os, sys
from pathlib import Path
with open(os.environ['SGPU_TEST_CALLS'], 'a') as log:
    log.write(json.dumps([Path(sys.argv[0]).name, *sys.argv[1:]]) + '\\n')
'''
    executable(mock_bin / "systemctl", record)
    prod = tmp_path / "prod"
    executable(prod / "bin" / "python", record)
    env = dict(os.environ, PATH=f"{mock_bin}:{os.environ['PATH']}",
               SGPU_UV=str(mock_bin / "uv"), SGPU_PROD_VENV=str(prod),
               SGPU_TEST_CALLS=str(calls), SGPU_TEST_TMPDIR=str(temp_root),
               SGPU_TEST_SMOKE=str(smoke), SGPU_TEST_PASS="1" if tests_pass else "0",
               PYTEST_DEBUG_TEMPROOT=str(foreign_root))
    result = subprocess.run(["bash", str(script)], env=env, capture_output=True,
                            text=True, timeout=20)
    recorded = [json.loads(line) for line in calls.read_text().splitlines()]
    test_call = recorded[0]
    assert test_call[:2] == ["uv", "run"]
    assert "--basetemp" in test_call and "no:cacheprovider" in test_call
    assert list(temp_root.iterdir()) == []
    assert (foreign_root / "pytest-of-fixture").exists()
    if tests_pass:
        assert result.returncode == 0, result.stdout + result.stderr
        assert any(call[:3] == ["uv", "pip", "install"] for call in recorded)
        assert ["systemctl", "restart", "sgpu-collector"] in recorded
    else:
        assert result.returncode != 0
        assert len(recorded) == 1
