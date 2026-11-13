use crate::Result;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub enum Program {
    Sinfo,
    Squeue,
    Scontrol,
    Sacct,
    Scancel,
    NvidiaSmi,
    Ipmitool,
}

impl Program {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sinfo => "sinfo",
            Self::Squeue => "squeue",
            Self::Scontrol => "scontrol",
            Self::Sacct => "sacct",
            Self::Scancel => "scancel",
            Self::NvidiaSmi => "nvidia-smi",
            Self::Ipmitool => "ipmitool",
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct CommandStats {
    pub calls: u64,
    pub failures: u64,
    pub seconds: f64,
    pub last_seconds: f64,
}
static STATS: OnceLock<Mutex<BTreeMap<String, CommandStats>>> = OnceLock::new();

pub fn stats() -> BTreeMap<String, CommandStats> {
    STATS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn run(program: Program, args: &[&str], timeout: Duration) -> Result<String> {
    let start = Instant::now();
    let mut command = Command::new(program.name());
    command.args(args).env("LC_ALL", "C");
    if matches!(program, Program::Squeue) {
        command.env("SLURM_BITSTR_LEN", "0");
    }
    let result = capture(&mut command, timeout, 64 * 1024 * 1024);
    let label = match (program, args.contains(&"node")) {
        (Program::Scontrol, true) => "scontrol_nodes",
        (Program::Scontrol, false) => "scontrol_jobs",
        _ => program.name(),
    };
    let mut stats = STATS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let item = stats.entry(label.to_string()).or_default();
    item.calls += 1;
    item.failures += u64::from(result.is_err());
    item.last_seconds = start.elapsed().as_secs_f64();
    item.seconds += item.last_seconds;
    result.map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

pub fn capture(command: &mut Command, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let overflow = Arc::new(AtomicBool::new(false));
    fn reader<R: Read + Send + 'static>(
        mut stream: R,
        limit: usize,
        overflow: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buf = [0; 16384];
            loop {
                let size = stream.read(&mut buf)?;
                if size == 0 {
                    break;
                }
                let room = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buf[..size.min(room)]);
                if size > room {
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            Ok(bytes)
        })
    }
    let stdout = reader(
        child.stdout.take().ok_or("missing stdout")?,
        limit,
        overflow.clone(),
    );
    let stderr = reader(
        child.stderr.take().ok_or("missing stderr")?,
        limit.min(65536),
        overflow.clone(),
    );
    let start = Instant::now();
    let mut failure = None;
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        if status.is_some() && stdout.is_finished() && stderr.is_finished() {
            break;
        }
        if start.elapsed() >= timeout || overflow.load(Ordering::Relaxed) {
            failure = Some(
                if overflow.load(Ordering::Relaxed) {
                    "command output exceeded limit"
                } else {
                    "command timed out"
                }
                .into(),
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if failure.is_some() {
        // SAFETY: child starts its own process group; only that group is killed.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    }
    let output = stdout.join().map_err(|_| "stdout worker panicked")??;
    let error = stderr.join().map_err(|_| "stderr worker panicked")??;
    if let Some(error) = failure {
        return Err(error.into());
    }
    if overflow.load(Ordering::Relaxed) {
        return Err("command output exceeded limit".into());
    }
    if !status.is_some_and(|s| s.success()) {
        return Err(format!("command failed: {}", String::from_utf8_lossy(&error)).into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_kills_and_reaps_the_child() {
        let start = Instant::now();
        assert!(
            capture(
                Command::new("sleep").arg("5"),
                Duration::from_millis(30),
                1024
            )
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_is_bounded_and_errors_are_reported() {
        assert!(
            capture(
                Command::new("printf").arg("abcdefgh"),
                Duration::from_secs(1),
                3
            )
            .is_err()
        );
        assert!(capture(&mut Command::new("false"), Duration::from_secs(1), 3).is_err());
    }

    #[test]
    fn timeout_also_covers_pipes_inherited_by_grandchildren() {
        let start = Instant::now();
        let result = capture(
            Command::new("sh").args(["-c", "sleep 5 & exit 0"]),
            Duration::from_millis(50),
            1024,
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
