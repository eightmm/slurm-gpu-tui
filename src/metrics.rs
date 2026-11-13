use crate::{
    model::{Snapshot, json_number, number},
    rules::{self, Class},
    runtime, scheduler,
};
use std::{collections::BTreeMap, ffi::CString, fmt::Write, fs, path::Path};

pub fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "")
}

#[derive(Default)]
struct Encoder {
    points: BTreeMap<String, String>,
    types: BTreeMap<String, &'static str>,
}
impl Encoder {
    fn emit(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        if !value.is_finite() {
            return;
        }
        self.types.entry(name.into()).or_insert(
            if name.ends_with("_total") && name.starts_with("sgpu_command_") {
                "counter"
            } else {
                "gauge"
            },
        );
        let labels = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
            .collect::<Vec<_>>()
            .join(",");
        let key = if labels.is_empty() {
            name.into()
        } else {
            format!("{name}{{{labels}}}")
        };
        self.points.insert(key, value.to_string());
    }
    fn optional(&mut self, name: &str, labels: &[(&str, &str)], value: &str) {
        if let Some(value) = number(value) {
            self.emit(name, labels, value);
        }
    }
    fn finish(self) -> String {
        let mut out = String::new();
        for (name, kind) in self.types {
            let _ = writeln!(out, "# TYPE {name} {kind}");
        }
        for (key, value) in self.points {
            let _ = writeln!(out, "{key} {value}");
        }
        out
    }
}

