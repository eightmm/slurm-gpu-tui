use crate::{Result, model::json_number, runtime};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub fn text_at(dir: &File, name: &str) -> Result<String> {
    Ok(String::from_utf8(runtime::read_at(dir, name, 8192)?)?)
}

pub fn pressure(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(dir) = runtime::open_dir(root) else {
        return out;
    };
    for name in ["cpu", "memory", "io"] {
        let Ok(text) = text_at(&dir, name) else {
            continue;
        };
        if let Some(value) = text
            .lines()
            .find(|s| s.starts_with("some "))
            .and_then(|s| s.split_whitespace().find_map(|v| v.strip_prefix("avg10=")))
            .and_then(crate::model::number)
            .filter(|n| (0.0..=100.0).contains(n))
        {
            out.insert(name.into(), format!("{value:.1}"));
        }
    }
    out
}

pub fn clean_sample(raw: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in [
        "observed_at",
        "cpu_cores",
        "mem_current_mib",
        "mem_peak_mib",
        "mem_limit_mib",
        "oom_kill",
        "vram_mib",
        "io_read_bps",
        "io_write_bps",
    ] {
        if let Some(number) = json_number(&raw[key]).filter(|n| *n >= 0.0) {
            out.insert(key.into(), json!(number));
        }
    }
    Value::Object(out)
}

pub fn clean_telemetry(raw: &Value) -> Value {
    let mut out = clean_sample(raw);
    if let Some(n) = json_number(&raw["cpu_util"]).filter(|n| (0.0..=100.0).contains(n)) {
        out["cpu_util"] = json!(format!("{n:.1}"));
    }
    out["host_io"] = crate::host::clean(&raw["host_io"]);
    out["pressure"] = json!({});
    for key in ["cpu", "memory", "io"] {
        if let Some(n) = json_number(&raw["pressure"][key]).filter(|n| (0.0..=100.0).contains(n)) {
            out["pressure"][key] = json!(format!("{n:.1}"));
        }
    }
    out["jobs"] = json!({});
    if let Some(jobs) = raw["jobs"].as_object() {
        for (id, sample) in jobs.iter().take(4096) {
            if numeric_id(id) {
                out["jobs"][id] = clean_sample(sample);
            }
        }
    }
    out["jobs_truncated"] = json!(
        raw["jobs_truncated"] == true || raw["jobs"].as_object().is_some_and(|m| m.len() > 4096)
    );
    out
}

fn numeric_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
}

fn children(dir: &File, limit: usize) -> Vec<std::ffi::OsString> {
    use std::os::fd::AsRawFd;
    // The proc fd link refers to this open directory even if its path is renamed.
    let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .take(limit)
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    names.sort();
    names
}

type IoCounters = BTreeMap<String, [u64; 2]>;

pub struct NodeSampler {
    proc_root: PathBuf,
    cgroup_root: PathBuf,
    interval: f64,
    max_jobs: usize,
    next: f64,
    cache: Value,
    cpu_previous: Option<[u64; 8]>,
    host: crate::host::Sampler,
    io_previous: BTreeMap<(u64, u64), (f64, IoCounters)>,
    job_previous: BTreeMap<(u64, u64), (f64, u64)>,
}

impl Default for NodeSampler {
    fn default() -> Self {
        Self::new(
            "/proc".into(),
            "/sys/fs/cgroup".into(),
            runtime::env_seconds("SLURM_GPU_TUI_TELEMETRY_SEC", 10.0, 1.0),
            runtime::env_seconds("SLURM_GPU_TUI_TELEMETRY_MAX_JOBS", 256.0, 1.0) as usize,
        )
    }
}

impl NodeSampler {
    pub fn new(proc_root: PathBuf, cgroup_root: PathBuf, interval: f64, max_jobs: usize) -> Self {
        Self {
            host: crate::host::Sampler::new(proc_root.clone(), "/sys".into(), interval),
            io_previous: BTreeMap::new(),
            proc_root,
            cgroup_root,
            interval: interval.max(1.0),
            max_jobs: max_jobs.clamp(1, 4096),
            next: 0.0,
            cache: Value::Null,
            cpu_previous: None,
            job_previous: BTreeMap::new(),
        }
    }

