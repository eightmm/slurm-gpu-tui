use crate::{
    Result,
    collector::{Collector, Config},
    history, logs,
    model::{Job, Snapshot, number},
    process::{self, Program},
    rules, runtime, scheduler, usage,
};
use chrono::{Datelike, Local, NaiveDate};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

pub fn terminal_safe(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
pub fn current_user() -> String {
    runtime::user(runtime::euid())
        .map(|u| u.name)
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| runtime::euid().to_string()))
}
fn resource(raw: &str) -> std::result::Result<f64, String> {
    number(raw)
        .filter(|n| *n >= 0.0 && (n * 1024.0).is_finite())
        .ok_or_else(|| "must be finite and non-negative in MiB".into())
}
fn positive(raw: &str) -> std::result::Result<usize, String> {
    raw.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "must be positive".into())
}

#[derive(Parser)]
#[command(
    name = "sgpu",
    about = "Oh My Slurm: push-only cluster monitoring and diagnostics. No arguments: interactive TUI.",
    disable_version_flag = true,
    args_conflicts_with_subcommands = true
)]
pub struct Args {
    #[arg(long, value_name = "USER", value_parser = highlight_user, help = "Highlight this user's jobs in the TUI; permissions stay with the executing account")]
    pub highlight_user: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}
fn highlight_user(value: &str) -> std::result::Result<String, String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err("user must be nonempty, at most 256 bytes and contain no control characters".into())
    } else {
        Ok(value.into())
    }
}
#[derive(Subcommand)]
pub enum Command {
    Version,
    Json,
    Once,
    #[command(about = "Short login overview from published data only; never queries Slurm")]
    Welcome(crate::welcome::Options),
    Doctor {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Published controller CPU, memory, storage and network snapshot")]
    Host {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Published per-user job resources; node network/load are shared")]
    Users {
        #[arg(long)]
        json: bool,
    },
    Me {
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Explain a pending job using scheduler facts and published node observations"
    )]
    Explain {
        jobid: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "My failed/stopped allocations ending in a rolling time window")]
    Failures(FailureOptions),
    #[command(about = "Pending jobs by numeric priority, with reasons and wait times")]
    Queue(QueueOptions),
    Waste {
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    Usage {
        #[arg(default_value="7",value_parser=positive)]
        days: usize,
        #[arg(long)]
        daily: bool,
        #[arg(long)]
        json: bool,
    },
    Jobs {
        #[arg(default_value="7",value_parser=positive)]
        days: usize,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long)]
        json: bool,
    },
    Report {
        month: Option<String>,
        #[arg(long)]
        json: bool,
    },
    WaitFree {
        #[arg(default_value="1",value_parser=positive)]
        count: usize,
        #[arg(long, default_value = "")]
        partition: String,
        #[arg(long,default_value="10",value_parser=positive)]
        interval: usize,
    },
    Fit {
        #[arg(default_value="1",value_parser=positive)]
        count: usize,
        #[arg(long,default_value="0",value_parser=resource)]
        vram: f64,
        #[arg(long, default_value = "")]
        partition: String,
        #[arg(long, default_value = "")]
        model: String,
        #[arg(long,value_parser=positive)]
        cpus: Option<usize>,
        #[arg(long,default_value="0",value_parser=resource)]
        ram: f64,
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        cpu_only: bool,
        #[arg(long,default_value="1",value_parser=positive)]
        nodes: usize,
        #[arg(long)]
        json: bool,
    },
    Bench {
        #[arg(long,default_value="128",value_parser=positive)]
        nodes: usize,
        #[arg(long,default_value="5",value_parser=positive)]
        repeat: usize,
        #[arg(long)]
        replay: Option<PathBuf>,
        #[arg(long, conflicts_with = "replay")]
        suite: bool,
        #[arg(long, value_parser=positive, requires = "suite")]
        jobs: Option<usize>,
    },
    Logs {
        jobid: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'e', long = "err")]
        err: bool,
    },
}

#[derive(clap::Args)]
pub struct FailureOptions {
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..=744))]
    pub hours: u32,
    #[arg(long)]
    pub json: bool,
    #[arg(
        long,
        value_enum,
        default_value = "auto",
        help = "Color text output; auto respects NO_COLOR and TERM=dumb"
    )]
    pub color: crate::terminal::ColorMode,
}

pub fn failures(options: FailureOptions) -> Result<i32> {
    let report = crate::failures::query(options.hours)?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            crate::failures::render_with_color(&report, options.color.stdout())
        );
    }
    Ok(0)
}

#[derive(clap::Args)]
pub struct QueueOptions {
    #[arg(long)]
    pub mine: bool,
    #[arg(long, default_value = "")]
    pub partition: String,
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    #[arg(long)]
    pub json: bool,
    #[arg(
        long,
        value_enum,
        default_value = "auto",
        help = "Color text output; auto respects NO_COLOR and TERM=dumb"
    )]
    pub color: crate::terminal::ColorMode,
}
pub fn queue(options: QueueOptions) -> Result<i32> {
    let data = snapshot()?;
    if !data.errors.is_empty() {
        return Err(format!("scheduler queue unavailable: {}", data.errors).into());
    }
    let user = if options.mine {
        current_user()
    } else {
        String::new()
    };
    let report = crate::queue::report(&data, &user, &options.partition, options.limit as usize);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            crate::queue::render_with_color(&report, options.color.stdout())
        );
    }
    Ok(0)
}

pub fn normalize(mut args: Vec<String>) -> Vec<String> {
    const VERBS: &[&str] = &[
        "version",
        "json",
        "once",
        "welcome",
        "doctor",
        "me",
        "waste",
        "usage",
        "jobs",
        "report",
        "wait-free",
        "fit",
        "logs",
        "host",
        "users",
        "explain",
        "failures",
        "queue",
        "bench",
    ];
    let mut skip = false;
    let mut position = None;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        let verb = if arg == "-V" {
            Some("version")
        } else if VERBS.contains(&arg.as_str()) {
            Some(arg.as_str())
        } else {
            arg.strip_prefix("--")
                .filter(|name| VERBS[..VERBS.len() - 1].contains(name))
        };
        if let Some(verb) = verb {
            let verb = verb.to_string();
            position = Some(i);
            args[i] = verb;
            break;
        }
        if [
            "--partition",
            "--model",
            "--user",
            "--interval",
            "--cpus",
            "--ram",
            "--vram",
            "--nodes",
            "--repeat",
            "--replay",
            "--highlight-user",
            "--hours",
            "--limit",
            "--color",
            "--width",
            "--scope",
            "--config",
        ]
        .contains(&arg.as_str())
        {
            skip = true;
        }
    }
    if let Some(index) = position.filter(|i| *i > 1) {
        let verb = args.remove(index);
        args.insert(1, verb);
    }
    args
}

pub fn snapshot() -> Result<Snapshot> {
    if let Ok(snapshot) = runtime::read_snapshot(Some(30.0)) {
        return Ok(snapshot);
    }
    let config = Config::default();
    let sample = scheduler::Scheduler::default().sample();
    if sample.nodes.is_empty() && !sample.error.is_empty() {
        return Err(format!("collector unavailable; {}", sample.error).into());
    }
    Ok(Collector::new(config).fold(sample, runtime::now()))
}

