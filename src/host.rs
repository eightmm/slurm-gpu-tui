use crate::{model::json_number, runtime};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const KEYS: &[&str] = &[
    "disk_read_bps",
    "disk_write_bps",
    "net_rx_bps",
    "net_tx_bps",
    "load1",
    "load5",
    "load15",
    "cpu_util",
    "mem_total_bytes",
    "mem_used_bytes",
];

pub fn clean(raw: &Value) -> Value {
    let mut out = json!({});
    for key in KEYS {
        if let Some(value) = json_number(&raw[key]).filter(|n| *n >= 0.0) {
            out[key] = json!(value);
        }
    }
    out
}
fn label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

pub struct Sampler {
    proc_root: PathBuf,
    sys_root: PathBuf,
    previous: BTreeMap<String, (f64, Vec<u64>)>,
    pub disks: BTreeSet<(u32, u32)>,
    next: f64,
    interval: f64,
    cache: Value,
}
impl Default for Sampler {
    fn default() -> Self {
        Self::new("/proc".into(), "/sys".into(), 10.0)
    }
}
impl Sampler {
    pub fn new(proc_root: PathBuf, sys_root: PathBuf, interval: f64) -> Self {
        Self {
            proc_root,
            sys_root,
            previous: BTreeMap::new(),
            disks: BTreeSet::new(),
            next: 0.0,
            interval: interval.max(1.0),
            cache: Value::Null,
        }
    }
    fn text(&self, name: &str) -> Option<String> {
        let dir = runtime::open_dir(&self.proc_root).ok()?;
        String::from_utf8(runtime::read_at(&dir, name, 256 * 1024).ok()?).ok()
    }
    fn network(&self) -> Option<String> {
        let root = runtime::open_dir(&self.proc_root).ok()?;
        let process = if self.proc_root == Path::new("/proc") {
            runtime::open_dir_at(&root, std::process::id().to_string().as_ref()).ok()?
        } else {
            root
        };
        let net = runtime::open_dir_at(&process, "net".as_ref()).ok()?;
        String::from_utf8(runtime::read_at(&net, "dev", 256 * 1024).ok()?).ok()
    }
    fn delta(
        &mut self,
        key: String,
        current: Vec<u64>,
        now: f64,
        seen: &mut BTreeSet<String>,
    ) -> Option<Vec<f64>> {
        seen.insert(key.clone());
        let (time, previous) = self.previous.insert(key, (now, current.clone()))?;
        if now <= time || previous.len() != current.len() {
            return None;
        }
        current
            .iter()
            .zip(previous)
            .map(|(n, p)| n.checked_sub(p).map(|v| v as f64 / (now - time)))
            .collect()
    }
    pub fn sample(&mut self, monotonic: f64, wall: f64) -> Value {
        if monotonic < self.next {
            return self.cache.clone();
        }
        let mut out = json!({"observed_at":wall,"disks":[],"interfaces":[]});
        let mut seen = BTreeSet::new();
        if let Some(raw) = runtime::open_dir(&self.proc_root)
            .ok()
            .and_then(|dir| runtime::first_line_at(&dir, "stat", 8192).ok())
            .filter(|s| s.starts_with("cpu "))
        {
            let values = raw
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .skip(1)
                .take(8)
                .map(str::parse::<u64>)
                .collect::<std::result::Result<Vec<_>, _>>();
            if let Ok(values) = values
                && values.len() == 8
                && let Some(delta) = self.delta("cpu".into(), values, monotonic, &mut seen)
            {
                let total: f64 = delta.iter().sum();
                if total > 0.0 {
                    out["cpu_util"] = json!(100.0 * (total - delta[3] - delta[4]) / total);
                }
            }
        }
        if let Some(raw) = self.text("loadavg") {
            for (key, value) in ["load1", "load5", "load15"]
                .into_iter()
                .zip(raw.split_whitespace())
            {
                if let Some(value) = crate::model::number(value).filter(|v| *v >= 0.0) {
                    out[key] = json!(value);
                }
            }
        }
        if let Some(raw) = self.text("meminfo") {
            let fields: BTreeMap<_, _> = raw
                .lines()
                .filter_map(|line| {
                    let (key, rest) = line.split_once(':')?;
                    Some((key, rest.split_whitespace().next()?.parse::<u64>().ok()?))
                })
                .collect();
            if let Some(total) = fields.get("MemTotal") {
                out["mem_total_bytes"] = json!(*total as f64 * 1024.0);
            }
            if let Some((total, available)) = fields.get("MemTotal").zip(fields.get("MemAvailable"))
                && available <= total
            {
                out["mem_used_bytes"] = json!((total - available) as f64 * 1024.0);
            }
        }
        self.disks.clear();
        let mut disk_rows = Vec::new();
        let mut disk_truncated = false;
        if let Some(raw) = self.text("diskstats") {
            for line in raw.lines() {
                let p: Vec<_> = line.split_whitespace().collect();
                if p.len() < 10 || !label(p[2]) {
                    continue;
                }
                let sys = self.sys_root.join("class/block").join(p[2]);
                if !sys.join("device").exists() || sys.join("partition").exists() {
                    continue;
                }
                let Some((major, minor)) = p[0].parse::<u32>().ok().zip(p[1].parse::<u32>().ok())
                else {
                    continue;
                };
                let Some((read, write)) = p[5].parse::<u64>().ok().zip(p[9].parse::<u64>().ok())
                else {
                    continue;
                };
                if !self.disks.insert((major, minor)) {
                    continue;
                }
                if disk_rows.len() >= 64 {
                    disk_truncated = true;
                    continue;
                }
                let delta = self.delta(
                    format!("disk:{major}:{minor}:{}", p[2]),
                    vec![read, write],
                    monotonic,
                    &mut seen,
                );
                disk_rows.push(json!({"name":p[2],"read_bps":delta.as_ref().map(|v| v[0]*512.0),"write_bps":delta.as_ref().map(|v|v[1]*512.0)}));
            }
        }
        for (key, field) in [
            ("disk_read_bps", "read_bps"),
            ("disk_write_bps", "write_bps"),
        ] {
            if !disk_rows.is_empty()
                && !disk_truncated
                && let Some(sum) = disk_rows
                    .iter()
                    .map(|v| json_number(&v[field]))
                    .sum::<Option<f64>>()
            {
                out[key] = json!(sum);
            }
        }
        out["disks"] = json!(disk_rows);
        out["disk_truncated"] = json!(disk_truncated);
        let mut net_rows = Vec::new();
        let mut network_truncated = false;
        if let Some(raw) = self.network() {
            for line in raw.lines() {
                let Some((name, values)) = line.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                if !label(name)
                    || !self
                        .sys_root
                        .join("class/net")
                        .join(name)
                        .join("device")
                        .exists()
                {
                    continue;
                }
                let p: Vec<_> = values.split_whitespace().collect();
                if p.len() < 16 {
                    continue;
                }
                let Some((rx, tx)) = p[0].parse::<u64>().ok().zip(p[8].parse::<u64>().ok()) else {
                    continue;
                };
                if net_rows.len() >= 64 {
                    network_truncated = true;
                    continue;
                }
                let delta = self.delta(format!("net:{name}"), vec![rx, tx], monotonic, &mut seen);
                net_rows.push(json!({"name":name,"rx_bps":delta.as_ref().map(|v|v[0]),"tx_bps":delta.as_ref().map(|v|v[1])}));
            }
        }
        for (key, field) in [("net_rx_bps", "rx_bps"), ("net_tx_bps", "tx_bps")] {
            if !net_rows.is_empty()
                && !network_truncated
                && let Some(sum) = net_rows
                    .iter()
                    .map(|v| json_number(&v[field]))
                    .sum::<Option<f64>>()
            {
                out[key] = json!(sum);
            }
        }
        out["interfaces"] = json!(net_rows);
        out["network_truncated"] = json!(network_truncated);
        self.previous.retain(|key, _| seen.contains(key));
        self.next = monotonic + self.interval;
        self.cache = out.clone();
        out
    }
}

