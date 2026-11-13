use crate::{
    Result,
    model::{Gpu, Job, Node, PendingJob, SchedulerStatus},
    process::{self, Program},
    runtime,
};
use chrono::{Local, TimeZone};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

fn job_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+(?:_[0-9]+|_\[[0-9,:%-]+\]|\+[0-9]+)?$").unwrap())
}

pub fn valid_job_id(id: &str) -> bool {
    id.len() <= 8192 && job_regex().is_match(id)
}

fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 256
        && user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'@' | b'-'))
}

pub fn range_values(spec: &str, limit: usize) -> Option<Vec<String>> {
    let mut values = Vec::new();
    for part in spec.split(',') {
        let (start, end) = part.split_once('-').unwrap_or((part, part));
        if start.is_empty()
            || !start.bytes().all(|c| c.is_ascii_digit())
            || !end.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let lo: u64 = start.parse().ok()?;
        let hi: u64 = end.parse().ok()?;
        let count = hi.checked_sub(lo)?.checked_add(1)?;
        if count > limit.saturating_sub(values.len()) as u64 || start.len() > 255 {
            return None;
        }
        if !part.contains('-') {
            values.push(start.to_string());
        } else {
            for value in lo..=hi {
                values.push(format!("{value:0width$}", width = start.len()));
            }
        }
    }
    Some(values)
}

fn top_level(expr: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut level = 0;
    let mut start = 0;
    for (i, ch) in expr.char_indices() {
        match ch {
            '[' => {
                if level != 0 {
                    return None;
                }
                level = 1;
            }
            ']' => {
                if level != 1 {
                    return None;
                }
                level = 0;
            }
            ',' if level == 0 => {
                parts.push(&expr[start..i]);
                start = i + 1;
            }
            _ => (),
        }
    }
    if level != 0 {
        return None;
    }
    parts.push(&expr[start..]);
    Some(parts)
}

pub fn expand_nodes(expr: &str) -> Option<Vec<String>> {
    if matches!(expr, "" | "N/A" | "(null)") {
        return Some(Vec::new());
    }
    if expr.len() > 8192 || expr.chars().any(char::is_control) {
        return None;
    }
    let mut all = Vec::new();
    for token in top_level(expr)? {
        if token.is_empty() {
            return None;
        }
        let mut hosts = vec![String::new()];
        let mut remaining = token;
        while !remaining.is_empty() {
            let (group, rest) = if let Some(after) = remaining.strip_prefix('[') {
                let (spec, rest) = after.split_once(']')?;
                (range_values(spec, 4096)?, rest)
            } else {
                let end = remaining.find('[').unwrap_or(remaining.len());
                let literal = &remaining[..end];
                if !runtime::valid_host(literal) {
                    return None;
                }
                (vec![literal.to_string()], &remaining[end..])
            };
            if hosts.len().checked_mul(group.len())? > 4096usize.saturating_sub(all.len()) {
                return None;
            }
            hosts = hosts
                .iter()
                .flat_map(|prefix| group.iter().map(move |suffix| format!("{prefix}{suffix}")))
                .collect();
            remaining = rest;
        }
        all.extend(hosts);
    }
    all.sort();
    if all.len() > 4096
        || all.windows(2).any(|w| w[0] == w[1])
        || all.iter().any(|s| !runtime::valid_host(s))
    {
        return None;
    }
    Some(all)
}

pub fn gpu_count(gres: &str) -> u32 {
    top_level(gres)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|part| {
            let part = part.trim().strip_prefix("gres/").unwrap_or(part.trim());
            if !part.starts_with("gpu:") {
                return None;
            }
            part.split('(')
                .next()?
                .rsplit(':')
                .next()?
                .parse::<u32>()
                .ok()
        })
        .fold(0u32, u32::saturating_add)
}

