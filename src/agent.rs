use crate::{
    Result,
    model::{AgentPayload, Gpu, Memory, number},
    node_telemetry::NodeSampler,
    process::{self, Program},
    runtime,
};
use clap::Parser;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq)]
pub enum Mode {
    Gpu,
    Cpu,
}

#[derive(Parser)]
#[command(version = crate::VERSION, about = "Autonomous node telemetry publisher; run locally on each node")]
pub struct Args {
    #[arg(long, value_enum, default_value = "gpu")]
    pub mode: Mode,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub daemon: bool,
    #[arg(long)]
    pub stop: bool,
}

pub fn parse_csv(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                values.push(current.trim().into());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    values.push(current.trim().into());
    values
}

pub fn parse_gpu_query(raw: &str) -> Vec<Gpu> {
    let mut gpus = Vec::new();
    let mut seen = BTreeSet::new();
    for line in raw.lines().take(65) {
        let p = parse_csv(line);
        if p.len() < 9 || p[0].parse::<u32>().is_err() || !seen.insert(p[0].clone()) {
            continue;
        }
        let field = |i| p.get(i).cloned().unwrap_or_default();
        gpus.push(Gpu {
            index: field(0),
            uuid: field(1),
            name: field(2),
            util: field(3),
            mem_used: field(4),
            mem_total: field(5),
            temp: field(6),
            power: field(7),
            power_cap: field(8),
            pci_bus: field(9),
            ecc: field(10),
            serial: field(11),
            sm_clock: field(12),
            mem_clock: field(13),
            ..Default::default()
        });
    }
    gpus
}

pub fn read_memory(root: &Path) -> Result<Memory> {
    let dir = runtime::open_dir(root)?;
    let raw = crate::node_telemetry::text_at(&dir, "meminfo")?;
    let values: BTreeMap<_, _> = raw
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key, value.split_whitespace().next()?.parse::<u64>().ok()?))
        })
        .collect();
    let total = *values
        .get("MemTotal")
        .filter(|n| **n > 0)
        .ok_or("missing MemTotal")?;
    let avail = *values
        .get("MemAvailable")
        .or_else(|| values.get("MemFree"))
        .ok_or("missing available memory")?;
    Ok(Memory {
        total: (total / 1024).to_string(),
        used: (total.saturating_sub(avail) / 1024).to_string(),
        avail: (avail / 1024).to_string(),
    })
}

pub fn attach_processes(gpus: &mut [Gpu], pmon: &str, proc_root: &Path) {
    let mut fb_column = None;
    for line in pmon.lines().take(8192) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if line.trim_start().starts_with('#') {
            if fields.contains(&"pid") {
                fb_column = fields
                    .iter()
                    .filter(|s| **s != "#")
                    .position(|s| *s == "fb");
            }
            continue;
        }
        if fields.len() < 3 || fields[1].parse::<u32>().is_err() {
            continue;
        }
        let Some(gpu) = gpus.iter_mut().find(|g| g.index == fields[0]) else {
            continue;
        };
        let pid = fields[1];
        if !gpu.pids.iter().any(|p| p == pid) {
            gpu.pids.push(pid.into());
        }
        if let Some(value) = fb_column
            .and_then(|i| fields.get(i))
            .and_then(|v| number(v))
            .filter(|v| *v >= 0.0)
        {
            gpu.pid_mem.insert(pid.into(), value.to_string());
        }
        let Ok(proc) = runtime::open_dir(proc_root) else {
            continue;
        };
        let Ok(dir) = runtime::open_dir_at(&proc, pid.as_ref()) else {
            continue;
        };
        if let Some(uid) = crate::node_telemetry::text_at(&dir, "status")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("Uid:")
                        .and_then(|s| s.split_whitespace().next()?.parse::<u32>().ok())
                })
            })
        {
            let user = runtime::user(uid)
                .map(|u| u.name)
                .unwrap_or_else(|| uid.to_string());
            if !gpu.users.contains(&user) {
                gpu.users.push(user);
            }
        }
        if let Ok(cgroup) = crate::node_telemetry::text_at(&dir, "cgroup")
            && let Some(job) = cgroup.split(['/', '\n']).find_map(|s| {
                s.strip_prefix("job_").filter(|id| {
                    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
                })
            })
        {
            gpu.pid_jobid.insert(pid.into(), job.into());
        }
    }
}

