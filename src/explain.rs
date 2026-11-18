use crate::{
    cli,
    model::{Node, PendingJob, Snapshot, number},
    rules, runtime,
};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct NodeObservation {
    pub node: String,
    pub partition: String,
    pub fresh: bool,
    pub has_gpu: bool,
    pub free_gpus: Option<usize>,
    pub free_cpus: Option<f64>,
    pub free_ram_gib: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub job: PendingJob,
    pub reason_full: String,
    pub interpretation: String,
    pub wait_seconds: Option<u64>,
    pub snapshot: String,
    pub nodes: Vec<NodeObservation>,
}

fn meaning(reason: &str) -> &'static str {
    if reason.starts_with("DependencyNeverSatisfied") {
        "A dependency cannot be satisfied. Review the prerequisite job outcomes and dependency expression."
    } else if reason.starts_with("Dependency") {
        "A prerequisite has not completed in the required state. Review the dependency expression."
    } else if reason.starts_with("Priority") {
        "Higher-priority work is ahead under the applicable partition/reservation policy. The score is not a queue position."
    } else if reason.starts_with("Resources") {
        "The scheduler cannot allocate the requested resources now. Placement, reservations and policy can matter even when a node has spare resources."
    } else if reason.starts_with("QOS") || reason.starts_with("Assoc") {
        "A QOS or association limit/policy is blocking this request. Check the exact reason and assigned QOS with your administrator."
    } else if reason.starts_with("JobHeld") {
        "The job is held. Check who placed the hold and whether it should remain in effect."
    } else if reason.starts_with("ReqNodeNotAvail") || reason.starts_with("NodeDown") {
        "Requested nodes are unavailable or reserved. Review the node state, reservation and requested placement."
    } else if reason.starts_with("Partition") {
        "A partition state or limit is blocking the job. Review the requested partition and the exact scheduler reason."
    } else if reason.starts_with("BeginTime") {
        "The requested earliest start time has not arrived."
    } else if reason.starts_with("BadConstraints") {
        "The requested constraints cannot currently be matched. Review the submitted constraints and node features."
    } else if reason.starts_with("Licenses") {
        "Requested licenses are unavailable."
    } else if reason.starts_with("Reservation") {
        "A reservation condition is blocking the request. Review its name, time and resource limits."
    } else {
        "No built-in interpretation for this reason. Keep the raw reason and review the scheduler detail."
    }
}

fn headroom_num(total: &str, allocated: &str, scale: f64) -> Option<f64> {
    number(total)
        .zip(number(allocated))
        .filter(|(total, used)| *total >= 0.0 && *used >= 0.0)
        .map(|(total, used)| (total - used).max(0.0) / scale)
}

fn headroom(total: &str, allocated: &str, scale: f64) -> String {
    headroom_num(total, allocated, scale)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "?".into())
}

fn partitions_match(node: &Node, requested: &str) -> bool {
    requested.is_empty()
        || node
            .partition
            .split(',')
            .any(|part| requested.split(',').any(|want| part == want))
}

pub fn report(data: &Snapshot, job: &PendingJob) -> Report {
    let now = runtime::now();
    let mut matching: Vec<_> = data
        .nodes
        .iter()
        .filter(|node| partitions_match(node, &job.partition))
        .collect();
    matching.sort_by(|a, b| a.name.cmp(&b.name));
    let nodes = matching
        .into_iter()
        .map(|node| {
            let fresh = data.errors.is_empty() && rules::schedulable(node);
            let free_gpus = if fresh && node.has_gpu {
                Some(
                    rules::classes(node, now)
                        .iter()
                        .filter(|class| **class == rules::Class::Free)
                        .count(),
                )
            } else {
                None
            };
            NodeObservation {
                node: node.name.clone(),
                partition: node.partition.clone(),
                fresh,
                has_gpu: node.has_gpu,
                free_gpus,
                free_cpus: fresh
                    .then(|| headroom_num(&node.cpus, &node.cpu_alloc, 1.0))
                    .flatten(),
                free_ram_gib: fresh
                    .then(|| headroom_num(&node.scheduler_mem_total, &node.mem_alloc, 1024.0))
                    .flatten(),
            }
        })
        .collect();
    Report {
        version: 1,
        job: job.clone(),
        reason_full: rules::pending_reason(job),
        interpretation: meaning(&job.reason).into(),
        wait_seconds: rules::pending_wait(job, now).map(|wait| wait as u64),
        snapshot: data.ts.clone(),
        nodes,
    }
}