pub fn mem_mib(raw: &str, cpus: u32) -> Option<f64> {
    let mut text = raw.trim();
    let per_cpu = text.ends_with('c');
    if text.ends_with(['c', 'n']) {
        text = &text[..text.len() - 1];
    }
    let (digits, scale) = match text.chars().last()? {
        'K' | 'k' => (&text[..text.len() - 1], 1.0 / 1024.0),
        'M' | 'm' => (&text[..text.len() - 1], 1.0),
        'G' | 'g' => (&text[..text.len() - 1], 1024.0),
        'T' | 't' => (&text[..text.len() - 1], 1048576.0),
        _ => (text, 1.0),
    };
    crate::model::number(digits)
        .map(|n| n * scale * if per_cpu { cpus as f64 } else { 1.0 })
        .filter(|n| n.is_finite() && *n >= 0.0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub user: String,
    pub uid: u32,
    pub state: String,
    pub nodes: Vec<String>,
}

#[derive(Default)]
pub struct Queue {
    pub jobs: Vec<Job>,
    pub pending: Vec<PendingJob>,
    pub anchors: BTreeMap<String, Anchor>,
}

pub fn parse_queue(raw: &str) -> Queue {
    let mut out = Queue::default();
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for line in raw.lines() {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if !matches!(fields.len(), 14 | 15) {
            continue;
        }
        let state = fields[0].to_uppercase();
        let (id, user) = (fields[1], fields[2]);
        if !matches!(state.as_str(), "RUNNING" | "PENDING" | "COMPLETING")
            || !valid_job_id(id)
            || !valid_user(user)
            || fields[3].is_empty()
            || !fields[3].bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(uid) = fields[3].parse::<u32>() else {
            continue;
        };
        let Some(nodes) = (if state == "PENDING" {
            Some(Vec::new())
        } else {
            expand_nodes(fields[6])
        }) else {
            continue;
        };
        if state != "PENDING" && nodes.is_empty() {
            continue;
        }
        if !seen.insert(id.to_string()) {
            duplicates.insert(id.to_string());
        }
        out.anchors.insert(
            id.into(),
            Anchor {
                user: user.into(),
                uid,
                state: state.clone(),
                nodes,
            },
        );
        if state == "RUNNING" {
            out.jobs.push(Job {
                jobid: id.into(),
                user: user.into(),
                uid: i64::from(uid),
                partition: fields[4].into(),
                elapsed: fields[5].into(),
                node: fields[6].into(),
                gpu_count: gpu_count(fields[7]),
                cpu_count: fields[9].parse().unwrap_or(0),
                gres_raw: fields[7].into(),
                time_limit: fields[8].into(),
                mem: fields[10].into(),
                ..Default::default()
            });
        } else if state == "PENDING" {
            out.pending.push(PendingJob {
                jobid: id.into(),
                user: user.into(),
                partition: fields[4].into(),
                time_limit: fields[8].into(),
                gpu_count: gpu_count(fields[7]),
                reason: fields[11].into(),
                priority: fields[12].into(),
                start_time: fields[13].into(),
                submit_time: fields.get(14).copied().unwrap_or("").into(),
                cpu_count: fields[9].parse().unwrap_or(0),
                mem: fields[10].into(),
                ..Default::default()
            });
        }
    }
    out.anchors.retain(|id, _| !duplicates.contains(id));
    out.jobs.retain(|job| !duplicates.contains(&job.jobid));
    out.pending.retain(|job| !duplicates.contains(&job.jobid));
    out
}

pub fn parse_nodes(raw: &str) -> Vec<Node> {
    let mut nodes = BTreeMap::<String, Node>::new();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if p.len() != 9 || !runtime::valid_host(p[0]) {
            continue;
        }
        let partition = p[8].trim_end_matches('*');
        if let Some(node) = nodes.get_mut(p[0]) {
            if !partition.is_empty() && !node.partition.split(',').any(|s| s == partition) {
                if !node.partition.is_empty() {
                    node.partition.push(',');
                }
                node.partition.push_str(partition);
            }
        } else {
            nodes.insert(
                p[0].into(),
                Node {
                    name: p[0].into(),
                    state: p[1].into(),
                    cpus: p[2].into(),
                    cpu_load: p[3].into(),
                    mem_total: p[4].into(),
                    scheduler_mem_total: p[4].into(),
                    mem_free: p[5].into(),
                    gres: p[6].into(),
                    has_gpu: p[6].to_lowercase().contains("gpu"),
                    cpu_alloc: if p[7].split('/').count() >= 4 {
                        p[7].split('/').next().unwrap_or("").into()
                    } else {
                        String::new()
                    },
                    partition: partition.into(),
                    ..Default::default()
                },
            );
        }
    }
    nodes.into_values().collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogSpec {
    pub out: PathBuf,
    pub err: PathBuf,
    pub merged: bool,
}

#[derive(Clone, Default)]
pub struct JobRecords {
    pub alloc: BTreeMap<String, BTreeMap<String, String>>,
    pub users: BTreeMap<String, String>,
    pub uids: BTreeMap<String, u32>,
    pub details: BTreeMap<String, String>,
    pub logs: BTreeMap<String, LogSpec>,
    pub rejected: BTreeSet<String>,
}

const PUBLIC_FIELDS: &str = "JobId ArrayJobId ArrayTaskId JobName UserId Priority Nice Account QOS JobState Reason Dependency Requeue Restarts BatchFlag ExitCode RunTime TimeLimit TimeMin SubmitTime EligibleTime AccrueTime StartTime EndTime Deadline SuspendTime SecsPreSuspend LastSchedEval Scheduler Partition ReqNodeList ExcNodeList NodeList BatchHost NumNodes NumCPUs NumTasks CPUs/Task ReqB:S:C:T ReqTRES AllocTRES Socks/Node NtasksPerN:B:S:C CoreSpec MinCPUsNode MinMemoryNode MinTmpDiskNode Features DelayBoot OverSubscribe Contiguous Licenses Network Power TresPerNode TresPerTask TresPerSocket TresPerJob";

pub fn display_scalar(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        Value::Array(values) => values
            .iter()
            .map(display_scalar)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(4096)
        .collect()
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.get("number")?.as_i64())
}

fn wrapped(value: &Value) -> String {
    if !value.is_object() {
        return display_scalar(value);
    }
    if value["infinite"] == true {
        "UNLIMITED".into()
    } else if value["set"] == false {
        String::new()
    } else {
        display_scalar(&value["number"])
    }
}

