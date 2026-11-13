use sgpu::runtime;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};
const NAMES: &[&str] = &[
    "sgpu",
    "sgpu-agent",
    "sgpu-collector",
    "chkgpu",
    "sgpu-install",
];
fn source() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_sgpu-install"))
        .parent()
        .unwrap()
        .to_path_buf()
}
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sgpu-install"))
        .args(args)
        .env("HOME", root)
        .env("SLURM_GPU_TUI_AGENT_DIR", root.join("agents"))
        .env("SLURM_GPU_TUI_DATA_DIR", root.join("data"))
        .env("SLURM_GPU_TUI_STATE_DIR", root.join("state"))
        .env("SLURM_GPU_TUI_SLACK_CONFIG", root.join("slack.json"))
        .env("SLURM_GPU_TUI_SECRET_FOR_TEST", "MUST_NEVER_ENTER_UNIT")
        .output()
        .unwrap()
}
fn install(root: &Path, prefix: &Path, source: &Path, role: &str) -> Output {
    run(
        root,
        &[
            "install",
            "--no-service",
            "--prefix",
            prefix.to_str().unwrap(),
            "--bin-dir",
            source.to_str().unwrap(),
            "--role",
            role,
            "--mode",
            "cpu",
        ],
    )
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_upgrades_role_removal_and_missing_artifacts_preserve_data() {
    let root = tempfile::tempdir().unwrap();
    runtime::secure_dir(root.path(), 0o700).unwrap();
    let prefix = root.path().join("install prefix");
    success(install(root.path(), &prefix, &source(), "master"));
    let first = fs::read_link(prefix.join("current")).unwrap();
    for name in NAMES {
        let result = Command::new(prefix.join("bin").join(name))
            .arg("--version")
            .output()
            .unwrap();
        assert!(result.status.success(), "{name}");
    }
    let unit = fs::read_to_string(prefix.join("units/sgpu-collector.service")).unwrap();
    assert!(!unit.contains("MUST_NEVER_ENTER_UNIT"));
    assert!(!unit.contains("SLACK_BOT_TOKEN"));
    assert!(unit.contains("bin/sgpu-collector"));
    let sentinel = prefix.join("user-data");
    fs::write(&sentinel, "preserve").unwrap();
    success(install(root.path(), &prefix, &source(), "node"));
    let current = fs::read_link(prefix.join("current")).unwrap();
    assert_ne!(current, first);
    assert!(prefix.join(&first).join("sgpu").is_file());
    let missing = root.path().join("missing");
    fs::create_dir(&missing).unwrap();
    assert!(
        !install(root.path(), &prefix, &missing, "master")
            .status
            .success()
    );
    assert_eq!(fs::read_link(prefix.join("current")).unwrap(), current);
    success(run(
        root.path(),
        &[
            "uninstall",
            "--no-service",
            "--prefix",
            prefix.to_str().unwrap(),
            "--role",
            "master",
        ],
    ));
    assert!(prefix.join("bin/sgpu-agent").exists());
    success(run(
        root.path(),
        &[
            "uninstall",
            "--no-service",
            "--prefix",
            prefix.to_str().unwrap(),
            "--role",
            "node",
        ],
    ));
    assert!(sentinel.exists());
    assert!(prefix.join(&first).exists());
    assert!(fs::symlink_metadata(prefix.join("current")).is_err());
    assert!(!root.path().join("agents").exists());
    assert!(!root.path().join(".profile").exists());
}
#[test]
fn failed_upgrade_rolls_back_partial_links_and_unit_changes() {
    let root = tempfile::tempdir().unwrap();
    runtime::secure_dir(root.path(), 0o700).unwrap();
    let prefix = root.path().join("app");
    success(install(root.path(), &prefix, &source(), "master"));
    let current = fs::read_link(prefix.join("current")).unwrap();
    let unit = fs::read(prefix.join("units/sgpu-collector.service")).unwrap();
    fs::remove_file(prefix.join("bin/chkgpu")).unwrap();
    fs::create_dir(prefix.join("bin/chkgpu")).unwrap();
    assert!(
        !install(root.path(), &prefix, &source(), "master")
            .status
            .success()
    );
    assert_eq!(fs::read_link(prefix.join("current")).unwrap(), current);
    assert_eq!(
        fs::read(prefix.join("units/sgpu-collector.service")).unwrap(),
        unit
    );
    assert!(
        Command::new(prefix.join("bin/sgpu"))
            .arg("--version")
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn installer_rejects_unsafe_prefixes_and_preserves_private_config_keys() {
    let root = tempfile::tempdir().unwrap();
    runtime::secure_dir(root.path(), 0o700).unwrap();
    let other = root.path().join("other");
    fs::create_dir(&other).unwrap();
    let link = root.path().join("linked");
    symlink(&other, &link).unwrap();
    assert!(
        !install(root.path(), &link, &source(), "master")
            .status
            .success()
    );
    assert!(fs::read_dir(&other).unwrap().next().is_none());
    assert!(
        !run(root.path(), &["install", "--no-service", "--prefix", "/"])
            .status
            .success()
    );
    let cfg = root.path().join("slack.json");
    fs::write(
        &cfg,
        r#"{"bot_token":"fixture-token","channel":"fixture-channel","dm_users":{"alice":"U1"}}"#,
    )
    .unwrap();
    success(run(
        root.path(),
        &["slack-config", "--output", cfg.to_str().unwrap()],
    ));
    assert_eq!(
        fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(&cfg).unwrap()).unwrap();
    assert_eq!(raw["dm_users"]["alice"], "U1");
    fs::write(&cfg, "broken").unwrap();
    assert!(
        !run(
            root.path(),
            &["slack-config", "--output", cfg.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert_eq!(fs::read_to_string(cfg).unwrap(), "broken");
}

#[test]
fn nonroot_background_install_starts_and_uninstalls_only_its_local_collector() {
    if runtime::euid() == 0 {
        eprintln!("background user-install test requires a nonroot account");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    runtime::secure_dir(root.path(), 0o700).unwrap();
    let prefix = root.path().join("app");
    let commands = root.path().join("commands");
    fs::create_dir(&commands).unwrap();
    for (name, body) in [
        ("systemctl", "exit 1"),
        (
            "sinfo",
            "printf '%s\\n' 'cpu1|idle|8|0|65536|60000|(null)|0/0/8/8|cpu'",
        ),
        ("squeue", "exit 0"),
        ("scontrol", "printf '%s\\n' '{\"jobs\":[]}'"),
        ("ssh", "exit 97"),
        ("scp", "exit 97"),
    ] {
        let path = commands.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let invoke = |action: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sgpu-install"));
        command.args([
            action,
            "--prefix",
            prefix.to_str().unwrap(),
            "--role",
            "master",
        ]);
        if action == "install" {
            command.args(["--bin-dir", source().to_str().unwrap()]);
        }
        command
            .env("HOME", root.path())
            .env("PATH", &commands)
            .env("SLURM_GPU_TUI_DATA_DIR", root.path().join("data"))
            .env("SLURM_GPU_TUI_STATE_DIR", root.path().join("state"))
            .env("SLURM_GPU_TUI_AGENT_DIR", root.path().join("agents"))
            .env(
                "SLURM_GPU_TUI_SLACK_CONFIG",
                root.path().join("disabled.json"),
            )
            .env("SLURM_GPU_TUI_SACCT_SEC", "0")
            .env("SGPU_SHARE_LOGS", "0")
            .env("SGPU_SHARE_JOB_DETAILS", "0")
            .env_remove("SGPU_SLACK_BOT_TOKEN");
        command.output().unwrap()
    };
    struct Guard {
        binary: PathBuf,
        data: PathBuf,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = Command::new(&self.binary)
                .arg("--stop")
                .env("SLURM_GPU_TUI_DATA_DIR", &self.data)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let _guard = Guard {
        binary: prefix.join("bin/sgpu-collector"),
        data: root.path().join("data"),
    };
    success(invoke("install"));
    assert!(runtime::lock_is_held(&root.path().join("data/collector.lock")).unwrap());
    assert!(
        fs::read_to_string(root.path().join(".profile"))
            .unwrap()
            .contains("# sgpu native")
    );
    success(invoke("uninstall"));
    assert!(!runtime::lock_is_held(&root.path().join("data/collector.lock")).unwrap());
    assert!(root.path().join("state").exists());
    assert!(
        !fs::read_to_string(root.path().join(".profile"))
            .unwrap()
            .contains("# sgpu native")
    );
}

#[test]
fn user_service_failure_restores_the_previous_release_unit_and_running_state() {
    if runtime::euid() == 0 {
        eprintln!("user-service test requires a nonroot account");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    runtime::secure_dir(root.path(), 0o700).unwrap();
    let prefix = root.path().join("app");
    let commands = root.path().join("commands");
    fs::create_dir(&commands).unwrap();
    let script = r#"#!/bin/sh
set -eu
[ "${1:-}" != --user ] || shift
printf '%s\n' "$*" >> "$SGPU_TEST_SYSTEMCTL/calls"
case "$1" in
    show-environment|daemon-reload) exit 0 ;;
    is-enabled) [ -f "$SGPU_TEST_SYSTEMCTL/enabled" ] ;;
    is-active) [ -f "$SGPU_TEST_SYSTEMCTL/active" ] ;;
    enable) : > "$SGPU_TEST_SYSTEMCTL/enabled" ;;
    restart)
        if [ -f "$SGPU_TEST_SYSTEMCTL/fail-once" ]; then /bin/rm "$SGPU_TEST_SYSTEMCTL/fail-once"; exit 1; fi
        : > "$SGPU_TEST_SYSTEMCTL/active" ;;
    disable) /bin/rm -f "$SGPU_TEST_SYSTEMCTL/enabled"; case "$*" in *--now*) /bin/rm -f "$SGPU_TEST_SYSTEMCTL/active";; esac ;;
    stop) /bin/rm -f "$SGPU_TEST_SYSTEMCTL/active" ;;
    *) exit 99 ;;
esac
"#;
    let systemctl = commands.join("systemctl");
    fs::write(&systemctl, script).unwrap();
    fs::set_permissions(systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let invoke = |action: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sgpu-install"));
        command.args([
            action,
            "--role",
            "master",
            "--prefix",
            prefix.to_str().unwrap(),
        ]);
        if action == "install" {
            command.args(["--bin-dir", source().to_str().unwrap()]);
        }
        command
            .env("HOME", root.path())
            .env("PATH", &commands)
            .env("SGPU_TEST_SYSTEMCTL", root.path())
            .env("SLURM_GPU_TUI_DATA_DIR", root.path().join("data"))
            .env("SLURM_GPU_TUI_STATE_DIR", root.path().join("state"))
            .env("SLURM_GPU_TUI_AGENT_DIR", root.path().join("agents"))
            .env(
                "SLURM_GPU_TUI_SLACK_CONFIG",
                root.path().join("disabled.json"),
            )
            .env_remove("SGPU_SLACK_BOT_TOKEN");
        command.output().unwrap()
    };
    success(invoke("install"));
    let unit = root
        .path()
        .join(".config/systemd/user/sgpu-collector.service");
    let original = fs::read(&unit).unwrap();
    assert_eq!(
        fs::metadata(unit.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    let previous = fs::read_link(prefix.join("current")).unwrap();
    fs::write(root.path().join("fail-once"), "").unwrap();
    let failed = invoke("install");
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("previous files restored"));
    assert_eq!(fs::read(&unit).unwrap(), original);
    assert_eq!(fs::read_link(prefix.join("current")).unwrap(), previous);
    assert!(root.path().join("active").is_file());
    assert!(root.path().join("enabled").is_file());
    success(invoke("uninstall"));
    assert!(!unit.exists());
    assert!(!root.path().join("active").exists());
    assert!(root.path().join("state").is_dir());
}
