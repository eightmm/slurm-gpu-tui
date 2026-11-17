use crate::{
    Result,
    job_view::Entry,
    model::{Node, Snapshot, json_number},
    rules, runtime,
    terminal::{self, ColorMode, Tone},
};
use serde::Deserialize;
use std::{cmp::Reverse, os::unix::fs::MetadataExt, path::PathBuf};

#[derive(Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Scope {
    All,
    Cluster,
    Personal,
}

#[derive(clap::Args)]
pub struct Options {
    #[arg(
        long,
        value_enum,
        default_value = "all",
        help = "Shared MOTD must use cluster; personal information is for the user's own shell"
    )]
    pub scope: Scope,
    #[arg(long, value_enum, default_value = "auto")]
    pub color: ColorMode,
    #[arg(long, value_parser = clap::value_parser!(u16).range(16..=240))]
    pub width: Option<u16>,
    #[arg(
        long,
        default_value = "/etc/sgpu/welcome.json",
        help = "Optional public, administrator-owned login display settings"
    )]
    pub config: PathBuf,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    title: String,
    notice_file: Option<PathBuf>,
    guide: String,
    hints: Vec<String>,
}

fn public_file(path: &std::path::Path, limit: u64) -> Result<Vec<u8>> {
    let (bytes, st) = runtime::read_regular(path, limit, Some(0))?;
    // A root-run shared MOTD may publish only explicitly public admin text.
    if st.mode() & 0o022 != 0 || st.mode() & 0o004 == 0 {
        return Err("login text must be public and not writable by other accounts".into());
    }
    Ok(bytes)
}

pub fn run(options: Options) -> Result<i32> {
    let config = match public_file(&options.config, 8192) {
        Ok(bytes) => serde_json::from_slice::<Config>(&bytes).ok(),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Some(Config::default())
        }
        Err(_) => None,
    };
    let notices = if options.scope == Scope::Personal {
        None
    } else {
        config
            .as_ref()
            .and_then(|c| c.notice_file.as_ref())
            .map(|path| public_file(path, 8192))
    };
    let width = options
        .width
        .or_else(|| crossterm::terminal::size().ok().map(|s| s.0))
        .unwrap_or(80)
        .clamp(16, 240) as usize;
    let mut lines = Lines {
        text: String::new(),
        width,
        color: options.color.stdout(),
    };
    let config_bad = config.is_none();
    let config = config.unwrap_or_default();
    let data = runtime::read_snapshot(Some(30.0)).and_then(|data| {
        let stamp = chrono::DateTime::parse_from_rfc3339(&data.ts)?;
        if !(-5.0..=30.0).contains(&(runtime::now() - stamp.timestamp_millis() as f64 / 1000.0)) {
            return Err("collector snapshot is stale".into());
        }
        Ok(data)
    });
    let identity = if options.scope == Scope::Cluster {
        None
    } else {
        runtime::user(runtime::euid()).map(|u| (runtime::euid(), u.name))
    };
    render(
        &mut lines,
        data.as_ref().ok(),
        &config,
        options.scope,
        identity.as_ref().map(|(uid, name)| (*uid, name.as_str())),
    );
    if options.scope != Scope::Personal {
        if config_bad {
            lines.push("! Login display settings unavailable", Tone::Busy);
        }
        if let Some(notices) = notices {
            match notices {
                Ok(bytes) => notice_lines(
                    &mut lines,
                    &String::from_utf8_lossy(&bytes),
                    chrono::Local::now().date_naive(),
                ),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                Err(_) => lines.push("! Administrator notice unavailable", Tone::Busy),
            }
        }
        if !config.guide.is_empty() {
            lines.push(&format!("Guide: {}", config.guide), Tone::Muted);
        }
        for hint in config
            .hints
            .iter()
            .filter(|hint| !hint.trim().is_empty())
            .take(3)
        {
            lines.push(hint, Tone::Muted);
        }
        lines.push("Open: sgpu | chkgpu --summary", Tone::Value);
        lines.push("Jobs: chkqueue --mine | chkfail", Tone::Value);
    }
    print!("{}", lines.text);
    // An unavailable collector must never prevent a login or start a query.
    Ok(0)
}

struct Lines {
    text: String,
    width: usize,
    color: bool,
}
impl Lines {
    fn push(&mut self, raw: &str, tone: Tone) {
        let clipped = terminal::cell(raw, self.width, false);
        self.text
            .push_str(&terminal::paint(clipped.trim_end(), tone, self.color));
        self.text.push('\n');
    }
}