fn timestamp(value: &Value) -> String {
    if !value.is_object() || value["set"] == false {
        return String::new();
    }
    integer(value)
        .filter(|n| *n > 0)
        .and_then(|n| Local.timestamp_opt(n, 0).single())
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn minute_duration(value: &Value) -> String {
    if value["infinite"] == true {
        return "UNLIMITED".into();
    }
    let Some(minutes) = integer(value).filter(|n| *n >= 0) else {
        return String::new();
    };
    let day = minutes / 1440;
    format!(
        "{}{:02}:{:02}:00",
        if day > 0 {
            format!("{day}-")
        } else {
            String::new()
        },
        minutes % 1440 / 60,
        minutes % 60
    )
}

fn json_ids(record: &Value) -> Vec<String> {
    let Some(raw) = integer(&record["job_id"]).filter(|n| *n > 0) else {
        return Vec::new();
    };
    let mut ids = vec![raw.to_string()];
    if let Some(parent) = integer(&record["array_job_id"]).filter(|n| *n > 0) {
        if record["array_task_id"]["set"] == true {
            if let Some(task) = integer(&record["array_task_id"]).filter(|n| *n >= 0) {
                ids.push(format!("{parent}_{task}"));
            }
        } else if let Some(tasks) = record["array_task_string"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            let alias = format!("{parent}_[{tasks}]");
            if valid_job_id(&alias) {
                ids.push(alias);
            }
        }
    }
    if let (Some(parent), Some(offset)) = (
        integer(&record["het_job_id"]).filter(|n| *n > 0),
        integer(&record["het_job_offset"]).filter(|n| *n >= 0),
    ) {
        ids.push(format!("{parent}+{offset}"));
    }
    ids
}

fn public_detail(record: &Value, ids: &[String]) -> String {
    let mut fields = vec![("JobId", ids[0].clone())];
    for (key, name) in [
        ("name", "JobName"),
        ("nice", "Nice"),
        ("account", "Account"),
        ("qos", "QOS"),
        ("job_state", "JobState"),
        ("state_reason", "Reason"),
        ("dependency", "Dependency"),
        ("requeue", "Requeue"),
        ("restart_cnt", "Restarts"),
        ("batch_flag", "BatchFlag"),
        ("partition", "Partition"),
        ("required_nodes", "ReqNodeList"),
        ("excluded_nodes", "ExcNodeList"),
        ("nodes", "NodeList"),
        ("batch_host", "BatchHost"),
        ("tres_req_str", "ReqTRES"),
        ("tres_alloc_str", "AllocTRES"),
        ("features", "Features"),
        ("oversubscribe", "OverSubscribe"),
        ("contiguous", "Contiguous"),
        ("licenses", "Licenses"),
        ("network", "Network"),
        ("tres_per_node", "TresPerNode"),
        ("tres_per_task", "TresPerTask"),
        ("tres_per_socket", "TresPerSocket"),
        ("tres_per_job", "TresPerJob"),
    ] {
        fields.push((name, display_scalar(&record[key])));
    }
    for (key, name) in [
        ("priority", "Priority"),
        ("node_count", "NumNodes"),
        ("cpus", "NumCPUs"),
        ("tasks", "NumTasks"),
        ("cpus_per_task", "CPUs/Task"),
        ("minimum_cpus_per_node", "MinCPUsNode"),
        ("memory_per_node", "MinMemoryNode"),
        ("minimum_tmp_disk_per_node", "MinTmpDiskNode"),
    ] {
        fields.push((name, wrapped(&record[key])));
    }
    for (key, name) in [
        ("submit_time", "SubmitTime"),
        ("eligible_time", "EligibleTime"),
        ("start_time", "StartTime"),
        ("end_time", "EndTime"),
        ("deadline", "Deadline"),
        ("suspend_time", "SuspendTime"),
        ("last_sched_evaluation", "LastSchedEval"),
    ] {
        fields.push((name, timestamp(&record[key])));
    }
    fields.push(("TimeLimit", minute_duration(&record["time_limit"])));
    let user = display_scalar(&record["user_name"]);
    if !user.is_empty() {
        fields.push((
            "UserId",
            integer(&record["user_id"])
                .map(|id| format!("{user}({id})"))
                .unwrap_or(user),
        ));
    }
    if let Some(parent) = integer(&record["array_job_id"]).filter(|n| *n > 0) {
        fields.push(("ArrayJobId", parent.to_string()));
        fields.push((
            "ArrayTaskId",
            if record["array_task_id"]["set"] != false {
                wrapped(&record["array_task_id"])
            } else {
                display_scalar(&record["array_task_string"])
            },
        ));
    }
    fields
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(16384)
        .collect()
}

pub fn detail_field(detail: &str, key: &str) -> String {
    detail
        .lines()
        .find_map(|line| {
            line.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
        .unwrap_or_default()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn log_path(value: &str, workdir: &str, legacy: bool) -> Option<PathBuf> {
    if value.is_empty() || value == "(null)" {
        return Some(PathBuf::new());
    }
    if value.len() > 4096
        || value
            .chars()
            .any(|c| c.is_control() || legacy && c.is_whitespace())
    {
        return None;
    }
    let path = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        if !Path::new(workdir).is_absolute() {
            return None;
        }
        Path::new(workdir).join(value)
    };
    Some(normalize_path(&path))
}

pub fn gpu_indices(gres: &str) -> Option<Vec<String>> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?:^|,)gpu(?::[^,()]*)?\(IDX:([^)]+)\)").unwrap());
    let mut all = Vec::new();
    let captures: Vec<_> = re.captures_iter(gres).collect();
    if gres.matches("IDX:").count() != captures.len() {
        return None;
    }
    for capture in captures {
        if capture[1].eq_ignore_ascii_case("N/A") {
            continue;
        }
        for value in range_values(&capture[1], 1024usize.saturating_sub(all.len()))? {
            let value: u16 = value.parse().ok()?;
            all.push(value.to_string());
        }
    }
    Some(all)
}

