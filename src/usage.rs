use crate::{
    Result,
    model::{Node, json_number, number},
    rules, runtime, scheduler,
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
        let raw = read_freshest("usage.json", candidates).unwrap_or_else(
            || json!({"days":{},"meta":{},"telemetry":{},"sacct_days":{},"sacct_cpu_days":{},"sacct_mem_days":{}}),
        );
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
        for key in [
            "days",
            "meta",
            "telemetry",
            "sacct_days",
            "sacct_cpu_days",
            "sacct_mem_days",
        ] {
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
                let owners: Vec<String> = {
                    let alloc = gpu.users_alloc();
                    if !alloc.is_empty() {
                        alloc.into_iter().map(str::to_string).collect()
                    } else if let Some(user) = gpu.users.first() {
                        vec![user.clone()]
                    } else {
                        continue;
                    }
                };
                let share = dt / owners.len() as f64;
                let fresh = !node.stale && number(&gpu.util).is_some();
                let busy = fresh && number(&gpu.util).is_some_and(|u| u > 5.0);
                let wasted = fresh && gpu.idle_sec.max(gpu.parked_sec) >= minimum;
                for user in &owners {
                    let entry = &mut self.raw["days"][&day][user];
                    if !entry.is_object() {
                        *entry = json!({"alloc":0.0,"busy":0.0,"observed_alloc":0.0});
                    }
                    if entry["observed_alloc"].is_null() {
                        entry["observed_alloc"] = json!(n(entry, "alloc"));
                    }
                    add(entry, "alloc", share);
                    if fresh {
                        add(entry, "observed_alloc", share);
                        if busy {
                            add(entry, "busy", share);
                        }
                        if wasted {
                            add(entry, "waste", share);
                        }
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
            let node_clean = !node.stale && node.error.is_empty();
            for job in &node.jobs {
                if job.user.is_empty() {
                    continue;
                }
                let entry = &mut self.raw["days"][&day][&job.user];
                if !entry.is_object() {
                    *entry = json!({"alloc":0.0,"busy":0.0,"observed_alloc":0.0});
                }
                if job.cpu_count > 0 {
                    add(entry, "cpu_alloc", job.cpu_count as f64 * dt);
                }
                if let Some(mem_mib) = scheduler::mem_mib(&job.mem, job.cpu_count.max(1)) {
                    add(entry, "ram_alloc_gib_sec", (mem_mib / 1024.0) * dt);
                }
                if node_clean
                    && let Some(sample) = job.telemetry.get(&node.name)
                    && json_number(&sample["observed_at"])
                        .is_some_and(|stamp| rules::telemetry_fresh(stamp, now))
                {
                    if let Some(cores) = rules::sample_metric(sample, "cpu_cores") {
                        if job.cpu_count > 0 {
                            add(entry, "cpu_observed_alloc", job.cpu_count as f64 * dt);
                        }
                        add(entry, "cpu_busy", cores * dt);
                    }
                    if let Some(mem_mib) = rules::sample_metric(sample, "mem_current_mib") {
                        add(entry, "ram_gib_sec", (mem_mib / 1024.0) * dt);
                        add(entry, "ram_observed_sec", dt);
                    }
                }
            }
        }
        let keep = runtime::env_seconds("SLURM_GPU_TUI_USAGE_KEEP_DAYS", 30.0, 1.0) as u64;
        let cutoff = date
            .date_naive()
            .checked_sub_days(Days::new(keep))
            .unwrap_or(date.date_naive())
            .to_string();
        for key in [
            "days",
            "meta",
            "telemetry",
            "sacct_days",
            "sacct_cpu_days",
            "sacct_mem_days",
        ] {
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
        let mut cpu_days = BTreeMap::<String, BTreeMap<String, f64>>::new();
        let mut mem_days = BTreeMap::<String, BTreeMap<String, f64>>::new();
        for line in raw.lines() {
            let parts: Vec<_> = line.split('|').collect();
            if parts.len() != 4 || parts[0].is_empty() {
                continue;
            }
            let count = tres_gpus(parts[1]);
            let cpus = tres_cpus(parts[1]);
            let mem_gib = tres_mem_gib(parts[1]);
            if count == 0 && cpus == 0 && mem_gib <= 0.0 {
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
                    let span = end.min(next) - start;
                    let day_key = day.to_string();
                    if count > 0 {
                        *days
                            .entry(day_key.clone())
                            .or_default()
                            .entry(parts[0].into())
                            .or_default() += count as f64 * span;
                    }
                    if cpus > 0 {
                        *cpu_days
                            .entry(day_key.clone())
                            .or_default()
                            .entry(parts[0].into())
                            .or_default() += cpus as f64 * span;
                    }
                    if mem_gib > 0.0 {
                        *mem_days
                            .entry(day_key)
                            .or_default()
                            .entry(parts[0].into())
                            .or_default() += mem_gib * span;
                    }
                }
                start = next;
            }
        }
        self.raw["sacct_days"] = json!(days);
        self.raw["sacct_cpu_days"] = json!(cpu_days);
        self.raw["sacct_mem_days"] = json!(mem_days);
        self.raw["sacct_ts"] = json!(now);
        self.dirty = true;
        Ok(())
    }
}

pub fn tres_gpus(raw: &str) -> u32 {
    scheduler::parse_tres_gpus(raw)
}

pub fn tres_cpus(raw: &str) -> u32 {
    raw.split(',')
        .filter_map(|field| field.trim().split_once('='))
        .find_map(|(k, v)| {
            (k.trim() == "cpu")
                .then(|| v.trim().parse::<u32>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

pub fn tres_mem_gib(raw: &str) -> f64 {
    raw.split(',')
        .filter_map(|field| field.trim().split_once('='))
        .find_map(|(k, v)| {
            (k.trim() == "mem")
                .then(|| scheduler::mem_mib(v.trim(), 1).map(|m| m / 1024.0))
                .flatten()
        })
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Totals {
    pub alloc: f64,
    pub busy: f64,
    pub observed_alloc: f64,
    pub waste: f64,
    pub cpu_alloc: f64,
    pub cpu_busy: f64,
    pub cpu_observed_alloc: f64,
    pub ram_alloc_gib_sec: f64,
    pub ram_used_gib_sec: f64,
    pub ram_observed_sec: f64,
}

pub fn merge(
    raw: &Value,
    keep: impl Fn(&str) -> bool,
) -> (BTreeMap<String, Totals>, BTreeMap<String, Totals>) {
    merge_filtered(raw, keep, "")
}

pub fn merge_filtered(
    raw: &Value,
    keep: impl Fn(&str) -> bool,
    only_user: &str,
) -> (BTreeMap<String, Totals>, BTreeMap<String, Totals>) {
    let mut users = BTreeMap::<String, Totals>::new();
    let mut daily = BTreeMap::<String, Totals>::new();
    let days: BTreeMap<String, ()> = ["days", "sacct_days", "sacct_cpu_days", "sacct_mem_days"]
        .iter()
        .filter_map(|k| raw[*k].as_object())
        .flat_map(|m| m.keys().map(|s| (s.clone(), ())))
        .collect();
    for day in days.keys().filter(|day| keep(day)) {
        let names: BTreeMap<String, ()> = [
            &raw["days"][day],
            &raw["sacct_days"][day],
            &raw["sacct_cpu_days"][day],
            &raw["sacct_mem_days"][day],
        ]
        .iter()
        .filter_map(|v| v.as_object())
        .flat_map(|m| m.keys().map(|s| (s.clone(), ())))
        .collect();
        for user in names
            .keys()
            .filter(|user| only_user.is_empty() || user.as_str() == only_user)
        {
            let sampled = &raw["days"][day][user];
            let alloc = n(sampled, "alloc").max(
                json_number(&raw["sacct_days"][day][user])
                    .filter(|n| *n >= 0.0)
                    .unwrap_or(0.0),
            );
            let busy = n(sampled, "busy");
            let observed = json_number(&sampled["observed_alloc"]).unwrap_or(n(sampled, "alloc"));
            let waste = n(sampled, "waste");
            let cpu_alloc = n(sampled, "cpu_alloc").max(
                json_number(&raw["sacct_cpu_days"][day][user])
                    .filter(|n| *n >= 0.0)
                    .unwrap_or(0.0),
            );
            let cpu_busy = n(sampled, "cpu_busy");
            let cpu_observed_alloc = n(sampled, "cpu_observed_alloc");
            let ram_alloc_gib_sec = n(sampled, "ram_alloc_gib_sec").max(
                json_number(&raw["sacct_mem_days"][day][user])
                    .filter(|n| *n >= 0.0)
                    .unwrap_or(0.0),
            );
            let ram_used_gib_sec = n(sampled, "ram_gib_sec");
            let ram_observed_sec = n(sampled, "ram_observed_sec");
            for entry in [
                users.entry(user.clone()).or_default(),
                daily.entry(day.clone()).or_default(),
            ] {
                entry.alloc += alloc;
                entry.busy += busy;
                entry.observed_alloc += observed;
                entry.waste += waste;
                entry.cpu_alloc += cpu_alloc;
                entry.cpu_busy += cpu_busy;
                entry.cpu_observed_alloc += cpu_observed_alloc;
                entry.ram_alloc_gib_sec += ram_alloc_gib_sec;
                entry.ram_used_gib_sec += ram_used_gib_sec;
                entry.ram_observed_sec += ram_observed_sec;
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
    render_with_options(raw, days, daily, 0, false, "", "")
}

#[derive(Clone, Debug)]
pub struct TableRow {
    pub name: String,
    pub totals: Totals,
    pub cells: Vec<String>,
    pub is_total: bool,
}

#[derive(Clone, Debug)]
pub struct TableData {
    pub title: String,
    pub has_compute: bool,
    pub headers: Vec<String>,
    pub rows: Vec<TableRow>,
    pub note: &'static str,
}

pub fn table(
    raw: &Value,
    days: u64,
    daily: bool,
    sort: usize,
    reverse: bool,
    user_filter: &str,
    search: &str,
) -> TableData {
    let cutoff = window(days);
    let (users, dates) = merge_filtered(raw, |day| day >= cutoff.as_str(), user_filter);
    let has_compute = users.values().chain(dates.values()).any(|t| {
        t.cpu_alloc > 0.0
            || t.cpu_busy > 0.0
            || t.ram_alloc_gib_sec > 0.0
            || t.ram_used_gib_sec > 0.0
    });
    let first = if daily { "day" } else { "user" };
    let title = if has_compute {
        format!("Cluster usage — last {days} days")
    } else {
        format!("GPU-hours — last {days} days")
    };
    let headers: Vec<String> = if has_compute {
        vec![
            first,
            "gpu_alloc",
            "gpu_busy",
            "eff",
            "waste",
            "cpu_alloc",
            "cpu_used",
            "cpu%",
            "ram_used/req",
        ]
    } else {
        vec![first, "alloc", "busy", "eff", "waste"]
    }
    .into_iter()
    .map(Into::into)
    .collect();
    let query = search.to_lowercase();
    let mut rows: Vec<_> = if daily { dates } else { users }
        .into_iter()
        .filter(|(name, _)| query.is_empty() || name.to_lowercase().contains(&query))
        .collect();
    if !daily {
        rows.sort_by(|a, b| {
            match sort % 4 {
                1 => b.1.busy.total_cmp(&a.1.busy),
                2 => b.1.cpu_alloc.total_cmp(&a.1.cpu_alloc),
                3 => {
                    b.1.ram_alloc_gib_sec
                        .total_cmp(&a.1.ram_alloc_gib_sec)
                        .then_with(|| b.1.ram_used_gib_sec.total_cmp(&a.1.ram_used_gib_sec))
                }
                _ => {
                    b.1.alloc
                        .total_cmp(&a.1.alloc)
                        .then_with(|| b.1.cpu_alloc.total_cmp(&a.1.cpu_alloc))
                }
            }
            .then(a.0.cmp(&b.0))
        });
    } else if !sort.is_multiple_of(4) {
        rows.sort_by(|a, b| {
            match sort % 4 {
                1 => b.1.busy.total_cmp(&a.1.busy),
                2 => b.1.cpu_alloc.total_cmp(&a.1.cpu_alloc),
                3 => {
                    b.1.ram_alloc_gib_sec
                        .total_cmp(&a.1.ram_alloc_gib_sec)
                        .then_with(|| b.1.ram_used_gib_sec.total_cmp(&a.1.ram_used_gib_sec))
                }
                _ => a.0.cmp(&b.0),
            }
            .then(a.0.cmp(&b.0))
        });
    }
    if reverse {
        rows.reverse();
    }
    let format_cells = |name: &str, t: &Totals| -> Vec<String> {
        let efficiency = if t.observed_alloc > 0.0 {
            format!("{:.0}%", 100.0 * t.busy / t.observed_alloc)
        } else {
            "?".into()
        };
        if has_compute {
            let cpu_eff = if t.cpu_observed_alloc > 0.0 {
                format!("{:.0}%", 100.0 * t.cpu_busy / t.cpu_observed_alloc)
            } else {
                "?".into()
            };
            let ram = if t.ram_used_gib_sec > 0.0 || t.ram_alloc_gib_sec > 0.0 {
                format!(
                    "{:.0}/{:.0}Gh",
                    t.ram_used_gib_sec / 3600.0,
                    t.ram_alloc_gib_sec / 3600.0
                )
            } else {
                "?".into()
            };
            vec![
                name.into(),
                format!("{:.1}h", t.alloc / 3600.0),
                format!("{:.1}h", t.busy / 3600.0),
                efficiency,
                format!("{:.1}h", t.waste / 3600.0),
                format!("{:.1}h", t.cpu_alloc / 3600.0),
                format!("{:.1}h", t.cpu_busy / 3600.0),
                cpu_eff,
                ram,
            ]
        } else {
            vec![
                name.into(),
                format!("{:.1}h", t.alloc / 3600.0),
                format!("{:.1}h", t.busy / 3600.0),
                efficiency,
                format!("{:.1}h", t.waste / 3600.0),
            ]
        }
    };
    let mut total = Totals::default();
    let mut out_rows = Vec::with_capacity(rows.len() + 1);
    for (name, t) in &rows {
        total.alloc += t.alloc;
        total.busy += t.busy;
        total.observed_alloc += t.observed_alloc;
        total.waste += t.waste;
        total.cpu_alloc += t.cpu_alloc;
        total.cpu_busy += t.cpu_busy;
        total.cpu_observed_alloc += t.cpu_observed_alloc;
        total.ram_alloc_gib_sec += t.ram_alloc_gib_sec;
        total.ram_used_gib_sec += t.ram_used_gib_sec;
        total.ram_observed_sec += t.ram_observed_sec;
        out_rows.push(TableRow {
            name: name.clone(),
            totals: t.clone(),
            cells: format_cells(name, t),
            is_total: false,
        });
    }
    if rows.len() > 1 {
        out_rows.push(TableRow {
            name: "TOTAL".into(),
            cells: format_cells("TOTAL", &total),
            totals: total,
            is_total: true,
        });
    }
    TableData {
        title,
        has_compute,
        headers,
        rows: out_rows,
        note: "alloc = max(sampled, sacct); busy/eff exclude unobserved telemetry.",
    }
}

pub fn render_with_options(
    raw: &Value,
    days: u64,
    daily: bool,
    sort: usize,
    reverse: bool,
    user_filter: &str,
    search: &str,
) -> String {
    let data = table(raw, days, daily, sort, reverse, user_filter, search);
    let mut out = if data.has_compute {
        format!(
            "{}\n{:<16}{:>10}{:>10}{:>8}{:>10}{:>11}{:>11}{:>8}{:>13}\n",
            data.title,
            data.headers[0],
            data.headers[1],
            data.headers[2],
            data.headers[3],
            data.headers[4],
            data.headers[5],
            data.headers[6],
            data.headers[7],
            data.headers[8],
        )
    } else {
        format!(
            "{}\n{:<16}{:>10}{:>10}{:>8}{:>10}\n",
            data.title,
            data.headers[0],
            data.headers[1],
            data.headers[2],
            data.headers[3],
            data.headers[4],
        )
    };
    if data.rows.is_empty() {
        out.push_str("(no matching usage records in window)\n");
    } else {
        for row in &data.rows {
            if data.has_compute {
                out.push_str(&format!(
                    "{:<16}{:>10}{:>10}{:>8}{:>10}{:>11}{:>11}{:>8}{:>13}\n",
                    row.cells[0],
                    row.cells[1],
                    row.cells[2],
                    row.cells[3],
                    row.cells[4],
                    row.cells[5],
                    row.cells[6],
                    row.cells[7],
                    row.cells[8],
                ));
            } else {
                out.push_str(&format!(
                    "{:<16}{:>10}{:>10}{:>8}{:>10}\n",
                    row.cells[0], row.cells[1], row.cells[2], row.cells[3], row.cells[4],
                ));
            }
        }
    }
    out.push_str(&format!("\n{}\n", data.note));
    out
}

pub fn save_json(path: &Path, raw: &Value) -> Result<()> {
    runtime::atomic_write(path, &serde_json::to_vec(raw)?, 0o644, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Gpu, Job};
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
    #[test]
    fn shared_gpu_cpu_and_ram_usage_and_sacct_backfill_are_tracked() {
        let tmp = tempfile::tempdir().unwrap();
        runtime::secure_dir(tmp.path(), 0o700).unwrap();
        let mut store = Store::load(tmp.path().join("usage.json"), &[]);
        let now = runtime::now();
        let node = Node {
            name: "gpu1".into(),
            gpus: vec![Gpu {
                index: "0".into(),
                util: "80".into(),
                alloc_user: "alice".into(),
                alloc_users: vec!["alice".into(), "bob".into()],
                ..Default::default()
            }],
            jobs: vec![
                Job {
                    jobid: "1".into(),
                    user: "alice".into(),
                    cpu_count: 4,
                    mem: "8G".into(),
                    telemetry: BTreeMap::from([(
                        "gpu1".into(),
                        json!({"observed_at": now + 10.0, "cpu_cores": 2.0, "mem_current_mib": 4096.0}),
                    )]),
                    ..Default::default()
                },
                Job {
                    jobid: "2".into(),
                    user: "charlie".into(),
                    cpu_count: 8,
                    mem: "16G".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        store.sample(std::slice::from_ref(&node), now);
        store.sample(&[node], now + 10.0);
        let (users, _) = merge(&store.raw, |_| true);
        assert_eq!(users["alice"].alloc, 5.0);
        assert_eq!(users["bob"].alloc, 5.0);
        assert_eq!(users["alice"].cpu_alloc, 40.0);
        assert_eq!(users["alice"].cpu_busy, 20.0);
        assert_eq!(users["alice"].cpu_observed_alloc, 40.0);
        assert_eq!(users["alice"].ram_alloc_gib_sec, 80.0);
        assert_eq!(users["alice"].ram_used_gib_sec, 40.0);
        assert_eq!(users["charlie"].cpu_alloc, 80.0);

        let date = Local.timestamp_opt(now as i64, 0).single().unwrap();
        let day_start = date.format("%Y-%m-%dT00:00:00").to_string();
        let day_mid = date.format("%Y-%m-%dT00:10:00").to_string();
        store
            .backfill(
                &format!("charlie|cpu=16,mem=32G,node=1|{day_start}|{day_mid}\n"),
                now + 3600.0,
            )
            .unwrap();
        let (backfilled, _) = merge(&store.raw, |_| true);
        assert!((backfilled["charlie"].cpu_alloc - 9600.0).abs() < 1e-6);
        assert!((backfilled["charlie"].ram_alloc_gib_sec - 19200.0).abs() < 1e-6);
        assert!(render(&store.raw, 7, false).contains("cpu_alloc"));
    }
    #[test]
    fn table_and_render_with_options_cover_five_and_nine_column_modes() {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let yesterday = (Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let gpu_only = json!({
            "days": {
                &today: {
                    "alice": {"alloc": 7200, "busy": 1800, "observed_alloc": 7200},
                    "bob": {"alloc": 3600, "busy": 3600, "observed_alloc": 3600}
                },
                &yesterday: {
                    "alice": {"alloc": 3600, "busy": 900, "observed_alloc": 3600}
                }
            }
        });
        let compact = table(&gpu_only, 7, false, 0, false, "", "");
        assert!(!compact.has_compute);
        assert_eq!(compact.headers, ["user", "alloc", "busy", "eff", "waste"]);
        assert_eq!(compact.rows.len(), 3);
        assert_eq!(compact.rows[0].name, "alice");
        assert_eq!(compact.rows[1].name, "bob");
        assert!(compact.rows[2].is_total);
        assert_eq!(compact.rows[2].cells[1], "4.0h");
        let reversed = table(&gpu_only, 7, false, 1, true, "", "");
        assert_eq!(reversed.rows[0].name, "alice");
        assert_eq!(reversed.rows[1].name, "bob");
        let daily_gpu = table(&gpu_only, 7, true, 0, false, "", "");
        assert_eq!(daily_gpu.headers[0], "day");
        assert_eq!(daily_gpu.rows[0].name, yesterday);
        assert_eq!(daily_gpu.rows[1].name, today);
        assert!(daily_gpu.rows[2].is_total);

        let compute = json!({
            "days": {
                &today: {
                    "alice": {
                        "alloc": 7200, "busy": 3600, "observed_alloc": 7200,
                        "cpu_alloc": 3600, "cpu_busy": 1800, "cpu_observed_alloc": 3600,
                        "ram_alloc_gib_sec": 3600.0, "ram_gib_sec": 1800.0
                    },
                    "bob": {
                        "alloc": 3600, "busy": 3600, "observed_alloc": 3600,
                        "cpu_alloc": 14400, "cpu_busy": 7200, "cpu_observed_alloc": 14400,
                        "ram_alloc_gib_sec": 7200.0, "ram_gib_sec": 3600.0
                    }
                }
            }
        });
        let by_cpu = table(&compute, 7, false, 2, false, "", "");
        assert!(by_cpu.has_compute);
        assert_eq!(
            by_cpu.headers,
            [
                "user",
                "gpu_alloc",
                "gpu_busy",
                "eff",
                "waste",
                "cpu_alloc",
                "cpu_used",
                "cpu%",
                "ram_used/req"
            ]
        );
        assert_eq!(by_cpu.rows[0].name, "bob");
        assert_eq!(by_cpu.rows[0].cells[5], "4.0h");
        assert_eq!(by_cpu.rows[0].cells[8], "1/2Gh");
        assert_eq!(by_cpu.rows[2].name, "TOTAL");
        let rendered = render_with_options(&compute, 7, false, 3, false, "", "alice");
        assert!(rendered.contains("alice"));
        assert!(!rendered.contains("bob"));
        assert!(!rendered.contains("TOTAL"));
        let empty = render_with_options(&compute, 7, false, 0, false, "", "nobody");
        assert!(empty.contains("(no matching usage records in window)"));
    }
}
