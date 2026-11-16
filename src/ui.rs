use crate::{
    Result, cli, history,
    job_view::{self, Entry},
    model::{Gpu, Job, Node, PendingJob, Snapshot, number},
    process::{self, Program},
    rules::{self, Class},
    runtime,
    ui_input::{Action as Click, Hits, Scope},
    ui_preferences, usage,
};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, HighlightSpacing, Paragraph, Row, Table,
        TableState, Wrap,
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

const BACKGROUND: Color = Color::Rgb(15, 20, 30);
const PANEL: Color = Color::Rgb(22, 29, 42);
const BORDER: Color = Color::Rgb(61, 78, 101);
const TEXT: Color = Color::Rgb(226, 232, 240);
const MUTED: Color = Color::Rgb(158, 173, 194);
const BLUE: Color = Color::Rgb(126, 176, 255);
const CYAN: Color = Color::Rgb(103, 212, 235);
const GREEN: Color = Color::Rgb(115, 215, 158);
const YELLOW: Color = Color::Rgb(243, 202, 115);
const ORANGE: Color = Color::Rgb(255, 173, 105);
const RED: Color = Color::Rgb(255, 128, 142);
const PURPLE: Color = Color::Rgb(196, 163, 245);
const FOCUS: Color = Color::Rgb(112, 224, 211);
const FOCUS_BG: Color = Color::Rgb(22, 52, 58);
const HEADER_BG: Color = Color::Rgb(29, 42, 60);
const SELECTED_BG: Color = Color::Rgb(35, 53, 76);

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
            Self::Activity if value < 30.0 => BLUE,
            Self::Activity if value < 70.0 => CYAN,
            Self::Activity => GREEN,
            Self::Memory if value < 50.0 => BLUE,
            Self::Memory if value < 80.0 => CYAN,
            Self::Memory if value < 90.0 => YELLOW,
            Self::Memory if value < 97.0 => ORANGE,
            Self::Memory => RED,
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
        3 => column == 0 || matches!(column, 3 | 4 | 6..=9 | 12),
        4 => matches!(column, 1..=5),
        5 => matches!(column, 1..=9),
        6 => matches!(column, 1 | 2),
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

fn buttons(frame: &mut Frame, area: Rect, bindings: &[(KeyCode, String)], hits: &mut Hits) {
    let mut x = area.x;
    for (index, (key, label)) in bindings.iter().enumerate() {
        let text = if area.width < 32 {
            label.clone()
        } else {
            format!(" {label} ")
        };
        let width = text.width().min(u16::MAX as usize) as u16;
        let remaining = area.right().saturating_sub(x);
        if width > remaining && index > 0 {
            continue;
        }
        let rect = Rect::new(x, area.y, width.min(remaining), area.height.min(1));
        frame.render_widget(
            Paragraph::new(text).style(
                Style::default()
                    .fg(CYAN)
                    .bg(HEADER_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            rect,
        );
        hits.add(rect, Click::Key(*key));
        x = rect.right().saturating_add(1).min(area.right());
    }
}

fn tab_bar(
    frame: &mut Frame,
    area: Rect,
    labels: &[String],
    selected: usize,
    hits: &mut Hits,
    panes: bool,
) {
    if panes
        && labels
            .iter()
            .map(|label| label.width() + 3)
            .sum::<usize>()
            .saturating_sub(1)
            > area.width as usize
    {
        let [arrows, name] =
            Layout::horizontal([Constraint::Length(6), Constraint::Min(0)]).areas(area);
        buttons(
            frame,
            arrows,
            &[(KeyCode::BackTab, "←".into()), (KeyCode::Tab, "→".into())],
            hits,
        );
        if let Some(label) = labels.get(selected) {
            frame.render_widget(
                Paragraph::new(cli::terminal_safe(label)).style(Style::default().fg(FOCUS)),
                name,
            );
        }
        return;
    }
    let mut spans = Vec::new();
    let mut actions = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("│", Style::default().fg(BORDER)));
            actions.push(None);
        }
        spans.push(Span::styled(
            format!(" {} ", cli::terminal_safe(label)),
            if index == selected {
                Style::default()
                    .fg(BACKGROUND)
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            },
        ));
        actions.push(Some(if panes {
            Click::Pane(index)
        } else {
            Click::Key(KeyCode::Char(b'1'.saturating_add(index as u8) as char))
        }));
    }
    hits.spans(area, &spans, &actions);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn view_column_sort(tab: usize, compact: bool, column: usize) -> Option<usize> {
    match (tab, column) {
        (0, 0) => Some(0),
        (0, 1) if compact => Some(1),
        (0, 2) if !compact => Some(1),
        (5, 2) => Some(0),
        (5, 3) => Some(1),
        (5, 8) => Some(3),
        _ => None,
    }
}

