use crate::{
    Result, cli,
    process::{self, Program},
    runtime, scheduler,
    terminal::{Tone, cell, paint},
};
use chrono::{NaiveDateTime, TimeDelta, Utc};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

// Compressed array ranges must not bypass the command output's memory bound.
const MAX_ALLOCATIONS: usize = 100_000;

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

fn allocation_ids(id: &str) -> Result<Vec<String>> {
    if !scheduler::valid_job_id(id) {
        return Err("invalid or truncated accounting job ID".into());
    }
    let Some((base, tasks)) = id.split_once("_[") else {
        return Ok(vec![id.into()]);
    };
    let tasks = tasks.strip_suffix(']').ok_or("invalid accounting array")?;
    let tasks = if let Some((tasks, throttle)) = tasks.split_once('%') {
        if !throttle.parse::<u32>().is_ok_and(|value| value > 0) {
            return Err("invalid accounting array throttle".into());
        }
        tasks
    } else {
        tasks
    };
    let mut ids = Vec::new();
    for range in tasks.split(',') {
        let (range, step) = range.split_once(':').unwrap_or((range, "1"));
        let (start, end) = range.split_once('-').unwrap_or((range, range));
        let start: u32 = start
            .parse()
            .map_err(|_| "invalid accounting array start")?;
        let end: u32 = end.parse().map_err(|_| "invalid accounting array end")?;
        let step: u32 = step.parse().map_err(|_| "invalid accounting array step")?;
        if step == 0 || end < start {
            return Err("invalid accounting array range".into());
        }
        let count = u64::from((end - start) / step) + 1;
        if count > MAX_ALLOCATIONS.saturating_sub(ids.len()) as u64 {
            return Err("accounting array exceeds 100000 tasks; report was not truncated".into());
        }
        ids.extend(
            (0..count).map(|i| format!("{base}_{}", u64::from(start) + i * u64::from(step))),
        );
    }
    Ok(ids)
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
    let mut expanded = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if fields.len() != 6 {
            return Err("unexpected sacct failure-report format".into());
        }
        let owner = fields[1]
            .parse::<u32>()
            .map_err(|_| "accounting UID is unavailable")?;
        if owner != uid {
            continue;
        }
        if fields[0]
            .split_once('.')
            .is_some_and(|(allocation, _)| scheduler::valid_job_id(allocation))
        {
            continue;
        }
        let Some(outcome) = outcome(fields[3], fields[4]) else {
            continue;
        };
        let ids = allocation_ids(fields[0])?;
        expanded += ids.len();
        if expanded > MAX_ALLOCATIONS {
            return Err("accounting report exceeds 100000 allocations; narrow --hours".into());
        }
        let Some(ended) = utc_timestamp(fields[5]) else {
            report.unknown_end += ids.len();
            continue;
        };
        if ended < from || ended > until {
            continue;
        }
        for jobid in ids {
            if jobs
                .get(&jobid)
                .is_some_and(|(previous, _)| ended < *previous)
            {
                continue;
            }
            let job = Job {
                jobid,
                partition: fields[2].into(),
                state: fields[3].into(),
                outcome: outcome.clone(),
                exit: fields[4].into(),
                ended: format!("{}Z", fields[5]),
            };
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
        // --array requires Slurm 23.02. Expand grouped task IDs locally instead.
        &[("TZ", "UTC"), ("SLURM_BITSTR_LEN", "0")],
    )?;
    parse(&raw, &user, uid, hours, &since, &through)
}