fn node_tone(node: &Node) -> Tone {
    if ["down", "drain", "fail", "maint", "unknown"]
        .iter()
        .any(|s| node.state.to_lowercase().contains(s))
    {
        Tone::Alert
    } else if node.stale || !node.error.is_empty() {
        Tone::Busy
    } else {
        Tone::Normal
    }
}

fn render(
    out: &mut Lines,
    data: Option<&Snapshot>,
    config: &Config,
    scope: Scope,
    identity: Option<(u32, &str)>,
) {
    if scope != Scope::Personal {
        out.push(
            if config.title.is_empty() {
                "Oh My Slurm"
            } else {
                &config.title
            },
            Tone::Heading,
        );
    }
    let Some(data) = data else {
        out.push(
            "! Snapshot unavailable or older than 30s; try sgpu doctor",
            Tone::Busy,
        );
        return;
    };
    let scheduler_ok = data.errors.is_empty()
        && data.scheduler.error.is_empty()
        && data.nodes.iter().all(|n| n.scheduler_available);
    if scope != Scope::Personal {
        out.push(
            &format!(
                "{} | published {}",
                data.controller["hostname"].as_str().unwrap_or("cluster"),
                data.ts
            ),
            Tone::Muted,
        );
        if scheduler_ok {
            out.push(
                &format!(
                    "{} nodes | {} running | {} pending",
                    data.nodes.len(),
                    data.jobs.len(),
                    data.pending.len()
                ),
                Tone::Bold,
            );
            let mut nodes: Vec<_> = data.nodes.iter().collect();
            nodes.sort_by_key(|n| (!matches!(node_tone(n), Tone::Alert), !n.stale, &n.name));
            for node in nodes.iter().take(6) {
                let cpus = |raw: &str| {
                    raw.parse::<u64>()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|_| "?".into())
                };
                let gpu = if node.has_gpu {
                    let count = node
                        .gpus
                        .len()
                        .max(crate::scheduler::gpu_count(&node.gres) as usize);
                    let allocated: u64 = node.jobs.iter().map(|job| u64::from(job.gpu_count)).sum();
                    format!(
                        " | GPU {allocated}/{} allocated",
                        if count == 0 || allocated > count as u64 {
                            "?".into()
                        } else {
                            count.to_string()
                        }
                    )
                } else {
                    String::new()
                };
                out.push(
                    &format!(
                        "{}{} {} | CPU {}/{} allocated{}",
                        if node.stale { "STALE " } else { "" },
                        node.name,
                        node.state,
                        cpus(&node.cpu_alloc),
                        cpus(&node.cpus),
                        gpu
                    ),
                    node_tone(node),
                );
            }
            if nodes.len() > 6 {
                out.push(
                    &format!("... {} more nodes: sgpu", nodes.len() - 6),
                    Tone::Muted,
                );
            }
        } else {
            out.push(
                "! Scheduler unavailable; current job/allocation counts are unknown",
                Tone::Alert,
            );
        }
        let fresh = data
            .nodes
            .iter()
            .filter(|n| !n.stale && n.error.is_empty())
            .count();
        out.push(
            &format!("Telemetry: {fresh}/{} nodes fresh", data.nodes.len()),
            if fresh == data.nodes.len() {
                Tone::Good
            } else {
                Tone::Busy
            },
        );
        if crate::users::storage_fresh(&data.controller, runtime::now()) {
            let mut shown = 0;
            for fs in data.controller["filesystems"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let (Some(used), Some(avail)) = (
                    json_number(&fs["used_bytes"]),
                    json_number(&fs["available_bytes"]),
                ) && used >= 0.0
                    && avail >= 0.0
                    && used + avail > 0.0
                    && used / (used + avail) >= 0.8
                {
                    out.push(
                        &format!(
                            "! Storage {}: {:.0}% used",
                            fs["mount"].as_str().unwrap_or("?"),
                            100.0 * used / (used + avail)
                        ),
                        if used / (used + avail) >= 0.9 {
                            Tone::Alert
                        } else {
                            Tone::Busy
                        },
                    );
                    shown += 1;
                    if shown == 3 {
                        out.push("Storage details: sgpu host", Tone::Muted);
                        break;
                    }
                }
            }
        }
    }
    if scope != Scope::Cluster && scheduler_ok {
        let Some((uid, user)) = identity else {
            out.push(
                "! Personal summary unavailable: account lookup failed",
                Tone::Busy,
            );
            return;
        };
        let running: Vec<_> = data
            .jobs
            .iter()
            .filter(|j| {
                if j.uid >= 0 {
                    j.uid == i64::from(uid)
                } else {
                    j.user == user
                }
            })
            .collect();
        let mut pending: Vec<_> = data.pending.iter().filter(|j| j.user == user).collect();
        pending.sort_by_key(|j| Reverse(j.priority.parse::<u64>().unwrap_or(0)));
        out.push(
            &format!(
                "My jobs ({user}): {} running | {} pending",
                running.len(),
                pending.len()
            ),
            Tone::Mine,
        );
        for job in running.iter().take(2) {
            let left = Entry::Running(job)
                .left()
                .map(rules::span)
                .unwrap_or_else(|| "?".into());
            out.push(
                &format!(
                    "RUN #{} {} | CPU {} GPU {} | left {}",
                    job.jobid, job.jobname, job.cpu_count, job.gpu_count, left
                ),
                Tone::Good,
            );
        }
        for job in pending.iter().take(2) {
            out.push(
                &format!(
                    "WAIT #{} {} | {}",
                    job.jobid,
                    job.jobname,
                    rules::pending_reason(job)
                ),
                terminal::reason_tone(&job.reason),
            );
        }
        if running.len() > 2 || pending.len() > 2 {
            out.push("More: sgpu me | chkqueue --mine", Tone::Muted);
        }
    } else if scope == Scope::Personal && !scheduler_ok {
        out.push(
            "! Scheduler unavailable; personal job counts are unknown",
            Tone::Alert,
        );
    }
}