fn bus_tail(bus: &str) -> String {
    bus.to_lowercase()
        .chars()
        .rev()
        .take(7)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

pub fn topology(gpus: &mut [Gpu], proc_root: &Path, sys_root: &Path) {
    let Ok(entries) = fs::read_dir(proc_root.join("driver/nvidia/gpus")) else {
        return;
    };
    let mut minor = BTreeMap::new();
    for entry in entries.take(64).flatten() {
        let Ok((bytes, _)) = runtime::read_regular(&entry.path().join("information"), 8192, None)
        else {
            continue;
        };
        let raw = String::from_utf8_lossy(&bytes);
        let fields: BTreeMap<_, _> = raw
            .lines()
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim(), v.trim())))
            .collect();
        if let (Some(bus), Some(value)) = (fields.get("Bus Location"), fields.get("Device Minor")) {
            minor.insert(bus_tail(bus), value.to_string());
        }
    }
    let slots: BTreeMap<String, String> = fs::read_dir(sys_root.join("bus/pci/slots"))
        .ok()
        .into_iter()
        .flatten()
        .take(256)
        .flatten()
        .filter_map(|entry| {
            let (bytes, _) =
                runtime::read_regular(&entry.path().join("address"), 128, None).ok()?;
            Some((
                String::from_utf8(bytes).ok()?.trim().to_lowercase(),
                entry.file_name().to_string_lossy().into_owned(),
            ))
        })
        .collect();
    for gpu in gpus {
        gpu.minor = minor
            .get(&bus_tail(&gpu.pci_bus))
            .cloned()
            .unwrap_or_default();
        if let Ok(path) = fs::canonicalize(sys_root.join("bus/pci/devices").join(&gpu.pci_bus)) {
            for part in path.components().rev() {
                let address = part.as_os_str().to_string_lossy();
                if let Some((address, _)) = address.rsplit_once('.')
                    && let Some(slot) = slots.get(&address.to_lowercase())
                {
                    gpu.slot = slot.clone();
                    break;
                }
            }
        }
    }
}

pub struct Sampler {
    proc_root: PathBuf,
    sys_root: PathBuf,
    telemetry: NodeSampler,
    topology_next: f64,
    topology_cache: BTreeMap<String, (String, String, String)>,
    health_next: f64,
    health_fields: Option<Vec<String>>,
    health: BTreeMap<String, (String, String)>,
    health_stamp: f64,
    rapl: BTreeMap<PathBuf, (f64, u64)>,
    ipmi_next: f64,
    ipmi: String,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            proc_root: "/proc".into(),
            sys_root: "/sys".into(),
            telemetry: NodeSampler::default(),
            topology_next: 0.0,
            topology_cache: BTreeMap::new(),
            health_next: 0.0,
            health_fields: None,
            health: BTreeMap::new(),
            health_stamp: 0.0,
            rapl: BTreeMap::new(),
            ipmi_next: 0.0,
            ipmi: String::new(),
        }
    }
}

