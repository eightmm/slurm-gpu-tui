use crate::{
    Result, cli,
    process::{self, Program},
    rules, runtime, scheduler,
};
use std::{cmp::Ordering, collections::BTreeMap, path::PathBuf, time::Duration};

#[derive(Clone, Default)]
pub struct Inventory {
    pub state: String,
    pub partition: String,
    pub gpus: u32,
    pub cpus: u32,
}
#[derive(Default)]
pub struct Allocation {
    pub gpu: BTreeMap<String, BTreeMap<String, u32>>,
    pub cpu: BTreeMap<String, BTreeMap<String, u32>>,
    pub pending: BTreeMap<String, BTreeMap<String, u32>>,
    pub ends: BTreeMap<String, f64>,
}

pub fn parse_inventory(raw: &str, cpu: &str) -> BTreeMap<String, Inventory> {
    let mut out = BTreeMap::<String, Inventory>::new();
    let mut partitions = BTreeMap::<String, Vec<String>>::new();
    let mut members = BTreeMap::<String, Vec<String>>::new();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if p.len() != 4 {
            continue;
        }
        let name = p[0].split('.').next().unwrap_or("");
        if !runtime::valid_host(name) {
            continue;
        }
        let part = p[3].trim_end_matches('*');
        let entry = out.entry(name.into()).or_default();
        entry.state = p[1].into();
        entry.gpus = entry.gpus.max(scheduler::gpu_count(p[2]));
        let parts = partitions.entry(name.into()).or_default();
        if !parts.iter().any(|p| p == part) {
            parts.push(part.into());
        }
        members.entry(part.into()).or_default().push(name.into());
    }
    let gpu_only: Vec<_> = members
        .iter()
        .filter(|(_, nodes)| !nodes.is_empty() && nodes.iter().all(|n| out[n].gpus > 0))
        .map(|(p, _)| p.clone())
        .collect();
    for (name, parts) in partitions {
        out.get_mut(&name).unwrap().partition = parts
            .iter()
            .find(|p| gpu_only.contains(p))
            .or_else(|| parts.first())
            .cloned()
            .unwrap_or_default();
    }
    for line in cpu.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if p.len() != 2 {
            continue;
        }
        let name = p[0].split('.').next().unwrap_or("");
        if !runtime::valid_host(name) {
            continue;
        }
        if let Some(count) = p[1].rsplit('/').next().and_then(|s| s.parse::<u32>().ok()) {
            out.entry(name.into()).or_default().cpus = count;
        }
    }
    out
}

pub fn parse_allocations(raw: &str) -> Allocation {
    let mut out = Allocation::default();
    for line in raw.lines() {
        let p: Vec<_> = line.split('|').map(str::trim).collect();
        if p.len() != 9
            || !scheduler::valid_job_id(p[0])
            || p[2].is_empty()
            || p[2].len() > 256
            || p[2].chars().any(char::is_control)
        {
            continue;
        }
        if p[3] == "PENDING" {
            *out.pending
                .entry(p[2].into())
                .or_default()
                .entry(p[1].into())
                .or_default() += 1;
            continue;
        }
        if p[3] != "RUNNING" {
            continue;
        }
        let Some(nodes) = scheduler::expand_nodes(p[8]) else {
            continue;
        };
        if nodes.is_empty() {
            continue;
        }
        let per_node = scheduler::gpu_count(p[6]);
        let cpu = p[5].parse::<u32>().unwrap_or(0);
        let count = nodes.len() as u32;
        let end = rules::parse_timestamp(p[7]);
        for (index, node) in nodes.iter().enumerate() {
            let node = node.split('.').next().unwrap_or(node).to_string();
            if per_node > 0 {
                let value = out
                    .gpu
                    .entry(p[2].into())
                    .or_default()
                    .entry(node.clone())
                    .or_default();
                *value = value.saturating_add(per_node);
                if let Some(end) = end {
                    let earliest = out.ends.entry(node.clone()).or_insert(end);
                    *earliest = earliest.min(end);
                }
            }
            if cpu > 0 {
                let allocated = cpu / count + u32::from((index as u32) < cpu % count);
                let value = out
                    .cpu
                    .entry(p[2].into())
                    .or_default()
                    .entry(node)
                    .or_default();
                *value = value.saturating_add(allocated);
            }
        }
    }
    out
}

