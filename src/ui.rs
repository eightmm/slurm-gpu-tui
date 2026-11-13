use crate::{
    Result, cli, history,
    model::{Gpu, Job, Node, PendingJob, Snapshot, number},
    process::{self, Program},
    rules::{self, Class},
    runtime, usage,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Tabs,
        Wrap,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::IsTerminal,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const BACKGROUND: Color = Color::Rgb(13, 17, 23);
const PANEL: Color = Color::Rgb(22, 27, 34);
const BORDER: Color = Color::Rgb(48, 58, 70);
const TEXT: Color = Color::Rgb(220, 225, 232);
const MUTED: Color = Color::Rgb(145, 155, 170);
const FOCUS: Color = Color::Rgb(99, 230, 232);
const FOCUS_BG: Color = Color::Rgb(18, 58, 63);

#[derive(Clone, Copy)]
enum MetricKind {
    Activity,
    Memory,
}
impl MetricKind {
    fn color(self, value: Option<f64>) -> Color {
        let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0) else {
            return MUTED;
        };
        match self {
            Self::Activity if value <= 5.0 => MUTED,
            Self::Activity if value < 30.0 => Color::Blue,
            Self::Activity if value < 70.0 => Color::Cyan,
            Self::Activity => Color::Green,
            Self::Memory if value < 50.0 => Color::Blue,
            Self::Memory if value < 80.0 => Color::Cyan,
            Self::Memory if value < 90.0 => Color::Yellow,
            Self::Memory if value < 97.0 => Color::Rgb(255, 166, 87),
            Self::Memory => Color::Red,
        }
    }
}

fn memory_percent(node: &Node) -> Option<f64> {
    if node.stale {
        return None;
    }
    let total = number(&node.mem_total).filter(|t| *t > 0.0)?;
    let used = number(&node.mem_used).or_else(|| {
        number(&node.mem_avail)
            .or_else(|| number(&node.mem_free))
            .map(|a| total - a)
    })?;
    (used >= 0.0)
        .then_some(used / total * 100.0)
        .filter(|v| v.is_finite())
}

fn clipped(raw: &str, width: u16) -> String {
    let safe = cli::terminal_safe(raw).replace(['\n', '\t'], " ");
    if safe.width() <= width as usize {
        return safe;
    }
    let mut left = (width as usize).saturating_sub(1);
    let mut text = String::new();
    for c in safe.chars() {
        let size = c.width().unwrap_or(0);
        if size > left {
            break;
        }
        text.push(c);
        left -= size;
    }
    if width > 0 {
        text.push('…');
    }
    text
}
fn column_alignment(tab: usize, compact: bool, column: usize) -> Alignment {
    let numeric = match tab {
        0 => (compact && column == 4) || (!compact && column == 5),
        1 => matches!(column, 2 | 4 | 5),
        3 => column == 0 || matches!(column, 3..=8),
        4 => matches!(column, 1..=5),
        5 => matches!(column, 1..=9),
        _ => false,
    };
    if numeric {
        Alignment::Right
    } else {
        Alignment::Left
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title_style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(PANEL).fg(TEXT))
}
fn gpu_mark(class: Class) -> (&'static str, &'static str, Color) {
    match class {
        Class::Busy => ("●", "busy", Color::Cyan),
        Class::Free => ("○", "free", Color::Green),
        Class::Idle => ("!", "idle", Color::Yellow),
        Class::Parked => ("◇", "parked", Color::Magenta),
        Class::Rogue => ("×", "rogue", Color::Red),
        Class::Unknown => ("?", "unknown", MUTED),
    }
}
fn mini_meter(value: Option<f64>) -> String {
    let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0) else {
        return "?".into();
    };
    let filled = (value.clamp(0.0, 100.0) / 10.0).round() as usize;
    format!(
        "{}{} {value:>3.0}%",
        "━".repeat(filled),
        "·".repeat(10 - filled)
    )
}
fn throughput(value: Option<f64>) -> String {
    let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0) else {
        return "?".into();
    };
    for (scale, unit) in [
        (1073741824.0, "GiB/s"),
        (1048576.0, "MiB/s"),
        (1024.0, "KiB/s"),
    ] {
        if value >= scale {
            return format!("{:.1} {unit}", value / scale);
        }
    }
    format!("{value:.1} B/s")
}

#[derive(Default)]
struct Meter {
    used: f64,
    capacity: f64,
    samples: usize,
    total: usize,
}
impl Meter {
    fn push(&mut self, reading: Option<(f64, f64)>) {
        self.total += 1;
        if let Some((used, capacity)) =
            reading.filter(|(u, t)| u.is_finite() && *u >= 0.0 && t.is_finite() && *t > 0.0)
        {
            self.used += used;
            self.capacity += capacity;
            self.samples += 1;
        }
    }
    fn value(&self) -> Option<f64> {
        (self.samples > 0)
            .then(|| self.used / self.capacity * 100.0)
            .filter(|v| v.is_finite())
    }
    fn label(&self) -> String {
        self.value()
            .map(|value| {
                format!(
                    "{}{value:.0}% · {}/{}",
                    if self.samples < self.total { "~" } else { "" },
                    self.samples,
                    self.total
                )
            })
            .unwrap_or_else(|| "? unavailable".into())
    }
    fn draw(&self, frame: &mut Frame, area: Rect, name: &str, kind: MetricKind) {
        let columns = Layout::horizontal([Constraint::Length(9), Constraint::Min(1)]).split(area);
        frame.render_widget(
            Paragraph::new(name).style(Style::default().fg(MUTED)),
            Rect::new(
                columns[0].x,
                columns[0].y + columns[0].height / 2,
                columns[0].width,
                u16::from(columns[0].height > 0),
            ),
        );
        frame.render_widget(
            Gauge::default()
                .ratio(self.value().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0)
                .label(self.label())
                .use_unicode(true)
                .gauge_style(Style::default().fg(kind.color(self.value())).bg(BACKGROUND)),
            columns[1],
        );
    }
}

#[derive(Clone)]
pub struct Pane {
    pub name: String,
    pub body: String,
    pub log: Option<cli::LogView>,
}
pub struct Popup {
    pub title: String,
    pub panes: Vec<Pane>,
    pub active: usize,
    pub scroll: u16,
    pub follow: bool,
    pub history: Vec<history::Record>,
    pub selected: usize,
    pub cancel: Option<String>,
    pub token: u64,
}
impl Popup {
    fn text(title: &str, body: String) -> Self {
        Self {
            title: title.into(),
            panes: vec![Pane {
                name: "Info".into(),
                body,
                log: None,
            }],
            active: 0,
            scroll: 0,
            follow: false,
            history: Vec::new(),
            selected: 0,
            cancel: None,
            token: 0,
        }
    }
}

enum Task {
    Refresh,
    Detail {
        job: Option<Box<Job>>,
        pending: Option<Box<PendingJob>>,
        node: Option<Box<Node>>,
    },
    History,
    HistoryDetail(history::Record),
    Cancel(String),
    Logs {
        token: u64,
        views: Vec<(usize, cli::LogView)>,
    },
}
enum Answer {
    Snapshot(Box<crate::Result<(Snapshot, Option<serde_json::Value>)>>),
    Popup(crate::Result<Popup>),
    Status(String),
    Logs {
        token: u64,
        values: Vec<(usize, String)>,
    },
}
struct Worker {
    sender: SyncSender<Task>,
    receiver: Receiver<Answer>,
}
impl Worker {
    fn new() -> Self {
        let (sender, requests) = mpsc::sync_channel(8);
        let (results, receiver) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            while let Ok(task) = requests.recv() {
                let answer = match task {
                    Task::Refresh => Answer::Snapshot(Box::new(
                        cli::snapshot().map(|snapshot| (snapshot, usage::read())),
                    )),
                    Task::Detail { job, pending, node } => {
                        Answer::Popup(details(job.as_deref(), pending.as_deref(), node.as_deref()))
                    }
                    Task::History => Answer::Popup((|| -> Result<Popup> {
                        let mut rows = history::query(
                            &format!("{}T00:00:00", usage::window(7)),
                            "",
                            &cli::current_user(),
                        )?;
                        rows.reverse();
                        let mut popup = Popup::text("My job history · 7 days", String::new());
                        popup.history = rows;
                        Ok(popup)
                    })()),
                    Task::HistoryDetail(row) => Answer::Popup((|| -> Result<Popup> {
                        let mut popup = Popup::text(
                            &format!("History #{}", row.jobid),
                            history::details(&row.jobid)?,
                        );
                        if !row.workdir.is_empty() && PathBuf::from(&row.workdir).is_absolute() {
                            for (suffix, name) in [("out", "StdOut"), ("err", "StdErr")] {
                                let view = cli::LogView {
                                    path: PathBuf::from(&row.workdir)
                                        .join(format!("slurm-{}.{}", row.jobid, suffix)),
                                    owner: Some(runtime::euid()),
                                    shared: false,
                                    merged: false,
                                };
                                if let Ok(body) = view.read(65536) {
                                    popup.panes.push(Pane {
                                        name: name.into(),
                                        body,
                                        log: Some(view),
                                    });
                                }
                            }
                        }
                        Ok(popup)
                    })()),
                    Task::Cancel(id) => Answer::Status(match cancel_owned(&id) {
                        Ok(()) => format!("Job #{id} cancellation requested"),
                        Err(error) => format!("Cancel failed: {error}"),
                    }),
                    Task::Logs { token, views } => Answer::Logs {
                        token,
                        values: views
                            .into_iter()
                            .map(|(index, view)| {
                                (
                                    index,
                                    view.read(65536)
                                        .unwrap_or_else(|_| "Log unavailable; retrying…".into()),
                                )
                            })
                            .collect(),
                    },
                };
                if results.send(answer).is_err() {
                    break;
                }
            }
        });
        Self { sender, receiver }
    }
    fn send(&self, task: Task) -> bool {
        self.sender.try_send(task).is_ok()
    }
}

pub fn cancel_owned(jobid: &str) -> Result<()> {
    let records = cli::query_job(jobid)?;
    let parent = jobid.split("_[").next().unwrap_or(jobid);
    let owner = records.uids.get(jobid).or_else(|| records.uids.get(parent));
    if owner != Some(&runtime::euid()) {
        return Err("only your own validated jobs can be cancelled".into());
    }
    // --ctld routes cancellation through the scheduler, never directly to nodes.
    process::run(
        Program::Scancel,
        &["--ctld", &format!("--user={}", cli::current_user()), jobid],
        Duration::from_secs(12),
    )?;
    Ok(())
}

fn details(job: Option<&Job>, pending: Option<&PendingJob>, node: Option<&Node>) -> Result<Popup> {
    let id = job
        .map(|j| j.jobid.as_str())
        .or_else(|| pending.map(|j| j.jobid.as_str()));
    if let Some(id) = id {
        let published = job
            .map(|j| j.detail.as_str())
            .or_else(|| pending.map(|j| j.detail.as_str()))
            .unwrap_or("");
        let info = if !published.is_empty() {
            published.to_string()
        } else {
            cli::query_job(id)
                .ok()
                .and_then(|r| {
                    r.details
                        .get(id)
                        .or_else(|| r.details.get(id.split("_[").next().unwrap_or(id)))
                        .cloned()
                })
                .unwrap_or_else(|| "Scheduler detail unavailable".into())
        };
        let mut popup = Popup::text(&format!("Job #{id}"), info);
        if let Some(job) = job {
            let summary = rules::resource_summary(
                job,
                &cli::snapshot().map(|s| s.nodes).unwrap_or_default(),
                runtime::now(),
            );
            popup.panes[0].body.push_str(&format!("\n\nObserved resource coverage:\n{}\nSummed node peaks are not a simultaneous whole-job peak.\nCgroup RAM includes cache and descendants.",serde_json::to_string_pretty(&summary)?));
        }
        let script = job
            .map(|j| j.script.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| crate::batch::read_own(id).ok());
        if let Some(script) = script.filter(|s| !s.is_empty()) {
            popup.panes.push(Pane {
                name: "Script".into(),
                body: script
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{:>4} {line}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
                log: None,
            });
        }
        for (err, name) in [(false, "StdOut"), (true, "StdErr")] {
            if let Ok(view) = cli::resolve_log(id, err) {
                let body = view
                    .read(65536)
                    .unwrap_or_else(|_| "Log unavailable".into());
                popup.panes.push(Pane {
                    name: name.into(),
                    body,
                    log: Some(view),
                });
            }
        }
        return Ok(popup);
    }
    if let Some(node) = node {
        let detail = process::run(
            Program::Scontrol,
            &["show", "node", &node.name],
            Duration::from_secs(12),
        )
        .unwrap_or_default();
        return Ok(Popup::text(
            &format!("Node {}", node.name),
            format!("{}\n\n{detail}", serde_json::to_string_pretty(node)?),
        ));
    }
    Err("select a job or node".into())
}