pub fn render(report: &Report) -> String {
    render_with_color(report, false)
}
pub(crate) fn render_with_color(report: &Report, color: bool) -> String {
    let mark = |text: &str, tone| paint(text, tone, color);
    let column = |text: &str, width, tone| mark(&cell(text, width, false), tone);
    let id_width = report
        .jobs
        .iter()
        .take(20)
        .map(|job| job.jobid.len())
        .max()
        .unwrap_or(16)
        .max(16);
    let outcome_tone = |state: &str| match state {
        "CANCELLED" | "PREEMPTED" => Tone::Busy,
        _ => Tone::Alert,
    };
    let mut out = format!(
        "{}\n{} · last {}h\n{}\n\n",
        mark("sgpu · failures / stopped allocations", Tone::Heading),
        mark(&report.user, Tone::Mine),
        report.hours,
        mark(
            &format!("{} → {} (UTC)", report.since, report.through),
            Tone::Muted
        )
    );
    if report.jobs.is_empty() {
        out.push_str(&mark(
            "No reported failed/stopped allocations ended in this window.\n",
            Tone::Good,
        ));
    }
    for (state, count) in &report.outcomes {
        out.push_str(&format!(
            "{} {}\n",
            column(state, 18, outcome_tone(state)),
            mark(&count.to_string(), Tone::Bold)
        ));
    }
    out.push_str(&mark(
        &format!(
            "\n{} allocations · CANCELLED/PREEMPTED are listed separately from failures.\n",
            report.jobs.len()
        ),
        Tone::Muted,
    ));
    out.push_str(&mark(
        &format!(
            "{:<id_width$} {:<18} {:<9} {:<20} {}\n",
            "JobID", "Outcome", "Exit", "Ended", "Partition"
        ),
        Tone::Heading,
    ));
    for job in report.jobs.iter().take(20) {
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            column(&job.jobid, id_width, Tone::Value),
            column(&job.outcome, 18, outcome_tone(&job.outcome)),
            column(&job.exit, 9, outcome_tone(&job.outcome)),
            column(&job.ended, 20, Tone::Muted),
            mark(&job.partition, Tone::Normal)
        ));
    }
    if report.jobs.len() > 20 {
        out.push_str(&mark(
            &format!(
                "… {} more allocations; use chkfail --json for the full list.\n",
                report.jobs.len() - 20
            ),
            Tone::Muted,
        ));
    }
    if report.unknown_end > 0 {
        out.push_str(&mark(
            &format!(
                "\n{} allocations excluded: end time unavailable.\n",
                report.unknown_end
            ),
            Tone::Busy,
        ));
    }
    out.push_str(&mark("\nAllocation records only; step-level errors may differ. Accounting availability/retention limits coverage.\nInspect a job with sgpu logs JOBID -e or sgpu jobs.\n", Tone::Muted));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_grouped_arrays_expand_ranges_steps_and_throttles_without_double_counting() {
        let raw = "42_[1-5:2,8%2]|7|gpu|CANCELLED by 7|0:15|2026-11-16T00:00:00\n42_3|7|gpu|TIMEOUT|0:15|2026-11-16T01:00:00\n43_[0-2]|7|gpu|FAILED|1:0|Unknown\n42_[1-5:2,8%2].batch|7|gpu|FAILED|1:0|2026-11-16T00:00:00";
        let report = parse(
            raw,
            "owner",
            7,
            24,
            "2026-11-15T12:00:00",
            "2026-11-16T12:00:00",
        )
        .unwrap();
        assert_eq!(report.jobs.len(), 4);
        assert_eq!(report.outcomes["CANCELLED"], 3);
        assert_eq!(report.outcomes["TIMEOUT"], 1);
        assert_eq!(report.jobs[0].jobid, "42_3");
        assert_eq!(report.unknown_end, 3);
        assert_eq!(
            allocation_ids("42_[0,2-3]").unwrap(),
            ["42_0", "42_2", "42_3"]
        );
        assert_eq!(
            allocation_ids("42_[4294967295]").unwrap(),
            ["42_4294967295"]
        );
        for id in [
            "42_[1-3:0]",
            "42_[3-1]",
            "42_[1%0]",
            "42_[1%2%3]",
            "42_[1,,2]",
            "42_[1-4294967295]",
            "42_[4294967296]",
            "42_[1-3...]",
        ] {
            assert!(allocation_ids(id).is_err(), "{id}");
        }
        assert!(
            parse(
                "42_[1-3...]|7|gpu|FAILED|1:0|2026-11-16T00:00:00",
                "owner",
                7,
                24,
                &report.since,
                &report.through
            )
            .is_err()
        );
        assert!(
            parse(
                "42_[0-49999]|7|gpu|FAILED|1:0|Unknown\n43_[0-50000]|7|gpu|FAILED|1:0|Unknown",
                "owner",
                7,
                24,
                &report.since,
                &report.through
            )
            .is_err()
        );
    }

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
