use crate::{
    Result, cli,
    process::{self, Program},
    rules, runtime, scheduler,
};
use crossterm::style::Stylize;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    io::IsTerminal,
    path::PathBuf,
    time::Duration,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}
impl ColorMode {
    fn enabled(self, terminal: bool, no_color: bool, dumb: bool) -> bool {
        match self {
            Self::Auto => terminal && !no_color && !dumb,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

#[derive(Clone, Default)]
pub struct Inventory {
    pub state: String,
    pub partition: String,
    pub gpus: u32,
    pub cpus: u32,
    pub cpu_alloc: Option<u32>,
    pub cpu_idle: Option<u32>,
    pub reason: String,
}
#[derive(Default)]
pub struct Allocation {
    pub gpu: BTreeMap<String, BTreeMap<String, u32>>,
    pub cpu: BTreeMap<String, BTreeMap<String, u32>>,
    pub pending: BTreeMap<String, BTreeMap<String, u32>>,
    pub ends: BTreeMap<String, f64>,
    pub pending_reasons: BTreeMap<String, u32>,
    pub oldest_pending: Option<f64>,
}

pub fn parse_inventory(raw: &str, cpu: &str) -> BTreeMap<String, Inventory> {
    let mut out = BTreeMap::<String, Inventory>::new();
    let mut partitions = BTreeMap::<String, Vec<String>>::new();
    let mut members = BTreeMap::<String, BTreeSet<String>>::new();
    for line in raw.lines() {
        let p: Vec<_> = line.splitn(5, '|').map(str::trim).collect();
        if !matches!(p.len(), 4 | 5) {
            continue;
        }
        let name = p[0].split('.').next().unwrap_or("");
        if !runtime::valid_host(name) {
            continue;
        }
        let part = p[3].trim_end_matches('*');
        let entry = out.entry(name.into()).or_default();
        entry.state = p[1].into();
        entry.gpus = entry.gpus.max(scheduler::gpu_count(p[2]));
        if let Some(reason) = p.get(4).filter(|s| !matches!(**s, "" | "none" | "(null)")) {
            entry.reason = reason.to_string();
        }
        let parts = partitions.entry(name.into()).or_default();
        if !parts.iter().any(|p| p == part) {
            parts.push(part.into());
        }
        members.entry(part.into()).or_default().insert(name.into());
    }
    let gpu_only: Vec<_> = members
        .iter()
        .filter(|(_, nodes)| !nodes.is_empty() && nodes.iter().all(|n| out[n].gpus > 0))
        .map(|(p, _)| p.clone())
        .collect();
    for (name, parts) in partitions {
        out.get_mut(&name).unwrap().partition = parts
            .iter()
            .filter(|p| gpu_only.contains(p))
            .min_by_key(|p| {
                let lower = p.to_ascii_lowercase();
                let cpu = lower == "cpu"
                    || lower == "cpuonly"
                    || lower.starts_with("cpu_")
                    || lower.starts_with("cpu-");
                // Prefer a dedicated GPU partition over a broad CPU/default alias.
                (cpu, members[*p].len())
            })
            .or_else(|| parts.first())
            .cloned()
            .unwrap_or_default();
    }
    for line in cpu.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if p.len() != 2 {
            continue;
        }
        let name = p[0].split('.').next().unwrap_or("");
        if !runtime::valid_host(name) {
            continue;
        }
        let counts: Vec<_> = p[1]
            .split('/')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<_, _>>()
            .unwrap_or_default();
        if let [allocated, idle, other, total] = counts.as_slice()
            && allocated
                .checked_add(*idle)
                .and_then(|n| n.checked_add(*other))
                == Some(*total)
        {
            let entry = out.entry(name.into()).or_default();
            entry.cpus = *total;
            entry.cpu_alloc = Some(*allocated);
            entry.cpu_idle = Some(*idle);
        }
    }
    out
}

pub fn parse_allocations(raw: &str) -> Allocation {
    let mut out = Allocation::default();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if !matches!(p.len(), 9 | 10)
            || !scheduler::valid_job_id(p[0])
            || p[2].is_empty()
            || p[2].len() > 256
            || p[2].chars().any(char::is_control)
        {
            continue;
        }
        if p[3] == "PENDING" {
            *out.pending
                .entry(p[2].into())
                .or_default()
                .entry(p[1].into())
                .or_default() += 1;
            let reason = p[8].trim_matches(['(', ')']);
            *out.pending_reasons
                .entry(if reason.is_empty() { "unknown" } else { reason }.into())
                .or_default() += 1;
            if let Some(stamp) = p.get(9).and_then(|s| rules::parse_timestamp(s)) {
                out.oldest_pending = Some(out.oldest_pending.unwrap_or(stamp).min(stamp));
            }
            continue;
        }
        if !matches!(p[3], "RUNNING" | "COMPLETING") {
            continue;
        }
        let Some(nodes) = scheduler::expand_nodes(p[8]) else {
            continue;
        };
        if nodes.is_empty() {
            continue;
        }
        let per_node = scheduler::gpu_count(p[6]);
        let cpu = p[5].parse::<u32>().unwrap_or(0);
        let count = nodes.len() as u32;
        let end = (p[3] == "RUNNING")
            .then(|| rules::parse_timestamp(p[7]))
            .flatten();
        for (index, node) in nodes.iter().enumerate() {
            let node = node.split('.').next().unwrap_or(node).to_string();
            if per_node > 0 {
                let value = out
                    .gpu
                    .entry(p[2].into())
                    .or_default()
                    .entry(node.clone())
                    .or_default();
                *value = value.saturating_add(per_node);
                if let Some(end) = end {
                    let earliest = out.ends.entry(node.clone()).or_insert(end);
                    *earliest = earliest.min(end);
                }
            }
            if cpu > 0 {
                let allocated = cpu / count + u32::from((index as u32) < cpu % count);
                let value = out
                    .cpu
                    .entry(p[2].into())
                    .or_default()
                    .entry(node)
                    .or_default();
                *value = value.saturating_add(allocated);
            }
        }
    }
    out
}

pub fn natural(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (0, 0);
    let (a, b) = (a.as_bytes(), b.as_bytes());
    while ai < a.len() && bi < b.len() {
        if a[ai].is_ascii_digit() && b[bi].is_ascii_digit() {
            let (mut ae, mut be) = (ai, bi);
            while ae < a.len() && a[ae].is_ascii_digit() {
                ae += 1;
            }
            while be < b.len() && b[be].is_ascii_digit() {
                be += 1;
            }
            let mut az = ai;
            while az + 1 < ae && a[az] == b'0' {
                az += 1;
            }
            let mut bz = bi;
            while bz + 1 < be && b[bz] == b'0' {
                bz += 1;
            }
            let comparison = (ae - az).cmp(&(be - bz)).then(a[az..ae].cmp(&b[bz..be]));
            if comparison != Ordering::Equal {
                return comparison;
            }
            ai = ae;
            bi = be;
        } else {
            let comparison = a[ai].cmp(&b[bi]);
            if comparison != Ordering::Equal {
                return comparison;
            }
            ai += 1;
            bi += 1;
        }
    }
    a.len().cmp(&b.len())
}

pub fn eta(end: f64, now: f64) -> String {
    let seconds = (end - now).max(0.0) as u64;
    if seconds >= 86400 {
        format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}
fn ordered(rows: &BTreeMap<String, BTreeMap<String, u32>>) -> Vec<String> {
    let mut users: Vec<_> = rows.keys().cloned().collect();
    users.sort_by(|a, b| {
        rows[b]
            .values()
            .map(|n| *n as u64)
            .sum::<u64>()
            .cmp(&rows[a].values().map(|n| *n as u64).sum::<u64>())
            .then(a.cmp(b))
    });
    users
}
fn sum(rows: &BTreeMap<String, BTreeMap<String, u32>>, node: &str) -> u64 {
    rows.values()
        .map(|r| *r.get(node).unwrap_or(&0) as u64)
        .sum()
}
#[derive(Clone, Copy)]
enum Tone {
    Normal,
    Muted,
    Heading,
    Mine,
    Good,
    Busy,
    Alert,
    Bold,
}
struct Presentation {
    color: bool,
    width: usize,
    user: String,
    summary: bool,
}
impl Presentation {
    fn paint(&self, text: &str, tone: Tone) -> String {
        if !self.color {
            return text.into();
        }
        match tone {
            Tone::Normal => text.to_string(),
            Tone::Muted => text.dark_grey().to_string(),
            Tone::Heading => text.cyan().bold().to_string(),
            Tone::Mine => text.green().bold().to_string(),
            Tone::Good => text.green().to_string(),
            Tone::Busy => text.yellow().to_string(),
            Tone::Alert => text.red().bold().to_string(),
            Tone::Bold => text.bold().to_string(),
        }
    }
    fn user(&self, user: &str) -> (String, Tone) {
        if user == self.user && !user.is_empty() {
            (format!("{user} *"), Tone::Mine)
        } else {
            (user.into(), Tone::Normal)
        }
    }
    fn resource(&self, label: &str, used: u64, total: u64) -> String {
        if total == 0 {
            return format!("{label}: {used}/0 · no reported capacity\n");
        }
        let fraction = if total > 0 {
            used as f64 / total as f64
        } else {
            0.0
        };
        let filled = (fraction.clamp(0.0, 1.0) * 12.0).round() as usize;
        let tone = allocation_tone(used, total);
        format!(
            "{label} [{}{}]  {}\n",
            self.paint(&"━".repeat(filled), tone),
            self.paint(&"·".repeat(12 - filled), Tone::Muted),
            self.paint(&format!("{used}/{total} ({:.0}%)", fraction * 100.0), tone)
        )
    }
}
fn allocation_tone(used: u64, total: u64) -> Tone {
    if total == 0 {
        Tone::Muted
    } else if used >= total {
        Tone::Alert
    } else if used as f64 / total as f64 >= 0.75 {
        Tone::Busy
    } else {
        Tone::Good
    }
}
fn admitting(state: &str) -> bool {
    let state = state.to_ascii_lowercase();
    !state.contains('*')
        && ![
            "down", "drain", "fail", "maint", "reserv", "planned", "power", "reboot", "unknown",
        ]
        .iter()
        .any(|flag| state.contains(flag))
        && matches!(
            state.split('+').next().unwrap_or(""),
            "idle" | "mix" | "mixed" | "alloc" | "allocated"
        )
}
pub(crate) fn cell(raw: &str, width: usize, right: bool) -> String {
    let safe: String = raw.chars().filter(|c| !c.is_control()).collect();
    let mut text = String::new();
    let clipped = safe.width() > width;
    let mut remaining = width.saturating_sub(usize::from(clipped));
    for c in safe.chars() {
        let size = c.width().unwrap_or(0);
        if size > remaining {
            break;
        }
        text.push(c);
        remaining -= size;
    }
    if clipped {
        text.push('…');
    }
    let padding = " ".repeat(width.saturating_sub(text.width()));
    if right {
        format!("{padding}{text}")
    } else {
        format!("{text}{padding}")
    }
}
#[derive(Clone, Copy)]
struct TableContext<'a> {
    inventory: &'a BTreeMap<String, Inventory>,
    gpu: bool,
    ends: &'a BTreeMap<String, f64>,
    now: f64,
    view: &'a Presentation,
}
fn table(
    out: &mut String,
    title: &str,
    nodes: &[String],
    users: &[String],
    rows: &BTreeMap<String, BTreeMap<String, u32>>,
    context: TableContext<'_>,
) {
    let TableContext {
        inventory,
        gpu,
        ends,
        now,
        view,
    } = context;
    if nodes.is_empty() {
        return;
    }
    let width = nodes
        .iter()
        .map(|s| s.width() + 2)
        .max()
        .unwrap_or(9)
        .clamp(9, 24);
    let columns = (view.width.saturating_sub(26) / (width + 3)).max(1);
    let pages = nodes.len().div_ceil(columns);
    for (page, nodes) in nodes.chunks(columns).enumerate() {
        let label = if pages > 1 {
            format!("{title} · {}/{pages}", page + 1)
        } else {
            title.into()
        };
        out.push_str(&format!(
            "\n{}\n",
            view.paint(&format!("── {label}"), Tone::Heading)
        ));
        let separator = view.paint(" │ ", Tone::Muted);
        let rule = view.paint(
            &format!(
                "{}─┼─{}─┼─{}\n",
                "─".repeat(13),
                vec!["─".repeat(width); nodes.len()].join("─┼─"),
                "─".repeat(10)
            ),
            Tone::Muted,
        );
        for (label, partition) in [("Partition", true), ("User", false)] {
            out.push_str(&view.paint(&cell(label, 13, false), Tone::Heading));
            out.push_str(&separator);
            for node in nodes {
                let warning = !admitting(&inventory[node].state);
                let text = if partition {
                    inventory[node].partition.clone()
                } else {
                    format!("{}{}", node.to_uppercase(), if warning { "!" } else { "" })
                };
                out.push_str(&view.paint(
                    &cell(&text, width, true),
                    if warning {
                        Tone::Alert
                    } else if partition {
                        Tone::Muted
                    } else {
                        Tone::Heading
                    },
                ));
                out.push_str(&separator);
            }
            out.push_str(&view.paint(
                &cell(if partition { "" } else { "Total" }, 10, true),
                Tone::Heading,
            ));
            out.push('\n');
        }
        out.push_str(&rule);
        for user in users {
            let total = rows
                .get(user)
                .map(|r| {
                    nodes
                        .iter()
                        .filter_map(|node| r.get(node))
                        .map(|n| *n as u64)
                        .sum::<u64>()
                })
                .unwrap_or(0);
            if total == 0 {
                continue;
            }
            let (label, tone) = view.user(user);
            out.push_str(&view.paint(&cell(&label, 13, false), tone));
            out.push_str(&separator);
            for node in nodes {
                let value = rows
                    .get(user)
                    .and_then(|r| r.get(node))
                    .copied()
                    .filter(|v| *v > 0)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                out.push_str(&view.paint(&cell(&value, width, true), tone));
                out.push_str(&separator);
            }
            out.push_str(&view.paint(&cell(&total.to_string(), 10, true), Tone::Bold));
            out.push('\n');
        }
        out.push_str(&rule);
        out.push_str(&view.paint(&cell("Allocated", 13, false), Tone::Bold));
        out.push_str(&separator);
        let (mut used, mut capacity) = (0, 0u64);
        for node in nodes {
            let allocated = if gpu {
                sum(rows, node)
            } else {
                inventory[node]
                    .cpu_alloc
                    .map(u64::from)
                    .unwrap_or_else(|| sum(rows, node))
            };
            let total = if gpu {
                inventory[node].gpus
            } else {
                inventory[node].cpus
            } as u64;
            used += allocated;
            capacity += total;
            out.push_str(&view.paint(
                &cell(&format!("{allocated}/{total}"), width, true),
                allocation_tone(allocated, total),
            ));
            out.push_str(&separator);
        }
        out.push_str(&view.paint(
            &cell(&format!("{used}/{capacity}"), 10, true),
            allocation_tone(used, capacity),
        ));
        out.push('\n');
        if gpu && nodes.iter().any(|n| ends.contains_key(n)) {
            out.push_str(&view.paint(&cell("Next end ~", 13, false), Tone::Muted));
            out.push_str(&separator);
            for node in nodes {
                let value = ends
                    .get(node)
                    .filter(|_| sum(rows, node) > 0)
                    .map(|end| {
                        if *end > now {
                            eta(*end, now)
                        } else {
                            "?".into()
                        }
                    })
                    .unwrap_or_default();
                out.push_str(&view.paint(&cell(&value, width, true), Tone::Busy));
                out.push_str(&separator);
            }
            out.push_str(&cell("", 10, true));
            out.push('\n');
        }
    }
}

fn node_summary(
    out: &mut String,
    inventory: &BTreeMap<String, Inventory>,
    allocation: &Allocation,
    now: f64,
    view: &Presentation,
) {
    out.push_str(&format!(
        "\n{}\n",
        view.paint("── Node capacity", Tone::Heading)
    ));
    let name_width = (view.width / 5).clamp(8, 20);
    let full_cpu = view.width >= 85;
    let ends = view.width >= 100;
    let gpu_headroom = view.width >= 60;
    out.push_str(&view.paint(
        &format!(
            "{} {} {}{} {}{}{}\n",
            cell("Node", name_width, false),
            cell("State", 12, false),
            cell("GPU alloc", 9, true),
            if gpu_headroom {
                format!(" {}", cell("GPU unalloc", 11, true))
            } else {
                String::new()
            },
            cell("CPU idle", 8, true),
            if full_cpu {
                format!(" {}", cell("CPU alloc", 12, true))
            } else {
                String::new()
            },
            if ends {
                format!(" {}", cell("Next end ~", 10, true))
            } else {
                String::new()
            },
        ),
        Tone::Heading,
    ));
    let mut nodes: Vec<_> = inventory.iter().collect();
    nodes.sort_by(|a, b| natural(a.0, b.0));
    for (name, node) in nodes {
        let up = admitting(&node.state);
        let tone = if up { Tone::Normal } else { Tone::Alert };
        let idle = if up {
            node.cpu_idle
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into())
        } else {
            "—".into()
        };
        out.push_str(&format!(
            "{} {} {}{} {}{}{}\n",
            view.paint(&cell(name, name_width, false), tone),
            view.paint(
                &cell(
                    if node.state.is_empty() {
                        "unknown"
                    } else {
                        &node.state
                    },
                    12,
                    false
                ),
                tone
            ),
            cell(
                &format!("{}/{}", sum(&allocation.gpu, name), node.gpus),
                9,
                true
            ),
            if gpu_headroom {
                let idle = if up {
                    u64::from(node.gpus)
                        .saturating_sub(sum(&allocation.gpu, name))
                        .to_string()
                } else {
                    "—".into()
                };
                format!(" {}", cell(&idle, 11, true))
            } else {
                String::new()
            },
            cell(&idle, 8, true),
            if full_cpu {
                format!(
                    " {}",
                    cell(
                        &format!(
                            "{}/{}",
                            node.cpu_alloc
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?".into()),
                            node.cpus
                        ),
                        12,
                        true
                    )
                )
            } else {
                String::new()
            },
            if ends {
                format!(
                    " {}",
                    cell(
                        &allocation
                            .ends
                            .get(name)
                            .filter(|end| **end > now)
                            .map(|end| eta(*end, now))
                            .unwrap_or_else(|| "?".into()),
                        10,
                        true
                    )
                )
            } else {
                String::new()
            },
        ));
        if !up && !node.reason.is_empty() {
            out.push_str(&format!(
                "  {}\n",
                view.paint(
                    cell(&node.reason, view.width.saturating_sub(2), false).trim_end(),
                    Tone::Muted
                )
            ));
        }
    }
}

