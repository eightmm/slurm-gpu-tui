use crate::{
    Result,
    model::{AgentPayload, Gpu, Job, Node, Snapshot, json_number, number},
    node_telemetry, rules, runtime,
    scheduler::{self, Sample},
    usage,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Config {
    pub data: PathBuf,
    pub state: PathBuf,
    pub agents: PathBuf,
    pub max_age: f64,
    pub max_payload: u64,
    pub trusted: BTreeSet<u32>,
    pub share_details: bool,
    pub share_scripts: bool,
    pub share_logs: bool,
}
impl Default for Config {
    fn default() -> Self {
        let agents = runtime::agent_dir();
        Self {
            data: runtime::data_dir(),
            state: runtime::state_dir(),
            trusted: runtime::trusted_uids(&agents),
            agents,
            max_age: runtime::env_seconds("SLURM_GPU_TUI_AGENT_MAX_AGE_SEC", 45.0, 1.0),
            max_payload: runtime::env_seconds("SLURM_GPU_TUI_AGENT_MAX_BYTES", 1048576.0, 1.0)
                .min(16.0 * 1048576.0) as u64,
            share_details: runtime::enabled("SLURM_GPU_TUI_SHARE_JOB_DETAILS"),
            share_scripts: runtime::enabled("SLURM_GPU_TUI_SHARE_SCRIPTS"),
            share_logs: runtime::enabled("SLURM_GPU_TUI_SHARE_LOGS"),
        }
    }
}

#[derive(Clone)]
struct Observation {
    payload: AgentPayload,
    stamp: f64,
}

struct StoragePump {
    requests: SyncSender<BTreeMap<String, u32>>,
    results: Receiver<(Value, f64)>,
    inflight: bool,
    next: f64,
    value: Value,
    observed_at: f64,
}
impl StoragePump {
    fn new() -> Self {
        let (requests, work) = mpsc::sync_channel(1);
        let (finished, results) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(users) = work.recv() {
                let observed_at = runtime::now();
                let value = crate::host::storage(&users);
                if finished.send((value, observed_at)).is_err() {
                    break;
                }
            }
        });
        Self {
            requests,
            results,
            inflight: false,
            next: 0.0,
            value: Value::Null,
            observed_at: 0.0,
        }
    }
    fn poll(&mut self, users: BTreeMap<String, u32>, monotonic: f64) {
        if let Ok((value, stamp)) = self.results.try_recv() {
            self.value = value;
            self.observed_at = stamp;
            self.inflight = false;
            self.next = monotonic + 120.0;
        }
        if !self.inflight && monotonic >= self.next && self.requests.try_send(users).is_ok() {
            self.inflight = true;
        }
    }
}

#[derive(Debug)]
pub enum PayloadError {
    Missing,
    Untrusted(u32),
    Stale,
    Invalid(String),
}

pub fn read_payload(
    config: &Config,
    name: &str,
    kind: &str,
    now: f64,
) -> std::result::Result<(AgentPayload, f64), PayloadError> {
    if !runtime::valid_host(name) {
        return Err(PayloadError::Invalid("invalid node name".into()));
    }
    let path = config.agents.join(format!("{name}.json"));
    let (bytes, st) = runtime::read_regular(&path, config.max_payload, None)
        .map_err(|_| PayloadError::Missing)?;
    if !config.trusted.contains(&st.uid()) {
        return Err(PayloadError::Untrusted(st.uid()));
    }
    let stamp = st.mtime() as f64 + st.mtime_nsec() as f64 / 1e9;
    if !(-5.0..=config.max_age).contains(&(now - stamp)) {
        return Err(PayloadError::Stale);
    }
    let raw: Value =
        serde_json::from_slice(&bytes).map_err(|_| PayloadError::Invalid("invalid JSON".into()))?;
    let valid = (|| -> Option<()> {
        if raw["hostname"].as_str() != Some(name)
            || raw["node_kind"].as_str() != Some(kind)
            || raw["agent_version"].as_u64() != Some(crate::AGENT_PAYLOAD_VERSION as u64)
            || raw["agent_build"].as_str().is_none()
            || json_number(&raw["ts"]).is_none()
        {
            return None;
        }
        for key in ["total", "used", "avail"] {
            raw["mem"][key].as_str()?;
        }
        let gpus = raw["gpus"].as_array()?;
        if gpus.len() > 64
            || (kind == "gpu" && gpus.is_empty())
            || (kind == "cpu" && !gpus.is_empty())
            || !matches!(kind, "gpu" | "cpu")
        {
            return None;
        }
        let mut seen = BTreeSet::new();
        for gpu in gpus {
            let index = gpu["index"].as_str()?;
            if index.parse::<u16>().is_err() || !seen.insert(index) {
                return None;
            }
            for field in ["name", "mem_total"] {
                if gpu[field].as_str()?.len() > 4096 {
                    return None;
                }
            }
            for field in ["pids", "users"] {
                let values = gpu[field].as_array()?;
                if values.len() > 4096
                    || values.iter().any(|v| {
                        v.as_str()
                            .is_none_or(|s| s.len() > 256 || s.chars().any(char::is_control))
                    })
                {
                    return None;
                }
            }
            for field in ["clock_reasons", "recovery_action"] {
                if let Some(value) = gpu.get(field)
                    && (value.as_str()?.len() > 128
                        || value.as_str()?.chars().any(char::is_control))
                {
                    return None;
                }
            }
            if let Some(stamp) = gpu.get("health_observed_at")
                && !json_number(stamp).is_some_and(|n| n >= 0.0)
            {
                return None;
            }
        }
        Some(())
    })();
    if valid.is_none() {
        return Err(PayloadError::Invalid(
            "invalid payload identity, version, kind, or fields".into(),
        ));
    }
    let mut payload: AgentPayload = serde_json::from_value(raw)
        .map_err(|_| PayloadError::Invalid("invalid payload types".into()))?;
    payload.telemetry = node_telemetry::clean_telemetry(&payload.telemetry);
    Ok((payload, stamp))
}

