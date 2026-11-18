use serde_json::json;
use sgpu::{
    model::{Gpu, Job, Node, PendingJob, SchedulerStatus, Snapshot},
    runtime,
};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    data: PathBuf,
    state: PathBuf,
    bin: PathBuf,
    user: String,
    log: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        runtime::secure_dir(root.path(), 0o700).unwrap();
        let data = root.path().join("data");
        let state = root.path().join("state");
        let bin = root.path().join("commands");
        for directory in [&data, &state, &bin] {
            fs::create_dir(directory).unwrap();
        }
        let user = sgpu::cli::current_user();
        let log = root.path().join("owner.log");
        fs::write(&log, "fixture output\n").unwrap();
        let job = Job {
            jobid: "42".into(),
            user: user.clone(),
            uid: runtime::euid() as i64,
            node: "cpu1".into(),
            jobname: "MY_CPU_JOB".into(),
            cpu_count: 4,
            ..Default::default()
        };
        let snapshot = Snapshot {
            ts: chrono::Local::now().to_rfc3339(),
            controller: json!({"hostname":"fixture-master","observed_at":runtime::now(),"cpu_util":25,"cpu_online":8,"mem_total_bytes":17179869184_u64,"mem_used_bytes":4294967296_u64}),
            scheduler: SchedulerStatus {
                job_backend: "structured-json".into(),
                ..Default::default()
            },
            nodes: vec![Node {
                name: "gpu1".into(),
                state: "idle".into(),
                partition: "gpu".into(),
                gres: "gpu:h100:1".into(),
                source: "agent".into(),
                cpus: "16".into(),
                cpu_alloc: "0".into(),
                scheduler_mem_total: "131072".into(),
                mem_total: "131072".into(),
                mem_alloc: "0".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    name: "H100".into(),
                    util: "0".into(),
                    mem_used: "0".into(),
                    mem_total: "80000".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            jobs: vec![
                job,
                Job {
                    jobid: "99".into(),
                    user: "other-user".into(),
                    jobname: "OTHER_JOB".into(),
                    ..Default::default()
                },
            ],
            pending: vec![PendingJob {
                jobid: "43".into(),
                user: user.clone(),
                jobname: "MY_PENDING".into(),
                reason: "Resources".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        fs::write(
            data.join("data.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        fs::write(state.join("usage.json"),json!({"days":{day:{&user:{"alloc":3600,"busy":1800,"observed_alloc":3600,"waste":600}}}}).to_string()).unwrap();
        let record = json!({"jobs":[{"job_id":42,"user_id":runtime::euid(),"user_name":user,"job_state":["RUNNING"],"nodes":"cpu1","standard_output":log,"standard_error":log,"current_working_directory":root.path()}]});
        let write = |name: &str, body: String| {
            let path = bin.join(name);
            fs::write(
                &path,
                format!("#!/bin/sh\nprintf '%s\\n' '{name}' >> \"$SGPU_TEST_CALLS\"\n{body}\n"),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        write("scontrol", format!("printf '%s\\n' '{}'", record));
        write("sinfo", "printf '%s\\n' 'slurm fixture'".into());
        write(
            "sacct",
            format!(
                "case \"$*\" in --version) echo 'slurm fixture';; *) printf '%s\\n' '42|{user}|historical|gpu|gres/gpu=1|2026-11-01T00:00:00|2026-11-01T00:01:00|2026-11-01T01:01:00|COMPLETED|0:0|01:00:00|/fixture';; esac"
            ),
        );
        for name in ["ssh", "scp", "srun", "pdsh", "nvidia-smi"] {
            write(name, "exit 97".into());
        }
        Self {
            root,
            data,
            state,
            bin,
            user,
            log,
        }
    }
    fn command(&self) -> Command {
        self.binary(env!("CARGO_BIN_EXE_sgpu"))
    }
    fn binary(&self, executable: &str) -> Command {
        let mut command = Command::new(executable);
        command
            .env("HOME", self.root.path())
            .env("PATH", &self.bin)
            .env("SLURM_GPU_TUI_DATA_DIR", &self.data)
            .env("SLURM_GPU_TUI_STATE_DIR", &self.state)
            .env("SLURM_GPU_TUI_AGENT_DIR", self.root.path().join("agents"))
            .env(
                "SLURM_GPU_TUI_SLACK_CONFIG",
                self.root.path().join("disabled.json"),
            )
            .env("SGPU_TEST_CALLS", self.root.path().join("calls"))
            .env_remove("SLURM_GPU_TUI_SLACK_BOT_TOKEN");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}
#[test]
fn welcome_uses_only_fresh_cached_data_and_keeps_shared_and_personal_output_separate() {
    let fixture = Fixture::new();
    let personal = successful(
        fixture
            .command()
            .args(["welcome", "--scope", "personal"])
            .env("USER", "other-user")
            .output()
            .unwrap(),
    );
    assert!(personal.contains("MY_CPU_JOB") && personal.contains("MY_PENDING"));
    assert!(!personal.contains("OTHER_JOB"));
    let shared = successful(fixture.run(&["welcome", "--scope", "cluster"]));
    assert!(shared.contains("Oh My Slurm") && shared.contains("fixture-master"));
    assert!(
        !shared.contains("MY_CPU_JOB")
            && !shared.contains("MY_PENDING")
            && !shared.contains("My jobs")
    );
    let colored = successful(fixture.run(&["welcome", "--color", "always"]));
    assert!(colored.contains("\x1b[38;"));
    let plain = successful(
        fixture
            .command()
            .args(["welcome", "--color", "auto"])
            .env("NO_COLOR", "1")
            .output()
            .unwrap(),
    );
    assert!(!plain.contains('\x1b'));
    for raw in ["{}", "not json", "{\"ts\":\"2000-01-01T00:00:00Z\"}"] {
        fs::write(fixture.data.join("data.json"), raw).unwrap();
        let output = successful(fixture.run(&["welcome"]));
        assert!(output.contains("Snapshot unavailable"), "{output}");
        assert!(!output.contains("0 running") && !output.contains("MY_CPU_JOB"));
    }
    fs::remove_file(fixture.data.join("data.json")).unwrap();
    assert!(successful(fixture.run(&["welcome"])).contains("Snapshot unavailable"));
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn welcome_rejects_private_or_untrusted_display_settings_and_bounds_narrow_output() {
    let fixture = Fixture::new();
    let config = fixture.root.path().join("welcome.json");
    fs::write(&config, r#"{"title":"PRIVATE_CONFIG_VALUE"}"#).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let output = successful(
        fixture
            .command()
            .args(["welcome", "--config", config.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(output.contains("Login display settings unavailable"));
    assert!(!output.contains("PRIVATE_CONFIG_VALUE"));
    let output = successful(fixture.run(&["welcome", "--width", "16", "--color", "never"]));
    assert!(
        output
            .lines()
            .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 16)
    );
    assert_eq!(
        fixture.run(&["welcome", "--width", "0"]).status.code(),
        Some(2)
    );
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn quick_failure_and_queue_commands_share_the_cli_and_keep_owner_and_window_boundaries() {
    let fixture = Fixture::new();
    let uid = runtime::euid();
    let recent = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let old = (chrono::Utc::now() - chrono::Duration::hours(25))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let raw = format!(
        "42|{uid}|gpu|OUT_OF_MEMORY|0:9|{recent}\n43_1|{uid}|cpu|TIMEOUT|0:15|{recent}\n44|{}|gpu|FAILED|1:0|{recent}\n42.batch|{uid}|gpu|FAILED|1:0|{recent}\n45|{uid}|gpu|FAILED|1:0|{old}",
        uid + 1
    );
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$SGPU_TEST_CALLS"
[ "$SLURM_TIME_FORMAT" = standard ] || exit 19
[ "$TZ" = UTC ] || exit 20
[ "$SLURM_BITSTR_LEN" = 0 ] || exit 21
for arg do
    if [ "$arg" = --array ]; then
        printf '%s\n' "sacct: unrecognized option '--array'" >&2
        exit 1
    fi
done
printf '%s\n' '{raw}'
"#
    );
    fs::write(fixture.bin.join("sacct"), script).unwrap();
    for executable in [
        env!("CARGO_BIN_EXE_chkfail"),
        env!("CARGO_BIN_EXE_chkqueue"),
    ] {
        for arg in ["--help", "--version"] {
            assert!(
                fixture
                    .binary(executable)
                    .arg(arg)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }
    assert!(!fixture.root.path().join("calls").exists());
    let report: serde_json::Value = serde_json::from_str(&successful(
        fixture
            .command()
            .args(["failures", "--json"])
            .env("SLURM_TIME_FORMAT", "relative")
            .env("TZ", "Pacific/Honolulu")
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(report["user"], fixture.user);
    assert_eq!(report["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(report["outcomes"]["OUT_OF_MEMORY"], 1);
    assert_eq!(report["outcomes"]["TIMEOUT"], 1);
    assert!(report["through"].as_str().unwrap().ends_with('Z'));
    assert!(!report.to_string().contains("workdir"));
    let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains(&format!("-u {uid} -S")) && calls.contains(" -E "));
    assert!(calls.contains("-X --local"));
    assert!(!calls.contains("--array"));
    let mut alias = fixture.binary(env!("CARGO_BIN_EXE_chkfail"));
    let alias_report: serde_json::Value =
        serde_json::from_str(&successful(alias.arg("--json").output().unwrap())).unwrap();
    assert_eq!(alias_report["jobs"], report["jobs"]);
    let queue: serde_json::Value =
        serde_json::from_str(&successful(fixture.run(&["queue", "--mine", "--json"]))).unwrap();
    assert_eq!(queue["total"], 1);
    assert_eq!(queue["jobs"][0]["jobid"], "43");
    let alias_queue: serde_json::Value = serde_json::from_str(&successful(
        fixture
            .binary(env!("CARGO_BIN_EXE_chkqueue"))
            .args(["--mine", "--json"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(queue["jobs"], alias_queue["jobs"]);
    assert!(successful(fixture.run(&["explain", "43"])).contains("Reason: Resources"));
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(
        !fixture
            .run(&["failures", "--user", "other-user"])
            .status
            .success()
    );
    assert!(!fixture.run(&["failures", "--hours", "0"]).status.success());
    fs::write(fixture.bin.join("sacct"), "#!/bin/sh\nexit 77\n").unwrap();
    let unavailable = fixture.run(&["failures", "--json"]);
    assert!(!unavailable.status.success() && unavailable.stdout.is_empty());
}

fn successful(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn cli_commands_use_local_published_state_and_preserve_personal_cpu_jobs() {
    let fixture = Fixture::new();
    assert!(successful(fixture.run(&["--version"])).contains(sgpu::VERSION));
    assert!(successful(fixture.run(&["--once"])).contains("gpu1"));
    let snapshot: Snapshot = serde_json::from_str(&successful(fixture.run(&["--json"]))).unwrap();
    assert_eq!(snapshot.nodes.len(), 1);
    assert!(fixture.run(&["waste"]).status.success());
    let personal = successful(fixture.run(&["me"]));
    assert!(personal.contains("MY_CPU_JOB"));
    assert!(personal.contains("MY_PENDING"));
    assert!(!personal.contains("OTHER_JOB"));
    assert!(successful(fixture.run(&["usage", "7", "--daily"])).contains(&fixture.user));
    assert!(successful(fixture.run(&["jobs", "7"])).contains("historical"));
    let month = chrono::Local::now().format("%Y-%m").to_string();
    assert!(successful(fixture.run(&["report", &month])).contains("GPU usage report"));
    assert!(
        successful(fixture.run(&["fit", "1", "--model", "h100", "--cpus", "4", "--ram", "32"]))
            .contains("sbatch")
    );
    assert!(fixture.run(&["wait-free", "1"]).status.success());
    assert_eq!(successful(fixture.run(&["logs", "42"])), "fixture output\n");
    assert!(successful(fixture.run(&["doctor"])).contains("push-only"));
    let before = fs::read(fixture.root.path().join("calls")).unwrap();
    let host = successful(fixture.run(&["host"]));
    assert!(
        host.contains("fixture-master") && host.contains("CPU busy") && host.contains("4.0 GiB")
    );
    let users = successful(fixture.run(&["users"]));
    assert!(users.contains(&fixture.user) && users.contains("other-user"));
    assert!(users.contains("CPU cores") && users.contains("SHARED"));
    assert_eq!(before, fs::read(fixture.root.path().join("calls")).unwrap());
    assert!(
        successful(fixture.run(&["bench", "--nodes", "2", "--repeat", "1"]))
            .contains("median_update_render_ms")
    );
    assert_eq!(before, fs::read(fixture.root.path().join("calls")).unwrap());
    let suite: serde_json::Value = serde_json::from_str(&successful(fixture.run(&[
        "bench", "--suite", "--nodes", "2", "--jobs", "17", "--repeat", "1",
    ])))
    .unwrap();
    let cases = suite.as_array().unwrap();
    assert_eq!(cases.len(), 13);
    let wait = cases.iter().find(|row| row["case"] == "jobs_wait").unwrap();
    assert_eq!(wait["pending"], 17);
    for key in [
        "median_parse_ms",
        "median_apply_ms",
        "median_render_ms",
        "median_update_render_ms",
    ] {
        assert!(
            wait[key]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0)
        );
    }
    assert!(!fixture.run(&["bench", "--jobs", "17"]).status.success());
    assert!(
        !fixture
            .run(&["bench", "--suite", "--jobs", "100001"])
            .status
            .success()
    );
    assert_eq!(before, fs::read(fixture.root.path().join("calls")).unwrap());
    let calls = String::from_utf8(before).unwrap();
    assert!(
        calls
            .lines()
            .all(|c| matches!(c, "scontrol" | "sinfo" | "sacct"))
    );
    assert!(fixture.log.exists());
}
#[test]
fn host_and_users_require_fresh_publications_without_sampling_the_client() {
    let fixture = Fixture::new();
    let path = fixture.data.join("data.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    snapshot["controller"]["observed_at"] = json!(runtime::now() - 600.0);
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(!fixture.run(&["host"]).status.success());
    let old = std::time::SystemTime::now() - Duration::from_secs(60);
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    assert!(!fixture.run(&["users"]).status.success());
    assert!(!fixture.root.path().join("calls").exists());
}
#[test]
fn missing_publishers_explain_stale_nodes_without_showing_blank_or_cached_measurements() {
    let fixture = Fixture::new();
    let path = fixture.data.join("data.json");
    let mut snapshot: Snapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let node = &mut snapshot.nodes[0];
    node.stale = true;
    node.source = "stale".into();
    node.error = "missing agent payload".into();
    node.gpus[0].util.clear();
    node.gpus[0].mem_used.clear();
    node.gpus[0].alloc_user = fixture.user.clone();
    node.gpus[0].alloc_jobid = "42".into();
    let mut cached = node.clone();
    cached.name = "gpu2".into();
    cached.error = "stale agent payload".into();
    cached.gpus[0].util = "99".into();
    cached.gpus[0].mem_used = "50000".into();
    cached.gpus[0].users = vec!["previous-observed-owner".into()];
    snapshot.nodes.push(cached);
    snapshot.stale_nodes = vec!["gpu1".into(), "gpu2".into()];
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let once = successful(fixture.run(&["once"]));
    assert!(once.contains("STALE · missing agent payload"));
    assert!(once.contains("STALE · stale agent payload"));
    assert_eq!(once.matches("util    ?% VRAM ?/80000 MiB").count(), 2);
    assert!(once.contains(&format!("{} job #42 health ?", fixture.user)));
    assert!(!once.contains("previous-observed-owner") && !once.contains("50000"));
    assert!(once.contains("Run sgpu doctor"));
    assert!(!fixture.root.path().join("calls").exists());
    let doctor = fixture.run(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(1));
    let diagnosis = String::from_utf8(doctor.stdout).unwrap();
    assert!(diagnosis.contains("gpu1: missing agent payload"));
    assert!(diagnosis.contains("gpu2: stale agent payload"));
    assert!(diagnosis.contains("Master installation alone does not install node services"));
    assert!(diagnosis.contains("same shared filesystem"));
    let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
    assert_eq!(calls.lines().collect::<Vec<_>>(), ["sinfo", "sacct"]);
    let node = &mut snapshot.nodes[1];
    node.stale = false;
    node.source = "agent".into();
    node.error.clear();
    node.gpus[0].util = "0".into();
    node.gpus[0].mem_used = "0".into();
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(successful(fixture.run(&["once"])).contains("util    0% VRAM 0/80000 MiB"));
}

#[test]
fn invalid_input_is_rejected_and_private_logs_cannot_follow_symlinks() {
    let fixture = Fixture::new();
    for args in [
        vec!["fit", "0"],
        vec!["fit", "1", "--ram", "nan"],
        vec!["usage", "0"],
        vec!["--json", "--once"],
        vec!["fit", "1", "--cpus", "0"],
    ] {
        assert_eq!(fixture.run(&args).status.code(), Some(2), "{args:?}");
    }
    assert!(!fixture.run(&["logs", "--bad-id"]).status.success());
    assert!(!fixture.run(&["report", "2026-99"]).status.success());
    fs::remove_file(&fixture.log).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &fixture.log).unwrap();
    assert!(!fixture.run(&["logs", "42"]).status.success());
    assert!(!fixture.run(&[]).status.success());
}

#[test]
fn quick_commands_share_colors_respect_pipes_and_keep_json_and_columns_clean() {
    let fixture = Fixture::new();
    for (name, body) in [
        (
            "squeue",
            "printf '%s\\n' '42|gpu|alice|RUNNING|00:01|8|gpu:2|N/A|gpu1' '43|gpu|bob|COMPLETING|00:01|0|gpu:1|N/A|gpu1' '44|gpu|alice|PENDING|00:00|4|gpu:1|N/A|(Resources)' '45|gpu|bob|PENDING|00:00|4|gpu:1|N/A|(DependencyNeverSatisfied)'",
        ),
        (
            "sinfo",
            "case \"$*\" in *'%N|%C'*) echo 'gpu1|8/8/0/16';; *) echo 'gpu1|idle|gpu:4|gpu';; esac",
        ),
    ] {
        let path = fixture.bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |args: &[&str]| {
        successful(
            Command::new(env!("CARGO_BIN_EXE_chkgpu"))
                .args(args)
                .env("PATH", &fixture.bin)
                .env("HOME", fixture.root.path())
                .env("TERM", "xterm-256color")
                .env("NO_COLOR", "1")
                .output()
                .unwrap(),
        )
    };
    let plain = run(&[]);
    assert!(!plain.contains('\x1b'));
    assert!(plain.contains("GPU allocation") && plain.contains("3/4 (75%)"));
    assert!(plain.contains("Pending reasons") && plain.contains("Resources"));
    let summary = run(&["--summary"]);
    assert!(summary.contains("Node capacity") && summary.contains("CPU idle"));
    assert!(!summary.contains("GPU allocation") && !summary.contains('\x1b'));
    assert!(run(&["--color", "always"]).contains("\x1b[38;"));
    assert!(!run(&["--color", "never"]).contains('\x1b'));

    let mut snapshot: Snapshot =
        serde_json::from_slice(&fs::read(fixture.data.join("data.json")).unwrap()).unwrap();
    snapshot.pending.push(PendingJob {
        jobid: "4294967295_4294967295".into(),
        user: "another-user".into(),
        reason: "DependencyNeverSatisfied".into(),
        ..Default::default()
    });
    fs::write(
        fixture.data.join("data.json"),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    let uid = runtime::euid();
    let ended = (chrono::Utc::now() - chrono::Duration::minutes(5)).format("%Y-%m-%dT%H:%M:%S");
    fs::write(
        fixture.bin.join("sacct"),
        format!("#!/bin/sh\nprintf '%s\\n' '42|{uid}|gpu|OUT_OF_MEMORY|0:9|{ended}' '43|{uid}|gpu|CANCELLED by {uid}|0:15|{ended}'\n"),
    ).unwrap();
    let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let timestamps = regex::Regex::new(r"20\d\d[-/]\d\d[-/]\d\d[T ]\d\d:\d\d:\d\dZ?").unwrap();
    let normalize = |text: &str| timestamps.replace_all(text, "<timestamp>").into_owned();
    for (executable, prefix) in [
        (env!("CARGO_BIN_EXE_chkgpu"), vec![]),
        (env!("CARGO_BIN_EXE_chkqueue"), vec![]),
        (env!("CARGO_BIN_EXE_chkfail"), vec![]),
        (env!("CARGO_BIN_EXE_sgpu"), vec!["queue"]),
        (env!("CARGO_BIN_EXE_sgpu"), vec!["failures"]),
    ] {
        let render = |args: &[&str], term: &str, no_color: bool| {
            let mut command = fixture.binary(executable);
            command
                .args(&prefix)
                .args(args)
                .env("TERM", term)
                .env_remove("NO_COLOR");
            if no_color {
                command.env("NO_COLOR", "1");
            }
            successful(command.output().unwrap())
        };
        let plain = render(&["--color", "never"], "xterm-256color", false);
        assert!(!plain.contains('\x1b'));
        for (term, no_color) in [
            ("xterm-256color", false),
            ("xterm-256color", true),
            ("dumb", false),
        ] {
            let auto = render(&[], term, no_color);
            assert!(!auto.contains('\x1b'));
            assert_eq!(normalize(&auto), normalize(&plain));
        }
        let colored = render(&["--color", "always"], "dumb", true);
        assert!(colored.contains("\x1b[38;5;117;1msgpu ·"));
        assert_eq!(
            normalize(&ansi.replace_all(&colored, "")),
            normalize(&plain)
        );
        if prefix == ["failures"] || executable == env!("CARGO_BIN_EXE_chkfail") {
            assert!(colored.contains("\x1b[38;5;210;1mOUT_OF_MEMORY"));
            assert!(colored.contains("\x1b[38;5;222mCANCELLED"));
        } else {
            assert!(colored.contains("\x1b[38;5;210;1mDependencyNeverSatisfied"));
            assert!(colored.contains("\x1b[38;5;222mResources"));
            if executable != env!("CARGO_BIN_EXE_chkgpu") {
                assert!(plain.contains("4294967295_4294967295"));
            }
        }
        if executable != env!("CARGO_BIN_EXE_chkgpu") {
            let json = render(&["--json", "--color", "always"], "xterm-256color", false);
            assert!(!json.contains('\x1b'));
            let report: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(report["jobs"].is_array());
        }
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn interactive_terminal_opens_handles_tabs_and_restores_terminal_mode() {
    let fixture = Fixture::new();
    let preferences = fixture.root.path().join(".sgpu/ui/preferences.json");
    fs::create_dir_all(preferences.parent().unwrap()).unwrap();
    fs::write(
        &preferences,
        json!({"version":1,"tab":3,"job_sort":"wait","job_filter":"pending"}).to_string(),
    )
    .unwrap();
    fs::set_permissions(&preferences, fs::Permissions::from_mode(0o600)).unwrap();
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let mut master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut before = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), before.as_mut_ptr()) },
        0
    );
    let before = unsafe { before.assume_init() };
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut command = fixture.command();
    command
        .args(["--highlight-user", &fixture.user])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    let start = Instant::now();
    let mut output = Vec::new();
    let mut sent = false;
    while start.elapsed() < Duration::from_secs(5) {
        let mut bytes = [0; 8192];
        match master.read(&mut bytes) {
            Ok(n) => output.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(e) => panic!("terminal read: {e}"),
        }
        if !sent
            && start.elapsed() > Duration::from_millis(300)
            && output.windows(8).any(|w| w == b"\x1b[?1049h")
        {
            assert!(
                String::from_utf8_lossy(&output).contains("state:pending"),
                "saved Jobs state was not loaded"
            );
            master.write_all(b"1234sv?q").unwrap();
            std::thread::sleep(Duration::from_millis(100));
            master.write_all(b"\x1b[<0;13;24M\x1b[<0;13;24m").unwrap();
            sent = true;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(child.0.try_wait().unwrap().is_some(), "UI did not exit");
    assert!(sent);
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&preferences).unwrap()).unwrap();
    assert_eq!(saved["tab"], 3);
    assert_eq!(saved["job_sort"], "left");
    assert_eq!(saved["job_filter"], "all");
    assert_eq!(
        fs::metadata(&preferences).unwrap().permissions().mode() & 0o777,
        0o600
    );
    loop {
        let mut bytes = [0; 8192];
        match master.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("terminal drain: {e}"),
        }
    }
    assert!(
        output.windows(8).any(|w| w == b"\x1b[?1006h"),
        "mouse capture not enabled"
    );
    assert!(
        output.windows(8).any(|w| w == b"\x1b[?1006l"),
        "mouse capture not restored"
    );
    let mut after = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), after.as_mut_ptr()) },
        0
    );
    let after = unsafe { after.assume_init() };
    assert_eq!(
        before.c_lflag & (libc::ICANON | libc::ECHO),
        after.c_lflag & (libc::ICANON | libc::ECHO)
    );
}

#[test]
fn json_subcommands_fit_options_and_doctor_diagnostics_follow_contracts() {
    let fixture = Fixture::new();
    let path = fixture.data.join("data.json");
    let mut snapshot: Snapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut gpu2 = snapshot.nodes[0].clone();
    gpu2.name = "gpu2".into();
    let cpu1 = Node {
        name: "cpu1".into(),
        state: "idle".into(),
        partition: "cpu".into(),
        source: "agent".into(),
        has_gpu: false,
        cpus: "64".into(),
        cpu_alloc: "8".into(),
        scheduler_mem_total: "262144".into(),
        mem_alloc: "32768".into(),
        ..Default::default()
    };
    snapshot.nodes.push(gpu2);
    snapshot.nodes.push(cpu1);
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let agents_dir = fixture.root.path().join("agents");
    fs::create_dir(&agents_dir).unwrap();
    fs::set_permissions(&agents_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(agents_dir.join("gpu1.json"), b"{}").unwrap();
    let stale_payload = agents_dir.join("gpu2.json");
    fs::write(&stale_payload, b"{}").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(120);
    fs::File::open(&stale_payload)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    fs::write(fixture.data.join("metrics.prom"), "# dummy\n").unwrap();

    let doctor_out = successful(fixture.run(&["doctor", "--json"]));
    assert!(!doctor_out.contains('\x1b'));
    let doctor_json: serde_json::Value = serde_json::from_str(&doctor_out).unwrap();
    assert_eq!(doctor_json["ok"], true);
    assert_eq!(doctor_json["problems"], 0);
    assert_eq!(doctor_json["data_file"]["exists"], true);
    assert_eq!(doctor_json["metrics_file"]["exists"], true);
    assert_eq!(doctor_json["agent_dir"]["exists"], true);
    assert_eq!(doctor_json["agent_dir"]["fresh_payloads"], 1);
    assert_eq!(doctor_json["agent_dir"]["stale_payloads"], 1);
    let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
    assert_eq!(calls.lines().collect::<Vec<_>>(), ["sinfo", "sacct"]);

    for (args, key) in [
        (vec!["host", "--json"], "rows"),
        (vec!["users", "--json"], "users"),
        (vec!["me", "--json"], "running"),
        (vec!["explain", "43", "--json"], "nodes"),
        (vec!["waste", "--json"], "waste"),
        (vec!["usage", "7", "--json"], "users"),
        (vec!["jobs", "7", "--json"], "jobs"),
    ] {
        let out = successful(fixture.run(&args));
        assert!(!out.contains('\x1b'), "ANSI leak in {args:?}");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["version"], 1, "{args:?}");
        assert!(!parsed[key].is_null(), "missing {key} in {args:?}");
    }

    let month = chrono::Local::now().format("%Y-%m").to_string();
    let report_out = successful(fixture.run(&["report", &month, "--json"]));
    assert!(!report_out.contains('\x1b'));
    let report_json: serde_json::Value = serde_json::from_str(&report_out).unwrap();
    assert_eq!(report_json["month"], month);
    assert!(report_json["users"].is_object() && report_json["daily"].is_object());

    let explain_json: serde_json::Value =
        serde_json::from_str(&successful(fixture.run(&["explain", "43", "--json"]))).unwrap();
    assert_eq!(explain_json["job"]["jobid"], "43");
    assert_eq!(explain_json["reason_full"], "Resources");
    assert_eq!(explain_json["nodes"].as_array().unwrap().len(), 3);

    let fit_cpu: serde_json::Value = serde_json::from_str(&successful(fixture.run(&[
        "fit",
        "--cpu-only",
        "--cpus",
        "8",
        "--ram",
        "16",
        "--json",
    ])))
    .unwrap();
    assert_eq!(fit_cpu["satisfied"], true);
    let cpu_sbatch = fit_cpu["sbatch"].as_str().unwrap();
    assert!(cpu_sbatch.contains("-p cpu") && !cpu_sbatch.contains("--gres=gpu"));

    let fit_multi: serde_json::Value = serde_json::from_str(&successful(
        fixture.run(&["fit", "1", "--nodes", "2", "--cpus", "4", "--json"]),
    ))
    .unwrap();
    assert_eq!(fit_multi["satisfied"], true);
    let multi_sbatch = fit_multi["sbatch"].as_str().unwrap();
    assert!(multi_sbatch.contains("-N 2") && multi_sbatch.contains("--gres=gpu:1"));
    assert!(multi_sbatch.contains("gpu1,gpu2"));

    let fit_miss = fixture.run(&["fit", "1", "--nodes", "3", "--json"]);
    assert_eq!(fit_miss.status.code(), Some(1));
    let miss_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8(fit_miss.stdout).unwrap()).unwrap();
    assert_eq!(miss_json["satisfied"], false);
    assert!(miss_json["sbatch"].is_null());

    snapshot.nodes[0].gpus[0].alloc_user = fixture.user.clone();
    snapshot.nodes[0].gpus[0].alloc_jobid = "42".into();
    snapshot.nodes[0].gpus[0].alloc_jobids = vec!["42".into()];
    snapshot.nodes[0].gpus[0].alloc_users = vec![fixture.user.clone()];
    snapshot.nodes[0].gpus[0].idle_sec = 1200;
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let waste_hit = fixture.run(&["waste", "--verbose", "--json"]);
    assert_eq!(waste_hit.status.code(), Some(1));
    let waste_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8(waste_hit.stdout).unwrap()).unwrap();
    assert_eq!(waste_json["waste"].as_array().unwrap().len(), 1);
    assert!(waste_json["waste"][0]["details"]["42"].is_string());

    let mut exited = Command::new("/bin/true").spawn().unwrap();
    let dead_pid = exited.id();
    let _ = exited.wait();
    let pid_file = fixture.data.join("collector.pid");
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::set_permissions(&pid_file, fs::Permissions::from_mode(0o600)).unwrap();
    let stale_doctor = fixture.run(&["doctor", "--json"]);
    assert_eq!(stale_doctor.status.code(), Some(1));
    let stale_doctor_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8(stale_doctor.stdout).unwrap()).unwrap();
    assert_eq!(stale_doctor_json["collector_pid"]["status"], "stale");
}

#[test]
fn logs_falls_back_to_sacct_workdir_and_enforces_owner_and_symlink_guards() {
    let fixture = Fixture::new();
    let uid = runtime::euid();
    let workdir = fixture.root.path().join("jobwork");
    fs::create_dir(&workdir).unwrap();
    let out_path = workdir.join("slurm-55.out");
    let err_path = workdir.join("slurm-55.err");
    fs::write(&out_path, "stdout from sacct fallback\n").unwrap();

    let scontrol_path = fixture.bin.join("scontrol");
    fs::write(&scontrol_path, "#!/bin/sh\nprintf '{\"jobs\":[]}\\n'\n").unwrap();
    let sacct_path = fixture.bin.join("sacct");
    let write_sacct = |row: String| {
        fs::write(
            &sacct_path,
            format!("#!/bin/sh\ncase \"$*\" in --version) echo 'slurm fixture';; *) printf '%s\\n' '{row}';; esac\n"),
        )
        .unwrap();
    };
    write_sacct(format!("55|{uid}|{}", workdir.display()));

    assert_eq!(
        successful(fixture.run(&["logs", "55"])),
        "stdout from sacct fallback\n"
    );
    let merged_err = fixture.run(&["logs", "55", "-e"]);
    assert!(merged_err.status.success());
    assert_eq!(
        String::from_utf8(merged_err.stdout).unwrap(),
        "stdout from sacct fallback\n"
    );
    assert!(String::from_utf8_lossy(&merged_err.stderr).contains("stderr is merged into stdout"));

    fs::write(&err_path, "stderr from sacct fallback\n").unwrap();
    let separate_err = fixture.run(&["logs", "55", "-e"]);
    assert!(separate_err.status.success());
    assert_eq!(
        String::from_utf8(separate_err.stdout).unwrap(),
        "stderr from sacct fallback\n"
    );
    assert!(
        !String::from_utf8_lossy(&separate_err.stderr).contains("stderr is merged into stdout")
    );

    fs::remove_file(&out_path).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &out_path).unwrap();
    assert!(!fixture.run(&["logs", "55"]).status.success());

    fs::remove_file(&out_path).unwrap();
    fs::write(&out_path, "stdout from sacct fallback\n").unwrap();
    write_sacct(format!("55|{}|{}", uid + 1, workdir.display()));
    assert!(!fixture.run(&["logs", "55"]).status.success());

    write_sacct(format!("55|{uid}|relative/workdir"));
    assert!(!fixture.run(&["logs", "55"]).status.success());
}
