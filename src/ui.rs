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
    layout::{Constraint, Layout, Rect},
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

const BACKGROUND: Color = Color::Rgb(13, 17, 23);
const PANEL: Color = Color::Rgb(22, 27, 34);
const BORDER: Color = Color::Rgb(48, 58, 70);
const TEXT: Color = Color::Rgb(220, 225, 232);
const MUTED: Color = Color::Rgb(145, 155, 170);

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
    let filled = (value.clamp(0.0, 100.0) / 20.0).round() as usize;
    format!(
        "{}{} {value:>3.0}%",
        "━".repeat(filled),
        "·".repeat(5 - filled)
    )
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
    fn draw(&self, frame: &mut Frame, area: Rect, name: &str, color: Color) {
        let columns = Layout::horizontal([Constraint::Length(9), Constraint::Min(1)]).split(area);
        frame.render_widget(
            Paragraph::new(name).style(Style::default().fg(MUTED)),
            columns[0],
        );
        frame.render_widget(
            Gauge::default()
                .ratio(self.value().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0)
                .label(self.label())
                .use_unicode(true)
                .gauge_style(
                    Style::default()
                        .fg(if self.value().is_some() { color } else { MUTED })
                        .bg(BACKGROUND),
                ),
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
}
pub struct App {
    pub data: Snapshot,
    pub usage: Option<serde_json::Value>,
    pub tab: usize,
    pub collapsed: BTreeSet<String>,
    pub compact: bool,
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
    pub popup: Option<Popup>,
    status: String,
    watched: BTreeMap<String, bool>,
    job_baseline: Option<BTreeSet<String>>,
    own_baseline: BTreeSet<String>,
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
            current_user,
            popup: None,
            status: "Loading published telemetry…".into(),
            watched: BTreeMap::new(),
            job_baseline: None,
            own_baseline: BTreeSet::new(),
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
    }
    fn selected(&self) -> Option<&ViewRow> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }
    fn user_filter(&self) -> &str {
        if self.mine {
            &self.current_user
        } else {
            &self.filter_user
        }
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
                                    g.alloc_user == self.current_user
                                        || g.users.contains(&self.current_user)
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
                        .map(|_| {
                            format!(
                                "{}{:.0}/{:.0}G",
                                if group_vram.samples < group_vram.total {
                                    "~"
                                } else {
                                    ""
                                },
                                group_vram.used / 1024.0,
                                group_vram.capacity / 1024.0
                            )
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
                        style: Style::default()
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
                            .map(|(u, t)| format!("{:.1}/{:.0}G", u / 1024.0, t / 1024.0))
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
                        let style = if gpu.alloc_user == self.current_user
                            || gpu.users.contains(&self.current_user)
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
            _ => {
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
                        style: Style::default().fg(if job.user == self.current_user {
                            Color::Cyan
                        } else {
                            Color::White
                        }),
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
                            style: Style::default().fg(Color::Yellow),
                        });
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
        let meters = Layout::vertical([Constraint::Length(1); 4]).split(inner);
        for (index, (name, meter, color)) in [
            ("GPU util", &gpu_util, Color::Cyan),
            ("VRAM", &vram, Color::Magenta),
            ("CPU busy", &cpu, Color::Green),
            ("RAM used", &memory, Color::Blue),
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
        if compact != self.compact {
            self.compact = compact;
            self.rebuild();
        }
        frame.render_widget(
            Block::default().style(Style::default().fg(TEXT).bg(BACKGROUND)),
            frame.area(),
        );
        let dashboard_height = if frame.area().height >= 20 && frame.area().width >= 70 {
            6
        } else {
            0
        };
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(dashboard_height),
            Constraint::Length(if self.trend { 6 } else { 3 }),
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
            Tabs::new(["1 GPU", "2 CPU", "3 Usage", "4 Jobs"])
                .select(self.tab)
                .style(Style::default().fg(MUTED))
                .divider(Span::styled(" │ ", Style::default().fg(BORDER)))
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
                " · cluster: {} running · {} pending · {fresh}/{} nodes fresh",
                self.data.jobs.len(),
                self.data.pending.len(),
                self.data.nodes.len()
            )),
        ])];
        let scope = format!(
            "user={} partition={} sort={}{}",
            if self.user_filter().is_empty() {
                "all"
            } else {
                self.user_filter()
            },
            if self.partition.is_empty() {
                "all"
            } else {
                &self.partition
            },
            ["node", "util", "user", "free"][self.sort],
            if self.reverse { " ↓" } else { "" }
        );
        summary.push(Line::styled(scope, Style::default().fg(MUTED)));
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
                1 if frame.area().width < 110 => vec![0, 1, 2, 3, 4, 7],
                3 if frame.area().width < 110 => vec![0, 1, 2, 3, 7, 9],
                3 if frame.area().width < 140 => vec![0, 1, 2, 3, 4, 5, 7, 9],
                0 if self.details => {
                    let base = if self.compact { 6 } else { 7 };
                    let reserve = if self.compact { 78 } else { 112 };
                    let extra = (frame.area().width.saturating_sub(reserve) / 13).min(4) as usize;
                    (0..(base + extra)).collect()
                }
                _ => (0..self.headers.len()).collect(),
            };
            let widths: Vec<_> = visible_columns
                .iter()
                .copied()
                .map(|i| {
                    if self.tab == 0 {
                        if self.compact {
                            match i {
                                0 => Constraint::Length(12),
                                1 => Constraint::Length(10),
                                2 => Constraint::Length(12),
                                3 => Constraint::Min(12),
                                4 => Constraint::Length(7),
                                _ => Constraint::Length(10),
                            }
                        } else {
                            match i {
                                0 => Constraint::Length(15),
                                1 => Constraint::Length(20),
                                2 => Constraint::Length(10),
                                3 => Constraint::Length(14),
                                4 => Constraint::Min(18),
                                5 => Constraint::Length(8),
                                _ => Constraint::Length(12),
                            }
                        }
                    } else if self.tab == 1 {
                        match i {
                            0 => Constraint::Length(12),
                            1 => Constraint::Length(9),
                            2 => Constraint::Length(10),
                            3 => Constraint::Length(10),
                            4 => Constraint::Length(13),
                            _ => Constraint::Min(8),
                        }
                    } else {
                        match i {
                            0 => Constraint::Length(10),
                            1 => Constraint::Length(12),
                            2 => Constraint::Length(8),
                            3 => Constraint::Length(15),
                            4..=6 => Constraint::Length(9),
                            7 => Constraint::Length(10),
                            8 => Constraint::Length(15),
                            _ => Constraint::Min(12),
                        }
                    }
                })
                .collect();
            let rows = self.rows.iter().enumerate().map(|(index, row)| {
                Row::new(
                    visible_columns
                        .iter()
                        .filter_map(|i| row.cells.get(*i))
                        .map(|s| Cell::from(cli::terminal_safe(s).replace('\n', " "))),
                )
                .style(
                    Style::default()
                        .bg(if index % 2 == 0 { BACKGROUND } else { PANEL })
                        .patch(row.style),
                )
            });
            let table = Table::new(rows, widths)
                .header(
                    Row::new(
                        visible_columns
                            .iter()
                            .filter_map(|i| self.headers.get(*i))
                            .cloned(),
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
                    _ => " Jobs · ? unavailable · ~ partial coverage ",
                }));
            frame.render_stateful_widget(table, areas[3], &mut self.table);
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
            for (key, label) in [
                ("?", "Help"),
                ("/", "Search"),
                ("↵", "Detail"),
                ("s", "Sort"),
                ("u", "User"),
                ("p", "Part"),
                ("r", "Refresh"),
                ("q", "Quit"),
            ] {
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
                Line::styled(cli::terminal_safe(&self.status), Style::default().fg(MUTED)),
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
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = (c as u8 - b'1') as usize;
                self.table = TableState::default();
            }
            KeyCode::Char('g') => self.tab = 2,
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
            KeyCode::Char('s') => self.sort = (self.sort + 1) % 4,
            KeyCode::Char('S') => self.reverse = !self.reverse,
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
                users.remove(&self.current_user);
                let list = std::iter::once(String::new())
                    .chain(std::iter::once(self.current_user.clone()))
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