pub struct Collector {
    pub config: Config,
    pub usage: usage::Store,
    observations: BTreeMap<String, Observation>,
    inventory: BTreeMap<String, Vec<Gpu>>,
    idle: Value,
    last_roster: Vec<Node>,
    state_cache: BTreeMap<PathBuf, Vec<u8>>,
}

impl Collector {
    pub fn new(config: Config) -> Self {
        let mut candidates = vec![config.state.clone()];
        if config.state == runtime::state_dir() {
            candidates.extend(runtime::state_candidates());
        }
        let inventory = usage::read_freshest("inventory.json", &candidates)
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let mut idle = usage::read_freshest("idle_state.json", &candidates)
            .unwrap_or_else(|| json!({"idle":{},"parked":{}}));
        if idle.get("idle").is_none() && idle.get("parked").is_none() {
            idle = json!({"idle":idle,"parked":{}});
        }
        for key in ["idle", "parked"] {
            if !idle[key].is_object() {
                idle[key] = json!({});
            }
        }
        Self {
            usage: usage::Store::load(config.state.join("usage.json"), &candidates),
            config,
            observations: BTreeMap::new(),
            inventory,
            idle,
            last_roster: Vec::new(),
            state_cache: BTreeMap::new(),
        }
    }

    pub fn fold(&mut self, mut sample: Sample, now: f64) -> Snapshot {
        let scheduler_age = (now - sample.status.observed_at).max(0.0);
        let scheduler_available = sample.error.is_empty()
            && sample.status.job_backend != "unavailable"
            && sample.status.observed_at > 0.0
            && scheduler_age
                <= runtime::env_seconds("SLURM_GPU_TUI_SCHEDULER_MAX_AGE_SEC", 30.0, 1.0);
        sample.status.age_sec = if sample.status.observed_at > 0.0 {
            scheduler_age
        } else {
            -1.0
        };
        if !scheduler_available {
            sample.records = Default::default();
            for job in &mut sample.jobs {
                job.uid = -1;
                job.detail.clear();
            }
            for job in &mut sample.pending {
                job.detail.clear();
            }
            sample.status.job_backend = "unavailable".into();
            if sample.error.is_empty() {
                sample.error = "scheduler snapshot unavailable or stale".into();
            }
        }
        if !sample.nodes.is_empty() {
            self.last_roster = sample.nodes.clone();
        } else if !self.last_roster.is_empty() {
            sample.nodes = self.last_roster.clone();
        }
        let mut nodes = sample.nodes;
        let mut rejected = BTreeMap::new();
        let mut owners: BTreeMap<_, _> = sample
            .jobs
            .iter()
            .map(|j| (j.jobid.clone(), j.user.clone()))
            .collect();
        owners.extend(sample.records.users.clone());
        let ignored = rules::ignored_users();
        for node in &mut nodes {
            let kind = if node.has_gpu { "gpu" } else { "cpu" };
            let failure = match read_payload(&self.config, &node.name, kind, now) {
                Ok((payload, stamp)) => {
                    self.observations
                        .insert(node.name.clone(), Observation { payload, stamp });
                    None
                }
                Err(PayloadError::Untrusted(uid)) => {
                    rejected.insert(node.name.clone(), uid);
                    Some("untrusted agent payload")
                }
                Err(PayloadError::Invalid(_)) => Some("invalid agent payload"),
                Err(PayloadError::Stale) => Some("stale agent payload"),
                Err(PayloadError::Missing) => Some("missing agent payload"),
            };
            node.stale = failure.is_some();
            node.error = failure.unwrap_or("").into();
            node.error_kind = if node.stale { "stale_cached" } else { "ok" }.into();
            node.source = if node.stale { "stale" } else { "agent" }.into();
            node.scheduler_age_sec = sample.status.age_sec;
            node.scheduler_available = scheduler_available;
            if let Some(obs) = self.observations.get(&node.name) {
                node.gpus = obs.payload.gpus.clone();
                node.observed_at = obs.stamp;
                node.data_age_sec = (now - obs.stamp).max(0.0);
                node.mem_used = obs.payload.mem.used.clone();
                node.mem_avail = obs.payload.mem.avail.clone();
                if number(&obs.payload.mem.total).is_some_and(|n| n > 0.0) {
                    node.mem_total = obs.payload.mem.total.clone();
                }
                node.cpu_power = obs.payload.power.get("cpu").cloned().unwrap_or_default();
                node.ram_power = obs.payload.power.get("ram").cloned().unwrap_or_default();
                node.sys_power = obs.payload.power.get("sys").cloned().unwrap_or_default();
                let telemetry = &obs.payload.telemetry;
                if !node.stale
                    && json_number(&telemetry["observed_at"])
                        .is_some_and(|stamp| rules::telemetry_fresh(stamp, now))
                {
                    node.telemetry_observed_at =
                        json_number(&telemetry["observed_at"]).unwrap_or(0.0);
                    node.cpu_util = telemetry["cpu_util"].as_str().unwrap_or("").into();
                    node.host_io = crate::host::KEYS
                        .iter()
                        .filter_map(|key| {
                            json_number(&telemetry["host_io"][key])
                                .map(|v| (key.to_string(), v.to_string()))
                        })
                        .collect();
                    node.pressure =
                        serde_json::from_value(telemetry["pressure"].clone()).unwrap_or_default();
                    node.jobs_truncated = telemetry["jobs_truncated"] == true;
                }
            } else {
                node.gpus = self
                    .inventory
                    .get(&node.name)
                    .cloned()
                    .unwrap_or_else(|| skeleton(&node.gres));
                node.observed_at = 0.0;
                node.data_age_sec = -1.0;
            }
            if !node.stale && !node.gpus.is_empty() {
                self.inventory.insert(
                    node.name.clone(),
                    node.gpus
                        .iter()
                        .map(|g| Gpu {
                            index: g.index.clone(),
                            minor: g.minor.clone(),
                            name: g.name.clone(),
                            mem_total: g.mem_total.clone(),
                            uuid: g.uuid.clone(),
                            pci_bus: g.pci_bus.clone(),
                            slot: g.slot.clone(),
                            serial: g.serial.clone(),
                            ..Default::default()
                        })
                        .collect(),
                );
            }
            for gpu in &mut node.gpus {
                for user in &mut gpu.users {
                    if let Ok(uid) = user.parse::<u32>()
                        && let Some(resolved) = runtime::user(uid)
                    {
                        *user = resolved.name;
                    }
                }
            }
            scheduler::reconcile(
                sample
                    .records
                    .alloc
                    .get(&node.name)
                    .unwrap_or(&BTreeMap::new()),
                &owners,
                &mut node.gpus,
                &ignored,
            );
            node.jobs = sample
                .jobs
                .iter()
                .filter(|j| {
                    scheduler::expand_nodes(&j.node).is_some_and(|nodes| nodes.contains(&node.name))
                })
                .cloned()
                .map(|mut job| {
                    let names = scheduler::expand_nodes(&job.node).unwrap_or_default();
                    let count = names.len().max(1) as u32;
                    let offset = names.iter().position(|n| n == &node.name).unwrap_or(0) as u32;
                    job.cpu_count =
                        job.cpu_count / count + u32::from(offset < job.cpu_count % count);
                    job.detail.clear();
                    job.script.clear();
                    job.log_out.clear();
                    job.log_err.clear();
                    job.log_status.clear();
                    job
                })
                .collect();
            for gpu in &mut node.gpus {
                track_waste(&mut self.idle, &node.name, gpu, now, node.stale);
            }
        }
        if scheduler_available {
            attach_job_samples(
                &mut sample.jobs,
                &mut nodes,
                &sample.records,
                &self.observations,
                now,
            );
        }
        self.usage.sample(&nodes, now);
        for job in &mut sample.jobs {
            if !self.config.share_details {
                job.detail.clear();
            }
        }
        for job in &mut sample.pending {
            if !self.config.share_details {
                job.detail.clear();
            }
        }
        Snapshot {
            ts: chrono::Local::now().to_rfc3339(),
            stale_nodes: nodes
                .iter()
                .filter(|n| n.stale)
                .map(|n| n.name.clone())
                .collect(),
            nodes,
            jobs: sample.jobs,
            pending: sample.pending,
            untrusted_payloads: rejected,
            scheduler: sample.status,
            errors: sample.error,
            ..Default::default()
        }
    }

