use crate::{
    model::{Gpu, Job, Node, PendingJob, json_number, number},
    runtime, scheduler,
};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Busy,
    Parked,
    Idle,
    Free,
    Rogue,
    Unknown,
}

pub fn ignored_users() -> BTreeSet<String> {
    std::env::var("SLURM_GPU_TUI_ROGUE_IGNORE")
        .unwrap_or_else(|_| "root,gdm,xdm".into())
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn classify(gpu: &Gpu, ignored: &BTreeSet<String>) -> Class {
    let real = gpu.users.iter().any(|u| !ignored.contains(u));
    if real && gpu.alloc_jobid.is_empty() {
        return Class::Rogue;
    }
    let Some(util) = number(&gpu.util) else {
        return Class::Unknown;
    };
    if util > 5.0 {
        return Class::Busy;
    }
    if number(&gpu.mem_used)
        .zip(number(&gpu.mem_total))
        .is_some_and(|(used, total)| total > 0.0 && used / total >= 0.3)
    {
        return Class::Parked;
    }
    if !gpu.alloc_jobid.is_empty() || real {
        return Class::Idle;
    }
    Class::Free
}

pub fn schedulable(node: &Node) -> bool {
    !node.stale
        && node.error.is_empty()
        && node.scheduler_available
        && node.scheduler_age_sec <= 30.0
        && !["down", "drain", "fail", "maint", "unknown"]
            .iter()
            .any(|s| node.state.to_lowercase().contains(s))
}

pub fn telemetry_fresh(stamp: f64, now: f64) -> bool {
    stamp > 0.0
        && (-5.0..=runtime::env_seconds("SLURM_GPU_TUI_TELEMETRY_SEC", 10.0, 1.0)
            .mul_add(2.0, 0.0)
            .max(30.0))
            .contains(&(now - stamp))
}

pub fn health(gpu: &Gpu, now: f64) -> (String, u8) {
    let mut flags = Vec::new();
    let mut severity = 0;
    let fresh = telemetry_fresh(gpu.health_observed_at, now);
    if fresh {
        match gpu.recovery_action.to_lowercase().as_str() {
            "" | "none" => (),
            "reboot" => {
                flags.push("REBOOT".into());
                severity = 2;
            }
            "reset" => {
                flags.push("RESET".into());
                severity = 2;
            }
            _ => {
                flags.push("DRAIN".into());
                severity = 2;
            }
        }
        if gpu.clock_reasons.contains("thermal") {
            flags.push("THERMAL".into());
            severity = 2;
        } else if ["hw-slowdown", "power-brake"]
            .iter()
            .any(|s| gpu.clock_reasons.contains(s))
        {
            flags.push("SLOW".into());
            severity = severity.max(1);
        } else if gpu.clock_reasons.contains("power-cap") {
            flags.push("CAP".into());
        }
    }
    let temp = number(&gpu.temp);
    let ecc = number(&gpu.ecc);
    if temp.is_some_and(|v| v >= 85.0) {
        flags.push("HOT".into());
        severity = severity.max(1);
    }
    if let Some(ecc) = ecc.filter(|v| *v > 0.0) {
        flags.push(format!("ECC:{ecc:.0}"));
        severity = severity.max(1);
    }
    if flags.is_empty() {
        (
            if temp.is_some() || ecc.is_some() || fresh {
                "OK"
            } else {
                "—"
            }
            .into(),
            severity,
        )
    } else {
        (flags.join("/"), severity)
    }
}

pub fn classes(node: &Node, now: f64) -> Vec<Class> {
    let ignored = ignored_users();
    node.gpus
        .iter()
        .map(|gpu| {
            if node.stale {
                return Class::Unknown;
            }
            let class = classify(gpu, &ignored);
            if class == Class::Free && (!schedulable(node) || health(gpu, now).1 == 2) {
                Class::Unknown
            } else {
                class
            }
        })
        .collect()
}

pub fn duration(raw: &str) -> Option<u64> {
    let (days, time) = raw
        .split_once('-')
        .map(|(d, t)| Some((d.parse::<u64>().ok()?, t)))
        .unwrap_or(Some((0, raw)))?;
    let p: Vec<_> = time
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let seconds = match p.as_slice() {
        [hours, minutes, seconds] => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        _ => return None,
    };
    days.checked_mul(86400)?.checked_add(seconds)
}

pub fn span(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{:.1}h", seconds as f64 / 3600.0)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

pub fn parse_timestamp(raw: &str) -> Option<f64> {
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|n| Local.from_local_datetime(&n).earliest())
        .map(|d| d.timestamp() as f64)
}

pub fn pending_wait(job: &PendingJob, now: f64) -> Option<f64> {
    parse_timestamp(&job.submit_time).map(|s| (now - s).max(0.0))
}

pub fn pending_reason(job: &PendingJob) -> String {
    if job.reason.starts_with("Dependency")
        && !matches!(job.dependency.as_str(), "" | "None" | "(null)")
    {
        format!("{} {}", job.reason, job.dependency)
    } else if (job.reason.starts_with("QOS") || job.reason.starts_with("Assoc"))
        && !job.qos.is_empty()
    {
        format!("{} [{}]", job.reason, job.qos)
    } else {
        job.reason.clone()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Waste {
    pub node: String,
    pub gpu: String,
    pub kind: String,
    pub sec: u64,
    pub user: String,
    pub jobid: String,
}

pub fn waste(nodes: &[Node], minimum: u64) -> Vec<Waste> {
    let ignored = ignored_users();
    let mut out = Vec::new();
    for node in nodes.iter().filter(|n| !n.stale) {
        for gpu in &node.gpus {
            let real: Vec<_> = gpu
                .users
                .iter()
                .filter(|u| !ignored.contains(*u))
                .cloned()
                .collect();
            let (kind, sec, user, jobid) = if !real.is_empty() && gpu.alloc_jobid.is_empty() {
                let jobid = node
                    .jobs
                    .iter()
                    .find(|j| j.gpu_count == 0 && real.contains(&j.user))
                    .map(|j| j.jobid.clone())
                    .unwrap_or_default();
                (
                    if jobid.is_empty() { "rogue" } else { "no-gres" },
                    0,
                    real.join(","),
                    jobid,
                )
            } else if gpu.idle_sec >= minimum {
                (
                    "idle",
                    gpu.idle_sec,
                    gpu.alloc_user.clone(),
                    gpu.alloc_jobid.clone(),
                )
            } else if gpu.parked_sec >= minimum {
                (
                    "parked",
                    gpu.parked_sec,
                    if gpu.alloc_user.is_empty() {
                        gpu.users.join(",")
                    } else {
                        gpu.alloc_user.clone()
                    },
                    gpu.alloc_jobid.clone(),
                )
            } else {
                continue;
            };
            out.push(Waste {
                node: node.name.clone(),
                gpu: gpu.index.clone(),
                kind: kind.into(),
                sec,
                user,
                jobid,
            });
        }
    }
    out.sort_by(|a, b| {
        let key = |w: &Waste| !matches!(w.kind.as_str(), "rogue" | "no-gres");
        key(a)
            .cmp(&key(b))
            .then(b.sec.cmp(&a.sec))
            .then(a.node.cmp(&b.node))
    });
    out
}

pub fn resource_summary(job: &Job, nodes: &[Node], now: f64) -> BTreeMap<String, f64> {
    let expected = scheduler::expand_nodes(&job.node).unwrap_or_default();
    let mut out = BTreeMap::from([
        ("expected_nodes".into(), expected.len() as f64),
        ("sampled_nodes".into(), 0.0),
    ]);
    for (name, raw) in &job.telemetry {
        if !expected.contains(name)
            || !nodes.iter().any(|n| {
                n.name == *name
                    && !n.stale
                    && n.error.is_empty()
                    && n.scheduler_available
                    && n.scheduler_age_sec <= 30.0
            })
            || !json_number(&raw["observed_at"]).is_some_and(|stamp| telemetry_fresh(stamp, now))
        {
            continue;
        }
        let sample = crate::node_telemetry::clean_sample(raw);
        if sample.as_object().map_or(0, |m| m.len()) <= 1 {
            continue;
        }
        *out.get_mut("sampled_nodes").unwrap() += 1.0;
        for key in [
            "cpu_cores",
            "mem_current_mib",
            "mem_peak_mib",
            "mem_limit_mib",
            "oom_kill",
            "vram_mib",
            "io_read_bps",
            "io_write_bps",
        ] {
            if let Some(v) = json_number(&sample[key]) {
                *out.entry(key.into()).or_default() += v;
                *out.entry(format!("{key}_nodes")).or_default() += 1.0;
            }
        }
    }
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct FitResult {
    pub node: String,
    pub eligible: bool,
    pub free_gpus: usize,
    pub reasons: Vec<String>,
}

pub fn fit(
    nodes: &[Node],
    gpus: u32,
    vram: f64,
    model: &str,
    cpus: u32,
    ram: f64,
    partition: &str,
) -> Vec<FitResult> {
    nodes
        .iter()
        .filter(|n| n.has_gpu)
        .map(|node| {
            let mut reasons = Vec::new();
            if !schedulable(node) {
                reasons.push("stale, unavailable, or unschedulable node".into());
            }
            if !partition.is_empty() && !node.partition.split(',').any(|p| p == partition) {
                reasons.push("partition does not match".into());
            }
            let normalize = |value: &str| {
                value
                    .to_lowercase()
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>()
            };
            let free = node
                .gpus
                .iter()
                .zip(classes(node, runtime::now()))
                .filter(|(gpu, class)| {
                    *class == Class::Free
                        && (vram <= 0.0
                            || number(&gpu.mem_total).is_some_and(|m| m >= vram * 1024.0))
                        && (model.is_empty()
                            || node.gres.split(',').any(|g| {
                                g.split(':')
                                    .nth(1)
                                    .is_some_and(|label| label.eq_ignore_ascii_case(model))
                            }) && normalize(&gpu.name).contains(&normalize(model)))
                })
                .count();
            if free < (gpus as usize) {
                reasons.push("insufficient free GPUs matching model/VRAM".into());
            }
            if cpus > 0
                && !number(&node.cpus)
                    .zip(number(&node.cpu_alloc))
                    .is_some_and(|(total, allocated)| total - allocated >= cpus as f64)
            {
                reasons.push("insufficient scheduler CPU allocation headroom".into());
            }
            if ram > 0.0
                && !number(&node.scheduler_mem_total)
                    .zip(number(&node.mem_alloc))
                    .is_some_and(|(total, allocated)| total - allocated >= ram * 1024.0)
            {
                reasons.push("insufficient scheduler RAM allocation headroom".into());
            }
            FitResult {
                node: node.name.clone(),
                eligible: reasons.is_empty(),
                free_gpus: free,
                reasons,
            }
        })
        .collect()
}

pub fn observation(nodes: &[Node]) -> [Option<f64>; 3] {
    let mut utils = Vec::new();
    let mut memory = Vec::new();
    let mut watts = Vec::new();
    for gpu in nodes
        .iter()
        .filter(|n| n.has_gpu && !n.stale && n.observed_at > 0.0)
        .flat_map(|n| &n.gpus)
    {
        if let Some(n) = number(&gpu.util) {
            utils.push(n);
        }
        if let Some((used, total)) = number(&gpu.mem_used)
            .zip(number(&gpu.mem_total))
            .filter(|(_, t)| *t > 0.0)
        {
            memory.push(100.0 * used / total);
        }
        if let Some(n) = number(&gpu.power) {
            watts.push(n);
        }
    }
    let mean = |v: &Vec<f64>| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    [
        mean(&utils),
        mean(&memory),
        (!watts.is_empty()).then(|| watts.iter().sum()),
    ]
}

pub fn sample_metric(sample: &Value, key: &str) -> Option<f64> {
    json_number(&sample[key]).filter(|n| *n >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_stale_and_health_action_never_become_free() {
        let mut node = Node {
            gpus: vec![Gpu {
                util: "0".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(classes(&node, 100.0), [Class::Free]);
        node.scheduler_available = false;
        assert_eq!(classes(&node, 100.0), [Class::Unknown]);
        node.scheduler_available = true;
        node.gpus[0].health_observed_at = 100.0;
        node.gpus[0].recovery_action = "reset".into();
        assert_eq!(classes(&node, 100.0), [Class::Unknown]);
        node.gpus[0].recovery_action.clear();
        node.gpus[0].util = "NaN".into();
        assert_eq!(classes(&node, 100.0), [Class::Unknown]);
    }
    #[test]
    fn rogue_precedes_busy_and_duration_is_checked() {
        let gpu = Gpu {
            users: vec!["alice".into()],
            util: "99".into(),
            ..Default::default()
        };
        assert_eq!(classify(&gpu, &BTreeSet::new()), Class::Rogue);
        assert_eq!(duration("1-02:03:04"), Some(93784));
        assert_eq!(duration("03:04"), Some(184));
        assert_eq!(duration("UNLIMITED"), None);
        assert_eq!(duration("18446744073709551615-01:00:00"), None);
    }
}