pub fn run() -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive mode requires a terminal; use sgpu once or sgpu json".into());
    }
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<i32> {
        let mut app = App::new();
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

const HELP: &str = "1/2/3/4  GPU / CPU / Usage / Jobs\ng Usage · v pending · t 5-minute trend\nr refresh published data\ns sort: node → utilization → user → free · S reverse\nu user filter (me first) · p partition · m my jobs · i free GPUs\nd detail columns · Space fold node · z fold/unfold all\n/ search node/user/job · Esc clear search\nj/k or arrows move cursor\nEnter job/node detail · Tab info/script/stdout/stderr\nw wasted GPUs · f free capacity by model/VRAM\nh my recent job history · Enter history details\nn watch any job for start/end\nx cancel my own job (confirmation required)\ne export JSON · ? help · q quit\n\nUnknown/stale observations are not free capacity.\nMeters show measured usage: GPU/VRAM sample GPUs; CPU/RAM sample nodes.\n~ means partial sampling; ? means no fresh observation. CPU is node-wide.\nJobs: ? unavailable; ~ partial coverage. RAM includes cgroup cache.\nSummed per-node memory peaks are not a simultaneous whole-job peak.\nPending start times and capacity are estimates, not scheduler guarantees.\nPSI is pressure, not a diagnosis. GPU power caps alone are informational.\n\nNode results are push-only. Missing agents stay stale; the master never polls or repairs nodes.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Gpu;
    use ratatui::{Terminal, backend::TestBackend};
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
            .draw(|frame| meter.draw(frame, frame.area(), "GPU", Color::Cyan))
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
    fn all_four_tabs_render_at_narrow_and_wide_widths() {
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
            for tab in [0, 1, 2, 3] {
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
                assert!(
                    text.contains(["Node / GPU", "CPU alloc", "GPU-hours", "Req GPU/CPU/RAM"][tab])
                );
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