    fn write_state(&mut self, name: &str, value: &Value) -> Result<()> {
        let path = self.config.state.join(name);
        let bytes = serde_json::to_vec(value)?;
        if self.state_cache.get(&path) == Some(&bytes)
            && let Ok((current, st)) =
                runtime::read_regular(&path, 64 * 1024 * 1024, Some(runtime::euid()))
            && current == bytes
            && st.mode() & 0o777 == 0o644
        {
            return Ok(());
        }
        runtime::atomic_write(&path, &bytes, 0o644, true)?;
        self.state_cache.insert(path, bytes);
        Ok(())
    }

    pub fn checkpoint(&mut self, monotonic: f64, force: bool) -> Result<()> {
        self.write_state("idle_state.json", &self.idle.clone())?;
        self.write_state("inventory.json", &serde_json::to_value(&self.inventory)?)?;
        self.usage.checkpoint(monotonic, force)?;
        Ok(())
    }
}

fn skeleton(gres: &str) -> Vec<Gpu> {
    let mut gpus = Vec::new();
    for part in gres.split(',') {
        let part = part.split('(').next().unwrap_or("");
        let fields: Vec<_> = part.split(':').collect();
        if fields.first() != Some(&"gpu") {
            continue;
        }
        let Some(count) = fields
            .last()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n <= 64usize.saturating_sub(gpus.len()))
        else {
            continue;
        };
        for _ in 0..count {
            gpus.push(Gpu {
                index: gpus.len().to_string(),
                name: if fields.len() > 2 {
                    fields[1].into()
                } else {
                    String::new()
                },
                ..Default::default()
            });
        }
    }
    gpus
}

