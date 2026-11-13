use crate::{
    Result,
    logs::{self, Seed},
    model::{Snapshot, number},
    process::Program,
    rules, runtime,
    scheduler::{self, Runner, SystemRunner},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, SystemTime},
};

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bot_token: String,
    pub channel: String,
    pub sender_name: String,
    pub lang: String,
    pub node_health: bool,
    pub down_grace_sec: f64,
    pub collect_alert: bool,
    pub collect_grace_sec: f64,
    pub waste_alert_hours: f64,
    pub rogue_alert: bool,
    pub rogue_grace_sec: f64,
    pub temp_alert_c: f64,
    pub ecc_alert: bool,
    pub job_done_users: Vec<String>,
    pub job_fail_users: Vec<String>,
    pub dm_users: BTreeMap<String, String>,
    pub pending_alert_hours: f64,
    pub free_gpus_min: usize,
    pub mem_fair_factor: f64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            channel: String::new(),
            sender_name: "AI-master".into(),
            lang: "en".into(),
            node_health: true,
            down_grace_sec: 180.0,
            collect_alert: true,
            collect_grace_sec: 600.0,
            waste_alert_hours: 0.0,
            rogue_alert: false,
            rogue_grace_sec: 300.0,
            temp_alert_c: 0.0,
            ecc_alert: true,
            job_done_users: Vec::new(),
            job_fail_users: Vec::new(),
            dm_users: BTreeMap::new(),
            pending_alert_hours: 0.0,
            free_gpus_min: 0,
            mem_fair_factor: 0.0,
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Self {
        use std::os::unix::fs::MetadataExt;
        let mut cfg = runtime::read_regular(path, 65536, None)
            .ok()
            .filter(|(_, st)| {
                (st.uid() == 0 || st.uid() == runtime::euid()) && st.mode() & 0o022 == 0
            })
            .and_then(|(bytes, _)| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default();
        if cfg.bot_token.is_empty() {
            cfg.bot_token = std::env::var("SLURM_GPU_TUI_SLACK_BOT_TOKEN").unwrap_or_default();
        }
        if cfg.lang != "ko" {
            cfg.lang = "en".into();
        }
        cfg
    }
    pub fn enabled(&self) -> bool {
        !self.bot_token.trim().is_empty() && !self.channel.trim().is_empty()
    }
    fn language(&self, en: &str, ko: &str) -> String {
        if self.lang == "ko" { ko } else { en }.into()
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SLURM_GPU_TUI_SLACK_CONFIG") {
        return path.into();
    }
    let system = PathBuf::from("/etc/sgpu/slack.json");
    let home = runtime::home();
    let mut paths = vec![
        home.join(".sgpu/slack.json"),
        home.join(".sgpu/webhook.json"),
    ];
    if runtime::euid() == 0 {
        paths.insert(0, system);
    } else {
        paths.push(system);
    }
    paths
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| paths.remove(0))
}

fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn valid_threshold(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

pub trait Transport: Send + Sync {
    fn post(&self, cfg: &Config, target: &str, text: &str, thread: Option<&str>) -> Option<String>;
}
struct Slack {
    client: reqwest::blocking::Client,
}
impl Slack {
    fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("sgpu/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }
}
impl Transport for Slack {
    fn post(&self, cfg: &Config, target: &str, text: &str, thread: Option<&str>) -> Option<String> {
        let mut payload =
            json!({"channel":target,"text":text,"unfurl_links":false,"unfurl_media":false});
        if let Some(thread) = thread {
            payload["thread_ts"] = json!(thread);
        }
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << attempt));
            }
            let response = self
                .client
                .post("https://slack.com/api/chat.postMessage")
                .bearer_auth(&cfg.bot_token)
                .json(&payload)
                .send();
            let Ok(response) = response else {
                continue;
            };
            let status = response.status();
            if status.as_u16() == 429 {
                let seconds = response
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(60);
                std::thread::sleep(Duration::from_secs(seconds));
                continue;
            }
            if status.is_client_error() || status.is_redirection() {
                return None;
            }
            if !status.is_success() {
                continue;
            }
            let mut bytes = Vec::new();
            if response.take(65537).read_to_end(&mut bytes).is_err() || bytes.len() > 65536 {
                return None;
            }
            let reply: Value = serde_json::from_slice(&bytes).ok()?;
            if reply["ok"] != true {
                return None;
            }
            return reply["ts"].as_str().map(str::to_string);
        }
        None
    }
}