pub fn parse_structured(raw: &str) -> Result<JobRecords> {
    if raw.len() > 32 * 1024 * 1024 {
        return Err("scheduler output too large".into());
    }
    let value: Value = serde_json::from_str(raw)?;
    let records = value["jobs"]
        .as_array()
        .filter(|rows| rows.len() <= 262144)
        .ok_or("scontrol JSON has no bounded jobs list")?;
    let mut out = JobRecords::default();
    let mut seen = BTreeSet::new();
    let mut claims = BTreeMap::<(String, String), Vec<String>>::new();
    for record in records {
        let ids = json_ids(record);
        if ids.is_empty() {
            continue;
        }
        let user = display_scalar(&record["user_name"]);
        let uid = integer(&record["user_id"]).and_then(|n| u32::try_from(n).ok());
        let detail = public_detail(record, &ids);
        let workdir = record["current_working_directory"].as_str().unwrap_or("");
        let stdout = log_path(
            record["standard_output"].as_str().unwrap_or(""),
            workdir,
            false,
        )
        .unwrap_or_default();
        let stderr = log_path(
            record["standard_error"].as_str().unwrap_or(""),
            workdir,
            false,
        )
        .unwrap_or_default();
        let merged = !stdout.as_os_str().is_empty() && stdout == stderr;
        for id in &ids {
            if !seen.insert(id.clone()) {
                out.rejected.insert(id.clone());
            }
            if !user.is_empty() {
                out.users.insert(id.clone(), user.clone());
            }
            if let Some(uid) = uid {
                out.uids.insert(id.clone(), uid);
            }
            out.details.insert(id.clone(), detail.clone());
            out.logs.insert(
                id.clone(),
                LogSpec {
                    out: stdout.clone(),
                    err: if merged {
                        PathBuf::new()
                    } else {
                        stderr.clone()
                    },
                    merged,
                },
            );
        }
        let state = display_scalar(&record["job_state"]);
        if !state
            .split(',')
            .any(|s| matches!(s, "RUNNING" | "COMPLETING"))
        {
            continue;
        }
        let resources = &record["job_resources"];
        let mut allocation: Vec<&Value>;
        let key;
        if let Some(old) = resources["allocated_nodes"].as_array() {
            allocation = old.iter().collect();
            key = "nodename";
        } else if let Some(new) = resources["nodes"]["allocation"].as_array() {
            allocation = new.iter().collect();
            allocation.sort_by_key(|n| n["index"].as_i64().unwrap_or(0));
            key = "name";
        } else {
            continue;
        }
        let nodes: Option<Vec<_>> = allocation
            .iter()
            .map(|node| node[key].as_str().filter(|s| runtime::valid_host(s)))
            .collect();
        let Some(nodes) = nodes.filter(|n| n.len() <= 4096) else {
            continue;
        };
        let Some(gres) = record["gres_detail"]
            .as_array()
            .filter(|rows| rows.len() == nodes.len())
        else {
            continue;
        };
        for (node, gres) in nodes.iter().zip(gres) {
            let Some(indices) = gres.as_str().and_then(gpu_indices) else {
                continue;
            };
            for index in indices {
                claims
                    .entry((node.to_string(), index))
                    .or_default()
                    .push(ids[0].clone());
            }
        }
    }
    for ((node, index), ids) in claims {
        if ids.len() == 1 && !out.rejected.contains(&ids[0]) {
            out.alloc
                .entry(node)
                .or_default()
                .insert(index, ids[0].clone());
        }
    }
    for id in &out.rejected {
        out.users.remove(id);
        out.uids.remove(id);
        out.details.remove(id);
        out.logs.remove(id);
    }
    Ok(out)
}

pub fn field_items(line: &str) -> Vec<(String, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?:^|[ \t])([A-Za-z][A-Za-z0-9_/:]*)=").unwrap());
    let captures: Vec<_> = re.captures_iter(line).collect();
    captures
        .iter()
        .enumerate()
        .map(|(i, c)| {
            (
                c[1].to_string(),
                line[c.get(0).unwrap().end()
                    ..captures
                        .get(i + 1)
                        .map(|c| c.get(0).unwrap().start())
                        .unwrap_or(line.len())]
                    .trim_end()
                    .to_string(),
            )
        })
        .collect()
}