pub fn natural(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (0, 0);
    let (a, b) = (a.as_bytes(), b.as_bytes());
    while ai < a.len() && bi < b.len() {
        if a[ai].is_ascii_digit() && b[bi].is_ascii_digit() {
            let (mut ae, mut be) = (ai, bi);
            while ae < a.len() && a[ae].is_ascii_digit() {
                ae += 1;
            }
            while be < b.len() && b[be].is_ascii_digit() {
                be += 1;
            }
            let mut az = ai;
            while az + 1 < ae && a[az] == b'0' {
                az += 1;
            }
            let mut bz = bi;
            while bz + 1 < be && b[bz] == b'0' {
                bz += 1;
            }
            let comparison = (ae - az).cmp(&(be - bz)).then(a[az..ae].cmp(&b[bz..be]));
            if comparison != Ordering::Equal {
                return comparison;
            }
            ai = ae;
            bi = be;
        } else {
            let comparison = a[ai].cmp(&b[bi]);
            if comparison != Ordering::Equal {
                return comparison;
            }
            ai += 1;
            bi += 1;
        }
    }
    a.len().cmp(&b.len())
}

pub fn eta(end: f64, now: f64) -> String {
    let seconds = (end - now).max(0.0) as u64;
    if seconds >= 86400 {
        format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}
fn ordered(rows: &BTreeMap<String, BTreeMap<String, u32>>) -> Vec<String> {
    let mut users: Vec<_> = rows.keys().cloned().collect();
    users.sort_by(|a, b| {
        rows[b]
            .values()
            .map(|n| *n as u64)
            .sum::<u64>()
            .cmp(&rows[a].values().map(|n| *n as u64).sum::<u64>())
            .then(a.cmp(b))
    });
    users
}
fn sum(rows: &BTreeMap<String, BTreeMap<String, u32>>, node: &str) -> u64 {
    rows.values()
        .map(|r| *r.get(node).unwrap_or(&0) as u64)
        .sum()
}
struct TableContext<'a> {
    inventory: &'a BTreeMap<String, Inventory>,
    gpu: bool,
    ends: &'a BTreeMap<String, f64>,
    now: f64,
}
fn table(
    out: &mut String,
    title: &str,
    nodes: &[String],
    users: &[String],
    rows: &BTreeMap<String, BTreeMap<String, u32>>,
    context: TableContext<'_>,
) {
    let TableContext {
        inventory,
        gpu,
        ends,
        now,
    } = context;
    if nodes.is_empty() {
        return;
    }
    let width = nodes
        .iter()
        .map(|s| s.len() + 2)
        .max()
        .unwrap_or(9)
        .clamp(9, 40);
    out.push_str(&format!("\n─── {title} ───\n{:>13}|", "Part"));
    for node in nodes {
        let partition = &inventory[node].partition;
        let short = if partition.len() < width {
            partition.clone()
        } else {
            partition
                .split('_')
                .next()
                .unwrap_or(partition)
                .chars()
                .take(width - 2)
                .collect::<String>()
        };
        out.push_str(&format!("{short:>width$}|"));
    }
    out.push_str("     Total\n");
    out.push_str(&format!("{:>13}|", "User"));
    for node in nodes {
        let mark = if ["down", "drain"]
            .iter()
            .any(|s| inventory[node].state.to_lowercase().contains(s))
        {
            "!"
        } else {
            ""
        };
        out.push_str(&format!(
            "{:>width$}|",
            format!("{}{mark}", node.to_uppercase())
        ));
    }
    out.push_str("     Total\n");
    for user in users {
        let total = rows
            .get(user)
            .map(|r| {
                nodes
                    .iter()
                    .filter_map(|node| r.get(node))
                    .map(|n| *n as u64)
                    .sum::<u64>()
            })
            .unwrap_or(0);
        if total == 0 {
            continue;
        }
        out.push_str(&format!("{:>13}|", cli::terminal_safe(user)));
        for node in nodes {
            let value = rows
                .get(user)
                .and_then(|r| r.get(node))
                .copied()
                .filter(|v| *v > 0)
                .map(|v| v.to_string())
                .unwrap_or_default();
            out.push_str(&format!("{value:>width$}|"));
        }
        out.push_str(&format!("{total:>10}\n"));
    }
    out.push_str(&format!("{:>13}|", "Total"));
    let mut used = 0;
    let mut capacity = 0u64;
    for node in nodes {
        let allocated = sum(rows, node);
        let total = if gpu {
            inventory[node].gpus
        } else {
            inventory[node].cpus
        };
        used += allocated;
        capacity += total as u64;
        out.push_str(&format!("{:>width$}|", format!("{allocated}/{total}")));
    }
    out.push_str(&format!("{used}/{capacity}\n"));
    if gpu && nodes.iter().any(|n| ends.contains_key(n)) {
        out.push_str(&format!("{:>13}|", "Next free"));
        for node in nodes {
            let value = ends
                .get(node)
                .filter(|_| sum(rows, node) > 0)
                .map(|end| eta(*end, now))
                .unwrap_or_default();
            out.push_str(&format!("{value:>width$}|"));
        }
        out.push('\n');
    }
}

