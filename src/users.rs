use crate::{
    model::{Snapshot, json_number, number},
    rules, scheduler,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub struct Measure {
    pub value: f64,
    pub samples: usize,
    pub expected: usize,
}
impl Measure {
    pub fn add(&mut self, value: Option<f64>, samples: usize, expected: usize) {
        self.expected += expected;
        if let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0) {
            self.value += value;
            self.samples += samples;
        }
    }
    pub fn text(&self, scale: f64) -> String {
        if self.samples == 0 {
            "?".into()
        } else {
            format!(
                "{}{:.1}",
                if self.samples < self.expected {
                    "~"
                } else {
                    ""
                },
                self.value / scale
            )
        }
    }
}
#[derive(Clone, Default)]
pub struct User {
    pub name: String,
    pub running: usize,
    pub pending: usize,
    pub gpus: usize,
    pub allocated_cpus: u64,
    pub nodes: BTreeSet<String>,
    jobs: BTreeSet<String>,
    pub cpu: Measure,
    pub ram: Measure,
    pub read: Measure,
    pub write: Measure,
    pub rx: Measure,
    pub tx: Measure,
    pub load: Measure,
    pub quota: Measure,
    pub files: Measure,
}
impl User {
    pub fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            format!("{}/{}", self.running, self.pending),
            self.gpus.to_string(),
            self.cpu.text(1.0),
            self.ram.text(1024.0),
            format!(
                "{}/{}",
                self.read.text(1048576.0),
                self.write.text(1048576.0)
            ),
            format!("{}/{}", self.rx.text(1048576.0), self.tx.text(1048576.0)),
            self.load.text(1.0),
            self.quota.text(1073741824.0),
            self.files.text(1.0),
            self.nodes.iter().cloned().collect::<Vec<_>>().join(","),
        ]
    }
}
pub fn storage_fresh(controller: &serde_json::Value, now: f64) -> bool {
    json_number(&controller["storage_observed_at"])
        .is_some_and(|stamp| stamp > 0.0 && (-5.0..=300.0).contains(&(now - stamp)))
}
pub fn summarize(data: &Snapshot, now: f64, user: &str, partition: &str) -> Vec<User> {
    let mut out = BTreeMap::<String, User>::new();
    let matches = |name: &str, part: &str| {
        (user.is_empty() || name == user)
            && (partition.is_empty() || part.split(',').any(|p| p == partition))
    };
    for job in data.jobs.iter().filter(|j| matches(&j.user, &j.partition)) {
        let row = out.entry(job.user.clone()).or_insert_with(|| User {
            name: job.user.clone(),
            ..Default::default()
        });
        row.running += 1;
        row.jobs.insert(job.jobid.clone());
        row.allocated_cpus += job.cpu_count as u64;
        row.nodes
            .extend(scheduler::expand_nodes(&job.node).unwrap_or_default());
        let metrics = rules::resource_summary(job, &data.nodes, now);
        let expected = metrics.get("expected_nodes").copied().unwrap_or(0.0) as usize;
        for (key, target) in [
            ("cpu_cores", &mut row.cpu),
            ("mem_current_mib", &mut row.ram),
            ("io_read_bps", &mut row.read),
            ("io_write_bps", &mut row.write),
        ] {
            target.add(
                metrics.get(key).copied(),
                metrics.get(&format!("{key}_nodes")).copied().unwrap_or(0.0) as usize,
                expected,
            );
        }
    }
    for job in data
        .pending
        .iter()
        .filter(|j| matches(&j.user, &j.partition))
    {
        out.entry(job.user.clone())
            .or_insert_with(|| User {
                name: job.user.clone(),
                ..Default::default()
            })
            .pending += 1;
    }
    let quota_fresh = storage_fresh(&data.controller, now);
    if quota_fresh && partition.is_empty() {
        for fs in data.controller["filesystems"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(quotas) = fs["quotas"].as_object() {
                for name in quotas
                    .keys()
                    .filter(|name| user.is_empty() || name.as_str() == user)
                {
                    out.entry(name.clone()).or_insert_with(|| User {
                        name: name.clone(),
                        ..Default::default()
                    });
                }
            }
        }
    }
    for row in out.values_mut() {
        for name in &row.nodes {
            let Some(node) = data.nodes.iter().find(|n| n.name == *name) else {
                row.rx.add(None, 0, 1);
                row.tx.add(None, 0, 1);
                row.load.expected += 1;
                continue;
            };
            row.gpus += node
                .gpus
                .iter()
                .filter(|g| g.alloc_user == row.name && row.jobs.contains(&g.alloc_jobid))
                .count();
            let fresh = !node.stale && rules::telemetry_fresh(node.telemetry_observed_at, now);
            for (key, target) in [("net_rx_bps", &mut row.rx), ("net_tx_bps", &mut row.tx)] {
                let value = fresh
                    .then(|| node.host_io.get(key).and_then(|v| number(v)))
                    .flatten();
                target.add(value, usize::from(value.is_some()), 1);
            }
            let load = fresh
                .then(|| node.host_io.get("load1").and_then(|v| number(v)))
                .flatten();
            row.load.expected += 1;
            if let Some(load) = load {
                row.load.value = row.load.value.max(load);
                row.load.samples += 1;
            }
        }
        if quota_fresh {
            for fs in data.controller["filesystems"]
                .as_array()
                .into_iter()
                .flatten()
            {
                for (key, target) in [("used_bytes", &mut row.quota), ("files", &mut row.files)] {
                    let value = json_number(&fs["quotas"][&row.name][key]);
                    target.add(value, usize::from(value.is_some()), 1);
                }
            }
            if data.controller["storage_truncated"] == true {
                row.quota.expected += 1;
                row.files.expected += 1;
            }
        }
    }
    out.into_values().collect()
}
#[derive(Default)]
pub struct Attention {
    pub user: String,
    pub idle: usize,
    pub parked: usize,
    pub rogue: usize,
    pub longest: u64,
    pub jobs: BTreeSet<String>,
    pub nodes: BTreeSet<String>,
    pub details: Vec<String>,
}
impl Attention {
    pub fn total(&self) -> usize {
        self.idle + self.parked + self.rogue
    }
    pub fn cells(&self) -> Vec<String> {
        vec![
            self.user.clone(),
            self.total().to_string(),
            self.idle.to_string(),
            self.parked.to_string(),
            self.rogue.to_string(),
            if self.idle + self.parked > 0 {
                rules::span(self.longest)
            } else {
                "—".into()
            },
            self.jobs.iter().cloned().collect::<Vec<_>>().join(","),
            self.nodes.iter().cloned().collect::<Vec<_>>().join(","),
        ]
    }
}
pub fn attention(data: &Snapshot, minimum: u64, now: f64) -> Vec<Attention> {
    attention_in_partition(data, minimum, now, "")
}
pub fn attention_in_partition(
    data: &Snapshot,
    minimum: u64,
    now: f64,
    partition: &str,
) -> Vec<Attention> {
    let mut out = BTreeMap::<String, Attention>::new();
    for waste in rules::waste(&data.nodes, minimum) {
        let Some(node) = data.nodes.iter().find(|n| {
            n.name == waste.node
                && n.scheduler_available
                && n.error.is_empty()
                && n.scheduler_age_sec <= 30.0
        }) else {
            continue;
        };
        let Some(index) = node.gpus.iter().position(|g| g.index == waste.gpu) else {
            continue;
        };
        if !partition.is_empty() {
            let included = if waste.jobid.is_empty() {
                node.partition.split(',').any(|p| p == partition)
            } else {
                data.jobs.iter().any(|j| {
                    j.jobid == waste.jobid && j.partition.split(',').any(|p| p == partition)
                })
            };
            if !included {
                continue;
            }
        }
        let class = rules::classes(node, now)[index];
        if !matches!(
            (waste.kind.as_str(), class),
            ("idle", rules::Class::Idle)
                | ("parked", rules::Class::Parked)
                | ("rogue" | "no-gres", rules::Class::Rogue)
        ) {
            continue;
        }
        let user = if !node.gpus[index].alloc_user.is_empty() {
            node.gpus[index].alloc_user.clone()
        } else if waste.user.is_empty() {
            "(unattributed)".into()
        } else {
            waste.user.clone()
        };
        let row = out.entry(user.clone()).or_insert_with(|| Attention {
            user,
            ..Default::default()
        });
        match waste.kind.as_str() {
            "idle" => row.idle += 1,
            "parked" => row.parked += 1,
            _ => row.rogue += 1,
        }
        row.longest = row.longest.max(waste.sec);
        row.nodes.insert(waste.node.clone());
        if !waste.jobid.is_empty() {
            row.jobs.insert(waste.jobid.clone());
        }
        row.details.push(format!(
            "{} GPU{} · {} · {} · #{}",
            waste.node,
            waste.gpu,
            waste.kind,
            rules::span(waste.sec),
            waste.jobid
        ));
    }
    let mut rows: Vec<_> = out.into_values().collect();
    rows.sort_by(|a, b| {
        b.rogue
            .cmp(&a.rogue)
            .then(b.total().cmp(&a.total()))
            .then(b.longest.cmp(&a.longest))
            .then(a.user.cmp(&b.user))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Gpu, Job, Node};
    use serde_json::json;
    #[test]
    fn partition_gpu_counts_quota_age_and_missing_node_coverage_are_independent() {
        let now = crate::runtime::now();
        let mut data = Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                telemetry_observed_at: now,
                host_io: BTreeMap::from([("net_rx_bps".into(), "100".into())]),
                gpus: vec![
                    Gpu {
                        alloc_user: "alice".into(),
                        alloc_jobid: "1".into(),
                        ..Default::default()
                    },
                    Gpu {
                        alloc_user: "alice".into(),
                        alloc_jobid: "2".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            jobs: vec![
                Job {
                    jobid: "1".into(),
                    user: "alice".into(),
                    partition: "a".into(),
                    node: "gpu[1-2]".into(),
                    ..Default::default()
                },
                Job {
                    jobid: "2".into(),
                    user: "alice".into(),
                    partition: "b".into(),
                    node: "gpu1".into(),
                    ..Default::default()
                },
            ],
            controller: json!({"observed_at":now,"storage_observed_at":now-200.0,"filesystems":[{"quotas":{"alice":{"used_bytes":1073741824,"files":3}}}]}),
            ..Default::default()
        };
        let row = &summarize(&data, now, "alice", "a")[0];
        assert_eq!(row.gpus, 1);
        assert_eq!(row.rx.text(1.0), "~100.0");
        assert_eq!(row.quota.text(1073741824.0), "1.0");
        data.controller["storage_observed_at"] = json!(now - 301.0);
        assert_eq!(summarize(&data, now, "alice", "a")[0].quota.text(1.0), "?");
        data.controller["storage_observed_at"] = json!(now);
        data.controller["storage_truncated"] = json!(true);
        assert_eq!(
            summarize(&data, now, "alice", "a")[0]
                .quota
                .text(1073741824.0),
            "~1.0"
        );
    }
    #[test]
    fn shared_hosts_are_deduplicated_per_user_and_stale_values_stay_unknown() {
        let now = crate::runtime::now();
        let node = Node {
            name: "gpu1".into(),
            telemetry_observed_at: now,
            host_io: BTreeMap::from([
                ("net_rx_bps".into(), "1048576".into()),
                ("net_tx_bps".into(), "0".into()),
                ("load1".into(), "3".into()),
            ]),
            ..Default::default()
        };
        let job = |id: &str, user: &str| Job {
            jobid: id.into(),
            user: user.into(),
            node: "gpu1".into(),
            telemetry: BTreeMap::from([(
                "gpu1".into(),
                json!({"observed_at":now,"cpu_cores":1,"io_read_bps":0}),
            )]),
            ..Default::default()
        };
        let mut data = Snapshot {
            nodes: vec![node],
            jobs: vec![job("1", "alice"), job("2", "alice"), job("3", "bob")],
            ..Default::default()
        };
        let rows = summarize(&data, now, "", "");
        assert_eq!(rows[0].cpu.text(1.0), "2.0");
        assert_eq!(rows[0].rx.text(1048576.0), "1.0");
        assert_eq!(rows[1].rx.text(1048576.0), "1.0");
        assert_eq!(rows[0].write.text(1.0), "?");
        assert_eq!(rows[0].read.text(1.0), "0.0");
        data.nodes[0].stale = true;
        let rows = summarize(&data, now, "alice", "");
        assert_eq!(rows[0].cpu.text(1.0), "?");
        assert_eq!(rows[0].rx.text(1.0), "?");
    }
    #[test]
    fn attention_requires_current_evidence_and_excludes_stale_nodes() {
        let now = crate::runtime::now();
        let mut data = Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                state: "allocated".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    util: "0".into(),
                    mem_used: "0".into(),
                    mem_total: "100".into(),
                    alloc_user: "alice".into(),
                    alloc_jobid: "42".into(),
                    idle_sec: 1200,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(attention(&data, 600, now)[0].idle, 1);
        data.nodes[0].gpus[0].util = "80".into();
        assert!(attention(&data, 600, now).is_empty());
        data.nodes[0].gpus[0].util = "0".into();
        data.nodes[0].stale = true;
        assert!(attention(&data, 600, now).is_empty());
    }
}