fn legacy_ids(line: &str) -> Vec<String> {
    let fields: BTreeMap<_, _> = field_items(line).into_iter().collect();
    let Some(raw) = fields
        .get("JobId")
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    else {
        return Vec::new();
    };
    let mut ids = vec![raw.clone()];
    if let (Some(parent), Some(task)) = (fields.get("ArrayJobId"), fields.get("ArrayTaskId")) {
        let alias = format!("{parent}_{task}");
        if valid_job_id(&alias) && !ids.contains(&alias) {
            ids.push(alias);
        }
    }
    if let (Some(parent), Some(offset)) = (fields.get("HetJobId"), fields.get("HetJobOffset")) {
        let alias = format!("{parent}+{offset}");
        if valid_job_id(&alias) && !ids.contains(&alias) {
            ids.push(alias);
        }
    }
    ids
}

pub fn parse_legacy(
    raw: &str,
    before: &BTreeMap<String, Anchor>,
    after: &BTreeMap<String, Anchor>,
) -> Result<JobRecords> {
    if raw.len() > 32 * 1024 * 1024 {
        return Err("legacy scheduler output too large".into());
    }
    let mut out = JobRecords::default();
    let candidates: Vec<_> = raw
        .lines()
        .filter(|l| l.starts_with("JobId="))
        .map(|l| (l, legacy_ids(l)))
        .filter(|(_, ids)| !ids.is_empty())
        .collect();
    let mut counts = BTreeMap::<String, usize>::new();
    for (_, ids) in &candidates {
        for id in ids {
            *counts.entry(id.clone()).or_default() += 1;
        }
    }
    let mut claims = BTreeMap::<(String, String), Vec<(String, Vec<String>)>>::new();
    static SEGMENTS: OnceLock<Regex> = OnceLock::new();
    let segments = SEGMENTS.get_or_init(|| {
        Regex::new(r"(?:^|\s)Nodes=(\S+)\s+CPU_IDs=\S+\s+Mem=\S+\s+GRES=(\S+)").unwrap()
    });
    static OWNER: OnceLock<Regex> = OnceLock::new();
    let owner =
        OWNER.get_or_init(|| Regex::new(r"^([A-Za-z0-9_.@-]{1,256})\(([0-9]{1,10})\)$").unwrap());
    for (line, ids) in candidates {
        if !ids
            .iter()
            .any(|id| before.contains_key(id) || after.contains_key(id))
        {
            continue;
        }
        let fields_vec = field_items(line);
        let fields: BTreeMap<_, _> = fields_vec.iter().cloned().collect();
        let field = |key| fields.get(key).map(String::as_str).unwrap_or("");
        let validation = (|| -> Option<Anchor> {
            if line.len() > 128 * 1024
                || line.chars().any(|c| c.is_control() && c != '\t')
                || ids.iter().any(|id| counts[id] != 1)
            {
                return None;
            }
            let mut found = BTreeSet::new();
            for (key, _) in &fields_vec {
                if !found.insert(key)
                    && !matches!(key.as_str(), "Nodes" | "CPU_IDs" | "Mem" | "GRES")
                {
                    return None;
                }
            }
            let anchors: Vec<_> = ids
                .iter()
                .filter(|id| before.contains_key(*id) || after.contains_key(*id))
                .map(|id| {
                    let a = before.get(id)?;
                    (after.get(id) == Some(a)).then_some(a)
                })
                .collect::<Option<_>>()?;
            let anchor = *anchors.first()?;
            if anchors.iter().any(|a| *a != anchor)
                || field("JobId") != ids[0]
                || field("JobState") != anchor.state
            {
                return None;
            }
            let uid = owner.captures(field("UserId"))?;
            if uid[1] != anchor.user
                || uid[2].parse::<u32>().ok()? != anchor.uid
                || expand_nodes(field("NodeList"))? != anchor.nodes
            {
                return None;
            }
            Some(anchor.clone())
        })();
        let Some(anchor) = validation else {
            out.rejected.extend(ids);
            continue;
        };
        let work = log_path(field("WorkDir"), "", true);
        let Some(work) = work else {
            out.rejected.extend(ids);
            continue;
        };
        let out_path = log_path(field("StdOut"), &work.to_string_lossy(), true);
        let err_path = log_path(field("StdErr"), &work.to_string_lossy(), true);
        let (Some(stdout), Some(stderr)) = (out_path, err_path) else {
            out.rejected.extend(ids);
            continue;
        };
        let merged = !stdout.as_os_str().is_empty() && stdout == stderr;
        let detail = fields_vec
            .iter()
            .filter(|(k, v)| {
                PUBLIC_FIELDS.split_whitespace().any(|s| s == k) && !v.is_empty() && v != "(null)"
            })
            .map(|(k, v)| format!("{k}={}", display_scalar(&Value::String(v.clone()))))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(16384)
            .collect::<String>();
        for id in &ids {
            out.users.insert(id.clone(), anchor.user.clone());
            out.uids.insert(id.clone(), anchor.uid);
            out.details.insert(id.clone(), detail.clone());
            out.logs.insert(
                id.clone(),
                LogSpec {
                    out: stdout.clone(),
                    err: if merged {
                        PathBuf::new()
                    } else {
                        stderr.clone()
                    },
                    merged,
                },
            );
        }
        if !matches!(anchor.state.as_str(), "RUNNING" | "COMPLETING") {
            continue;
        }
        let valid_claims = (|| -> Option<Vec<(String, String)>> {
            let mut covered = BTreeSet::new();
            let mut node_claims = Vec::new();
            let matches: Vec<_> = segments.captures_iter(line).collect();
            if matches.is_empty() {
                return Some(node_claims);
            }
            for segment in matches {
                let nodes = expand_nodes(&segment[1])?;
                let indices = gpu_indices(&segment[2])?;
                for node in nodes {
                    if !anchor.nodes.contains(&node) || !covered.insert(node.clone()) {
                        return None;
                    }
                    for index in &indices {
                        node_claims.push((node.clone(), index.clone()));
                    }
                }
            }
            (covered.into_iter().collect::<Vec<_>>() == anchor.nodes).then_some(node_claims)
        })();
        if let Some(node_claims) = valid_claims {
            for key in node_claims {
                claims
                    .entry(key)
                    .or_default()
                    .push((ids[0].clone(), ids.clone()));
            }
        } else {
            out.rejected.extend(ids);
        }
    }
    for ((node, index), mut entries) in claims {
        if entries.len() == 1 {
            out.alloc
                .entry(node)
                .or_default()
                .insert(index, entries.remove(0).0);
        } else {
            for (_, ids) in entries {
                out.rejected.extend(ids);
            }
        }
    }
    Ok(out)
}