fn notice_lines(out: &mut Lines, raw: &str, today: chrono::NaiveDate) {
    for line in raw
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            if let Some(rest) = line.strip_prefix("[until ") {
                let (date, text) = rest.split_once(']')?;
                let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
                (date >= today && !text.trim().is_empty()).then_some(text.trim())
            } else {
                Some(line)
            }
        })
        .take(3)
    {
        out.push(&format!("Notice: {line}"), Tone::Busy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Job, PendingJob};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn shared_and_personal_scopes_keep_accounts_states_and_terminal_width_distinct() {
        let data = Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                state: "mixed+planned".into(),
                gres: "gpu:h100:2".into(),
                jobs: vec![Job {
                    gpu_count: 1,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            jobs: vec![
                Job {
                    jobid: "1".into(),
                    user: "alice".into(),
                    uid: 1001,
                    jobname: "MY_JOB".into(),
                    ..Default::default()
                },
                Job {
                    jobid: "2".into(),
                    user: "alice".into(),
                    uid: 1002,
                    jobname: "OTHER_UID".into(),
                    ..Default::default()
                },
            ],
            pending: vec![PendingJob {
                jobid: "3".into(),
                user: "alice".into(),
                reason: "DependencyNeverSatisfied".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(node_tone(&data.nodes[0]), Tone::Normal));
        let mut out = Lines {
            text: String::new(),
            width: 120,
            color: false,
        };
        render(
            &mut out,
            Some(&data),
            &Config::default(),
            Scope::Cluster,
            Some((1001, "alice")),
        );
        assert!(!out.text.contains("My jobs") && !out.text.contains("MY_JOB"));
        assert!(out.text.contains("GPU 1/2 allocated"));
        out.text.clear();
        render(
            &mut out,
            Some(&data),
            &Config::default(),
            Scope::Personal,
            Some((1001, "alice")),
        );
        assert!(out.text.contains("1 running | 1 pending") && out.text.contains("MY_JOB"));
        assert!(!out.text.contains("OTHER_UID"));
        out.width = 16;
        out.text.clear();
        render(
            &mut out,
            Some(&data),
            &Config {
                title: "한글 긴 제목\x1b\r\n".into(),
                ..Default::default()
            },
            Scope::All,
            Some((1001, "alice")),
        );
        assert!(out.text.lines().all(|l| l.width() <= 16));
        assert!(!out.text.contains(['\x1b', '\r']));
    }

    #[test]
    fn expired_notices_and_failed_scheduler_never_claim_current_totals() {
        let mut out = Lines {
            text: String::new(),
            width: 100,
            color: false,
        };
        let data = Snapshot {
            errors: "scheduler timeout".into(),
            ..Default::default()
        };
        render(
            &mut out,
            Some(&data),
            &Config::default(),
            Scope::All,
            Some((1001, "alice")),
        );
        assert!(out.text.contains("Scheduler unavailable"));
        assert!(!out.text.contains("0 running") && !out.text.contains("My jobs"));
        out.text.clear();
        notice_lines(
            &mut out,
            "# comment\n[until 2026-01-01] expired\n[until 2026-11-17] today\n[until bad] invalid\nhello\n",
            chrono::NaiveDate::from_ymd_opt(2026, 11, 17).unwrap(),
        );
        assert!(out.text.contains("today") && out.text.contains("hello"));
        assert!(!out.text.contains("expired") && !out.text.contains("invalid"));
    }
}