pub fn once(data: &Snapshot) -> String {
    let metric = |raw: &str, available| {
        if available && number(raw).is_some_and(|value| value >= 0.0) {
            raw.to_string()
        } else {
            "?".into()
        }
    };
    let mut out = format!("sgpu {} · push-only\n", data.ts);
    for node in &data.nodes {
        out.push_str(&format!(
            "\n{} {} [{}] CPU {}/{} {}\n",
            node.name,
            terminal_safe(&node.state),
            terminal_safe(&node.partition),
            node.cpu_alloc,
            node.cpus,
            if node.stale {
                format!(
                    "STALE · {}",
                    terminal_safe(if node.error.is_empty() {
                        "agent telemetry unavailable"
                    } else {
                        &node.error
                    })
                )
            } else {
                String::new()
            }
        ));
        for gpu in &node.gpus {
            out.push_str(&format!(
                "  GPU{} {:<20} util {:>4}% VRAM {}/{} MiB {} job {} health {}\n",
                gpu.index,
                terminal_safe(&gpu.name),
                metric(&gpu.util, !node.stale),
                metric(&gpu.mem_used, !node.stale),
                metric(&gpu.mem_total, true),
                terminal_safe(&if node.stale || gpu.users.is_empty() {
                    if gpu.alloc_user.is_empty() {
                        "—".into()
                    } else {
                        gpu.alloc_user.clone()
                    }
                } else {
                    gpu.users.join(",")
                }),
                if gpu.alloc_jobid.is_empty() {
                    "—".into()
                } else {
                    format!("#{}", gpu.alloc_jobid)
                },
                if node.stale {
                    "?".into()
                } else {
                    rules::health(gpu, runtime::now()).0
                }
            ));
        }
    }
    if !data.pending.is_empty() {
        out.push_str(&format!("\nPENDING ({})\n", data.pending.len()));
        for job in &data.pending {
            out.push_str(&format!(
                "  #{} {} x{} [{}] {} est.start={}\n",
                job.jobid,
                terminal_safe(&job.user),
                job.gpu_count,
                terminal_safe(&job.partition),
                terminal_safe(&rules::pending_reason(job)),
                terminal_safe(&job.start_time)
            ));
        }
    }
    if !data.errors.is_empty() {
        out.push_str(&format!("\nScheduler: {}\n", terminal_safe(&data.errors)));
    }
    if data.nodes.iter().any(|node| node.stale) {
        out.push_str("\n? = unavailable node measurements; GPU names/capacity may be cached.\nCPU allocations and job IDs come from Slurm. Run sgpu doctor for node delivery checks.\n");
    }
    out
}

pub fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-=".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub struct FitRequest<'a> {
    pub count: usize,
    pub vram: f64,
    pub partition: &'a str,
    pub model: &'a str,
    pub cpus: usize,
    pub ram: f64,
    pub explain: bool,
    pub cpu_only: bool,
    pub nodes: usize,
    pub json: bool,
}
pub fn fit(data: &Snapshot, request: FitRequest<'_>) -> (String, i32) {
    let FitRequest {
        count,
        vram,
        partition,
        model,
        cpus,
        ram,
        explain,
        cpu_only,
        nodes,
        json,
    } = request;
    let err_out = |message: &str| {
        if json {
            let payload = json!({
                "version": 1,
                "satisfied": false,
                "error": message.trim(),
                "eligible_nodes": [],
                "skipped": [],
                "sbatch": null,
            });
            (
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                ),
                1,
            )
        } else {
            (message.into(), 1)
        }
    };
    let effective_gpus = if cpu_only {
        0
    } else {
        match u32::try_from(count) {
            Ok(v) => v,
            Err(_) => return err_out("GPU request too large\n"),
        }
    };
    let Ok(cpus) = u32::try_from(cpus) else {
        return err_out("CPU request too large\n");
    };
    let effective_vram = if cpu_only { 0.0 } else { vram };
    let effective_model = if cpu_only { "" } else { model };
    let mut results = rules::fit(
        &data.nodes,
        effective_gpus,
        effective_vram,
        effective_model,
        cpus,
        ram,
        partition,
    );
    let node_by_name = |name: &str| data.nodes.iter().find(|n| n.name == name);
    let free_cpus = |name: &str| {
        node_by_name(name).and_then(|node| {
            number(&node.cpus)
                .zip(number(&node.cpu_alloc))
                .filter(|(t, a)| *t >= 0.0 && *a >= 0.0)
                .map(|(t, a)| (t - a).max(0.0))
        })
    };
    let free_ram_gib = |name: &str| {
        node_by_name(name).and_then(|node| {
            number(&node.scheduler_mem_total)
                .zip(number(&node.mem_alloc))
                .filter(|(t, a)| *t >= 0.0 && *a >= 0.0)
                .map(|(t, a)| (t - a).max(0.0) / 1024.0)
        })
    };
    results.sort_by(|a, b| {
        if cpu_only {
            let a_gpu = node_by_name(&a.node).is_some_and(|n| n.has_gpu);
            let b_gpu = node_by_name(&b.node).is_some_and(|n| n.has_gpu);
            a_gpu
                .cmp(&b_gpu)
                .then_with(|| {
                    free_cpus(&a.node)
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&free_cpus(&b.node).unwrap_or(f64::INFINITY))
                })
                .then(a.node.cmp(&b.node))
        } else {
            a.free_gpus.cmp(&b.free_gpus).then(a.node.cmp(&b.node))
        }
    });
    let hits: Vec<_> = results.iter().filter(|r| r.eligible).collect();
    let satisfied = hits.len() >= nodes;
    let sbatch = if satisfied {
        let mut part = String::new();
        let mut chosen: Vec<&rules::FitResult> = Vec::new();
        if !partition.is_empty() {
            chosen = hits.iter().copied().take(nodes).collect();
            part = partition.to_string();
        } else if nodes == 1 {
            chosen = vec![hits[0]];
            if let Some(node) = node_by_name(&hits[0].node) {
                part = node.partition.split(',').next().unwrap_or("").to_string();
            }
        } else {
            'outer: for r in &hits {
                if let Some(node) = node_by_name(&r.node) {
                    for p in node.partition.split(',').filter(|s| !s.is_empty()) {
                        let matching: Vec<_> = hits
                            .iter()
                            .copied()
                            .filter(|h| {
                                node_by_name(&h.node)
                                    .is_some_and(|dn| dn.partition.split(',').any(|np| np == p))
                            })
                            .take(nodes)
                            .collect();
                        if matching.len() >= nodes {
                            part = p.to_string();
                            chosen = matching;
                            break 'outer;
                        }
                    }
                }
            }
            if chosen.is_empty() {
                chosen = hits.iter().copied().take(nodes).collect();
                let mut parts = Vec::new();
                for r in &chosen {
                    if let Some(node) = node_by_name(&r.node) {
                        for p in node.partition.split(',').filter(|s| !s.is_empty()) {
                            if !parts.contains(&p) {
                                parts.push(p);
                            }
                        }
                    }
                }
                part = parts.join(",");
            }
        }
        let mut command = vec!["sbatch".into()];
        if !part.is_empty() {
            command.extend(["-p".into(), part]);
        }
        if nodes > 1 {
            command.extend(["-N".into(), nodes.to_string()]);
        }
        if !cpu_only {
            command.push(format!(
                "--gres=gpu:{}{count}",
                if model.is_empty() {
                    String::new()
                } else {
                    format!("{model}:")
                }
            ));
        }
        let chosen_names: Vec<_> = chosen.iter().map(|r| r.node.clone()).collect();
        command.extend(["-w".into(), chosen_names.join(",")]);
        if cpus > 0 {
            command.push(format!("--cpus-per-task={cpus}"));
        }
        if ram > 0.0 {
            command.push(format!("--mem={:.0}M", (ram * 1024.0).ceil()));
        }
        command.push("your_job.sh".into());
        Some(
            command
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        None
    };

    if json {
        let eligible_json: Vec<_> = hits
            .iter()
            .map(|row| {
                let node = node_by_name(&row.node);
                let models: Vec<_> = node
                    .map(|n| n.gpus.iter().map(|g| terminal_safe(&g.name)).collect())
                    .unwrap_or_default();
                json!({
                    "node": row.node,
                    "partition": node.map(|n| n.partition.as_str()).unwrap_or(""),
                    "has_gpu": node.is_some_and(|n| n.has_gpu),
                    "free_gpus": node.filter(|n| n.has_gpu).map(|_| row.free_gpus),
                    "free_cpus": free_cpus(&row.node),
                    "free_ram_gib": free_ram_gib(&row.node),
                    "models": models,
                })
            })
            .collect();
        let skipped_json: Vec<_> = results
            .iter()
            .filter(|r| !r.eligible)
            .map(|row| json!({"node": row.node, "reasons": row.reasons}))
            .collect();
        let payload = json!({
            "version": 1,
            "requested": {
                "gpus_per_node": effective_gpus,
                "cpu_only": cpu_only,
                "nodes": nodes,
                "vram_gib": effective_vram,
                "model": effective_model,
                "cpus_per_node": cpus,
                "ram_gib": ram,
                "partition": partition,
            },
            "satisfied": satisfied,
            "eligible_nodes": eligible_json,
            "skipped": skipped_json,
            "sbatch": sbatch,
        });
        return (
            format!(
                "{}\n",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            ),
            if satisfied { 0 } else { 1 },
        );
    }

    let mut out = String::new();
    if explain || !satisfied {
        for row in results.iter().filter(|r| !r.eligible) {
            out.push_str(&format!("skip {}: {}\n", row.node, row.reasons.join(", ")));
        }
    }
    if hits.is_empty() {
        out.push_str("no node fits the requested resources right now\n");
        return (out, 1);
    }
    out.push_str("node             partition           gpu  cpu_free  ram_free  models\n");
    for row in &hits {
        let node = node_by_name(&row.node).unwrap();
        let gpu_cell = if node.has_gpu {
            row.free_gpus.to_string()
        } else {
            "—".into()
        };
        let cpu_cell = free_cpus(&row.node)
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "?".into());
        let ram_cell = free_ram_gib(&row.node)
            .map(|v| format!("{v:.1}G"))
            .unwrap_or_else(|| "?".into());
        let models = if node.has_gpu && !node.gpus.is_empty() {
            node.gpus
                .iter()
                .map(|g| terminal_safe(&g.name))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            "—".into()
        };
        out.push_str(&format!(
            "{:<17}{:<18}{:>5}  {:>8}  {:>8}  {models}\n",
            row.node, node.partition, gpu_cell, cpu_cell, ram_cell
        ));
    }
    let Some(sbatch) = sbatch else {
        out.push_str(&format!(
            "\nonly {} of {nodes} requested nodes fit right now\n",
            hits.len()
        ));
        return (out, 1);
    };
    out.push_str(&format!(
        "\n{sbatch}\nCurrent capacity estimate; reservations, QOS and scheduler policy still decide admission.\n"
    ));
    (out, 0)
}