pub fn format(snapshot: &Snapshot, now: f64) -> String {
    let mut out = Encoder::default();
    out.emit("sgpu_collector_last_success_timestamp_seconds", &[], now);
    out.emit(
        "sgpu_build_info",
        &[("version", &snapshot.release), ("build", &snapshot.build)],
        1.0,
    );
    for (name, value) in [
        ("sgpu_jobs_running", snapshot.jobs.len()),
        ("sgpu_jobs_pending", snapshot.pending.len()),
        ("sgpu_nodes_total", snapshot.nodes.len()),
        (
            "sgpu_nodes_up",
            snapshot.nodes.iter().filter(|n| n.error.is_empty()).count(),
        ),
        (
            "sgpu_nodes_stale",
            snapshot.nodes.iter().filter(|n| n.stale).count(),
        ),
    ] {
        out.emit(name, &[], value as f64);
    }
    let mut total = 0;
    let mut allocated = 0;
    let mut free = 0;
    let mut idle = 0;
    let mut parked = 0;
    let mut rogue = 0;
    for node in &snapshot.nodes {
        let labels = [("node", node.name.as_str())];
        out.emit(
            "sgpu_node_info",
            &[
                ("node", &node.name),
                ("partition", &node.partition),
                ("source", &node.source),
            ],
            1.0,
        );
        out.emit("sgpu_node_up", &labels, f64::from(node.error.is_empty()));
        out.emit("sgpu_node_stale", &labels, f64::from(node.stale));
        for (metric, value) in [
            ("cpus_total", &node.cpus),
            ("cpus_alloc", &node.cpu_alloc),
            ("cpu_load", &node.cpu_load),
            ("mem_total_mib", &node.mem_total),
            ("mem_used_mib", &node.mem_used),
            ("mem_alloc_mib", &node.mem_alloc),
            ("mem_avail_mib", &node.mem_avail),
            ("cpu_power_watts", &node.cpu_power),
            ("ram_power_watts", &node.ram_power),
            ("sys_power_watts", &node.sys_power),
        ] {
            if node.stale
                && matches!(
                    metric,
                    "mem_used_mib"
                        | "mem_avail_mib"
                        | "cpu_power_watts"
                        | "ram_power_watts"
                        | "sys_power_watts"
                )
            {
                continue;
            }
            if !node.scheduler_available
                && matches!(metric, "cpus_alloc" | "mem_alloc_mib" | "cpu_load")
            {
                continue;
            }
            out.optional(&format!("sgpu_node_{metric}"), &labels, value);
        }
        out.emit("sgpu_node_data_age_seconds", &labels, node.data_age_sec);
        out.emit(
            "sgpu_node_scheduler_age_seconds",
            &labels,
            node.scheduler_age_sec,
        );
        if !node.stale && rules::telemetry_fresh(node.telemetry_observed_at, now) {
            out.emit(
                "sgpu_node_telemetry_timestamp_seconds",
                &labels,
                node.telemetry_observed_at,
            );
            out.optional("sgpu_node_cpu_util_percent", &labels, &node.cpu_util);
            for (resource, value) in &node.pressure {
                if matches!(resource.as_str(), "cpu" | "memory" | "io") {
                    out.optional(
                        "sgpu_node_pressure_percent",
                        &[("node", &node.name), ("resource", resource)],
                        value,
                    );
                }
            }
        }
        let classes = rules::classes(node, now);
        for (gpu, class) in node.gpus.iter().zip(classes) {
            total += 1;
            if !gpu.alloc_jobid.is_empty() || !gpu.alloc_user.is_empty() {
                allocated += 1;
            }
            if class == Class::Free {
                free += 1;
            }
            if !node.stale && gpu.idle_sec > 0 {
                idle += 1;
            }
            if !node.stale && gpu.parked_sec > 0 {
                parked += 1;
            }
            if class == Class::Rogue {
                rogue += 1;
            }
            let labels = [("node", node.name.as_str()), ("gpu", gpu.index.as_str())];
            out.emit(
                "sgpu_gpu_info",
                &[
                    ("node", &node.name),
                    ("gpu", &gpu.index),
                    ("name", &gpu.name),
                    ("uuid", &gpu.uuid),
                ],
                1.0,
            );
            for (metric, value) in [
                ("util", &gpu.util),
                ("mem_used_mib", &gpu.mem_used),
                ("mem_total_mib", &gpu.mem_total),
                ("temp_celsius", &gpu.temp),
                ("power_watts", &gpu.power),
                ("ecc_errors", &gpu.ecc),
                ("sm_clock_mhz", &gpu.sm_clock),
                ("mem_clock_mhz", &gpu.mem_clock),
            ] {
                if node.stale && metric != "mem_total_mib" {
                    continue;
                }
                out.optional(&format!("sgpu_gpu_{metric}"), &labels, value);
            }
            if !node.stale
                && let Some((used, total)) = number(&gpu.mem_used)
                    .zip(number(&gpu.mem_total))
                    .filter(|(_, t)| *t > 0.0)
            {
                out.emit("sgpu_gpu_mem_used_percent", &labels, used / total * 100.0);
            }
            out.emit(
                "sgpu_gpu_allocated",
                &[
                    ("node", &node.name),
                    ("gpu", &gpu.index),
                    ("user", &gpu.alloc_user),
                ],
                f64::from(!gpu.alloc_jobid.is_empty() || !gpu.alloc_user.is_empty()),
            );
            if !gpu.alloc_jobid.is_empty() {
                let job = snapshot.jobs.iter().find(|j| j.jobid == gpu.alloc_jobid);
                out.emit(
                    "sgpu_gpu_job_info",
                    &[
                        ("node", &node.name),
                        ("gpu", &gpu.index),
                        ("user", &gpu.alloc_user),
                        ("jobid", &gpu.alloc_jobid),
                        ("jobname", job.map(|j| j.jobname.as_str()).unwrap_or("")),
                    ],
                    1.0,
                );
            }
            if !node.stale {
                out.emit("sgpu_gpu_idle_seconds", &labels, gpu.idle_sec as f64);
                out.emit("sgpu_gpu_parked_seconds", &labels, gpu.parked_sec as f64);
            }
            let (health, severity) = rules::health(gpu, now);
            if !node.stale && health != "—" {
                out.emit("sgpu_gpu_health_severity", &labels, severity as f64);
            }
        }
    }
    for (name, value) in [
        ("total", total),
        ("allocated", allocated),
        ("free", free),
        ("idle", idle),
        ("parked", parked),
        ("rogue", rogue),
    ] {
        out.emit(&format!("sgpu_gpus_{name}"), &[], value as f64);
    }
    let max = runtime::env_seconds("SLURM_GPU_TUI_METRICS_PENDING_MAX", 100.0, 0.0) as usize;
    for job in snapshot.pending.iter().take(max) {
        out.emit(
            "sgpu_pending_job_info",
            &[
                ("jobid", &job.jobid),
                ("user", &job.user),
                ("partition", &job.partition),
                ("jobname", &job.jobname),
                ("reason", &job.reason),
                ("gpus", &job.gpu_count.to_string()),
            ],
            1.0,
        );
        if let Some(wait) = rules::pending_wait(job, now) {
            out.emit("sgpu_pending_wait_seconds", &[("jobid", &job.jobid)], wait);
        }
    }
    for job in &snapshot.jobs {
        let memory = scheduler::mem_mib(&job.mem, job.cpu_count.max(1));
        if let Some(node) = snapshot.nodes.iter().find(|n| n.name == job.node)
            && job.gpu_count > 0
            && !node.gpus.is_empty()
            && let Some((mem, total)) = memory
                .zip(number(&node.mem_total))
                .filter(|(m, t)| *m > 0.0 && *t > 0.0)
        {
            let labels = [
                ("jobid", job.jobid.as_str()),
                ("user", job.user.as_str()),
                ("node", node.name.as_str()),
                ("gpus", &job.gpu_count.to_string()),
            ];
            out.emit("sgpu_job_mem_mib", &labels, mem);
            out.emit(
                "sgpu_job_mem_fair_ratio",
                &labels,
                mem / (total * job.gpu_count as f64 / node.gpus.len() as f64),
            );
        }
        let expected = scheduler::expand_nodes(&job.node).unwrap_or_default();
        for (node, sample) in &job.telemetry {
            if !expected.contains(node)
                || !snapshot.nodes.iter().any(|n| {
                    n.name == *node
                        && !n.stale
                        && n.error.is_empty()
                        && n.scheduler_available
                        && n.scheduler_age_sec <= 30.0
                })
                || !json_number(&sample["observed_at"])
                    .is_some_and(|s| rules::telemetry_fresh(s, now))
            {
                continue;
            }
            let labels = [
                ("jobid", job.jobid.as_str()),
                ("user", job.user.as_str()),
                ("node", node.as_str()),
            ];
            for (key, metric) in [
                ("cpu_cores", "cpu_cores"),
                ("mem_current_mib", "mem_current_mib"),
                ("mem_peak_mib", "mem_peak_mib"),
                ("mem_limit_mib", "mem_limit_mib"),
                ("oom_kill", "oom_kills"),
                ("vram_mib", "pid_vram_mib"),
                ("observed_at", "telemetry_timestamp_seconds"),
            ] {
                if let Some(v) = rules::sample_metric(sample, key) {
                    out.emit(&format!("sgpu_job_{metric}"), &labels, v);
                }
            }
        }
    }
    if let Some(phases) = snapshot.collector["phase_seconds"].as_object() {
        for (phase, value) in phases {
            if matches!(
                phase.as_str(),
                "collect" | "write" | "idle" | "usage" | "metrics" | "notify"
            ) && let Some(value) = json_number(value)
            {
                out.emit("sgpu_collector_phase_seconds", &[("phase", phase)], value);
            }
        }
    }
    for field in [
        "cycle_seconds",
        "interval_seconds",
        "log_inflight",
        "notify_backlog",
    ] {
        if let Some(value) = json_number(&snapshot.collector[field]) {
            out.emit(&format!("sgpu_collector_{field}"), &[], value);
        }
    }
    for (command, stats) in crate::process::stats() {
        if matches!(
            command.as_str(),
            "sinfo" | "squeue" | "scontrol_jobs" | "scontrol_nodes" | "sacct"
        ) {
            let label = [("command", command.as_str())];
            out.emit("sgpu_command_calls_total", &label, stats.calls as f64);
            out.emit("sgpu_command_failures_total", &label, stats.failures as f64);
            out.emit("sgpu_command_seconds_total", &label, stats.seconds);
            out.emit("sgpu_command_last_seconds", &label, stats.last_seconds);
        }
    }
    out.finish()
}

