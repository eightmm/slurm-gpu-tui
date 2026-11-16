use crate::{
    cli,
    job_view::{Entry, Sort},
    model::Snapshot,
    rules, runtime,
    terminal::{Tone, cell, paint, reason_tone},
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub struct Job {
    jobid: String,
    user: String,
    partition: String,
    qos: String,
    priority: String,
    wait_seconds: Option<u64>,
    gpus_per_node: u32,
    cpus_per_job: u32,
    reason: String,
}
#[derive(Serialize)]
pub struct Report {
    pub total: usize,
    pub reasons: BTreeMap<String, usize>,
    snapshot: String,
    user: String,
    partition: String,
    jobs: Vec<Job>,
}
pub fn report(data: &Snapshot, user: &str, partition: &str, limit: usize) -> Report {
    let now = runtime::now();
    let mut jobs: Vec<_> = data
        .pending
        .iter()
        .filter(|job| {
            (user.is_empty() || job.user == user)
                && (partition.is_empty()
                    || job.partition.split(',').any(|value| value == partition))
        })
        .collect();
    jobs.sort_by(|a, b| Sort::Priority.compare(Entry::Pending(a), Entry::Pending(b), now, false));
    let mut reasons = BTreeMap::new();
    for job in &jobs {
        *reasons
            .entry(if job.reason.is_empty() {
                "unknown".into()
            } else {
                job.reason.clone()
            })
            .or_default() += 1;
    }
    Report {
        total: jobs.len(),
        reasons,
        snapshot: data.ts.clone(),
        user: user.into(),
        partition: partition.into(),
        jobs: jobs
            .into_iter()
            .take(limit)
            .map(|job| Job {
                jobid: job.jobid.clone(),
                user: job.user.clone(),
                partition: job.partition.clone(),
                qos: job.qos.clone(),
                priority: job.priority.clone(),
                wait_seconds: rules::pending_wait(job, now).map(|wait| wait as u64),
                gpus_per_node: job.gpu_count,
                cpus_per_job: job.cpu_count,
                reason: rules::pending_reason(job),
            })
            .collect(),
    }
}
pub fn render(report: &Report) -> String {
    render_with_color(report, false)
}
pub(crate) fn render_with_color(report: &Report, color: bool) -> String {
    let mark = |text: &str, tone| paint(text, tone, color);
    let column = |text: &str, width, right, tone| mark(&cell(text, width, right), tone);
    let id_width = report
        .jobs
        .iter()
        .map(|job| job.jobid.len())
        .max()
        .unwrap_or(14)
        .max(14);
    let mut out = format!(
        "{}\n{} pending · user:{} · partition:{}\n{}\n",
        mark("sgpu · pending queue", Tone::Heading),
        mark(
            &report.total.to_string(),
            if report.total == 0 {
                Tone::Good
            } else {
                Tone::Bold
            }
        ),
        if report.user.is_empty() {
            "all".into()
        } else {
            mark(&report.user, Tone::Mine)
        },
        if report.partition.is_empty() {
            "all".into()
        } else {
            mark(&report.partition, Tone::Value)
        },
        mark(&format!("Published {}", report.snapshot), Tone::Muted)
    );
    let current_user = cli::current_user();
    if !report.reasons.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            mark("── Pending reasons", Tone::Heading)
        ));
    }
    for (reason, count) in &report.reasons {
        out.push_str(&format!(
            "  {}: {}\n",
            mark(reason, reason_tone(reason)),
            mark(&count.to_string(), Tone::Bold)
        ));
    }
    out.push_str(&mark(
        &format!(
            "\n{:<id_width$} {:<12} {:>10} {:>7} {:>6} {:>6}  {}\n",
            "JobID", "User", "Priority", "Wait", "GPU/n", "CPU/j", "Partition / QOS / reason"
        ),
        Tone::Heading,
    ));
    for job in &report.jobs {
        let mine = job.user == current_user && !job.user.is_empty();
        out.push_str(&format!(
            "{} {} {} {} {} {}  {} / {} / {}\n",
            column(&job.jobid, id_width, false, Tone::Value),
            column(
                &if mine {
                    format!("{} *", job.user)
                } else {
                    job.user.clone()
                },
                12,
                false,
                if mine { Tone::Mine } else { Tone::Normal }
            ),
            column(&job.priority, 10, true, Tone::Value),
            column(
                &job.wait_seconds
                    .map(rules::span)
                    .unwrap_or_else(|| "?".into()),
                7,
                true,
                if job.wait_seconds.is_some() {
                    Tone::Normal
                } else {
                    Tone::Muted
                }
            ),
            column(&job.gpus_per_node.to_string(), 6, true, Tone::Bold),
            column(&job.cpus_per_job.to_string(), 6, true, Tone::Normal),
            mark(&job.partition, Tone::Normal),
            mark(&job.qos, Tone::Muted),
            mark(&job.reason, reason_tone(&job.reason))
        ));
    }
    if report.total > report.jobs.len() {
        out.push_str(&mark(
            &format!(
                "… {} more jobs; increase --limit to show more.\n",
                report.total - report.jobs.len()
            ),
            Tone::Muted,
        ));
    }
    out.push_str(&mark("\nPriority is a numeric score, not queue position. Start/admission depend on scheduler policy.\n* marks your jobs. Use sgpu explain JOBID for facts, interpretation and per-node observations.\n", Tone::Muted));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PendingJob;
    #[test]
    fn queue_filters_keep_totals_independent_of_limit_and_sort_priority_numerically() {
        let data = Snapshot {
            pending: vec![
                PendingJob {
                    jobid: "1".into(),
                    user: "alice".into(),
                    partition: "gpu".into(),
                    priority: "9".into(),
                    reason: "Priority".into(),
                    ..Default::default()
                },
                PendingJob {
                    jobid: "2".into(),
                    user: "alice".into(),
                    partition: "gpu".into(),
                    priority: "100".into(),
                    reason: "Resources".into(),
                    ..Default::default()
                },
                PendingJob {
                    jobid: "3".into(),
                    user: "bob".into(),
                    partition: "cpu".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let report = report(&data, "alice", "gpu", 1);
        assert_eq!(report.total, 2);
        assert_eq!(report.jobs[0].jobid, "2");
        assert_eq!(report.reasons.len(), 2);
        assert!(render(&report).contains("1 more jobs"));
    }
}
