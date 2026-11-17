use crate::{
    Result, job_view,
    model::{Gpu, Job, Node, PendingJob, Snapshot},
    runtime,
    ui::App,
};
use ratatui::{Terminal, backend::TestBackend};
use serde::Serialize;
use std::{path::Path, time::Instant};

pub fn synthetic(nodes: usize) -> Snapshot {
    let mut data = Snapshot::default();
    for i in 0..nodes {
        let name = format!("gpu{i:04}");
        let job = Job {
            jobid: (1000 + i).to_string(),
            user: "fixture".into(),
            node: name.clone(),
            gpu_count: 8,
            cpu_count: 32,
            time_limit: "1-00:00:00".into(),
            elapsed: "01:00".into(),
            ..Default::default()
        };
        data.jobs.push(job.clone());
        data.nodes.push(Node {
            name,
            state: "mix".into(),
            partition: "gpu".into(),
            cpus: "64".into(),
            cpu_alloc: "32".into(),
            mem_total: "262144".into(),
            mem_avail: "131072".into(),
            jobs: vec![job.clone()],
            observed_at: 1.0,
            gpus: (0..8)
                .map(|g| Gpu {
                    index: g.to_string(),
                    name: "H100".into(),
                    util: "60".into(),
                    temp: "60".into(),
                    power: "200".into(),
                    mem_used: "10000".into(),
                    mem_total: "80000".into(),
                    users: vec!["fixture".into()],
                    alloc_jobid: job.jobid.clone(),
                    alloc_user: "fixture".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
    }
    data
}
#[derive(Serialize)]
pub struct Measurement {
    case: String,
    nodes: usize,
    gpus: usize,
    jobs: usize,
    pending: usize,
    median_update_render_ms: f64,
    median_parse_ms: f64,
    median_apply_ms: f64,
    median_render_ms: f64,
}
pub fn run(nodes: usize, repeat: usize, replay: Option<&Path>) -> Result<Vec<Measurement>> {
    measure(nodes, repeat, replay, None)
}

pub fn suite(nodes: usize, repeat: usize, jobs: usize) -> Result<Vec<Measurement>> {
    if !(1..=100_000).contains(&jobs) {
        return Err("jobs must be 1..100000".into());
    }
    measure(nodes, repeat, None, Some(jobs))
}

pub fn queued(nodes: usize, jobs: usize) -> Snapshot {
    let mut data = synthetic(nodes);
    data.pending = (0..jobs)
        .map(|i| PendingJob {
            jobid: (100_000 + i).to_string(),
            user: format!("user{}", i % 32),
            partition: "gpu".into(),
            priority: if i % 17 == 0 {
                String::new()
            } else {
                ((i * 7919) % 99991).to_string()
            },
            submit_time: if i % 29 == 0 {
                String::new()
            } else {
                format!(
                    "2026-11-16T{:02}:{:02}:{:02}",
                    (i * 13) % 24,
                    (i * 29) % 60,
                    (i * 47) % 60
                )
            },
            cpu_count: 8,
            gpu_count: 1,
            reason: "Priority".into(),
            ..Default::default()
        })
        .collect();
    data
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn measure(
    nodes: usize,
    repeat: usize,
    replay: Option<&Path>,
    jobs: Option<usize>,
) -> Result<Vec<Measurement>> {
    if !(1..=4096).contains(&nodes) || !(1..=1000).contains(&repeat) {
        return Err("nodes must be 1..4096 and repeat 1..1000".into());
    }
    let replay = if let Some(path) = replay {
        let (bytes, _) = runtime::read_regular(path, 64 * 1024 * 1024, None)?;
        let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
        let rows = if let Some(array) = raw.as_array() {
            array.clone()
        } else if let Some(array) = raw.get("snapshots").and_then(|v| v.as_array()) {
            array.clone()
        } else if raw.is_object() {
            vec![raw]
        } else {
            return Err("replay requires a snapshot or nonempty list".into());
        };
        if rows.is_empty() || rows.iter().any(|r| !r.is_object()) {
            return Err("replay contains no snapshots or a malformed entry".into());
        }
        Some(
            rows.into_iter()
                .map(serde_json::from_value)
                .collect::<std::result::Result<Vec<Snapshot>, _>>()?,
        )
    } else {
        None
    };
    let mut cases = if replay.is_some() {
        vec!["replay"]
    } else {
        vec![
            "identical",
            "one_util",
            "missing_metric",
            "stale_node",
            "collapsed",
        ]
    };
    if jobs.is_some() {
        cases.extend([
            "jobs_priority",
            "jobs_wait",
            "jobs_left",
            "attention",
            "users",
            "search",
            "free_sort",
            "host",
        ]);
    }
    let mut out = Vec::new();
    let base = replay
        .as_ref()
        .map(|r| r[0].clone())
        .unwrap_or_else(|| synthetic(nodes));
    for case in cases {
        let mut base = if case.starts_with("jobs_") || case == "users" {
            queued(nodes, jobs.unwrap_or(0))
        } else {
            base.clone()
        };
        if case == "attention" {
            for node in &mut base.nodes {
                for gpu in &mut node.gpus {
                    gpu.util = "0".into();
                    gpu.mem_used = "0".into();
                    gpu.users.clear();
                    gpu.idle_sec = 900;
                }
            }
        }
        let mut app = App::for_user("fixture".into());
        app.benchmark_view(
            match case {
                "jobs_priority" | "jobs_wait" | "jobs_left" => 3,
                "attention" => 4,
                "users" => 5,
                "host" => 6,
                _ => 0,
            },
            if case == "search" { "fixture" } else { "" },
            if case == "free_sort" { 3 } else { 0 },
            match case {
                "jobs_wait" => job_view::Sort::Wait,
                "jobs_left" => job_view::Sort::Left,
                _ => job_view::Sort::Priority,
            },
        );
        app.apply(base.clone());
        app.collapsed.clear();
        if case == "collapsed" {
            app.collapsed = base.nodes.iter().map(|n| n.name.clone()).collect();
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 50))?;
        let mut times = Vec::new();
        let mut parses = Vec::new();
        let mut updates = Vec::new();
        let mut renders = Vec::new();
        let mut counts = (0, 0, 0, 0);
        for step in 0..repeat {
            let mut data = replay
                .as_ref()
                .map(|r| r[step % r.len()].clone())
                .unwrap_or_else(|| base.clone());
            if let Some(node) = data.nodes.first_mut() {
                if case == "stale_node" {
                    node.stale = true;
                }
                if let Some(gpu) = node.gpus.first_mut() {
                    match case {
                        "one_util" => gpu.util = (70 + step % 20).to_string(),
                        "missing_metric" => gpu.util = "N/A".into(),
                        _ => (),
                    }
                }
            }
            counts = (
                data.nodes.len(),
                data.nodes.iter().map(|n| n.gpus.len()).sum(),
                data.jobs.len(),
                data.pending.len(),
            );
            let bytes = serde_json::to_vec(&data)?;
            let parse = Instant::now();
            let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
            let data: Snapshot = serde_json::from_value(raw)?;
            parses.push(parse.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            app.apply(data);
            let render = Instant::now();
            updates.push(start.elapsed().as_secs_f64() * 1000.0);
            terminal.draw(|f| app.draw(f))?;
            renders.push(render.elapsed().as_secs_f64() * 1000.0);
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        out.push(Measurement {
            case: case.into(),
            nodes: counts.0,
            gpus: counts.1,
            jobs: counts.2,
            pending: counts.3,
            median_update_render_ms: median(&mut times),
            median_parse_ms: median(&mut parses),
            median_apply_ms: median(&mut updates),
            median_render_ms: median(&mut renders),
        });
    }
    Ok(out)
}