pub fn master_host(proc: &Path, sys: &Path) -> String {
    let mut out = Encoder::default();
    let read = |root: &Path, name: &str| {
        runtime::read_regular(&root.join(name), 8 * 1024 * 1024, None)
            .ok()
            .and_then(|(bytes, _)| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    };
    for line in read(proc, "stat").lines() {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() > 4
            && p[0]
                .strip_prefix("cpu")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            if let Some(value) = number(p[4]) {
                out.emit(
                    "sgpu_master_cpu_seconds_total",
                    &[("cpu", &p[0][3..]), ("mode", "idle")],
                    value / 100.0,
                );
            }
        } else if p.first() == Some(&"btime") && p.len() > 1 {
            out.optional("sgpu_master_boot_time_seconds", &[], p[1]);
        }
    }
    for line in read(proc, "meminfo").lines() {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() > 1 {
            let key = p[0].trim_end_matches(':');
            if matches!(key, "MemTotal" | "MemAvailable")
                && let Some(n) = number(p[1])
            {
                out.emit(&format!("sgpu_master_memory_{key}_bytes"), &[], n * 1024.0);
            }
        }
    }
    if let Some(load) = read(proc, "loadavg").split_whitespace().next() {
        out.optional("sgpu_master_load1", &[], load);
    }
    for line in read(proc, "net/dev").lines() {
        if let Some((device, rest)) = line.split_once(':') {
            let device = device.trim();
            let p: Vec<_> = rest.split_whitespace().collect();
            if device != "lo" && p.len() > 8 {
                out.optional(
                    "sgpu_master_network_receive_bytes_total",
                    &[("device", device)],
                    p[0],
                );
                out.optional(
                    "sgpu_master_network_transmit_bytes_total",
                    &[("device", device)],
                    p[8],
                );
            }
        }
    }
    for line in read(proc, "diskstats").lines() {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() > 9
            && ["sd", "hd", "vd", "xvd", "nvme", "mmcblk"]
                .iter()
                .any(|prefix| p[2].starts_with(prefix))
        {
            for (metric, index) in [("read", 5), ("written", 9)] {
                if let Some(n) = number(p[index]) {
                    out.emit(
                        &format!("sgpu_master_disk_{metric}_bytes_total"),
                        &[("device", p[2])],
                        n * 512.0,
                    );
                }
            }
        }
    }
    for line in read(proc, "mounts").lines().take(4096) {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() < 3
            || !matches!(
                p[2],
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "tmpfs" | "overlay"
            )
        {
            continue;
        }
        let Ok(path) = CString::new(p[1]) else {
            continue;
        };
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } == 0 {
            let st = unsafe { st.assume_init() };
            out.emit(
                "sgpu_master_filesystem_size_bytes",
                &[("mountpoint", p[1])],
                st.f_blocks as f64 * st.f_frsize as f64,
            );
            out.emit(
                "sgpu_master_filesystem_avail_bytes",
                &[("mountpoint", p[1])],
                st.f_bavail as f64 * st.f_frsize as f64,
            );
        }
    }
    if let Ok(entries) = fs::read_dir(sys.join("class/hwmon")) {
        let mut entries: Vec<_> = entries.take(256).flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for (index, entry) in entries.into_iter().enumerate() {
            let name = read(&entry.path(), "name").trim().to_string();
            let chip = if name == "coretemp" {
                fs::canonicalize(entry.path().join("device"))
                    .ok()
                    .and_then(|p| {
                        p.file_name()
                            .map(|s| format!("platform_{}", s.to_string_lossy()))
                    })
                    .unwrap_or_else(|| format!("platform_coretemp.{index}"))
            } else {
                name.clone()
            };
            if let Ok(sensors) = fs::read_dir(entry.path()) {
                for sensor in sensors.take(256).flatten() {
                    let file = sensor.file_name().to_string_lossy().to_string();
                    if name == "coretemp"
                        && file.starts_with("temp")
                        && file.ends_with("_input")
                        && let Some(n) = number(read(&entry.path(), &file).trim())
                    {
                        out.emit(
                            "sgpu_master_hwmon_temp_celsius",
                            &[("chip", &chip), ("sensor", file.trim_end_matches("_input"))],
                            n / 1000.0,
                        );
                    }
                    if file.starts_with("power")
                        && file.ends_with("_average")
                        && let Some(n) = number(read(&entry.path(), &file).trim())
                    {
                        out.emit(
                            "sgpu_master_hwmon_power_average_watt",
                            &[
                                ("chip", &name),
                                ("sensor", file.trim_end_matches("_average")),
                            ],
                            n / 1e6,
                        );
                    }
                }
            }
        }
    }
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Gpu, Node, PendingJob};
    #[test]
    fn metrics_keep_unknowns_absent_and_cap_pending_labels_without_losing_count() {
        let snapshot = Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    util: "NaN".into(),
                    temp: "60".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            pending: (0..150)
                .map(|i| PendingJob {
                    jobid: i.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let text = format(&snapshot, 100.0);
        assert!(text.contains("sgpu_jobs_pending 150\n"));
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("sgpu_pending_job_info{"))
                .count(),
            100
        );
        assert!(!text.lines().any(|l| l.starts_with("sgpu_gpu_util{")));
        assert!(text.contains("sgpu_gpu_temp_celsius{node=\"gpu1\",gpu=\"0\"} 60"));
    }
    #[test]
    fn labels_escape_control_syntax_and_host_metrics_have_no_duplicate_points() {
        assert_eq!(escape("a\"b\\c\nx"), "a\\\"b\\\\c\\nx");
        let text = master_host(Path::new("/proc"), Path::new("/sys"));
        let names: Vec<_> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        let unique: std::collections::BTreeSet<_> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len());
    }
    #[test]
    fn stale_observations_keep_inventory_but_do_not_export_live_measurements() {
        let data = Snapshot {
            nodes: vec![Node {
                name: "gpu1".into(),
                stale: true,
                mem_total: "65536".into(),
                mem_used: "50000".into(),
                sys_power: "700".into(),
                gpus: vec![Gpu {
                    index: "0".into(),
                    util: "95".into(),
                    mem_total: "80000".into(),
                    mem_used: "70000".into(),
                    power: "300".into(),
                    temp: "90".into(),
                    idle_sec: 900,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = format(&data, 100.0);
        for name in [
            "sgpu_gpu_util{",
            "sgpu_gpu_mem_used_mib{",
            "sgpu_gpu_temp_celsius{",
            "sgpu_gpu_power_watts{",
            "sgpu_gpu_idle_seconds{",
            "sgpu_node_mem_used_mib{",
            "sgpu_node_sys_power_watts{",
        ] {
            assert!(!text.contains(name), "stale series {name}");
        }
        assert!(text.contains("sgpu_gpu_mem_total_mib{"));
        assert!(text.contains("sgpu_gpus_free 0\n"));
        assert!(text.contains("sgpu_gpus_idle 0\n"));
    }
}