    pub fn sample(&mut self, monotonic: f64, wall: f64) -> Value {
        if monotonic < self.next {
            return self.cache.clone();
        }
        let mut out =
            json!({"observed_at":wall, "pressure":pressure(&self.proc_root.join("pressure"))});
        let counters = (|| -> Option<[u64; 8]> {
            let dir = runtime::open_dir(&self.proc_root).ok()?;
            let raw = runtime::first_line_at(&dir, "stat", 8192).ok()?;
            let mut fields = raw.lines().next()?.split_whitespace();
            if fields.next()? != "cpu" {
                return None;
            }
            let values: Vec<u64> = fields
                .take(8)
                .map(str::parse)
                .collect::<std::result::Result<_, _>>()
                .ok()?;
            values.try_into().ok()
        })();
        if let (Some(previous), Some(current)) = (self.cpu_previous, counters)
            && current.iter().zip(previous).all(|(a, b)| *a >= b)
        {
            let diffs: Vec<u64> = current.iter().zip(previous).map(|(a, b)| a - b).collect();
            let total: f64 = diffs.iter().map(|n| *n as f64).sum();
            if total > 0.0 {
                out["cpu_util"] = json!(format!(
                    "{:.1}",
                    100.0 * (total - diffs[3] as f64 - diffs[4] as f64) / total
                ));
            }
        }
        self.cpu_previous = counters;
        out["host_io"] = crate::host::clean(&self.host.sample(monotonic, wall));
        let (jobs, truncated) = self.jobs(monotonic, wall);
        out["jobs"] = json!(jobs);
        out["jobs_truncated"] = json!(truncated);
        self.next = monotonic + self.interval;
        self.cache = out.clone();
        out
    }

