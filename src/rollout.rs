use crate::{Result, collector, packaging, process, runtime, update};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new() -> Result<Self> {
        let base = if runtime::euid() == 0 && Path::new("/run").is_dir() {
            PathBuf::from("/run")
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "sgpu-install-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    name: String,
    mode: &'static str,
}

fn valid_target(name: &str) -> bool {
    runtime::valid_host(name) && name.as_bytes()[0].is_ascii_alphanumeric()
}

fn inventory(raw: &str) -> Result<Vec<Node>> {
    let mut nodes = BTreeMap::new();
    for line in raw.lines().filter(|s| !s.trim().is_empty()) {
        let (name, gres) = line
            .trim()
            .split_once('|')
            .ok_or("invalid Slurm node inventory")?;
        if !valid_target(name)
            || gres.contains('|')
            || nodes.len() >= 4096 && !nodes.contains_key(name)
        {
            return Err("invalid or oversized Slurm node inventory".into());
        }
        let gpu = crate::scheduler::gpu_count(gres) > 0;
        let node = nodes.entry(name.to_owned()).or_insert(Node {
            name: name.into(),
            mode: "cpu",
        });
        if gpu {
            node.mode = "gpu";
        }
    }
    Ok(nodes.into_values().collect())
}

fn release(config: &collector::Config, node: &Node) -> Option<[u64; 3]> {
    ["gpu", "cpu"].into_iter().find_map(|kind| {
        collector::read_payload(config, &node.name, kind, runtime::now())
            .ok()
            .and_then(|(p, _)| update::version(&p.release))
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn ssh_user(user: &str) -> std::result::Result<String, String> {
    if user.is_empty()
        || user.len() > 64
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        || user.starts_with('-')
    {
        Err("invalid node SSH user".into())
    } else {
        Ok(user.into())
    }
}

fn ssh_command(node: &Node, user: &str, agents: &Path) -> Result<Command> {
    if !valid_target(&node.name) {
        return Err("invalid node destination".into());
    }
    ssh_user(user)?;
    let script = format!(
        "set -eu\n[ \"$(id -u)\" = 0 ] || {{ echo 'node bootstrap requires root' >&2; exit 1; }}\numask 077\nsgpu_stage=$(mktemp -d)\ntrap 'rm -rf -- \"$sgpu_stage\"' EXIT HUP INT TERM\ntar -xf - -C \"$sgpu_stage\"\n\"$sgpu_stage/sgpu-install\" node-bootstrap --bin-dir \"$sgpu_stage\" --agent-dir {} --fallback-mode {}\n",
        quote(&agents.to_string_lossy()),
        quote(node.mode),
    );
    let remote = if user == "root" {
        format!("sh -c {}", quote(&script))
    } else {
        format!("sudo -n -H sh -c {}", quote(&script))
    };
    let mut command = Command::new("ssh");
    command
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=6",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ClearAllForwardings=yes",
            "--",
        ])
        .arg(format!("{user}@{}", node.name))
        .arg(remote);
    Ok(command)
}

struct Bundle {
    _scratch: Scratch,
    directory: PathBuf,
    archive: PathBuf,
}
impl Bundle {
    fn new(prefix: &Path) -> Result<Self> {
        let scratch = Scratch::new()?;
        let directory = scratch.0.join("bin");
        fs::create_dir(&directory)?;
        let executable = fs::canonicalize(prefix.join("bin/sgpu-install"))?;
        let source = executable
            .parent()
            .ok_or("installed binaries unavailable")?;
        let mut total = 0u64;
        for name in packaging::BINARIES {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(source.join(name))?;
            let st = file.metadata()?;
            total = total.checked_add(st.len()).ok_or("oversized node bundle")?;
            if !st.is_file()
                || st.uid() != runtime::euid()
                || st.mode() & 0o022 != 0
                || total > 256 * 1024 * 1024
            {
                return Err("untrusted or oversized installed binary".into());
            }
            let mut magic = [0; 4];
            file.read_exact(&mut magic)?;
            if magic != *b"\x7fELF" {
                return Err("node bundle requires native executables".into());
            }
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0))?;
            let mut target = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(directory.join(name))?;
            std::io::copy(&mut file, &mut target)?;
            target.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        let archive = scratch.0.join("nodes.tar");
        let mut tar = Command::new("tar");
        tar.arg("-C")
            .arg(&directory)
            .arg("-cf")
            .arg(&archive)
            .args(packaging::BINARIES);
        process::capture(&mut tar, Duration::from_secs(30), 65536)?;
        Ok(Self {
            _scratch: scratch,
            directory,
            archive,
        })
    }
}

fn deploy(node: &Node, user: &str, config: &collector::Config, bundle: &Bundle) -> Result<()> {
    let mut command;
    if node.name == runtime::hostname()? {
        command = Command::new(bundle.directory.join("sgpu-install"));
        command
            .args(["node-bootstrap", "--fallback-mode", node.mode])
            .arg("--bin-dir")
            .arg(&bundle.directory)
            .arg("--agent-dir")
            .arg(&config.agents);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SLURM_GPU_TUI_") || name == "SGPU_INSTALL_DIR" {
                command.env_remove(name);
            }
        }
        process::capture(&mut command, Duration::from_secs(240), 1024 * 1024)?;
    } else {
        command = ssh_command(node, user, &config.agents)?;
        process::capture_file(
            &mut command,
            File::open(&bundle.archive)?,
            Duration::from_secs(240),
            1024 * 1024,
        )?;
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if release(config, node).is_some_and(|v| v >= update::version(crate::VERSION).unwrap()) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("installer completed, but the new shared telemetry is unverified; inspect sgpu-agent.service and retry".into())
}