pub fn reconcile(
    alloc: &BTreeMap<String, String>,
    users: &BTreeMap<String, String>,
    gpus: &mut [Gpu],
    ignored: &BTreeSet<String>,
) {
    let mut remaining: Vec<_> = alloc.values().cloned().collect();
    for gpu in gpus.iter_mut() {
        gpu.alloc_jobid.clear();
        gpu.alloc_user.clear();
    }
    for gpu in gpus.iter_mut() {
        if let Some(pos) = remaining
            .iter()
            .position(|id| gpu.pid_jobid.values().any(|j| j == id))
        {
            gpu.alloc_jobid = remaining.remove(pos);
            gpu.alloc_user = users.get(&gpu.alloc_jobid).cloned().unwrap_or_default();
        }
    }
    for gpu in gpus.iter_mut().filter(|g| g.alloc_jobid.is_empty()) {
        if let Some(pos) = remaining.iter().position(|id| {
            users
                .get(id)
                .is_some_and(|u| !ignored.contains(u) && gpu.users.contains(u))
        }) {
            gpu.alloc_jobid = remaining.remove(pos);
            gpu.alloc_user = users.get(&gpu.alloc_jobid).cloned().unwrap_or_default();
        }
    }
    for id in remaining {
        let free: Vec<_> = gpus
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                g.alloc_jobid.is_empty() && g.users.iter().all(|u| ignored.contains(u))
            })
            .map(|(i, _)| i)
            .collect();
        let preferred = free
            .iter()
            .find(|i| {
                alloc.get(if gpus[**i].minor.is_empty() {
                    &gpus[**i].index
                } else {
                    &gpus[**i].minor
                }) == Some(&id)
            })
            .copied()
            .or_else(|| free.first().copied());
        if let Some(i) = preferred {
            gpus[i].alloc_jobid = id.clone();
            gpus[i].alloc_user = users.get(&id).cloned().unwrap_or_default();
        }
    }
}

pub trait Runner: Send + Sync {
    fn run(&self, program: Program, args: &[&str], timeout: Duration) -> Result<String>;
}
pub struct SystemRunner;
impl Runner for SystemRunner {
    fn run(&self, program: Program, args: &[&str], timeout: Duration) -> Result<String> {
        process::run(program, args, timeout)
    }
}

pub const QUEUE_FORMAT: &str = "%T|%i|%u|%U|%P|%M|%N|%b|%l|%C|%m|%r|%Q|%S|%V";

#[derive(Clone, Default)]
pub struct Sample {
    pub nodes: Vec<Node>,
    pub jobs: Vec<Job>,
    pub pending: Vec<PendingJob>,
    pub records: JobRecords,
    pub status: SchedulerStatus,
    pub error: String,
}