fn track_waste(idle: &mut Value, node: &str, gpu: &mut Gpu, now: f64, stale: bool) {
    let key = format!("{node}:{}", gpu.index);
    if stale {
        gpu.idle_sec = if idle["idle"][&key]["jobid"].as_str() == Some(&gpu.alloc_jobid) {
            json_number(&idle["idle"][&key]["since"])
                .map(|t| (now - t).max(0.0) as u64)
                .unwrap_or(0)
        } else {
            0
        };
        gpu.parked_sec = json_number(&idle["parked"][&key]["since"])
            .map(|t| (now - t).max(0.0) as u64)
            .unwrap_or(0);
        return;
    }
    if !gpu.alloc_jobid.is_empty() && gpu.users.is_empty() {
        if idle["idle"][&key]["jobid"].as_str() != Some(&gpu.alloc_jobid) {
            idle["idle"][&key] = json!({"jobid":gpu.alloc_jobid,"since":now});
        }
        gpu.idle_sec = json_number(&idle["idle"][&key]["since"])
            .map(|t| (now - t).max(0.0) as u64)
            .unwrap_or(0);
    } else {
        if let Some(map) = idle["idle"].as_object_mut() {
            map.remove(&key);
        }
        gpu.idle_sec = 0;
    }
    let owner = if !gpu.alloc_jobid.is_empty() {
        gpu.alloc_jobid.clone()
    } else {
        gpu.users.join(",")
    };
    if !owner.is_empty()
        && number(&gpu.util).is_some_and(|u| (0.0..=5.0).contains(&u))
        && number(&gpu.mem_used)
            .zip(number(&gpu.mem_total))
            .is_some_and(|(used, total)| total > 0.0 && used / total >= 0.3)
    {
        if idle["parked"][&key]["owner"].as_str() != Some(&owner) {
            idle["parked"][&key] = json!({"owner":owner,"since":now});
        }
        gpu.parked_sec = json_number(&idle["parked"][&key]["since"])
            .map(|t| (now - t).max(0.0) as u64)
            .unwrap_or(0);
    } else {
        if let Some(map) = idle["parked"].as_object_mut() {
            map.remove(&key);
        }
        gpu.parked_sec = 0;
    }
}