fn mount_path(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let octal: String = chars.by_ref().take(3).collect();
            let value = u8::from_str_radix(&octal, 8).ok()?;
            if value == 0 {
                return None;
            }
            out.push(value as char);
        } else {
            out.push(c);
        }
    }
    Some(out)
}
pub fn storage(users: &BTreeMap<String, u32>) -> Value {
    let mut users = users.clone();
    let mut truncated = users.len() > 128;
    if let Ok((bytes, _)) = runtime::read_regular(Path::new("/etc/passwd"), 1024 * 1024, Some(0)) {
        for line in String::from_utf8_lossy(&bytes).lines() {
            let fields: Vec<_> = line.split(':').collect();
            if fields.len() >= 7
                && let Ok(uid) = fields[2].parse::<u32>()
                && (1000..65534).contains(&uid)
                && label(fields[0])
            {
                if users.len() < 128 {
                    users.entry(fields[0].into()).or_insert(uid);
                } else if !users.contains_key(fields[0]) {
                    truncated = true;
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut devices = BTreeSet::new();
    let raw = runtime::read_regular(
        &PathBuf::from(format!("/proc/{}/mounts", std::process::id())),
        1024 * 1024,
        None,
    )
    .ok()
    .and_then(|(b, _)| String::from_utf8(b).ok())
    .unwrap_or_default();
    truncated |= raw.lines().count() > 4096;
    for line in raw.lines().take(4096) {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() < 3
            || !matches!(
                p[2],
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "overlay"
            )
        {
            continue;
        }
        let Some(mount) = mount_path(p[1]) else {
            continue;
        };
        let Ok(st) = std::fs::metadata(&mount) else {
            truncated = true;
            continue;
        };
        if !devices.insert(st.dev()) {
            continue;
        }
        if out.len() >= 16 {
            truncated = true;
            break;
        }
        let Ok(path) = CString::new(mount.as_str()) else {
            continue;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // The kernel writes one statvfs record; neither mount nor filesystem is modified.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            truncated = true;
            continue;
        }
        let stats = unsafe { stats.assume_init() };
        let mut quotas = json!({});
        if let Ok(device) = CString::new(p[0]) {
            for (name, uid) in users.iter().take(128) {
                let mut quota = std::mem::MaybeUninit::<libc::dqblk>::zeroed();
                let command = ((libc::Q_GETQUOTA as u32) << 8) | libc::USRQUOTA as u32;
                // Q_GETQUOTA only reads accounting; limit-changing operations are never used.
                if unsafe {
                    libc::quotactl(
                        command as libc::c_int,
                        device.as_ptr(),
                        *uid as libc::c_int,
                        quota.as_mut_ptr().cast(),
                    )
                } == 0
                {
                    let quota = unsafe { quota.assume_init() };
                    quotas[name] = json!({"used_bytes":quota.dqb_curspace,"soft_bytes":quota.dqb_bsoftlimit.saturating_mul(1024),"hard_bytes":quota.dqb_bhardlimit.saturating_mul(1024),"files":quota.dqb_curinodes,"soft_files":quota.dqb_isoftlimit,"hard_files":quota.dqb_ihardlimit});
                }
            }
        }
        out.push(json!({"mount":mount,"filesystem":p[2],"total_bytes":stats.f_blocks as f64*stats.f_frsize as f64,"used_bytes":stats.f_blocks.saturating_sub(stats.f_bfree) as f64*stats.f_frsize as f64,"available_bytes":stats.f_bavail as f64*stats.f_frsize as f64,"files_total":stats.f_files,"files_available":stats.f_favail,"quotas":quotas}));
    }
    json!({"filesystems":out,"truncated":truncated,"accounts":users.len().min(128)})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_need_two_samples_and_skip_partitions_and_virtual_interfaces() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path().join("proc");
        let sys = tmp.path().join("sys");
        for path in [
            proc.join("net"),
            sys.join("class/block/sda/device"),
            sys.join("class/block/sda1/device"),
            sys.join("class/net/eth0/device"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(sys.join("class/block/sda1/partition"), "1").unwrap();
        let write = |read, rx| {
            std::fs::write(
                proc.join("diskstats"),
                format!("8 0 sda 0 0 {read} 0 0 0 200 0\n8 1 sda1 0 0 999 0 0 0 999 0\n"),
            )
            .unwrap();
            std::fs::write(proc.join("net/dev"),format!("eth0: {rx} 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\nveth0: 999 0 0 0 0 0 0 0 999 0 0 0 0 0 0 0\n")).unwrap();
        };
        let mut sampler = Sampler::new(proc.clone(), sys, 1.0);
        write(100, 100);
        assert!(sampler.sample(0.0, 100.0)["disk_read_bps"].is_null());
        write(102, 104);
        let data = sampler.sample(2.0, 102.0);
        assert_eq!(data["disk_read_bps"], 512.0);
        assert_eq!(data["net_rx_bps"], 2.0);
        assert_eq!(data["disks"].as_array().unwrap().len(), 1);
        write(1, 1);
        let data = sampler.sample(4.0, 104.0);
        assert!(data["disk_read_bps"].is_null());
        assert!(data["net_rx_bps"].is_null());
    }
    #[test]
    fn missing_is_unknown_and_mount_escapes_are_decoded() {
        let mut sampler = Sampler::new("/does-not-exist".into(), "/does-not-exist".into(), 1.0);
        assert!(sampler.sample(0.0, 100.0)["net_rx_bps"].is_null());
        assert_eq!(mount_path("/mnt/a\\040b"), Some("/mnt/a b".into()));
        assert_eq!(mount_path("/mnt/\\000"), None);
        assert!(
            clean(&json!({"net_rx_bps":true,"disk_read_bps":-1}))
                .as_object()
                .unwrap()
                .is_empty()
        );
    }
}
