use crate::{
    Result, cli,
    process::{self, Program},
    runtime, scheduler,
};
use chrono::{NaiveDateTime, TimeDelta, Utc};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub jobid: String,
    pub partition: String,
    pub state: String,
    pub outcome: String,
    pub exit: String,
    pub ended: String,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub user: String,
    pub hours: u32,
    pub since: String,
    pub through: String,
    pub outcomes: BTreeMap<String, usize>,
    pub jobs: Vec<Job>,
    pub unknown_end: usize,
}

fn outcome(state: &str, exit: &str) -> Option<String> {
    let state = state
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('+');
    match state {
        "FAILED" | "OUT_OF_MEMORY" | "TIMEOUT" | "NODE_FAIL" | "BOOT_FAIL" | "DEADLINE"
        | "CANCELLED" | "PREEMPTED" => Some(state.into()),
        "COMPLETED"
            if exit.split_once(':').is_some_and(|(code, signal)| {
                code.parse::<u32>().is_ok_and(|v| v != 0)
                    || signal.parse::<u32>().is_ok_and(|v| v != 0)
            }) =>
        {
            Some("NONZERO_EXIT".into())
        }
        _ => None,
    }
}

fn utc_timestamp(value: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|value| value.and_utc().timestamp())
}

fn parse(
    raw: &str,
    user: &str,
    uid: u32,
    hours: u32,
    since: &str,
    through: &str,
) -> Result<Report> {
    let from = utc_timestamp(since).ok_or("invalid failure window start")?;
    let until = utc_timestamp(through).ok_or("invalid failure window end")?;
    let mut report = Report {
        version: 1,
        user: user.into(),
        hours,
        since: format!("{since}Z"),
        through: format!("{through}Z"),
        outcomes: BTreeMap::new(),
        jobs: Vec::new(),
        unknown_end: 0,
    };
    let mut jobs = BTreeMap::<String, (i64, Job)>::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if fields.len() != 6 {
            return Err("unexpected sacct failure-report format".into());
        }
        let owner = fields[1]
            .parse::<u32>()
            .map_err(|_| "accounting UID is unavailable")?;
        if owner != uid || fields[0].contains('.') {
            continue;
        }
        if !scheduler::valid_job_id(fields[0]) {
            return Err("invalid accounting job ID".into());
        }
        let Some(outcome) = outcome(fields[3], fields[4]) else {
            continue;
        };
        let Some(ended) = utc_timestamp(fields[5]) else {
            report.unknown_end += 1;
            continue;
        };
        if ended < from || ended > until {
            continue;
        }
        let job = Job {
            jobid: fields[0].into(),
            partition: fields[2].into(),
            state: fields[3].into(),
            outcome,
            exit: fields[4].into(),
            ended: format!("{}Z", fields[5]),
        };
        if jobs
            .get(fields[0])
            .is_none_or(|(previous, _)| ended >= *previous)
        {
            jobs.insert(job.jobid.clone(), (ended, job));
        }
    }
    let mut jobs: Vec<_> = jobs.into_values().collect();
    jobs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.jobid.cmp(&b.1.jobid)));
    for (_, job) in jobs {
        *report.outcomes.entry(job.outcome.clone()).or_default() += 1;
        report.jobs.push(job);
    }
    Ok(report)
}

pub fn query(hours: u32) -> Result<Report> {
    if !(1..=744).contains(&hours) {
        return Err("hours must be between 1 and 744".into());
    }
    let now = Utc::now();
    let since = (now - TimeDelta::hours(hours as i64))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let through = now.format("%Y-%m-%dT%H:%M:%S").to_string();
    let user = cli::current_user();
    let uid = runtime::euid();
    let raw = process::run_with_env(
        Program::Sacct,
        &[
            "-X",
            "--array",
            "--local",
            "--noheader",
            "--parsable2",
            "--format=JobID%256,UID,Partition%128,State%64,ExitCode,End",
            "-u",
            &uid.to_string(),
            "-S",
            &since,
            "-E",
            &through,
        ],
        Duration::from_secs(30),
        &[("TZ", "UTC")],
    )?;
    parse(&raw, &user, uid, hours, &since, &through)
}