pub struct Scheduler {
    runner: Arc<dyn Runner>,
    legacy: bool,
    mem_next: f64,
    mem: BTreeMap<String, String>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(Arc::new(SystemRunner))
    }
}
impl Scheduler {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            legacy: false,
            mem_next: 0.0,
            mem: BTreeMap::new(),
        }
    }

    fn queue(&self) -> Result<Queue> {
        Ok(parse_queue(&self.runner.run(
            Program::Squeue,
            &["-h", "-t", "R,PD,CG", "-o", QUEUE_FORMAT],
            Duration::from_secs(12),
        )?))
    }

    pub fn sample(&mut self) -> Sample {
        let stamp = runtime::now();
        let mut sample = Sample::default();
        let timeout = Duration::from_secs(12);
        let (nodes, queue, structured) = std::thread::scope(|scope| {
            let nodes = scope.spawn(|| {
                self.runner.run(
                    Program::Sinfo,
                    &["-N", "-h", "-o", "%N|%T|%c|%O|%m|%e|%G|%C|%P"],
                    timeout,
                )
            });
            let structured = (!self.legacy).then(|| {
                scope.spawn(|| {
                    self.runner
                        .run(Program::Scontrol, &["--json", "show", "jobs"], timeout)
                })
            });
            let queue = self.queue();
            (
                nodes
                    .join()
                    .unwrap_or_else(|_| Err("node query worker panicked".into())),
                queue,
                structured.map(|t| {
                    t.join()
                        .unwrap_or_else(|_| Err("job query worker panicked".into()))
                }),
            )
        });
        match nodes {
            Ok(raw) => sample.nodes = parse_nodes(&raw),
            Err(e) => sample.error = format!("sinfo failed: {e}"),
        }
        let Ok(queue) = queue else {
            sample.error.push_str(" squeue failed");
            sample.status = SchedulerStatus {
                job_backend: "unavailable".into(),
                observed_at: stamp,
                error: sample.error.clone(),
                ..Default::default()
            };
            return sample;
        };
        let mut backend_error = String::new();
        if let Some(result) = structured {
            match result {
                Ok(raw) => match parse_structured(&raw) {
                    Ok(records) => sample.records = records,
                    Err(error) => backend_error = error.to_string(),
                },
                Err(error) => {
                    let text = error.to_string().to_lowercase();
                    if (text.contains("unrecognized option") || text.contains("unknown option"))
                        && text.contains("json")
                        || text.contains("invalid option -- '-'")
                    {
                        self.legacy = true;
                    } else {
                        backend_error = error.to_string();
                    }
                }
            }
        }
        if self.legacy && backend_error.is_empty() {
            let result = (|| -> Result<JobRecords> {
                let raw =
                    self.runner
                        .run(Program::Scontrol, &["-o", "show", "job", "-d"], timeout)?;
                let after = self.queue()?;
                parse_legacy(&raw, &queue.anchors, &after.anchors)
            })();
            match result {
                Ok(records) => sample.records = records,
                Err(error) => backend_error = error.to_string(),
            }
        }
        sample.jobs = queue.jobs;
        sample.pending = queue.pending;
        for job in &mut sample.jobs {
            job.detail = sample
                .records
                .details
                .get(&job.jobid)
                .cloned()
                .unwrap_or_default();
            job.uid = sample
                .records
                .uids
                .get(&job.jobid)
                .map(|u| i64::from(*u))
                .unwrap_or(-1);
            if let Some(user) = sample.records.users.get(&job.jobid) {
                job.user = user.clone();
            }
            job.jobname = detail_field(&job.detail, "JobName");
            job.qos = detail_field(&job.detail, "QOS");
        }
        for job in &mut sample.pending {
            let parent = job.jobid.split('_').next().unwrap_or("");
            job.detail = sample
                .records
                .details
                .get(&job.jobid)
                .or_else(|| sample.records.details.get(parent))
                .cloned()
                .unwrap_or_default();
            job.jobname = detail_field(&job.detail, "JobName");
            job.qos = detail_field(&job.detail, "QOS");
            job.dependency = detail_field(&job.detail, "Dependency");
            job.eligible_time = detail_field(&job.detail, "EligibleTime");
            let submit = detail_field(&job.detail, "SubmitTime");
            if !submit.is_empty() {
                job.submit_time = submit;
            }
        }
        if stamp >= self.mem_next
            && let Ok(raw) = self
                .runner
                .run(Program::Scontrol, &["-o", "show", "node"], timeout)
        {
            self.mem = raw
                .lines()
                .filter_map(|line| {
                    let fields: BTreeMap<_, _> = field_items(line).into_iter().collect();
                    let name = fields.get("NodeName")?;
                    let mem = fields.get("AllocMem")?;
                    if !runtime::valid_host(name) || mem.parse::<u64>().is_err() {
                        return None;
                    }
                    Some((name.clone(), mem.clone()))
                })
                .collect();
            self.mem_next =
                stamp + runtime::env_seconds("SLURM_GPU_TUI_MEM_REFRESH_SEC", 15.0, 1.0);
        }
        for node in &mut sample.nodes {
            node.mem_alloc = self.mem.get(&node.name).cloned().unwrap_or_default();
        }
        if !backend_error.is_empty() {
            sample
                .error
                .push_str(&format!(" scontrol jobs failed: {backend_error}"));
        }
        sample.status = SchedulerStatus {
            job_backend: if backend_error.is_empty() {
                if self.legacy {
                    "legacy-text"
                } else {
                    "structured-json"
                }
            } else {
                "unavailable"
            }
            .into(),
            fallback_reason: if self.legacy {
                "scontrol --json unsupported".into()
            } else {
                String::new()
            },
            error: sample.error.clone(),
            rejected_records: sample.records.rejected.len(),
            observed_at: stamp,
            age_sec: 0.0,
        };
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expansion_preserves_padding_and_rejects_bombs_duplicates_and_odd_names() {
        assert_eq!(
            expand_nodes("rack[1-2]gpu[01-02]"),
            Some(vec![
                "rack1gpu01".into(),
                "rack1gpu02".into(),
                "rack2gpu01".into(),
                "rack2gpu02".into()
            ])
        );
        for input in [
            "gpu[0-999999999]",
            "gpu[1-2],gpu1",
            "../bad",
            "gpu[2-1]",
            "gpu[1-2",
            "gpu[]",
        ] {
            assert!(expand_nodes(input).is_none(), "{input}");
        }
    }

    #[test]
    fn queue_rejects_duplicate_identity_and_keeps_submit_time() {
        let line = "RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:h100:2|01:00:00|4|8G|None|1|N/A|2026-07-01T00:00:00";
        assert_eq!(parse_queue(line).jobs[0].gpu_count, 2);
        let duplicate = parse_queue(&format!("{line}\n{line}"));
        assert!(duplicate.jobs.is_empty());
        assert!(duplicate.anchors.is_empty());
        let pending = parse_queue(&line.replacen("RUNNING", "PENDING", 1));
        assert_eq!(pending.pending[0].submit_time, "2026-07-01T00:00:00");
        assert_eq!(mem_mib("4Gc", 3), Some(12288.0));
    }

    fn modern(resources: Value) -> String {
        json!({"jobs":[{"job_id":487,"name":"train\nJobId=999","user_id":2000,"user_name":"alice","job_state":["RUNNING"],"nodes":"server[3-4]","standard_output":"my logs/out","standard_error":"my logs/out","current_working_directory":"/home/alice","job_resources":resources,"gres_detail":["gpu:x:1(IDX:0)","gpu:x:1(IDX:1)"]}]}).to_string()
    }

    #[test]
    fn slurm_2511_ordering_and_private_paths_are_preserved() {
        let records=parse_structured(&modern(json!({"nodes":{"allocation":[{"index":1,"name":"server4"},{"index":0,"name":"server3"}]}}))).unwrap();
        assert_eq!(records.alloc["server3"]["0"], "487");
        assert_eq!(records.alloc["server4"]["1"], "487");
        assert!(records.logs["487"].merged);
        assert_eq!(
            records.logs["487"].out,
            PathBuf::from("/home/alice/my logs/out")
        );
        assert!(!records.details["487"].contains("\nJobId=999"));
        assert!(!records.details["487"].contains("my logs"));
        let old = parse_structured(&modern(
            json!({"allocated_nodes":[{"nodename":"server3"},{"nodename":"server4"}]}),
        ))
        .unwrap();
        assert_eq!(old.alloc, records.alloc);
        assert!(
            parse_structured(&modern(json!({"allocated_nodes":[{"nodename":"server3"}]})))
                .unwrap()
                .alloc
                .is_empty()
        );
    }

    #[test]
    fn legacy_identity_is_bound_to_stable_numeric_uid_rosters() {
        let queue =
            parse_queue("RUNNING|42|alice|1001|gpu|00:01|gpu1|gpu:1|01:00:00|4|8G|None|1|N/A");
        let line = "JobId=42 JobName=train UserId=alice(1001) JobState=RUNNING NodeList=gpu1 Nodes=gpu1 CPU_IDs=0-3 Mem=8192 GRES=gpu:1(IDX:0) WorkDir=/home/alice StdOut=slurm-42.out StdErr=slurm-42.out";
        let good = parse_legacy(line, &queue.anchors, &queue.anchors).unwrap();
        assert_eq!(good.alloc["gpu1"]["0"], "42");
        assert!(good.logs["42"].merged);
        for bad in [
            line.replace("alice(1001)", "alice(0)"),
            format!("{line}\n{line}"),
            format!("{line} StdOut=/private"),
            line.replace("Nodes=gpu1", "Nodes=gpu2"),
            line.replace("slurm-42.out", "has space"),
        ] {
            let out = parse_legacy(&bad, &queue.anchors, &queue.anchors).unwrap();
            assert!(out.alloc.is_empty(), "{bad}");
        }
        let out = parse_legacy(line, &queue.anchors, &BTreeMap::new()).unwrap();
        assert!(out.logs.is_empty());
    }

    #[test]
    fn cgroups_disambiguate_same_user_and_rogue_is_never_overwritten() {
        let alloc = BTreeMap::from([("0".into(), "10".into()), ("1".into(), "11".into())]);
        let users = BTreeMap::from([("10".into(), "alice".into()), ("11".into(), "alice".into())]);
        let mut gpus = vec![
            Gpu {
                index: "0".into(),
                users: vec!["alice".into()],
                pid_jobid: BTreeMap::from([("123".into(), "11".into())]),
                ..Default::default()
            },
            Gpu {
                index: "1".into(),
                ..Default::default()
            },
            Gpu {
                index: "2".into(),
                users: vec!["bob".into()],
                ..Default::default()
            },
        ];
        reconcile(&alloc, &users, &mut gpus, &BTreeSet::new());
        assert_eq!(gpus[0].alloc_jobid, "11");
        assert_eq!(gpus[1].alloc_jobid, "10");
        assert!(gpus[2].alloc_jobid.is_empty());
    }
}
