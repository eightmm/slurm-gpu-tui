use crate::{model::json_number as number, rules, users};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Info,
    Good,
    Watch,
    High,
    Critical,
    Unknown,
}
pub struct Row {
    pub key: String,
    pub cells: Vec<String>,
    pub level: Level,
    pub percent: Option<f64>,
    pub detail: String,
}
fn valid(value: Option<f64>) -> Option<f64> {
    value.filter(|n| n.is_finite() && *n >= 0.0)
}
pub fn amount(value: Option<f64>, rate: bool) -> String {
    let Some(mut value) = valid(value) else {
        return "?".into();
    };
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut index = 0;
    while value >= 1023.95 && index < units.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    let unit = if rate {
        format!("{}/s", units[index])
    } else {
        units[index].into()
    };
    format!(
        "{value:>6.1} {unit:<width$}",
        width = if rate { 5 } else { 3 }
    )
}
fn decimal(value: Option<f64>, unit: &str) -> String {
    valid(value)
        .map(|v| format!("{v:.2}{unit}"))
        .unwrap_or_else(|| "?".into())
}
fn count(value: Option<f64>) -> String {
    valid(value)
        .map(|v| {
            for (scale, unit) in [(1e9, "G"), (1e6, "M"), (1e3, "k")] {
                if v >= scale {
                    return if v / scale >= 100.0 {
                        format!("{:.0}{unit}", v / scale)
                    } else {
                        format!("{:.1}{unit}", v / scale)
                    };
                }
            }
            format!("{v:.0}")
        })
        .unwrap_or_else(|| "?".into())
}
fn link_speed(value: Option<f64>) -> String {
    valid(value)
        .map(|v| {
            if v >= 1e6 {
                format!("{:.1} Tb/s", v / 1e6)
            } else if v >= 1e3 {
                format!("{:.1} Gb/s", v / 1e3)
            } else {
                format!("{v:.0} Mb/s")
            }
        })
        .unwrap_or_else(|| "? speed".into())
}
fn meter(value: Option<f64>) -> String {
    let Some(v) = valid(value) else {
        return "—".into();
    };
    let n = (v.clamp(0.0, 100.0) / 10.0).round() as usize;
    let value = if v > 0.0 && v < 1.0 {
        format!("{v:>3.1}")
    } else {
        format!("{v:>3.0}")
    };
    format!("{}{} {value}%", "━".repeat(n), "·".repeat(10 - n))
}
fn capacity(pct: Option<f64>) -> (&'static str, Level) {
    match valid(pct) {
        None => ("UNKNOWN", Level::Unknown),
        Some(p) if p >= 97.0 => ("CRITICAL >=97%", Level::Critical),
        Some(p) if p >= 90.0 => ("HIGH >=90%", Level::High),
        Some(p) if p >= 80.0 => ("WATCH >=80%", Level::Watch),
        _ => ("HEADROOM <80%", Level::Good),
    }
}
fn ratio(used: Option<f64>, total: Option<f64>) -> Option<f64> {
    valid(used)
        .zip(valid(total))
        .filter(|(_, t)| *t > 0.0)
        .map(|(u, t)| u / t * 100.0)
}
fn row(
    key: &str,
    name: &str,
    a: String,
    b: String,
    pct: Option<f64>,
    guide: (&str, Level),
    detail: String,
) -> Row {
    Row {
        key: key.into(),
        cells: vec![name.into(), a, b, meter(pct), guide.0.into()],
        level: guide.1,
        percent: pct,
        detail,
    }
}

