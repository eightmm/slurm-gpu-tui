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