    fn sluid_job(&self, dir: &File) -> Option<String> {
        for step in children(dir, 4096)
            .into_iter()
            .filter(|n| {
                n.to_str().is_some_and(|n| {
                    n.starts_with("step_")
                        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
            })
            .take(32)
        {
            let Ok(step) = runtime::open_dir_at(dir, &step) else {
                continue;
            };
            let Ok(daemon) = runtime::open_dir_at(&step, "slurm".as_ref()) else {
                continue;
            };
            let Ok(pids) = text_at(&daemon, "cgroup.procs") else {
                continue;
            };
            let Ok(proc) = runtime::open_dir(&self.proc_root) else {
                continue;
            };
            for pid in pids.split_whitespace().take(32).filter(|s| numeric_id(s)) {
                let Ok(pid) = runtime::open_dir_at(&proc, pid.as_ref()) else {
                    continue;
                };
                if !pid.metadata().is_ok_and(|s| s.uid() == 0) {
                    continue;
                }
                let Ok(command) = text_at(&pid, "cmdline") else {
                    continue;
                };
                let first = command.split('\0').next().unwrap_or("");
                if let Some(inner) = first
                    .strip_prefix("slurmstepd: [")
                    .and_then(|s| s.strip_suffix(']'))
                    && let Some((id, step)) = inner.split_once('.')
                    && numeric_id(id)
                    && !step.is_empty()
                    && step.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Some(id.into());
                }
            }
        }
        None
    }

    fn job_values(&mut self, dir: &File, monotonic: f64, wall: f64) -> Value {
        let mut out = json!({"observed_at":wall});
        let Ok(st) = dir.metadata() else {
            return out;
        };
        let identity = (st.dev(), st.ino());
        let cpu = text_at(dir, "cpu.stat").ok().and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("usage_usec ")
                    .and_then(|s| s.trim().parse::<u64>().ok())
            })
        });
        if let Some(usage) = cpu {
            if let Some((prev_time, prev_usage)) =
                self.job_previous.insert(identity, (monotonic, usage))
                && monotonic > prev_time
                && usage >= prev_usage
            {
                out["cpu_cores"] =
                    json!((usage - prev_usage) as f64 / (monotonic - prev_time) / 1e6);
            }
        } else {
            self.job_previous.remove(&identity);
        }
        let io = text_at(dir, "io.stat").ok().and_then(|raw| {
            let mut devices = BTreeMap::new();
            for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                let mut fields = line.split_whitespace();
                let device = fields.next()?;
                let (major, minor) = device.split_once(':')?;
                let id = (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?);
                if !self.host.disks.contains(&id) {
                    continue;
                }
                let values: BTreeMap<_, _> = fields.filter_map(|v| v.split_once('=')).collect();
                let read = values.get("rbytes")?.parse::<u64>().ok()?;
                let write = values.get("wbytes")?.parse::<u64>().ok()?;
                if devices.insert(device.to_string(), [read, write]).is_some() {
                    return None;
                }
            }
            if !raw.trim().is_empty() && devices.is_empty() {
                None
            } else {
                Some(devices)
            }
        });
        if let Some(current) = io {
            if let Some((time, previous)) = self
                .io_previous
                .insert(identity, (monotonic, current.clone()))
                && monotonic > time
                && current.keys().eq(previous.keys())
            {
                let rates = current.iter().try_fold([0u64; 2], |sum, (device, values)| {
                    Some([
                        sum[0].checked_add(values[0].checked_sub(previous[device][0])?)?,
                        sum[1].checked_add(values[1].checked_sub(previous[device][1])?)?,
                    ])
                });
                if let Some(rates) = rates {
                    out["io_read_bps"] = json!(rates[0] as f64 / (monotonic - time));
                    out["io_write_bps"] = json!(rates[1] as f64 / (monotonic - time));
                }
            }
        } else {
            self.io_previous.remove(&identity);
        }
        for (file, key) in [
            ("memory.current", "mem_current_mib"),
            ("memory.peak", "mem_peak_mib"),
            ("memory.max", "mem_limit_mib"),
        ] {
            if let Some(value) = text_at(dir, file)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                out[key] = json!(value as f64 / 1048576.0);
            }
        }
        if let Some(count) = text_at(dir, "memory.events").ok().and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("oom_kill ")
                    .and_then(|s| s.trim().parse::<u64>().ok())
            })
        }) {
            out["oom_kill"] = json!(count);
        }
        out
    }

    fn jobs(&mut self, monotonic: f64, wall: f64) -> (BTreeMap<String, Value>, bool) {
        let mut jobs = BTreeMap::new();
        let mut truncated = false;
        let mut identities = BTreeSet::new();
        let mut duplicates = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut count = 0;
        let Ok(root) = runtime::open_dir(&self.cgroup_root) else {
            self.job_previous.clear();
            self.io_previous.clear();
            return (jobs, false);
        };
        if text_at(&root, "cgroup.controllers").is_err() {
            self.job_previous.clear();
            self.io_previous.clear();
            return (jobs, false);
        }
        let parents = [Some(&root), None];
        let system = runtime::open_dir_at(&root, "system.slice".as_ref()).ok();
        for parent in parents {
            let Some(parent) = parent.or(system.as_ref()) else {
                continue;
            };
            for name in children(parent, 4096)
                .into_iter()
                .filter(|s| {
                    s.to_str().is_some_and(|s| {
                        s == "slurm"
                            || (s.ends_with("slurmstepd.scope")
                                && s.bytes().all(|b| {
                                    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
                                }))
                    })
                })
                .take(64)
            {
                let Ok(scope) = runtime::open_dir_at(parent, &name) else {
                    continue;
                };
                let mut queue = VecDeque::from([(scope, 0)]);
                while let Some((dir, depth)) = queue.pop_front() {
                    let Ok(st) = dir.metadata() else {
                        continue;
                    };
                    if !visited.insert((st.dev(), st.ino())) {
                        continue;
                    }
                    for name in children(&dir, 4097) {
                        count += 1;
                        if count > 4096 || jobs.len() >= self.max_jobs {
                            truncated = true;
                            break;
                        }
                        let Some(text) = name.to_str() else {
                            continue;
                        };
                        let id = text.strip_prefix("job_").filter(|s| numeric_id(s));
                        let sluid = text.starts_with('s')
                            && (2..=33).contains(&text.len())
                            && text[1..].bytes().all(|b| b.is_ascii_alphanumeric());
                        let uid = depth < 2 && text.strip_prefix("uid_").is_some_and(numeric_id);
                        if id.is_none() && !sluid && !uid {
                            continue;
                        }
                        let Ok(child) = runtime::open_dir_at(&dir, &name) else {
                            continue;
                        };
                        if uid {
                            queue.push_back((child, depth + 1));
                            continue;
                        }
                        let Some(id) = id.map(str::to_string).or_else(|| self.sluid_job(&child))
                        else {
                            continue;
                        };
                        let Ok(st) = child.metadata() else {
                            continue;
                        };
                        identities.insert((st.dev(), st.ino()));
                        if jobs.contains_key(&id) || duplicates.contains(&id) {
                            jobs.remove(&id);
                            duplicates.insert(id);
                        } else {
                            jobs.insert(id, self.job_values(&child, monotonic, wall));
                        }
                    }
                    if truncated {
                        break;
                    }
                }
                if truncated {
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        self.job_previous
            .retain(|identity, _| identities.contains(identity));
        self.io_previous
            .retain(|identity, _| identities.contains(identity));
        (jobs, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn job_io_uses_physical_byte_counters_and_handles_empty_reset_and_symlink_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path().join("proc");
        let sys = tmp.path().join("sys");
        let cgroup = tmp.path().join("cgroup");
        let job = cgroup.join("slurm/job_42");
        for dir in [&proc, &job, &sys.join("class/block/sda/device")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(proc.join("diskstats"), "8 0 sda 0 0 100 0 0 0 100 0\n").unwrap();
        fs::write(cgroup.join("cgroup.controllers"), "cpu memory io").unwrap();
        fs::write(
            job.join("io.stat"),
            "8:0 rbytes=100 wbytes=200 rios=1 wios=1\n",
        )
        .unwrap();
        let mut sampler = NodeSampler::new(proc.clone(), cgroup, 1.0, 256);
        sampler.host = crate::host::Sampler::new(proc, sys, 1.0);
        assert!(sampler.sample(0.0, 100.0)["jobs"]["42"]["io_read_bps"].is_null());
        fs::write(job.join("io.stat"), "8:0 rbytes=104 wbytes=208\n").unwrap();
        let data = sampler.sample(2.0, 102.0);
        assert_eq!(data["jobs"]["42"]["io_read_bps"], 2.0);
        assert_eq!(data["jobs"]["42"]["io_write_bps"], 4.0);
        fs::write(job.join("io.stat"), "").unwrap();
        assert!(sampler.sample(4.0, 104.0)["jobs"]["42"]["io_read_bps"].is_null());
        assert_eq!(sampler.sample(6.0, 106.0)["jobs"]["42"]["io_read_bps"], 0.0);
        fs::write(job.join("io.stat"), "8:0 rbytes=bad wbytes=0\n").unwrap();
        assert!(sampler.sample(8.0, 108.0)["jobs"]["42"]["io_read_bps"].is_null());
        fs::remove_file(job.join("io.stat")).unwrap();
        let other = tmp.path().join("other-counter");
        fs::write(&other, "8:0 rbytes=999 wbytes=999").unwrap();
        symlink(other, job.join("io.stat")).unwrap();
        assert!(sampler.sample(10.0, 110.0)["jobs"]["42"]["io_read_bps"].is_null());
    }

    #[test]
    fn cpu_needs_two_samples_and_counter_resets_are_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path().join("proc");
        fs::create_dir(&proc).unwrap();
        fs::write(
            proc.join("stat"),
            format!("cpu 100 0 0 100 0 0 0 0\n{}", "cpu0 1 2 3 4\n".repeat(1024)),
        )
        .unwrap();
        let mut sampler = NodeSampler::new(proc.clone(), tmp.path().join("missing"), 1.0, 256);
        assert!(sampler.sample(0.0, 100.0)["cpu_util"].is_null());
        fs::write(proc.join("stat"), "cpu 200 0 0 200 0 0 0 0\n").unwrap();
        assert_eq!(sampler.sample(1.0, 101.0)["cpu_util"], "50.0");
        fs::write(proc.join("stat"), "cpu 1 0 0 1 0 0 0 0\n").unwrap();
        assert!(sampler.sample(2.0, 102.0)["cpu_util"].is_null());
    }

    #[test]
    fn cgroup_samples_are_bounded_and_never_follow_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("cgroup.controllers"), "cpu memory").unwrap();
        let scope = tmp.path().join("slurm");
        let job = scope.join("job_42");
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join("cpu.stat"), "usage_usec 1000000\n").unwrap();
        fs::write(job.join("memory.current"), "1048576").unwrap();
        fs::write(job.join("memory.max"), "max").unwrap();
        fs::write(job.join("memory.events"), "oom_kill 2\n").unwrap();
        symlink(&job, scope.join("job_43")).unwrap();
        let mut sampler = NodeSampler::new(tmp.path().join("missing"), tmp.path().into(), 1.0, 256);
        let first = sampler.sample(0.0, 100.0);
        assert_eq!(first["jobs"]["42"]["mem_current_mib"], 1.0);
        assert!(first["jobs"]["42"]["mem_limit_mib"].is_null());
        assert!(first["jobs"]["43"].is_null());
        fs::write(job.join("cpu.stat"), "usage_usec 3000000\n").unwrap();
        assert_eq!(sampler.sample(1.0, 101.0)["jobs"]["42"]["cpu_cores"], 2.0);
    }

    #[test]
    fn telemetry_allowlist_rejects_negative_nonfinite_and_boolean() {
        let clean = clean_telemetry(
            &json!({"observed_at":10, "cpu_util":"NaN", "secret":"private", "jobs":{"42":{"oom_kill":true,"cpu_cores":-1,"mem_current_mib":3},"../x":{"cpu_cores":2}}}),
        );
        assert!(clean["cpu_util"].is_null());
        assert!(clean["secret"].is_null());
        assert!(clean["jobs"]["42"]["oom_kill"].is_null());
        assert!(clean["jobs"]["42"]["cpu_cores"].is_null());
        assert_eq!(clean["jobs"]["42"]["mem_current_mib"], 3.0);
        assert_eq!(clean["jobs"].as_object().unwrap().len(), 1);
    }
}
