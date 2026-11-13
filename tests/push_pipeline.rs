use sgpu::{
    model::{AgentPayload, Gpu, Memory, Snapshot},
    runtime,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Cluster {
    root: tempfile::TempDir,
    bin: PathBuf,
    agents: PathBuf,
    data: PathBuf,
    state: PathBuf,
    trace: PathBuf,
}
impl Cluster {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        runtime::secure_dir(root.path(), 0o700).unwrap();
        let bin = root.path().join("commands");
        let agents = root.path().join("agents");
        let data = root.path().join("data");
        let state = root.path().join("state");
        let trace = root.path().join("calls");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&agents).unwrap();
        let user = "fixture";
        let uid = runtime::euid();
        let json=serde_json::json!({"jobs":[{"job_id":42,"user_name":user,"user_id":uid,"job_state":["RUNNING"],"name":"train","nodes":"gpu1","job_resources":{"nodes":{"allocation":[{"index":0,"name":"gpu1"}]}},"gres_detail":["gpu:h100:1(IDX:0)"],"standard_output":"/private/owner/log","current_working_directory":"/private/owner"}]}).to_string();
        let write = |name: &str, body: &str| {
            let path = bin.join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s %s\\n' '{name}' \"$*\" >> \"$SGPU_TEST_TRACE\"\n{body}\n"
                ),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        write(
            "sinfo",
            "printf '%s\\n' 'gpu1|idle|8|0.1|65536|60000|gpu:h100:1|4/4/0/8|gpu'",
        );
        write(
            "squeue",
            &format!(
                "printf '%s\\n' 'RUNNING|42|{user}|{uid}|gpu|00:01|gpu1|gpu:h100:1|01:00:00|4|8G|None|1|N/A'"
            ),
        );
        write(
            "scontrol",
            &format!(
                "case \"$*\" in\n '--json show jobs') printf '%s\\n' '{json}' ;;\n '-o show node') printf '%s\\n' 'NodeName=gpu1 AllocMem=8192' ;;\n *) exit 17 ;;\nesac"
            ),
        );
        for program in ["ssh", "scp", "srun", "pdsh"] {
            write(program, "exit 91");
        }
        Self {
            root,
            bin,
            agents,
            data,
            state,
            trace,
        }
    }
    fn collector(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sgpu-collector"))
            .arg("--once")
            .env("PATH", &self.bin)
            .env("SGPU_TEST_TRACE", &self.trace)
            .env("SLURM_GPU_TUI_AGENT_DIR", &self.agents)
            .env("SLURM_GPU_TUI_DATA_DIR", &self.data)
            .env("SLURM_GPU_TUI_STATE_DIR", &self.state)
            .env("SLURM_GPU_TUI_SACCT_SEC", "0")
            .env("SLURM_GPU_TUI_SHARE_LOGS", "0")
            .env("SLURM_GPU_TUI_SHARE_SCRIPTS", "0")
            .env("SLURM_GPU_TUI_SHARE_JOB_DETAILS", "0")
            .env(
                "SLURM_GPU_TUI_SLACK_CONFIG",
                self.root.path().join("disabled-slack.json"),
            )
            .env_remove("SLURM_GPU_TUI_METRICS_FILE")
            .output()
            .unwrap()
    }
}

