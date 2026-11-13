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
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

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
    about = "Push-only Slurm GPU operations monitor. No arguments: interactive TUI.",
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
    Doctor,
    Me,
    Waste {
        #[arg(short, long)]
        verbose: bool,
    },
    Usage {
        #[arg(default_value="7",value_parser=positive)]
        days: usize,
        #[arg(long)]
        daily: bool,
    },
    Jobs {
        #[arg(default_value="7",value_parser=positive)]
        days: usize,
        #[arg(long, default_value = "")]
        user: String,
    },
    Report {
        month: Option<String>,
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
    },
    Bench {
        #[arg(long,default_value="128",value_parser=positive)]
        nodes: usize,
        #[arg(long,default_value="5",value_parser=positive)]
        repeat: usize,
        #[arg(long)]
        replay: Option<PathBuf>,
    },
    Logs {
        jobid: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'e', long = "err")]
        err: bool,
    },
}

pub fn normalize(mut args: Vec<String>) -> Vec<String> {
    const VERBS: &[&str] = &[
        "version",
        "json",
        "once",
        "doctor",
        "me",
        "waste",
        "usage",
        "jobs",
        "report",
        "wait-free",
        "fit",
        "logs",
        "bench",
    ];
    for arg in args.iter_mut().skip(1) {
        if arg == "-V" {
            *arg = "version".into();
        } else if arg.starts_with("--") && VERBS[..VERBS.len() - 1].contains(&&arg[2..]) {
            *arg = arg[2..].into();
        }
    }
    let mut skip = false;
    let mut position = None;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if VERBS.contains(&arg.as_str()) {
            position = Some(i);
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
    let mut out = format!("sgpu {} · push-only\n", data.ts);
    for node in &data.nodes {
        out.push_str(&format!(
            "\n{} {} [{}] CPU {}/{} {}\n",
            node.name,
            terminal_safe(&node.state),
            terminal_safe(&node.partition),
            node.cpu_alloc,
            node.cpus,
            if node.stale { "STALE" } else { "" }
        ));
        for gpu in &node.gpus {
            out.push_str(&format!(
                "  GPU{} {:<20} util {:>4}% VRAM {}/{} MiB {} #{} {}\n",
                gpu.index,
                terminal_safe(&gpu.name),
                gpu.util,
                gpu.mem_used,
                gpu.mem_total,
                terminal_safe(&if gpu.users.is_empty() {
                    gpu.alloc_user.clone()
                } else {
                    gpu.users.join(",")
                }),
                gpu.alloc_jobid,
                rules::health(gpu, runtime::now()).0
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
    } = request;
    let Ok(count32) = u32::try_from(count) else {
        return ("GPU request too large\n".into(), 1);
    };
    let Ok(cpus) = u32::try_from(cpus) else {
        return ("CPU request too large\n".into(), 1);
    };
    let mut results = rules::fit(&data.nodes, count32, vram, model, cpus, ram, partition);
    results.sort_by(|a, b| a.free_gpus.cmp(&b.free_gpus).then(a.node.cmp(&b.node)));
    let mut out = String::new();
    if explain || !results.iter().any(|r| r.eligible) {
        for row in results.iter().filter(|r| !r.eligible) {
            out.push_str(&format!("skip {}: {}\n", row.node, row.reasons.join(", ")));
        }
    }
    let hits: Vec<_> = results.iter().filter(|r| r.eligible).collect();
    if hits.is_empty() {
        out.push_str("no node fits the requested resources right now\n");
        return (out, 1);
    }
    out.push_str("node             partition          free   models (VRAM)\n");
    for row in &hits {
        let node = data.nodes.iter().find(|n| n.name == row.node).unwrap();
        let models = node
            .gpus
            .iter()
            .map(|g| terminal_safe(&g.name))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{:<17}{:<19}{:>4}   {models}\n",
            row.node, node.partition, row.free_gpus
        ));
    }
    let node = data.nodes.iter().find(|n| n.name == hits[0].node).unwrap();
    let part = if partition.is_empty() {
        node.partition.split(',').next().unwrap_or("")
    } else {
        partition
    };
    let mut command = vec!["sbatch".into()];
    if !part.is_empty() {
        command.extend(["-p".into(), part.into()]);
    }
    command.push(format!(
        "--gres=gpu:{}{count}",
        if model.is_empty() {
            String::new()
        } else {
            format!("{model}:")
        }
    ));
    command.extend(["-w".into(), node.name.clone()]);
    if cpus > 0 {
        command.push(format!("--cpus-per-task={cpus}"));
    }
    if ram > 0.0 {
        command.push(format!("--mem={:.0}M", (ram * 1024.0).ceil()));
    }
    command.push("your_job.sh".into());
    out.push_str(&format!("\n{}\nCurrent capacity estimate; reservations, QOS and scheduler policy still decide admission.\n",command.iter().map(|s|quote(s)).collect::<Vec<_>>().join(" ")));
    (out, 0)
}

pub fn doctor() -> i32 {
    let mut problems = 0;
    match runtime::read_snapshot(Some(30.0)) {
        Ok(data) => {
            println!(
                "data: fresh · {} nodes, {} jobs",
                data.nodes.len(),
                data.jobs.len()
            );
            println!(
                "scheduler jobs: {} · {} rejected records",
                data.scheduler.job_backend, data.scheduler.rejected_records
            );
            if !data.errors.is_empty() {
                println!("scheduler error: {}", terminal_safe(&data.errors));
                problems += 1;
            }
            let fresh = data
                .nodes
                .iter()
                .filter(|n| !n.stale && n.source == "agent")
                .count();
            println!(
                "node delivery: push-only · {fresh}/{} fresh; no master node requests",
                data.nodes.len()
            );
            if fresh < data.nodes.len() {
                println!("stale nodes: {}", data.stale_nodes.join(","));
                problems += 1;
            }
            if !data.untrusted_payloads.is_empty() {
                println!(
                    "agent payload trust: {} rejected writers",
                    data.untrusted_payloads.len()
                );
                problems += 1;
            } else {
                println!("agent payload trust: no rejected writers");
            }
        }
        Err(error) => {
            println!("data: {error}");
            problems += 1;
        }
    }
    for (program, args, label) in [
        (Program::Sinfo, vec!["--version"], "slurm"),
        (Program::Sacct, vec!["--version"], "sacct"),
    ] {
        match process::run(program, &args, Duration::from_secs(5)) {
            Ok(version) => println!("{label}: {}", terminal_safe(&version)),
            Err(_) => {
                println!("{label}: unavailable");
                problems += 1;
            }
        }
    }
    println!(
        "usage history: {}",
        if usage::read().is_some() {
            "available"
        } else {
            "not yet recorded"
        }
    );
    let path = crate::notify::config_path();
    let cfg = crate::notify::Config::load(&path);
    println!(
        "slack: {} ({})",
        if cfg.enabled() {
            "bot daily thread"
        } else {
            "not configured"
        },
        path.display()
    );
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

fn report(month: &str) -> Result<i32> {
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
    if let Ok(rows) = history::query(&format!("{first}T00:00:00"), &format!("{end}T23:59:59"), "") {
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
        Some(Command::Doctor) => Ok(doctor()),
        Some(Command::Waste { verbose }) => {
            let rows = rules::waste(
                &snapshot()?.nodes,
                runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            );
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
                if verbose
                    && !row.jobid.is_empty()
                    && let Ok(records) = query_job(&row.jobid)
                    && let Some(detail) = records.details.get(&row.jobid)
                {
                    println!("{}", terminal_safe(detail));
                }
            }
            Ok(i32::from(!rows.is_empty()))
        }
        Some(Command::Usage { days, daily }) => {
            let raw = usage::read().ok_or("no usage history available")?;
            print!("{}", usage::render(&raw, days as u64, false));
            if daily {
                print!("\n{}", usage::render(&raw, days as u64, true));
            }
            Ok(0)
        }
        Some(Command::Jobs { days, user }) => {
            let rows = history::query(
                &format!("{}T00:00:00", usage::window(days as u64)),
                "",
                &user,
            )?;
            print!("{}", history::render(&rows, true));
            Ok(0)
        }
        Some(Command::Report { month }) => {
            report(&month.unwrap_or_else(|| Local::now().format("%Y-%m").to_string()))
        }
        Some(Command::Fit {
            count,
            vram,
            partition,
            model,
            cpus,
            ram,
            explain,
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
                },
            );
            print!("{text}");
            Ok(status)
        }
        Some(Command::Me) => {
            let user = current_user();
            let mut data = snapshot()?;
            data.jobs.retain(|j| j.user == user);
            data.pending.retain(|j| j.user == user);
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
            let waste: Vec<_> = rules::waste(
                &data.nodes,
                runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64,
            )
            .into_iter()
            .filter(|w| w.user.split(',').any(|u| u == user))
            .collect();
            for w in &waste {
                println!("{} GPU{} {} {}", w.node, w.gpu, w.kind, rules::span(w.sec));
            }
            if let Some(raw) = usage::read() {
                let (users, _) = usage::merge(&raw, |day| day >= usage::window(7).as_str());
                if let Some(t) = users.get(&user) {
                    println!(
                        "my week: alloc {:.1}h busy {:.1}h waste {:.1}h",
                        t.alloc / 3600.0,
                        t.busy / 3600.0,
                        t.waste / 3600.0
                    );
                }
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
        }) => match crate::benchmark::run(nodes, repeat, replay.as_deref()) {
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
        assert!(
            Args::try_parse_from(normalize(
                ["sgpu", "--waste", "--json"].map(str::to_string).to_vec()
            ))
            .is_err()
        );
    }
    #[test]
    fn resource_validation_rejects_nan_overflow_and_zero_cpu() {
        for value in ["NaN", "inf", "1e308", "-1"] {
            assert!(resource(value).is_err());
        }
        assert!(Args::try_parse_from(["sgpu", "fit", "--cpus", "0"]).is_err());
    }
    #[test]
    fn bounded_mirror_overlap_works_with_binary_bytes() {
        assert_eq!(overlap(b"abcde", b"defgh"), 2);
        assert_eq!(overlap(&[0, 255, 1], &[255, 1, 2]), 2);
        assert_eq!(overlap(b"same", b"same"), 4);
    }
}
