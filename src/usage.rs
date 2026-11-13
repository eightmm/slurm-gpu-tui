use crate::{
    Result,
    model::{Node, json_number, number},
    rules, runtime,
};
use chrono::{Days, Local, TimeZone};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn read_freshest(name: &str, candidates: &[PathBuf]) -> Option<Value> {
    let mut files: Vec<_> = candidates
        .iter()
        .filter_map(|d| {
            let p = d.join(name);
            let st = std::fs::symlink_metadata(&p).ok()?;
            st.is_file().then_some((p, st.modified().ok()?))
        })
        .collect();
    files.sort_by_key(|(_, stamp)| std::cmp::Reverse(*stamp));
    for (path, _) in files {
        if let Ok((bytes, _)) = runtime::read_regular(&path, 64 * 1024 * 1024, None)
            && let Ok(raw) = serde_json::from_slice::<Value>(&bytes)
            && raw.is_object()
        {
            return Some(raw);
        }
    }
    None
}

fn add(raw: &mut Value, key: &str, value: f64) {
    let old = json_number(&raw[key]).filter(|v| *v >= 0.0).unwrap_or(0.0);
    raw[key] = json!(old + value);
}
fn n(raw: &Value, key: &str) -> f64 {
    json_number(&raw[key]).filter(|v| *v >= 0.0).unwrap_or(0.0)
}