#[derive(Default)]
pub struct History {
    samples: VecDeque<(f64, BTreeMap<String, f64>)>,
    host: String,
}
impl History {
    pub fn observe(&mut self, host: &Value, now: f64) {
        let name = host["hostname"].as_str().unwrap_or("");
        if self.host != name {
            self.samples.clear();
            self.host = name.into();
        }
        self.samples
            .retain(|(t, _)| (0.0..=300.0).contains(&(now - *t)));
        let Some(stamp) = number(&host["observed_at"]).filter(|t| rules::telemetry_fresh(*t, now))
        else {
            return;
        };
        if self.samples.back().is_some_and(|(t, _)| stamp <= *t) {
            return;
        }
        let mut values = BTreeMap::new();
        for key in ["cpu_util", "mem_used_bytes", "load1"] {
            if let Some(v) = number(&host[key]) {
                values.insert(key.into(), v);
            }
        }
        for (group, fields) in [
            ("disks", ["read_bps", "write_bps", "await_ms"]),
            ("interfaces", ["rx_bps", "tx_bps", "link_pct"]),
        ] {
            for device in host[group].as_array().into_iter().flatten().take(64) {
                let name = device["name"].as_str().unwrap_or("?");
                for field in fields {
                    if let Some(v) = number(&device[field]) {
                        values.insert(format!("{group}:{name}:{field}"), v);
                    }
                }
            }
        }
        self.samples.push_back((stamp, values));
        while self.samples.len() > 64 {
            self.samples.pop_front();
        }
    }
    fn prior_median(&self, key: &str, now: f64) -> Option<f64> {
        let mut values = self
            .samples
            .iter()
            .rev()
            .skip(1)
            .filter(|(t, _)| (0.0..=300.0).contains(&(now - *t)))
            .filter_map(|(_, v)| v.get(key).copied())
            .collect::<Vec<_>>();
        if values.len() < 6 {
            return None;
        }
        values.sort_by(f64::total_cmp);
        Some(values[values.len() / 2])
    }
    fn describe(&self, key: &str, now: f64, rate: bool) -> String {
        let mut bins = [None; 30];
        for (stamp, values) in &self.samples {
            let age = now - stamp;
            if (0.0..300.0).contains(&age) {
                bins[29 - (age / 10.0) as usize] = values.get(key).copied();
            }
        }
        let values = bins.iter().flatten().copied().collect::<Vec<_>>();
        let Some(peak) = values.iter().copied().reduce(f64::max) else {
            return "5m trend: ? no observations".into();
        };
        let glyphs = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let spark = bins
            .iter()
            .map(|v| {
                v.map(|v| {
                    glyphs[if peak > 0.0 {
                        (v / peak * 7.0).round().clamp(0.0, 7.0) as usize
                    } else {
                        0
                    }]
                })
                .unwrap_or('·')
            })
            .collect::<String>();
        let mut sorted = values;
        sorted.sort_by(f64::total_cmp);
        let fmt = |v| {
            if rate {
                amount(Some(v), true).trim().into()
            } else {
                decimal(Some(v), "")
            }
        };
        format!(
            "5m trend: {spark}\nMedian {} · peak {} · {} bins observed\n10-second bins; · is missing, not zero. Scale is relative to this metric's peak.",
            fmt(sorted[sorted.len() / 2]),
            fmt(peak),
            sorted.len()
        )
    }
}

