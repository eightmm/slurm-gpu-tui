use crate::{
    Result,
    process::{self, Program},
    rules, runtime, scheduler, usage,
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Record {
    pub jobid: String,
    pub user: String,
    pub name: String,
    pub partition: String,
    pub gpus: u32,
    pub wait: f64,
    pub elapsed: f64,
    pub state: String,
    pub start: f64,
    pub end: String,
    pub exit: String,
    pub workdir: String,
}

pub fn parse(raw: &str, now: f64) -> Vec<Record> {
    let mut rows = Vec::new();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').collect();
        if !matches!(p.len(), 9 | 12) || !scheduler::valid_job_id(p[0]) {
            continue;
        }
        let submit = rules::parse_timestamp(p[5]);
        let start = rules::parse_timestamp(p[6]);
        let end = rules::parse_timestamp(p[7]).unwrap_or(now).min(now);
        let wait = start
            .zip(submit)
            .map(|(a, b)| (a - b).max(0.0))
            .unwrap_or(0.0);
        let elapsed = start.map(|s| (end - s).max(0.0)).unwrap_or(0.0);
        rows.push(Record {
            jobid: p[0].into(),
            user: p[1].into(),
            name: p[2].into(),
            partition: p[3].into(),
            gpus: usage::tres_gpus(p[4]),
            wait,
            elapsed,
            state: p[8].split_whitespace().next().unwrap_or("").into(),
            start: start.or(submit).unwrap_or(0.0),
            end: p[7].into(),
            exit: p.get(9).unwrap_or(&"").to_string(),
            workdir: p.get(11).unwrap_or(&"").to_string(),
        });
    }
    rows.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.jobid.cmp(&b.jobid)));
    rows
}

pub fn query(start: &str, end: &str, user: &str) -> Result<Vec<Record>> {
    let mut args = vec![
        "-X",
        "--noheader",
        "--parsable2",
        "--format=JobID,User,JobName,Partition,AllocTRES,Submit,Start,End,State,ExitCode,Elapsed,WorkDir",
        "-S",
        start,
    ];
    if user.is_empty() {
        args.push("-a");
    } else {
        args.extend(["-u", user]);
    }
    if !end.is_empty() {
        args.extend(["-E", end]);
    }
    Ok(parse(
        &process::run(Program::Sacct, &args, Duration::from_secs(30))?,
        runtime::now(),
    ))
}

pub fn details(jobid: &str) -> Result<String> {
    if !scheduler::valid_job_id(jobid) {
        return Err("invalid job ID".into());
    }
    let fields =
        "JobID,JobName,User,State,ExitCode,Elapsed,Start,End,NodeList,AllocTRES,MaxRSS,WorkDir";
    let raw = process::run(
        Program::Sacct,
        &["-j", jobid, "-n", "-P", "-o", fields],
        Duration::from_secs(30),
    )?;
    let headers: Vec<_> = fields.split(',').collect();
    let mut out = String::new();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').collect();
        if p.first()
            .is_none_or(|id| *id != jobid && !id.starts_with(&format!("{jobid}.")))
        {
            continue;
        }
        for (key, value) in headers.iter().zip(p) {
            if !value.is_empty() {
                out.push_str(&format!("{key}={}\n", crate::cli::terminal_safe(value)));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

type StateTotals = BTreeMap<String, (usize, f64)>;
type PartitionWaits = BTreeMap<String, Vec<f64>>;
pub fn summaries(rows: &[Record]) -> (StateTotals, PartitionWaits) {
    let mut states = BTreeMap::<String, (usize, f64)>::new();
    let mut waits = BTreeMap::<String, Vec<f64>>::new();
    for row in rows {
        let entry = states.entry(row.state.clone()).or_default();
        entry.0 += 1;
        entry.1 += row.gpus as f64 * row.elapsed;
        if row.start > 0.0 && row.state != "PENDING" {
            waits
                .entry(row.partition.clone())
                .or_default()
                .push(row.wait);
        }
    }
    for waits in waits.values_mut() {
        waits.sort_by(f64::total_cmp);
    }
    (states, waits)
}

pub fn render(rows: &[Record], summary: bool) -> String {
    let mut out = format!(
        "{:<14}{:<14}{:<17}{:>5}{:>9}{:>10}  {}\n",
        "jobid", "user", "state", "GPUs", "wait", "elapsed", "partition / name / exit"
    );
    for row in rows {
        out.push_str(&format!(
            "{:<14}{:<14}{:<17}{:>5}{:>9}{:>10}  {} / {} / {}\n",
            row.jobid,
            crate::cli::terminal_safe(&row.user),
            row.state,
            row.gpus,
            rules::span(row.wait as u64),
            rules::span(row.elapsed as u64),
            crate::cli::terminal_safe(&row.partition),
            crate::cli::terminal_safe(&row.name),
            row.exit
        ));
    }
    if summary {
        let (states, waits) = summaries(rows);
        out.push_str("\noutcome              jobs  GPU-hours\n");
        for (state, (count, seconds)) in states {
            out.push_str(&format!(
                "{state:<20}{count:>4}  {:>9.1}\n",
                seconds / 3600.0
            ));
        }
        out.push_str("\npartition            jobs  median wait  max wait\n");
        for (partition, waits) in waits {
            out.push_str(&format!(
                "{partition:<20}{:>4}  {:>11}  {}\n",
                waits.len(),
                rules::span(waits[waits.len() / 2] as u64),
                rules::span(*waits.last().unwrap() as u64)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_cpu_jobs_and_upper_median_are_retained() {
        let rows = parse(
            "42|alice|a job|gpu|gres/gpu=2|2026-07-01T00:00:00|2026-07-01T00:00:10|2026-07-01T00:01:10|COMPLETED|0:0|00:01:00|/work\n43|alice|cpu|cpu|cpu=4|2026-07-01T00:00:00|2026-07-01T00:00:20|2026-07-01T00:01:20|FAILED|1:0|00:01:00|/work",
            2000000000.0,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a job");
        let (states, waits) = summaries(&rows);
        assert_eq!(states["COMPLETED"], (1, 120.0));
        assert_eq!(waits["cpu"][0], 20.0);
    }
}
