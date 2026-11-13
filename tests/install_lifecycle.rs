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
fn automatic_agent_updates_activate_and_roll_back_without_losing_cpu_mode_or_stop() {
    use std::{
        io::Write,
        process::Stdio,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let agents = root.path().join("agents");
    let runtime_dir = root.path().join("runtime");
    let cache = runtime_dir.join("updates");
    let channel = agents.join(".sgpu-updates").join(std::env::consts::ARCH);
    fs::create_dir_all(&channel).unwrap();
    for path in [&agents, &agents.join(".sgpu-updates"), &channel] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let publish = |release: &str, action: &str| {
        let executable = channel.join(format!("{release}.elf"));
        let mut compiler = Command::new("cc")
            .args(["-x", "c", "-O0", "-o"])
            .arg(&executable)
            .arg("-")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let code = format!(
            r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/prctl.h>
int main(int argc, char **argv) {{
 prctl(PR_SET_PDEATHSIG, SIGTERM);
 if (argc == 2 && !strcmp(argv[1], "--version")) {{ puts("sgpu {release}"); return 0; }}
 if (argc != 4 || strcmp(argv[1], "--worker") || strcmp(argv[2], "--mode") || strcmp(argv[3], "cpu")) return 44;
 {action}
}}
"#
        );
        compiler
            .stdin
            .take()
            .unwrap()
            .write_all(code.as_bytes())
            .unwrap();
        let output = compiler.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = serde_json::json!({"protocol":1,"version":release,"arch":std::env::consts::ARCH,"bytes":fs::metadata(&executable).unwrap().len()});
        runtime::atomic_write(
            &channel.join("latest.json"),
            &serde_json::to_vec(&metadata).unwrap(),
            0o644,
            true,
        )
        .unwrap();
    };
    publish(
        "99.0.0",
        "execl(getenv(\"SGPU_TEST_REAL_AGENT\"), \"sgpu-agent\", \"--worker\", \"--mode\", \"cpu\", (char*)0); return 9;",
    );
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sgpu-agent"));
        command
            .env("SLURM_GPU_TUI_AGENT_DIR", &agents)
            .env("SLURM_GPU_TUI_AGENT_RUNTIME_DIR", &runtime_dir)
            .env("SLURM_GPU_TUI_AGENT_AUTO_UPDATE", "1")
            .env("SLURM_GPU_TUI_AGENT_UPDATE_SEC", "1")
            .env("SLURM_GPU_TUI_AGENT_UPDATE_STARTUP_SEC", "1")
            .env("SLURM_GPU_TUI_CPU_AGENT_SEC", "0.1")
            .env("SGPU_TEST_REAL_AGENT", env!("CARGO_BIN_EXE_sgpu-agent"));
        command
    };
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Guard(
        command()
            .args(["--mode", "cpu"])
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.path().join("update.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let wait = |current: &str, blocked: &str| {
        let start = Instant::now();
        loop {
            let state: serde_json::Value = fs::read(cache.join("state.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            if state["current"] == current && state["blocked"] == blocked {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "state {state}; {}",
                fs::read_to_string(root.path().join("update.log")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait("99.0.0", "");
    let payload = agents.join(format!("{}.json", runtime::hostname().unwrap()));
    let first: serde_json::Value = serde_json::from_slice(&fs::read(&payload).unwrap()).unwrap();
    assert_eq!(first["node_kind"], "cpu");
    publish("99.0.1", "return 42;");
    wait("99.0.0", "99.0.1");
    publish("99.0.2", "for (;;) pause();");
    wait("99.0.0", "99.0.2");
    publish("99.0.3", "write(1, \"R\", 1); sleep(1); return 42;");
    wait("99.0.3", "99.0.2");
    wait("99.0.0", "99.0.3");
    let start = Instant::now();
    loop {
        let latest: serde_json::Value =
            serde_json::from_slice(&fs::read(&payload).unwrap()).unwrap();
        if latest["ts"].as_f64().unwrap() > first["ts"].as_f64().unwrap() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(command().arg("--stop").status().unwrap().success());
    assert!(child.0.wait().unwrap().success());
    let mut restarted = Guard(
        command()
            .args(["--mode", "cpu"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(restarted.0.try_wait().unwrap().is_none());
    assert!(command().arg("--stop").status().unwrap().success());
    assert!(restarted.0.wait().unwrap().success());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(cache.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["current"], "99.0.0");
    assert_eq!(state["previous"], sgpu::VERSION);
    fs::set_permissions(
        cache.join("99.0.0/sgpu-agent"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let mut cached_failure = Guard(
        command()
            .args(["--mode", "cpu"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait(sgpu::VERSION, "99.0.3");
    assert!(command().arg("--stop").status().unwrap().success());
    assert!(cached_failure.0.wait().unwrap().success());
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