impl Sampler {
    fn gpu(&mut self, monotonic: f64) -> Result<Vec<Gpu>> {
        let fields = "--query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,pci.bus_id,ecc.errors.uncorrected.aggregate.total,serial,clocks.sm,clocks.mem";
        let timeout = Duration::from_secs_f64(runtime::env_seconds(
            "SLURM_GPU_TUI_AGENT_CMD_TIMEOUT_SEC",
            40.0,
            1.0,
        ));
        let raw = process::run(
            Program::NvidiaSmi,
            &[fields, "--format=csv,noheader,nounits"],
            timeout,
        )?;
        let mut gpus = parse_gpu_query(&raw);
        if gpus.is_empty() || gpus.len() > 64 {
            return Err("nvidia-smi returned no GPUs or exceeded the GPU limit".into());
        }
        let pmon = process::run(Program::NvidiaSmi, &["pmon", "-c", "1", "-s", "m"], timeout)?;
        attach_processes(&mut gpus, &pmon, &self.proc_root);
        let unchanged = gpus.len() == self.topology_cache.len()
            && gpus.iter().all(|g| {
                self.topology_cache
                    .get(&g.uuid)
                    .is_some_and(|(bus, _, _)| bus == &g.pci_bus)
            });
        if monotonic >= self.topology_next || !unchanged {
            topology(&mut gpus, &self.proc_root, &self.sys_root);
            self.topology_cache = gpus
                .iter()
                .map(|g| {
                    (
                        g.uuid.clone(),
                        (g.pci_bus.clone(), g.minor.clone(), g.slot.clone()),
                    )
                })
                .collect();
            self.topology_next = if gpus.iter().all(|g| !g.minor.is_empty()) {
                monotonic + runtime::env_seconds("SLURM_GPU_TUI_GPU_TOPOLOGY_TTL_SEC", 300.0, 0.0)
            } else {
                0.0
            };
        } else {
            for gpu in &mut gpus {
                if let Some((_, minor, slot)) = self.topology_cache.get(&gpu.uuid) {
                    gpu.minor = minor.clone();
                    gpu.slot = slot.clone();
                }
            }
        }
        self.health(&mut gpus, monotonic);
        Ok(gpus)
    }

    fn health(&mut self, gpus: &mut [Gpu], monotonic: f64) {
        if monotonic >= self.health_next {
            let result = (|| -> Result<BTreeMap<String, (String, String)>> {
                if self.health_fields.is_none() {
                    let help = process::run(
                        Program::NvidiaSmi,
                        &["--help-query-gpu"],
                        Duration::from_secs(3),
                    )?;
                    let mut fields = Vec::new();
                    for choices in [
                        &[
                            "clocks_event_reasons.active",
                            "clocks_throttle_reasons.active",
                        ][..],
                        &["gpu_recovery_action"][..],
                    ] {
                        if let Some(field) = choices.iter().find(|field| help.contains(**field)) {
                            fields.push(field.to_string());
                        }
                    }
                    self.health_fields = Some(fields);
                }
                let fields = self.health_fields.as_ref().ok_or("missing health fields")?;
                let mut values = BTreeMap::new();
                if fields.is_empty() {
                    return Ok(values);
                }
                let query = format!("--query-gpu=index,{}", fields.join(","));
                let raw = process::run(
                    Program::NvidiaSmi,
                    &[&query, "--format=csv,noheader,nounits"],
                    Duration::from_secs(3),
                )?;
                for line in raw.lines().take(64) {
                    let parts = parse_csv(line);
                    if parts.len() != fields.len() + 1 {
                        continue;
                    }
                    let mut reasons = String::new();
                    let mut action = String::new();
                    for (field, value) in fields.iter().zip(&parts[1..]) {
                        if field.ends_with(".active") {
                            let bits = value
                                .strip_prefix("0x")
                                .map(|v| u64::from_str_radix(v, 16))
                                .unwrap_or_else(|| value.parse());
                            if let Ok(bits) = bits {
                                reasons = [
                                    (4, "power-cap"),
                                    (8, "hw-slowdown"),
                                    (16, "sync-boost"),
                                    (32, "sw-thermal"),
                                    (64, "hw-thermal"),
                                    (128, "power-brake"),
                                ]
                                .into_iter()
                                .filter_map(|(flag, label)| (bits & flag != 0).then_some(label))
                                .collect::<Vec<_>>()
                                .join(",");
                                if reasons.is_empty() {
                                    reasons = "none".into();
                                }
                            }
                        } else if matches!(
                            value.to_lowercase().as_str(),
                            "none" | "reset" | "reboot" | "drain p2p" | "drain and reset"
                        ) {
                            action = value.to_lowercase();
                        }
                    }
                    values.insert(parts[0].clone(), (reasons, action));
                }
                Ok(values)
            })();
            match result {
                Ok(values) => {
                    self.health = values;
                    self.health_stamp = runtime::now();
                    self.health_next =
                        monotonic + runtime::env_seconds("SLURM_GPU_TUI_TELEMETRY_SEC", 10.0, 10.0);
                }
                Err(_) => {
                    self.health.clear();
                    self.health_stamp = 0.0;
                    self.health_next = monotonic + 60.0;
                }
            }
        }
        for gpu in gpus {
            if let Some((reasons, action)) = self.health.get(&gpu.index) {
                gpu.clock_reasons = reasons.clone();
                gpu.recovery_action = action.clone();
                gpu.health_observed_at = self.health_stamp;
            }
        }
    }

