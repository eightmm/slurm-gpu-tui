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
        let mut command = Command::new(env!("CARGO_BIN_EXE_sgpu"));
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
    assert!(
        successful(fixture.run(&["bench", "--nodes", "2", "--repeat", "1"]))
            .contains("median_update_render_ms")
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
fn chkgpu_colors_are_opt_in_for_pipes_and_remain_explicitly_controllable() {
    let fixture = Fixture::new();
    for (name, body) in [
        (
            "squeue",
            "echo '42|gpu|alice|RUNNING|00:01|8|gpu:2|N/A|gpu1'",
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
    assert!(plain.contains("GPU allocation") && plain.contains("2/4 (50%)"));
    assert!(run(&["--color", "always"]).contains("\x1b[38;"));
    assert!(!run(&["--color", "never"]).contains('\x1b'));
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
            master.write_all(b"2341?q").unwrap();
            std::thread::sleep(Duration::from_millis(100));
            master.write_all(b"q").unwrap();
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