#[derive(Clone)]
struct ViewRow {
    key: String,
    node: String,
    job: Option<String>,
    cells: Vec<String>,
    style: Style,
    focus: Option<usize>,
    accents: Vec<(usize, Style)>,
}
pub struct App {
    pub data: Snapshot,
    pub usage: Option<serde_json::Value>,
    pub tab: usize,
    pub collapsed: BTreeSet<String>,
    pub compact: bool,
    narrow: bool,
    rows: Vec<ViewRow>,
    table: TableState,
    headers: Vec<String>,
    filter_user: String,
    partition: String,
    mine: bool,
    free_only: bool,
    search: String,
    searching: bool,
    sort: usize,
    reverse: bool,
    details: bool,
    pending: bool,
    trend: bool,
    initialized: bool,
    current_user: String,
    highlight_user: String,
    pub popup: Option<Popup>,
    status: String,
    watched: BTreeMap<String, bool>,
    job_baseline: Option<BTreeSet<String>>,
    own_baseline: BTreeSet<String>,
    attention_keys: BTreeSet<String>,
    usage_scroll: u16,
    down_baseline: BTreeSet<String>,
    observations: VecDeque<(f64, [Option<f64>; 3])>,
    last_observation: Vec<(String, u64, bool)>,
    popup_sequence: u64,
}
impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
impl App {
    pub fn new() -> Self {
        Self::for_user(cli::current_user())
    }
    pub fn for_user(current_user: String) -> Self {
        Self {
            data: Snapshot::default(),
            usage: None,
            tab: 0,
            collapsed: BTreeSet::new(),
            compact: false,
            narrow: false,
            rows: Vec::new(),
            table: TableState::default(),
            headers: Vec::new(),
            filter_user: String::new(),
            partition: String::new(),
            mine: false,
            free_only: false,
            search: String::new(),
            searching: false,
            sort: 0,
            reverse: false,
            details: false,
            pending: false,
            trend: false,
            initialized: false,
            highlight_user: current_user.clone(),
            current_user,
            popup: None,
            status: "Loading published telemetry…".into(),
            watched: BTreeMap::new(),
            job_baseline: None,
            own_baseline: BTreeSet::new(),
            attention_keys: BTreeSet::new(),
            usage_scroll: 0,
            down_baseline: BTreeSet::new(),
            observations: VecDeque::new(),
            last_observation: Vec::new(),
            popup_sequence: 0,
        }
    }
    pub fn apply(&mut self, data: Snapshot) {
        let now = runtime::now();
        if data.errors.is_empty() {
            let current: BTreeSet<_> = data.jobs.iter().map(|j| j.jobid.clone()).collect();
            if let Some(previous) = &self.job_baseline {
                let started = data
                    .jobs
                    .iter()
                    .filter(|j| j.user == self.current_user && !previous.contains(&j.jobid))
                    .count();
                if started > 0 {
                    self.status = format!("{started} of your jobs started");
                }
                let finished = self.own_baseline.difference(&current).count();
                if finished > 0 {
                    self.status = format!("{finished} of your jobs finished");
                }
            }
            self.own_baseline = data
                .jobs
                .iter()
                .filter(|j| j.user == self.current_user)
                .map(|j| j.jobid.clone())
                .collect();
            let pending: BTreeSet<_> = data.pending.iter().map(|j| j.jobid.clone()).collect();
            let mut done = Vec::new();
            for (id, was_running) in &mut self.watched {
                if current.contains(id) && !*was_running {
                    self.status = format!("Watched job #{id} started");
                    *was_running = true;
                } else if !current.contains(id) && !pending.contains(id) {
                    self.status = format!("Watched job #{id} ended");
                    done.push(id.clone());
                }
            }
            for id in done {
                self.watched.remove(&id);
            }
            self.job_baseline = Some(current);
            let down: BTreeSet<_> = data
                .nodes
                .iter()
                .filter(|n| {
                    ["down", "fail"]
                        .iter()
                        .any(|s| n.state.to_lowercase().contains(s))
                })
                .map(|n| n.name.clone())
                .collect();
            if self.initialized && down != self.down_baseline {
                self.status = format!(
                    "Node state changed · down: {}",
                    down.iter().cloned().collect::<Vec<_>>().join(",")
                );
            }
            self.down_baseline = down;
        }
        let stamp: Vec<_> = data
            .nodes
            .iter()
            .filter(|n| n.has_gpu)
            .map(|n| (n.name.clone(), n.observed_at.to_bits(), n.stale))
            .collect();
        if stamp != self.last_observation {
            self.observations
                .push_back((now, rules::observation(&data.nodes)));
            self.last_observation = stamp;
        }
        while self.observations.len() > 301
            || self
                .observations
                .front()
                .is_some_and(|(t, _)| now - *t > 300.0)
        {
            self.observations.pop_front();
        }
        if !self.initialized && !data.nodes.is_empty() {
            self.initialized = true;
            self.status = "Snapshot loaded · ↑/↓ select · Space fold · t trend".into();
            if data.nodes.iter().filter(|n| n.has_gpu).count()
                >= runtime::env_seconds("SLURM_GPU_TUI_AUTO_COLLAPSE_NODES", 12.0, 0.0) as usize
            {
                self.collapsed = data
                    .nodes
                    .iter()
                    .filter(|n| n.has_gpu)
                    .map(|n| n.name.clone())
                    .collect();
            }
        }
        self.data = data;
        self.rebuild();
        let attention = crate::users::attention(
            &self.data,
            runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            now,
        );
        let keys: BTreeSet<_> = attention
            .iter()
            .flat_map(|a| {
                a.details
                    .iter()
                    .map(|line| line.split(" · ").take(2).collect::<Vec<_>>().join(" · "))
            })
            .collect();
        if keys.difference(&self.attention_keys).next().is_some()
            && !self.status.starts_with("Cancel")
        {
            self.status = format!(
                "! {} GPUs need attention · 5 Attention for users and evidence",
                keys.len()
            );
        }
        self.attention_keys = keys;
    }
    fn selected(&self) -> Option<&ViewRow> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }
    fn user_filter(&self) -> &str {
        if self.mine {
            &self.highlight_user
        } else {
            &self.filter_user
        }
    }
    pub fn set_highlight_user(&mut self, user: String) {
        self.highlight_user = user;
        self.rebuild();
    }
    fn node_visible(&self, node: &Node) -> bool {
        (self.partition.is_empty() || node.partition.split(',').any(|p| p == self.partition))
            && (self.search.is_empty()
                || std::iter::once(node.name.as_str())
                    .chain(node.gpus.iter().flat_map(|g| {
                        g.users
                            .iter()
                            .map(String::as_str)
                            .chain(std::iter::once(g.alloc_user.as_str()))
                    }))
                    .any(|s| s.to_lowercase().contains(&self.search.to_lowercase())))
    }
    fn gpu_visible(&self, node: &Node, gpu: &Gpu, class: Class) -> bool {
        self.node_visible(node)
            && (!self.free_only || class == Class::Free)
            && (self.user_filter().is_empty()
                || gpu.alloc_user == self.user_filter()
                || gpu.users.iter().any(|u| u == self.user_filter()))
    }
    fn job_visible(&self, user: &str, partition: &str, fields: &[&str]) -> bool {
        (self.user_filter().is_empty() || user == self.user_filter())
            && (self.partition.is_empty() || partition.split(',').any(|p| p == self.partition))
            && (self.search.is_empty()
                || fields
                    .iter()
                    .any(|s| s.to_lowercase().contains(&self.search.to_lowercase())))
    }
    pub fn rebuild(&mut self) {
        let selected = self.selected().map(|r| r.key.clone());
        let now = runtime::now();
        let mut rows = Vec::new();
        match self.tab {
            0 => {
                self.headers = if self.compact {
                    vec!["Node / GPU", "Util", "VRAM", "User / job", "Left", "Health"]
                } else {
                    vec![
                        "Node / GPU",
                        "Model / state",
                        "Util",
                        "VRAM",
                        "User / job",
                        "Left",
                        "Health",
                    ]
                }
                .into_iter()
                .map(str::to_string)
                .collect();
                if self.details {
                    self.headers
                        .extend(["Temp", "Power", "JobID", "Job name"].map(str::to_string));
                }
                let mut nodes: Vec<_> = self.data.nodes.iter().filter(|n| n.has_gpu).collect();
                nodes.sort_by(|a, b| {
                    match self.sort {
                        1 => {
                            let peak = |n: &Node| {
                                n.gpus
                                    .iter()
                                    .filter_map(|g| number(&g.util))
                                    .fold(0.0, f64::max)
                            };
                            peak(b).total_cmp(&peak(a))
                        }
                        2 => {
                            let own = |n: &Node| {
                                n.gpus.iter().any(|g| {
                                    g.alloc_user == self.highlight_user
                                        || g.users.contains(&self.highlight_user)
                                })
                            };
                            own(b).cmp(&own(a))
                        }
                        3 => {
                            let free = |n: &Node| {
                                rules::classes(n, now)
                                    .iter()
                                    .filter(|c| **c == Class::Free)
                                    .count()
                            };
                            free(b).cmp(&free(a))
                        }
                        _ => a.name.cmp(&b.name),
                    }
                    .then(a.name.cmp(&b.name))
                });
                if self.reverse {
                    nodes.reverse();
                }
                for node in nodes {
                    let classes = rules::classes(node, now);
                    if !node
                        .gpus
                        .iter()
                        .zip(&classes)
                        .any(|(g, c)| self.gpu_visible(node, g, *c))
                    {
                        continue;
                    }
                    let free = node
                        .gpus
                        .iter()
                        .zip(&classes)
                        .filter(|(g, c)| **c == Class::Free && self.gpu_visible(node, g, **c))
                        .count();
                    let state = if node.stale { "STALE" } else { &node.state };
                    let model = node
                        .gpus
                        .iter()
                        .map(|g| g.name.as_str())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(",");
                    let fold = if self.collapsed.contains(&node.name) {
                        "▶"
                    } else {
                        "▼"
                    };
                    let mut group_util = Meter::default();
                    let mut group_vram = Meter::default();
                    for (gpu, class) in node.gpus.iter().zip(&classes) {
                        if self.gpu_visible(node, gpu, *class) {
                            group_util.push(
                                (!node.stale)
                                    .then(|| number(&gpu.util).map(|v| (v, 100.0)))
                                    .flatten(),
                            );
                            group_vram.push(
                                (!node.stale)
                                    .then(|| number(&gpu.mem_used).zip(number(&gpu.mem_total)))
                                    .flatten(),
                            );
                        }
                    }
                    let util = if group_util.samples < group_util.total {
                        group_util
                            .value()
                            .map(|v| format!("~{v:.0}%"))
                            .unwrap_or_else(|| "?".into())
                    } else {
                        mini_meter(group_util.value())
                    };
                    let vram = group_vram
                        .value()
                        .map(|pct| {
                            let bar = format!(
                                "{}{}",
                                if group_vram.samples < group_vram.total {
                                    "~"
                                } else {
                                    ""
                                },
                                mini_meter(Some(pct))
                            );
                            if self.narrow {
                                bar
                            } else {
                                format!(
                                    "{bar} {:.0}/{:.0}G",
                                    group_vram.used / 1024.0,
                                    group_vram.capacity / 1024.0
                                )
                            }
                        })
                        .unwrap_or_else(|| "?".into());
                    let resources = format!(
                        "{model} · CPU {}/{} · RAM {}",
                        node.cpu_alloc,
                        node.cpus,
                        if node.stale { "?".into() } else { ram(node) }
                    );
                    let mut header = if self.compact {
                        vec![
                            format!("{fold} {}", node.name),
                            util,
                            vram,
                            resources,
                            format!("free {free}"),
                            if node.stale {
                                "STALE".into()
                            } else {
                                state.into()
                            },
                        ]
                    } else {
                        vec![
                            format!("{fold} {}", node.name),
                            state.into(),
                            util,
                            vram,
                            resources,
                            format!("free {free}"),
                            if node.stale {
                                "STALE".into()
                            } else {
                                String::new()
                            },
                        ]
                    };
                    if self.details {
                        header.resize(header.len() + 4, String::new());
                    }
                    rows.push(ViewRow {
                        key: format!("node:{}", node.name),
                        node: node.name.clone(),
                        job: None,
                        cells: header,
                        focus: node
                            .gpus
                            .iter()
                            .zip(&classes)
                            .any(|(g, c)| {
                                self.gpu_visible(node, g, *c)
                                    && (g.alloc_user == self.highlight_user
                                        || (g.alloc_user.is_empty()
                                            && g.users.contains(&self.highlight_user)))
                            })
                            .then_some(0),
                        accents: vec![
                            (
                                if self.compact { 1 } else { 2 },
                                Style::default().fg(MetricKind::Activity.color(group_util.value())),
                            ),
                            (
                                if self.compact { 2 } else { 3 },
                                Style::default().fg(MetricKind::Memory.color(group_vram.value())),
                            ),
                        ],
                        style: Style::default()
                            .bg(Color::Rgb(30, 44, 61))
                            .fg(if node.stale {
                                Color::Yellow
                            } else {
                                Color::White
                            })
                            .add_modifier(Modifier::BOLD),
                    });
                    if self.collapsed.contains(&node.name) {
                        continue;
                    }
                    for (gpu, class) in node.gpus.iter().zip(classes) {
                        if !self.gpu_visible(node, gpu, class) {
                            continue;
                        }
                        let job = node.jobs.iter().find(|j| j.jobid == gpu.alloc_jobid);
                        let user = if gpu.users.is_empty() {
                            gpu.alloc_user.clone()
                        } else {
                            gpu.users.join(",")
                        };
                        let util = mini_meter((!node.stale).then(|| number(&gpu.util)).flatten());
                        let vram = number(&gpu.mem_used)
                            .zip(number(&gpu.mem_total))
                            .filter(|(u, t)| !node.stale && *u >= 0.0 && *t > 0.0)
                            .map(|(u, t)| {
                                let bar = mini_meter(Some(u / t * 100.0));
                                if self.narrow {
                                    bar
                                } else {
                                    format!("{bar} {:.0}/{:.0}G", u / 1024.0, t / 1024.0)
                                }
                            })
                            .unwrap_or_else(|| "?".into());
                        let left = job
                            .and_then(|j| {
                                rules::duration(&j.time_limit).zip(rules::duration(&j.elapsed))
                            })
                            .map(|(limit, elapsed)| rules::span(limit.saturating_sub(elapsed)))
                            .unwrap_or_default();
                        let label = format!(
                            "{user} {}{}",
                            if self.watched.contains_key(&gpu.alloc_jobid) {
                                "★ "
                            } else {
                                ""
                            },
                            if gpu.alloc_jobid.is_empty() {
                                String::new()
                            } else {
                                format!("#{}", gpu.alloc_jobid)
                            }
                        );
                        let health = if node.stale {
                            "STALE".into()
                        } else {
                            rules::health(gpu, now).0
                        };
                        let mut cells = if self.compact {
                            vec![
                                format!(" {} GPU{}", gpu_mark(class).0, gpu.index),
                                util,
                                vram,
                                label,
                                left,
                                health,
                            ]
                        } else {
                            vec![
                                format!(" {} GPU{}", gpu_mark(class).0, gpu.index),
                                gpu.name.clone(),
                                util,
                                vram,
                                label,
                                left,
                                health,
                            ]
                        };
                        if self.details {
                            cells.extend([
                                gpu.temp.clone(),
                                format!("{}/{}W", gpu.power, gpu.power_cap),
                                gpu.alloc_jobid.clone(),
                                job.map(|j| j.jobname.clone()).unwrap_or_default(),
                            ]);
                        }
                        let style = Style::default().fg(match class {
                            Class::Rogue => Color::Red,
                            Class::Idle => Color::Yellow,
                            Class::Parked => Color::Magenta,
                            Class::Unknown => MUTED,
                            Class::Free => Color::Green,
                            _ => Color::White,
                        });
                        let style = if gpu.alloc_user == self.highlight_user
                            || (gpu.alloc_user.is_empty()
                                && gpu.users.contains(&self.highlight_user))
                        {
                            style.bg(Color::Rgb(22, 35, 50)).fg(gpu_mark(class).2)
                        } else {
                            style
                        };
                        rows.push(ViewRow {
                            key: format!("gpu:{}:{}", node.name, gpu.index),
                            node: node.name.clone(),
                            job: (!gpu.alloc_jobid.is_empty()).then(|| gpu.alloc_jobid.clone()),
                            cells,
                            focus: (gpu.alloc_user == self.highlight_user
                                || (gpu.alloc_user.is_empty()
                                    && gpu.users.contains(&self.highlight_user)))
                            .then_some(if self.compact { 3 } else { 4 }),
                            accents: vec![
                                (
                                    if self.compact { 1 } else { 2 },
                                    Style::default().fg(MetricKind::Activity
                                        .color((!node.stale).then(|| number(&gpu.util)).flatten())),
                                ),
                                (
                                    if self.compact { 2 } else { 3 },
                                    Style::default().fg(MetricKind::Memory.color(
                                        (!node.stale)
                                            .then(|| {
                                                number(&gpu.mem_used)
                                                    .zip(number(&gpu.mem_total))
                                                    .filter(|(u, t)| *u >= 0.0 && *t > 0.0)
                                                    .map(|(u, t)| u / t * 100.0)
                                            })
                                            .flatten(),
                                    )),
                                ),
                                (
                                    if self.compact { 5 } else { 6 },
                                    Style::default().fg(if node.stale {
                                        Color::Yellow
                                    } else if rules::health(gpu, now).1 > 0 {
                                        Color::Red
                                    } else {
                                        MUTED
                                    }),
                                ),
                            ],
                            style,
                        });
                    }
                }
                if self.pending {
                    for job in &self.data.pending {
                        if self.job_visible(
                            &job.user,
                            &job.partition,
                            &[
                                &job.jobid,
                                &job.user,
                                &job.jobname,
                                &job.partition,
                                &job.reason,
                            ],
                        ) {
                            let mut cells = vec![
                                format!("PD #{}", job.jobid),
                                job.user.clone(),
                                format!("x{}", job.gpu_count),
                                job.partition.clone(),
                                rules::pending_reason(job),
                                rules::pending_wait(job, now)
                                    .map(|s| rules::span(s as u64))
                                    .unwrap_or_else(|| "?".into()),
                                job.start_time.clone(),
                            ];
                            if self.compact {
                                cells.remove(2);
                            }
                            if self.details {
                                cells.resize(cells.len() + 4, String::new());
                            }
                            rows.push(ViewRow {
                                key: format!("pending:{}", job.jobid),
                                node: String::new(),
                                job: Some(job.jobid.clone()),
                                cells,
                                focus: (job.user == self.highlight_user).then_some(1),
                                accents: Vec::new(),
                                style: Style::default().fg(Color::Yellow),
                            });
                        }
                    }
                }
            }
            1 => {
                self.headers = [
                    "Node",
                    "State",
                    "CPU alloc",
                    "CPU busy",
                    "RAM",
                    "Load",
                    "PSI C/M/I",
                    "Partition",
                ]
                .map(str::to_string)
                .to_vec();
                for node in &self.data.nodes {
                    if !self.partition.is_empty()
                        && !node.partition.split(',').any(|p| p == self.partition)
                        || !self.search.is_empty()
                            && !node
                                .name
                                .to_lowercase()
                                .contains(&self.search.to_lowercase())
                        || !self.user_filter().is_empty()
                            && !node.jobs.iter().any(|j| j.user == self.user_filter())
                    {
                        continue;
                    }
                    let fresh =
                        !node.stale && rules::telemetry_fresh(node.telemetry_observed_at, now);
                    let psi = ["cpu", "memory", "io"]
                        .iter()
                        .map(|r| {
                            if fresh {
                                node.pressure.get(*r).cloned().unwrap_or_else(|| "?".into())
                            } else {
                                "?".into()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("/");
                    rows.push(ViewRow {
                        key: format!("node:{}", node.name),
                        node: node.name.clone(),
                        job: None,
                        cells: vec![
                            node.name.clone(),
                            if node.stale {
                                "STALE".into()
                            } else {
                                node.state.clone()
                            },
                            format!("{}/{}", node.cpu_alloc, node.cpus),
                            mini_meter(fresh.then(|| number(&node.cpu_util)).flatten()),
                            ram(node),
                            node.cpu_load.clone(),
                            psi,
                            node.partition.clone(),
                        ],
                        focus: node
                            .jobs
                            .iter()
                            .any(|j| j.user == self.highlight_user)
                            .then_some(0),
                        accents: vec![
                            (
                                3,
                                Style::default().fg(MetricKind::Activity
                                    .color(fresh.then(|| number(&node.cpu_util)).flatten())),
                            ),
                            (
                                4,
                                Style::default().fg(MetricKind::Memory.color(memory_percent(node))),
                            ),
                        ],
                        style: Style::default().fg(if node.stale {
                            Color::Yellow
                        } else {
                            Color::White
                        }),
                    });
                }
            }
            2 => {
                self.headers.clear();
            }
            3 => {
                self.headers = [
                    "JobID",
                    "User",
                    "State",
                    "Req GPU/CPU/RAM",
                    "CPU used",
                    "RAM used",
                    "VRAM",
                    "Left / wait",
                    "Coverage / OOM",
                    "Name",
                ]
                .map(str::to_string)
                .to_vec();
                for job in &self.data.jobs {
                    if !self.job_visible(
                        &job.user,
                        &job.partition,
                        &[&job.jobid, &job.user, &job.jobname, &job.node],
                    ) {
                        continue;
                    }
                    let metrics = rules::resource_summary(job, &self.data.nodes, now);
                    let field = |key: &str, scale: f64| {
                        metrics
                            .get(key)
                            .map(|v| {
                                format!(
                                    "{}{:.1}",
                                    if metrics.get(&format!("{key}_nodes")).copied().unwrap_or(0.0)
                                        < metrics.get("expected_nodes").copied().unwrap_or(0.0)
                                    {
                                        "~"
                                    } else {
                                        ""
                                    },
                                    v / scale
                                )
                            })
                            .unwrap_or_else(|| "?".into())
                    };
                    rows.push(ViewRow {
                        key: format!("job:{}", job.jobid),
                        node: String::new(),
                        job: Some(job.jobid.clone()),
                        cells: vec![
                            job.jobid.clone(),
                            job.user.clone(),
                            "RUNNING".into(),
                            format!("{}/{}/{}", job.gpu_count, job.cpu_count, job.mem),
                            field("cpu_cores", 1.0),
                            field("mem_current_mib", 1024.0),
                            field("vram_mib", 1024.0),
                            rules::duration(&job.time_limit)
                                .zip(rules::duration(&job.elapsed))
                                .map(|(l, e)| rules::span(l.saturating_sub(e)))
                                .unwrap_or_else(|| job.elapsed.clone()),
                            format!(
                                "{}/{} OOM:{}",
                                metrics.get("sampled_nodes").unwrap_or(&0.0),
                                metrics.get("expected_nodes").unwrap_or(&0.0),
                                field("oom_kill", 1.0)
                            ),
                            job.jobname.clone(),
                        ],
                        focus: (job.user == self.highlight_user).then_some(1),
                        accents: vec![
                            (2, Style::default().fg(Color::Green)),
                            (
                                8,
                                Style::default().fg(
                                    if metrics.get("oom_kill").is_some_and(|n| *n > 0.0) {
                                        Color::Red
                                    } else {
                                        MUTED
                                    },
                                ),
                            ),
                        ],
                        style: Style::default().fg(TEXT),
                    });
                }
                for job in &self.data.pending {
                    if self.job_visible(
                        &job.user,
                        &job.partition,
                        &[&job.jobid, &job.user, &job.jobname, &job.reason],
                    ) {
                        rows.push(ViewRow {
                            key: format!("pending:{}", job.jobid),
                            node: String::new(),
                            job: Some(job.jobid.clone()),
                            cells: vec![
                                job.jobid.clone(),
                                job.user.clone(),
                                "PENDING".into(),
                                format!("{}/{}/{}", job.gpu_count, job.cpu_count, job.mem),
                                "?".into(),
                                "?".into(),
                                "?".into(),
                                rules::pending_wait(job, now)
                                    .map(|s| rules::span(s as u64))
                                    .unwrap_or_else(|| "?".into()),
                                rules::pending_reason(job),
                                job.jobname.clone(),
                            ],
                            focus: (job.user == self.highlight_user).then_some(1),
                            accents: vec![(2, Style::default().fg(Color::Yellow))],
                            style: Style::default().fg(TEXT),
                        });
                    }
                }
            }
            4 => {
                self.headers = [
                    "User", "GPUs", "Idle", "Parked", "Rogue", "Longest", "Jobs", "Nodes",
                ]
                .map(str::to_string)
                .to_vec();
                for group in crate::users::attention_in_partition(
                    &self.data,
                    runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
                    now,
                    &self.partition,
                ) {
                    if !self.user_filter().is_empty() && group.user != self.user_filter() {
                        continue;
                    }
                    if !self.search.is_empty()
                        && !group
                            .user
                            .to_lowercase()
                            .contains(&self.search.to_lowercase())
                        && !group
                            .nodes
                            .iter()
                            .any(|n| n.to_lowercase().contains(&self.search.to_lowercase()))
                    {
                        continue;
                    }
                    rows.push(ViewRow {
                        key: format!("attention:{}", group.user),
                        node: String::new(),
                        job: None,
                        focus: (group.user == self.highlight_user).then_some(0),
                        accents: vec![(
                            4,
                            Style::default().fg(if group.rogue > 0 { Color::Red } else { MUTED }),
                        )],
                        style: Style::default().fg(if group.rogue > 0 {
                            Color::Red
                        } else {
                            Color::Yellow
                        }),
                        cells: group.cells(),
                    });
                }
            }
            5 => {
                self.headers = [
                    "User",
                    "Run/PD",
                    "GPU",
                    "CPU cores",
                    "RAM GiB",
                    "IO R/W MiB/s",
                    "Node net MiB/s",
                    "Load1 max",
                    "Quota GiB",
                    "Files",
                    "Nodes",
                ]
                .map(str::to_string)
                .to_vec();
                let mut people =
                    crate::users::summarize(&self.data, now, self.user_filter(), &self.partition);
                people.sort_by(|a, b| {
                    match self.sort {
                        1 => b.cpu.value.total_cmp(&a.cpu.value),
                        2 => (b.name == self.highlight_user).cmp(&(a.name == self.highlight_user)),
                        3 => b.quota.value.total_cmp(&a.quota.value),
                        _ => b.gpus.cmp(&a.gpus),
                    }
                    .then(a.name.cmp(&b.name))
                });
                if self.reverse {
                    people.reverse();
                }
                for person in people {
                    if !self.search.is_empty()
                        && !person
                            .name
                            .to_lowercase()
                            .contains(&self.search.to_lowercase())
                        && !person
                            .nodes
                            .iter()
                            .any(|n| n.to_lowercase().contains(&self.search.to_lowercase()))
                    {
                        continue;
                    }
                    rows.push(ViewRow {
                        key: format!("user:{}", person.name),
                        node: String::new(),
                        job: None,
                        focus: (person.name == self.highlight_user).then_some(0),
                        accents: vec![
                            (6, Style::default().fg(Color::Blue)),
                            (7, Style::default().fg(Color::Blue)),
                        ],
                        style: Style::default().fg(TEXT),
                        cells: person.cells(),
                    });
                }
            }
            _ => {
                self.headers = ["Resource", "Value", "Context"]
                    .map(str::to_string)
                    .to_vec();
                let host = &self.data.controller;
                if crate::model::json_number(&host["observed_at"])
                    .is_some_and(|t| rules::telemetry_fresh(t, now))
                {
                    let mut add = |key: String,
                                   resource: String,
                                   value: String,
                                   context: String,
                                   color: Color| {
                        rows.push(ViewRow {
                            key,
                            node: String::new(),
                            job: None,
                            cells: vec![resource, value, context],
                            focus: None,
                            accents: vec![(1, Style::default().fg(color))],
                            style: Style::default().fg(TEXT),
                        })
                    };
                    let hostname = host["hostname"].as_str().unwrap_or("controller");
                    let cpu = crate::model::json_number(&host["cpu_util"]);
                    add(
                        "host:cpu".into(),
                        "CPU busy".into(),
                        mini_meter(cpu),
                        hostname.into(),
                        MetricKind::Activity.color(cpu),
                    );
                    let ram = crate::model::json_number(&host["mem_used_bytes"])
                        .zip(crate::model::json_number(&host["mem_total_bytes"]))
                        .filter(|(_, t)| *t > 0.0);
                    let pct = ram.map(|(u, t)| u / t * 100.0);
                    add(
                        "host:ram".into(),
                        "RAM".into(),
                        format!(
                            "{} {}",
                            mini_meter(pct),
                            ram.map(|(u, t)| format!(
                                "{:.1}/{:.1} GiB",
                                u / 1073741824.0,
                                t / 1073741824.0
                            ))
                            .unwrap_or_default()
                        ),
                        "Observed host memory".into(),
                        MetricKind::Memory.color(pct),
                    );
                    let load = ["load1", "load5", "load15"]
                        .map(|key| {
                            crate::model::json_number(&host[key])
                                .map(|v| format!("{v:.2}"))
                                .unwrap_or_else(|| "?".into())
                        })
                        .join(" / ");
                    add(
                        "host:load".into(),
                        "Load average".into(),
                        load,
                        "1 / 5 / 15 minutes; shared host".into(),
                        Color::Blue,
                    );
                    for (key, label, unit) in [
                        ("cycle_seconds", "Collector cycle", "s"),
                        ("interval_seconds", "Refresh interval", "s"),
                        ("log_inflight", "Log queue", "items"),
                        ("notify_backlog", "Slack queue", "items"),
                    ] {
                        if let Some(value) = crate::model::json_number(&self.data.collector[key]) {
                            add(
                                format!("service:{key}"),
                                label.into(),
                                format!("{value:.2} {unit}"),
                                "Collector diagnostics".into(),
                                MUTED,
                            );
                        }
                    }
                    for disk in host["disks"].as_array().into_iter().flatten() {
                        let name = disk["name"].as_str().unwrap_or("?");
                        let rate = |key: &str| throughput(crate::model::json_number(&disk[key]));
                        add(
                            format!("disk:{name}"),
                            format!("Disk {name}"),
                            format!("R {} / W {}", rate("read_bps"), rate("write_bps")),
                            "Physical device; two samples".into(),
                            TEXT,
                        );
                    }
                    for net in host["interfaces"].as_array().into_iter().flatten() {
                        let name = net["name"].as_str().unwrap_or("?");
                        let rate = |key: &str| throughput(crate::model::json_number(&net[key]));
                        add(
                            format!("net:{name}"),
                            format!("Net {name}"),
                            format!("RX {} / TX {}", rate("rx_bps"), rate("tx_bps")),
                            "Physical interface; shared host".into(),
                            Color::Blue,
                        );
                    }
                    let storage_fresh = crate::users::storage_fresh(host, now);
                    if !storage_fresh {
                        add(
                            "fs:pending".into(),
                            "Filesystems".into(),
                            "?".into(),
                            "Background sample pending or stale".into(),
                            MUTED,
                        );
                    }
                    if host["storage_truncated"] == true {
                        add(
                            "fs:partial".into(),
                            "Storage coverage".into(),
                            "~ partial".into(),
                            "Filesystem/account sampling limit".into(),
                            Color::Yellow,
                        );
                    }
                    for fs in host["filesystems"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|_| storage_fresh)
                    {
                        let mount = fs["mount"].as_str().unwrap_or("?");
                        let used = crate::model::json_number(&fs["used_bytes"]);
                        let total = crate::model::json_number(&fs["total_bytes"]);
                        let pct = used
                            .zip(total)
                            .filter(|(_, t)| *t > 0.0)
                            .map(|(u, t)| u / t * 100.0);
                        let value = used
                            .zip(total)
                            .map(|(u, t)| {
                                format!(
                                    "{} {:.1}/{:.1} GiB",
                                    mini_meter(pct),
                                    u / 1073741824.0,
                                    t / 1073741824.0
                                )
                            })
                            .unwrap_or_else(|| "?".into());
                        let available = crate::model::json_number(&fs["available_bytes"])
                            .map(|n| format!("{:.1} GiB free", n / 1073741824.0))
                            .unwrap_or_else(|| "?".into());
                        add(
                            format!("fs:{mount}"),
                            format!("FS {mount}"),
                            value,
                            available,
                            MetricKind::Memory.color(pct),
                        );
                    }
                }
            }
        }
        self.rows = rows;
        let index = selected
            .and_then(|key| self.rows.iter().position(|r| r.key == key))
            .unwrap_or_else(|| {
                self.table
                    .selected()
                    .unwrap_or(0)
                    .min(self.rows.len().saturating_sub(1))
            });
        self.table.select((!self.rows.is_empty()).then_some(index));
    }
    fn spark(&self, column: usize) -> String {
        let now = runtime::now();
        let mut values = vec![None; 40];
        for (stamp, observation) in &self.observations {
            let age = now - stamp;
            if (0.0..300.0).contains(&age) {
                values[39 - (age / 7.5) as usize] = observation[column];
            }
        }
        let peak = if column < 2 {
            100.0
        } else {
            values.iter().flatten().copied().fold(1.0, f64::max)
        };
        let glyphs = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        values
            .into_iter()
            .map(|v| {
                v.map(|v| glyphs[((v / peak * 7.0).round() as usize).min(7)])
                    .unwrap_or('·')
            })
            .collect()
    }
    fn draw_dashboard(&self, frame: &mut Frame, area: Rect) {
        if area.height < 3 || area.width < 50 {
            return;
        }
        let now = runtime::now();
        let mut gpu_util = Meter::default();
        let mut vram = Meter::default();
        let mut cpu = Meter::default();
        let mut memory = Meter::default();
        let mut mapped = Vec::new();
        for node in &self.data.nodes {
            let visible: Vec<_> = node
                .gpus
                .iter()
                .zip(rules::classes(node, now))
                .filter(|(gpu, class)| self.gpu_visible(node, gpu, *class))
                .collect();
            for (gpu, _) in &visible {
                let fresh = !node.stale && node.observed_at > 0.0;
                gpu_util.push(
                    fresh
                        .then(|| number(&gpu.util).map(|v| (v, 100.0)))
                        .flatten(),
                );
                vram.push(
                    fresh
                        .then(|| number(&gpu.mem_used).zip(number(&gpu.mem_total)))
                        .flatten(),
                );
            }
            if node.has_gpu && !visible.is_empty() {
                mapped.push((node, visible));
            }
            let included = if self.tab == 0 {
                mapped.last().is_some_and(|(n, _)| n.name == node.name)
            } else {
                self.node_visible(node)
                    && (self.user_filter().is_empty()
                        || node.jobs.iter().any(|j| j.user == self.user_filter()))
            };
            if !included {
                continue;
            }
            let fresh = !node.stale && rules::telemetry_fresh(node.telemetry_observed_at, now);
            cpu.push(
                fresh
                    .then(|| {
                        number(&node.cpu_util)
                            .zip(number(&node.cpus))
                            .map(|(u, c)| (u * c / 100.0, c))
                    })
                    .flatten(),
            );
            let ram = (!node.stale && node.observed_at > 0.0)
                .then(|| {
                    let total = number(&node.mem_total)?;
                    let used = number(&node.mem_used).or_else(|| {
                        number(&node.mem_avail)
                            .or_else(|| number(&node.mem_free))
                            .map(|v| (total - v).max(0.0))
                    })?;
                    Some((used, total))
                })
                .flatten();
            memory.push(ram);
        }
        let columns =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);
        let resources = panel(" Resources · visible / ~ partial ");
        let inner = resources.inner(columns[0]);
        frame.render_widget(resources, columns[0]);
        let meters = Layout::vertical([Constraint::Ratio(1, 4); 4]).split(inner);
        for (index, (name, meter, color)) in [
            ("GPU util", &gpu_util, MetricKind::Activity),
            ("VRAM", &vram, MetricKind::Memory),
            ("CPU busy", &cpu, MetricKind::Activity),
            ("RAM used", &memory, MetricKind::Memory),
        ]
        .into_iter()
        .enumerate()
        {
            meter.draw(frame, meters[index], name, color);
        }
        let selected = self.selected().map(|r| r.node.as_str()).unwrap_or("");
        let capacity = columns[1].height.saturating_sub(2) as usize;
        let selected_index = mapped
            .iter()
            .position(|(n, _)| n.name == selected)
            .unwrap_or(0);
        let start = selected_index
            .saturating_sub(capacity / 2)
            .min(mapped.len().saturating_sub(capacity));
        let title = format!(
            " GPU map · {}–{}/{} nodes ",
            if mapped.is_empty() { 0 } else { start + 1 },
            (start + capacity).min(mapped.len()),
            mapped.len()
        );
        let map = panel(&title);
        let inner = map.inner(columns[1]);
        frame.render_widget(map, columns[1]);
        let lines = mapped
            .iter()
            .skip(start)
            .take(capacity)
            .map(|(node, gpus)| {
                let name = cli::terminal_safe(&node.name)
                    .chars()
                    .take(10)
                    .collect::<String>();
                let mut spans = vec![Span::styled(
                    format!(
                        "{} {name:<10} ",
                        if node.name == selected { "▸" } else { " " }
                    ),
                    Style::default().fg(if node.stale { Color::Yellow } else { TEXT }),
                )];
                let room = inner.width.saturating_sub(24) as usize;
                for (_, class) in gpus.iter().take(room) {
                    let (mark, _, color) = gpu_mark(*class);
                    spans.push(Span::styled(
                        mark,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ));
                }
                if gpus.len() > room {
                    spans.push(Span::styled("…", Style::default().fg(MUTED)));
                }
                let free = gpus.iter().filter(|(_, c)| *c == Class::Free).count();
                spans.push(Span::styled(
                    format!(" {free}/{} free", gpus.len()),
                    Style::default().fg(if node.stale { Color::Yellow } else { MUTED }),
                ));
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(if lines.is_empty() {
                vec![Line::from("No GPU matches these filters")]
            } else {
                lines
            }),
            inner,
        );
    }
    pub fn draw(&mut self, frame: &mut Frame) {
        let compact = frame.area().width < 130;
        let narrow = frame.area().width < 100;
        if compact != self.compact || narrow != self.narrow {
            self.compact = compact;
            self.narrow = narrow;
            self.rebuild();
        }
        frame.render_widget(
            Block::default().style(Style::default().fg(TEXT).bg(BACKGROUND)),
            frame.area(),
        );
        let dashboard_height =
            if frame.area().height >= 20 && frame.area().width >= 70 && self.tab != 6 {
                if frame.area().height >= 30 { 10 } else { 6 }
            } else {
                0
            };
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(dashboard_height),
            Constraint::Length(if self.trend { 7 } else { 4 }),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
        let heading =
            Layout::horizontal([Constraint::Length(9), Constraint::Min(1)]).split(areas[0]);
        frame.render_widget(
            Paragraph::new(" ▰ sgpu ").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            heading[0],
        );
        frame.render_widget(
            Tabs::new([
                "1 GPU", "2 CPU", "3 Use", "4 Jobs", "5 Attn", "6 Users", "7 Host",
            ])
            .select(self.tab)
            .style(Style::default().fg(MUTED))
            .divider(Span::styled("│", Style::default().fg(BORDER)))
            .highlight_style(
                Style::default()
                    .fg(BACKGROUND)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            heading[1],
        );
        self.draw_dashboard(frame, areas[1]);
        let mut free = 0;
        let mut total = 0;
        for node in &self.data.nodes {
            for (gpu, class) in node.gpus.iter().zip(rules::classes(node, runtime::now())) {
                if self.gpu_visible(node, gpu, class) {
                    total += 1;
                    free += usize::from(class == Class::Free);
                }
            }
        }
        let fresh = self.data.nodes.iter().filter(|n| !n.stale).count();
        let mut summary = vec![Line::from(vec![
            Span::styled(
                format!("GPU {free}/{total} FREE"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " · cluster: {} run / {} pd · fresh {fresh}/{}",
                self.data.jobs.len(),
                self.data.pending.len(),
                self.data.nodes.len()
            )),
        ])];
        let sort_label = match self.tab {
            1 => "node",
            2 => "GPU-hours",
            3 => "scheduler",
            4 => "priority",
            5 => ["GPU", "CPU", "focus", "quota"][self.sort],
            6 => "resource",
            _ => ["node", "util", "focus", "free"][self.sort],
        };
        let mut chips = vec![Span::styled(
            format!(
                " sort:{sort_label}{} ",
                if self.reverse ^ (self.tab == 5 || (self.tab == 0 && self.sort > 0)) {
                    "↓"
                } else {
                    "↑"
                }
            ),
            Style::default().fg(MUTED),
        )];
        for (text, active) in [
            (
                format!(
                    "user:{}",
                    if self.user_filter().is_empty() {
                        "all"
                    } else {
                        self.user_filter()
                    }
                ),
                !self.user_filter().is_empty(),
            ),
            (
                format!(
                    "part:{}",
                    if self.partition.is_empty() {
                        "all"
                    } else {
                        &self.partition
                    }
                ),
                !self.partition.is_empty(),
            ),
            ("MINE".into(), self.mine),
            ("FREE".into(), self.free_only),
            (format!("search:{}", self.search), !self.search.is_empty()),
        ] {
            if active || text.starts_with("user:") || text.starts_with("part:") {
                chips.push(Span::styled(
                    format!(" {text} "),
                    Style::default()
                        .fg(if active { BACKGROUND } else { MUTED })
                        .bg(if active { FOCUS } else { BACKGROUND }),
                ));
            }
        }
        chips.push(Span::styled(" 0 reset", Style::default().fg(MUTED)));
        summary.push(Line::from(chips));
        let own_gpu = self
            .data
            .nodes
            .iter()
            .flat_map(|n| &n.gpus)
            .filter(|g| g.alloc_user == self.highlight_user)
            .count();
        let own_run = self
            .data
            .jobs
            .iter()
            .filter(|j| j.user == self.highlight_user)
            .count();
        let own_pending = self
            .data
            .pending
            .iter()
            .filter(|j| j.user == self.highlight_user)
            .count();
        let attention = crate::users::attention(
            &self.data,
            runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            runtime::now(),
        );
        let attention_count: usize = attention.iter().map(|r| r.total()).sum();
        summary[0].spans.push(Span::styled(
            format!(" · ! {attention_count} [5]"),
            Style::default().fg(if attention_count > 0 {
                Color::Yellow
            } else {
                MUTED
            }),
        ));
        summary.push(Line::from(vec![
            Span::styled(
                format!(
                    "◆ {}: {own_gpu} GPUs · {own_run} run · {own_pending} pending",
                    clipped(&self.highlight_user, 16)
                ),
                Style::default().fg(FOCUS),
            ),
            Span::styled(
                format!(" · viewer {}", clipped(&self.current_user, 12)),
                Style::default().fg(MUTED),
            ),
        ]));
        if self.data.errors.is_empty() {
            summary.push(Line::from(vec![
                Span::styled("● busy ", Style::default().fg(Color::Cyan)),
                Span::styled("○ free ", Style::default().fg(Color::Green)),
                Span::styled("! idle ", Style::default().fg(Color::Yellow)),
                Span::styled("◇ parked ", Style::default().fg(Color::Magenta)),
                Span::styled("× rogue ", Style::default().fg(Color::Red)),
                Span::styled("? unknown", Style::default().fg(MUTED)),
            ]));
        } else {
            summary.push(Line::styled(
                format!(
                    "Scheduler unavailable: {}",
                    cli::terminal_safe(&self.data.errors)
                ),
                Style::default().fg(Color::Red),
            ));
        }
        if self.trend {
            summary.extend([
                Line::styled(
                    format!("5m GPU% {}", self.spark(0)),
                    Style::default().fg(Color::Cyan),
                ),
                Line::styled(
                    format!("VRAM% {}", self.spark(1)),
                    Style::default().fg(Color::Magenta),
                ),
                Line::styled(
                    format!("GPU W {} · = unknown", self.spark(2)),
                    Style::default().fg(Color::Yellow),
                ),
            ]);
        }
        frame.render_widget(Paragraph::new(summary), areas[2]);
        if self.tab == 2 {
            let body = self
                .usage
                .as_ref()
                .map(|v| usage::render(v, 7, false))
                .unwrap_or_else(|| {
                    "No usage data yet. Start the collector to record usage.".into()
                });
            let paragraph = Paragraph::new(cli::terminal_safe(&body))
                .block(panel(" GPU-hours · observation coverage "))
                .wrap(Wrap { trim: false });
            let max = paragraph
                .line_count(areas[3].width)
                .saturating_sub(areas[3].height as usize)
                .min(u16::MAX as usize) as u16;
            self.usage_scroll = self.usage_scroll.min(max);
            frame.render_widget(paragraph.scroll((self.usage_scroll, 0)), areas[3]);
        } else {
            let visible_columns: Vec<usize> = match self.tab {
                0 if self.narrow => vec![0, 1, 2, 3, 5],
                1 if frame.area().width < 110 => vec![0, 1, 2, 3, 4, 7],
                3 if frame.area().width < 110 => vec![0, 1, 2, 3, 7, 9],
                3 if frame.area().width < 140 => vec![0, 1, 2, 3, 4, 5, 7, 9],
                4 if self.narrow => vec![0, 1, 2, 3, 4, 5, 7],
                5 if frame.area().width < 110 => vec![0, 3, 4, 5, 6, 7],
                5 if frame.area().width < 150 => vec![0, 1, 2, 3, 4, 5, 6, 7, 10],
                0 if self.details => {
                    let base = if self.compact { 6 } else { 7 };
                    let reserve = if self.compact { 110 } else { 140 };
                    let extra = (frame.area().width.saturating_sub(reserve) / 13).min(4) as usize;
                    (0..(base + extra)).collect()
                }
                _ => (0..self.headers.len()).collect(),
            };
            let widths: Vec<_> = visible_columns
                .iter()
                .map(|i| match self.tab {
                    0 if self.compact => match i {
                        0 => Constraint::Length(11),
                        1 => Constraint::Length(15),
                        2 => Constraint::Length(if self.narrow { 16 } else { 28 }),
                        3 => Constraint::Min(15),
                        4 => Constraint::Length(7),
                        _ => Constraint::Length(9),
                    },
                    0 => match i {
                        0 => Constraint::Length(15),
                        1 => Constraint::Length(18),
                        2 => Constraint::Length(15),
                        3 => Constraint::Length(28),
                        4 => Constraint::Min(20),
                        5 => Constraint::Length(8),
                        _ => Constraint::Length(12),
                    },
                    1 => match i {
                        0 => Constraint::Length(11),
                        1 => Constraint::Length(8),
                        2 => Constraint::Length(9),
                        3 => Constraint::Length(15),
                        4 => Constraint::Length(11),
                        _ => Constraint::Min(9),
                    },
                    3 => match i {
                        0 => Constraint::Length(10),
                        1 => Constraint::Length(12),
                        2 => Constraint::Length(8),
                        3 => Constraint::Length(15),
                        4..=6 => Constraint::Length(9),
                        7 => Constraint::Length(10),
                        8 => Constraint::Length(15),
                        _ => Constraint::Min(12),
                    },
                    4 => match i {
                        0 => Constraint::Length(15),
                        1..=4 => Constraint::Length(6),
                        5 => Constraint::Length(9),
                        _ => Constraint::Min(12),
                    },
                    5 if self.narrow => match i {
                        0 => Constraint::Length(12),
                        3 => Constraint::Length(9),
                        4 => Constraint::Length(8),
                        5 => Constraint::Length(13),
                        6 => Constraint::Length(15),
                        _ => Constraint::Min(8),
                    },
                    5 => match i {
                        0 => Constraint::Length(14),
                        1 => Constraint::Length(7),
                        2 => Constraint::Length(5),
                        3 => Constraint::Length(9),
                        4 => Constraint::Length(9),
                        5 => Constraint::Length(13),
                        6 => Constraint::Length(16),
                        7 => Constraint::Length(10),
                        8 => Constraint::Length(10),
                        9 => Constraint::Length(10),
                        _ => Constraint::Min(12),
                    },
                    _ => match i {
                        0 => Constraint::Length(if self.narrow { 16 } else { 24 }),
                        1 => Constraint::Length(if self.narrow { 34 } else { 48 }),
                        _ => Constraint::Min(20),
                    },
                })
                .collect();
            let inner_width = areas[3].width.saturating_sub(4);
            let cell_areas = Layout::horizontal(widths.clone())
                .spacing(1)
                .split(Rect::new(0, 0, inner_width, 1));
            let rows = self.rows.iter().enumerate().map(|(index, row)| {
                let cells = visible_columns
                    .iter()
                    .enumerate()
                    .filter_map(|(position, column)| {
                        let text = row.cells.get(*column)?;
                        let focused = row.focus == Some(*column);
                        let text = if focused {
                            format!("◆ {}", cli::terminal_safe(text))
                        } else {
                            cli::terminal_safe(text)
                        }
                        .replace('\n', " ");
                        let mut style = if focused {
                            Style::default().fg(FOCUS).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        if let Some((_, accent)) = row.accents.iter().find(|(i, _)| i == column) {
                            style = style.patch(*accent);
                        }
                        Some(
                            Cell::from(
                                Line::from(clipped(&text, cell_areas[position].width))
                                    .alignment(column_alignment(self.tab, self.compact, *column)),
                            )
                            .style(style),
                        )
                    });
                let style = Style::default()
                    .bg(if index % 2 == 0 { BACKGROUND } else { PANEL })
                    .patch(row.style);
                Row::new(cells)
                    .style(if row.focus.is_some() {
                        style.bg(FOCUS_BG)
                    } else {
                        style
                    })
                    .top_margin(u16::from(
                        self.tab == 0 && index > 0 && row.key.starts_with("node:"),
                    ))
            });
            let table = Table::new(rows, widths)
                .header(
                    Row::new(
                        visible_columns
                            .iter()
                            .enumerate()
                            .filter_map(|(position, i)| {
                                self.headers.get(*i).map(|text| {
                                    Cell::from(
                                        Line::from(clipped(text, cell_areas[position].width))
                                            .alignment(column_alignment(
                                                self.tab,
                                                self.compact,
                                                *i,
                                            )),
                                    )
                                })
                            }),
                    )
                    .style(
                        Style::default()
                            .fg(Color::Cyan)
                            .bg(PANEL)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .column_spacing(1)
                .row_highlight_style(
                    Style::default()
                        .bg(Color::Rgb(36, 55, 76))
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ")
                .block(panel(match self.tab {
                    0 => " GPUs · Space fold · Enter inspect ",
                    1 => " Nodes · measured CPU / RAM ",
                    3 => " Jobs · ? unavailable · ~ partial coverage ",
                    4 => " Attention · sustained idle / parked allocations; unallocated processes ",
                    5 => " Users · job CPU/RAM/IO; node network/load are SHARED ",
                    _ => " Controller · live host resources and local filesystems ",
                }));
            frame.render_stateful_widget(table, areas[3], &mut self.table);
            if self.rows.is_empty() && areas[3].height > 3 {
                let message = match self.tab {
                    4 => "No confirmed sustained waste matches these filters",
                    6 => {
                        "Controller telemetry unavailable; update/start the collector on the controller"
                    }
                    _ => "No matching items · 0 reset filters",
                };
                frame.render_widget(
                    Paragraph::new(message)
                        .style(Style::default().fg(MUTED))
                        .wrap(Wrap { trim: false }),
                    Rect::new(
                        areas[3].x + 2,
                        areas[3].y + 2,
                        areas[3].width.saturating_sub(4),
                        areas[3].height.saturating_sub(3),
                    ),
                );
            }
        }
        let footer = if self.searching {
            vec![
                Line::styled(
                    format!("Search: {}▏", cli::terminal_safe(&self.search)),
                    Style::default().fg(Color::Yellow),
                ),
                Line::from("Enter apply · Esc clear"),
            ]
        } else {
            let mut keys = Vec::new();
            let mut bindings = vec![
                ("?", "Help"),
                ("/", "Search"),
                ("↵", "Detail"),
                ("m", "Mine"),
                ("0", "Reset"),
            ];
            if self.tab < 2 {
                bindings.push(("[]", "Node"));
            }
            if frame.area().width >= 110 {
                bindings.extend([("s", "Sort"), ("p", "Part"), ("r", "Refresh")]);
            }
            bindings.push(("q", "Quit"));
            for (key, label) in bindings {
                keys.push(Span::styled(
                    format!(" {key} "),
                    Style::default()
                        .bg(BORDER)
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                keys.push(Span::styled(format!("{label} "), Style::default().fg(TEXT)));
            }
            vec![
                Line::styled(
                    format!(
                        "{} · row {}/{}",
                        cli::terminal_safe(&self.status),
                        self.table.selected().map_or(0, |i| i + 1),
                        self.rows.len()
                    ),
                    Style::default().fg(MUTED),
                ),
                Line::from(keys),
            ]
        };
        frame.render_widget(Paragraph::new(footer), areas[4]);
        if let Some(popup) = &mut self.popup {
            draw_popup(frame, popup);
        }
    }
    fn open(&mut self, mut popup: Popup) {
        self.popup_sequence += 1;
        popup.token = self.popup_sequence;
        self.popup = Some(popup);
    }
    fn move_selection(&mut self, down: bool) {
        if self.tab == 2 {
            self.usage_scroll = if down {
                self.usage_scroll.saturating_add(1)
            } else {
                self.usage_scroll.saturating_sub(1)
            };
            return;
        }
        if down {
            self.table.select_next();
        } else {
            self.table.select_previous();
        }
        if let Some(i) = self.table.selected() {
            self.table
                .select(Some(i.min(self.rows.len().saturating_sub(1))));
        }
    }
    fn jump_node(&mut self, forward: bool) {
        if self.tab > 1 {
            return;
        }
        let current = self.table.selected().unwrap_or(0);
        let index = if forward {
            self.rows
                .iter()
                .enumerate()
                .find(|(i, r)| *i > current && r.key.starts_with("node:"))
                .map(|(i, _)| i)
        } else {
            self.rows
                .iter()
                .enumerate()
                .rev()
                .find(|(i, r)| *i < current && r.key.starts_with("node:"))
                .map(|(i, _)| i)
        };
        if let Some(index) = index {
            self.table.select(Some(index));
        }
    }
    fn key(&mut self, key: KeyCode, worker: &Worker) -> bool {
        if self.searching {
            match key {
                KeyCode::Esc => {
                    self.search.clear();
                    self.searching = false;
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) if !c.is_control() => self.search.push(c),
                _ => (),
            }
            self.rebuild();
            return false;
        }
        if let Some(popup) = &mut self.popup {
            if let Some(id) = popup.cancel.clone() {
                match key {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.status = if worker.send(Task::Cancel(id)) {
                            "Cancelling…"
                        } else {
                            "Worker busy"
                        }
                        .into();
                        self.popup = None;
                    }
                    KeyCode::Esc | KeyCode::Char('n') => self.popup = None,
                    _ => (),
                }
                return false;
            }
            match key {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => self.popup = None,
                KeyCode::Tab | KeyCode::Right => {
                    popup.active = (popup.active + 1) % popup.panes.len().max(1);
                    popup.scroll = 0;
                    popup.follow = popup
                        .panes
                        .get(popup.active)
                        .is_some_and(|p| p.log.is_some());
                    if popup.follow {
                        popup.scroll = u16::MAX;
                    }
                }
                KeyCode::Left | KeyCode::BackTab => {
                    popup.active =
                        (popup.active + popup.panes.len() - 1) % popup.panes.len().max(1);
                    popup.scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if popup.history.is_empty() {
                        popup.scroll = popup.scroll.saturating_add(1);
                        popup.follow = false;
                    } else {
                        popup.selected = (popup.selected + 1).min(popup.history.len() - 1);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if popup.history.is_empty() {
                        popup.scroll = popup.scroll.saturating_sub(1);
                        popup.follow = false;
                    } else {
                        popup.selected = popup.selected.saturating_sub(1);
                    }
                }
                KeyCode::PageDown if !popup.history.is_empty() => {
                    popup.selected = popup
                        .selected
                        .saturating_add(20)
                        .min(popup.history.len() - 1)
                }
                KeyCode::PageUp if !popup.history.is_empty() => {
                    popup.selected = popup.selected.saturating_sub(20)
                }
                KeyCode::End if !popup.history.is_empty() => {
                    popup.selected = popup.history.len() - 1
                }
                KeyCode::Home if !popup.history.is_empty() => popup.selected = 0,
                KeyCode::PageDown => {
                    popup.scroll = popup.scroll.saturating_add(20);
                    popup.follow = false;
                }
                KeyCode::PageUp => {
                    popup.scroll = popup.scroll.saturating_sub(20);
                    popup.follow = false;
                }
                KeyCode::End => {
                    popup.scroll = u16::MAX;
                    popup.follow = true;
                }
                KeyCode::Home => {
                    popup.scroll = 0;
                    popup.follow = false;
                }
                KeyCode::Enter => {
                    if let Some(row) = popup.history.get(popup.selected).cloned() {
                        worker.send(Task::HistoryDetail(row));
                        self.status = "Loading history detail…".into();
                    } else {
                        self.popup = None;
                    }
                }
                _ => (),
            }
            return false;
        }
        match key {
            KeyCode::Char('q') => return true,
            KeyCode::Char(c @ '1'..='7') => {
                self.tab = (c as u8 - b'1') as usize;
                self.table = TableState::default();
            }
            KeyCode::Char('g') => self.tab = 2,
            KeyCode::Char(']') => self.jump_node(true),
            KeyCode::Char('[') => self.jump_node(false),
            KeyCode::Char('0') | KeyCode::Backspace => {
                self.filter_user.clear();
                self.partition.clear();
                self.mine = false;
                self.free_only = false;
                self.search.clear();
                self.status = "Filters cleared".into();
            }
            KeyCode::Home if self.tab != 2 => {
                self.table.select((!self.rows.is_empty()).then_some(0))
            }
            KeyCode::End if self.tab != 2 => self
                .table
                .select((!self.rows.is_empty()).then_some(self.rows.len().saturating_sub(1))),
            KeyCode::Home if self.tab == 2 => self.usage_scroll = 0,
            KeyCode::End if self.tab == 2 => self.usage_scroll = u16::MAX,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(false),
            KeyCode::PageDown => {
                for _ in 0..20 {
                    self.move_selection(true);
                }
            }
            KeyCode::PageUp => {
                for _ in 0..20 {
                    self.move_selection(false);
                }
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Esc => self.search.clear(),
            KeyCode::Char('s') if matches!(self.tab, 0 | 5) => self.sort = (self.sort + 1) % 4,
            KeyCode::Char('S') if matches!(self.tab, 0 | 5) => self.reverse = !self.reverse,
            KeyCode::Char('s' | 'S') => {
                self.status = "Interactive sorting is available in GPU and Users".into()
            }
            KeyCode::Char('i') => self.free_only = !self.free_only,
            KeyCode::Char('m') => self.mine = !self.mine,
            KeyCode::Char('d') => self.details = !self.details,
            KeyCode::Char('v') => self.pending = !self.pending,
            KeyCode::Char('t') => self.trend = !self.trend,
            KeyCode::Char('u') => {
                let mut users: BTreeSet<_> = self
                    .data
                    .jobs
                    .iter()
                    .map(|j| j.user.clone())
                    .chain(self.data.pending.iter().map(|j| j.user.clone()))
                    .collect();
                users.remove(&self.highlight_user);
                let list = std::iter::once(String::new())
                    .chain(std::iter::once(self.highlight_user.clone()))
                    .chain(users)
                    .collect::<Vec<_>>();
                let i = list
                    .iter()
                    .position(|u| u == &self.filter_user)
                    .unwrap_or(0);
                self.filter_user = list[(i + 1) % list.len()].clone();
                self.mine = false;
            }
            KeyCode::Char('p') => {
                let parts: BTreeSet<_> = self
                    .data
                    .nodes
                    .iter()
                    .flat_map(|n| n.partition.split(','))
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                let list = std::iter::once(String::new())
                    .chain(parts)
                    .collect::<Vec<_>>();
                let i = list.iter().position(|p| p == &self.partition).unwrap_or(0);
                self.partition = list[(i + 1) % list.len()].clone();
            }
            KeyCode::Char(' ') => {
                if let Some(node) = self
                    .selected()
                    .filter(|r| !r.node.is_empty())
                    .map(|r| r.node.clone())
                    && !self.collapsed.remove(&node)
                {
                    self.collapsed.insert(node);
                }
            }
            KeyCode::Char('z') => {
                if self.collapsed.is_empty() {
                    self.collapsed = self.data.nodes.iter().map(|n| n.name.clone()).collect();
                } else {
                    self.collapsed.clear();
                }
            }
            KeyCode::Char('r') => {
                self.status = if worker.send(Task::Refresh) {
                    "Refreshing published telemetry…"
                } else {
                    "Worker busy"
                }
                .into();
            }
            KeyCode::Char('h') => {
                worker.send(Task::History);
                self.status = "Loading my history…".into();
            }
            KeyCode::Char('n') => {
                if let Some(id) = self.selected().and_then(|r| r.job.clone()) {
                    if self.watched.remove(&id).is_some() {
                        self.status = format!("Stopped watching #{id}");
                    } else {
                        self.watched
                            .insert(id.clone(), self.data.jobs.iter().any(|j| j.jobid == id));
                        self.status = format!("Watching #{id}");
                    }
                }
            }
            KeyCode::Char('x') => {
                if let Some(id) = self.selected().and_then(|r| r.job.clone()) {
                    let owner = self
                        .data
                        .jobs
                        .iter()
                        .find(|j| j.jobid == id)
                        .map(|j| j.user.as_str())
                        .or_else(|| {
                            self.data
                                .pending
                                .iter()
                                .find(|j| j.jobid == id)
                                .map(|j| j.user.as_str())
                        });
                    if owner == Some(&self.current_user) {
                        let mut popup = Popup::text(
                            "Confirm cancellation",
                            format!(
                                "Cancel your job #{id}?\n\ny / Enter: cancel · n / Esc: keep running"
                            ),
                        );
                        popup.cancel = Some(id);
                        self.open(popup);
                    } else {
                        self.status = "Only your own jobs can be cancelled".into();
                    }
                }
            }
            KeyCode::Enter => {
                if self.tab >= 4 {
                    self.rebuild();
                }
                if let Some(row) = self.selected()
                    && self.tab >= 4
                {
                    let key = row.key.clone();
                    let title = row.cells.first().cloned().unwrap_or_default();
                    let mut body = self
                        .headers
                        .iter()
                        .zip(&row.cells)
                        .map(|(h, c)| format!("{h}: {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Some(user) = key.strip_prefix("attention:")
                        && let Some(group) = crate::users::attention_in_partition(
                            &self.data,
                            runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
                            runtime::now(),
                            &self.partition,
                        )
                        .iter()
                        .find(|g| g.user == user)
                    {
                        body.push_str(&format!("\n\n{}\n\nIdle/parked timers require current valid observations. Missing telemetry is not classified as waste.\nSlack alerts use the existing waste_alert_hours setting.",group.details.join("\n")));
                    }
                    if let Some(user) = key.strip_prefix("user:") {
                        body.push_str("\n\nCPU/RAM/block I/O: attributed job cgroups.\nNetwork/load: entire assigned nodes, shared with other users; nodes counted once per user.\nLoad1 is the maximum host load among these nodes.\n? unavailable; ~ partial observations. Block I/O excludes cached and network-filesystem traffic.");
                        if let Some(person) = crate::users::summarize(
                            &self.data,
                            runtime::now(),
                            user,
                            &self.partition,
                        )
                        .first()
                        {
                            for name in &person.nodes {
                                if let Some(node) = self.data.nodes.iter().find(|n| n.name == *name)
                                {
                                    let fresh = !node.stale
                                        && rules::telemetry_fresh(
                                            node.telemetry_observed_at,
                                            runtime::now(),
                                        );
                                    let value = |key: &str, scale: f64| {
                                        if fresh {
                                            node.host_io
                                                .get(key)
                                                .and_then(|v| number(v))
                                                .map(|v| format!("{:.2}", v / scale))
                                                .unwrap_or_else(|| "?".into())
                                        } else {
                                            "?".into()
                                        }
                                    };
                                    body.push_str(&format!("\n\n{name} (SHARED node):\n  load 1/5/15m: {} / {} / {}\n  network RX/TX: {} / {} MiB/s\n  physical disk R/W: {} / {} MiB/s",value("load1",1.0),value("load5",1.0),value("load15",1.0),value("net_rx_bps",1048576.0),value("net_tx_bps",1048576.0),value("disk_read_bps",1048576.0),value("disk_write_bps",1048576.0)));
                                }
                            }
                        }
                        let storage_fresh =
                            crate::users::storage_fresh(&self.data.controller, runtime::now());
                        if !storage_fresh {
                            body.push_str("\n\nQuota sample unavailable or older than 5 minutes.");
                        }
                        for fs in self.data.controller["filesystems"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|_| storage_fresh)
                        {
                            let q = &fs["quotas"][user];
                            if let Some(used) = crate::model::json_number(&q["used_bytes"]) {
                                let soft = crate::model::json_number(&q["soft_bytes"])
                                    .filter(|n| *n > 0.0)
                                    .map(|n| format!("{:.1} GiB", n / 1073741824.0))
                                    .unwrap_or_else(|| "not set".into());
                                let hard = crate::model::json_number(&q["hard_bytes"])
                                    .filter(|n| *n > 0.0)
                                    .map(|n| format!("{:.1} GiB", n / 1073741824.0))
                                    .unwrap_or_else(|| "not set".into());
                                body.push_str(&format!("\n\n{}: {:.2} GiB, {} files\nsoft limit {soft}; hard limit {hard}",fs["mount"].as_str().unwrap_or("?"),used/1073741824.0,q["files"]));
                            }
                        }
                    }
                    self.open(Popup::text(&title, body));
                    return false;
                }
                if let Some(row) = self.selected() {
                    let job = row
                        .job
                        .as_ref()
                        .and_then(|id| self.data.jobs.iter().find(|j| j.jobid == *id))
                        .cloned()
                        .map(Box::new);
                    let pending = row
                        .job
                        .as_ref()
                        .and_then(|id| self.data.pending.iter().find(|j| j.jobid == *id))
                        .cloned()
                        .map(Box::new);
                    let node = self
                        .data
                        .nodes
                        .iter()
                        .find(|n| n.name == row.node)
                        .cloned()
                        .map(Box::new);
                    worker.send(Task::Detail { job, pending, node });
                    self.status = "Loading detail…".into();
                }
            }
            KeyCode::Char('w') => {
                let rows = rules::waste(
                    &self.data.nodes,
                    runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
                );
                self.open(Popup::text(
                    "Wasted GPUs",
                    rows.iter()
                        .map(|r| {
                            format!(
                                "{} GPU{} {:<8} {:>8} {} #{}",
                                r.node,
                                r.gpu,
                                r.kind,
                                rules::span(r.sec),
                                r.user,
                                r.jobid
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
            KeyCode::Char('f') => {
                let mut groups = BTreeMap::<String, (usize, usize, BTreeSet<String>)>::new();
                for node in &self.data.nodes {
                    let mut local = BTreeMap::<String, usize>::new();
                    for (gpu, class) in node.gpus.iter().zip(rules::classes(node, runtime::now())) {
                        if class == Class::Free {
                            let key = format!("{} ({} MiB)", gpu.name, gpu.mem_total);
                            *local.entry(key).or_default() += 1;
                        }
                    }
                    for (key, count) in local {
                        let entry = groups.entry(key).or_default();
                        entry.0 += count;
                        entry.1 = entry.1.max(count);
                        entry.2.insert(node.name.clone());
                    }
                }
                self.open(Popup::text(
                    "Free capacity · current observation, not a reservation",
                    groups
                        .iter()
                        .map(|(key, (total, largest, nodes))| {
                            format!(
                                "{key}: free {total}, single-node max {largest}, {} nodes\n  {}",
                                nodes.len(),
                                nodes.iter().cloned().collect::<Vec<_>>().join(",")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ));
            }
            KeyCode::Char('e') => {
                let directory = PathBuf::from("exports");
                let result = (|| -> Result<PathBuf> {
                    runtime::secure_dir(&directory, 0o755)?;
                    let path = directory.join(format!(
                        "sgpu-{}.json",
                        chrono::Local::now().format("%Y%m%d-%H%M%S")
                    ));
                    runtime::atomic_write(
                        &path,
                        &serde_json::to_vec_pretty(&self.data)?,
                        0o644,
                        false,
                    )?;
                    Ok(path)
                })();
                self.status = match result {
                    Ok(path) => format!("Exported {}", path.display()),
                    Err(error) => format!("Export failed: {error}"),
                };
            }
            KeyCode::Char('?') => self.open(Popup::text("sgpu help", HELP.into())),
            _ => (),
        }
        self.rebuild();
        false
    }
}

fn ram(node: &Node) -> String {
    let total = number(&node.mem_total);
    let used = total
        .zip(number(&node.mem_avail).or_else(|| number(&node.mem_free)))
        .map(|(t, a)| (t - a).clamp(0.0, t))
        .or_else(|| number(&node.mem_alloc));
    total
        .zip(used)
        .map(|(t, u)| {
            format!(
                "{}{:.0}/{:.0}G",
                if node.mem_avail.is_empty() && node.mem_free.is_empty() {
                    "~"
                } else {
                    ""
                },
                u / 1024.0,
                t / 1024.0
            )
        })
        .unwrap_or_else(|| "?".into())
}
fn draw_popup(frame: &mut Frame, popup: &mut Popup) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).max(area.width.min(3)).min(140);
    let height = area.height.saturating_sub(4).max(area.height.min(3));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} · Esc close ",
        cli::terminal_safe(&popup.title)
    ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if !popup.history.is_empty() {
        let widths = [
            Constraint::Length(14),
            Constraint::Length(17),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Min(20),
        ];
        let rows = popup.history.iter().map(|row| {
            Row::new([
                row.jobid.clone(),
                row.state.clone(),
                row.exit.clone(),
                rules::span(row.elapsed as u64),
                row.partition.clone(),
                cli::terminal_safe(&row.name),
            ])
            .style(Style::default().fg(if row.exit != "0:0" {
                Color::Yellow
            } else {
                Color::White
            }))
        });
        let table = Table::new(rows, widths)
            .header(Row::new([
                "JobID",
                "State",
                "Exit",
                "Elapsed",
                "Partition",
                "Name",
            ]))
            .row_highlight_style(Style::default().bg(Color::Rgb(40, 50, 60)));
        let mut state = TableState::default().with_selected(Some(popup.selected));
        frame.render_stateful_widget(table, inner, &mut state);
        return;
    }
    let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    frame.render_widget(
        Tabs::new(popup.panes.iter().map(|p| p.name.clone()))
            .select(popup.active)
            .highlight_style(Style::default().fg(Color::Cyan)),
        sections[0],
    );
    if let Some(pane) = popup.panes.get(popup.active) {
        let safe = cli::terminal_safe(&pane.body);
        let lines = safe
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(
                        if ["Error", "ERROR", "Traceback", "Exception", "OOM", "FAILED"]
                            .iter()
                            .any(|s| line.contains(s))
                        {
                            Color::Red
                        } else {
                            Color::White
                        },
                    ),
                ))
            })
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let max = paragraph
            .line_count(sections[1].width)
            .saturating_sub(sections[1].height as usize)
            .min(u16::MAX as usize) as u16;
        popup.scroll = popup.scroll.min(max);
        frame.render_widget(paragraph.scroll((popup.scroll, 0)), sections[1]);
    }
}

fn ime(key: KeyCode) -> KeyCode {
    if let KeyCode::Char(c) = key {
        let mapped = match c {
            'ㅂ' => 'q',
            'ㄱ' => 'r',
            'ㄴ' => 's',
            'ㅕ' => 'u',
            'ㅔ' => 'p',
            'ㅡ' => 'm',
            'ㅑ' => 'i',
            'ㅇ' => 'd',
            'ㄷ' => 'e',
            'ㅈ' => 'w',
            'ㅗ' => 'h',
            'ㅜ' => 'n',
            'ㅎ' => 'g',
            'ㅍ' => 'v',
            'ㅅ' => 't',
            'ㄹ' => 'f',
            'ㅓ' => 'j',
            'ㅏ' => 'k',
            _ => c,
        };
        KeyCode::Char(mapped)
    } else {
        key
    }
}

pub fn run(highlight_user: Option<String>) -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive mode requires a terminal; use sgpu once or sgpu json".into());
    }
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<i32> {
        let mut app = App::new();
        if let Some(user) = highlight_user {
            app.set_highlight_user(user);
        }
        let worker = Worker::new();
        worker.send(Task::Refresh);
        let mut next = Instant::now()
            + Duration::from_secs_f64(runtime::env_seconds("SLURM_GPU_TUI_REFRESH_SEC", 3.0, 0.1));
        let mut refresh_inflight = true;
        let mut logs_inflight = false;
        let mut next_logs = Instant::now();
        let mut dirty = true;
        loop {
            while let Ok(answer) = worker.receiver.try_recv() {
                match answer {
                    Answer::Snapshot(result) => {
                        refresh_inflight = false;
                        match *result {
                            Ok((data, usage)) => {
                                app.usage = usage;
                                app.apply(data);
                                if app.status.starts_with("Loading")
                                    || app.status.starts_with("Refreshing")
                                {
                                    app.status = "Published telemetry loaded".into();
                                }
                            }
                            Err(error) => {
                                app.status = format!("Snapshot unavailable: {error}");
                                app.data.errors = error.to_string();
                                for node in &mut app.data.nodes {
                                    node.stale = true;
                                    node.scheduler_available = false;
                                }
                                app.rebuild();
                            }
                        }
                    }
                    Answer::Popup(result) => match result {
                        Ok(popup) => app.open(popup),
                        Err(error) => app.status = error.to_string(),
                    },
                    Answer::Status(status) => {
                        app.status = status;
                        if !refresh_inflight {
                            refresh_inflight = worker.send(Task::Refresh);
                        }
                    }
                    Answer::Logs { token, values } => {
                        logs_inflight = false;
                        if let Some(popup) = app.popup.as_mut().filter(|p| p.token == token) {
                            for (index, body) in values {
                                if let Some(pane) = popup.panes.get_mut(index) {
                                    pane.body = body;
                                }
                            }
                            if popup.follow {
                                popup.scroll = u16::MAX;
                            }
                        }
                    }
                }
                dirty = true;
            }
            if Instant::now() >= next && !refresh_inflight {
                refresh_inflight = worker.send(Task::Refresh);
                next = Instant::now()
                    + Duration::from_secs_f64(runtime::env_seconds(
                        "SLURM_GPU_TUI_REFRESH_SEC",
                        3.0,
                        0.1,
                    ));
            }
            if Instant::now() >= next_logs && !logs_inflight {
                if let Some(popup) = &app.popup {
                    let views = popup
                        .panes
                        .iter()
                        .enumerate()
                        .filter_map(|(i, p)| p.log.clone().map(|v| (i, v)))
                        .collect::<Vec<_>>();
                    if !views.is_empty() {
                        logs_inflight = worker.send(Task::Logs {
                            token: popup.token,
                            views,
                        });
                    }
                }
                next_logs = Instant::now() + Duration::from_secs(3);
            }
            if dirty {
                terminal.draw(|frame| app.draw(frame))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('c')
                        {
                            return Ok(0);
                        }
                        let code = if app.searching {
                            key.code
                        } else {
                            ime(key.code)
                        };
                        if app.key(code, &worker) {
                            return Ok(0);
                        }
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => (),
                }
            }
        }
    })();
    ratatui::restore();
    result
}

const HELP: &str = concat!(
    "1/2/3/4  GPU / CPU / Usage / Jobs\n5/6/7  Attention / Users / Controller\n",
    "g Usage · v pending · t 5-minute cluster trend\nr refresh published data\n",
    "s sort · S reverse · [ / ] previous/next node · Home/End first/last row\n",
    "u user filter (highlight first) · p partition · m highlighted user's jobs\n",
    "i free GPUs · 0 reset all filters · / search · Esc clear search\n",
    "d detail columns · Space fold node · z fold/unfold all\nj/k or arrows move\n",
    "Enter full values or job/node detail · Tab info/script/stdout/stderr\n",
    "w wasted GPUs · f free capacity · h my job history (PageUp/Down supported)\n",
    "n watch job · x cancel my own job (confirmation required)\ne export JSON · ? help · q quit\n\n",
    "◆ + teal background: highlighted account. Cursor selection is separate.\n",
    "--highlight-user changes display/filtering; OS ownership still controls cancel/private logs.\n",
    "Util: dim → blue → cyan → green as activity rises.\n",
    "VRAM/RAM: blue → cyan → yellow → orange → red as memory fills.\n",
    "Attention groups current idle/parked allocations and unallocated GPU processes.\n",
    "Timer age includes observation gaps; inspect evidence before acting.\n",
    "Users: job CPU/RAM/block I/O; node network and load are SHARED.\n",
    "Controller: local resources, physical device rates, filesystems and available quotas.\n",
    "Unknown/stale data is never free capacity. ? unavailable; ~ partial.\n",
    "GPU/VRAM meters sample GPUs; CPU/RAM meters sample nodes. CPU is node-wide.\n",
    "Job RAM includes cgroup cache. Summed peaks are not simultaneous whole-job peaks.\n",
    "Block I/O excludes cached and network-filesystem traffic; rates need two samples.\n",
    "Network/load are not attributed to individual users. Storage queries run in the background.\n",
    "Pending start and capacity are estimates, not scheduler guarantees.\n",
    "PSI is pressure, not a diagnosis. GPU power caps alone are informational.\n\n",
    "Node results are push-only; missing agents stay stale without master-to-node requests."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Gpu;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn user_details_expire_quota_with_the_table_even_without_a_new_snapshot() {
        let now = runtime::now();
        let mut app = App::for_user("alice".into());
        app.tab = 5;
        app.apply(Snapshot {
            jobs: vec![Job {
                jobid: "42".into(),
                user: "alice".into(),
                node: "gpu1".into(),
                ..Default::default()
            }],
            controller: serde_json::json!({
                "observed_at": now,
                "storage_observed_at": now,
                "filesystems": [{
                    "mount": "/fixture-storage",
                    "quotas": {"alice": {
                        "used_bytes": 1073741824,
                        "files": 12,
                        "soft_bytes": 2147483648_u64,
                        "hard_bytes": 3221225472_u64
                    }}
                }]
            }),
            ..Default::default()
        });
        let worker = Worker::new();
        assert_eq!(app.rows[0].cells[8], "1.0");
        app.key(KeyCode::Enter, &worker);
        let body = &app.popup.as_ref().unwrap().panes[0].body;
        assert!(body.contains("soft limit 2.0 GiB"));
        app.popup = None;
        app.data.controller["storage_observed_at"] = serde_json::json!(now - 400.0);
        app.key(KeyCode::Enter, &worker);
        assert_eq!(app.rows[0].cells[8], "?");
        let body = &app.popup.as_ref().unwrap().panes[0].body;
        assert!(body.contains("Quota sample unavailable or older than 5 minutes"));
        assert!(!body.contains("soft limit"));
        assert!(!body.contains("/fixture-storage"));
    }
    #[test]
    fn focus_colors_node_spacing_and_navigation_preserve_real_rows_and_permissions() {
        let now = runtime::now();
        let mut app = App::for_user("root".into());
        app.set_highlight_user("alice".into());
        let owned = Gpu {
            index: "0".into(),
            alloc_user: "alice".into(),
            alloc_jobid: "42".into(),
            users: vec!["alice".into()],
            util: "80".into(),
            mem_used: "95".into(),
            mem_total: "100".into(),
            temp: "90".into(),
            health_observed_at: now,
            ..Default::default()
        };
        app.apply(Snapshot {
            nodes: vec![
                Node {
                    name: "gpu1".into(),
                    state: "allocated".into(),
                    observed_at: now,
                    gpus: vec![owned],
                    ..Default::default()
                },
                Node {
                    name: "gpu2".into(),
                    state: "idle".into(),
                    observed_at: now,
                    gpus: vec![Gpu {
                        index: "0".into(),
                        util: "0".into(),
                        mem_used: "0".into(),
                        mem_total: "100".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            jobs: vec![Job {
                jobid: "42".into(),
                user: "alice".into(),
                node: "gpu1".into(),
                ..Default::default()
            }],
            pending: vec![PendingJob {
                jobid: "43".into(),
                user: "alice".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let worker = Worker::new();
        app.table.select(Some(1));
        let before = app.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            before,
            app.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>()
        );
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "◆" && c.fg == FOCUS)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "━" && c.fg == Color::Green)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "━" && c.fg == Color::Rgb(255, 166, 87))
        );
        let lines = buffer
            .content
            .chunks(160)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let second = lines
            .iter()
            .position(|line| line.contains("▼ gpu2"))
            .unwrap();
        assert!(lines[second - 1].trim_matches(['│', ' ']).is_empty());
        let hot = lines
            .iter()
            .position(|line| line.contains("HOT") && line.contains("#42"))
            .unwrap();
        let x = lines[hot].chars().position(|c| c == 'H').unwrap();
        assert_eq!(buffer[(x as u16, hot as u16)].fg, Color::Red);
        app.key(KeyCode::Char('x'), &worker);
        assert!(app.popup.is_none());
        assert_eq!(app.current_user, "root");
        app.key(KeyCode::Char(']'), &worker);
        assert_eq!(app.selected().unwrap().key, "node:gpu2");
        app.key(KeyCode::Char('['), &worker);
        assert_eq!(app.selected().unwrap().key, "node:gpu1");
        app.filter_user = "alice".into();
        app.partition = "gpu".into();
        app.search = "x".into();
        app.mine = true;
        app.free_only = true;
        app.sort = 2;
        app.collapsed.insert("gpu2".into());
        app.key(KeyCode::Char('0'), &worker);
        assert!(
            app.filter_user.is_empty()
                && app.partition.is_empty()
                && app.search.is_empty()
                && !app.mine
                && !app.free_only
        );
        assert_eq!(app.sort, 2);
        assert!(app.collapsed.contains("gpu2"));
        app.tab = 3;
        app.rebuild();
        assert!(app.rows.iter().all(|r| r.focus == Some(1)));
    }
    #[test]
    fn history_pagination_moves_selection_and_color_ranges_keep_unknown_distinct() {
        let mut app = App::for_user("alice".into());
        let mut popup = Popup::text("History", String::new());
        popup.history = (0..45)
            .map(|id| history::Record {
                jobid: id.to_string(),
                ..Default::default()
            })
            .collect();
        app.open(popup);
        let worker = Worker::new();
        app.key(KeyCode::PageDown, &worker);
        assert_eq!(app.popup.as_ref().unwrap().selected, 20);
        app.key(KeyCode::End, &worker);
        assert_eq!(app.popup.as_ref().unwrap().selected, 44);
        app.key(KeyCode::PageUp, &worker);
        assert_eq!(app.popup.as_ref().unwrap().selected, 24);
        app.key(KeyCode::Home, &worker);
        assert_eq!(app.popup.as_ref().unwrap().selected, 0);
        assert_eq!(MetricKind::Activity.color(Some(80.0)), Color::Green);
        assert_eq!(MetricKind::Activity.color(Some(0.0)), MUTED);
        assert_eq!(MetricKind::Memory.color(Some(98.0)), Color::Red);
        assert_eq!(MetricKind::Memory.color(None), MUTED);
        assert_eq!(MetricKind::Memory.color(Some(f64::NAN)), MUTED);
        assert_eq!(clipped("가나다라마바사", 5).width(), 5);
    }
    #[test]
    fn meters_keep_unknown_distinct_from_zero_and_bound_only_the_graph() {
        let mut meter = Meter::default();
        meter.push(None);
        assert_eq!(meter.value(), None);
        assert_eq!(meter.label(), "? unavailable");
        meter.push(Some((0.0, 100.0)));
        assert_eq!(meter.value(), Some(0.0));
        assert_eq!(meter.label(), "~0% · 1/2");
        assert_eq!(mini_meter(None), "?");
        assert_eq!(mini_meter(Some(f64::NAN)), "?");
        assert!(mini_meter(Some(0.0)).contains("0%"));
        assert!(mini_meter(Some(125.0)).contains("125%"));
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        meter.push(Some((250.0, 100.0)));
        terminal
            .draw(|frame| meter.draw(frame, frame.area(), "GPU", MetricKind::Activity))
            .unwrap();
    }
    #[test]
    fn dashboard_excludes_stale_samples_and_preserves_selection_on_resize() {
        let now = runtime::now();
        let fresh = Node {
            name: "gpu1".into(),
            state: "idle".into(),
            observed_at: now,
            telemetry_observed_at: now,
            cpu_util: "20".into(),
            cpus: "8".into(),
            mem_total: "100".into(),
            mem_used: "50".into(),
            gpus: vec![Gpu {
                index: "0".into(),
                util: "0".into(),
                mem_used: "0".into(),
                mem_total: "100".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut stale = fresh.clone();
        stale.name = "gpu2".into();
        stale.stale = true;
        stale.cpu_util = "100".into();
        stale.mem_used = "100".into();
        stale.gpus[0].util = "100".into();
        stale.gpus[0].mem_used = "100".into();
        let mut app = App::for_user("fixture".into());
        app.apply(Snapshot {
            nodes: vec![fresh, stale],
            ..Default::default()
        });
        app.table.select(Some(1));
        let selected = app.selected().unwrap().key.clone();
        for width in [80, 160, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_eq!(app.selected().unwrap().key, selected);
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("GPU 1/2 FREE"));
            assert!(text.contains("~0% · 1/2"));
            assert!(text.contains("~20% · 1/2"));
            assert!(text.contains("~50% · 1/2"));
            assert!(text.contains("? unknown"));
        }
    }
    #[test]
    fn all_tabs_render_at_narrow_and_wide_widths() {
        let mut app = App::new();
        app.apply(Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                state: "idle".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    name: "H100".into(),
                    util: "0".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        });
        for width in [80, 120, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            for tab in 0..=6 {
                app.tab = tab;
                app.rebuild();
                terminal.draw(|f| app.draw(f)).unwrap();
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .any(|c| c.symbol() == "G")
                );
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains(
                    [
                        "Node / GPU",
                        "CPU alloc",
                        "GPU-hours",
                        "Req GPU/CPU/RAM",
                        "User",
                        "User",
                        "Resource"
                    ][tab]
                ));
            }
        }
    }
    #[test]
    fn missing_metrics_are_displayed_as_unknown_and_keyboard_filters_work() {
        let mut app = App::new();
        app.apply(Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    util: "NaN".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(app.rows.iter().any(|r| r.cells.iter().any(|c| c == "?")));
        let worker = Worker::new();
        app.key(KeyCode::Char('i'), &worker);
        assert!(app.rows.is_empty());
        assert_eq!(ime(KeyCode::Char('ㅂ')), KeyCode::Char('q'));
    }
    #[test]
    fn gpu_filters_apply_to_cards_and_summary_even_when_collapsed() {
        let mut app = App::for_user("alice".into());
        app.apply(Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                state: "idle".into(),
                gpus: vec![
                    Gpu {
                        index: "0".into(),
                        util: "0".into(),
                        mem_used: "0".into(),
                        ..Default::default()
                    },
                    Gpu {
                        index: "1".into(),
                        util: "80".into(),
                        alloc_user: "alice".into(),
                        alloc_jobid: "42".into(),
                        users: vec!["alice".into()],
                        pids: vec!["1".into()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        });
        app.free_only = true;
        app.rebuild();
        assert_eq!(app.rows.len(), 2);
        assert!(app.rows.iter().all(|r| r.job.is_none()));
        app.collapsed.insert("gpu1".into());
        app.rebuild();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("GPU 1/1 FREE"));
        app.free_only = false;
        app.collapsed.clear();
        app.filter_user = "alice".into();
        app.rebuild();
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.rows[1].job.as_deref(), Some("42"));
    }
    #[test]
    fn wrapped_popups_and_usage_scroll_and_tiny_terminals_render() {
        let mut app = App::for_user("fixture".into());
        app.open(Popup::text(
            "wrapped",
            format!("{} END_MARKER", "longword ".repeat(400)),
        ));
        app.popup.as_mut().unwrap().scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.popup.as_ref().unwrap().scroll > 0);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("END_MARKER"));
        app.popup = None;
        app.tab = 2;
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let users = (0..80)
            .map(|i| {
                (
                    format!("user{i:02}"),
                    serde_json::json!({"alloc":3600,"busy":1800,"observed_alloc":3600}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        app.usage = Some(serde_json::json!({"days":{day:users}}));
        app.usage_scroll = u16::MAX;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.usage_scroll > 0);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("user79"));
        app.open(Popup::text("tiny", "text".into()));
        for (w, h) in [(1, 1), (2, 2), (20, 3)] {
            Terminal::new(TestBackend::new(w, h))
                .unwrap()
                .draw(|f| app.draw(f))
                .unwrap();
        }
    }
}