    fn power(&mut self, monotonic: f64) -> BTreeMap<String, String> {
        let mut watts = BTreeMap::<String, f64>::new();
        let root = self.sys_root.join("class/powercap");
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries
                .take(128)
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("intel-rapl:"))
            {
                let Ok(path) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                if !path.starts_with(&self.sys_root) {
                    continue;
                }
                let Ok(dir) = runtime::open_dir(&path) else {
                    continue;
                };
                let Ok(name) = crate::node_telemetry::text_at(&dir, "name") else {
                    continue;
                };
                let depth = entry.file_name().to_string_lossy().matches(':').count();
                let key = if depth == 1 {
                    "cpu"
                } else if name.trim() == "dram" {
                    "ram"
                } else {
                    continue;
                };
                let Some(uj) = crate::node_telemetry::text_at(&dir, "energy_uj")
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                else {
                    continue;
                };
                if let Some((previous_time, previous_energy)) =
                    self.rapl.insert(path, (monotonic, uj))
                    && monotonic > previous_time
                    && uj >= previous_energy
                {
                    *watts.entry(key.into()).or_default() +=
                        (uj - previous_energy) as f64 / (monotonic - previous_time) / 1e6;
                }
            }
        }
        let mut power: BTreeMap<_, _> = watts
            .into_iter()
            .map(|(k, v)| (k, format!("{v:.1}")))
            .collect();
        if monotonic >= self.ipmi_next {
            self.ipmi = process::run(
                Program::Ipmitool,
                &["dcmi", "power", "reading"],
                Duration::from_secs(5),
            )
            .ok()
            .and_then(|raw| {
                raw.lines().find_map(|line| {
                    line.contains("Instantaneous power reading")
                        .then(|| {
                            line.split_once(':')?
                                .1
                                .split_whitespace()
                                .next()
                                .and_then(number)
                        })
                        .flatten()
                })
            })
            .filter(|n| *n >= 0.0)
            .map(|n| n.to_string())
            .unwrap_or_default();
            self.ipmi_next = monotonic + if self.ipmi.is_empty() { 60.0 } else { 10.0 };
        }
        if !self.ipmi.is_empty() {
            power.insert("sys".into(), self.ipmi.clone());
        }
        power
    }

    pub fn collect(&mut self, mode: Mode, monotonic: f64) -> Result<AgentPayload> {
        let gpus = if mode == Mode::Gpu {
            self.gpu(monotonic)?
        } else {
            Vec::new()
        };
        Ok(AgentPayload {
            agent_version: crate::AGENT_PAYLOAD_VERSION,
            release: crate::VERSION.into(),
            package_build: crate::VERSION.into(),
            agent_build: crate::VERSION.into(),
            ts: runtime::now(),
            hostname: runtime::hostname()?,
            node_kind: if mode == Mode::Gpu { "gpu" } else { "cpu" }.into(),
            gpus,
            mem: read_memory(&self.proc_root)?,
            telemetry: self.telemetry.sample(monotonic, runtime::now()),
            power: self.power(monotonic),
        })
    }
}