pub fn rows(host: &Value, collector: &Value, history: &History, now: f64) -> Vec<Row> {
    let mut out = Vec::new();
    let Some(stamp) = number(&host["observed_at"]).filter(|t| rules::telemetry_fresh(*t, now))
    else {
        return out;
    };
    let cpus = number(&host["cpu_online"]).filter(|n| *n > 0.0);
    let cpu = number(&host["cpu_util"]);
    let cpu_psi = number(&host["cpu_some_pct"]);
    let cpu_guide = match (cpu, cpu_psi) {
        (_, Some(p)) if p >= 10.0 => ("CPU WAIT >=10%", Level::Watch),
        (Some(p), _) if p >= 90.0 => ("BUSY >=90%", Level::Info),
        (Some(_), _) => ("BUSY IS NOT FAULT", Level::Info),
        _ => ("WARMUP / UNKNOWN", Level::Unknown),
    };
    out.push(row("host:cpu","CPU busy",decimal(cpu,"%"),cpus.map(|n|format!("{n:.0} CPUs")).unwrap_or_else(||"? CPUs".into()),cpu,cpu_guide,
        format!("High CPU activity can be productive. CPU PSI >=10% is a review hint, not a fault diagnosis.\nOnline logical CPUs: {}\nI/O wait: {} (CPU accounting, not device utilization)\n{}",decimal(cpus,""),decimal(number(&host["cpu_iowait_pct"]),"%"),history.describe("cpu_util",now,false))));
    for (prefix, label) in [("mem", "RAM"), ("swap", "Swap")] {
        let used = number(&host[format!("{prefix}_used_bytes")]);
        let total = number(&host[format!("{prefix}_total_bytes")]);
        let pct = ratio(used, total);
        let guide = if prefix == "swap" {
            match (used, total) {
                (_, Some(0.0)) => ("NOT CONFIGURED", Level::Info),
                (None, _) | (_, None) => ("UNKNOWN", Level::Unknown),
                (Some(v), _) if v > 0.0 => ("IN USE; CHECK PSI", Level::Info),
                _ => ("NO SWAP IN USE", Level::Info),
            }
        } else {
            capacity(pct)
        };
        out.push(row(&format!("host:{prefix}"),label,amount(used,false),amount(total,false),pct,guide,
            format!("Used {} / total {}\nAvailable {}\nRAM uses MemTotal - MemAvailable, so reclaimable cache is not treated as unavailable.\nCapacity guide: <80% headroom; >=80% watch; >=90% high; >=97% critical. These are sgpu review hints.\nSwap occupancy alone does not prove current swapping or a fault; inspect memory PSI.",amount(used,false).trim(),amount(total,false).trim(),amount(total.zip(used).and_then(|(t,u)|(u<=t).then_some(t-u)),false).trim())));
    }
    let load = number(&host["load1"]);
    let per_cpu = load.zip(cpus).map(|(l, c)| l / c);
    let load_guide = match per_cpu {
        Some(v) if v > 1.0 => ("LOAD/CPU >1", Level::Watch),
        Some(_) => ("LOAD/CPU <=1", Level::Info),
        _ => ("? CPU CAPACITY", Level::Unknown),
    };
    let mut load_row = row(
        "host:load",
        "Load 1 / 5 / 15",
        decimal(load, ""),
        decimal(number(&host["load5"]), ""),
        None,
        load_guide,
        format!(
            "1m {} / 5m {} / 15m {}\nLoad1 per online logical CPU: {}\nA sustained ratio >1 suggests runnable or uninterruptible work beyond CPU count. Load includes I/O waits; it is not CPU utilization. Host CPU count ignores container CPU quotas.\n{}",
            decimal(load, ""),
            decimal(number(&host["load5"]), ""),
            decimal(number(&host["load15"]), ""),
            decimal(per_cpu, ""),
            history.describe("load1", now, false)
        ),
    );
    load_row.cells[3] = decimal(number(&host["load15"]), "");
    out.push(load_row);
    for (kind, label) in [
        ("cpu", "CPU pressure"),
        ("memory", "Memory pressure"),
        ("io", "I/O pressure"),
    ] {
        let some = number(&host[format!("{kind}_some_pct")]);
        let full = if kind == "cpu" {
            None
        } else {
            number(&host[format!("{kind}_full_pct")])
        };
        let guide = match some {
            None => ("NOT AVAILABLE", Level::Unknown),
            Some(p) if p >= 10.0 => ("STALL >=10%", Level::High),
            Some(p) if p >= 1.0 => ("STALL >=1%", Level::Watch),
            _ => ("LOW STALL <1%", Level::Good),
        };
        out.push(row(&format!("host:psi:{kind}"),label,decimal(some,"% some"),if kind=="cpu" {"—".into()} else {decimal(full,"% full")},some,guide,
            "PSI avg10: percentage of time over the recent 10 seconds. Some: at least one task stalled; full: all non-idle tasks stalled together. System CPU full is undefined.\nsgpu hints: <1% low stall, >=1% review, >=10% high stall. Confirm sustained impact and workload expectations; these are not universal limits.".into()));
    }
    for disk in host["disks"].as_array().into_iter().flatten().take(64) {
        let name = disk["name"].as_str().unwrap_or("?");
        let busy = number(&disk["busy_pct"]);
        let await_ms = number(&disk["await_ms"]);
        let await_key = format!("disks:{name}:await_ms");
        let rising = await_ms
            .zip(history.prior_median(&await_key, now))
            .is_some_and(|(v, m)| v >= 1.0 && v > m.max(0.1) * 2.0);
        let guide = if rising {
            ("LATENCY >2x MED", Level::Watch)
        } else if busy.is_some_and(|v| v >= 80.0) {
            ("ACTIVE >=80%", Level::Watch)
        } else {
            ("CHECK LATENCY/QUEUE", Level::Info)
        };
        out.push(row(&format!("disk:{name}"),&format!("Disk {name}"),amount(number(&disk["read_bps"]),true),amount(number(&disk["write_bps"]),true),busy,guide,
            format!("Read {} / write {}\nBusy {} · read+write IOPS {}\nRead+write await {} ms · average queue depth {} · in flight {}\nBusy time is an activity hint, not percent of maximum disk throughput. Parallel SSD/NVMe devices can still have headroom at high busy time. Await and IOPS exclude discard/flush; idle await is unavailable, not zero.\nNo universal normal MiB/s or latency. LATENCY >2x MED compares at least 6 prior observations (>=1ms current) with the recent median.\nRead {}\nWrite {}\nAwait {}",amount(number(&disk["read_bps"]),true).trim(),amount(number(&disk["write_bps"]),true).trim(),decimal(busy,"%"),decimal(number(&disk["iops"]),""),decimal(await_ms,""),decimal(number(&disk["queue_depth"]),""),decimal(number(&disk["inflight"]),""),history.describe(&format!("disks:{name}:read_bps"),now,true),history.describe(&format!("disks:{name}:write_bps"),now,true),history.describe(&await_key,now,false))));
        out.push(row(&format!("disk-latency:{name}"),"  await / queue",decimal(await_ms," ms"),decimal(number(&disk["queue_depth"])," queued"),None,("R+W completed I/O",Level::Info),"Read/write completion latency and time-weighted queue depth. Interpret together with IOPS, busy time, workload and I/O PSI.".into()));
    }
    for net in host["interfaces"].as_array().into_iter().flatten().take(64) {
        let name = net["name"].as_str().unwrap_or("?");
        let link = number(&net["link_pct"]);
        let errors = number(&net["errors_ps"]);
        let drops = number(&net["drops_ps"]);
        let state = net["state"].as_str().unwrap_or("unknown");
        let guide = if matches!(state, "down" | "lowerlayerdown" | "notpresent") {
            ("LINK DOWN / UNUSED?", Level::Watch)
        } else if errors.is_some_and(|v| v > 0.0) {
            ("ERRORS; INSPECT", Level::High)
        } else if drops.is_some_and(|v| v > 0.0) {
            ("DROPS; INSPECT", Level::Watch)
        } else if link.is_some_and(|v| v >= 80.0) {
            ("LINK >=80%", Level::Watch)
        } else if link.is_some() {
            ("LINK <80%", Level::Good)
        } else {
            ("? LINK CAPACITY", Level::Unknown)
        };
        out.push(row(&format!("net:{name}"),&format!("Net {name}"),amount(number(&net["rx_bps"]),true),amount(number(&net["tx_bps"]),true),link,guide,
            format!("RX {} / TX {}\nLink: {} Mb/s · {} duplex · state {state}\nErrors {} /s · drops {} /s (RX+TX)\nFull duplex uses max(RX,TX) / per-direction link capacity; half duplex uses RX+TX. Percentage requires reported speed, duplex and an up link. Near >=80% is a review hint; payload throughput is below nominal wire speed. An unused down port is not necessarily a fault.\nCounters include host traffic, not per-user traffic or complete offloaded RDMA accounting. Drops are kernel counter increments, not measured end-to-end loss.\nRX {}\nTX {}",amount(number(&net["rx_bps"]),true).trim(),amount(number(&net["tx_bps"]),true).trim(),decimal(number(&net["speed_mbps"]),""),net["duplex"].as_str().unwrap_or("unknown"),decimal(errors,""),decimal(drops,""),history.describe(&format!("interfaces:{name}:rx_bps"),now,true),history.describe(&format!("interfaces:{name}:tx_bps"),now,true))));
        let mut link_row=row(&format!("net-link:{name}"),"  link capacity",link_speed(number(&net["speed_mbps"])),format!("{} duplex",net["duplex"].as_str().unwrap_or("?")),None,(state,Level::Info),"Link speed uses decimal bits/s; full duplex provides nominal capacity independently in each direction. Unknown speed or duplex leaves the link percentage unavailable.".into());
        link_row.cells[3] = state.into();
        out.push(link_row);
        out.push(row(&format!("net-errors:{name}"),"  errors / drops",decimal(errors," /s"),decimal(drops," /s"),None,("RX+TX counters",Level::Info),"Observed error/drop counter increments per second. Zero is observed only after two valid samples; missing/reset counters stay unavailable.".into()));
    }
    if users::storage_fresh(host, now) {
        for fs in host["filesystems"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
        {
            let mount = fs["mount"].as_str().unwrap_or("?");
            let used = number(&fs["used_bytes"]);
            let total = number(&fs["total_bytes"]);
            let available = number(&fs["available_bytes"]);
            let pct = ratio(used, used.zip(available).map(|(u, a)| u + a));
            let inode_total = number(&fs["files_total"]).filter(|v| *v > 0.0);
            let inode_free = number(&fs["files_free"]);
            let inode_pct = ratio(
                inode_total
                    .zip(inode_free)
                    .and_then(|(t, f)| (f <= t).then_some(t - f)),
                inode_total,
            );
            let guide = capacity(pct);
            let age = number(&host["storage_observed_at"])
                .map(|t| (now - t).max(0.0))
                .unwrap_or(0.0);
            out.push(row(&format!("fs:{mount}"),&format!("FS {mount}"),amount(used,false),amount(total,false),pct,guide,
                format!("Used {} / total {}\nAvailable to ordinary users: {}\nUsed fraction: used / (used + available), excluding reserved blocks (df-style). Capacity guide: 80/90/97% watch/high/critical.\nInodes used {} · available {} of {}\nSample age {age:.0}s; storage refreshes in the background every 120s and expires at 300s. No filesystem scan is performed.",amount(used,false).trim(),amount(total,false).trim(),amount(available,false).trim(),decimal(inode_pct,"%"),decimal(number(&fs["files_available"]),""),decimal(inode_total,""))));
            out.push(row(&format!("fs-free:{mount}"),"  available",amount(available,false),format!("{} inodes",count(number(&fs["files_available"]))),inode_pct,capacity(inode_pct),"Available bytes exclude reserved blocks. Inode fullness may prevent file creation even when bytes remain; unsupported inode counters are unknown.".into()));
        }
    } else {
        out.push(row(
            "fs:pending",
            "Filesystems",
            "?".into(),
            "?".into(),
            None,
            ("PENDING / STALE", Level::Unknown),
            "Storage sampling runs in the background. Samples older than 300s are unavailable."
                .into(),
        ));
    }
    for (key, label) in [
        ("disk_truncated", "Disk coverage"),
        ("network_truncated", "NIC coverage"),
        ("storage_truncated", "Storage coverage"),
    ] {
        if host[key] == true {
            out.push(row(
                &format!("coverage:{key}"),
                label,
                "~ partial".into(),
                "".into(),
                None,
                ("SAMPLING LIMIT", Level::Watch),
                "Sampling limits were reached. Missing devices/accounts are not treated as zero."
                    .into(),
            ));
        }
    }
    if let Some(cycle) = number(&collector["cycle_seconds"]) {
        let interval = number(&collector["target_interval_seconds"]);
        let guide = if interval.is_some_and(|i| i > 0.0 && cycle >= i) {
            ("CYCLE >= INTERVAL", Level::Watch)
        } else if interval.is_some_and(|i| i > 0.0) {
            ("WITHIN INTERVAL", Level::Info)
        } else {
            ("TARGET UNKNOWN", Level::Unknown)
        };
        out.push(row("service:cycle","Collect / target",decimal(Some(cycle)," s"),decimal(interval," s"),ratio(Some(cycle),interval),guide,"Collection cycle duration compared with the configured target interval. Review sustained overruns and collector diagnostics.".into()));
    }
    for (key, label) in [
        ("log_inflight", "Log queue"),
        ("notify_backlog", "Slack queue"),
    ] {
        if let Some(v) = number(&collector[key]) {
            out.push(row(&format!("service:{key}"),label,format!("{v:.0} items"),"".into(),None,("QUEUE, NOT FAILURE",Level::Info),"A nonzero queue may be normal transient work; inspect whether it persists or grows.".into()));
        }
    }
    out.push(row("host:sample","Host sample",format!("{:.0}s ago",(now-stamp).max(0.0)),number(&host["sample_seconds"]).map(|n|format!("{n:.0}s sample")).unwrap_or_else(||"? sample".into()),None,("LIVE HOST",Level::Info),format!("Host: {}\nUptime {} seconds\nHost snapshot age {:.1}s. Rates need two observations; ? is unavailable, never measured zero.\nAll thresholds are review hints, not hardware health diagnoses. Enter shows details and recent trends.",host["hostname"].as_str().unwrap_or("controller"),decimal(number(&host["uptime_seconds"]),""),(now-stamp).max(0.0))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn binary_units_keep_rates_aligned_and_distinguish_zero_from_missing() {
        assert_eq!(
            amount(Some(2.5 * 1024.0_f64.powi(4)), false).trim(),
            "2.5 TiB"
        );
        assert_eq!(amount(Some(1024.0_f64.powi(3)), true).trim(), "1.0 GiB/s");
        assert_eq!(
            amount(Some(1023.99 * 1024.0_f64.powi(2)), false).trim(),
            "1.0 GiB"
        );
        assert_eq!(amount(Some(0.0), true).trim(), "0.0 B/s");
        assert_eq!(amount(Some(f64::NAN), true), "?");
        assert_eq!(amount(Some(-1.0), false), "?");
        assert!(meter(Some(0.5)).contains("0.5%"));
        assert_eq!(
            amount(Some(10.0), true).len(),
            amount(Some(10240.0), true).len()
        );
    }
    #[test]
    fn guides_keep_high_activity_separate_from_capacity_and_pressure() {
        let now = 1000.0;
        let host = json!({"observed_at":now,"cpu_util":99,"cpu_online":4,"load1":8,"memory_some_pct":15,"storage_observed_at":now,
            "disks":[{"name":"nvme0n1","busy_pct":99,"await_ms":0.2}],
            "interfaces":[{"name":"eth0","link_pct":95,"state":"up","errors_ps":0,"drops_ps":0}],
            "filesystems":[{"mount":"/work","used_bytes":900,"total_bytes":1000,"available_bytes":50,"files_total":100,"files_free":2,"files_available":1}]
        });
        let items = rows(&host, &Value::Null, &History::default(), now);
        let get = |key: &str| items.iter().find(|r| r.key == key).unwrap();
        assert_eq!(get("host:cpu").level, Level::Info);
        assert_eq!(get("host:load").level, Level::Watch);
        assert_eq!(get("host:psi:memory").level, Level::High);
        assert_eq!(get("disk:nvme0n1").level, Level::Watch);
        assert!(
            get("disk:nvme0n1")
                .detail
                .contains("not percent of maximum")
        );
        assert_eq!(get("net:eth0").level, Level::Watch);
        assert_eq!(get("fs:/work").level, Level::High);
        assert!((get("fs:/work").percent.unwrap() - 94.7368).abs() < 0.001);
        assert_eq!(get("fs-free:/work").percent, Some(98.0));
        assert_eq!(get("host:swap").level, Level::Unknown);
        let overrun = rows(
            &host,
            &json!({"cycle_seconds":10,"interval_seconds":10,"target_interval_seconds":3}),
            &History::default(),
            now,
        );
        assert_eq!(
            overrun
                .iter()
                .find(|r| r.key == "service:cycle")
                .unwrap()
                .level,
            Level::Watch
        );
        let legacy = rows(
            &host,
            &json!({"cycle_seconds":10,"interval_seconds":10}),
            &History::default(),
            now,
        );
        assert_eq!(
            legacy
                .iter()
                .find(|r| r.key == "service:cycle")
                .unwrap()
                .level,
            Level::Unknown
        );
        assert!(rows(&host, &Value::Null, &History::default(), now + 301.0).is_empty());
        let mut host = host;
        host["observed_at"] = json!(now + 301.0);
        let items = rows(&host, &Value::Null, &History::default(), now + 301.0);
        assert!(items.iter().all(|r| r.key != "fs:/work"));
        assert!(items.iter().any(|r| r.key == "fs:pending"));
    }
    #[test]
    fn recent_history_deduplicates_samples_keeps_gaps_and_compares_latency() {
        let mut history = History::default();
        for n in 0..8 {
            let stamp = 1000.0 + n as f64 * 10.0;
            let host = json!({"hostname":"controller","observed_at":stamp,"disks":[{"name":"sda","await_ms":1.0,"read_bps":0.0}]});
            history.observe(&host, stamp);
            history.observe(&host, stamp);
        }
        assert_eq!(history.samples.len(), 8);
        let host = json!({"hostname":"controller","observed_at":1080.0,"disks":[{"name":"sda","await_ms":3.0,"read_bps":100.0}]});
        history.observe(&host, 1080.0);
        let items = rows(&host, &Value::Null, &history, 1080.0);
        let disk = items.iter().find(|r| r.key == "disk:sda").unwrap();
        assert!(disk.cells[4].contains("2x MED"));
        assert!(disk.detail.contains('·'));
        assert!(disk.detail.contains("9 bins observed"));
        history.observe(&json!({"hostname":"other","observed_at":1080.0}), 1080.0);
        assert_eq!(history.samples.len(), 1);
        assert!(history.prior_median("disks:sda:await_ms", 1080.0).is_none());
        history.observe(&json!({"hostname":"other","observed_at":1000.0}), 1500.0);
        assert!(history.samples.is_empty());
    }
}