fn file_age_bytes(path: &std::path::Path, now: f64) -> Option<(f64, u64)> {
    let st = std::fs::symlink_metadata(path).ok()?;
    if !st.is_file() {
        return None;
    }
    let mtime = st.mtime() as f64 + st.mtime_nsec() as f64 / 1e9;
    Some(((now - mtime).max(0.0), st.len()))
}

pub fn doctor(json: bool) -> i32 {
    let now = runtime::now();
    let mut problems = 0;
    let mut lines: Vec<String> = Vec::new();
    let mut stale_details = Vec::new();
    let mut untrusted_count = 0usize;
    let data_json = match runtime::read_snapshot(Some(30.0)) {
        Ok(data) => {
            lines.push(format!(
                "data: fresh · {} nodes, {} jobs",
                data.nodes.len(),
                data.jobs.len()
            ));
            lines.push(format!(
                "scheduler jobs: {} · {} rejected records",
                data.scheduler.job_backend, data.scheduler.rejected_records
            ));
            if !data.errors.is_empty() {
                lines.push(format!("scheduler error: {}", terminal_safe(&data.errors)));
                problems += 1;
            }
            let fresh = data
                .nodes
                .iter()
                .filter(|n| !n.stale && n.source == "agent")
                .count();
            lines.push(format!(
                "node delivery: push-only · {fresh}/{} fresh; no master node requests",
                data.nodes.len()
            ));
            if fresh < data.nodes.len() {
                lines.push(format!("stale nodes: {}", data.stale_nodes.join(",")));
                for node in data
                    .nodes
                    .iter()
                    .filter(|node| node.stale || node.source != "agent")
                {
                    let reason = terminal_safe(if node.error.is_empty() {
                        "agent telemetry unavailable"
                    } else {
                        &node.error
                    });
                    lines.push(format!("  {}: {reason}", terminal_safe(&node.name)));
                    stale_details.push(json!({"node": node.name, "error": reason}));
                }
                lines.push(
                    "node setup: every node needs a local sgpu-agent service. Master installation alone does not install node services.".into(),
                );
                lines.push(
                    "publication: collector and agents need SLURM_GPU_TUI_AGENT_DIR on the same shared filesystem; matching local directory names are insufficient.".into(),
                );
                lines.push(
                    "check on each node: systemctl status sgpu-agent; journalctl -u sgpu-agent -n 30".into(),
                );
                problems += 1;
            }
            untrusted_count = data.untrusted_payloads.len();
            if untrusted_count > 0 {
                lines.push(format!(
                    "agent payload trust: {untrusted_count} rejected writers"
                ));
                problems += 1;
            } else {
                lines.push("agent payload trust: no rejected writers".into());
            }
            json!({
                "fresh": true,
                "nodes": data.nodes.len(),
                "jobs": data.jobs.len(),
                "scheduler_backend": data.scheduler.job_backend,
                "rejected_records": data.scheduler.rejected_records,
                "scheduler_error": if data.errors.is_empty() { None::<String> } else { Some(terminal_safe(&data.errors)) },
                "fresh_nodes": fresh,
                "stale_nodes": data.stale_nodes,
                "stale_details": stale_details,
                "untrusted_writers": untrusted_count,
            })
        }
        Err(error) => {
            lines.push(format!("data: {error}"));
            problems += 1;
            json!({
                "fresh": false,
                "error": terminal_safe(&error.to_string()),
                "stale_details": stale_details,
                "untrusted_writers": untrusted_count,
            })
        }
    };

    let data_dir = runtime::data_dir();
    let pid_path = data_dir.join("collector.pid");
    let collector_pid = match runtime::read_regular(&pid_path, 64, None) {
        Ok((bytes, st)) => {
            if st.mode() & 0o022 != 0 {
                problems += 1;
                lines.push(format!(
                    "collector pid: unsafe file permissions ({})",
                    pid_path.display()
                ));
                json!({"path": pid_path, "status": "unsafe_permissions"})
            } else if let Ok(pid) = String::from_utf8_lossy(&bytes).trim().parse::<u32>()
                && pid > 1
            {
                let proc_dir = PathBuf::from(format!("/proc/{pid}"));
                match std::fs::metadata(&proc_dir) {
                    Ok(_) => {
                        let verified =
                            runtime::read_regular(&proc_dir.join("cmdline"), 65536, None).map(
                                |(args, _)| {
                                    args.split(|b| *b == 0).any(|arg| {
                                        arg == b"sgpu.collector"
                                            || std::path::Path::new(
                                                &String::from_utf8_lossy(arg).into_owned(),
                                            )
                                            .file_name()
                                            .is_some_and(|n| n == "sgpu-collector")
                                    })
                                },
                            );
                        match verified {
                            Ok(false) => {
                                problems += 1;
                                lines.push(format!(
                                    "collector pid: {pid} does not identify sgpu-collector ({})",
                                    pid_path.display()
                                ));
                                json!({"path": pid_path, "pid": pid, "status": "wrong_process"})
                            }
                            _ => {
                                lines.push(format!(
                                    "collector pid: running (pid={pid}, {})",
                                    pid_path.display()
                                ));
                                json!({"path": pid_path, "pid": pid, "status": "running"})
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        problems += 1;
                        lines.push(format!(
                            "collector pid: stale PID {pid} (process not running, {})",
                            pid_path.display()
                        ));
                        json!({"path": pid_path, "pid": pid, "status": "stale"})
                    }
                    Err(e) => {
                        lines.push(format!(
                            "collector pid: {pid} (unverified: {e}, {})",
                            pid_path.display()
                        ));
                        json!({"path": pid_path, "pid": pid, "status": "unverified"})
                    }
                }
            } else {
                problems += 1;
                lines.push(format!(
                    "collector pid: invalid PID contents ({})",
                    pid_path.display()
                ));
                json!({"path": pid_path, "status": "invalid"})
            }
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            lines.push(format!(
                "collector pid: not running (no PID file at {})",
                pid_path.display()
            ));
            json!({"path": pid_path, "status": "absent"})
        }
        Err(error) => {
            problems += 1;
            lines.push(format!(
                "collector pid: unreadable ({error}, {})",
                pid_path.display()
            ));
            json!({"path": pid_path, "status": "unreadable", "error": error.to_string()})
        }
    };

    let data_file_path = data_dir.join("data.json");
    let metrics_file_path = std::env::var_os("SLURM_GPU_TUI_METRICS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("metrics.prom"));
    let data_meta = file_age_bytes(&data_file_path, now);
    let metrics_meta = file_age_bytes(&metrics_file_path, now);
    lines.push(format!(
        "published files: data.json {}, metrics.prom {}",
        data_meta
            .map(|(age, len)| format!("{age:.1}s old/{len}B"))
            .unwrap_or_else(|| "missing".into()),
        metrics_meta
            .map(|(age, len)| format!("{age:.1}s old/{len}B"))
            .unwrap_or_else(|| "not published".into())
    ));

    let agent_dir = runtime::agent_dir();
    let max_age = runtime::env_seconds("SLURM_GPU_TUI_AGENT_MAX_AGE_SEC", 45.0, 1.0);
    let agent_dir_json = match std::fs::symlink_metadata(&agent_dir) {
        Ok(st) if !st.is_dir() => {
            problems += 1;
            lines.push(format!(
                "agent dir: {} is not a directory",
                agent_dir.display()
            ));
            json!({"path": agent_dir, "exists": true, "is_dir": false})
        }
        Ok(st) => {
            let mode = st.mode();
            let unsafe_world_writable = mode & 0o002 != 0 && mode & libc::S_ISVTX == 0;
            if unsafe_world_writable {
                problems += 1;
            }
            let mount_info = crate::host::mount_fstype(&agent_dir);
            let mut fresh_payloads = 0usize;
            let mut stale_payloads = 0usize;
            if let Ok(entries) = std::fs::read_dir(&agent_dir) {
                for entry in entries.take(4096).flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with('.')
                        && name.ends_with(".json")
                        && let Some((age, _)) = file_age_bytes(&entry.path(), now)
                    {
                        if (-5.0..=max_age).contains(&age) {
                            fresh_payloads += 1;
                        } else {
                            stale_payloads += 1;
                        }
                    }
                }
            }
            let fs_note = mount_info
                .as_ref()
                .map(|(mount, fstype, shared)| {
                    format!(
                        "{fstype} on {mount} ({})",
                        if *shared { "shared FS" } else { "local FS" }
                    )
                })
                .unwrap_or_else(|| "unknown FS".into());
            lines.push(format!(
                "agent dir: {} · {fs_note} · mode {:04o}{} · {fresh_payloads} fresh/{stale_payloads} stale *.json",
                agent_dir.display(),
                mode & 0o7777,
                if unsafe_world_writable {
                    " (missing sticky bit)"
                } else {
                    ""
                }
            ));
            json!({
                "path": agent_dir,
                "exists": true,
                "is_dir": true,
                "mode": format!("{:04o}", mode & 0o7777),
                "unsafe_permissions": unsafe_world_writable,
                "mount": mount_info.as_ref().map(|(m, _, _)| m),
                "fstype": mount_info.as_ref().map(|(_, f, _)| f),
                "shared_fs": mount_info.as_ref().map(|(_, _, s)| *s),
                "fresh_payloads": fresh_payloads,
                "stale_payloads": stale_payloads,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            lines.push(format!("agent dir: {} (not present)", agent_dir.display()));
            json!({"path": agent_dir, "exists": false})
        }
        Err(e) => {
            problems += 1;
            lines.push(format!(
                "agent dir: {} (unreadable: {e})",
                agent_dir.display()
            ));
            json!({"path": agent_dir, "error": e.to_string()})
        }
    };

    let mut slurm_version = None;
    let mut sacct_version = None;
    for (program, args, label) in [
        (Program::Sinfo, vec!["--version"], "slurm"),
        (Program::Sacct, vec!["--version"], "sacct"),
    ] {
        match process::run(program, &args, Duration::from_secs(5)) {
            Ok(version) => {
                let clean = terminal_safe(&version).trim().to_string();
                lines.push(format!("{label}: {clean}"));
                if label == "slurm" {
                    slurm_version = Some(clean);
                } else {
                    sacct_version = Some(clean);
                }
            }
            Err(_) => {
                lines.push(format!("{label}: unavailable"));
                problems += 1;
            }
        }
    }
    let usage_available = usage::read().is_some();
    lines.push(format!(
        "usage history: {}",
        if usage_available {
            "available"
        } else {
            "not yet recorded"
        }
    ));
    let path = crate::notify::config_path();
    let cfg = crate::notify::Config::load(&path);
    lines.push(format!(
        "slack: {} ({})",
        if cfg.enabled() {
            "bot daily thread"
        } else {
            "not configured"
        },
        path.display()
    ));

    if json {
        let payload = json!({
            "version": 1,
            "ok": problems == 0,
            "problems": problems,
            "data": data_json,
            "collector_pid": collector_pid,
            "data_file": {
                "path": data_file_path,
                "exists": data_meta.is_some(),
                "age_seconds": data_meta.map(|(a, _)| a),
                "bytes": data_meta.map(|(_, b)| b),
            },
            "metrics_file": {
                "path": metrics_file_path,
                "exists": metrics_meta.is_some(),
                "age_seconds": metrics_meta.map(|(a, _)| a),
                "bytes": metrics_meta.map(|(_, b)| b),
            },
            "agent_dir": agent_dir_json,
            "slurm": slurm_version,
            "sacct": sacct_version,
            "usage_history": usage_available,
            "slack": {
                "configured": cfg.enabled(),
                "config_path": path,
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    if problems == 0 { 0 } else { 1 }
}

pub fn query_job(jobid: &str) -> Result<scheduler::JobRecords> {
    if !scheduler::valid_job_id(jobid) {
        return Err("invalid job ID".into());
    }
    match process::run(
        Program::Scontrol,
        &["--json", "show", "job", jobid],
        Duration::from_secs(12),
    ) {
        Ok(raw) => scheduler::parse_structured(&raw),
        Err(error) => {
            let text = error.to_string().to_lowercase();
            if (text.contains("unrecognized option") || text.contains("unknown option"))
                && text.contains("json")
                || text.contains("invalid option -- '-'")
            {
                let sample = scheduler::Scheduler::default().sample();
                if !sample.error.is_empty() {
                    return Err(sample.error.into());
                }
                Ok(sample.records)
            } else {
                Err(error)
            }
        }
    }
}

#[derive(Clone)]
pub struct LogView {
    pub path: PathBuf,
    pub owner: Option<u32>,
    pub shared: bool,
    pub merged: bool,
}
impl LogView {
    pub fn read(&self, limit: usize) -> Result<String> {
        let bytes = if let Some(owner) = self.owner {
            logs::tail_as_owner(&self.path, owner, limit)?
        } else {
            runtime::read_regular(&self.path, 1024 * 1024, None)?.0
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn mirror(job: &Job, err: bool) -> Option<LogView> {
    let merged = job.log_status.get("err").is_some_and(|s| s == "merged");
    let suffix = if err && !merged { "err" } else { "out" };
    let path = PathBuf::from(if suffix == "err" {
        &job.log_err
    } else {
        &job.log_out
    });
    if path.as_os_str().is_empty() {
        return None;
    }
    if job.jobid.len() > 200
        || !scheduler::valid_job_id(&job.jobid)
        || !runtime::state_candidates()
            .iter()
            .any(|d| path == d.join("logs").join(format!("{}.{suffix}", job.jobid)))
    {
        return None;
    }
    Some(LogView {
        path,
        owner: None,
        shared: true,
        merged,
    })
}

pub fn parse_sacct_log_line(line: &str, jobid: &str, caller_euid: u32) -> Option<(u32, PathBuf)> {
    let fields: Vec<_> = line.split('|').collect();
    if fields.len() != 3 || fields[0].trim() != jobid {
        return None;
    }
    let uid = fields[1].trim().parse::<u32>().ok()?;
    if caller_euid != 0 && uid != caller_euid {
        return None;
    }
    let workdir = PathBuf::from(fields[2].trim());
    if workdir.as_os_str().is_empty() || !workdir.is_absolute() {
        return None;
    }
    Some((uid, workdir))
}

pub fn resolve_log(jobid: &str, err: bool) -> Result<LogView> {
    if !scheduler::valid_job_id(jobid) {
        return Err("invalid job ID".into());
    }
    if let Ok(records) = query_job(jobid)
        && let Some((spec, owner)) = records.logs.get(jobid).zip(records.uids.get(jobid))
    {
        let path = if err && !spec.merged {
            &spec.err
        } else {
            &spec.out
        };
        if !path.as_os_str().is_empty() {
            let view = LogView {
                path: path.clone(),
                owner: Some(*owner),
                shared: false,
                merged: spec.merged,
            };
            if view.read(1).is_ok() {
                return Ok(view);
            }
        }
    }
    if let Ok(raw) = process::run(
        Program::Sacct,
        &[
            "-j",
            jobid,
            "-X",
            "--noheader",
            "--parsable2",
            "--format=JobIDRaw%256,UID,WorkDir%4096",
        ],
        Duration::from_secs(10),
    ) && let Some((owner, workdir)) = raw
        .lines()
        .find_map(|line| parse_sacct_log_line(line, jobid, runtime::euid()))
    {
        let candidates: Vec<(PathBuf, bool)> = if err {
            vec![
                (workdir.join(format!("slurm-{jobid}.err")), false),
                (workdir.join(format!("slurm-{jobid}.out")), true),
            ]
        } else {
            vec![(workdir.join(format!("slurm-{jobid}.out")), false)]
        };
        for (path, merged) in candidates {
            let view = LogView {
                path,
                owner: Some(owner),
                shared: false,
                merged,
            };
            if view.read(1).is_ok() {
                return Ok(view);
            }
        }
    }
    if let Ok(snapshot) = runtime::read_snapshot(Some(30.0))
        && let Some(view) = snapshot
            .jobs
            .iter()
            .find(|j| j.jobid == jobid)
            .and_then(|j| mirror(j, err))
    {
        return Ok(view);
    }
    Err("job log is unavailable; shared logs require a publishing collector".into())
}

pub fn overlap(previous: &[u8], current: &[u8]) -> usize {
    if current.is_empty() || previous.is_empty() {
        return 0;
    }
    let mut sequence: Vec<u16> = current.iter().map(|b| *b as u16).collect();
    sequence.push(256);
    sequence.extend(
        previous[previous.len().saturating_sub(current.len())..]
            .iter()
            .map(|b| *b as u16),
    );
    let mut prefix = vec![0; sequence.len()];
    for i in 1..sequence.len() {
        let mut j = prefix[i - 1];
        while j > 0 && sequence[i] != sequence[j] {
            j = prefix[j - 1];
        }
        if sequence[i] == sequence[j] {
            j += 1;
        }
        prefix[i] = j;
    }
    prefix.last().copied().unwrap_or(0).min(current.len())
}

fn follow(jobid: &str, view: LogView) -> Result<i32> {
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopped.clone())?;
    let mut previous = Vec::new();
    let mut signature = None;
    let mut pos = 0u64;
    let mut initial = true;
    let mut last_check = Instant::now();
    while !stopped.load(Ordering::Relaxed) {
        if view.shared || runtime::euid() == 0 && view.owner.is_some_and(|uid| uid != 0) {
            let read = if let Some(owner) = view.owner {
                logs::tail_as_owner(&view.path, owner, 65536).map(|bytes| (bytes, None))
            } else {
                runtime::read_regular(&view.path, 1024 * 1024, None).map(|(bytes, st)| {
                    (
                        bytes,
                        Some((st.dev(), st.ino(), st.mtime(), st.mtime_nsec())),
                    )
                })
            };
            if let Ok((current, sig)) = read
                && (initial || signature != sig || previous != current)
            {
                let offset = if initial {
                    current.len().saturating_sub(4096)
                } else {
                    overlap(&previous, &current)
                };
                std::io::stdout().write_all(&current[offset..])?;
                std::io::stdout().flush()?;
                previous = current;
                signature = sig;
                initial = false;
            }
        } else if let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&view.path)
        {
            let st = file.metadata()?;
            if st.is_file() && view.owner == Some(st.uid()) {
                let sig = (st.dev(), st.ino(), 0, 0);
                if initial {
                    pos = st.len().saturating_sub(4096);
                } else if signature != Some(sig) || st.len() < pos {
                    pos = 0;
                }
                signature = Some(sig);
                initial = false;
                if st.len() > pos {
                    file.seek(SeekFrom::Start(pos))?;
                    let mut bytes = Vec::new();
                    file.take(1024 * 1024).read_to_end(&mut bytes)?;
                    pos += bytes.len() as u64;
                    std::io::stdout().write_all(&bytes)?;
                    std::io::stdout().flush()?;
                }
            }
        }
        if last_check.elapsed() >= Duration::from_secs(30) {
            last_check = Instant::now();
            if let Ok(data) = runtime::read_snapshot(Some(30.0)) {
                if data.errors.is_empty() && !data.jobs.iter().any(|j| j.jobid == jobid) {
                    return Ok(0);
                }
            } else if let Ok(records) = query_job(jobid)
                && !records.uids.contains_key(jobid)
            {
                return Ok(0);
            }
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) && !stopped.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(0)
}

fn report(month: &str, json: bool) -> Result<i32> {
    let first = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")?;
    if month.len() != 7 {
        return Err("month must be YYYY-MM".into());
    }
    let next = if first.month() == 12 {
        NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
    }
    .ok_or("invalid report month")?;
    let end = next.pred_opt().unwrap().min(Local::now().date_naive());
    let raw = usage::read().unwrap_or_else(|| json!({}));
    let (users, days) = usage::merge(&raw, |day| {
        day >= first.to_string().as_str() && day <= end.to_string().as_str()
    });
    let history_rows =
        history::query(&format!("{first}T00:00:00"), &format!("{end}T23:59:59"), "").ok();
    if json {
        let mut sorted_users: Vec<_> = users.into_iter().collect();
        sorted_users.sort_by(|a, b| b.1.alloc.total_cmp(&a.1.alloc).then(a.0.cmp(&b.0)));
        let users_json: BTreeMap<_, _> = sorted_users.into_iter().collect();
        let days_json: BTreeMap<_, _> = days
            .into_iter()
            .map(|(day, totals)| {
                let busy_sampled =
                    raw["meta"][&day].as_f64().unwrap_or(0.0) >= 60.0 || totals.busy > 0.0;
                (
                    day,
                    json!({
                        "totals": totals,
                        "busy_sampled": busy_sampled,
                    }),
                )
            })
            .collect();
        let (outcomes, waits) = if let Some(rows) = &history_rows {
            let (states, waits) = history::summaries(rows);
            let outcomes: BTreeMap<_, _> = states
                .into_iter()
                .map(|(state, (count, seconds))| {
                    (state, json!({"jobs": count, "gpu_hours": seconds / 3600.0}))
                })
                .collect();
            let waits: BTreeMap<_, _> = waits
                .into_iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(part, values)| {
                    (
                        part,
                        json!({
                            "jobs": values.len(),
                            "median_wait_sec": values[values.len() / 2],
                            "max_wait_sec": values.last().copied().unwrap_or(0.0),
                        }),
                    )
                })
                .collect();
            (Some(outcomes), Some(waits))
        } else {
            (None, None)
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "month": month,
                "first": first.to_string(),
                "end": end.to_string(),
                "users": users_json,
                "daily": days_json,
                "outcomes": outcomes,
                "waits": waits,
            }))?
        );
        return Ok(0);
    }
    println!(
        "# GPU usage report — {month}\n\n| user | alloc | busy | eff* | waste |\n|---|---:|---:|---:|---:|"
    );
    let mut rows: Vec<_> = users.into_iter().collect();
    rows.sort_by(|a, b| b.1.alloc.total_cmp(&a.1.alloc));
    for (user, t) in rows {
        let eff = if t.observed_alloc > 0.0 {
            format!("{:.0}%", 100.0 * t.busy / t.observed_alloc)
        } else {
            "-".into()
        };
        println!(
            "| {} | {:.1}h | {:.1}h | {eff} | {:.1}h |",
            terminal_safe(&user).replace('|', "\\|"),
            t.alloc / 3600.0,
            t.busy / 3600.0,
            t.waste / 3600.0
        );
    }
    println!(
        "\n* busy/eff/waste are sampled; alloc includes sacct backfill.\n\n| day | alloc | busy |\n|---|---:|---:|"
    );
    for (day, t) in days {
        let busy = if raw["meta"][&day].as_f64().unwrap_or(0.0) >= 60.0 || t.busy > 0.0 {
            format!("{:.0}h", t.busy / 3600.0)
        } else {
            "-".into()
        };
        println!("| {day} | {:.0}h | {busy} |", t.alloc / 3600.0);
    }
    if let Some(rows) = history_rows {
        let (states, waits) = history::summaries(&rows);
        println!("\n## Job outcomes\n\n| outcome | jobs | GPU-hours |\n|---|---:|---:|");
        for (state, (count, seconds)) in states {
            println!("| {state} | {count} | {:.1} |", seconds / 3600.0);
        }
        println!("\n## Queue wait\n\n| partition | jobs | median | max |\n|---|---:|---:|---:|");
        for (part, values) in waits {
            println!(
                "| {} | {} | {} | {} |",
                terminal_safe(&part).replace('|', "\\|"),
                values.len(),
                rules::span(values[values.len() / 2] as u64),
                rules::span(*values.last().unwrap() as u64)
            );
        }
    }
    Ok(0)
}

fn text_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|s| s.width())
                .max()
                .unwrap_or(0)
                .max(header.width())
                .min(28)
        })
        .collect();
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, value)| {
                if i + 1 == headers.len() {
                    terminal_safe(value).replace(['\n', '\t'], " ")
                } else {
                    crate::quickview::cell(value, widths[i], i > 0)
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let mut out = format!(
        "{}\n",
        line(&headers.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    );
    for row in rows {
        out.push_str(&format!("{}\n", line(row)));
    }
    out
}

pub fn run(args: Args) -> Result<i32> {
    match args.command {
        None => crate::ui::run(args.highlight_user),
        Some(Command::Version) => {
            println!("sgpu {} (build {})", crate::VERSION, crate::VERSION);
            Ok(0)
        }
        Some(Command::Json) => {
            println!("{}", serde_json::to_string(&snapshot()?)?);
            Ok(0)
        }
        Some(Command::Once) => {
            print!("{}", once(&snapshot()?));
            Ok(0)
        }
        Some(Command::Doctor { json }) => Ok(doctor(json)),
        Some(Command::Welcome(options)) => crate::welcome::run(options),
        Some(Command::Failures(options)) => failures(options),
        Some(Command::Queue(options)) => queue(options),
        Some(Command::Explain { jobid, json }) => {
            if !scheduler::valid_job_id(&jobid) {
                return Err("invalid job ID".into());
            }
            let data = snapshot()?;
            let job = data
                .pending
                .iter()
                .find(|job| job.jobid == jobid)
                .ok_or("job is not in the current pending snapshot")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::explain::report(&data, job))?
                );
            } else {
                print!("{}", crate::explain::pending(&data, job));
            }
            Ok(0)
        }
        Some(Command::Host { json }) => {
            let data = runtime::read_snapshot(Some(30.0))?;
            let rows = crate::host_view::rows(
                &data.controller,
                &data.collector,
                &crate::host_view::History::default(),
                runtime::now(),
            );
            if rows.is_empty() {
                return Err(
                    "controller telemetry unavailable or stale; check the collector".into(),
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "hostname": terminal_safe(
                            data.controller["hostname"].as_str().unwrap_or("controller")
                        ),
                        "published": data.ts,
                        "controller": data.controller,
                        "collector": data.collector,
                        "rows": rows,
                    }))?
                );
                return Ok(0);
            }
            println!(
                "Host {} · published {}",
                terminal_safe(data.controller["hostname"].as_str().unwrap_or("controller")),
                terminal_safe(&data.ts)
            );
            print!(
                "{}",
                text_table(
                    &[
                        "Resource",
                        "Now / R / RX",
                        "Total / W / TX",
                        "Fill / 15m",
                        "Guide"
                    ],
                    &rows.into_iter().map(|r| r.cells).collect::<Vec<_>>()
                )
            );
            println!(
                "Guide thresholds are review hints. ? unavailable; storage freshness is tracked separately."
            );
            Ok(0)
        }
        Some(Command::Users { json }) => {
            let data = runtime::read_snapshot(Some(30.0))?;
            let mut users = crate::users::summarize(&data, runtime::now(), "", "");
            users.sort_by(|a, b| b.gpus.cmp(&a.gpus).then(a.name.cmp(&b.name)));
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "published": data.ts,
                        "users": users,
                    }))?
                );
                return Ok(0);
            }
            println!("User resources · published {}", terminal_safe(&data.ts));
            print!(
                "{}",
                text_table(
                    &[
                        "User",
                        "Run/PD",
                        "GPU",
                        "CPU cores",
                        "RAM GiB",
                        "IO R/W MiB/s",
                        "Node net MiB/s",
                        "Load1 max",
                        "Quota GiB",
                        "Files",
                        "Nodes"
                    ],
                    &users.into_iter().map(|u| u.cells()).collect::<Vec<_>>()
                )
            );
            println!(
                "Job CPU/RAM/block I/O; node network/load are SHARED. ? unavailable; ~ partial coverage."
            );
            Ok(0)
        }
        Some(Command::Waste { verbose, json }) => {
            let rows = rules::waste(
                &snapshot()?.nodes,
                runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            );
            if json {
                let mut out = Vec::with_capacity(rows.len());
                for row in &rows {
                    let mut entry = serde_json::to_value(row)?;
                    if verbose && !row.jobid.is_empty() {
                        let mut details = BTreeMap::new();
                        for id in row.jobid.split(',').filter(|id| !id.is_empty()) {
                            if let Ok(records) = query_job(id)
                                && let Some(detail) = records.details.get(id)
                            {
                                details.insert(id.to_string(), terminal_safe(detail));
                            }
                        }
                        if !details.is_empty() {
                            entry["details"] = json!(details);
                        }
                    }
                    out.push(entry);
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "waste": out,
                    }))?
                );
                return Ok(i32::from(!rows.is_empty()));
            }
            for row in &rows {
                println!(
                    "{} GPU{} {} {} {} #{}",
                    row.node,
                    row.gpu,
                    row.kind,
                    rules::span(row.sec),
                    terminal_safe(&row.user),
                    row.jobid
                );
                if verbose && !row.jobid.is_empty() {
                    for id in row.jobid.split(',').filter(|id| !id.is_empty()) {
                        if let Ok(records) = query_job(id)
                            && let Some(detail) = records.details.get(id)
                        {
                            println!("{}", terminal_safe(detail));
                        }
                    }
                }
            }
            Ok(i32::from(!rows.is_empty()))
        }
        Some(Command::Usage { days, daily, json }) => {
            let raw = usage::read().ok_or("no usage history available")?;
            if json {
                let cutoff = usage::window(days as u64);
                let (users, dates) = usage::merge(&raw, |day| day >= cutoff.as_str());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "days": days,
                        "since": cutoff,
                        "users": users,
                        "daily": dates,
                    }))?
                );
            } else {
                print!("{}", usage::render(&raw, days as u64, false));
                if daily {
                    print!("\n{}", usage::render(&raw, days as u64, true));
                }
            }
            Ok(0)
        }
        Some(Command::Jobs { days, user, json }) => {
            let rows = history::query(
                &format!("{}T00:00:00", usage::window(days as u64)),
                "",
                &user,
            )?;
            if json {
                let (states, waits) = history::summaries(&rows);
                let outcomes: BTreeMap<_, _> = states
                    .into_iter()
                    .map(|(state, (count, seconds))| {
                        (state, json!({"jobs": count, "gpu_hours": seconds / 3600.0}))
                    })
                    .collect();
                let partition_waits: BTreeMap<_, _> = waits
                    .into_iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(part, values)| {
                        (
                            part,
                            json!({
                                "jobs": values.len(),
                                "median_wait_sec": values[values.len() / 2],
                                "max_wait_sec": values.last().copied().unwrap_or(0.0),
                            }),
                        )
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "days": days,
                        "user": if user.is_empty() { None } else { Some(&user) },
                        "jobs": rows,
                        "outcomes": outcomes,
                        "waits": partition_waits,
                    }))?
                );
            } else {
                print!("{}", history::render(&rows, true));
            }
            Ok(0)
        }
        Some(Command::Report { month, json }) => report(
            &month.unwrap_or_else(|| Local::now().format("%Y-%m").to_string()),
            json,
        ),
        Some(Command::Fit {
            count,
            vram,
            partition,
            model,
            cpus,
            ram,
            explain,
            cpu_only,
            nodes,
            json,
        }) => {
            let (text, status) = fit(
                &snapshot()?,
                FitRequest {
                    count,
                    vram,
                    partition: &partition,
                    model: &model,
                    cpus: cpus.unwrap_or(0),
                    ram,
                    explain,
                    cpu_only,
                    nodes,
                    json,
                },
            );
            print!("{text}");
            Ok(status)
        }
        Some(Command::Me { json }) => {
            let user = current_user();
            let mut data = snapshot()?;
            data.jobs.retain(|j| j.user == user);
            data.pending.retain(|j| j.user == user);
            let waste: Vec<_> = rules::waste(
                &data.nodes,
                runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            )
            .into_iter()
            .filter(|w| w.user.split(',').any(|u| u == user))
            .collect();
            let week = usage::read().and_then(|raw| {
                let (users, _) = usage::merge(&raw, |day| day >= usage::window(7).as_str());
                users.get(&user).cloned()
            });
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "version": 1,
                        "user": user,
                        "published": data.ts,
                        "running": data.jobs,
                        "pending": data.pending,
                        "waste": waste,
                        "week": week,
                    }))?
                );
                return Ok(i32::from(!waste.is_empty()));
            }
            println!(
                "{} · {} running · {} pending",
                terminal_safe(&user),
                data.jobs.len(),
                data.pending.len()
            );
            for job in &data.jobs {
                println!(
                    "#{} RUNNING {} CPU · {} GPU · {} · {}",
                    job.jobid,
                    job.cpu_count,
                    job.gpu_count,
                    terminal_safe(&job.node),
                    terminal_safe(&job.jobname)
                );
            }
            for job in &data.pending {
                println!(
                    "#{} PENDING {} · {}",
                    job.jobid,
                    terminal_safe(&job.reason),
                    terminal_safe(&job.jobname)
                );
            }
            for w in &waste {
                println!("{} GPU{} {} {}", w.node, w.gpu, w.kind, rules::span(w.sec));
            }
            if let Some(t) = week {
                println!(
                    "my week: alloc {:.1}h busy {:.1}h waste {:.1}h",
                    t.alloc / 3600.0,
                    t.busy / 3600.0,
                    t.waste / 3600.0
                );
            }
            Ok(i32::from(!waste.is_empty()))
        }
        Some(Command::WaitFree {
            count,
            partition,
            interval,
        }) => {
            let stopped = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
            loop {
                let data = snapshot()?;
                let free = data
                    .nodes
                    .iter()
                    .filter(|n| {
                        partition.is_empty() || n.partition.split(',').any(|p| p == partition)
                    })
                    .flat_map(|n| rules::classes(n, runtime::now()))
                    .filter(|c| *c == rules::Class::Free)
                    .count();
                if free >= count {
                    println!("{free} free GPUs");
                    return Ok(0);
                }
                eprintln!("{free}/{count} GPUs free; waiting for node publications");
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(interval as u64) {
                    if stopped.load(Ordering::Relaxed) {
                        return Ok(0);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Some(Command::Logs {
            jobid,
            follow: watch,
            err,
        }) => {
            let view = resolve_log(&jobid, err)?;
            if err && view.merged {
                eprintln!("(stderr is merged into stdout)");
            }
            eprintln!(
                "== {}{}",
                view.path.display(),
                if view.shared {
                    " (shared collector tail)"
                } else {
                    ""
                }
            );
            if watch {
                follow(&jobid, view)
            } else {
                let text = view.read(65536)?;
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
                Ok(0)
            }
        }
        Some(Command::Bench {
            nodes,
            repeat,
            replay,
            suite,
            jobs,
        }) => match if suite {
            crate::benchmark::suite(nodes, repeat, jobs.unwrap_or(1000))
        } else {
            crate::benchmark::run(nodes, repeat, replay.as_deref())
        } {
            Ok(rows) => {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                Ok(0)
            }
            Err(error) => {
                eprintln!("benchmark: {error}");
                Ok(2)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Gpu, Node};

    #[test]
    fn highlight_user_is_display_only_and_does_not_turn_names_into_subcommands() {
        let args = Args::try_parse_from(normalize(
            ["sgpu", "--highlight-user", "once"]
                .map(str::to_string)
                .to_vec(),
        ))
        .unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.highlight_user.as_deref(), Some("once"));
        assert!(Args::try_parse_from(["sgpu", "--highlight-user", "alice", "once"]).is_err());
        for value in ["", "a\nb", "\u{1b}[31malice"] {
            assert!(highlight_user(value).is_err());
        }
    }
    #[test]
    fn aliases_move_after_options_and_conflicts_fail() {
        let normalized = normalize(
            ["sgpu", "--partition", "gpu", "--wait-free", "2"]
                .map(str::to_string)
                .to_vec(),
        );
        assert!(matches!(
            Args::try_parse_from(normalized).unwrap().command,
            Some(Command::WaitFree { count: 2, .. })
        ));
        assert!(matches!(
            Args::try_parse_from(normalize(
                ["sgpu", "--waste", "--json"].map(str::to_string).to_vec()
            ))
            .unwrap()
            .command,
            Some(Command::Waste {
                verbose: false,
                json: true
            })
        ));
        assert!(
            Args::try_parse_from(normalize(
                ["sgpu", "--json", "--once"].map(str::to_string).to_vec()
            ))
            .is_err()
        );
    }
    #[test]
    fn subcommand_flags_and_welcome_options_are_not_rewritten_as_root_aliases() {
        let parse = |args: &[&str]| {
            Args::try_parse_from(normalize(args.iter().map(|arg| arg.to_string()).collect()))
                .unwrap()
                .command
                .unwrap()
        };
        assert!(matches!(
            parse(&[
                "sgpu",
                "fit",
                "--explain",
                "--cpu-only",
                "--nodes",
                "2",
                "--json"
            ]),
            Command::Fit {
                explain: true,
                cpu_only: true,
                nodes: 2,
                json: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["sgpu", "--hours", "12", "--failures", "--json"]),
            Command::Failures(FailureOptions {
                hours: 12,
                json: true,
                ..
            })
        ));
        assert!(matches!(
            parse(&["sgpu", "--scope", "personal", "--welcome"]),
            Command::Welcome(_)
        ));
        assert!(matches!(
            parse(&["sgpu", "doctor", "--json"]),
            Command::Doctor { json: true }
        ));
    }
    #[test]
    fn resource_validation_rejects_nan_overflow_and_zero_cpu() {
        for value in ["NaN", "inf", "1e308", "-1"] {
            assert!(resource(value).is_err());
        }
        assert!(Args::try_parse_from(["sgpu", "fit", "--cpus", "0"]).is_err());
        assert!(Args::try_parse_from(["sgpu", "fit", "--nodes", "0"]).is_err());
    }
    #[test]
    fn bounded_mirror_overlap_works_with_binary_bytes() {
        assert_eq!(overlap(b"abcde", b"defgh"), 2);
        assert_eq!(overlap(&[0, 255, 1], &[255, 1, 2]), 2);
        assert_eq!(overlap(b"same", b"same"), 4);
    }
    #[test]
    fn fit_supports_cpu_only_multinode_and_json() {
        let data = Snapshot {
            nodes: vec![
                Node {
                    name: "gpu1".into(),
                    state: "idle".into(),
                    partition: "gpu".into(),
                    has_gpu: true,
                    cpus: "32".into(),
                    cpu_alloc: "0".into(),
                    scheduler_mem_total: "131072".into(),
                    mem_alloc: "0".into(),
                    gpus: vec![Gpu {
                        index: "0".into(),
                        name: "H100".into(),
                        util: "0".into(),
                        mem_total: "81920".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Node {
                    name: "gpu2".into(),
                    state: "idle".into(),
                    partition: "gpu".into(),
                    has_gpu: true,
                    cpus: "32".into(),
                    cpu_alloc: "0".into(),
                    scheduler_mem_total: "131072".into(),
                    mem_alloc: "0".into(),
                    gpus: vec![Gpu {
                        index: "0".into(),
                        name: "H100".into(),
                        util: "0".into(),
                        mem_total: "81920".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Node {
                    name: "cpu1".into(),
                    state: "idle".into(),
                    partition: "cpu".into(),
                    has_gpu: false,
                    cpus: "64".into(),
                    cpu_alloc: "8".into(),
                    scheduler_mem_total: "262144".into(),
                    mem_alloc: "32768".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (text, status) = fit(
            &data,
            FitRequest {
                count: 1,
                vram: 0.0,
                partition: "",
                model: "",
                cpus: 16,
                ram: 64.0,
                explain: false,
                cpu_only: true,
                nodes: 1,
                json: false,
            },
        );
        assert_eq!(status, 0);
        assert!(text.contains("cpu1"));
        assert!(!text.contains("--gres=gpu"));

        let (text, status) = fit(
            &data,
            FitRequest {
                count: 1,
                vram: 0.0,
                partition: "",
                model: "",
                cpus: 4,
                ram: 0.0,
                explain: false,
                cpu_only: false,
                nodes: 2,
                json: true,
            },
        );
        assert_eq!(status, 0);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["satisfied"], true);
        let sbatch = parsed["sbatch"].as_str().unwrap();
        assert!(sbatch.contains("-N 2"));
        assert!(sbatch.contains("-w 'gpu1,gpu2'") || sbatch.contains("-w gpu1,gpu2"));
        assert!(sbatch.contains("--gres=gpu:1"));

        let (text, status) = fit(
            &data,
            FitRequest {
                count: 1,
                vram: 0.0,
                partition: "",
                model: "",
                cpus: 4,
                ram: 0.0,
                explain: false,
                cpu_only: false,
                nodes: 3,
                json: true,
            },
        );
        assert_eq!(status, 1);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["satisfied"], false);
        assert!(parsed["sbatch"].is_null());
    }
    #[test]
    fn parse_sacct_log_line_validates_jobid_uid_and_absolute_workdir() {
        assert_eq!(
            parse_sacct_log_line("42|1000|/home/alice/work", "42", 1000),
            Some((1000, PathBuf::from("/home/alice/work")))
        );
        assert_eq!(
            parse_sacct_log_line("42|1000|/home/alice/work", "42", 0),
            Some((1000, PathBuf::from("/home/alice/work")))
        );
        assert_eq!(
            parse_sacct_log_line("42|1001|/home/bob/work", "42", 1000),
            None
        );
        assert_eq!(
            parse_sacct_log_line("42.batch|1000|/home/alice/work", "42", 1000),
            None
        );
        assert_eq!(
            parse_sacct_log_line("42|1000|relative/dir", "42", 1000),
            None
        );
        assert_eq!(
            parse_sacct_log_line(
                "42|alice|name|gpu|gres|a|b|c|COMPLETED|0:0|1|/dir",
                "42",
                1000
            ),
            None
        );
    }
}