pub fn run(args: Args) -> Result<()> {
    if let Some(directory) = std::env::var_os("SLURM_GPU_TUI_AGENT_RUNTIME_DIR") {
        runtime::secure_dir(Path::new(&directory), 0o700)?;
    }
    if args.stop {
        return stop_local();
    }
    if args.daemon && !runtime::daemonize(&runtime::agent_runtime("log"))? {
        return Ok(());
    }
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopped.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
    let _lock = runtime::Lock::acquire(&runtime::agent_runtime("lock"), Duration::from_secs(5))?;
    let directory = runtime::agent_dir();
    fs::create_dir_all(&directory)?;
    let destination = directory.join(format!("{}.json", runtime::hostname()?));
    let interval = Duration::from_secs_f64(if args.mode == Mode::Gpu {
        runtime::env_seconds("SLURM_GPU_TUI_AGENT_SEC", 3.0, 0.1)
    } else {
        runtime::env_seconds("SLURM_GPU_TUI_CPU_AGENT_SEC", 20.0, 0.1)
    });
    let epoch = Instant::now();
    let mut sampler = Sampler::default();
    let mut failures = 0u64;
    while !stopped.load(Ordering::Relaxed) {
        let cycle = Instant::now();
        let result = sampler
            .collect(args.mode, epoch.elapsed().as_secs_f64())
            .and_then(|payload| {
                runtime::publish_agent(&destination, &serde_json::to_vec(&payload)?)
            });
        match result {
            Ok(()) => {
                if failures > 0 {
                    eprintln!("[agent] recovered after {failures} failures");
                }
                failures = 0;
            }
            Err(error) => {
                failures += 1;
                eprintln!("[agent] collect/publish failed ({failures}): {error}");
                if args.once {
                    return Err(error);
                }
            }
        }
        if args.once {
            break;
        }
        while cycle.elapsed() < interval && !stopped.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

fn stop_local() -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let lock = runtime::agent_runtime("lock");
    let st = match fs::symlink_metadata(&lock) {
        Ok(st) => st,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !st.is_file() || st.uid() != runtime::euid() {
        return Err("unsafe agent lock file".into());
    }
    let (raw, _) = runtime::read_regular(Path::new("/proc/locks"), 4 * 1024 * 1024, None)?;
    for line in String::from_utf8_lossy(&raw).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[1] != "FLOCK" {
            continue;
        }
        let device: Vec<_> = fields[5].split(':').collect();
        if device.len() != 3 {
            continue;
        }
        let major = u32::from_str_radix(device[0], 16).ok();
        let minor = u32::from_str_radix(device[1], 16).ok();
        let inode = device[2].parse::<u64>().ok();
        if major != Some(libc::major(st.dev()))
            || minor != Some(libc::minor(st.dev()))
            || inode != Some(st.ino())
        {
            continue;
        }
        let pid = fields[4].parse::<u32>()?;
        if pid <= 1 || pid == std::process::id() {
            return Err("invalid agent lock holder".into());
        }
        let proc = PathBuf::from(format!("/proc/{pid}"));
        if fs::metadata(&proc)?.uid() != runtime::euid() {
            return Err("agent holder belongs to another user".into());
        }
        let (args, _) = runtime::read_regular(&proc.join("cmdline"), 65536, None)?;
        let valid = args.split(|b| *b == 0).any(|arg| {
            Path::new(std::ffi::OsStr::from_bytes(arg))
                .file_name()
                .is_some_and(|s| s == "sgpu-agent")
                || arg == b"sgpu.agent"
        });
        if !valid {
            return Err("lock holder is not an sgpu agent".into());
        }
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let _lock = runtime::Lock::acquire(&lock, Duration::from_secs(60))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_fields_and_unknown_measurements_survive() {
        let gpus = parse_gpu_query(
            "0, GPU-1, \"Model, One\", N/A, 100, 80000, 60, 200, 300, 00000000:21:00.0, N/A, serial, 1200, 1500",
        );
        assert_eq!(gpus[0].name, "Model, One");
        assert!(number(&gpus[0].util).is_none());
        assert_eq!(gpus[0].pci_bus, "00000000:21:00.0");
    }

    #[test]
    fn pmon_memory_and_cgroup_identity_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("111")).unwrap();
        fs::write(
            tmp.path().join("111/status"),
            format!("Uid:\t{}\t0\t0\t0\n", runtime::euid()),
        )
        .unwrap();
        fs::write(
            tmp.path().join("111/cgroup"),
            "0::/system.slice/slurmstepd.scope/job_42/step_batch/user/task_0",
        )
        .unwrap();
        let mut gpus = vec![Gpu {
            index: "0".into(),
            ..Default::default()
        }];
        attach_processes(
            &mut gpus,
            "# gpu pid type fb command\n# Idx # C/G MB name\n0 111 C 8192 train\n",
            tmp.path(),
        );
        assert_eq!(gpus[0].pid_mem["111"], "8192");
        assert_eq!(gpus[0].pid_jobid["111"], "42");
        assert_eq!(gpus[0].pids, ["111"]);
    }
}