pub fn render(
    inventory: &BTreeMap<String, Inventory>,
    allocation: &Allocation,
    now: f64,
) -> String {
    render_with(
        inventory,
        allocation,
        now,
        &Presentation {
            color: false,
            width: 100,
            user: String::new(),
            summary: false,
        },
    )
}
fn render_with(
    inventory: &BTreeMap<String, Inventory>,
    allocation: &Allocation,
    now: f64,
    view: &Presentation,
) -> String {
    let mut gpu_nodes: Vec<_> = inventory
        .iter()
        .filter(|(_, n)| n.gpus > 0)
        .map(|(name, _)| name.clone())
        .collect();
    gpu_nodes.sort_by(|a, b| natural(a, b));
    let gpu_users = ordered(&allocation.gpu);
    let mut users = gpu_users.clone();
    users.extend(
        ordered(&allocation.cpu)
            .into_iter()
            .filter(|u| !gpu_users.contains(u)),
    );
    let mut out = String::new();
    if !view.summary {
        table(
            &mut out,
            "GPU allocation",
            &gpu_nodes,
            &gpu_users,
            &allocation.gpu,
            TableContext {
                inventory,
                gpu: true,
                ends: &allocation.ends,
                now,
                view,
            },
        );
        for (category, label) in [
            (0, "CPU Threads — GPU nodes"),
            (1, "CPU Threads — CPU nodes"),
            (2, "CPU Threads — Other nodes"),
        ] {
            let mut nodes: Vec<_> = inventory
                .iter()
                .filter(|(name, n)| {
                    n.cpus > 0
                        && match category {
                            0 => n.gpus > 0,
                            1 => n.gpus == 0 && name.starts_with("cpu"),
                            _ => n.gpus == 0 && !name.starts_with("cpu"),
                        }
                })
                .map(|(name, _)| name.clone())
                .collect();
            nodes.sort_by(|a, b| natural(a, b));
            table(
                &mut out,
                label,
                &nodes,
                &users,
                &allocation.cpu,
                TableContext {
                    inventory,
                    gpu: false,
                    ends: &BTreeMap::new(),
                    now,
                    view,
                },
            );
        }
        if !allocation.pending.is_empty() {
            out.push_str(&format!(
                "\n{}\n",
                view.paint("── Pending jobs", Tone::Heading)
            ));
            for user in ordered(&allocation.pending) {
                let row = &allocation.pending[&user];
                out.push_str(&format!(
                    "{}: {} ({})\n",
                    view.paint(&cell(&view.user(&user).0, 13, false), view.user(&user).1),
                    row.values().map(|n| *n as u64).sum::<u64>(),
                    row.iter()
                        .map(|(p, n)| format!("{}:{n}", cli::terminal_safe(p)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    } else {
        node_summary(&mut out, inventory, allocation, now, view);
    }
    if !allocation.pending_reasons.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            view.paint("── Pending reasons", Tone::Heading)
        ));
        let mut reasons: Vec<_> = allocation.pending_reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (reason, count) in reasons {
            out.push_str(&format!(
                "  {count:>5}  {}\n",
                view.paint(
                    &cell(reason, view.width.saturating_sub(10), false),
                    Tone::Busy
                )
                .trim_end()
            ));
        }
        out.push_str(&format!(
            "Oldest pending: {}\n",
            allocation
                .oldest_pending
                .filter(|t| *t > 0.0 && *t <= now + 5.0)
                .map(|t| rules::span((now - t).max(0.0) as u64))
                .unwrap_or_else(|| "?".into())
        ));
    }
    let gpu_used = gpu_nodes
        .iter()
        .map(|n| sum(&allocation.gpu, n))
        .sum::<u64>();
    let gpu_total = inventory.values().map(|n| n.gpus as u64).sum::<u64>();
    let cpu_used = inventory
        .keys()
        .map(|n| {
            inventory[n]
                .cpu_alloc
                .map(u64::from)
                .unwrap_or_else(|| sum(&allocation.cpu, n))
        })
        .sum::<u64>();
    let cpu_total = inventory.values().map(|n| n.cpus as u64).sum::<u64>();
    let timestamp = chrono::DateTime::from_timestamp(now as i64, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y/%m/%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    let mut header = format!(
        "\n{}\n{}\n\n",
        view.paint("sgpu · cluster allocation", Tone::Heading),
        view.paint(&timestamp, Tone::Muted)
    );
    header.push_str(&view.resource("GPU", gpu_used, gpu_total));
    header.push_str(&view.resource(
        if inventory.values().any(|n| n.cpu_alloc.is_none()) {
            "CPU ~"
        } else {
            "CPU"
        },
        cpu_used,
        cpu_total,
    ));
    let blocked = inventory.values().filter(|n| !admitting(&n.state)).count();
    let unallocated: u64 = inventory
        .iter()
        .filter(|(_, n)| admitting(&n.state))
        .map(|(name, n)| u64::from(n.gpus).saturating_sub(sum(&allocation.gpu, name)))
        .sum();
    header.push_str(&format!(
        "Nodes: {} · blocked/unknown {blocked} · GPU unallocated on up nodes: {unallocated}\n",
        inventory.len()
    ));
    let pending: u64 = allocation
        .pending
        .values()
        .flat_map(|r| r.values())
        .map(|n| u64::from(*n))
        .sum();
    header.push_str(&format!("Pending jobs: {pending}\n"));
    header.push_str(&out);
    out = header;
    for note in [
        "Allocation, not measured utilization.",
        "Next end ~ follows job time limits, not actual finishes.",
        "Unallocated is not an admission guarantee. * marks your jobs.",
        "CPU user rows split multi-node requests; node totals use sinfo.",
        "! marks blocked/unknown nodes. --summary shows node details.",
    ] {
        out.push_str(&view.paint(note, Tone::Muted));
        out.push('\n');
    }
    out
}

fn conf_fallback(inventory: &mut BTreeMap<String, Inventory>) {
    let mut candidates = std::env::var_os("SLURM_CONF")
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if let Ok(raw) = process::run(
        Program::Scontrol,
        &["show", "config"],
        Duration::from_secs(10),
    ) {
        for line in raw.lines() {
            if let Some(path) = line
                .strip_prefix("SLURM_CONF")
                .and_then(|s| s.split_once('=').map(|(_, v)| v.trim()))
            {
                candidates.push(path.into());
            }
        }
    }
    candidates.extend(
        [
            "/etc/slurm/slurm.conf",
            "/etc/slurm-llnl/slurm.conf",
            "/usr/local/etc/slurm.conf",
            "/opt/slurm/etc/slurm.conf",
        ]
        .map(PathBuf::from),
    );
    let config = candidates.iter().find_map(|p| {
        std::fs::canonicalize(p)
            .ok()
            .and_then(|p| runtime::read_regular(&p, 4 * 1024 * 1024, None).ok())
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
    });
    let Some(config) = config else {
        return;
    };
    let mut defaults = BTreeMap::new();
    for line in config.lines() {
        let fields: BTreeMap<_, _> = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|s| {
                s.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        let Some(names) = fields.get("NodeName") else {
            continue;
        };
        if names == "DEFAULT" {
            defaults.extend(fields);
            continue;
        }
        let gres = fields
            .get("Gres")
            .or_else(|| defaults.get("Gres"))
            .map(String::as_str)
            .unwrap_or("");
        let gpus = scheduler::gpu_count(gres);
        if gpus == 0 {
            continue;
        }
        for name in scheduler::expand_nodes(names).unwrap_or_default() {
            inventory.entry(name).or_default().gpus = gpus;
        }
    }
}

pub fn run(color: ColorMode, summary: bool) -> Result<()> {
    let (queue, nodes, cpu) = std::thread::scope(|scope| {
        let queue = scope.spawn(|| {
            process::run(
                Program::Squeue,
                &["--noheader", "-o", "%i|%P|%u|%T|%M|%C|%b|%e|%R|%V"],
                Duration::from_secs(12),
            )
        });
        let nodes = scope.spawn(|| {
            process::run(
                Program::Sinfo,
                &["-N", "--noheader", "-o", "%n|%T|%G|%P|%E"],
                Duration::from_secs(12),
            )
        });
        let cpu = process::run(
            Program::Sinfo,
            &["-N", "-h", "-o", "%N|%C"],
            Duration::from_secs(12),
        );
        (
            queue
                .join()
                .unwrap_or_else(|_| Err("queue worker failed".into())),
            nodes
                .join()
                .unwrap_or_else(|_| Err("inventory worker failed".into())),
            cpu,
        )
    });
    let queue = queue?;
    let mut inventory = parse_inventory(&nodes?, &cpu?);
    if !inventory.values().any(|n| n.gpus > 0) {
        conf_fallback(&mut inventory);
    }
    if inventory.is_empty() {
        return Err("no scheduler nodes found".into());
    }
    let color = color.enabled(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("TERM").is_ok_and(|v| v == "dumb"),
    );
    crossterm::style::force_color_output(color);
    print!(
        "{}",
        render_with(
            &inventory,
            &parse_allocations(&queue),
            runtime::now(),
            &Presentation {
                color,
                width: crossterm::terminal::size()
                    .map(|s| s.0 as usize)
                    .unwrap_or(100)
                    .max(40),
                user: cli::current_user(),
                summary,
            }
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_excludes_blocked_capacity_counts_completing_and_explains_pending() {
        let inventory = parse_inventory(
            "gpu1|mixed|gpu:4|gpu|none\ngpu2|drain|gpu:8|gpu|maintenance\ngpu3|idle*|gpu:2|gpu|not responding\ncpu1|idle|(null)|cpu|none",
            "gpu1|4/4/0/8\ngpu2|0/0/8/8\ngpu3|0/0/8/8\ncpu1|0/8/0/8",
        );
        let start = "2026-11-16T00:00:00";
        let now = rules::parse_timestamp(start).unwrap() + 7200.0;
        let allocation = parse_allocations(&format!(
            "42|gpu|alice|RUNNING|00:01|2|gpu:1|2026-11-16T04:00:00|gpu1|{start}\n43|gpu|bob|COMPLETING|00:01|2|gpu:1|2026-11-16T00:30:00|gpu1|{start}\n44|gpu|alice|PENDING|00:00|4|gpu:2|N/A|(Resources)|{start}"
        ));
        assert_eq!(sum(&allocation.gpu, "gpu1"), 2);
        assert_eq!(allocation.pending_reasons["Resources"], 1);
        assert_eq!(
            allocation.ends["gpu1"],
            rules::parse_timestamp("2026-11-16T04:00:00").unwrap()
        );
        let view = Presentation {
            color: false,
            width: 100,
            user: String::new(),
            summary: true,
        };
        let summary = render_with(&inventory, &allocation, now, &view);
        assert!(summary.contains("Node capacity"));
        assert!(summary.contains("GPU unalloc") && summary.contains("CPU idle"));
        assert!(summary.contains("blocked/unknown 2"));
        assert!(summary.contains("GPU unallocated on up nodes: 2"));
        assert!(summary.contains("Pending jobs: 1"));
        assert!(summary.contains("Oldest pending: 2.0h"));
        assert!(summary.contains("maintenance"));
        assert!(!summary.contains("GPU allocation"));
        let blocked = summary.lines().find(|l| l.starts_with("gpu2 ")).unwrap();
        assert!(blocked.contains('—'));
        let matrix = render(&inventory, &allocation, now);
        assert!(matrix.contains("GPU allocation") && matrix.contains("Pending reasons"));
        for state in [
            "down",
            "draining",
            "reserved",
            "mixed+drain",
            "idle*",
            "unknown",
            "completing",
        ] {
            assert!(!admitting(state), "{state}");
        }
    }

    #[test]
    fn summary_preserves_missing_cpu_counts_and_bounded_node_rows() {
        let inventory = parse_inventory(
            "gpu1|idle|gpu:4|gpu\ngpu2|idle|gpu:4|gpu",
            "gpu1|malformed\ngpu2|4/4/0/9",
        );
        assert!(inventory["gpu1"].cpu_idle.is_none());
        assert!(inventory["gpu2"].cpu_alloc.is_none());
        for width in [40, 60, 80, 100] {
            let view = Presentation {
                color: false,
                width,
                user: String::new(),
                summary: true,
            };
            let summary = render_with(&inventory, &Allocation::default(), 0.0, &view);
            for row in summary.lines().filter(|l| l.starts_with("gpu")) {
                assert!(row.contains('?'));
                assert!(row.width() <= width, "{}: {row}", row.width());
            }
        }
    }
    #[test]
    fn gpu_partition_labels_prefer_dedicated_members_over_cpu_aliases() {
        let lines = [
            "gpu1|idle|gpu:8|cpu_only*",
            "gpu2|idle|gpu:8|cpu_only*",
            "gpu1|idle|gpu:8|heavy",
            "gpu2|idle|gpu:8|heavy",
            "gpu3|idle|gpu:4|heavy",
            "gpu1|idle|gpu:8|6000ada",
            "gpu2|idle|gpu:8|6000ada",
        ];
        for raw in [
            lines.join("\n"),
            lines.into_iter().rev().collect::<Vec<_>>().join("\n"),
        ] {
            let inventory = parse_inventory(&raw, "");
            assert_eq!(inventory["gpu1"].partition, "6000ada");
            assert_eq!(inventory["gpu2"].partition, "6000ada");
            assert_eq!(inventory["gpu3"].partition, "heavy");
        }
    }
    #[test]
    fn terminal_colors_respect_pipes_opt_out_and_explicit_overrides() {
        assert!(ColorMode::Auto.enabled(true, false, false));
        assert!(!ColorMode::Auto.enabled(false, false, false));
        assert!(!ColorMode::Auto.enabled(true, true, false));
        assert!(!ColorMode::Auto.enabled(true, false, true));
        assert!(ColorMode::Always.enabled(false, true, true));
        assert!(!ColorMode::Never.enabled(true, false, false));
    }
    #[test]
    fn narrow_matrices_keep_unicode_cells_aligned_and_never_emit_untrusted_controls() {
        let inventory = parse_inventory(
            "gpu[invalid]|idle|gpu:8|gpu\ngpu1|idle|gpu:8|long-partition\ngpu2|drain|gpu:8|gpu\ngpu3|idle|gpu:8|gpu",
            "gpu1|0/64/0/64\ngpu2|0/64/0/64\ngpu3|0/64/0/64",
        );
        let allocation =
            parse_allocations("42|gpu|사용자이름이매우김|RUNNING|00:01|64|gpu:4|N/A|gpu[1-3]");
        let plain = render_with(
            &inventory,
            &allocation,
            0.0,
            &Presentation {
                color: false,
                width: 50,
                user: String::new(),
                summary: false,
            },
        );
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("GPU allocation · 1/"));
        assert!(plain.contains("GPU2!"));
        for line in plain
            .lines()
            .filter(|line| line.contains('│') || line.contains('┼'))
        {
            assert!(line.width() <= 50, "{}: {line}", line.width());
            assert!([38, 50].contains(&line.width()));
        }
        assert_eq!(cell("가나다라마바사", 9, false).width(), 9);
        assert_eq!(cell("bad\x1b\n\tname", 12, false), "badname     ");
    }
    #[test]
    fn multi_node_gpu_counts_cpu_remainder_and_pending_are_distinct() {
        let allocation = parse_allocations(
            "42|gpu|alice|RUNNING|00:01|5|gpu:h100:2|2026-07-01T12:00:00|gpu[1-2]\n43|gpu|bob|PENDING|00:00|4|gpu:8|N/A|(Priority)",
        );
        assert_eq!(allocation.gpu["alice"]["gpu1"], 2);
        assert_eq!(allocation.gpu["alice"]["gpu2"], 2);
        assert_eq!(allocation.cpu["alice"]["gpu1"], 3);
        assert_eq!(allocation.cpu["alice"]["gpu2"], 2);
        assert_eq!(allocation.pending["bob"]["gpu"], 1);
    }
    #[test]
    fn gpu_controller_and_duplicate_partitions_keep_one_capacity() {
        let inventory = parse_inventory(
            "master|idle|gpu:h100:2|cpu*\nmaster|idle|gpu:h100:2|gpu\ncpu1|idle|(null)|cpu",
            "master|0/8/0/8\ncpu1|0/16/0/16",
        );
        assert_eq!(inventory["master"].gpus, 2);
        assert_eq!(inventory["master"].partition, "gpu");
        assert_eq!(inventory["cpu1"].cpus, 16);
        assert_eq!(eta(90000.0, 0.0), "1d1h");
        assert_eq!(eta(-1.0, 0.0), "0h00m");
        assert_eq!(natural("gpu2", "gpu10"), Ordering::Less);
    }
}