fn execute(
    nodes: Vec<Node>,
    action: impl Fn(&Node) -> Result<()> + Sync,
) -> Vec<(String, Result<()>)> {
    let queue = Mutex::new(VecDeque::from(nodes));
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let sender = sender.clone();
            let queue = &queue;
            let action = &action;
            scope.spawn(move || {
                loop {
                    let Some(node) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
                    else {
                        break;
                    };
                    println!("Node {}: upgrading publisher", node.name);
                    let result = action(&node);
                    if sender.send((node.name, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        receiver
            .into_iter()
            .map(|(name, result)| {
                match &result {
                    Ok(()) => println!("Node {name}: {} verified", crate::VERSION),
                    Err(error) => eprintln!(
                        "Node {name}: FAILED / unverified: {}",
                        crate::cli::terminal_safe(&error.to_string())
                            .replace(['\n', '\r'], " ")
                            .chars()
                            .take(600)
                            .collect::<String>()
                    ),
                }
                (name, result)
            })
            .collect()
    })
}

pub(crate) fn run(prefix: &Path, user: &str, published: bool) -> Result<()> {
    ssh_user(user)?;
    let raw = process::run(
        process::Program::Sinfo,
        &["-N", "-h", "-o", "%N|%G"],
        Duration::from_secs(12),
    )?;
    let nodes = inventory(&raw)?;
    let config = collector::Config::default();
    let target = update::version(crate::VERSION).unwrap();
    let mut waiting = Vec::new();
    let mut bootstrap = Vec::new();
    let mut verified = 0;
    for node in &nodes {
        match release(&config, node) {
            Some(v) if v >= target => verified += 1,
            Some(v) if published && v >= [2, 2, 0] => waiting.push(node.clone()),
            _ => bootstrap.push(node.clone()),
        }
    }
    let mut failures = Vec::new();
    if !bootstrap.is_empty() {
        let bundle = Bundle::new(prefix)?;
        for (name, result) in execute(bootstrap, |node| deploy(node, user, &config, &bundle)) {
            if result.is_ok() {
                verified += 1;
            } else {
                failures.push(name);
            }
        }
    }
    if !waiting.is_empty() {
        println!(
            "Waiting up to 70 seconds for {} existing node updaters",
            waiting.len()
        );
        let start = Instant::now();
        while !waiting.is_empty() && start.elapsed() < Duration::from_secs(70) {
            waiting.retain(|node| {
                if release(&config, node).is_some_and(|v| v >= target) {
                    println!("Node {}: {} verified", node.name, crate::VERSION);
                    verified += 1;
                    false
                } else {
                    true
                }
            });
            if !waiting.is_empty() {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        if !waiting.is_empty() {
            let bundle = Bundle::new(prefix)?;
            for (name, result) in execute(waiting, |node| deploy(node, user, &config, &bundle)) {
                if result.is_ok() {
                    verified += 1;
                } else {
                    failures.push(name);
                }
            }
        }
    }
    failures.retain(|name| {
        let node = nodes.iter().find(|node| node.name == *name).unwrap();
        if release(&config, node).is_some_and(|v| v >= target) {
            println!(
                "Node {name}: fresh telemetry confirms {} after transport failure",
                crate::VERSION
            );
            verified += 1;
            false
        } else {
            true
        }
    });
    println!("Node updates: {verified}/{} verified", nodes.len());
    if !failures.is_empty() {
        return Err(format!("master installed; node updates incomplete: {}. Administrative SSH/remote sudo may be required; retry the same command. Use --local-only for a controller-only installation", failures.join(", ")).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn administrative_transport_keeps_authentication_and_validates_its_sink() {
        let node = Node {
            name: "gpu1".into(),
            mode: "gpu",
        };
        let command = ssh_command(&node, "admin", Path::new("/shared/nodes ' quoted")).unwrap();
        let args = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ForwardAgent=no",
            "--",
            "admin@gpu1",
        ] {
            assert!(args.iter().any(|a| a == required));
        }
        assert!(args.last().unwrap().starts_with("sudo -n -H sh -c "));
        assert!(
            ssh_command(
                &Node {
                    name: "-oProxyCommand=bad".into(),
                    mode: "cpu"
                },
                "root",
                Path::new("/shared")
            )
            .is_err()
        );
        for user in ["", "-bad", "root@other", "x;cmd", "a b"] {
            assert!(ssh_command(&node, user, Path::new("/shared")).is_err());
        }
    }
    #[test]
    fn inventory_uses_only_valid_scheduler_hosts_and_merges_partition_rows() {
        assert_eq!(
            inventory("gpu1|(null)\ngpu1|gpu:h100:4(S:0)\ncpu1|(null)\n").unwrap(),
            vec![
                Node {
                    name: "cpu1".into(),
                    mode: "cpu"
                },
                Node {
                    name: "gpu1".into(),
                    mode: "gpu"
                }
            ]
        );
        for raw in [
            "-oBad|gpu:1",
            "root@host|gpu:1",
            "x;cmd|gpu:1",
            "x|gpu:1|extra",
            "node[1-2]|gpu:1",
            "missing delimiter",
        ] {
            assert!(inventory(raw).is_err(), "{raw}");
        }
        let huge = (0..4097)
            .map(|i| format!("node{i}|(null)\n"))
            .collect::<String>();
        assert!(inventory(&huge).is_err());
    }

    #[test]
    fn shell_arguments_remain_literal_and_failed_nodes_do_not_stop_others() {
        let value = "a path'; $(exit 7) `exit 8`\nquoted";
        let output = Command::new("sh")
            .args(["-c", &format!("printf %s {}", quote(value))])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, value.as_bytes());
        let nodes = inventory("a|(null)\nb|gpu:1\nc|(null)\n").unwrap();
        let mut results = execute(nodes, |n| {
            if n.name == "b" {
                Err("SSH authentication failed".into())
            } else {
                Ok(())
            }
        });
        results.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_ok() && results[1].1.is_err() && results[2].1.is_ok());
    }

    #[test]
    fn reported_versions_require_fresh_trusted_payloads() {
        let root = tempfile::tempdir().unwrap();
        let mut config = collector::Config {
            data: root.path().join("data"),
            state: root.path().join("state"),
            agents: root.path().into(),
            max_age: 45.0,
            max_payload: 1048576,
            trusted: [runtime::euid()].into(),
            share_details: false,
            share_scripts: false,
            share_logs: false,
        };
        let node = Node {
            name: "cpu1".into(),
            mode: "cpu",
        };
        let mut payload = serde_json::json!({"agent_version":crate::AGENT_PAYLOAD_VERSION,"agent_build":"old","release":"1.3.0","hostname":"cpu1","node_kind":"cpu","ts":runtime::now(),"mem":{"total":"1","used":"0","avail":"1"},"gpus":[]});
        fs::write(
            root.path().join("cpu1.json"),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        assert_eq!(release(&config, &node), Some([1, 3, 0]));
        payload["release"] = serde_json::json!(crate::VERSION);
        fs::write(
            root.path().join("cpu1.json"),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        assert_eq!(release(&config, &node), update::version(crate::VERSION));
        config.trusted.clear();
        assert_eq!(release(&config, &node), None);
        config.trusted.insert(runtime::euid());
        payload["hostname"] = serde_json::json!("other");
        fs::write(
            root.path().join("cpu1.json"),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        assert_eq!(release(&config, &node), None);
    }
}