pub struct Store {
    pub raw: Value,
    pub path: PathBuf,
    pub dirty: bool,
    previous: Option<f64>,
    last_save: f64,
}
impl Store {
    pub fn load(path: PathBuf, candidates: &[PathBuf]) -> Self {
        let raw = read_freshest("usage.json", candidates)
            .unwrap_or_else(|| json!({"days":{},"meta":{},"telemetry":{},"sacct_days":{}}));
        Self {
            raw,
            path,
            dirty: false,
            previous: None,
            last_save: 0.0,
        }
    }
    pub fn sample(&mut self, nodes: &[Node], now: f64) {
        let previous = self.previous.replace(now);
        let Some(dt) = previous
            .map(|p| now - p)
            .filter(|dt| *dt > 0.0 && *dt <= 60.0)
        else {
            return;
        };
        let Some(date) = Local.timestamp_opt(now as i64, 0).single() else {
            return;
        };
        let day = date.format("%Y-%m-%d").to_string();
        for key in ["days", "meta", "telemetry", "sacct_days"] {
            if !self.raw[key].is_object() {
                self.raw[key] = json!({});
            }
        }
        add(&mut self.raw["meta"], &day, dt);
        if !self.raw["days"][&day].is_object() {
            self.raw["days"][&day] = json!({});
        }
        if !self.raw["telemetry"][&day].is_object() {
            self.raw["telemetry"][&day] = json!({});
        }
        let minimum = runtime::env_seconds("SLURM_GPU_TUI_WASTE_MIN_SEC", 600.0, 0.0) as u64;
        for node in nodes {
            for gpu in &node.gpus {
                let user = if !gpu.alloc_user.is_empty() {
                    &gpu.alloc_user
                } else if let Some(user) = gpu.users.first() {
                    user
                } else {
                    continue;
                };
                let entry = &mut self.raw["days"][&day][user];
                if !entry.is_object() {
                    *entry = json!({"alloc":0.0,"busy":0.0,"observed_alloc":0.0});
                }
                if entry["observed_alloc"].is_null() {
                    entry["observed_alloc"] = json!(n(entry, "alloc"));
                }
                add(entry, "alloc", dt);
                let fresh = !node.stale && number(&gpu.util).is_some();
                if fresh {
                    add(entry, "observed_alloc", dt);
                    if number(&gpu.util).is_some_and(|u| u > 5.0) {
                        add(entry, "busy", dt);
                    }
                    if gpu.idle_sec.max(gpu.parked_sec) >= minimum {
                        add(entry, "waste", dt);
                    }
                }
                add(
                    &mut self.raw["telemetry"][&day],
                    if fresh {
                        "fresh_gpu_sec"
                    } else {
                        "stale_gpu_sec"
                    },
                    dt,
                );
            }
        }
        let keep = runtime::env_seconds("SLURM_GPU_TUI_USAGE_KEEP_DAYS", 30.0, 1.0) as u64;
        let cutoff = date
            .date_naive()
            .checked_sub_days(Days::new(keep))
            .unwrap_or(date.date_naive())
            .to_string();
        for key in ["days", "meta", "telemetry", "sacct_days"] {
            if let Some(days) = self.raw[key].as_object_mut() {
                days.retain(|day, _| day >= &cutoff);
            }
        }
        self.dirty = true;
    }
    pub fn checkpoint(&mut self, now: f64, force: bool) -> Result<bool> {
        if !self.dirty
            || !force
                && self.last_save > 0.0
                && now - self.last_save
                    < runtime::env_seconds("SLURM_GPU_TUI_USAGE_SAVE_SEC", 30.0, 0.1)
        {
            return Ok(false);
        }
        runtime::atomic_write(&self.path, &serde_json::to_vec(&self.raw)?, 0o644, true)?;
        self.last_save = now;
        self.dirty = false;
        Ok(true)
    }
    pub fn backfill(&mut self, raw: &str, now: f64) -> Result<()> {
        let Some(date) = Local.timestamp_opt(now as i64, 0).single() else {
            return Err("invalid backfill time".into());
        };
        let cutoff = date
            .date_naive()
            .checked_sub_days(Days::new(runtime::env_seconds(
                "SLURM_GPU_TUI_USAGE_KEEP_DAYS",
                30.0,
                1.0,
            ) as u64))
            .unwrap_or(date.date_naive())
            .to_string();
        let mut days = BTreeMap::<String, BTreeMap<String, f64>>::new();
        for line in raw.lines() {
            let parts: Vec<_> = line.split('|').collect();
            if parts.len() != 4 || parts[0].is_empty() {
                continue;
            }
            let count = tres_gpus(parts[1]);
            if count == 0 {
                continue;
            }
            let Some(mut start) = rules::parse_timestamp(parts[2]) else {
                continue;
            };
            let end = rules::parse_timestamp(parts[3]).unwrap_or(now).min(now);
            if end <= start {
                continue;
            }
            // Clamp old jobs before walking days, bounding work to the retention window.
            let earliest = Local
                .timestamp_opt(now as i64, 0)
                .single()
                .and_then(|d| {
                    d.checked_sub_days(Days::new(
                        31 + runtime::env_seconds("SLURM_GPU_TUI_USAGE_KEEP_DAYS", 30.0, 1.0)
                            as u64,
                    ))
                })
                .map(|d| d.timestamp() as f64)
                .unwrap_or(start);
            start = start.max(earliest);
            while start < end {
                let Some(date) = Local.timestamp_opt(start as i64, 0).single() else {
                    break;
                };
                let day = date.date_naive();
                let Some(next) = day
                    .succ_opt()
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .and_then(|d| Local.from_local_datetime(&d).earliest())
                    .map(|d| d.timestamp() as f64)
                else {
                    break;
                };
                if next <= start {
                    break;
                }
                if day.to_string() >= cutoff {
                    *days
                        .entry(day.to_string())
                        .or_default()
                        .entry(parts[0].into())
                        .or_default() += count as f64 * (end.min(next) - start);
                }
                start = next;
            }
        }
        self.raw["sacct_days"] = json!(days);
        self.raw["sacct_ts"] = json!(now);
        self.dirty = true;
        Ok(())
    }
}