pub fn pending(data: &Snapshot, job: &PendingJob) -> String {
    let now = runtime::now();
    let value = |text: &str| {
        if text.is_empty() {
            "?".into()
        } else {
            cli::terminal_safe(text)
        }
    };
    let mut out = format!(
        "Job #{} · {}\n\nScheduler facts\nReason: {}\nPriority: {}\nPartition: {}\nQOS: {}\nDependency: {}\nRequested GPUs/node: {} · CPUs/job: {} · memory (raw): {}\nWait: {} · estimated start: {}\nSnapshot: {}\n\nInterpretation\n{}\n\nPer-node observations (not a whole-job fit test)\n{:<18} {:>9} {:>10} {:>13}\n",
        value(&job.jobid),
        value(&job.user),
        value(&rules::pending_reason(job)),
        value(&job.priority),
        value(&job.partition),
        value(&job.qos),
        value(&job.dependency),
        job.gpu_count,
        job.cpu_count,
        value(&job.mem),
        rules::pending_wait(job, now)
            .map(|wait| rules::span(wait as u64))
            .unwrap_or_else(|| "?".into()),
        value(&job.start_time),
        value(&data.ts),
        meaning(&job.reason),
        "node",
        "free GPU",
        "CPU spare",
        "RAM spare GiB"
    );
    let mut nodes: Vec<_> = data
        .nodes
        .iter()
        .filter(|node| partitions_match(node, &job.partition))
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    for node in nodes.iter().take(12) {
        let fresh = data.errors.is_empty() && rules::schedulable(node);
        let free = if !fresh {
            "?".into()
        } else if !node.has_gpu {
            "—".into()
        } else {
            rules::classes(node, now)
                .iter()
                .filter(|class| **class == rules::Class::Free)
                .count()
                .to_string()
        };
        out.push_str(&format!(
            "{:<18} {:>9} {:>10} {:>13}{}\n",
            cli::terminal_safe(&node.name),
            free,
            if fresh {
                headroom(&node.cpus, &node.cpu_alloc, 1.0)
            } else {
                "?".into()
            },
            if fresh {
                headroom(&node.scheduler_mem_total, &node.mem_alloc, 1024.0)
            } else {
                "?".into()
            },
            if fresh {
                ""
            } else {
                "  unavailable / stale / unschedulable"
            }
        ));
    }
    if nodes.is_empty() {
        out.push_str("No nodes in the published roster match the requested partition.\n");
    }
    if nodes.len() > 12 {
        out.push_str(&format!("… {} more nodes omitted\n", nodes.len() - 12));
    }
    out.push_str("\nLimits\nOnly the scheduler-reported reason is a fact; multiple conditions may block a job.\nNode observations do not evaluate node counts, per-task placement, GRES types, constraints, reservations or QOS/association policy.\nStart estimates and spare resources do not guarantee admission. No cluster settings are changed.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn facts_and_policy_interpretation_stay_separate_from_node_observations() {
        let job = PendingJob {
            jobid: "42".into(),
            reason: "Dependency".into(),
            dependency: "afterok:20".into(),
            partition: "cpu".into(),
            ..Default::default()
        };
        let mut data = Snapshot {
            nodes: vec![Node {
                name: "cpu1".into(),
                has_gpu: false,
                partition: "cpu".into(),
                cpus: "32".into(),
                cpu_alloc: "8".into(),
                scheduler_mem_total: "65536".into(),
                mem_alloc: "32768".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = pending(&data, &job);
        assert!(text.contains("Reason: Dependency afterok:20"));
        assert!(text.contains("24.0") && text.contains("32.0"));
        assert!(text.contains("not a whole-job fit test"));
        data.nodes[0].stale = true;
        let text = pending(&data, &job);
        assert!(!text.contains("24.0"));
        assert!(text.contains("unavailable / stale"));
        assert!(meaning("QOSMaxGRESPerUser").contains("limit/policy"));
        assert!(meaning("FutureReason").contains("No built-in interpretation"));
    }
}