fn job_column_sort(column: usize) -> Option<job_view::Sort> {
    match column {
        1 => Some(job_view::Sort::User),
        3 => Some(job_view::Sort::Gpu),
        4 => Some(job_view::Sort::Cpu),
        12 => Some(job_view::Sort::Priority),
        _ => None,
    }
}
fn gpu_mark(class: Class) -> (&'static str, &'static str, Color) {
    match class {
        Class::Busy => ("●", "busy", CYAN),
        Class::Free => ("○", "free", GREEN),
        Class::Idle => ("!", "idle", YELLOW),
        Class::Parked => ("◇", "parked", PURPLE),
        Class::Rogue => ("×", "rogue", RED),
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
    actions: Vec<(KeyCode, String)>,
    source_row: Option<String>,
    source_tab: Option<usize>,
    table: TableState,
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
            actions: Vec::new(),
            source_row: None,
            source_tab: None,
            table: TableState::default(),
        }
    }
    fn pane(&mut self, index: usize) {
        self.active = index.min(self.panes.len().saturating_sub(1));
        self.follow = self
            .panes
            .get(self.active)
            .is_some_and(|pane| pane.log.is_some());
        self.scroll = if self.follow { u16::MAX } else { 0 };
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

enum JobTarget {
    User(String),
    Job(String),
    Node(String),
    Attention {
        user: String,
        jobs: BTreeSet<String>,
    },
}
impl JobTarget {
    fn includes(&self, entry: Entry<'_>) -> bool {
        match self {
            Self::User(user) => entry.user() == user,
            Self::Job(id) => entry.id() == id,
            Self::Node(node) => match entry {
                Entry::Running(job) => crate::scheduler::expand_nodes(&job.node)
                    .is_some_and(|nodes| nodes.contains(node)),
                Entry::Pending(_) => false,
            },
            Self::Attention { jobs, .. } => jobs.contains(entry.id()),
        }
    }
    fn label(&self) -> String {
        match self {
            Self::User(user) => format!("user:{user}"),
            Self::Job(id) => format!("job:{id}"),
            Self::Node(node) => format!("node:{node}"),
            Self::Attention { user, .. } => format!("attention:{user}"),
        }
    }
}

struct ViewContext {
    preferences: ui_preferences::Preferences,
    filter_user: String,
    partition: String,
    mine: bool,
    free_only: bool,
    search: String,
    table: TableState,
    selected: Option<String>,
}
struct RelatedView {
    target: Option<JobTarget>,
    origin: ViewContext,
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
    job_sort: job_view::Sort,
    job_reverse: bool,
    job_filter: job_view::Filter,
    related: Option<RelatedView>,
    hits: Hits,
    quit_requested: bool,
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
    host_history: crate::host_view::History,
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
            job_sort: job_view::Sort::default(),
            job_reverse: false,
            job_filter: job_view::Filter::default(),
            related: None,
            hits: Hits::default(),
            quit_requested: false,
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
            host_history: crate::host_view::History::default(),
            popup_sequence: 0,
        }
    }
    fn preferences(&self) -> ui_preferences::Preferences {
        if let Some(related) = &self.related {
            return related.origin.preferences.clone();
        }
        ui_preferences::Preferences {
            tab: self.tab,
            sort: self.sort,
            reverse: self.reverse,
            job_sort: self.job_sort,
            job_reverse: self.job_reverse,
            job_filter: self.job_filter,
            ..Default::default()
        }
    }
    fn restore_preferences(&mut self, preferences: &ui_preferences::Preferences) {
        self.tab = preferences.tab;
        self.sort = preferences.sort;
        self.reverse = preferences.reverse;
        self.job_sort = preferences.job_sort;
        self.job_reverse = preferences.job_reverse;
        self.job_filter = preferences.job_filter;
        self.rebuild();
    }
    pub fn apply(&mut self, data: Snapshot) {
        let now = runtime::now();
        self.host_history.observe(&data.controller, now);
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
        if !self.initialized && (!data.nodes.is_empty() || data.controller.is_object()) {
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
    fn visible_jobs(&self) -> impl Iterator<Item = Entry<'_>> {
        self.data
            .jobs
            .iter()
            .map(Entry::Running)
            .chain(self.data.pending.iter().map(Entry::Pending))
            .filter(|entry| {
                self.job_filter.includes(*entry)
                    && self.job_visible(entry.user(), entry.partition(), &entry.fields())
                    && self
                        .related
                        .as_ref()
                        .and_then(|r| r.target.as_ref())
                        .is_none_or(|target| target.includes(*entry))
            })
    }
    fn open_related_jobs(&mut self) {
        let target = self.selected().and_then(|row| match self.tab {
            4 => row.key.strip_prefix("attention:").and_then(|user| {
                crate::users::attention_in_partition(
                    &self.data,
                    runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
                    runtime::now(),
                    &self.partition,
                )
                .into_iter()
                .find(|group| group.user == user && !group.jobs.is_empty())
                .map(|group| JobTarget::Attention {
                    user: group.user,
                    jobs: group.jobs,
                })
            }),
            5 => row
                .key
                .strip_prefix("user:")
                .map(|user| JobTarget::User(user.into())),
            0 | 1 => row
                .job
                .as_ref()
                .map(|id| JobTarget::Job(id.clone()))
                .or_else(|| (!row.node.is_empty()).then(|| JobTarget::Node(row.node.clone()))),
            _ => None,
        });
        let Some(target) = target else {
            self.status = if self.tab == 4 {
                "No linked Slurm jobs in current evidence · Enter to inspect"
            } else {
                "Select a GPU, node or user · J related Jobs"
            }
            .into();
            return;
        };
        let origin = ViewContext {
            preferences: self.preferences(),
            filter_user: std::mem::take(&mut self.filter_user),
            partition: std::mem::take(&mut self.partition),
            mine: std::mem::take(&mut self.mine),
            free_only: std::mem::take(&mut self.free_only),
            search: std::mem::take(&mut self.search),
            selected: self.selected().map(|row| row.key.clone()),
            table: std::mem::take(&mut self.table),
        };
        self.status = format!("Related Jobs · {} · Esc back", target.label());
        self.related = Some(RelatedView {
            target: Some(target),
            origin,
        });
        self.tab = 3;
        self.job_filter = job_view::Filter::All;
        self.rebuild();
    }
    fn return_from_jobs(&mut self) {
        let Some(related) = self.related.take() else {
            return;
        };
        let origin = related.origin;
        self.filter_user = origin.filter_user;
        self.partition = origin.partition;
        self.mine = origin.mine;
        self.free_only = origin.free_only;
        self.search = origin.search;
        self.restore_preferences(&origin.preferences);
        self.table = origin.table;
        self.rebuild();
        if let Some(index) = origin
            .selected
            .and_then(|key| self.rows.iter().position(|row| row.key == key))
        {
            self.table.select(Some(index));
        }
        self.status = "Returned to previous view".into();
    }
    fn job_summary(&self, now: f64) -> [String; 2] {
        let (mut running, mut pending) = (0, 0);
        let mut users = BTreeSet::new();
        let mut reasons = BTreeMap::<&str, usize>::new();
        let mut longest: Option<f64> = None;
        for entry in self.visible_jobs() {
            users.insert(entry.user());
            match entry {
                Entry::Running(_) => running += 1,
                Entry::Pending(job) => {
                    pending += 1;
                    *reasons
                        .entry(if job.reason.is_empty() {
                            "unknown"
                        } else {
                            &job.reason
                        })
                        .or_default() += 1;
                    if let Some(wait) = entry.wait(now) {
                        longest = Some(longest.unwrap_or(0.0).max(wait));
                    }
                }
            }
        }
        let mut reasons: Vec<_> = reasons.into_iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let detail = if pending == 0 {
            "Pending: none in current filters".into()
        } else {
            format!(
                "Pending: {}{} · max wait {}",
                reasons
                    .iter()
                    .take(2)
                    .map(|(reason, count)| format!("{} ×{count}", clipped(reason, 20)))
                    .collect::<Vec<_>>()
                    .join(" · "),
                if reasons.len() > 2 {
                    format!(" · +{} reasons", reasons.len() - 2)
                } else {
                    String::new()
                },
                longest
                    .map(|s| rules::span(s as u64))
                    .unwrap_or_else(|| "?".into())
            )
        };
        [
            format!(
                "Jobs: {running} running · {pending} pending · {} users",
                users.len()
            ),
            detail,
        ]
    }
    pub fn rebuild(&mut self) {
        self.hits.clear();
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
                            .bg(HEADER_BG)
                            .fg(if node.stale { YELLOW } else { TEXT })
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
                        let style =
                            Style::default().fg(if class == Class::Unknown { MUTED } else { TEXT });
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
                                    0,
                                    Style::default()
                                        .fg(gpu_mark(class).2)
                                        .add_modifier(Modifier::BOLD),
                                ),
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
                                        YELLOW
                                    } else if rules::health(gpu, now).1 > 0 {
                                        RED
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
                                accents: vec![
                                    (0, Style::default().fg(YELLOW)),
                                    (
                                        if self.compact { 3 } else { 4 },
                                        Style::default().fg(YELLOW),
                                    ),
                                ],
                                style: Style::default().fg(TEXT),
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
                            if node.stale {
                                "?".into()
                            } else {
                                node.cpu_load.clone()
                            },
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
                        style: Style::default().fg(if node.stale { YELLOW } else { TEXT }),
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
                    "Req GPU",
                    "Req CPU",
                    "Req RAM",
                    "CPU cores",
                    "RAM GiB",
                    "VRAM GiB",
                    "Left / wait",
                    "Reason / coverage",
                    "Name",
                    "Priority",
                ]
                .map(str::to_string)
                .to_vec();
                let mut entries: Vec<_> = self.visible_jobs().collect();
                entries.sort_by(|a, b| self.job_sort.compare(*a, *b, now, self.job_reverse));
                for entry in entries {
                    match entry {
                        Entry::Running(job) => {
                            let metrics = rules::resource_summary(job, &self.data.nodes, now);
                            let field = |key: &str, scale: f64| {
                                metrics
                                    .get(key)
                                    .map(|v| {
                                        format!(
                                            "{}{:.1}",
                                            if metrics
                                                .get(&format!("{key}_nodes"))
                                                .copied()
                                                .unwrap_or(0.0)
                                                < metrics
                                                    .get("expected_nodes")
                                                    .copied()
                                                    .unwrap_or(0.0)
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
                                    job.gpu_count.to_string(),
                                    job.cpu_count.to_string(),
                                    job.mem.clone(),
                                    field("cpu_cores", 1.0),
                                    field("mem_current_mib", 1024.0),
                                    field("vram_mib", 1024.0),
                                    entry.left().map(rules::span).unwrap_or_else(|| "?".into()),
                                    format!(
                                        "{}/{} OOM:{}",
                                        metrics.get("sampled_nodes").unwrap_or(&0.0),
                                        metrics.get("expected_nodes").unwrap_or(&0.0),
                                        field("oom_kill", 1.0)
                                    ),
                                    job.jobname.clone(),
                                    entry
                                        .priority()
                                        .map(|p| p.to_string())
                                        .unwrap_or_else(|| "?".into()),
                                ],
                                focus: (job.user == self.highlight_user).then_some(1),
                                accents: vec![
                                    (0, Style::default().fg(MUTED)),
                                    (2, Style::default().fg(GREEN)),
                                    (3, Style::default().fg(BLUE)),
                                    (4, Style::default().fg(BLUE)),
                                    (12, Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                                    (
                                        10,
                                        Style::default().fg(
                                            if metrics.get("oom_kill").is_some_and(|n| *n > 0.0) {
                                                RED
                                            } else {
                                                MUTED
                                            },
                                        ),
                                    ),
                                ],
                                style: Style::default().fg(TEXT),
                            });
                        }
                        Entry::Pending(job) => {
                            rows.push(ViewRow {
                                key: format!("pending:{}", job.jobid),
                                node: String::new(),
                                job: Some(job.jobid.clone()),
                                cells: vec![
                                    job.jobid.clone(),
                                    job.user.clone(),
                                    "PENDING".into(),
                                    job.gpu_count.to_string(),
                                    job.cpu_count.to_string(),
                                    job.mem.clone(),
                                    "?".into(),
                                    "?".into(),
                                    "?".into(),
                                    rules::pending_wait(job, now)
                                        .map(|s| rules::span(s as u64))
                                        .unwrap_or_else(|| "?".into()),
                                    rules::pending_reason(job),
                                    job.jobname.clone(),
                                    entry
                                        .priority()
                                        .map(|p| p.to_string())
                                        .unwrap_or_else(|| "?".into()),
                                ],
                                focus: (job.user == self.highlight_user).then_some(1),
                                accents: vec![
                                    (0, Style::default().fg(MUTED)),
                                    (2, Style::default().fg(YELLOW)),
                                    (3, Style::default().fg(BLUE)),
                                    (4, Style::default().fg(BLUE)),
                                    (10, Style::default().fg(YELLOW)),
                                    (12, Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                                ],
                                style: Style::default().fg(TEXT),
                            });
                        }
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
                        accents: vec![
                            (1, Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                            (
                                2,
                                Style::default().fg(if group.idle > 0 { YELLOW } else { MUTED }),
                            ),
                            (
                                3,
                                Style::default().fg(if group.parked > 0 { PURPLE } else { MUTED }),
                            ),
                            (
                                4,
                                Style::default().fg(if group.rogue > 0 { RED } else { MUTED }),
                            ),
                        ],
                        style: Style::default().fg(TEXT),
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
                            (2, Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
                            (6, Style::default().fg(BLUE)),
                            (7, Style::default().fg(BLUE)),
                        ],
                        style: Style::default().fg(TEXT),
                        cells: person.cells(),
                    });
                }
            }
            _ => {
                self.headers = [
                    "Resource",
                    "Now / R / RX",
                    "Total/W/TX",
                    "Fill / 15m",
                    "Guide",
                ]
                .map(str::to_string)
                .to_vec();
                for row in crate::host_view::rows(
                    &self.data.controller,
                    &self.data.collector,
                    &self.host_history,
                    now,
                ) {
                    if !self.search.is_empty()
                        && !row.key.to_lowercase().contains(&self.search.to_lowercase())
                        && !row
                            .cells
                            .iter()
                            .any(|c| c.to_lowercase().contains(&self.search.to_lowercase()))
                    {
                        continue;
                    }
                    let color = match row.level {
                        crate::host_view::Level::Info => CYAN,
                        crate::host_view::Level::Good => GREEN,
                        crate::host_view::Level::Watch => YELLOW,
                        crate::host_view::Level::High => ORANGE,
                        crate::host_view::Level::Critical => RED,
                        crate::host_view::Level::Unknown => MUTED,
                    };
                    rows.push(ViewRow {
                        key: row.key,
                        node: String::new(),
                        job: None,
                        cells: row.cells,
                        focus: None,
                        accents: vec![
                            (1, Style::default().add_modifier(Modifier::BOLD)),
                            (2, Style::default().add_modifier(Modifier::BOLD)),
                            (3, Style::default().fg(color)),
                            (4, Style::default().fg(color)),
                        ],
                        style: Style::default().fg(TEXT),
                    });
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
                    Style::default().fg(if node.stale { YELLOW } else { TEXT }),
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
                    Style::default().fg(if node.stale { YELLOW } else { MUTED }),
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
        self.hits.reset(Scope::Main {
            tab: self.tab,
            searching: self.searching,
        });
        frame.render_widget(
            Block::default().style(Style::default().fg(TEXT).bg(BACKGROUND)),
            frame.area(),
        );
        let dashboard_height =
            if frame.area().height >= 20 && frame.area().width >= 70 && self.tab != 6 {
                if frame.area().height >= 30 && matches!(self.tab, 0 | 1) {
                    10
                } else {
                    6
                }
            } else {
                0
            };
        let summary_height = if self.tab == 6 {
            3
        } else if self.trend {
            7
        } else {
            4
        };
        let spaced_rows = frame.area().height >= 20;
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(dashboard_height),
            Constraint::Length(summary_height.min(frame.area().height.saturating_sub(8))),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
        let heading = Layout::horizontal([
            Constraint::Length(if frame.area().width < 50 { 0 } else { 9 }),
            Constraint::Min(1),
        ])
        .split(areas[0]);
        frame.render_widget(
            Paragraph::new(" ▰ sgpu ")
                .style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            heading[0],
        );
        let labels = if frame.area().width < 58 {
            ["1", "2", "3", "4", "5", "6", "7"]
        } else {
            [
                "1 GPU", "2 CPU", "3 Use", "4 Jobs", "5 Attn", "6 Users", "7 Host",
            ]
        }
        .map(str::to_string);
        tab_bar(frame, heading[1], &labels, self.tab, &mut self.hits, false);
        self.hits.scroll = areas[3];
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
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
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
            3 => self.job_sort.label(),
            4 => "priority",
            5 => ["GPU", "CPU", "focus", "quota"][self.sort],
            6 => "resource",
            _ => ["node", "util", "focus", "free"][self.sort],
        };
        let descending = if self.tab == 3 {
            self.job_reverse ^ self.job_sort.descending()
        } else {
            self.reverse ^ (self.tab == 5 || (self.tab == 0 && self.sort > 0))
        };
        let mut chips = vec![
            Span::styled(format!(" sort:{sort_label}"), Style::default().fg(MUTED)),
            Span::styled(
                if descending { "↓ " } else { "↑ " },
                Style::default().fg(BLUE),
            ),
        ];
        let sortable = matches!(self.tab, 0 | 3 | 5);
        let mut chip_actions = vec![
            sortable.then_some(Click::Key(KeyCode::Char('s'))),
            sortable.then_some(Click::Key(KeyCode::Char('S'))),
        ];
        if let Some(related) = &self.related {
            let label = related
                .target
                .as_ref()
                .map(JobTarget::label)
                .unwrap_or_else(|| "all jobs".into());
            chips.insert(
                0,
                Span::styled(
                    format!(" related:{} · Esc back ", cli::terminal_safe(&label)),
                    Style::default().fg(FOCUS).add_modifier(Modifier::BOLD),
                ),
            );
            chip_actions.insert(0, Some(Click::Key(KeyCode::Esc)));
        }
        if self.tab == 3 {
            chips.push(Span::styled(
                format!(" state:{} [v] ", self.job_filter.label()),
                Style::default().fg(if self.job_filter == job_view::Filter::All {
                    MUTED
                } else {
                    FOCUS
                }),
            ));
            chip_actions.push(Some(Click::Key(KeyCode::Char('v'))));
        }
        for (text, active, key) in [
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
                'u',
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
                'p',
            ),
            ("MINE".into(), self.mine, 'm'),
            ("FREE".into(), self.free_only && self.tab == 0, 'i'),
            (
                format!("search:{}", self.search),
                !self.search.is_empty(),
                '/',
            ),
        ] {
            if active || text.starts_with("user:") || text.starts_with("part:") {
                chips.push(Span::styled(
                    format!(" {} ", cli::terminal_safe(&text)),
                    Style::default()
                        .fg(if active { BACKGROUND } else { MUTED })
                        .bg(if active { FOCUS } else { BACKGROUND }),
                ));
                chip_actions.push(Some(Click::Key(KeyCode::Char(key))));
            }
        }
        chips.push(Span::styled(" 0 reset", Style::default().fg(MUTED)));
        chip_actions.push(Some(Click::Key(KeyCode::Char('0'))));
        if self.tab != 6 && areas[2].height > 1 {
            self.hits.spans(
                Rect::new(areas[2].x, areas[2].y + 1, areas[2].width, 1),
                &chips,
                &chip_actions,
            );
        }
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
            Style::default().fg(if attention_count > 0 { YELLOW } else { MUTED }),
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
                Span::styled("● busy ", Style::default().fg(CYAN)),
                Span::styled("○ free ", Style::default().fg(GREEN)),
                Span::styled("! idle ", Style::default().fg(YELLOW)),
                Span::styled("◇ parked ", Style::default().fg(PURPLE)),
                Span::styled("× rogue ", Style::default().fg(RED)),
                Span::styled("? unknown", Style::default().fg(MUTED)),
            ]));
        } else {
            summary.push(Line::styled(
                format!(
                    "Scheduler unavailable: {}",
                    cli::terminal_safe(&self.data.errors)
                ),
                Style::default().fg(RED),
            ));
        }
        if self.tab == 3 {
            let [counts, reasons] = self.job_summary(runtime::now());
            summary[2] = Line::styled(counts, Style::default().fg(FOCUS));
            if self.data.errors.is_empty() {
                summary[3] = Line::styled(reasons, Style::default().fg(MUTED));
            }
        }
        if self.trend {
            summary.extend([
                Line::styled(
                    format!("5m GPU% {}", self.spark(0)),
                    Style::default().fg(CYAN),
                ),
                Line::styled(
                    format!("VRAM% {}", self.spark(1)),
                    Style::default().fg(PURPLE),
                ),
                Line::styled(
                    format!("GPU W {} · = unknown", self.spark(2)),
                    Style::default().fg(YELLOW),
                ),
            ]);
        }
        if self.tab == 6 {
            let host = &self.data.controller;
            let now = runtime::now();
            let age = crate::model::json_number(&host["observed_at"])
                .map(|t| format!("{:.0}s ago", (now - t).max(0.0)))
                .unwrap_or_else(|| "?".into());
            let fresh = crate::model::json_number(&host["observed_at"])
                .is_some_and(|t| rules::telemetry_fresh(t, now));
            summary = vec![
                Line::styled(
                    format!(
                        "Host {} · {age} · {}",
                        cli::terminal_safe(host["hostname"].as_str().unwrap_or("controller")),
                        if fresh { "LIVE" } else { "STALE / UNAVAILABLE" }
                    ),
                    Style::default().fg(if fresh { CYAN } else { YELLOW }),
                ),
                Line::styled(
                    "Capacity 80/90/97% · PSI 1/10% · link 80%: review hints, not diagnoses",
                    Style::default().fg(MUTED),
                ),
                Line::styled(
                    format!(
                        "/ search:{} · Enter full values, guides + recent trends · 0 reset",
                        cli::terminal_safe(&self.search)
                    ),
                    Style::default().fg(MUTED),
                ),
            ];
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
            let block = panel(" GPU-hours · observation coverage ");
            let inner = block.inner(areas[3]);
            frame.render_widget(block, areas[3]);
            let paragraph = Paragraph::new(cli::terminal_safe(&body)).wrap(Wrap { trim: false });
            let max = paragraph
                .line_count(inner.width)
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            self.usage_scroll = self.usage_scroll.min(max);
            frame.render_widget(paragraph.scroll((self.usage_scroll, 0)), inner);
        } else {
            let visible_columns: Vec<usize> = match self.tab {
                0 if self.narrow => vec![0, 1, 2, 3, 5],
                1 if frame.area().width < 110 => vec![0, 1, 2, 3, 4, 7],
                3 if frame.area().width < 100 => vec![0, 1, 2, 12, 3, 4, 9, 10],
                3 if frame.area().width < 140 => vec![0, 1, 2, 12, 3, 4, 5, 9, 10, 11],
                3 => vec![0, 1, 2, 12, 3, 4, 5, 6, 7, 8, 9, 10, 11],
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
                        0 => Constraint::Length(if self.narrow { 7 } else { 10 }),
                        1 => Constraint::Length(if self.narrow { 8 } else { 12 }),
                        2 => Constraint::Length(7),
                        3 | 4 => Constraint::Length(8),
                        5 => Constraint::Length(8),
                        6..=8 => Constraint::Length(9),
                        9 => Constraint::Length(10),
                        10 => Constraint::Min(if self.narrow { 8 } else { 18 }),
                        12 => Constraint::Length(9),
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
                        1 | 2 => Constraint::Length(12),
                        3 => Constraint::Length(15),
                        _ => Constraint::Min(15),
                    },
                })
                .collect();
            let table_area = panel("").inner(areas[3]);
            let [_, columns_area] =
                Layout::horizontal([Constraint::Length(2), Constraint::Fill(0)]).areas(table_area);
            let cell_areas = Layout::horizontal(widths.clone())
                .flex(Flex::Start)
                .spacing(1)
                .split(columns_area);
            if self.tab == 3 && !table_area.is_empty() {
                for (column, area) in visible_columns.iter().zip(cell_areas.iter()) {
                    if let Some(sort) = job_column_sort(*column) {
                        self.hits.add(
                            Rect::new(area.x, table_area.y, area.width, 1),
                            Click::JobSort(sort),
                        );
                    }
                }
            }
            for (column, area) in visible_columns.iter().zip(cell_areas.iter()) {
                if let Some(sort) = view_column_sort(self.tab, self.compact, *column) {
                    self.hits.add(
                        Rect::new(area.x, table_area.y, area.width, table_area.height.min(1)),
                        Click::Sort(sort),
                    );
                }
            }
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
                        if matches!(text.trim(), "?" | "—") {
                            style = style.fg(MUTED);
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
                        spaced_rows && self.tab == 0 && index > 0 && row.key.starts_with("node:"),
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
                                    let sort =
                                        (self.tab == 3).then(|| job_column_sort(*i)).flatten();
                                    let view_sort = view_column_sort(self.tab, self.compact, *i);
                                    let active_sort =
                                        sort == Some(self.job_sort) || view_sort == Some(self.sort);
                                    let clickable = sort.is_some() || view_sort.is_some();
                                    let text = if clickable {
                                        format!(
                                            "{}{}",
                                            clipped(
                                                text,
                                                cell_areas[position].width.saturating_sub(1)
                                            ),
                                            if active_sort {
                                                if descending { "↓" } else { "↑" }
                                            } else {
                                                "↕"
                                            }
                                        )
                                    } else {
                                        text.clone()
                                    };
                                    Cell::from(
                                        Line::from(clipped(&text, cell_areas[position].width))
                                            .alignment(column_alignment(
                                                self.tab,
                                                self.compact,
                                                *i,
                                            )),
                                    )
                                    .style(if active_sort {
                                        Style::default()
                                            .fg(FOCUS)
                                            .bg(SELECTED_BG)
                                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                                    } else if clickable {
                                        Style::default().fg(BLUE)
                                    } else {
                                        Style::default().fg(MUTED)
                                    })
                                })
                            }),
                    )
                    .style(
                        Style::default()
                            .fg(CYAN)
                            .bg(HEADER_BG)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .column_spacing(1)
                .row_highlight_style(
                    Style::default()
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ")
                .highlight_spacing(HighlightSpacing::Always)
                .block(panel(match self.tab {
                    0 => " GPUs · Space fold · Enter inspect ",
                    1 => " Nodes · measured CPU / RAM ",
                    3 => " Jobs · click ↕ header · s sort · S reverse · v state ",
                    4 => " Attention · sustained idle / parked allocations; unallocated processes ",
                    5 => " Users · job CPU/RAM/IO; node network/load are SHARED ",
                    _ => " Host · R/W disk · RX/TX net · Enter: guide + 5m trend ",
                }));
            frame.render_stateful_widget(table, areas[3], &mut self.table);
            let mut y = table_area.y.saturating_add(1);
            for (index, row) in self.rows.iter().enumerate().skip(self.table.offset()) {
                if spaced_rows && self.tab == 0 && index > 0 && row.key.starts_with("node:") {
                    y = y.saturating_add(1);
                }
                if y >= table_area.bottom() {
                    break;
                }
                let rect = Rect::new(table_area.x, y, table_area.width, 1);
                self.hits.add(rect, Click::Row(row.key.clone()));
                if self.tab == 0
                    && row.key.starts_with("node:")
                    && let Some(area) = cell_areas.first()
                {
                    self.hits.add(
                        Rect::new(area.x, y, area.width.min(1), 1),
                        Click::Fold(row.key.clone()),
                    );
                }
                y = y.saturating_add(1);
            }
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
        let [status_area, buttons_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(areas[4]);
        let bindings = if self.searching {
            self.hits.reset(Scope::Main {
                tab: self.tab,
                searching: true,
            });
            frame.render_widget(
                Paragraph::new(format!("Search: {}▏", cli::terminal_safe(&self.search)))
                    .style(Style::default().fg(YELLOW)),
                status_area,
            );
            vec![
                (KeyCode::Enter, "Enter Apply".into()),
                (KeyCode::Esc, "Esc Clear".into()),
            ]
        } else {
            frame.render_widget(
                Paragraph::new(format!(
                    "{} · row {}/{}",
                    cli::terminal_safe(&self.status),
                    self.table.selected().map_or(0, |i| i + 1),
                    self.rows.len()
                ))
                .style(Style::default().fg(MUTED)),
                status_area,
            );
            let mut bindings = vec![
                (KeyCode::Char('a'), "a Actions".into()),
                (KeyCode::Char('q'), "q Quit".into()),
            ];
            if self.related.is_some() {
                bindings.push((KeyCode::Esc, "Esc Back".into()));
            } else if matches!(self.tab, 0 | 1 | 4 | 5) {
                bindings.push((KeyCode::Char('J'), "J Jobs".into()));
            }
            if self.tab != 2 {
                bindings.push((KeyCode::Enter, "Enter Detail".into()));
            }
            if matches!(self.tab, 0 | 3 | 5) {
                bindings.extend([
                    (KeyCode::Char('s'), "s Sort".into()),
                    (KeyCode::Char('S'), "S Reverse".into()),
                ]);
            }
            if self.tab == 3 {
                bindings.push((KeyCode::Char('v'), "v State".into()));
            }
            if self.tab == 2 {
                bindings.extend([
                    (KeyCode::PageUp, "Pg↑".into()),
                    (KeyCode::PageDown, "Pg↓".into()),
                    (KeyCode::Home, "Home".into()),
                    (KeyCode::End, "End".into()),
                ]);
            }
            bindings.extend([
                (KeyCode::Char('/'), "/ Search".into()),
                (KeyCode::Char('0'), "0 Reset".into()),
                (KeyCode::Char('?'), "? Help".into()),
                (KeyCode::Char('r'), "r Refresh".into()),
            ]);
            bindings
        };
        buttons(frame, buttons_area, &bindings, &mut self.hits);
        if let Some(popup) = &mut self.popup {
            draw_popup(frame, popup, &mut self.hits);
        }
    }
    fn open(&mut self, mut popup: Popup) {
        self.searching = false;
        self.hits.clear();
        self.popup_sequence += 1;
        popup.token = self.popup_sequence;
        self.popup = Some(popup);
    }
    fn actions(&self) -> Vec<(KeyCode, String)> {
        let mut actions = Vec::new();
        if self.tab != 2 {
            actions.push((KeyCode::Enter, "Enter · Selected row details".into()));
        }
        for (index, name) in ["GPU", "CPU", "Usage", "Jobs", "Attention", "Users", "Host"]
            .iter()
            .enumerate()
        {
            actions.push((
                KeyCode::Char((b'1' + index as u8) as char),
                format!("{} · {name}", index + 1),
            ));
        }
        actions.extend([
            (KeyCode::Char('/'), "/ · Search".into()),
            (KeyCode::Char('0'), "0 · Reset filters".into()),
        ]);
        if matches!(self.tab, 0 | 1 | 4 | 5) {
            actions.push((KeyCode::Char('J'), "J · Related Jobs".into()));
        }
        if self.related.is_some() {
            actions.push((KeyCode::Esc, "Esc · Back to previous view".into()));
        }
        if matches!(self.tab, 0 | 3 | 5) {
            actions.extend([
                (KeyCode::Char('s'), "s · Next sort column".into()),
                (KeyCode::Char('S'), "S · Reverse sort".into()),
            ]);
        }
        if self.tab != 6 {
            actions.extend([
                (KeyCode::Char('u'), "u · Next user filter".into()),
                (KeyCode::Char('p'), "p · Next partition filter".into()),
                (KeyCode::Char('m'), "m · Highlighted account only".into()),
                (KeyCode::Char('t'), "t · Toggle five-minute trend".into()),
            ]);
        }
        if matches!(self.tab, 0 | 3) {
            actions.push((
                KeyCode::Char('v'),
                if self.tab == 3 {
                    "v · Next Jobs state"
                } else {
                    "v · Show pending jobs"
                }
                .into(),
            ));
        }
        if self.tab == 0 {
            actions.extend([
                (KeyCode::Char('i'), "i · Free GPUs only".into()),
                (KeyCode::Char('d'), "d · Detail columns".into()),
                (KeyCode::Char(' '), "Space · Fold selected node".into()),
                (KeyCode::Char('z'), "z · Fold/unfold all nodes".into()),
            ]);
        }
        if self.tab < 2 {
            actions.extend([
                (KeyCode::Char('['), "[ · Previous node".into()),
                (KeyCode::Char(']'), "] · Next node".into()),
            ]);
        }
        if self.selected().is_some_and(|row| row.job.is_some()) {
            actions.extend([
                (KeyCode::Char('n'), "n · Watch/unwatch selected job".into()),
                (KeyCode::Char('x'), "x · Cancel my own job…".into()),
            ]);
        }
        actions.extend([
            (KeyCode::Home, "Home · First row / top".into()),
            (KeyCode::End, "End · Last row / bottom".into()),
            (KeyCode::PageUp, "PgUp · Previous page".into()),
            (KeyCode::PageDown, "PgDn · Next page".into()),
            (KeyCode::Char('r'), "r · Refresh published data".into()),
            (KeyCode::Char('h'), "h · My job history".into()),
            (KeyCode::Char('w'), "w · Wasted GPU evidence".into()),
            (KeyCode::Char('f'), "f · Free capacity".into()),
            (KeyCode::Char('e'), "e · Export JSON".into()),
            (KeyCode::Char('?'), "? · Help".into()),
            (KeyCode::Char('q'), "q · Quit".into()),
        ]);
        actions
    }
    fn menu_action(&mut self, index: usize, worker: &Worker) -> bool {
        let Some(popup) = self.popup.take() else {
            return false;
        };
        let Some((key, _)) = popup.actions.get(index) else {
            return false;
        };
        if matches!(key, KeyCode::Enter | KeyCode::Char('J' | 'n' | 'x' | ' ')) {
            let target = popup
                .source_row
                .and_then(|key| self.rows.iter().position(|row| row.key == key));
            if popup.source_tab != Some(self.tab) || target.is_none() {
                self.status =
                    "Original row is no longer available · select a row and reopen Actions".into();
                return false;
            }
            self.table.select(target);
        }
        self.key(*key, worker)
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
    fn mouse(&mut self, mouse: MouseEvent, worker: &Worker) -> bool {
        let scope = self.popup.as_ref().map_or(
            Scope::Main {
                tab: self.tab,
                searching: self.searching,
            },
            |popup| Scope::Popup(popup.token),
        );
        if !mouse.modifiers.is_empty() || self.hits.scope != Some(scope) {
            return false;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Right)
            && self.popup.is_none()
            && !self.searching
        {
            if let Some(Click::Row(key) | Click::Fold(key)) = self.hits.at(mouse.column, mouse.row)
            {
                self.table
                    .select(self.rows.iter().position(|row| row.key == key));
            }
            self.key(KeyCode::Char('a'), worker);
            return true;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if !self.hits.scroll.contains((mouse.column, mouse.row).into()) {
                return false;
            }
            self.key(
                if mouse.kind == MouseEventKind::ScrollDown {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                },
                worker,
            );
            return true;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        let Some(action) = self.hits.at(mouse.column, mouse.row) else {
            return false;
        };
        self.hits.clear();
        match action {
            Click::Key(key) => self.quit_requested = self.key(key, worker),
            Click::Row(ref key) | Click::Fold(ref key) => {
                let Some(index) = self.rows.iter().position(|row| row.key == *key) else {
                    return false;
                };
                self.table.select(Some(index));
                if matches!(action, Click::Fold(_)) {
                    self.key(KeyCode::Char(' '), worker);
                }
            }
            Click::JobSort(sort) => {
                if self.job_sort == sort {
                    self.job_reverse = !self.job_reverse;
                } else {
                    self.job_sort = sort;
                    self.job_reverse = false;
                }
                self.status = format!(
                    "Jobs sort: {} · click again to reverse",
                    self.job_sort.label()
                );
                self.rebuild();
            }
            Click::Sort(sort) => {
                if self.sort == sort {
                    self.reverse = !self.reverse;
                } else {
                    self.sort = sort;
                    self.reverse = false;
                }
                self.rebuild();
            }
            Click::Pane(index) => {
                if let Some(popup) = &mut self.popup {
                    popup.pane(index);
                }
            }
            Click::History(index) => {
                if let Some(popup) = &mut self.popup {
                    popup.selected = index.min(popup.history.len().saturating_sub(1));
                }
            }
            Click::Menu(index) => {
                self.quit_requested = self.menu_action(index, worker);
            }
        }
        true
    }
    fn key(&mut self, key: KeyCode, worker: &Worker) -> bool {
        self.hits.clear();
        if self.searching && self.popup.is_none() {
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
            if !popup.actions.is_empty() && key == KeyCode::Enter {
                let index = popup.selected;
                return self.menu_action(index, worker);
            }
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
                        return false;
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                        self.popup = None;
                        return false;
                    }
                    _ => (),
                }
            }
            let row_count = if popup.actions.is_empty() {
                popup.history.len()
            } else {
                popup.actions.len()
            };
            match key {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => self.popup = None,
                KeyCode::Tab | KeyCode::Right => {
                    if row_count > 0 {
                        popup.selected = (popup.selected + 1).min(row_count - 1);
                    } else {
                        popup.pane((popup.active + 1) % popup.panes.len().max(1));
                    }
                }
                KeyCode::Left | KeyCode::BackTab => {
                    if row_count > 0 {
                        popup.selected = popup.selected.saturating_sub(1);
                    } else {
                        popup.pane(
                            (popup.active + popup.panes.len().max(1) - 1)
                                % popup.panes.len().max(1),
                        );
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if row_count == 0 {
                        popup.scroll = popup.scroll.saturating_add(1);
                        popup.follow = false;
                    } else {
                        popup.selected = (popup.selected + 1).min(row_count - 1);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if row_count == 0 {
                        popup.scroll = popup.scroll.saturating_sub(1);
                        popup.follow = false;
                    } else {
                        popup.selected = popup.selected.saturating_sub(1);
                    }
                }
                KeyCode::PageDown if row_count > 0 => {
                    popup.selected = popup.selected.saturating_add(20).min(row_count - 1)
                }
                KeyCode::PageUp if row_count > 0 => {
                    popup.selected = popup.selected.saturating_sub(20)
                }
                KeyCode::End if row_count > 0 => popup.selected = row_count - 1,
                KeyCode::Home if row_count > 0 => popup.selected = 0,
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
            KeyCode::Char('a') => {
                let mut popup = Popup::text("Actions · ↑↓ / Tab select · Enter run", String::new());
                popup.actions = self.actions();
                popup.source_row = self.selected().map(|row| row.key.clone());
                popup.source_tab = Some(self.tab);
                self.open(popup);
            }
            KeyCode::Char(c @ '1'..='7') => {
                self.return_from_jobs();
                self.tab = (c as u8 - b'1') as usize;
                self.table = TableState::default();
            }
            KeyCode::Char('g') => {
                self.return_from_jobs();
                self.tab = 2;
            }
            KeyCode::Char('J') => self.open_related_jobs(),
            KeyCode::Char(']') => self.jump_node(true),
            KeyCode::Char('[') => self.jump_node(false),
            KeyCode::Char('0') | KeyCode::Backspace => {
                self.filter_user.clear();
                self.partition.clear();
                self.mine = false;
                self.free_only = false;
                self.search.clear();
                self.job_filter = job_view::Filter::All;
                if let Some(related) = &mut self.related {
                    related.target = None;
                }
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
            KeyCode::Esc if self.related.is_some() => {
                self.return_from_jobs();
                return false;
            }
            KeyCode::Esc => self.search.clear(),
            KeyCode::Char('s') if matches!(self.tab, 0 | 5) => self.sort = (self.sort + 1) % 4,
            KeyCode::Char('S') if matches!(self.tab, 0 | 5) => self.reverse = !self.reverse,
            KeyCode::Char('s') if self.tab == 3 => {
                self.job_sort = self.job_sort.next();
                self.job_reverse = false;
                self.status = format!("Jobs sort: {} · s next · S reverse", self.job_sort.label());
            }
            KeyCode::Char('S') if self.tab == 3 => self.job_reverse = !self.job_reverse,
            KeyCode::Char('s' | 'S') => {
                self.status = "Interactive sorting is available in GPU, Jobs and Users".into()
            }
            KeyCode::Char('i') => self.free_only = !self.free_only,
            KeyCode::Char('m') => self.mine = !self.mine,
            KeyCode::Char('d') => self.details = !self.details,
            KeyCode::Char('v') if self.tab == 3 => {
                self.job_filter = self.job_filter.next();
                self.status = format!("Jobs state: {} · v next · 0 reset", self.job_filter.label());
            }
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
                    if self.tab == 6
                        && let Some(host_row) = crate::host_view::rows(
                            &self.data.controller,
                            &self.data.collector,
                            &self.host_history,
                            runtime::now(),
                        )
                        .into_iter()
                        .find(|r| r.key == key)
                    {
                        body.push_str(&format!("\n\n{}", host_row.detail));
                    }
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
    if node.stale {
        return "?".into();
    }
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
fn draw_popup(frame: &mut Frame, popup: &mut Popup, hits: &mut Hits) {
    let area = frame.area();
    let rect = if area.width < 80 || area.height < 18 {
        area
    } else {
        let width = area.width.saturating_sub(4).min(140);
        let height = area.height.saturating_sub(4);
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    };
    hits.reset(Scope::Popup(popup.token));
    hits.scroll = rect;
    frame.render_widget(Clear, rect);
    let bordered = rect.width >= 6 && rect.height >= 5;
    let title = format!(
        " {} ",
        clipped(
            &cli::terminal_safe(&popup.title),
            rect.width.saturating_sub(8)
        )
    );
    let block = panel(&title)
        .borders(if bordered {
            Borders::ALL
        } else {
            Borders::NONE
        })
        .border_style(Style::default().fg(BLUE));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if bordered {
        let close = Rect::new(rect.right().saturating_sub(5), rect.y, 3, 1);
        frame.render_widget(
            Paragraph::new("[×]").style(Style::default().fg(CYAN)),
            close,
        );
        hits.add(close, Click::Key(KeyCode::Esc));
    }
    let [heading, body, footer] = Layout::vertical([
        Constraint::Length(u16::from(inner.height >= 3)),
        Constraint::Min(1),
        Constraint::Length(u16::from(inner.height >= 2)),
    ])
    .areas(inner);
    let mut bindings = if popup.cancel.is_some() {
        vec![
            (
                KeyCode::Char('n'),
                if inner.width < 24 { "No" } else { "n Keep" }.into(),
            ),
            (
                KeyCode::Char('y'),
                if inner.width < 24 { "Yes" } else { "y Cancel" }.into(),
            ),
        ]
    } else {
        vec![(KeyCode::Esc, "Close".into())]
    };
    if !popup.actions.is_empty() || !popup.history.is_empty() {
        let menu = !popup.actions.is_empty();
        if !heading.is_empty() {
            frame.render_widget(
                Paragraph::new("↑↓ / wheel · Enter opens selection")
                    .style(Style::default().fg(MUTED)),
                heading,
            );
        }
        let rows: Vec<Row> = if menu {
            popup
                .actions
                .iter()
                .map(|(_, label)| Row::new([label.clone()]))
                .collect()
        } else {
            popup
                .history
                .iter()
                .map(|row| {
                    let cells = if body.width < 85 {
                        vec![
                            row.jobid.clone(),
                            row.state.clone(),
                            cli::terminal_safe(&row.name),
                        ]
                    } else {
                        vec![
                            row.jobid.clone(),
                            row.state.clone(),
                            row.exit.clone(),
                            rules::span(row.elapsed as u64),
                            row.partition.clone(),
                            cli::terminal_safe(&row.name),
                        ]
                    };
                    Row::new(cells).style(Style::default().fg(if row.exit != "0:0" {
                        YELLOW
                    } else {
                        TEXT
                    }))
                })
                .collect()
        };
        let widths = if menu {
            vec![Constraint::Min(1)]
        } else if body.width < 85 {
            vec![
                Constraint::Length(12),
                Constraint::Length(15),
                Constraint::Min(1),
            ]
        } else {
            vec![
                Constraint::Length(14),
                Constraint::Length(17),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(16),
                Constraint::Min(1),
            ]
        };
        let row_count = rows.len();
        popup.selected = popup.selected.min(row_count.saturating_sub(1));
        popup
            .table
            .select((row_count > 0).then_some(popup.selected));
        let mut table = Table::new(rows, widths).row_highlight_style(
            Style::default()
                .fg(FOCUS)
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD),
        );
        let header = !menu && body.height > 1;
        if header {
            table = table.header(
                Row::new(if body.width < 85 {
                    vec!["JobID", "State", "Name"]
                } else {
                    vec!["JobID", "State", "Exit", "Elapsed", "Partition", "Name"]
                })
                .style(Style::default().fg(BLUE).bg(HEADER_BG)),
            );
        }
        frame.render_stateful_widget(table, body, &mut popup.table);
        for (line, index) in (popup.table.offset()..row_count).enumerate() {
            let y = body
                .y
                .saturating_add(u16::from(header))
                .saturating_add(line.min(u16::MAX as usize) as u16);
            if y >= body.bottom() {
                break;
            }
            hits.add(
                Rect::new(body.x, y, body.width, 1),
                if menu {
                    Click::Menu(index)
                } else {
                    Click::History(index)
                },
            );
        }
        bindings.push((KeyCode::Enter, if menu { "Run" } else { "Open" }.into()));
    } else {
        if popup.panes.len() > 1 {
            let labels = popup
                .panes
                .iter()
                .map(|pane| pane.name.clone())
                .collect::<Vec<_>>();
            tab_bar(frame, heading, &labels, popup.active, hits, true);
        } else {
            frame.render_widget(
                Paragraph::new("↑↓ / wheel scroll · Home/End").style(Style::default().fg(MUTED)),
                heading,
            );
        }
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
                                .any(|word| line.contains(word))
                            {
                                RED
                            } else {
                                TEXT
                            },
                        ),
                    ))
                })
                .collect::<Vec<_>>();
            let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
            let total = paragraph.line_count(body.width);
            let max = total
                .saturating_sub(body.height as usize)
                .min(u16::MAX as usize) as u16;
            popup.scroll = if popup.follow {
                max
            } else {
                popup.scroll.min(max)
            };
            if popup.panes.len() == 1 {
                frame.render_widget(
                    Paragraph::new(format!(
                        "lines {}–{}/{total} · ↑↓ / wheel · Home/End",
                        (popup.scroll as usize + 1).min(total),
                        (popup.scroll as usize + body.height as usize).min(total)
                    ))
                    .style(Style::default().fg(MUTED)),
                    heading,
                );
            }
            frame.render_widget(paragraph.scroll((popup.scroll, 0)), body);
        }
    }
    if popup.panes.len() > 1 && heading.is_empty() {
        bindings.extend([(KeyCode::BackTab, "←".into()), (KeyCode::Tab, "→".into())]);
    }
    bindings.extend([
        (
            KeyCode::End,
            if inner.width < 32 {
                "End"
            } else if popup
                .panes
                .get(popup.active)
                .is_some_and(|pane| pane.log.is_some())
            {
                if popup.follow { "Follow ✓" } else { "Follow" }
            } else {
                "End"
            }
            .into(),
        ),
        (KeyCode::Up, "↑".into()),
        (KeyCode::Down, "↓".into()),
        (KeyCode::Home, "Home".into()),
        (KeyCode::PageUp, "Pg↑".into()),
        (KeyCode::PageDown, "Pg↓".into()),
    ]);
    if popup.panes.len() > 1 && !heading.is_empty() {
        bindings.extend([(KeyCode::BackTab, "←".into()), (KeyCode::Tab, "→".into())]);
    }
    buttons(frame, footer, &bindings, hits);
}

