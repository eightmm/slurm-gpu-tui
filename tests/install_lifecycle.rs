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
#[ignore = "requires root; scripts/check.sh runs these with sudo when available"]
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
#[ignore = "requires root; scripts/check.sh runs these with sudo when available"]
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
#[ignore = "requires root; scripts/check.sh runs these with sudo when available"]
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
fn nonroot_installer_rejects_install_remove_and_slack_changes_before_writes() {
    if runtime::euid() == 0 {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let prefix = root.path().join("app");
    for args in [
        vec![
            "install",
            "--no-service",
            "--prefix",
            prefix.to_str().unwrap(),
        ],
        vec!["uninstall"],
        vec!["slack-config"],
    ] {
        let output = run(root.path(), &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("require root"));
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}