#[test]
fn collector_merges_push_payload_without_a_node_request_and_preserves_inventory_when_it_stops() {
    let cluster = Cluster::new();
    let payload = AgentPayload {
        agent_version: 9,
        release: "fixture".into(),
        package_build: "fixture".into(),
        agent_build: "fixture".into(),
        ts: runtime::now(),
        hostname: "gpu1".into(),
        node_kind: "gpu".into(),
        gpus: vec![Gpu {
            index: "0".into(),
            name: "H100".into(),
            mem_total: "80000".into(),
            util: "0".into(),
            ..Default::default()
        }],
        mem: Memory {
            total: "65536".into(),
            used: "1024".into(),
            avail: "64512".into(),
        },
        telemetry: serde_json::json!({}),
        power: BTreeMap::new(),
    };
    fs::write(
        cluster.agents.join("gpu1.json"),
        serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    let first = cluster.collector();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let snapshot: Snapshot = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(snapshot.nodes[0].source, "agent");
    assert!(!snapshot.nodes[0].stale);
    assert_eq!(snapshot.nodes[0].gpus[0].alloc_jobid, "42");
    assert!(snapshot.jobs[0].detail.is_empty());
    assert!(!String::from_utf8_lossy(&first.stdout).contains("/private/owner"));
    fs::remove_file(cluster.agents.join("gpu1.json")).unwrap();
    let second = cluster.collector();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let snapshot: Snapshot = serde_json::from_slice(&second.stdout).unwrap();
    assert!(snapshot.nodes[0].stale);
    assert_eq!(snapshot.nodes[0].gpus[0].name, "H100");
    let text = fs::read_to_string(cluster.data.join("metrics.prom")).unwrap();
    assert!(text.contains("sgpu_gpus_free 0\n"));
    let calls = fs::read_to_string(&cluster.trace).unwrap();
    for line in calls.lines() {
        assert!(
            line.starts_with("sinfo ")
                || line.starts_with("squeue ")
                || line.starts_with("scontrol "),
            "unexpected command: {line}"
        );
    }
    assert!(cluster.state.join("inventory.json").exists());
    assert!(cluster.root.path().exists());
}

#[test]
fn cpu_agent_publishes_locally_without_scheduler_or_remote_commands() {
    let cluster = Cluster::new();
    let result = Command::new(env!("CARGO_BIN_EXE_sgpu-agent"))
        .args(["--mode", "cpu", "--once"])
        .env("PATH", &cluster.bin)
        .env("SGPU_TEST_TRACE", &cluster.trace)
        .env("SLURM_GPU_TUI_AGENT_DIR", &cluster.agents)
        .env(
            "SLURM_GPU_TUI_AGENT_RUNTIME_DIR",
            cluster.root.path().join("runtime"),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let files: Vec<_> = fs::read_dir(&cluster.agents).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    let payload: AgentPayload =
        serde_json::from_slice(&fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(payload.agent_version, 9);
    assert_eq!(payload.node_kind, "cpu");
    assert!(payload.gpus.is_empty());
    assert!(payload.mem.total.parse::<u64>().unwrap() > 0);
    assert!(!cluster.trace.exists());
}

#[test]
fn owned_log_helper_has_a_bounded_stdout_and_rejects_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("log");
    fs::write(&path, "0123456789").unwrap();
    let invoke = |path: &Path| {
        Command::new(env!("CARGO_BIN_EXE_sgpu-collector"))
            .arg("__read-owned-log")
            .arg(path)
            .arg("4")
            .output()
            .unwrap()
    };
    let valid = invoke(&path);
    assert!(valid.status.success());
    assert_eq!(valid.stdout, b"6789");
    symlink(&path, tmp.path().join("link")).unwrap();
    assert!(!invoke(&tmp.path().join("link")).status.success());
}

#[test]
fn local_stop_waits_for_the_matching_cpu_publisher_and_is_idempotent() {
    let cluster = Cluster::new();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sgpu-agent"));
        command
            .env("PATH", &cluster.bin)
            .env("SGPU_TEST_TRACE", &cluster.trace)
            .env("SLURM_GPU_TUI_AGENT_DIR", &cluster.agents)
            .env(
                "SLURM_GPU_TUI_AGENT_RUNTIME_DIR",
                cluster.root.path().join("runtime"),
            );
        command
    };
    assert!(command().arg("--stop").status().unwrap().success());
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
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = std::time::Instant::now();
    while fs::read_dir(&cluster.agents).unwrap().next().is_none()
        && start.elapsed() < std::time::Duration::from_secs(5)
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(fs::read_dir(&cluster.agents).unwrap().next().is_some());
    assert!(command().arg("--stop").status().unwrap().success());
    assert!(child.0.wait().unwrap().success());
    assert!(command().arg("--stop").status().unwrap().success());
    assert!(!cluster.trace.exists());
}
