use crate::{
    Result,
    model::{Gpu, Job, Node, Snapshot},
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
    median_update_render_ms: f64,
}
pub fn run(nodes: usize, repeat: usize, replay: Option<&Path>) -> Result<Vec<Measurement>> {
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
    let cases = if replay.is_some() {
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
    let mut out = Vec::new();
    let base = replay
        .as_ref()
        .map(|r| r[0].clone())
        .unwrap_or_else(|| synthetic(nodes));
    for case in cases {
        let mut app = App::for_user("fixture".into());
        app.apply(base.clone());
        app.collapsed.clear();
        if case == "collapsed" {
            app.collapsed = base.nodes.iter().map(|n| n.name.clone()).collect();
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 50))?;
        let mut times = Vec::new();
        let mut last = base.clone();
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
            let start = Instant::now();
            app.apply(data.clone());
            terminal.draw(|f| app.draw(f))?;
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            last = data;
        }
        times.sort_by(f64::total_cmp);
        out.push(Measurement {
            case: case.into(),
            nodes: last.nodes.len(),
            gpus: last.nodes.iter().map(|n| n.gpus.len()).sum(),
            median_update_render_ms: times[times.len() / 2],
        });
    }
    Ok(out)
}