pub fn render(
    inventory: &BTreeMap<String, Inventory>,
    allocation: &Allocation,
    now: f64,
) -> String {
    let mut gpu_nodes: Vec<_> = inventory
        .iter()
        .filter(|(_, n)| n.gpus > 0)
        .map(|(name, _)| name.clone())
        .collect();
    gpu_nodes.sort_by(|a, b| natural(a, b));
    let gpu_users = ordered(&allocation.gpu);
    let mut users = gpu_users.clone();
    users.extend(
        ordered(&allocation.cpu)
            .into_iter()
            .filter(|u| !gpu_users.contains(u)),
    );
    let mut out = String::new();
    table(
        &mut out,
        "GPU allocation",
        &gpu_nodes,
        &gpu_users,
        &allocation.gpu,
        TableContext {
            inventory,
            gpu: true,
            ends: &allocation.ends,
            now,
        },
    );
    for (category, label) in [
        (0, "CPU Threads — GPU nodes"),
        (1, "CPU Threads — CPU nodes"),
        (2, "CPU Threads — Other nodes"),
    ] {
        let mut nodes: Vec<_> = inventory
            .iter()
            .filter(|(name, n)| {
                n.cpus > 0
                    && match category {
                        0 => n.gpus > 0,
                        1 => n.gpus == 0 && name.starts_with("cpu"),
                        _ => n.gpus == 0 && !name.starts_with("cpu"),
                    }
            })
            .map(|(name, _)| name.clone())
            .collect();
        nodes.sort_by(|a, b| natural(a, b));
        table(
            &mut out,
            label,
            &nodes,
            &users,
            &allocation.cpu,
            TableContext {
                inventory,
                gpu: false,
                ends: &BTreeMap::new(),
                now,
            },
        );
    }
    if !allocation.pending.is_empty() {
        out.push_str("\n─── Pending jobs ───\n");
        for user in ordered(&allocation.pending) {
            let row = &allocation.pending[&user];
            out.push_str(&format!(
                "{user:>13}: {} ({})\n",
                row.values().map(|n| *n as u64).sum::<u64>(),
                row.iter()
                    .map(|(p, n)| format!("{}:{n}", cli::terminal_safe(p)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let gpu_used = gpu_nodes
        .iter()
        .map(|n| sum(&allocation.gpu, n))
        .sum::<u64>();
    let gpu_total = inventory.values().map(|n| n.gpus as u64).sum::<u64>();
    let cpu_used = inventory
        .keys()
        .map(|n| sum(&allocation.cpu, n))
        .sum::<u64>();
    let cpu_total = inventory.values().map(|n| n.cpus as u64).sum::<u64>();
    out.push_str(&format!("\nGPU {gpu_used}/{gpu_total} ({:.0}%)   CPU {cpu_used}/{cpu_total} ({:.0}%)\nAllocation, not measured utilization. Next free is an estimated job end, not a reservation.\n",if gpu_total>0{gpu_used as f64/gpu_total as f64*100.0}else{0.0},if cpu_total>0{cpu_used as f64/cpu_total as f64*100.0}else{0.0}));
    out
}

fn conf_fallback(inventory: &mut BTreeMap<String, Inventory>) {
    let mut candidates = std::env::var_os("SLURM_CONF")
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if let Ok(raw) = process::run(
        Program::Scontrol,
        &["show", "config"],
        Duration::from_secs(10),
    ) {
        for line in raw.lines() {
            if let Some(path) = line
                .strip_prefix("SLURM_CONF")
                .and_then(|s| s.split_once('=').map(|(_, v)| v.trim()))
            {
                candidates.push(path.into());
            }
        }
    }
    candidates.extend(
        [
            "/etc/slurm/slurm.conf",
            "/etc/slurm-llnl/slurm.conf",
            "/usr/local/etc/slurm.conf",
            "/opt/slurm/etc/slurm.conf",
        ]
        .map(PathBuf::from),
    );
    let config = candidates.iter().find_map(|p| {
        std::fs::canonicalize(p)
            .ok()
            .and_then(|p| runtime::read_regular(&p, 4 * 1024 * 1024, None).ok())
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
    });
    let Some(config) = config else {
        return;
    };
    let mut defaults = BTreeMap::new();
    for line in config.lines() {
        let fields: BTreeMap<_, _> = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|s| {
                s.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        let Some(names) = fields.get("NodeName") else {
            continue;
        };
        if names == "DEFAULT" {
            defaults.extend(fields);
            continue;
        }
        let gres = fields
            .get("Gres")
            .or_else(|| defaults.get("Gres"))
            .map(String::as_str)
            .unwrap_or("");
        let gpus = scheduler::gpu_count(gres);
        if gpus == 0 {
            continue;
        }
        for name in scheduler::expand_nodes(names).unwrap_or_default() {
            inventory.entry(name).or_default().gpus = gpus;
        }
    }
}

pub fn run() -> Result<()> {
    let (queue, nodes, cpu) = std::thread::scope(|scope| {
        let queue = scope.spawn(|| {
            process::run(
                Program::Squeue,
                &["--noheader", "-o", "%i|%P|%u|%T|%M|%C|%b|%e|%R"],
                Duration::from_secs(12),
            )
        });
        let nodes = scope.spawn(|| {
            process::run(
                Program::Sinfo,
                &["-N", "--noheader", "-o", "%n|%T|%G|%P"],
                Duration::from_secs(12),
            )
        });
        let cpu = process::run(
            Program::Sinfo,
            &["-N", "-h", "-o", "%N|%C"],
            Duration::from_secs(12),
        );
        (
            queue
                .join()
                .unwrap_or_else(|_| Err("queue worker failed".into())),
            nodes
                .join()
                .unwrap_or_else(|_| Err("inventory worker failed".into())),
            cpu,
        )
    });
    let queue = queue?;
    let mut inventory = parse_inventory(&nodes?, &cpu?);
    if !inventory.values().any(|n| n.gpus > 0) {
        conf_fallback(&mut inventory);
    }
    if inventory.is_empty() {
        return Err("no scheduler nodes found".into());
    }
    print!(
        "{}",
        render(&inventory, &parse_allocations(&queue), runtime::now())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multi_node_gpu_counts_cpu_remainder_and_pending_are_distinct() {
        let allocation = parse_allocations(
            "42|gpu|alice|RUNNING|00:01|5|gpu:h100:2|2026-07-01T12:00:00|gpu[1-2]\n43|gpu|bob|PENDING|00:00|4|gpu:8|N/A|(Priority)",
        );
        assert_eq!(allocation.gpu["alice"]["gpu1"], 2);
        assert_eq!(allocation.gpu["alice"]["gpu2"], 2);
        assert_eq!(allocation.cpu["alice"]["gpu1"], 3);
        assert_eq!(allocation.cpu["alice"]["gpu2"], 2);
        assert_eq!(allocation.pending["bob"]["gpu"], 1);
    }
    #[test]
    fn gpu_controller_and_duplicate_partitions_keep_one_capacity() {
        let inventory = parse_inventory(
            "master|idle|gpu:h100:2|cpu*\nmaster|idle|gpu:h100:2|gpu\ncpu1|idle|(null)|cpu",
            "master|0/8/0/8\ncpu1|0/16/0/16",
        );
        assert_eq!(inventory["master"].gpus, 2);
        assert_eq!(inventory["master"].partition, "gpu");
        assert_eq!(inventory["cpu1"].cpus, 16);
        assert_eq!(eta(90000.0, 0.0), "1d1h");
        assert_eq!(eta(-1.0, 0.0), "0h00m");
        assert_eq!(natural("gpu2", "gpu10"), Ordering::Less);
    }
}