fn ime(key: KeyCode) -> KeyCode {
    if let KeyCode::Char(c) = key {
        let mapped = match c {
            'ㅁ' => 'a',
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

struct MouseCapture;
impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        let capture = Self;
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        Ok(capture)
    }
}
impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}

pub fn run(highlight_user: Option<String>) -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive mode requires a terminal; use sgpu once or sgpu json".into());
    }
    let preferences_path = ui_preferences::path();
    let loaded_preferences = ui_preferences::load(&preferences_path);
    let can_save_preferences = loaded_preferences.is_ok();
    let initial_preferences = loaded_preferences.unwrap_or_else(|error| {
        eprintln!("sgpu: view settings ignored and left unchanged: {error}");
        ui_preferences::Preferences::default()
    });
    let mut app = App::new();
    app.restore_preferences(&initial_preferences);
    if let Some(user) = highlight_user {
        app.set_highlight_user(user);
    }
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<i32> {
        let _mouse = MouseCapture::enable()?;
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
                    Event::Mouse(mouse) => {
                        dirty |= app.mouse(mouse, &worker);
                        if app.quit_requested {
                            return Ok(0);
                        }
                    }
                    Event::Resize(_, _) => {
                        app.hits.clear();
                        dirty = true;
                    }
                    _ => (),
                }
            }
        }
    })();
    ratatui::restore();
    let preferences = app.preferences();
    if can_save_preferences
        && result.is_ok()
        && preferences != initial_preferences
        && let Err(error) = ui_preferences::save(&preferences_path, &preferences)
    {
        eprintln!("sgpu: view settings not saved: {error}");
    }
    result
}