pub fn render(report: &Report) -> String {
    let mut out = format!(
        "{} · last {}h · failures / stopped allocations\n{} → {}\n\n",
        cli::terminal_safe(&report.user),
        report.hours,
        report.since,
        report.through
    );
    if report.jobs.is_empty() {
        out.push_str("No reported failed/stopped allocations ended in this window.\n");
    }
    for (state, count) in &report.outcomes {
        out.push_str(&format!("{state:<18} {count}\n"));
    }
    out.push_str(&format!(
        "\n{} allocations · CANCELLED/PREEMPTED are listed separately from failures.\n",
        report.jobs.len()
    ));
    out.push_str(&format!(
        "{:<16} {:<18} {:<9} {:<20} {}\n",
        "JobID", "Outcome", "Exit", "Ended", "Partition"
    ));
    for job in report.jobs.iter().take(20) {
        out.push_str(&format!(
            "{:<16} {:<18} {:<9} {:<20} {}\n",
            job.jobid,
            job.outcome,
            cli::terminal_safe(&job.exit),
            job.ended,
            cli::terminal_safe(&job.partition)
        ));
    }
    if report.jobs.len() > 20 {
        out.push_str(&format!(
            "… {} more allocations; use chkfail --json for the full list.\n",
            report.jobs.len() - 20
        ));
    }
    if report.unknown_end > 0 {
        out.push_str(&format!(
            "\n{} records excluded: end time unavailable.\n",
            report.unknown_end
        ));
    }
    out.push_str("\nAllocation records only; step-level errors may differ. Accounting availability/retention limits coverage.\nInspect a job with sgpu logs JOBID -e or sgpu jobs.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_window_filters_owner_terminal_states_steps_and_end_times() {
        let raw = "1|1000|gpu|OUT_OF_MEMORY|0:9|2026-11-16T00:00:00\n2|1000|cpu|TIMEOUT|0:15|2026-11-15T00:00:00\n3|1001|gpu|FAILED|1:0|2026-11-16T00:00:00\n1.batch|1000|gpu|FAILED|1:0|2026-11-16T00:00:00\n4|1000|gpu|FAILED|1:0|Unknown\n5|1000|gpu|RUNNING|1:0|2026-11-16T00:00:00\n6_2|1000|gpu|CANCELLED by 123|0:15|2026-11-16T00:00:00\n7|1000|gpu|COMPLETED|1:0|2026-11-16T00:00:00\n8|1000|gpu|FAILED|1:0|2026-11-17T00:00:00";
        let report = parse(
            raw,
            "alice",
            1000,
            24,
            "2026-11-15T12:00:00",
            "2026-11-16T12:00:00",
        )
        .unwrap();
        assert_eq!(report.jobs.len(), 3);
        assert_eq!(report.outcomes["OUT_OF_MEMORY"], 1);
        assert_eq!(report.outcomes["CANCELLED"], 1);
        assert_eq!(report.outcomes["NONZERO_EXIT"], 1);
        let completed = report.jobs.iter().find(|job| job.jobid == "7").unwrap();
        assert_eq!(completed.state, "COMPLETED");
        assert_eq!(completed.outcome, "NONZERO_EXIT");
        assert_eq!(report.unknown_end, 1);
        assert!(!report.jobs.iter().any(|job| job.jobid == "3"));
        assert!(
            parse(
                "malformed",
                "alice",
                1000,
                24,
                &report.since,
                &report.through
            )
            .is_err()
        );
    }
    #[test]
    fn window_edges_duplicates_and_unknown_identity_do_not_inflate_counts() {
        let raw = "1|7|cpu|FAILED|1:0|2026-11-15T12:00:00\n1|7|cpu|TIMEOUT|0:15|2026-11-16T12:00:00\n2|7|cpu|COMPLETED|0:0|2026-11-16T12:00:00";
        let report = parse(
            raw,
            "owner",
            7,
            24,
            "2026-11-15T12:00:00",
            "2026-11-16T12:00:00",
        )
        .unwrap();
        assert_eq!(report.jobs.len(), 1);
        assert_eq!(report.outcomes["TIMEOUT"], 1);
        assert!(
            parse(
                "1|Unknown|cpu|FAILED|1:0|2026-11-16T12:00:00",
                "owner",
                7,
                24,
                &report.since,
                &report.through
            )
            .is_err()
        );
    }
}