pub trait TailReader: Send + Sync {
    fn tail(&self, seed: &Seed) -> Option<String>;
}
struct OwnerTail;
impl TailReader for OwnerTail {
    fn tail(&self, seed: &Seed) -> Option<String> {
        let path = if seed.spec.err.as_os_str().is_empty() {
            &seed.spec.out
        } else {
            &seed.spec.err
        };
        if path.as_os_str().is_empty() {
            return None;
        }
        let bytes = logs::tail_as_owner(path, seed.uid, 8192).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<_> = text.lines().rev().take(15).collect();
        lines.reverse();
        Some(escape(&lines.join("\n")).replace("```", "'''"))
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Tracked {
    jobname: String,
    user: String,
    elapsed: String,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persistent {
    down: BTreeMap<String, f64>,
    blind: BTreeMap<String, f64>,
    jobs: BTreeMap<String, Tracked>,
    pend_seen: BTreeMap<String, f64>,
    last_sent: BTreeMap<String, f64>,
    free_was_below: bool,
    thread_day: String,
    thread_ts: String,
}
impl Default for Persistent {
    fn default() -> Self {
        Self {
            down: BTreeMap::new(),
            blind: BTreeMap::new(),
            jobs: BTreeMap::new(),
            pend_seen: BTreeMap::new(),
            last_sent: BTreeMap::new(),
            free_was_below: true,
            thread_day: String::new(),
            thread_ts: String::new(),
        }
    }
}
impl Persistent {
    fn load(path: &Path) -> Self {
        let raw: Value = runtime::read_regular(path, 16 * 1024 * 1024, None)
            .ok()
            .and_then(|(bytes, _)| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let mut state = Self::default();
        for (key, target) in [
            ("down", &mut state.down),
            ("blind", &mut state.blind),
            ("pend_seen", &mut state.pend_seen),
            ("last_sent", &mut state.last_sent),
        ] {
            if let Some(map) = raw[key].as_object() {
                *target = map
                    .iter()
                    .filter_map(|(k, v)| {
                        let value = v
                            .as_f64()
                            .or_else(|| (v == true).then_some(runtime::now()))?;
                        (value.is_finite()
                            && value >= 0.0
                            && (!matches!(key, "down" | "blind") || value > 0.0))
                            .then_some((k.clone(), value))
                    })
                    .collect();
            }
        }
        state.jobs = serde_json::from_value(raw["jobs"].clone()).unwrap_or_default();
        state.free_was_below = raw["free_was_below"].as_bool().unwrap_or(true);
        state.thread_day = raw["thread_day"].as_str().unwrap_or("").into();
        state.thread_ts = raw["thread_ts"].as_str().unwrap_or("").into();
        state
    }
}

#[derive(Default)]
struct ThreadState {
    day: String,
    ts: String,
    next: f64,
    channel: String,
}
struct Delivery {
    cfg: Config,
    text: String,
    target: Option<String>,
    reservation: Option<(String, f64)>,
    parent: bool,
}
struct DeliveryQueue {
    sender: SyncSender<Delivery>,
    rollbacks: Receiver<(String, f64)>,
    thread: Arc<Mutex<ThreadState>>,
    backlog: Arc<AtomicUsize>,
    parent_pending: Arc<AtomicBool>,
}
impl DeliveryQueue {
    fn new(transport: Arc<dyn Transport>, day: String, ts: String) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Delivery>(200);
        let (rollback, rollbacks) = mpsc::sync_channel(200);
        let thread = Arc::new(Mutex::new(ThreadState {
            day,
            ts,
            ..Default::default()
        }));
        let shared = thread.clone();
        let backlog = Arc::new(AtomicUsize::new(0));
        let count = backlog.clone();
        let parent_pending = Arc::new(AtomicBool::new(false));
        let pending = parent_pending.clone();
        std::thread::spawn(move || {
            while let Ok(task) = receiver.recv() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    let now = runtime::now();
                    let mut parent = String::new();
                    if task.target.is_none() {
                        let needed = {
                            let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
                            if !st.channel.is_empty() && st.channel != task.cfg.channel {
                                st.day.clear();
                                st.ts.clear();
                                st.next = 0.0;
                            }
                            st.channel = task.cfg.channel.clone();
                            if st.day == today && !st.ts.is_empty() {
                                parent = st.ts.clone();
                                false
                            } else {
                                now >= st.next
                            }
                        };
                        if needed {
                            let title = format!(
                                "📅 {} — {} · {}",
                                task.cfg.language("GPU cluster alerts", "GPU 클러스터 알림"),
                                today,
                                escape(&task.cfg.sender_name)
                            );
                            let result = transport.post(&task.cfg, &task.cfg.channel, &title, None);
                            let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(ts) = result {
                                st.day = today;
                                st.ts = ts.clone();
                                parent = ts;
                            } else {
                                st.next = now + 60.0;
                            }
                        }
                    }
                    if task.parent {
                        return true;
                    }
                    if task.target.is_none() && parent.is_empty() {
                        return false;
                    }
                    let target = task.target.as_deref().unwrap_or(&task.cfg.channel);
                    let text = format!("[{}] {}", escape(&task.cfg.sender_name), task.text);
                    transport
                        .post(
                            &task.cfg,
                            target,
                            &text,
                            (!parent.is_empty() && task.target.is_none())
                                .then_some(parent.as_str()),
                        )
                        .is_some()
                }))
                .unwrap_or(false);
                if !result && let Some(reservation) = task.reservation {
                    let _ = rollback.try_send(reservation);
                }
                if task.parent {
                    pending.store(false, Ordering::Relaxed);
                }
                count.fetch_sub(1, Ordering::Relaxed);
            }
        });
        Self {
            sender,
            rollbacks,
            thread,
            backlog,
            parent_pending,
        }
    }
    fn enqueue(&self, task: Delivery) -> bool {
        self.backlog.fetch_add(1, Ordering::Relaxed);
        if self.sender.try_send(task).is_err() {
            self.backlog.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

struct Outcome {
    cfg: Config,
    jobs: Vec<(String, Tracked)>,
    lookup: Vec<String>,
    sources: BTreeMap<String, Seed>,
}
pub struct Notifier {
    pub config: Config,
    path: PathBuf,
    mtime: Option<SystemTime>,
    state_path: PathBuf,
    state: Persistent,
    delivery: DeliveryQueue,
    outcomes: SyncSender<Outcome>,
    outcome_backlog: Arc<AtomicUsize>,
    sources: BTreeMap<String, Seed>,
    pending_down: BTreeMap<String, f64>,
    pending_blind: BTreeMap<String, f64>,
    pending_rogue: BTreeMap<String, f64>,
    last_bytes: Vec<u8>,
}

fn outcome_states(runner: &dyn Runner, ids: &[String]) -> BTreeMap<String, String> {
    let wanted: BTreeSet<_> = ids
        .iter()
        .filter(|id| scheduler::valid_job_id(id) && id.len() <= 256)
        .cloned()
        .collect();
    if wanted.is_empty() {
        return BTreeMap::new();
    }
    let joined = wanted.iter().cloned().collect::<Vec<_>>().join(",");
    let Ok(raw) = runner.run(
        Program::Sacct,
        &[
            "-j",
            &joined,
            "-X",
            "-n",
            "-o",
            "JobIDRaw,State",
            "--parsable2",
        ],
        Duration::from_secs(10),
    ) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if fields.len() < 2 || !wanted.contains(fields[0]) {
            continue;
        }
        if let Some(state) = fields[1].split_whitespace().next() {
            out.entry(fields[0].into()).or_insert_with(|| state.into());
        }
    }
    out
}

impl Notifier {
    pub fn new(state_dir: &Path) -> Result<Self> {
        Self::with_backends(
            state_dir,
            config_path(),
            Arc::new(Slack::new()?),
            Arc::new(SystemRunner),
            Arc::new(OwnerTail),
        )
    }
    fn with_backends(
        state_dir: &Path,
        path: PathBuf,
        transport: Arc<dyn Transport>,
        runner: Arc<dyn Runner>,
        tail: Arc<dyn TailReader>,
    ) -> Result<Self> {
        let config = Config::load(&path);
        let state_path = state_dir.join("notify_state.json");
        let state = Persistent::load(&state_path);
        let delivery =
            DeliveryQueue::new(transport, state.thread_day.clone(), state.thread_ts.clone());
        let (sender, receiver) = mpsc::sync_channel::<Outcome>(8);
        let outcome_backlog = Arc::new(AtomicUsize::new(0));
        let count = outcome_backlog.clone();
        let messages = delivery.sender.clone();
        let message_count = delivery.backlog.clone();
        std::thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let states = outcome_states(runner.as_ref(), &event.lookup);
                    for (id, job) in &event.jobs {
                        let state = states.get(id).map(String::as_str).unwrap_or("");
                        let failed = ["FAILED", "OUT_OF_MEMORY", "TIMEOUT", "NODE_FAIL"]
                            .iter()
                            .any(|prefix| state.starts_with(prefix));
                        if !failed && !event.cfg.job_done_users.contains(&job.user) {
                            continue;
                        }
                        let text = format!(
                            "{} #{id} {} · {} · {} · {state}",
                            if failed {
                                event.cfg.language("Job failed", "작업 실패")
                            } else {
                                event.cfg.language("Job finished", "작업 종료")
                            },
                            escape(&job.jobname),
                            escape(&job.user),
                            escape(&job.elapsed)
                        );
                        let enqueue = |text: String, target: Option<String>| {
                            message_count.fetch_add(1, Ordering::Relaxed);
                            if messages
                                .try_send(Delivery {
                                    cfg: event.cfg.clone(),
                                    text,
                                    target,
                                    reservation: None,
                                    parent: false,
                                })
                                .is_err()
                            {
                                message_count.fetch_sub(1, Ordering::Relaxed);
                            }
                        };
                        enqueue(text.clone(), None);
                        if let Some(member) = event.cfg.dm_users.get(&job.user) {
                            let mut dm = text;
                            if failed
                                && let Some(source) = event.sources.get(id)
                                && let Some(tail) = tail.tail(source)
                                && !tail.is_empty()
                            {
                                dm.push_str(&format!("\n```{tail}```"));
                            }
                            enqueue(dm, Some(member.clone()));
                        }
                    }
                }));
                count.fetch_sub(1, Ordering::Relaxed);
            }
        });
        Ok(Self {
            mtime: std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok()),
            config,
            path,
            state_path,
            state,
            delivery,
            outcomes: sender,
            outcome_backlog,
            sources: BTreeMap::new(),
            pending_down: BTreeMap::new(),
            pending_blind: BTreeMap::new(),
            pending_rogue: BTreeMap::new(),
            last_bytes: Vec::new(),
        })
    }
    pub fn backlog(&self) -> usize {
        self.delivery.backlog.load(Ordering::Relaxed) + self.outcome_backlog.load(Ordering::Relaxed)
    }
    fn alert(&mut self, key: String, text: String, user: Option<&str>, nag: bool, now: f64) {
        let interval = if nag {
            runtime::env_seconds("SLURM_GPU_TUI_SLACK_NAG_SEC", 21600.0, 0.0)
        } else {
            runtime::env_seconds("SLURM_GPU_TUI_SLACK_DEBOUNCE_SEC", 1800.0, 0.0)
        };
        if self
            .state
            .last_sent
            .get(&key)
            .is_some_and(|last| now - last < interval)
        {
            return;
        }
        self.state.last_sent.insert(key.clone(), now);
        if !self.delivery.enqueue(Delivery {
            cfg: self.config.clone(),
            text: text.clone(),
            target: None,
            reservation: Some((key.clone(), now)),
            parent: false,
        }) {
            self.undo_reservation(&key, now);
        }
        if let Some(member) = user.and_then(|u| self.config.dm_users.get(u)) {
            self.delivery.enqueue(Delivery {
                cfg: self.config.clone(),
                text,
                target: Some(member.clone()),
                reservation: None,
                parent: false,
            });
        }
    }
    fn undo_reservation(&mut self, key: &str, stamp: f64) {
        if !self
            .state
            .last_sent
            .get(key)
            .is_some_and(|last| *last <= stamp)
        {
            return;
        }
        self.state.last_sent.remove(key);
        if let Some(node) = key.strip_prefix("down:") {
            self.state.down.remove(node);
        }
        if let Some(node) = key.strip_prefix("blind:") {
            self.state.blind.remove(node);
        }
        if let Some(node) = key.strip_prefix("up:") {
            self.state.down.insert(node.into(), stamp);
        }
        if let Some(node) = key.strip_prefix("sighted:") {
            self.state.blind.insert(node.into(), stamp);
        }
    }
    fn save(&mut self, now: f64) -> Result<()> {
        let thread = self
            .delivery
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.state.thread_day = thread.day.clone();
        self.state.thread_ts = thread.ts.clone();
        drop(thread);
        let retention = 2.0
            * runtime::env_seconds("SLURM_GPU_TUI_SLACK_DEBOUNCE_SEC", 1800.0, 0.0).max(
                runtime::env_seconds("SLURM_GPU_TUI_SLACK_NAG_SEC", 21600.0, 0.0),
            );
        self.state
            .last_sent
            .retain(|_, stamp| now - *stamp < retention);
        let bytes = serde_json::to_vec(&self.state)?;
        if bytes == self.last_bytes {
            use std::os::unix::fs::MetadataExt;
            if runtime::read_regular(&self.state_path, 16 * 1024 * 1024, Some(runtime::euid()))
                .is_ok_and(|(current, st)| current == bytes && st.mode() & 0o777 == 0o644)
            {
                return Ok(());
            }
        }
        runtime::atomic_write(&self.state_path, &bytes, 0o644, true)?;
        self.last_bytes = bytes;
        Ok(())
    }
    pub fn process(
        &mut self,
        snapshot: &Snapshot,
        sources: &BTreeMap<String, Seed>,
        now: f64,
    ) -> Result<()> {
        let mtime = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok());
        if mtime != self.mtime {
            self.config = Config::load(&self.path);
            self.mtime = mtime;
        }
        if !self.config.enabled() {
            return Ok(());
        }
        while let Ok((key, stamp)) = self.delivery.rollbacks.try_recv() {
            self.undo_reservation(&key, stamp);
        }
        if !self.delivery.parent_pending.swap(true, Ordering::Relaxed)
            && !self.delivery.enqueue(Delivery {
                cfg: self.config.clone(),
                text: String::new(),
                target: None,
                reservation: None,
                parent: true,
            })
        {
            self.delivery.parent_pending.store(false, Ordering::Relaxed);
        }
        let clean = snapshot.errors.is_empty() && snapshot.scheduler.job_backend != "unavailable";
        if clean {
            for node in &snapshot.nodes {
                let down = ["down", "drain", "fail", "err", "boot"]
                    .iter()
                    .any(|s| node.state.to_lowercase().contains(s));
                if self.config.node_health {
                    if down {
                        let since = *self.pending_down.entry(node.name.clone()).or_insert(now);
                        if !self.state.down.contains_key(&node.name)
                            && now - since >= self.config.down_grace_sec.max(0.0)
                        {
                            self.state.down.insert(node.name.clone(), since);
                            self.alert(
                                format!("down:{}", node.name),
                                format!(
                                    "{} {} ({})",
                                    self.config.language("Node down", "노드 장애"),
                                    escape(&node.name),
                                    escape(&node.state)
                                ),
                                None,
                                false,
                                now,
                            );
                        }
                    } else {
                        self.pending_down.remove(&node.name);
                        if self.state.down.remove(&node.name).is_some() {
                            self.alert(
                                format!("up:{}", node.name),
                                format!(
                                    "{} {}",
                                    self.config.language("Node recovered", "노드 복구"),
                                    escape(&node.name)
                                ),
                                None,
                                false,
                                now,
                            );
                        }
                    }
                }
                if self.config.collect_alert && node.has_gpu {
                    if !down && (node.stale || !node.error.is_empty()) {
                        let since = *self.pending_blind.entry(node.name.clone()).or_insert(now);
                        if !self.state.blind.contains_key(&node.name)
                            && now - since >= self.config.collect_grace_sec.max(0.0)
                        {
                            self.state.blind.insert(node.name.clone(), since);
                            self.alert(
                                format!("blind:{}", node.name),
                                format!(
                                    "{} {}",
                                    self.config
                                        .language("Node publication lost", "노드 결과 게시 중단"),
                                    escape(&node.name)
                                ),
                                None,
                                false,
                                now,
                            );
                        }
                    } else {
                        self.pending_blind.remove(&node.name);
                        if self.state.blind.remove(&node.name).is_some() && !down {
                            self.alert(
                                format!("sighted:{}", node.name),
                                format!(
                                    "{} {}",
                                    self.config.language(
                                        "Node publication restored",
                                        "노드 결과 게시 복구"
                                    ),
                                    escape(&node.name)
                                ),
                                None,
                                false,
                                now,
                            );
                        }
                    }
                }
            }
            self.job_transitions(snapshot, sources);
        } else {
            self.sources.clear();
            self.pending_down.clear();
            self.pending_blind.clear();
            self.pending_rogue.clear();
        }
        if clean {
            let ignored = rules::ignored_users();
            let mut rogue_live = BTreeSet::new();
            for node in snapshot
                .nodes
                .iter()
                .filter(|n| !n.stale && n.error.is_empty())
            {
                for gpu in &node.gpus {
                    let id = format!("{}:{}", node.name, gpu.index);
                    let hardware = format!(
                        "slot={} /dev/nvidia{} uuid={} pci={} serial={}",
                        escape(&gpu.slot),
                        escape(&gpu.minor),
                        escape(&gpu.uuid),
                        escape(&gpu.pci_bus),
                        escape(&gpu.serial)
                    );
                    let age = gpu.idle_sec.max(gpu.parked_sec);
                    if valid_threshold(self.config.waste_alert_hours)
                        && age as f64 >= self.config.waste_alert_hours * 3600.0
                    {
                        self.alert(
                            format!("waste:{id}:{}", gpu.alloc_jobid),
                            format!(
                                "{} {id} · {} · {} · #{}",
                                self.config.language("Wasted GPU", "GPU 낭비"),
                                rules::span(age),
                                escape(&gpu.alloc_user),
                                gpu.alloc_jobid
                            ),
                            None,
                            true,
                            now,
                        );
                    }
                    if self.config.rogue_alert
                        && rules::classify(gpu, &ignored) == rules::Class::Rogue
                    {
                        let users = gpu
                            .users
                            .iter()
                            .filter(|u| !ignored.contains(*u))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(",");
                        let key = format!("rogue:{id}:{users}");
                        rogue_live.insert(key.clone());
                        let since = *self.pending_rogue.entry(key.clone()).or_insert(now);
                        if now - since >= self.config.rogue_grace_sec.max(0.0) {
                            self.alert(
                                key,
                                format!(
                                    "{} {id} · {}",
                                    self.config.language(
                                        "GPU used outside Slurm allocation",
                                        "Slurm 할당 외 GPU 사용"
                                    ),
                                    escape(&users)
                                ),
                                None,
                                true,
                                now,
                            );
                        }
                    }
                    if valid_threshold(self.config.temp_alert_c)
                        && number(&gpu.temp).is_some_and(|t| t >= self.config.temp_alert_c)
                    {
                        self.alert(
                            format!("temp:{id}"),
                            format!(
                                "{} {id} · {}°C",
                                self.config.language("GPU hot", "GPU 고온"),
                                escape(&gpu.temp)
                            ),
                            None,
                            true,
                            now,
                        );
                    }
                    if self.config.ecc_alert && number(&gpu.ecc).is_some_and(|n| n > 0.0) {
                        self.alert(
                            format!("ecc:{id}"),
                            format!("ECC {id} · {} · {hardware}", escape(&gpu.ecc)),
                            None,
                            true,
                            now,
                        );
                    }
                }
                if valid_threshold(self.config.mem_fair_factor) && !node.gpus.is_empty() {
                    for job in &node.jobs {
                        if job.gpu_count == 0 {
                            continue;
                        }
                        if let Some((mem, total)) =
                            scheduler::mem_mib(&job.mem, job.cpu_count.max(1))
                                .zip(number(&node.mem_total))
                                .filter(|(m, t)| *m > 0.0 && *t > 0.0)
                        {
                            let share = total * job.gpu_count as f64 / node.gpus.len() as f64;
                            if mem > share * self.config.mem_fair_factor {
                                self.alert(
                                    format!("mem:{}:{}", node.name, job.jobid),
                                    format!(
                                        "{} #{} · {} · {:.1}×",
                                        self.config.language(
                                            "RAM over GPU fair share",
                                            "GPU 공정 할당량 초과 RAM"
                                        ),
                                        job.jobid,
                                        escape(&job.user),
                                        mem / share
                                    ),
                                    Some(&job.user),
                                    true,
                                    now,
                                );
                            }
                        }
                    }
                }
            }
            self.pending_rogue.retain(|key, _| rogue_live.contains(key));
            let pending: BTreeSet<_> = snapshot.pending.iter().map(|p| p.jobid.clone()).collect();
            self.state.pend_seen.retain(|id, _| pending.contains(id));
            for job in &snapshot.pending {
                let since = *self.state.pend_seen.entry(job.jobid.clone()).or_insert(now);
                if valid_threshold(self.config.pending_alert_hours)
                    && now - since >= self.config.pending_alert_hours * 3600.0
                    && !matches!(
                        job.reason.as_str(),
                        "Dependency"
                            | "DependencyNeverSatisfied"
                            | "BeginTime"
                            | "JobHeldUser"
                            | "JobHeldAdmin"
                            | "JobArrayTaskLimit"
                    )
                {
                    self.alert(
                        format!("pending:{}", job.jobid),
                        format!(
                            "{} #{} · {} · {}",
                            self.config.language("Job pending", "작업 대기"),
                            job.jobid,
                            escape(&job.user),
                            escape(&rules::pending_reason(job))
                        ),
                        Some(&job.user),
                        true,
                        now,
                    );
                }
            }
            if self.config.free_gpus_min > 0 {
                let free = snapshot
                    .nodes
                    .iter()
                    .flat_map(|n| rules::classes(n, now))
                    .filter(|c| *c == rules::Class::Free)
                    .count();
                let below = free < self.config.free_gpus_min;
                if !below && self.state.free_was_below {
                    self.alert(
                        "free".into(),
                        format!(
                            "{}: {free}",
                            self.config
                                .language("Free GPUs available", "사용 가능한 GPU")
                        ),
                        None,
                        false,
                        now,
                    );
                }
                self.state.free_was_below = below;
            }
        }
        self.save(now)
    }
    fn job_transitions(&mut self, snapshot: &Snapshot, sources: &BTreeMap<String, Seed>) {
        if self.config.job_done_users.is_empty() && self.config.job_fail_users.is_empty() {
            return;
        }
        let watched = |user: &str| {
            self.config
                .job_fail_users
                .iter()
                .any(|u| u == "*" || u == user)
                || self.config.job_done_users.iter().any(|u| u == user)
        };
        let mut current: BTreeMap<_, _> = snapshot
            .jobs
            .iter()
            .filter(|j| watched(&j.user))
            .map(|j| {
                (
                    j.jobid.clone(),
                    Tracked {
                        jobname: j.jobname.clone(),
                        user: j.user.clone(),
                        elapsed: j.elapsed.clone(),
                    },
                )
            })
            .collect();
        let gone: Vec<_> = self
            .state
            .jobs
            .iter()
            .filter(|(id, _)| !current.contains_key(*id))
            .map(|(id, j)| (id.clone(), j.clone()))
            .collect();
        let mut deferred = BTreeSet::new();
        for batch in gone.chunks(32) {
            let lookup = batch
                .iter()
                .filter(|(_, j)| {
                    self.config
                        .job_fail_users
                        .iter()
                        .any(|u| u == "*" || u == &j.user)
                })
                .map(|(id, _)| id.clone())
                .collect();
            let event = Outcome {
                cfg: self.config.clone(),
                jobs: batch.to_vec(),
                lookup,
                sources: batch
                    .iter()
                    .filter_map(|(id, _)| self.sources.get(id).map(|s| (id.clone(), s.clone())))
                    .collect(),
            };
            self.outcome_backlog.fetch_add(1, Ordering::Relaxed);
            if self.outcomes.try_send(event).is_err() {
                self.outcome_backlog.fetch_sub(1, Ordering::Relaxed);
                for (id, j) in batch {
                    current.insert(id.clone(), j.clone());
                    deferred.insert(id.clone());
                }
            }
        }
        self.sources.retain(|id, _| deferred.contains(id));
        for (id, source) in sources {
            if current.contains_key(id) {
                self.sources.insert(id.clone(), source.clone());
            }
        }
        self.state.jobs = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Job;
    use std::os::unix::fs::PermissionsExt;
    #[derive(Default)]
    struct Capture(Mutex<Vec<(String, String, Option<String>)>>);
    impl Transport for Capture {
        fn post(
            &self,
            _: &Config,
            target: &str,
            text: &str,
            thread: Option<&str>,
        ) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .push((target.into(), text.into(), thread.map(str::to_string)));
            Some("123.4".into())
        }
    }
    struct FakeRunner;
    impl Runner for FakeRunner {
        fn run(&self, _: Program, _: &[&str], _: Duration) -> Result<String> {
            Ok("42.batch|FAILED\n42|OUT_OF_MEMORY\n42|COMPLETED\n999|FAILED".into())
        }
    }
    struct PrivateTail;
    impl TailReader for PrivateTail {
        fn tail(&self, _: &Seed) -> Option<String> {
            Some("PRIVATE OWNER LOG".into())
        }
    }
    fn setup(extra: Value) -> (tempfile::TempDir, Notifier, Arc<Capture>) {
        let tmp = tempfile::tempdir().unwrap();
        runtime::secure_dir(tmp.path(), 0o700).unwrap();
        let mut cfg = json!({"bot_token":"fixture","channel":"C-shared","job_done_users":["alice"],"job_fail_users":["alice"],"dm_users":{"alice":"U-owner"}});
        for (k, v) in extra.as_object().unwrap() {
            cfg[k] = v.clone();
        }
        let path = tmp.path().join("slack.json");
        std::fs::write(&path, cfg.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let transport = Arc::new(Capture::default());
        let notifier = Notifier::with_backends(
            tmp.path(),
            path,
            transport.clone(),
            Arc::new(FakeRunner),
            Arc::new(PrivateTail),
        )
        .unwrap();
        (tmp, notifier, transport)
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            scheduler: crate::model::SchedulerStatus {
                job_backend: "structured-json".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn drain(notifier: &Notifier) {
        let start = std::time::Instant::now();
        while notifier.backlog() > 0 && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(notifier.backlog(), 0);
    }
    #[test]
    fn finished_failure_tail_is_only_sent_to_the_captured_owner_dm() {
        let (_tmp, mut notifier, transport) = setup(json!({}));
        let mut first = snapshot();
        first.jobs.push(Job {
            jobid: "42".into(),
            user: "alice".into(),
            ..Default::default()
        });
        let seed = Seed {
            uid: runtime::euid(),
            spec: Default::default(),
        };
        notifier
            .process(&first, &BTreeMap::from([("42".into(), seed)]), 100.0)
            .unwrap();
        notifier
            .process(&snapshot(), &BTreeMap::new(), 103.0)
            .unwrap();
        drain(&notifier);
        let posts = transport.0.lock().unwrap();
        assert!(
            posts
                .iter()
                .any(|(target, text, _)| target == "U-owner" && text.contains("PRIVATE OWNER LOG"))
        );
        assert!(
            posts.iter().all(
                |(target, text, _)| target != "C-shared" || !text.contains("PRIVATE OWNER LOG")
            )
        );
        assert!(
            posts
                .iter()
                .any(|(_, text, _)| text.contains("OUT_OF_MEMORY"))
        );
    }
    #[test]
    fn scheduler_failure_preserves_baseline_and_discards_private_sources() {
        let (_tmp, mut notifier, transport) = setup(json!({}));
        let mut first = snapshot();
        first.jobs.push(Job {
            jobid: "42".into(),
            user: "alice".into(),
            ..Default::default()
        });
        notifier.process(&first, &BTreeMap::new(), 100.0).unwrap();
        let mut failed = snapshot();
        failed.errors = "unavailable".into();
        notifier.process(&failed, &BTreeMap::new(), 103.0).unwrap();
        drain(&notifier);
        assert!(notifier.state.jobs.contains_key("42"));
        assert!(notifier.sources.is_empty());
        assert!(
            !transport
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text, _)| text.contains("#42"))
        );
    }
    #[test]
    fn quiet_day_creates_one_parent_and_state_survives_restart() {
        let (tmp, mut notifier, transport) = setup(json!({}));
        notifier
            .process(&snapshot(), &BTreeMap::new(), 100.0)
            .unwrap();
        drain(&notifier);
        notifier
            .process(&snapshot(), &BTreeMap::new(), 103.0)
            .unwrap();
        drain(&notifier);
        assert_eq!(transport.0.lock().unwrap().len(), 1);
        let state = Persistent::load(&tmp.path().join("notify_state.json"));
        assert_eq!(state.thread_ts, "123.4");
    }
    #[test]
    fn exact_outcome_ids_ignore_steps_duplicates_and_unrequested_rows() {
        let states = outcome_states(&FakeRunner, &["42".into()]);
        assert_eq!(states.len(), 1);
        assert_eq!(states["42"], "OUT_OF_MEMORY");
    }
    #[test]
    fn legacy_normal_node_states_do_not_become_confirmed_outages() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(
            &path,
            json!({"down":{"healthy":0,"down":123},"blind":{"healthy":false,"blind":true}})
                .to_string(),
        )
        .unwrap();
        let state = Persistent::load(&path);
        assert!(!state.down.contains_key("healthy"));
        assert!(!state.blind.contains_key("healthy"));
        assert_eq!(state.down["down"], 123.0);
        assert!(state.blind["blind"] > 0.0);
    }
    #[test]
    fn failed_outage_and_recovery_delivery_retry_the_continuing_condition() {
        struct Flaky {
            fail: AtomicBool,
            posts: Mutex<Vec<String>>,
        }
        impl Transport for Flaky {
            fn post(
                &self,
                _: &Config,
                _: &str,
                text: &str,
                thread: Option<&str>,
            ) -> Option<String> {
                self.posts.lock().unwrap().push(text.into());
                if thread.is_some() && self.fail.load(Ordering::Relaxed) {
                    None
                } else {
                    Some("123.4".into())
                }
            }
        }
        let (_tmp, mut notifier, _) = setup(json!({"down_grace_sec":0}));
        let flaky = Arc::new(Flaky {
            fail: AtomicBool::new(true),
            posts: Mutex::new(Vec::new()),
        });
        notifier.delivery = DeliveryQueue::new(flaky.clone(), String::new(), String::new());
        let mut data = snapshot();
        data.nodes.push(crate::model::Node {
            name: "gpu1".into(),
            state: "down".into(),
            ..Default::default()
        });
        notifier.process(&data, &BTreeMap::new(), 100.0).unwrap();
        drain(&notifier);
        flaky.fail.store(false, Ordering::Relaxed);
        notifier.process(&data, &BTreeMap::new(), 103.0).unwrap();
        drain(&notifier);
        assert!(notifier.state.down.contains_key("gpu1"));
        data.nodes[0].state = "idle".into();
        flaky.fail.store(true, Ordering::Relaxed);
        notifier.process(&data, &BTreeMap::new(), 106.0).unwrap();
        drain(&notifier);
        flaky.fail.store(false, Ordering::Relaxed);
        notifier.process(&data, &BTreeMap::new(), 109.0).unwrap();
        drain(&notifier);
        assert!(!notifier.state.down.contains_key("gpu1"));
        let posts = flaky.posts.lock().unwrap();
        assert_eq!(posts.iter().filter(|s| s.contains("Node down")).count(), 2);
        assert_eq!(
            posts
                .iter()
                .filter(|s| s.contains("Node recovered"))
                .count(),
            2
        );
    }
}