const HELP: &str = concat!(
    "1/2/3/4  GPU / CPU / Usage / Jobs\n5/6/7  Attention / Users / Controller\n",
    "g Usage · v pending · t 5-minute cluster trend\nr refresh published data\n",
    "s sort · S reverse · [ / ] previous/next node · Home/End first/last row\n",
    "Jobs: s cycles priority / user / GPU / CPU / wait / left; S reverses.\n",
    "Jobs: click User, Priority, Req GPU or Req CPU headers to sort; click again to reverse.\n",
    "Header arrows show direction; ↕ marks clickable headers. Mouse wheel scrolls.\n",
    "Mouse: click tabs, rows, marked headers, filter chips and footer buttons.\n",
    "Row click selects; Detail opens. Right-click or a opens Actions for every command.\n",
    "Actions: ↑↓ / Tab select · Enter runs · Esc closes. Terminal selection may use Shift-drag.\n",
    "Short detail windows expand to the screen; wheel/↑↓/PgUp/PgDn/Home/End scroll.\n",
    "Popup tabs, Close and paging buttons are clickable. Follow resumes logs; scrolling up pauses.\n",
    "Jobs: v cycles all / running / pending; 0 clears state and other filters.\n",
    "u user filter (highlight first) · p partition · m highlighted user's jobs\n",
    "i free GPUs · 0 reset all filters · / search · Esc clear search\n",
    "d detail columns · Space fold node · z fold/unfold all\nj/k or arrows move\n",
    "Enter full values or job/node detail · Tab info/script/stdout/stderr\n",
    "J related Jobs from GPU/CPU/Users/Attention · Esc restores the previous view.\n",
    "Users opens all user jobs; Attention opens only jobs linked in current evidence.\n",
    "GPU/node links use exact job IDs or expanded node names; unlinked processes stay inspectable.\n",
    "0 resets the related scope; Esc still returns.\n",
    "Tab, sort and Jobs state are saved on exit in ~/.sgpu/ui/preferences.json.\n",
    "Related views, user/partition/search filters, logs and job data are not saved.\n",
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
    "Host: R/W disk, RX/TX network; bytes scale through TiB, link speed uses bits/s.\n",
    "Host guide colors are review hints: capacity 80/90/97%, PSI 1/10%, link 80%.\n",
    "Enter Host rows for definitions and five-minute trends; high throughput is not a fault.\n",
    "Unknown/stale data is never free capacity. ? unavailable; ~ partial.\n",
    "GPU/VRAM meters sample GPUs; CPU/RAM meters sample nodes. CPU is node-wide.\n",
    "Job RAM includes cgroup cache. Summed peaks are not simultaneous whole-job peaks.\n",
    "Block I/O excludes cached and network-filesystem traffic; rates need two samples.\n",
    "Network/load are not attributed to individual users. Storage queries run in the background.\n",
    "Pending start and capacity are estimates, not scheduler guarantees.\n",
    "Priority is Slurm's numeric score, not queue position; partition/QOS policy still applies.\n",
    "Wait sorts pending jobs longest first; left sorts running jobs by time limit remaining.\n",
    "Unknown sort values stay last in either direction. Job RAM/VRAM columns use GiB.\n",
    "GPU requests follow Slurm's per-node GRES field; CPU requests cover the whole job.\n",
    "PSI is pressure, not a diagnosis. GPU power caps alone are informational.\n\n",
    "Node results are push-only; missing agents stay stale without master-to-node requests."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Gpu;
    use ratatui::{Terminal, backend::TestBackend};

    fn jobs_snapshot() -> Snapshot {
        let submitted = |hours: i64| {
            (chrono::Local::now() - chrono::Duration::hours(hours))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        };
        Snapshot {
            jobs: vec![
                Job {
                    jobid: "42".into(),
                    user: "alice".into(),
                    priority: "100".into(),
                    gpu_count: 2,
                    cpu_count: 16,
                    partition: "gpu".into(),
                    qos: "normal".into(),
                    node: "gpu1".into(),
                    jobname: "training".into(),
                    time_limit: "01:00:00".into(),
                    elapsed: "00:55:00".into(),
                    ..Default::default()
                },
                Job {
                    jobid: "43".into(),
                    user: "bob".into(),
                    priority: "20".into(),
                    cpu_count: 32,
                    partition: "cpu".into(),
                    node: "cpu1".into(),
                    ..Default::default()
                },
            ],
            pending: vec![
                PendingJob {
                    jobid: "90".into(),
                    user: "alice".into(),
                    priority: "90".into(),
                    gpu_count: 4,
                    cpu_count: 32,
                    partition: "gpu".into(),
                    qos: "normal".into(),
                    reason: "Resources".into(),
                    submit_time: submitted(2),
                    ..Default::default()
                },
                PendingJob {
                    jobid: "91".into(),
                    user: "bob".into(),
                    priority: "9".into(),
                    gpu_count: 8,
                    cpu_count: 16,
                    partition: "gpu".into(),
                    reason: "Dependency".into(),
                    submit_time: submitted(1),
                    ..Default::default()
                },
                PendingJob {
                    jobid: "92".into(),
                    user: "bob".into(),
                    gpu_count: 1,
                    partition: "gpu".into(),
                    reason: "Priority".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn related_user_jobs_restore_filters_and_selection_after_refresh() {
        let worker = Worker::new();
        let mut data = jobs_snapshot();
        data.jobs.push(Job {
            user: "alice2".into(),
            jobid: "420".into(),
            ..data.jobs[0].clone()
        });
        let mut app = App::for_user("alice".into());
        app.tab = 5;
        app.partition = "gpu".into();
        app.mine = true;
        app.free_only = true;
        app.search = "alice".into();
        app.job_filter = job_view::Filter::Pending;
        app.job_sort = job_view::Sort::Wait;
        app.apply(data.clone());
        assert_eq!(app.selected().unwrap().key, "user:alice");
        let original_preferences = app.preferences();
        app.key(KeyCode::Char('J'), &worker);
        assert_eq!(app.tab, 3);
        assert_eq!(
            app.rows
                .iter()
                .filter_map(|row| row.job.as_deref())
                .collect::<Vec<_>>(),
            ["90", "42"]
        );
        assert!(app.job_summary(runtime::now())[0].contains("1 running · 1 pending · 1 users"));
        assert!(app.partition.is_empty() && app.search.is_empty() && !app.mine && !app.free_only);
        assert_eq!(app.preferences(), original_preferences);
        for width in [80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("related:user:alice"));
            assert!(text.contains("Back") && text.contains("Quit"));
        }
        app.key(KeyCode::Char('/'), &worker);
        app.key(KeyCode::Char('9'), &worker);
        app.key(KeyCode::Esc, &worker);
        assert_eq!(app.tab, 3);
        assert!(app.search.is_empty());
        app.open(Popup::text("details", "evidence".into()));
        app.key(KeyCode::Esc, &worker);
        assert!(app.popup.is_none());
        assert_eq!(app.tab, 3);
        app.key(KeyCode::Char('0'), &worker);
        assert_eq!(app.rows.len(), data.jobs.len() + data.pending.len());
        app.key(KeyCode::Char('s'), &worker);
        assert_eq!(app.preferences(), original_preferences);
        data.jobs[0].gpu_count = 0;
        data.pending[0].gpu_count = 0;
        app.apply(data);
        app.key(KeyCode::Esc, &worker);
        assert_eq!(app.tab, 5);
        assert_eq!(app.partition, "gpu");
        assert_eq!(app.search, "alice");
        assert!(app.mine && app.free_only);
        assert_eq!(app.selected().unwrap().key, "user:alice");
        assert_eq!(app.preferences(), original_preferences);
        let mut restored = App::for_user("alice".into());
        restored.restore_preferences(
            &serde_json::from_str(&serde_json::to_string(&app.preferences()).unwrap()).unwrap(),
        );
        restored.apply(jobs_snapshot());
        assert_eq!(restored.preferences(), original_preferences);
        assert!(restored.search.is_empty() && restored.partition.is_empty() && !restored.mine);

        app.mine = false;
        app.rebuild();
        app.table
            .select(app.rows.iter().position(|row| row.key == "user:alice2"));
        app.key(KeyCode::Char('J'), &worker);
        let mut refreshed = app.data.clone();
        refreshed.jobs[0].gpu_count = 10;
        app.apply(refreshed.clone());
        app.key(KeyCode::Esc, &worker);
        assert_eq!(app.selected().unwrap().key, "user:alice2");
        assert_eq!(app.table.selected(), Some(1));
        app.key(KeyCode::Char('J'), &worker);
        refreshed.jobs.retain(|job| job.user != "alice2");
        app.apply(refreshed);
        app.key(KeyCode::Esc, &worker);
        assert_eq!(app.selected().unwrap().key, "user:alice");
    }

    #[test]
    fn related_gpu_and_node_jobs_match_exact_ids_and_expanded_nodes() {
        let worker = Worker::new();
        let mut data = jobs_snapshot();
        data.jobs.push(Job {
            jobid: "420".into(),
            node: "gpu10".into(),
            ..data.jobs[0].clone()
        });
        data.jobs.push(Job {
            jobid: "42_1".into(),
            node: "gpu[1-2]".into(),
            ..data.jobs[0].clone()
        });
        data.nodes.push(Node {
            name: "gpu1".into(),
            cpus: "32".into(),
            gpus: vec![Gpu {
                index: "0".into(),
                alloc_jobid: "42".into(),
                alloc_user: "alice".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut app = App::for_user("alice".into());
        app.tab = 1;
        app.apply(data);
        assert_eq!(app.selected().unwrap().key, "node:gpu1");
        app.key(KeyCode::Char('J'), &worker);
        assert_eq!(
            app.rows
                .iter()
                .filter_map(|row| row.job.as_deref())
                .collect::<Vec<_>>(),
            ["42", "42_1"]
        );
        app.key(KeyCode::Esc, &worker);
        app.key(KeyCode::Char('1'), &worker);
        app.table
            .select(app.rows.iter().position(|row| row.key == "gpu:gpu1:0"));
        app.key(KeyCode::Char('J'), &worker);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].job.as_deref(), Some("42"));
        app.key(KeyCode::Char('7'), &worker);
        assert_eq!(app.tab, 6);
        assert!(app.related.is_none());
        assert_eq!(app.job_filter, job_view::Filter::All);
    }

    #[test]
    fn attention_links_only_evidenced_jobs_and_keeps_unlinked_processes_inspectable() {
        let worker = Worker::new();
        let mut data = jobs_snapshot();
        data.nodes.push(Node {
            name: "gpu1".into(),
            state: "allocated".into(),
            gpus: vec![Gpu {
                index: "0".into(),
                util: "0".into(),
                mem_used: "0".into(),
                mem_total: "100".into(),
                alloc_jobid: "42".into(),
                alloc_user: "alice".into(),
                idle_sec: u64::MAX,
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut app = App::for_user("alice".into());
        app.tab = 4;
        app.apply(data);
        assert_eq!(app.selected().unwrap().key, "attention:alice");
        app.key(KeyCode::Enter, &worker);
        assert!(app.popup.is_some());
        app.key(KeyCode::Esc, &worker);
        app.key(KeyCode::Char('J'), &worker);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].job.as_deref(), Some("42"));
        app.key(KeyCode::Esc, &worker);
        let gpu = &mut app.data.nodes[0].gpus[0];
        gpu.alloc_jobid.clear();
        gpu.alloc_user.clear();
        gpu.idle_sec = 0;
        gpu.users = vec!["alice".into(), "bob".into()];
        gpu.pids = vec!["123".into()];
        app.rebuild();
        app.key(KeyCode::Char('J'), &worker);
        assert_eq!(app.tab, 4);
        assert!(app.related.is_none());
        assert!(app.status.contains("No linked Slurm jobs"));
        app.key(KeyCode::Enter, &worker);
        assert!(app.popup.is_some());
    }

    fn click_action(
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        worker: &Worker,
        action: Click,
    ) {
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let area = app
            .hits
            .items
            .iter()
            .find(|(_, candidate)| *candidate == action)
            .unwrap_or_else(|| panic!("missing click action {action:?}"))
            .0;
        assert!(app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE
            },
            worker
        ));
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn mouse_snapshot() -> Snapshot {
        let mut data = jobs_snapshot();
        for index in 1..=2 {
            data.nodes.push(Node {
                name: format!("gpu{index}"),
                partition: "gpu".into(),
                state: "allocated".into(),
                cpus: "32".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    util: "0".into(),
                    mem_used: "0".into(),
                    mem_total: "100".into(),
                    alloc_jobid: "42".into(),
                    alloc_user: "alice".into(),
                    idle_sec: u64::MAX,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        data.controller =
            serde_json::json!({"observed_at":runtime::now(),"cpu_online":16,"cpu_util":30});
        data
    }

    #[test]
    fn mouse_tabs_rows_folding_and_actions_work_at_narrow_and_wide_sizes() {
        let worker = Worker::new();
        let mut app = App::for_user("alice".into());
        app.apply(mouse_snapshot());
        for (width, height) in [(40, 24), (80, 24), (140, 24), (80, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for tab in 0..7 {
                click_action(
                    &mut app,
                    &mut terminal,
                    &worker,
                    Click::Key(KeyCode::Char((b'1' + tab) as char)),
                );
                assert_eq!(app.tab, tab as usize);
                terminal.draw(|frame| app.draw(frame)).unwrap();
                if tab != 2 {
                    let key = app
                        .hits
                        .items
                        .iter()
                        .rev()
                        .find_map(|(_, action)| {
                            if let Click::Row(key) = action {
                                Some(key.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap();
                    click_action(&mut app, &mut terminal, &worker, Click::Row(key.clone()));
                    assert_eq!(app.selected().unwrap().key, key);
                    assert!(app.popup.is_none());
                }
                click_action(
                    &mut app,
                    &mut terminal,
                    &worker,
                    Click::Key(KeyCode::Char('a')),
                );
                assert!(!app.popup.as_ref().unwrap().actions.is_empty());
                app.key(KeyCode::End, &worker);
                terminal.draw(|frame| app.draw(frame)).unwrap();
                assert!(app.popup.as_ref().unwrap().table.offset() > 0);
                click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::Esc));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.key(KeyCode::Char('6'), &worker);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Row("user:bob".into()),
        );
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('J')),
        );
        assert!(app.visible_jobs().all(|entry| entry.user() == "bob"));
        click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::Esc));
        assert_eq!(app.tab, 5);
        assert_eq!(app.selected().unwrap().key, "user:bob");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let row = app
            .hits
            .items
            .iter()
            .find(|(_, action)| *action == Click::Row("user:alice".into()))
            .unwrap()
            .0;
        assert!(app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE
            },
            &worker
        ));
        assert_eq!(
            app.popup.as_ref().unwrap().source_row.as_deref(),
            Some("user:alice")
        );
        click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::Esc));
        app.key(KeyCode::Char('1'), &worker);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let node = app
            .hits
            .items
            .iter()
            .find(|(_, action)| *action == Click::Row("node:gpu2".into()))
            .unwrap()
            .0;
        assert!(
            app.hits.at(node.x, node.y - 1).is_none(),
            "node margin selected a row"
        );
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Fold("node:gpu2".into()),
        );
        assert!(app.collapsed.contains("gpu2"));
        app.key(KeyCode::End, &worker);
        let mut short = Terminal::new(TestBackend::new(80, 12)).unwrap();
        short.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.table.offset() > 0);
        let key = app
            .hits
            .items
            .iter()
            .find_map(|(_, action)| {
                if let Click::Row(key) = action {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .unwrap();
        click_action(&mut app, &mut short, &worker, Click::Row(key.clone()));
        assert_eq!(app.selected().unwrap().key, key);
    }

    #[test]
    fn mouse_filters_search_menu_and_quit_share_keyboard_actions() {
        let worker = Worker::new();
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(mouse_snapshot());
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('u')),
        );
        assert_eq!(app.user_filter(), "alice");
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('v')),
        );
        assert_eq!(app.job_filter, job_view::Filter::Running);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('S')),
        );
        assert!(app.job_reverse);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('/')),
        );
        app.key(KeyCode::Char('4'), &worker);
        click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::Enter));
        assert!(!app.searching);
        assert_eq!(app.rows.len(), 1);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('0')),
        );
        assert_eq!(app.rows.len(), 5);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('a')),
        );
        let index = app
            .popup
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|(key, _)| *key == KeyCode::Char('m'))
            .unwrap();
        app.popup.as_mut().unwrap().selected = index;
        click_action(&mut app, &mut terminal, &worker, Click::Menu(index));
        assert!(app.mine);
        app.key(KeyCode::Char('a'), &worker);
        let index = app
            .popup
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|(key, _)| *key == KeyCode::Char('7'))
            .unwrap();
        for _ in 0..index {
            app.key(KeyCode::Tab, &worker);
        }
        app.key(KeyCode::Enter, &worker);
        assert_eq!(app.tab, 6);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('q')),
        );
        assert!(app.quit_requested);
    }

    #[test]
    fn short_detail_windows_keep_wrapped_end_and_controls_visible_on_resize() {
        let worker = Worker::new();
        let mut app = App::for_user("alice".into());
        app.open(Popup::text(
            "long 상세 / fixture",
            format!("TOP\n{}\nTAIL_END", "긴 내용 with wrapping ".repeat(300)),
        ));
        for (width, height) in [(80, 12), (40, 8), (20, 4), (80, 6), (140, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            if app.popup.as_ref().unwrap().follow {
                terminal.draw(|frame| app.draw(frame)).unwrap();
                assert!(
                    screen(&terminal).contains("TAIL_END"),
                    "lost followed end after resize {width}x{height}"
                );
            }
            app.key(KeyCode::Home, &worker);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(screen(&terminal).contains("TOP"));
            assert!(screen(&terminal).contains("Close"));
            click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::End));
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(
                screen(&terminal).contains("TAIL_END"),
                "clipped bottom at {width}x{height}"
            );
            let bottom = app.popup.as_ref().unwrap().scroll;
            assert!(bottom > 0);
            app.key(KeyCode::PageUp, &worker);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(app.popup.as_ref().unwrap().scroll < bottom);
            app.key(KeyCode::End, &worker);
        }
    }

    #[test]
    fn mouse_panes_and_history_use_the_rendered_offset_and_consistent_follow() {
        let (sender, requests) = mpsc::sync_channel(8);
        let (_, receiver) = mpsc::sync_channel(8);
        let worker = Worker { sender, receiver };
        let mut app = App::for_user("alice".into());
        let mut popup = Popup::text("job", "info".into());
        for name in ["stdout", "stderr"] {
            popup.panes.push(Pane {
                name: name.into(),
                body: "line\n".repeat(100),
                log: Some(cli::LogView {
                    path: "/fixture.log".into(),
                    owner: None,
                    shared: false,
                    merged: false,
                }),
            });
        }
        app.open(popup);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        click_action(&mut app, &mut terminal, &worker, Click::Pane(1));
        assert!(app.popup.as_ref().unwrap().follow);
        app.key(KeyCode::Right, &worker);
        app.key(KeyCode::Left, &worker);
        assert_eq!(app.popup.as_ref().unwrap().active, 1);
        assert!(app.popup.as_ref().unwrap().follow);
        click_action(&mut app, &mut terminal, &worker, Click::Pane(0));
        assert!(!app.popup.as_ref().unwrap().follow);
        assert_eq!(app.popup.as_ref().unwrap().scroll, 0);
        let mut narrow = Terminal::new(TestBackend::new(20, 4)).unwrap();
        click_action(&mut app, &mut narrow, &worker, Click::Key(KeyCode::Tab));
        narrow.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.popup.as_ref().unwrap().active, 1);
        assert!(screen(&narrow).contains("stdout"));
        click_action(&mut app, &mut narrow, &worker, Click::Key(KeyCode::Home));
        click_action(&mut app, &mut narrow, &worker, Click::Key(KeyCode::End));
        assert!(app.popup.as_ref().unwrap().follow);
        let mut popup = Popup::text("history", String::new());
        popup.history = (0..80)
            .map(|index| history::Record {
                jobid: index.to_string(),
                state: "COMPLETED".into(),
                name: format!("job{index}"),
                ..Default::default()
            })
            .collect();
        popup.selected = 60;
        app.open(popup);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let offset = app.popup.as_ref().unwrap().table.offset();
        assert!(offset > 0);
        assert!(screen(&terminal).contains(&format!("job{offset}")));
        click_action(&mut app, &mut terminal, &worker, Click::History(offset));
        assert_eq!(app.popup.as_ref().unwrap().selected, offset);
        assert!(requests.try_recv().is_err());
        click_action(&mut app, &mut terminal, &worker, Click::Key(KeyCode::Enter));
        assert!(
            matches!(requests.try_recv().unwrap(), Task::HistoryDetail(row) if row.jobid == offset.to_string())
        );
    }

    #[test]
    fn mouse_cancellation_requires_an_explicit_confirmation_for_the_actual_owner() {
        let (sender, requests) = mpsc::sync_channel(8);
        let (_, receiver) = mpsc::sync_channel(8);
        let worker = Worker { sender, receiver };
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(jobs_snapshot());
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        app.key(KeyCode::Char('a'), &worker);
        let index = app
            .popup
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|(key, _)| *key == KeyCode::Char('x'))
            .unwrap();
        app.popup.as_mut().unwrap().selected = index;
        click_action(&mut app, &mut terminal, &worker, Click::Menu(index));
        assert_eq!(app.popup.as_ref().unwrap().cancel.as_deref(), Some("42"));
        assert!(requests.try_recv().is_err());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(!app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE
            },
            &worker
        ));
        assert!(requests.try_recv().is_err());
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('n')),
        );
        assert!(app.popup.is_none());
        app.key(KeyCode::Char('x'), &worker);
        click_action(
            &mut app,
            &mut terminal,
            &worker,
            Click::Key(KeyCode::Char('y')),
        );
        assert!(matches!(requests.try_recv().unwrap(), Task::Cancel(id) if id == "42"));
        app.table.select(
            app.rows
                .iter()
                .position(|row| row.job.as_deref() == Some("43")),
        );
        app.key(KeyCode::Char('x'), &worker);
        assert!(app.popup.is_none());
        assert!(app.status.contains("Only your own jobs"));
        assert!(requests.try_recv().is_err());
        app.table.select(
            app.rows
                .iter()
                .position(|row| row.job.as_deref() == Some("42")),
        );
        app.key(KeyCode::Char('a'), &worker);
        let index = app
            .popup
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|(key, _)| *key == KeyCode::Char('x'))
            .unwrap();
        let mut updated = app.data.clone();
        updated.jobs.retain(|job| job.jobid != "42");
        app.apply(updated);
        app.popup.as_mut().unwrap().selected = index;
        click_action(&mut app, &mut terminal, &worker, Click::Menu(index));
        assert!(app.popup.is_none());
        assert!(app.status.contains("Original row is no longer available"));
        assert!(requests.try_recv().is_err());
    }

    fn header_position(terminal: &Terminal<TestBackend>, label: &str) -> (u16, u16) {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let line = |y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let y = (area.y..area.bottom())
            .find(|y| line(*y).contains("JobID"))
            .unwrap();
        let x = (area.x..area.right())
            .find(|x| {
                (*x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .starts_with(label)
            })
            .unwrap();
        (x, y)
    }

    #[test]
    fn jobs_header_clicks_follow_rendered_columns_across_resize_and_empty_results() {
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(jobs_snapshot());
        let worker = Worker::new();
        for width in [160, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            for (label, sort) in [
                ("User", job_view::Sort::User),
                ("Priority", job_view::Sort::Priority),
                ("Req GPU", job_view::Sort::Gpu),
                ("Req CPU", job_view::Sort::Cpu),
            ] {
                terminal.draw(|f| app.draw(f)).unwrap();
                let (column, row) = header_position(&terminal, label);
                let mouse = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
                let selected = app.selected().unwrap().key.clone();
                assert!(
                    app.mouse(mouse, &worker),
                    "{label} not clickable at {width}"
                );
                assert_eq!(app.job_sort, sort);
                assert!(!app.job_reverse);
                assert_eq!(app.selected().unwrap().key, selected);
                terminal.draw(|f| app.draw(f)).unwrap();
                assert!(app.mouse(mouse, &worker));
                assert!(app.job_reverse);
                terminal.draw(|f| app.draw(f)).unwrap();
                let (x, y) = header_position(&terminal, label);
                assert_eq!(terminal.backend().buffer()[(x, y)].bg, SELECTED_BG);
                assert_eq!(terminal.backend().buffer()[(x, y)].fg, FOCUS);
                let text: String = (x..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect();
                assert!(text.starts_with(&format!(
                    "{label}{}",
                    if sort.descending() { "↑" } else { "↓" }
                )));
            }
        }
        app.data.jobs.clear();
        app.data.pending.clear();
        app.rebuild();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let (column, row) = header_position(&terminal, "User");
        assert!(app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE
            },
            &worker
        ));
        assert_eq!(app.job_sort, job_view::Sort::User);
        app.apply(mouse_snapshot());
        for (tab, width) in [(0, 80), (0, 160), (5, 160)] {
            app.tab = tab;
            app.sort = 0;
            app.reverse = false;
            app.rebuild();
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            click_action(&mut app, &mut terminal, &worker, Click::Sort(1));
            assert_eq!(app.sort, 1);
            assert!(!app.reverse);
            click_action(&mut app, &mut terminal, &worker, Click::Sort(1));
            assert!(app.reverse);
            app.key(KeyCode::Char('S'), &worker);
            assert!(!app.reverse);
        }
    }

    #[test]
    fn mouse_respects_buttons_modals_search_and_rendered_scope() {
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(jobs_snapshot());
        let worker = Worker::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let (column, row) = header_position(&terminal, "User");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        for kind in [
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            assert!(!app.mouse(MouseEvent { kind, ..mouse }, &worker));
        }
        assert!(app.mouse(
            MouseEvent {
                row: row + 1,
                ..mouse
            },
            &worker
        ));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.mouse(
            MouseEvent {
                modifiers: KeyModifiers::SHIFT,
                ..mouse
            },
            &worker
        ));
        app.open(Popup::text("modal", "content".into()));
        assert!(!app.mouse(mouse, &worker));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.mouse(
            MouseEvent {
                column: 0,
                row: 0,
                ..mouse
            },
            &worker
        ));
        assert!(app.mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..mouse
            },
            &worker
        ));
        assert_eq!(app.popup.as_ref().unwrap().scroll, 1);
        app.popup = None;
        app.searching = true;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.mouse(mouse, &worker));
        app.searching = false;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..mouse
            },
            &worker
        ));
        assert_eq!(app.table.selected(), Some(1));
        app.tab = 0;
        assert!(!app.mouse(mouse, &worker));
        assert_eq!(app.job_sort, job_view::Sort::Priority);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(
            !app.hits
                .items
                .iter()
                .any(|(_, action)| matches!(action, Click::JobSort(_)))
        );
    }

    #[test]
    fn jobs_sort_state_and_account_filters_preserve_selection_and_tab_preferences() {
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(jobs_snapshot());
        let ids = |app: &App| {
            app.rows
                .iter()
                .map(|r| r.cells[0].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&app), ["42", "90", "43", "91", "92"]);
        assert_eq!(app.rows.last().unwrap().cells[12], "?");
        let worker = Worker::new();
        app.table.select(Some(1));
        app.key(KeyCode::Char('s'), &worker);
        assert_eq!(app.job_sort, job_view::Sort::User);
        assert_eq!(ids(&app), ["42", "90", "43", "91", "92"]);
        assert_eq!(app.selected().unwrap().job.as_deref(), Some("90"));
        app.key(KeyCode::Char('s'), &worker);
        assert_eq!(ids(&app), ["91", "90", "42", "92", "43"]);
        app.key(KeyCode::Char('S'), &worker);
        assert_eq!(ids(&app), ["43", "92", "42", "90", "91"]);
        app.key(KeyCode::Char('6'), &worker);
        app.key(KeyCode::Char('s'), &worker);
        app.key(KeyCode::Char('4'), &worker);
        assert_eq!(app.job_sort, job_view::Sort::Gpu);
        assert!(app.job_reverse);
        assert_eq!(app.sort, 1);
        app.key(KeyCode::Char('m'), &worker);
        assert_eq!(ids(&app), ["42", "90"]);
        app.key(KeyCode::Char('v'), &worker);
        assert_eq!(ids(&app), ["42"]);
        app.key(KeyCode::Char('v'), &worker);
        assert_eq!(ids(&app), ["90"]);
        let mut fresh = jobs_snapshot();
        fresh.pending[0].priority = "1000".into();
        app.apply(fresh);
        assert_eq!(app.selected().unwrap().job.as_deref(), Some("90"));
        app.key(KeyCode::Char('0'), &worker);
        assert_eq!(app.job_filter, job_view::Filter::All);
        assert_eq!(app.rows.len(), 5);
    }

    #[test]
    fn jobs_summary_and_search_follow_the_visible_queue() {
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.apply(jobs_snapshot());
        let summary = app.job_summary(runtime::now());
        assert_eq!(summary[0], "Jobs: 2 running · 3 pending · 2 users");
        assert!(summary[1].contains("+1 reasons"));
        assert!(summary[1].contains("max wait 2.0h"));
        app.mine = true;
        assert_eq!(
            app.job_summary(runtime::now())[0],
            "Jobs: 1 running · 1 pending · 1 users"
        );
        app.search = "normal".into();
        app.rebuild();
        assert_eq!(app.rows.len(), 2);
        app.search = "Resources".into();
        app.rebuild();
        assert_eq!(app.rows.len(), 1);
        assert!(app.job_summary(runtime::now())[1].contains("Resources ×1"));
        app.mine = false;
        app.search.clear();
        app.partition = "cpu".into();
        app.rebuild();
        assert_eq!(app.rows[0].job.as_deref(), Some("43"));
        assert_eq!(
            app.job_summary(runtime::now())[1],
            "Pending: none in current filters"
        );
    }

    #[test]
    fn text_palette_has_contrast_on_panels_selected_and_highlighted_rows() {
        let luminance = |color: Color| {
            let Color::Rgb(r, g, b) = color else {
                panic!("theme must use explicit RGB colors")
            };
            [r, g, b]
                .into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(v, weight)| {
                    let v = f64::from(v) / 255.0;
                    (if v <= 0.04045 {
                        v / 12.92
                    } else {
                        ((v + 0.055) / 1.055).powf(2.4)
                    }) * weight
                })
                .sum::<f64>()
        };
        for foreground in [
            TEXT, MUTED, BLUE, CYAN, GREEN, YELLOW, ORANGE, RED, PURPLE, FOCUS,
        ] {
            for background in [BACKGROUND, PANEL, HEADER_BG, SELECTED_BG, FOCUS_BG] {
                let ratio = (luminance(foreground) + 0.05) / (luminance(background) + 0.05);
                assert!(
                    ratio >= 4.5,
                    "low contrast {ratio:.2}: {foreground:?} on {background:?}"
                );
            }
        }
    }

    #[test]
    fn stale_cpu_rows_keep_cached_ram_and_load_unavailable() {
        let mut app = App::for_user("alice".into());
        app.tab = 1;
        app.apply(Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                stale: true,
                mem_total: "65536".into(),
                mem_avail: "32768".into(),
                cpu_load: "42".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(app.rows[0].cells[1], "STALE");
        assert_eq!(app.rows[0].cells[4], "?");
        assert_eq!(app.rows[0].cells[5], "?");
    }

    #[test]
    fn jobs_priority_reason_and_sort_controls_remain_visible_in_narrow_terminals() {
        let mut app = App::for_user("alice".into());
        app.tab = 3;
        app.job_filter = job_view::Filter::Pending;
        app.apply(jobs_snapshot());
        for width in [80, 120, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            for expected in [
                "Priority",
                "Req GPU",
                "Req CPU",
                "Resources",
                "state:pending",
                "sort:priority↓",
                "s Sort",
            ] {
                assert!(
                    text.contains(expected),
                    "missing {expected:?} at {width} columns"
                );
            }
            let lines: Vec<String> = terminal
                .backend()
                .buffer()
                .content
                .chunks(width as usize)
                .map(|row| row.iter().map(|c| c.symbol()).collect())
                .collect();
            let header = lines.iter().find(|line| line.contains("JobID")).unwrap();
            assert!(
                header.contains("Priority"),
                "priority header hidden at {width}"
            );
            assert!(header.contains("Reason"), "reason header hidden at {width}");
            assert!(
                lines.iter().any(|line| line.contains("PENDING")
                    && line.contains("Resources")
                    && line.contains("90")),
                "pending resource row hidden at {width}"
            );
        }
        app.data.errors = "controller query failed".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Scheduler unavailable"));
    }
    #[test]
    fn host_columns_units_guides_and_details_survive_resize_and_search() {
        let now = runtime::now();
        let mut app = App::for_user("viewer".into());
        app.tab = 6;
        app.apply(Snapshot { controller:serde_json::json!({"hostname":"controller","observed_at":now,"storage_observed_at":now,
            "cpu_util":30,"cpu_online":16,"load1":8,"load5":7,"load15":6,"memory_some_pct":0,
            "disks":[{"name":"nvme0n1","read_bps":1048576,"write_bps":0,"busy_pct":12,"await_ms":0.2}],
            "interfaces":[{"name":"eth0","rx_bps":1024,"tx_bps":1048576,"speed_mbps":10000,"link_pct":0.084,"state":"up"}],
            "filesystems":[{"mount":"/work","used_bytes":2748779069440_u64,"total_bytes":4398046511104_u64,"available_bytes":1649267441664_u64,"files_total":1000,"files_free":800,"files_available":700}]}), ..Default::default() });
        app.search = "/work".into();
        app.rebuild();
        let key = app.selected().unwrap().key.clone();
        for width in [80, 120, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("2.5 TiB"));
            assert!(text.contains("4.0 TiB"));
            assert!(text.contains("Now / R / RX"));
            assert!(text.contains("Total/W/TX"));
            assert!(text.contains("review hints"));
            assert!(!text.contains("GPU 0/0 FREE"));
            assert_eq!(app.selected().unwrap().key, key);
        }
        let worker = Worker::new();
        app.key(KeyCode::Enter, &worker);
        assert!(
            app.popup.as_ref().unwrap().panes[0]
                .body
                .contains("df-style")
        );
        app.popup = None;
        app.search = "nvme0n1".into();
        app.rebuild();
        assert_eq!(app.rows.len(), 2);
        app.key(KeyCode::Enter, &worker);
        let body = &app.popup.as_ref().unwrap().panes[0].body;
        assert!(body.contains("await"));
        assert!(body.contains("5m trend"));
        assert!(body.contains("not percent of maximum"));
    }
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
                .any(|c| c.symbol() == "━" && c.fg == GREEN)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "━" && c.fg == ORANGE)
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
        assert_eq!(buffer[(x as u16, hot as u16)].fg, RED);
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
        assert_eq!(MetricKind::Activity.color(Some(80.0)), GREEN);
        assert_eq!(MetricKind::Activity.color(Some(0.0)), MUTED);
        assert_eq!(MetricKind::Memory.color(Some(98.0)), RED);
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
                        "Req GPU",
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
