use crate::{
    cli,
    job_view::{Entry, Sort},
    model::Snapshot,
    rules, runtime,
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
    let mut out = format!(
        "{} pending · user:{} · partition:{} · published {}\n",
        report.total,
        if report.user.is_empty() {
            "all"
        } else {
            &report.user
        },
        if report.partition.is_empty() {
            "all"
        } else {
            &report.partition
        },
        report.snapshot
    );
    for (reason, count) in &report.reasons {
        out.push_str(&format!("  {reason}: {count}\n"));
    }
    out.push_str(&format!(
        "\n{:<14} {:<12} {:>10} {:>7} {:>6} {:>6}  {}\n",
        "JobID", "User", "Priority", "Wait", "GPU/n", "CPU/j", "Partition / QOS / reason"
    ));
    for job in &report.jobs {
        out.push_str(&format!(
            "{:<14} {:<12} {:>10} {:>7} {:>6} {:>6}  {} / {} / {}\n",
            job.jobid,
            job.user,
            job.priority,
            job.wait_seconds
                .map(rules::span)
                .unwrap_or_else(|| "?".into()),
            job.gpus_per_node,
            job.cpus_per_job,
            job.partition,
            job.qos,
            job.reason
        ));
    }
    if report.total > report.jobs.len() {
        out.push_str(&format!(
            "… {} more jobs; increase --limit to show more.\n",
            report.total - report.jobs.len()
        ));
    }
    out.push_str("\nPriority is a numeric score, not queue position. Start/admission depend on scheduler policy.\nUse sgpu explain JOBID for facts, interpretation and per-node observations.\n");
    cli::terminal_safe(&out)
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