pub fn tres_gpus(raw: &str) -> u32 {
    let mut typed = 0u32;
    for field in raw.split(',') {
        if let Some((key, value)) = field.split_once('=')
            && let Ok(value) = value.parse::<u32>()
        {
            if key == "gres/gpu" {
                return value;
            }
            if key.starts_with("gres/gpu:") {
                typed = typed.saturating_add(value);
            }
        }
    }
    typed
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Totals {
    pub alloc: f64,
    pub busy: f64,
    pub observed_alloc: f64,
    pub waste: f64,
}

pub fn merge(
    raw: &Value,
    keep: impl Fn(&str) -> bool,
) -> (BTreeMap<String, Totals>, BTreeMap<String, Totals>) {
    let mut users = BTreeMap::<String, Totals>::new();
    let mut daily = BTreeMap::<String, Totals>::new();
    let days: BTreeMap<String, ()> = ["days", "sacct_days"]
        .iter()
        .filter_map(|k| raw[*k].as_object())
        .flat_map(|m| m.keys().map(|s| (s.clone(), ())))
        .collect();
    for day in days.keys().filter(|day| keep(day)) {
        let names: BTreeMap<String, ()> = [&raw["days"][day], &raw["sacct_days"][day]]
            .iter()
            .filter_map(|v| v.as_object())
            .flat_map(|m| m.keys().map(|s| (s.clone(), ())))
            .collect();
        for user in names.keys() {
            let sampled = &raw["days"][day][user];
            let alloc = n(sampled, "alloc").max(
                json_number(&raw["sacct_days"][day][user])
                    .filter(|n| *n >= 0.0)
                    .unwrap_or(0.0),
            );
            let busy = n(sampled, "busy");
            let observed = json_number(&sampled["observed_alloc"]).unwrap_or(n(sampled, "alloc"));
            let waste = n(sampled, "waste");
            for entry in [
                users.entry(user.clone()).or_default(),
                daily.entry(day.clone()).or_default(),
            ] {
                entry.alloc += alloc;
                entry.busy += busy;
                entry.observed_alloc += observed;
                entry.waste += waste;
            }
        }
    }
    (users, daily)
}

pub fn window(days: u64) -> String {
    Local::now()
        .date_naive()
        .checked_sub_days(Days::new(days))
        .unwrap_or_else(|| Local::now().date_naive())
        .to_string()
}
pub fn read() -> Option<Value> {
    read_freshest("usage.json", &runtime::state_candidates())
}

pub fn render(raw: &Value, days: u64, daily: bool) -> String {
    let cutoff = window(days);
    let (users, dates) = merge(raw, |day| day >= cutoff.as_str());
    let mut out = format!(
        "GPU-hours — last {days} days\n{:<16}{:>10}{:>10}{:>8}{:>10}\n",
        if daily { "day" } else { "user" },
        "alloc",
        "busy",
        "eff",
        "waste"
    );
    let mut rows: Vec<_> = if daily {
        dates.into_iter().collect()
    } else {
        users.into_iter().collect()
    };
    if !daily {
        rows.sort_by(|a, b| b.1.alloc.total_cmp(&a.1.alloc).then(a.0.cmp(&b.0)));
    }
    for (name, t) in rows {
        let efficiency = if t.observed_alloc > 0.0 {
            format!("{:.0}%", 100.0 * t.busy / t.observed_alloc)
        } else {
            "?".into()
        };
        out.push_str(&format!(
            "{name:<16}{:>9.1}h{:>9.1}h{efficiency:>8}{:>9.1}h\n",
            t.alloc / 3600.0,
            t.busy / 3600.0,
            t.waste / 3600.0
        ));
    }
    out.push_str("\nalloc = max(sampled, sacct); busy/eff exclude unobserved telemetry.\n");
    out
}

pub fn save_json(path: &Path, raw: &Value) -> Result<()> {
    runtime::atomic_write(path, &serde_json::to_vec(raw)?, 0o644, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Gpu;
    #[test]
    fn merging_keeps_efficiency_on_the_observed_denominator() {
        let raw = json!({"days":{"2026-07-01":{"alice":{"alloc":100,"busy":60,"observed_alloc":80}}},"sacct_days":{"2026-07-01":{"alice":900,"bob":12}}});
        let (users, _) = merge(&raw, |_| true);
        assert_eq!(users["alice"].alloc, 900.0);
        assert_eq!(users["alice"].observed_alloc, 80.0);
        assert_eq!(users["alice"].busy, 60.0);
        assert_eq!(users["bob"].alloc, 12.0);
    }
    #[test]
    fn stale_samples_count_alloc_but_not_busy_and_failed_checkpoint_stays_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        runtime::secure_dir(tmp.path(), 0o700).unwrap();
        let mut store = Store::load(tmp.path().join("missing/usage.json"), &[]);
        let node = Node {
            stale: true,
            gpus: vec![Gpu {
                alloc_user: "alice".into(),
                util: "99".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let now = runtime::now();
        store.sample(std::slice::from_ref(&node), now);
        store.sample(&[node], now + 3.0);
        let (users, _) = merge(&store.raw, |_| true);
        assert_eq!(users["alice"].alloc, 3.0);
        assert_eq!(users["alice"].busy, 0.0);
        assert_eq!(users["alice"].observed_alloc, 0.0);
        assert!(store.checkpoint(3.0, false).is_err());
        assert!(store.dirty);
        store.path = tmp.path().join("usage.json");
        assert!(store.checkpoint(4.0, false).unwrap());
        assert!(!store.dirty);
    }
}