fn attach_job_samples(
    jobs: &mut [Job],
    nodes: &mut [Node],
    records: &scheduler::JobRecords,
    observations: &BTreeMap<String, Observation>,
    now: f64,
) {
    for job in jobs {
        if job.uid < 0 {
            continue;
        }
        let expected = scheduler::expand_nodes(&job.node).unwrap_or_default();
        let raw_id = scheduler::detail_field(
            records
                .details
                .get(&job.jobid)
                .map(String::as_str)
                .unwrap_or(""),
            "JobId",
        );
        for node in nodes
            .iter_mut()
            .filter(|n| expected.contains(&n.name) && !n.stale && n.error.is_empty())
        {
            let Some(obs) = observations.get(&node.name) else {
                continue;
            };
            let telemetry = &obs.payload.telemetry;
            let mut value = node_telemetry::clean_sample(
                telemetry["jobs"]
                    .get(&job.jobid)
                    .or_else(|| telemetry["jobs"].get(&raw_id))
                    .unwrap_or(&Value::Null),
            );
            if !json_number(&value["observed_at"]).is_some_and(|s| rules::telemetry_fresh(s, now)) {
                value = json!({});
            }
            let vram: Vec<_> = node
                .gpus
                .iter()
                .flat_map(|g| {
                    g.pid_jobid
                        .iter()
                        .filter(|(_, j)| *j == &job.jobid || !raw_id.is_empty() && *j == &raw_id)
                        .filter_map(|(pid, _)| {
                            g.pid_mem
                                .get(pid)
                                .and_then(|v| number(v))
                                .filter(|v| *v >= 0.0)
                        })
                })
                .collect();
            if !vram.is_empty() {
                value["vram_mib"] = json!(vram.iter().sum::<f64>());
                if value["observed_at"].is_null() {
                    value["observed_at"] = json!(obs.stamp);
                }
            }
            if value.as_object().is_some_and(|m| m.len() > 1) {
                job.telemetry.insert(node.name.clone(), value.clone());
                if let Some(local) = node.jobs.iter_mut().find(|j| j.jobid == job.jobid) {
                    local.telemetry.insert(node.name.clone(), value);
                }
            }
        }
    }
}

pub struct SchedulerPump {
    receiver: Receiver<Sample>,
    pub latest: Sample,
    stopped: Arc<AtomicBool>,
}
impl SchedulerPump {
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        std::thread::spawn(move || {
            let mut scheduler = scheduler::Scheduler::default();
            while !stop.load(Ordering::Relaxed) {
                let sample = scheduler.sample();
                if sender.send(sample).is_err() {
                    break;
                }
                let start = Instant::now();
                let interval = Duration::from_secs_f64(runtime::env_seconds(
                    "SLURM_GPU_TUI_SCHEDULER_SEC",
                    runtime::env_seconds("SLURM_GPU_TUI_COLLECTOR_SEC", 3.0, 0.1),
                    0.1,
                ));
                while start.elapsed() < interval && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            receiver,
            latest: Sample::default(),
            stopped,
        }
    }
    pub fn read(&mut self) -> Sample {
        while let Ok(sample) = self.receiver.try_recv() {
            if sample.error.is_empty() || self.latest.nodes.is_empty() {
                self.latest = sample;
            } else {
                self.latest.error = sample.error;
                self.latest.status.error = self.latest.error.clone();
            }
        }
        self.latest.clone()
    }
}
impl Drop for SchedulerPump {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub fn prepare(config: &Config) -> Result<()> {
    runtime::secure_dir(&config.data, 0o755)?;
    runtime::secure_dir(&config.state, 0o755)?;
    Ok(())
}

#[derive(clap::Parser)]
#[command(version=crate::VERSION,about="Push-only collector: reads node publications and local Slurm state")]
pub struct Args {
    #[arg(long)]
    pub daemon: bool,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub stop: bool,
    #[arg(long)]
    pub status: bool,
}

fn daemon_pid(config: &Config) -> Result<u32> {
    let (bytes, st) = runtime::read_regular(
        &config.data.join("collector.pid"),
        64,
        Some(runtime::euid()),
    )?;
    if st.mode() & 0o022 != 0 {
        return Err("unsafe PID file".into());
    }
    let pid = std::str::from_utf8(&bytes)?.trim().parse::<u32>()?;
    if pid <= 1 {
        return Err("invalid collector PID".into());
    }
    let path = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::metadata(&path)?.uid() != runtime::euid() {
        return Err("collector UID does not match".into());
    }
    let command = std::fs::read_link(path.join("exe"))?;
    if command.file_name().is_none_or(|n| n != "sgpu-collector") {
        use std::os::unix::ffi::OsStrExt;
        let (args, _) = runtime::read_regular(&path.join("cmdline"), 65536, None)?;
        if !args.split(|b| *b == 0).any(|arg| {
            arg == b"sgpu.collector"
                || std::path::Path::new(std::ffi::OsStr::from_bytes(arg))
                    .file_name()
                    .is_some_and(|n| n == "sgpu-collector")
        }) {
            return Err("PID does not identify the collector".into());
        }
    }
    Ok(pid)
}

struct Backfill {
    receiver: Option<Receiver<Result<String>>>,
    last_attempt: f64,
    failures: u32,
}
impl Backfill {
    fn poll(&mut self, store: &mut usage::Store, now: f64) {
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(result) => {
                    self.receiver = None;
                    match result.and_then(|raw| store.backfill(&raw, now)) {
                        Ok(()) => self.failures = 0,
                        Err(error) => {
                            self.failures += 1;
                            eprintln!(
                                "[collector] accounting backfill failed ({}/3): {error}",
                                self.failures
                            );
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.receiver = None;
                    self.failures += 1;
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        let interval = runtime::env_seconds("SLURM_GPU_TUI_SACCT_SEC", 3600.0, 0.0);
        if interval <= 0.0
            || self.receiver.is_some()
            || self.failures >= 3
            || now - json_number(&store.raw["sacct_ts"]).unwrap_or(0.0) < interval
            || now - self.last_attempt < interval.min(900.0)
        {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.receiver = Some(receiver);
        self.last_attempt = now;
        std::thread::spawn(move || {
            let start =
                usage::window(
                    runtime::env_seconds("SLURM_GPU_TUI_USAGE_KEEP_DAYS", 30.0, 1.0) as u64,
                );
            let result = crate::process::run(
                crate::process::Program::Sacct,
                &[
                    "-a",
                    "-X",
                    "--noheader",
                    "--parsable2",
                    "--format=User,AllocTRES,Start,End",
                    "-S",
                    &format!("{start}T00:00:00"),
                ],
                Duration::from_secs(60),
            );
            let _ = sender.send(result);
        });
    }
}

pub fn run(args: Args) -> Result<()> {
    let config = Config::default();
    if args.stop || args.status {
        match daemon_pid(&config) {
            Ok(pid) => {
                if args.stop {
                    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let _lock = runtime::Lock::acquire(
                        &config.data.join("collector.lock"),
                        Duration::from_secs(60),
                    )?;
                    println!("collector stop requested");
                } else {
                    println!("collector running (pid={pid})");
                }
                return Ok(());
            }
            Err(error)
                if args.stop
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    prepare(&config)?;
    if args.daemon && !runtime::daemonize(&config.data.join("collector.log"))? {
        return Ok(());
    }
    let _lock =
        runtime::Lock::acquire(&config.data.join("collector.lock"), Duration::from_secs(10))?;
    runtime::atomic_write(
        &config.data.join("collector.pid"),
        std::process::id().to_string().as_bytes(),
        0o644,
        false,
    )?;
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopped.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
    let mut collector = Collector::new(config.clone());
    let mut controller = crate::host::Sampler::default();
    let mut storage = StoragePump::new();
    let mut mirror = crate::logs::Mirror::new(&config.state, config.share_logs)?;
    let mut scripts = crate::batch::Fetcher::new(config.share_scripts);
    let mut notifier = crate::notify::Notifier::new(&config.state)?;
    let mut pump = (!args.once).then(SchedulerPump::start);
    let mut backfill = Backfill {
        receiver: None,
        last_attempt: 0.0,
        failures: 0,
    };
    let epoch = Instant::now();
    let mut last_cycle = None;
    let mut diagnostics = json!({});
    let mut last_metrics = None;
    let metrics = std::env::var_os("SLURM_GPU_TUI_METRICS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.data.join("metrics.prom"));
    if let Some(parent) = metrics.parent()
        && parent != config.data
    {
        runtime::secure_dir(parent, 0o755)?;
    }
    while !stopped.load(Ordering::Relaxed) {
        let cycle = Instant::now();
        let mut phases = BTreeMap::new();
        let sample = if let Some(pump) = &mut pump {
            pump.read()
        } else {
            scheduler::Scheduler::default().sample()
        };
        let seeds: BTreeMap<_, _> = sample
            .jobs
            .iter()
            .filter(|j| {
                j.uid >= 0
                    && sample.error.is_empty()
                    && runtime::now() - sample.status.observed_at
                        <= runtime::env_seconds("SLURM_GPU_TUI_SCHEDULER_MAX_AGE_SEC", 30.0, 1.0)
            })
            .filter_map(|j| {
                sample.records.logs.get(&j.jobid).map(|spec| {
                    (
                        j.jobid.clone(),
                        crate::logs::Seed {
                            uid: j.uid as u32,
                            spec: spec.clone(),
                        },
                    )
                })
            })
            .collect();
        let mut snapshot = collector.fold(sample, runtime::now());
        let monotonic = epoch.elapsed().as_secs_f64();
        snapshot.controller = controller.sample(monotonic, runtime::now());
        snapshot.controller["hostname"] =
            json!(runtime::hostname().unwrap_or_else(|_| "controller".into()));
        let users = snapshot
            .jobs
            .iter()
            .filter(|j| j.uid >= 0 && j.uid <= u32::MAX as i64)
            .map(|j| (j.user.clone(), j.uid as u32))
            .collect();
        storage.poll(users, monotonic);
        snapshot.controller["filesystems"] = storage.value["filesystems"].clone();
        snapshot.controller["storage_truncated"] = storage.value["truncated"].clone();
        snapshot.controller["storage_observed_at"] = json!(storage.observed_at);
        scripts.update(&mut snapshot.jobs, snapshot.errors.is_empty());
        phases.insert("collect", cycle.elapsed().as_secs_f64());
        let start = Instant::now();
        if let Err(error) =
            mirror.process(&mut snapshot.jobs, &seeds, epoch.elapsed().as_secs_f64())
        {
            eprintln!("[collector] log publication failed: {error}");
        }
        phases.insert("logs", start.elapsed().as_secs_f64());
        let start = Instant::now();
        if !args.once {
            backfill.poll(&mut collector.usage, runtime::now());
        }
        if let Err(error) = collector.checkpoint(epoch.elapsed().as_secs_f64(), args.once) {
            eprintln!("[collector] state checkpoint failed: {error}");
        }
        phases.insert("usage", start.elapsed().as_secs_f64());
        snapshot.collector = diagnostics.clone();
        let start = Instant::now();
        runtime::atomic_write(
            &config.data.join("data.json"),
            &serde_json::to_vec(&snapshot)?,
            0o644,
            false,
        )?;
        phases.insert("write", start.elapsed().as_secs_f64());
        let start = Instant::now();
        let metrics_sec = runtime::env_seconds("SLURM_GPU_TUI_METRICS_SEC", 15.0, 0.1);
        if last_metrics.is_none_or(|last: Instant| last.elapsed().as_secs_f64() >= metrics_sec) {
            let mut text = crate::metrics::format(&snapshot, runtime::now());
            text.push_str(&crate::metrics::master_host(
                std::path::Path::new("/proc"),
                std::path::Path::new("/sys"),
            ));
            match runtime::atomic_write(&metrics, text.as_bytes(), 0o644, false) {
                Ok(()) => last_metrics = Some(Instant::now()),
                Err(error) => eprintln!("[collector] metric publication failed: {error}"),
            }
        }
        phases.insert("metrics", start.elapsed().as_secs_f64());
        let start = Instant::now();
        if let Err(error) = notifier.process(&snapshot, &seeds, runtime::now()) {
            eprintln!("[collector] notification state failed: {error}");
        }
        phases.insert("notify", start.elapsed().as_secs_f64());
        let interval = Duration::from_secs_f64(runtime::env_seconds(
            "SLURM_GPU_TUI_COLLECTOR_SEC",
            3.0,
            0.1,
        ));
        diagnostics = json!({"phase_seconds":phases,"cycle_seconds":cycle.elapsed().as_secs_f64(),"interval_seconds":last_cycle.map(|last:Instant|cycle.duration_since(last).as_secs_f64()).unwrap_or(0.0),"target_interval_seconds":interval.as_secs_f64(),"log_inflight":mirror.inflight(),"script_inflight":scripts.inflight(),"notify_backlog":notifier.backlog(),"rpc":crate::process::stats()});
        last_cycle = Some(cycle);
        if args.once {
            println!("{}", serde_json::to_string(&snapshot)?);
            break;
        }
        while cycle.elapsed() < interval && !stopped.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    collector.checkpoint(epoch.elapsed().as_secs_f64(), true)?;
    match std::fs::remove_file(config.data.join("collector.pid")) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Memory, SchedulerStatus};
    use std::os::unix::fs::symlink;
    use std::{fs, path::Path};

    fn config(root: &Path) -> Config {
        Config {
            data: root.join("data"),
            state: root.join("state"),
            agents: root.join("agents"),
            max_age: 45.0,
            max_payload: 1048576,
            trusted: BTreeSet::from([runtime::euid()]),
            share_details: false,
            share_scripts: false,
            share_logs: false,
        }
    }
    fn payload() -> AgentPayload {
        AgentPayload {
            agent_version: 9,
            release: "2".into(),
            package_build: "2".into(),
            agent_build: "2".into(),
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
                total: "100000".into(),
                used: "1000".into(),
                avail: "99000".into(),
            },
            telemetry: json!({}),
            power: BTreeMap::new(),
        }
    }
    fn sample() -> Sample {
        Sample {
            nodes: vec![Node {
                name: "gpu1".into(),
                gres: "gpu:h100:1".into(),
                ..Default::default()
            }],
            status: SchedulerStatus {
                job_backend: "structured-json".into(),
                observed_at: runtime::now(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn push_only_missing_payload_retains_roster_and_never_invents_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        let mut collector = Collector::new(cfg);
        let snapshot = collector.fold(sample(), runtime::now());
        assert_eq!(snapshot.nodes.len(), 1);
        assert!(snapshot.nodes[0].stale);
        assert_eq!(snapshot.nodes[0].gpus.len(), 1);
        assert_eq!(
            rules::classes(&snapshot.nodes[0], runtime::now()),
            [rules::Class::Unknown]
        );
    }

    #[test]
    fn trust_version_shape_and_symlink_checks_apply_to_the_open_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(tmp.path());
        fs::create_dir(&cfg.agents).unwrap();
        let path = cfg.agents.join("gpu1.json");
        fs::write(&path, serde_json::to_vec(&payload()).unwrap()).unwrap();
        assert!(read_payload(&cfg, "gpu1", "gpu", runtime::now()).is_ok());
        cfg.trusted.clear();
        assert!(matches!(
            read_payload(&cfg, "gpu1", "gpu", runtime::now()),
            Err(PayloadError::Untrusted(_))
        ));
        cfg.trusted.insert(runtime::euid());
        assert!(read_payload(&cfg, "gpu1", "cpu", runtime::now()).is_err());
        assert!(matches!(
            read_payload(&cfg, "gpu1", "gpu", runtime::now() + 60.0),
            Err(PayloadError::Stale)
        ));
        let target = cfg.agents.join("target.json");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_payload(&cfg, "gpu1", "gpu", runtime::now()).is_err());
    }

    #[test]
    fn stale_scheduler_invalidates_privileged_identity_and_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        fs::create_dir(&cfg.agents).unwrap();
        fs::write(
            cfg.agents.join("gpu1.json"),
            serde_json::to_vec(&payload()).unwrap(),
        )
        .unwrap();
        let mut sample = sample();
        sample.jobs.push(Job {
            jobid: "42".into(),
            uid: 0,
            detail: "private".into(),
            node: "gpu1".into(),
            ..Default::default()
        });
        sample
            .records
            .alloc
            .insert("gpu1".into(), BTreeMap::from([("0".into(), "42".into())]));
        sample.error = "scheduler failed".into();
        let result = Collector::new(cfg).fold(sample, runtime::now());
        assert_eq!(result.jobs[0].uid, -1);
        assert!(result.jobs[0].detail.is_empty());
        assert!(result.nodes[0].gpus[0].alloc_jobid.is_empty());
        assert!(!result.nodes[0].scheduler_available);
    }
}
