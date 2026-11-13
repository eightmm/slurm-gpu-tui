use crate::{Result, process, runtime};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const PERSISTENCE_UNIT: &[u8] = br#"[Unit]
Description=Enable NVIDIA GPU persistence mode for sgpu monitoring
After=nvidia-persistenced.service

[Service]
Type=oneshot
ExecStart=/usr/bin/env nvidia-smi -pm 1
RemainAfterExit=yes
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
"#;

const BINARIES: &[&str] = &[
    "sgpu",
    "sgpu-agent",
    "sgpu-collector",
    "chkgpu",
    "sgpu-install",
];
// Only public, implemented settings may enter the world-readable unit.
const PUBLIC_ENV: &[&str] = &[
    "AGENT_RUNTIME_DIR",
    "AGENT_AUTO_UPDATE",
    "AGENT_UPDATE_SEC",
    "AGENT_UPDATE_STARTUP_SEC",
    "AGENT_UPDATE_STATE_DIR",
    "AGENT_SEC",
    "CPU_AGENT_SEC",
    "TELEMETRY_SEC",
    "TELEMETRY_MAX_JOBS",
    "GPU_TOPOLOGY_TTL_SEC",
    "AGENT_CMD_TIMEOUT_SEC",
    "COLLECTOR_SEC",
    "SCHEDULER_SEC",
    "SCHEDULER_MAX_AGE_SEC",
    "MEM_REFRESH_SEC",
    "AGENT_MAX_AGE_SEC",
    "AGENT_MAX_BYTES",
    "AGENT_TRUSTED_UIDS",
    "WASTE_MIN_SEC",
    "USAGE_KEEP_DAYS",
    "USAGE_SAVE_SEC",
    "SACCT_SEC",
    "METRICS_SEC",
    "METRICS_FILE",
    "METRICS_PENDING_MAX",
    "SLACK_CONFIG",
    "SLACK_DEBOUNCE_SEC",
    "SLACK_NAG_SEC",
    "ROGUE_IGNORE",
    "LOG_TAIL_BYTES",
    "LOG_MIRROR_SEC",
    "LOG_MIRROR_MAX_SEC",
];

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Master,
    Node,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum NodeMode {
    Gpu,
    Cpu,
}
#[derive(Parser)]
#[command(version = crate::VERSION, about = "Install or remove native sgpu on this host only")]
pub struct Args {
    #[command(subcommand)]
    pub command: Action,
}
#[derive(Subcommand)]
pub enum Action {
    Install {
        #[arg(long, value_enum, default_value = "master")]
        role: Role,
        #[arg(long, value_enum, default_value = "gpu")]
        mode: NodeMode,
        #[arg(long, env = "SGPU_INSTALL_DIR")]
        prefix: Option<PathBuf>,
        #[arg(long, env = "SGPU_BIN_DIR")]
        bin_dir: Option<PathBuf>,
        #[arg(long)]
        no_service: bool,
    },
    Uninstall {
        #[arg(long, value_enum, default_value = "master")]
        role: Role,
        #[arg(long, env = "SGPU_INSTALL_DIR")]
        prefix: Option<PathBuf>,
        #[arg(long)]
        no_service: bool,
    },
    SlackConfig {
        #[arg(long)]
        output: Option<PathBuf>,
    },
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    roles: BTreeSet<Role>,
}

fn default_prefix() -> PathBuf {
    discover_prefix(Path::new("/usr/local/bin")).unwrap_or_else(|| "/opt/sgpu".into())
}

fn discover_prefix(bin: &Path) -> Option<PathBuf> {
    ["sgpu", "sgpu-collector", "chkgpu"]
        .iter()
        .find_map(|name| installed_prefix(&bin.join(name), name))
}

fn installed_prefix(entry: &Path, name: &str) -> Option<PathBuf> {
    let link = fs::symlink_metadata(entry).ok()?;
    if link.uid() != runtime::euid() || (!link.is_file() && !link.file_type().is_symlink()) {
        return None;
    }
    let target = if link.file_type().is_symlink() {
        let path = fs::read_link(entry).ok()?;
        if path.is_absolute() {
            path
        } else {
            entry.parent()?.join(path)
        }
    } else {
        entry.to_path_buf()
    };
    let resolved = fs::canonicalize(&target).ok()?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&resolved)
        .ok()?;
    let st = file.metadata().ok()?;
    if !st.is_file() || st.uid() != runtime::euid() || st.mode() & 0o022 != 0 {
        return None;
    }
    let mut magic = [0; 4];
    file.read_exact(&mut magic).ok()?;
    if let Some(root) = target
        .parent()
        .filter(|p| p.file_name().is_some_and(|s| s == "bin"))
        .and_then(Path::parent)
        && let Ok(record) = manifest(root)
        && !record.roles.is_empty()
        && magic == *b"\x7fELF"
    {
        return Some(root.to_path_buf());
    }
    let (bytes, _) = runtime::read_regular(&resolved, 1024 * 1024, Some(runtime::euid())).ok()?;
    let body = std::str::from_utf8(&bytes).ok()?.trim_end();
    if let Some(executable) = body
        .strip_prefix("#!/bin/bash\nexec \"")
        .and_then(|s| s.strip_suffix("\" \"$@\""))
    {
        let suffix = format!("/.venv/bin/{name}");
        let root = PathBuf::from(executable.strip_suffix(&suffix)?);
        if prefix_path(Some(root.clone())).is_ok()
            && fs::metadata(&root).is_ok_and(|s| s.is_dir() && s.uid() == runtime::euid())
        {
            return Some(root);
        }
    }
    let module = match name {
        "sgpu" => "sgpu.cli",
        "sgpu-collector" => "sgpu.collector",
        "chkgpu" => "sgpu.quickview",
        _ => return None,
    };
    let bin = target.parent()?;
    let venv = bin.parent()?;
    if bin.file_name().is_some_and(|s| s == "bin")
        && venv.file_name().is_some_and(|s| s == ".venv")
        && body
            .lines()
            .any(|line| line == format!("from {module} import main"))
        && runtime::read_regular(&venv.join("pyvenv.cfg"), 65536, Some(runtime::euid())).is_ok()
    {
        return venv.parent().map(Path::to_path_buf);
    }
    None
}

fn inherited_settings(unit: &str, role: Role) -> Result<Vec<(String, String)>> {
    let mut settings = std::collections::BTreeMap::new();
    for line in unit
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Environment="))
    {
        if line.trim().is_empty() {
            settings.clear();
            continue;
        }
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quote = None;
        let mut escaped = false;
        for c in line.chars() {
            if escaped {
                match c {
                    '\\' | '\'' | '"' => word.push(c),
                    's' => word.push(' '),
                    _ => return Err("unsupported escape in previous service environment".into()),
                };
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if quote == Some(c) {
                quote = None;
            } else if quote.is_none() && matches!(c, '\'' | '"') {
                quote = Some(c);
            } else if quote.is_none() && c.is_whitespace() {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            } else {
                word.push(c);
            }
        }
        if escaped || quote.is_some() {
            return Err("invalid previous service environment".into());
        }
        if !word.is_empty() {
            words.push(word);
        }
        for word in words {
            let Some((name, value)) = word.split_once('=') else {
                continue;
            };
            let Some(suffix) = name.strip_prefix("SLURM_GPU_TUI_") else {
                continue;
            };
            if !PUBLIC_ENV.contains(&suffix)
                && !matches!(
                    suffix,
                    "AGENT_DIR"
                        | "DATA_DIR"
                        | "STATE_DIR"
                        | "SHARE_LOGS"
                        | "SHARE_JOB_DETAILS"
                        | "SHARE_SCRIPTS"
                )
            {
                continue;
            }
            let mut decoded = String::new();
            let mut chars = value.chars();
            while let Some(c) = chars.next() {
                if c == '%' && chars.next() != Some('%') {
                    return Err("previous service uses unsupported environment specifiers".into());
                }
                decoded.push(c);
            }
            let value = decoded;
            if value.chars().any(char::is_control) {
                return Err("control character in previous service environment".into());
            }
            settings.insert(name.to_string(), value);
        }
    }
    if role == Role::Master {
        for suffix in ["SHARE_LOGS", "SHARE_JOB_DETAILS", "SHARE_SCRIPTS"] {
            settings
                .entry(format!("SLURM_GPU_TUI_{suffix}"))
                .or_insert_with(|| "0".into());
        }
    }
    Ok(settings.into_iter().collect())
}

fn preserve_service_settings(prefix: &Path, role: Role) -> Result<()> {
    let path = service_dir().join(unit_name(role));
    let (bytes, st) = match runtime::read_regular(&path, 1024 * 1024, Some(runtime::euid())) {
        Ok(value) => value,
        Err(_)
            if fs::symlink_metadata(&path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if st.mode() & 0o022 != 0 {
        return Err("previous service is writable by other users".into());
    }
    let unit = std::str::from_utf8(&bytes)?;
    if !unit.contains(&escape(&prefix.to_string_lossy(), true)?) {
        return Err("service belongs to another installation; select its existing prefix".into());
    }
    let previous = inherited_settings(unit, role)?;
    for (name, value) in previous {
        if std::env::var_os(&name).is_none() {
            // The installer calls this at startup, before spawning any workers.
            unsafe {
                std::env::set_var(name, value);
            }
        }
    }
    Ok(())
}
fn prefix_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = path.unwrap_or_else(default_prefix);
    if !path.is_absolute()
        || path == Path::new("/")
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
    {
        return Err("install prefix must be an absolute dedicated path".into());
    }
    Ok(path)
}
fn secure(path: &Path, mode: u32) -> Result<()> {
    if let Ok(st) = fs::symlink_metadata(path)
        && (!st.is_dir() || st.uid() != runtime::euid())
    {
        return Err(format!("{} is not a directory owned by this user", path.display()).into());
    }
    runtime::secure_dir(path, mode)
}
fn manifest(prefix: &Path) -> Result<Manifest> {
    let path = prefix.join(".sgpu-native-install");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Manifest {
            version: 1,
            roles: BTreeSet::new(),
        }),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let (bytes, st) = runtime::read_regular(&path, 65536, Some(runtime::euid()))?;
            if st.mode() & 0o022 != 0 {
                return Err("install manifest is writable by other users".into());
            }
            let manifest: Manifest = serde_json::from_slice(&bytes)?;
            if manifest.version != 1 {
                return Err("unsupported install manifest".into());
            }
            Ok(manifest)
        }
    }
}
fn unit_name(role: Role) -> &'static str {
    match role {
        Role::Master => "sgpu-collector.service",
        Role::Node => "sgpu-agent.service",
    }
}
fn escape(raw: &str, exec: bool) -> Result<String> {
    if raw.chars().any(char::is_control) {
        return Err("control character in service path".into());
    }
    let value = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(if exec {
        value.replace('$', "$$")
    } else {
        value
    })
}
fn settings(role: Role) -> Vec<(String, String)> {
    let mut env = vec![(
        "SLURM_GPU_TUI_AGENT_DIR".into(),
        runtime::agent_dir().to_string_lossy().into_owned(),
    )];
    if role == Role::Master {
        env.push((
            "SLURM_GPU_TUI_DATA_DIR".into(),
            runtime::data_dir().to_string_lossy().into_owned(),
        ));
        env.push((
            "SLURM_GPU_TUI_STATE_DIR".into(),
            runtime::state_dir().to_string_lossy().into_owned(),
        ));
        for (name, default) in [
            ("SHARE_LOGS", true),
            ("SHARE_JOB_DETAILS", true),
            ("SHARE_SCRIPTS", false),
        ] {
            let value = std::env::var(format!("SGPU_{name}"))
                .or_else(|_| std::env::var(format!("SLURM_GPU_TUI_{name}")))
                .ok()
                .map(|v| {
                    !matches!(
                        v.to_ascii_lowercase().as_str(),
                        "0" | "false" | "no" | "off" | ""
                    )
                })
                .unwrap_or(default);
            env.push((format!("SLURM_GPU_TUI_{name}"), u8::from(value).to_string()));
        }
    }
    for suffix in PUBLIC_ENV {
        let name = format!("SLURM_GPU_TUI_{suffix}");
        if let Ok(value) = std::env::var(&name) {
            env.push((name, value));
        }
    }
    env
}
fn unit(role: Role, mode: NodeMode, prefix: &Path) -> Result<String> {
    let binary = prefix.join("bin").join(if role == Role::Master {
        "sgpu-collector"
    } else {
        "sgpu-agent"
    });
    let executable = escape(&binary.to_string_lossy(), true)?;
    let mut env = String::new();
    for (name, value) in settings(role) {
        env.push_str(&format!(
            "Environment=\"{name}={}\"\n",
            escape(&value, false)?
        ));
    }
    let command = if role == Role::Master {
        format!("\"{executable}\"")
    } else {
        format!(
            "\"{executable}\" --mode {}",
            if matches!(mode, NodeMode::Gpu) {
                "gpu"
            } else {
                "cpu"
            }
        )
    };
    let mount = format!(
        "RequiresMountsFor=\"{}\" \"{}\"\n",
        escape(&binary.to_string_lossy(), false)?,
        escape(&runtime::agent_dir().to_string_lossy(), false)?
    );
    Ok(format!(
        "[Unit]\nDescription=sgpu {} (native, push-only)\nAfter=network-online.target remote-fs.target\nWants=network-online.target\n{mount}StartLimitIntervalSec=60\nStartLimitBurst=6\n\n[Service]\nType=simple\n{env}ExecStart={command}\nRestart=always\nRestartSec=10\nTimeoutStopSec=60\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy={}\n",
        if role == Role::Master {
            "collector"
        } else {
            "node publisher"
        },
        "multi-user.target"
    ))
}

enum Entry {
    Missing,
    Link(PathBuf),
    File(Vec<u8>, u32),
}
#[derive(Default)]
struct Transaction {
    entries: Vec<(PathBuf, Entry)>,
}
impl Transaction {
    fn remember(&mut self, path: &Path) -> Result<()> {
        if self.entries.iter().any(|(p, _)| p == path) {
            return Ok(());
        }
        let entry = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Entry::Missing,
            Err(error) => return Err(error.into()),
            Ok(st) if st.uid() != runtime::euid() => {
                return Err(format!(
                    "refusing to change {} owned by another user",
                    path.display()
                )
                .into());
            }
            Ok(st) if st.file_type().is_symlink() => Entry::Link(fs::read_link(path)?),
            Ok(st) if st.is_file() => Entry::File(
                runtime::read_regular(path, 1024 * 1024, Some(runtime::euid()))?.0,
                st.mode() & 0o777,
            ),
            Ok(_) => return Err(format!("refusing to replace {}", path.display()).into()),
        };
        self.entries.push((path.to_path_buf(), entry));
        Ok(())
    }
    fn write(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        self.remember(path)?;
        runtime::atomic_write(path, bytes, mode, true)
    }
    fn link(&mut self, path: &Path, target: &Path) -> Result<()> {
        self.remember(path)?;
        replace_link(path, target)
    }
    fn rollback(&mut self) -> Result<()> {
        let mut failed = Vec::new();
        for (path, entry) in self.entries.drain(..).rev() {
            let result = match entry {
                Entry::Missing => match fs::remove_file(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    value => value.map_err(Into::into),
                },
                Entry::Link(target) => replace_link(&path, &target),
                Entry::File(bytes, mode) => runtime::atomic_write(&path, &bytes, mode, true),
            };
            if let Err(error) = result {
                failed.push(format!("{}: {error}", path.display()));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(format!("rollback incomplete: {}", failed.join("; ")).into())
        }
    }
}
fn replace_link(path: &Path, target: &Path) -> Result<()> {
    let parent = path.parent().ok_or("link parent missing")?;
    let pending = parent.join(format!(".sgpu-link-{}", std::process::id()));
    symlink(target, &pending)?;
    if let Err(error) = fs::rename(&pending, path) {
        let _ = fs::remove_file(pending);
        return Err(error.into());
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn source_dir(bin_dir: Option<PathBuf>) -> Result<PathBuf> {
    bin_dir
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        })
        .ok_or_else(|| "binary source directory unavailable".into())
}
fn stage_files(
    prefix: &Path,
    source: &Path,
    role: Role,
    mode: NodeMode,
    transaction: &mut Transaction,
) -> Result<()> {
    // Open every input first; copying through these handles avoids path swaps.
    let mut sources = Vec::new();
    for name in BINARIES {
        let path = source.join(name);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let st = file.metadata()?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        if !st.is_file() || st.mode() & 0o111 == 0 || magic != *b"\x7fELF" {
            return Err(format!("{} is not a native Linux executable", path.display()).into());
        }
        sources.push(file);
    }
    secure(prefix, 0o755)?;
    for child in ["releases", "bin", "units"] {
        secure(&prefix.join(child), 0o755)?;
    }
    let mut record = manifest(prefix)?;
    if let Ok(st) = fs::symlink_metadata(prefix.join("current"))
        && !st.file_type().is_symlink()
    {
        return Err("current must be a release symlink".into());
    }
    let generation = format!(
        "{}-{}-{}",
        crate::VERSION,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let target = PathBuf::from("releases").join(generation);
    let directory = prefix.join(&target);
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))?;
    use std::io::{Seek, SeekFrom};
    for (name, mut source) in BINARIES.iter().zip(sources) {
        source.seek(SeekFrom::Start(0))?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(directory.join(name))?;
        std::io::copy(&mut source, &mut destination)?;
        destination.set_permissions(fs::Permissions::from_mode(0o755))?;
        destination.sync_all()?;
    }
    runtime::atomic_write(&directory.join(".sgpu-release"), b"1", 0o644, true)?;
    fs::File::open(&directory)?.sync_all()?;
    transaction.write(
        &prefix.join("units").join(unit_name(role)),
        unit(role, mode, prefix)?.as_bytes(),
        0o644,
    )?;
    for name in BINARIES {
        transaction.link(
            &prefix.join("bin").join(name),
            Path::new(&format!("../current/{name}")),
        )?;
    }
    record.roles.insert(role);
    transaction.write(
        &prefix.join(".sgpu-native-install"),
        &serde_json::to_vec(&record)?,
        0o644,
    )?;
    transaction.link(&prefix.join("current"), &target)?;
    Ok(())
}
fn systemctl(args: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    command.args(args);
    process::capture(&mut command, Duration::from_secs(75), 1024 * 1024)?;
    Ok(())
}
fn service_available() -> bool {
    Path::new("/run/systemd/system").is_dir()
}
fn service_dir() -> PathBuf {
    "/etc/systemd/system".into()
}
fn stop_background(prefix: &Path, role: Role) -> Result<()> {
    let mut command = Command::new(prefix.join("bin").join(if role == Role::Master {
        "sgpu-collector"
    } else {
        "sgpu-agent"
    }));
    command.arg("--stop").envs(settings(role));
    process::capture(&mut command, Duration::from_secs(65), 65536)?;
    Ok(())
}
fn start_background(prefix: &Path, role: Role, mode: NodeMode) -> Result<()> {
    let mut command = Command::new(prefix.join("bin").join(if role == Role::Master {
        "sgpu-collector"
    } else {
        "sgpu-agent"
    }));
    command.arg("--daemon").envs(settings(role));
    if role == Role::Node {
        command.args([
            "--mode",
            if matches!(mode, NodeMode::Gpu) {
                "gpu"
            } else {
                "cpu"
            },
        ]);
    }
    process::capture(&mut command, Duration::from_secs(15), 65536)?;
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        if background_running(role)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("background service did not acquire its local lock; inspect its local log".into())
}
fn background_running(role: Role) -> Result<bool> {
    runtime::lock_is_held(&if role == Role::Master {
        runtime::data_dir().join("collector.lock")
    } else {
        runtime::agent_runtime("lock")
    })
}
fn apply_service(
    prefix: &Path,
    role: Role,
    mode: NodeMode,
    transaction: &mut Transaction,
) -> Result<()> {
    if role == Role::Master {
        crate::collector::prepare(&crate::collector::Config::default())?;
    }
    let agents = runtime::agent_dir();
    if !agents.exists() {
        fs::create_dir_all(&agents)?;
        fs::set_permissions(&agents, fs::Permissions::from_mode(0o1777))?;
    } else if !fs::symlink_metadata(&agents)?.is_dir() {
        return Err("agent directory is not a regular directory".into());
    }
    if service_available() {
        let destination = service_dir();
        if !destination.exists() {
            fs::create_dir_all(&destination)?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        }
        if fs::symlink_metadata(destination.join(unit_name(role))).is_ok() {
            let (old, _) = runtime::read_regular(
                &destination.join(unit_name(role)),
                1024 * 1024,
                Some(runtime::euid()),
            )?;
            if !String::from_utf8_lossy(&old).contains(&escape(&prefix.to_string_lossy(), true)?) {
                return Err(
                    "service belongs to another installation; select its existing prefix".into(),
                );
            }
        }
        let (bytes, _) = runtime::read_regular(
            &prefix.join("units").join(unit_name(role)),
            1024 * 1024,
            None,
        )?;
        transaction.write(&destination.join(unit_name(role)), &bytes, 0o644)?;
        if role == Role::Node {
            for legacy in ["sgpu-cpu-agent.service", "sgpu-gpu-agent.service"] {
                if service_dir().join(legacy).exists() {
                    systemctl(&["disable", "--now", legacy])?;
                }
            }
            stop_background(prefix, role)?;
            if runtime::euid() == 0
                && matches!(mode, NodeMode::Gpu)
                && std::env::var("SGPU_ENABLE_PERSISTENCE")
                    .map(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"))
                    .unwrap_or(true)
            {
                transaction.write(
                    &destination.join("sgpu-gpu-persistence.service"),
                    PERSISTENCE_UNIT,
                    0o644,
                )?;
                systemctl(&["daemon-reload"])?;
                systemctl(&["enable", "--now", "sgpu-gpu-persistence.service"])?;
                systemctl(&["restart", "sgpu-gpu-persistence.service"])?;
            }
        }
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", unit_name(role)])?;
        systemctl(&["restart", unit_name(role)])?;
        systemctl(&["is-active", "--quiet", unit_name(role)])?;
    } else {
        stop_background(prefix, role)?;
        start_background(prefix, role, mode)?;
    }
    Ok(())
}
fn path_link(prefix: &Path, transaction: &mut Transaction) -> Result<()> {
    path_links_in(Path::new("/usr/local/bin"), prefix, transaction)
}
fn path_links_in(bin: &Path, prefix: &Path, transaction: &mut Transaction) -> Result<()> {
    let directory = fs::symlink_metadata(bin)?;
    if !directory.is_dir() || directory.uid() != runtime::euid() || directory.mode() & 0o022 != 0 {
        return Err("unsafe public executable directory".into());
    }
    for name in BINARIES {
        let path = bin.join(name);
        if let Ok(st) = fs::symlink_metadata(&path)
            && (!(st.file_type().is_symlink() && fs::read_link(&path)?.starts_with(prefix))
                && installed_prefix(&path, name).as_deref() != Some(prefix))
        {
            return Err(format!("refusing to replace unrelated {}", path.display()).into());
        }
        transaction.link(&path, &prefix.join("bin").join(name))?;
    }
    Ok(())
}
pub fn write_slack_config(output: Option<PathBuf>) -> Result<PathBuf> {
    let path = output.unwrap_or_else(crate::notify::config_path);
    let mut raw: Value = match fs::symlink_metadata(&path) {
        Ok(_) => {
            serde_json::from_slice(&runtime::read_regular(&path, 65536, Some(runtime::euid()))?.0)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"sender_name":"AI-master", "lang":"en", "node_health":true, "collect_alert":true, "waste_alert_hours":2, "rogue_alert":true, "ecc_alert":true})
        }
        Err(error) => return Err(error.into()),
    };
    if !raw.is_object() {
        return Err("Slack configuration must be an object".into());
    }
    for (env, key) in [
        ("SGPU_SLACK_BOT_TOKEN", "bot_token"),
        ("SGPU_SLACK_CHANNEL", "channel"),
        ("SGPU_SLACK_SENDER", "sender_name"),
        ("SGPU_SLACK_LANG", "lang"),
    ] {
        if let Ok(value) = std::env::var(env) {
            raw[key] = json!(value);
        }
    }
    if raw["bot_token"].as_str().is_none_or(str::is_empty)
        || raw["channel"].as_str().is_none_or(str::is_empty)
    {
        return Err(
            "set SGPU_SLACK_BOT_TOKEN and SGPU_SLACK_CHANNEL, or keep an existing configuration"
                .into(),
        );
    }
    let parent = path.parent().ok_or("config parent missing")?;
    if !parent.exists() {
        secure(parent, 0o700)?;
    }
    runtime::atomic_write(&path, &serde_json::to_vec_pretty(&raw)?, 0o600, true)?;
    Ok(path)
}
fn install(
    prefix: &Path,
    source: &Path,
    role: Role,
    mode: NodeMode,
    no_service: bool,
) -> Result<()> {
    secure(prefix, 0o755)?;
    let _lock = runtime::Lock::acquire(&prefix.join("install.lock"), Duration::from_secs(5))?;
    let mut transaction = Transaction::default();
    let service = !no_service && service_available();
    let was_background_running = !no_service && !service && background_running(role)?;
    let mut service_states = Vec::new();
    if service {
        let mut names = vec![unit_name(role)];
        if role == Role::Node {
            names.extend([
                "sgpu-cpu-agent.service",
                "sgpu-gpu-agent.service",
                "sgpu-gpu-persistence.service",
            ]);
        }
        for name in names {
            service_states.push((
                name,
                systemctl(&["is-enabled", "--quiet", name]).is_ok(),
                systemctl(&["is-active", "--quiet", name]).is_ok(),
            ));
        }
    }
    let mut service_touched = false;
    let result: Result<()> = (|| {
        stage_files(prefix, source, role, mode, &mut transaction)?;
        if !no_service {
            path_link(prefix, &mut transaction)?;
            if role == Role::Master && std::env::var_os("SGPU_SLACK_BOT_TOKEN").is_some() {
                transaction.remember(&crate::notify::config_path())?;
                write_slack_config(None)?;
            }
            service_touched = true;
            apply_service(prefix, role, mode, &mut transaction)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = transaction.rollback();
        let restore = if service && service_touched {
            systemctl(&["daemon-reload"]).and_then(|()| {
                for (name, enabled, running) in service_states {
                    if enabled {
                        systemctl(&["enable", name])?;
                    } else {
                        let _ = systemctl(&["disable", name]);
                    }
                    if running {
                        systemctl(&["restart", name])?;
                    } else {
                        let _ = systemctl(&["stop", name]);
                    }
                }
                Ok(())
            })
        } else if service_touched && was_background_running {
            start_background(prefix, role, mode)
        } else {
            Ok(())
        };
        return Err(format!(
            "install failed: {error}; files: {}; service: {}",
            rollback
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "previous files restored".into()),
            restore
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "previous state restored".into())
        )
        .into());
    }
    println!(
        "Installed sgpu {} at {}{}",
        crate::VERSION,
        prefix.display(),
        if no_service {
            " (files only; services untouched)"
        } else {
            ""
        }
    );
    if !no_service {
        println!("Active native command: /usr/local/bin/sgpu (verify with sgpu --version)");
        if role == Role::Master {
            // Advertise only after the local installation and service succeeded.
            let agent = fs::canonicalize(prefix.join("bin/sgpu-agent"))?;
            match crate::update::publish(&agent) {
                Ok(()) if crate::update::enabled() => {
                    println!("Node update published; bootstrapped agents check within 60 seconds.")
                }
                Ok(()) => println!("Node automatic updates disabled by configuration."),
                Err(error) => eprintln!(
                    "WARNING: local install succeeded, but node update was not published: {error}"
                ),
            }
        }
    }
    Ok(())
}
fn uninstall(prefix: &Path, role: Role, no_service: bool) -> Result<()> {
    secure(prefix, 0o755)?;
    let _lock = runtime::Lock::acquire(&prefix.join("install.lock"), Duration::from_secs(5))?;
    let mut record = manifest(prefix)?;
    if !record.roles.remove(&role) {
        return Err("this role is not installed in the selected prefix".into());
    }
    if !no_service {
        if service_available() {
            let path = service_dir().join(unit_name(role));
            // Refuse to remove a service installed by another prefix.
            let (bytes, _) = runtime::read_regular(&path, 1024 * 1024, Some(runtime::euid()))?;
            if !String::from_utf8_lossy(&bytes)
                .contains(&escape(&prefix.join("bin").to_string_lossy(), true)?)
            {
                return Err("service belongs to another installation".into());
            }
            systemctl(&["disable", "--now", unit_name(role)])?;
            fs::remove_file(path)?;
            if role == Role::Node && runtime::euid() == 0 {
                let path = service_dir().join("sgpu-gpu-persistence.service");
                if runtime::read_regular(&path, 65536, Some(0))
                    .is_ok_and(|(bytes, _)| bytes == PERSISTENCE_UNIT)
                {
                    systemctl(&["disable", "--now", "sgpu-gpu-persistence.service"])?;
                    fs::remove_file(path)?;
                }
            }
            systemctl(&["daemon-reload"])?;
        } else {
            stop_background(prefix, role)?;
        }
    }
    let unit = prefix.join("units").join(unit_name(role));
    if fs::symlink_metadata(&unit).is_ok() {
        fs::remove_file(unit)?;
    }
    if record.roles.is_empty() {
        for name in BINARIES {
            let path = prefix.join("bin").join(name);
            if fs::read_link(&path).is_ok_and(|p| p == Path::new(&format!("../current/{name}"))) {
                fs::remove_file(path)?;
            }
            if !no_service && runtime::euid() == 0 {
                let link = Path::new("/usr/local/bin").join(name);
                if fs::read_link(&link).is_ok_and(|p| p == prefix.join("bin").join(name)) {
                    fs::remove_file(link)?;
                }
            }
        }
        if fs::symlink_metadata(prefix.join("current")).is_ok_and(|s| s.file_type().is_symlink()) {
            fs::remove_file(prefix.join("current"))?;
        }
    }
    runtime::atomic_write(
        &prefix.join(".sgpu-native-install"),
        &serde_json::to_vec(&record)?,
        0o644,
        true,
    )?;
    println!(
        "Removed local {} role. Persistent data, shared payloads and previous binaries are preserved.",
        if role == Role::Master {
            "master"
        } else {
            "node"
        }
    );
    Ok(())
}
pub fn run(args: Args) -> Result<()> {
    if runtime::euid() != 0 {
        return Err("installation and configuration require root (sudo)".into());
    }
    match args.command {
        Action::Install {
            role,
            mode,
            prefix,
            bin_dir,
            no_service,
        } => {
            let explicit = prefix.is_some();
            let prefix = prefix_path(prefix)?;
            if !no_service {
                preserve_service_settings(&prefix, role)?;
            }
            if !explicit {
                println!(
                    "Install location: {} (existing sgpu installations are reused)",
                    prefix.display()
                );
            }
            install(&prefix, &source_dir(bin_dir)?, role, mode, no_service)
        }
        Action::Uninstall {
            role,
            prefix,
            no_service,
        } => uninstall(&prefix_path(prefix)?, role, no_service),
        Action::SlackConfig { output } => {
            println!(
                "Slack configuration saved with mode 0600: {}",
                write_slack_config(output)?.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write_owned(path: &Path, body: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[test]
    fn previous_wrappers_are_discovered_for_symlink_and_regular_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("old install");
        let bin = tmp.path().join("public-bin");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let body = format!(
            "#!/bin/bash\nexec \"{}/.venv/bin/sgpu\" \"$@\"\n",
            root.display()
        );
        write_owned(&root.join("bin/sgpu"), body.as_bytes());
        symlink(root.join("bin/sgpu"), bin.join("sgpu")).unwrap();
        assert_eq!(discover_prefix(&bin), Some(root.clone()));
        fs::remove_file(bin.join("sgpu")).unwrap();
        write_owned(&bin.join("sgpu"), body.as_bytes());
        assert_eq!(discover_prefix(&bin), Some(root.clone()));
        write_owned(&bin.join("sgpu"), b"#!/bin/sh\necho unrelated\n");
        assert_eq!(discover_prefix(&bin), None);
        write_owned(&bin.join("sgpu"), body.as_bytes());
        fs::set_permissions(bin.join("sgpu"), fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(discover_prefix(&bin), None);
    }
    #[test]
    fn previous_console_scripts_and_owned_native_manifests_are_recognized() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("old");
        let bin = tmp.path().join("public-bin");
        fs::create_dir_all(&bin).unwrap();
        write_owned(
            &root.join(".venv/bin/sgpu"),
            b"#!/fixture\nfrom sgpu.cli import main\n",
        );
        write_owned(&root.join(".venv/pyvenv.cfg"), b"home = fixture");
        symlink(root.join(".venv/bin/sgpu"), bin.join("sgpu")).unwrap();
        assert_eq!(discover_prefix(&bin), Some(root.clone()));
        fs::remove_file(bin.join("sgpu")).unwrap();
        write_owned(&root.join("bin/sgpu"), b"\x7fELFfixture");
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("bin/sgpu"))
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();
        symlink(root.join("bin/sgpu"), bin.join("sgpu")).unwrap();
        assert_eq!(discover_prefix(&bin), None);
        write_owned(
            &root.join(".sgpu-native-install"),
            br#"{"version":1,"roles":["master"]}"#,
        );
        assert_eq!(discover_prefix(&bin), Some(root));
    }
    #[test]
    fn legacy_installation_upgrades_in_place_and_rolls_back_all_entrypoints() {
        for regular in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            runtime::secure_dir(tmp.path(), 0o700).unwrap();
            let root = tmp.path().join("legacy");
            let public = tmp.path().join("public");
            let source = tmp.path().join("source");
            fs::create_dir_all(&public).unwrap();
            runtime::secure_dir(&public, 0o755).unwrap();
            fs::create_dir_all(root.join(".venv")).unwrap();
            fs::write(root.join(".venv/preserve"), "old environment").unwrap();
            for name in BINARIES {
                write_owned(&source.join(name), b"\x7fELFfixture");
            }
            for name in ["sgpu", "sgpu-collector", "chkgpu"] {
                let body = format!(
                    "#!/bin/bash\nexec \"{}/.venv/bin/{name}\" \"$@\"\n",
                    root.display()
                );
                write_owned(&root.join("bin").join(name), body.as_bytes());
                if regular && name == "sgpu" {
                    write_owned(&public.join(name), body.as_bytes());
                } else {
                    symlink(root.join("bin").join(name), public.join(name)).unwrap();
                }
            }
            assert_eq!(discover_prefix(&public), Some(root.clone()));
            let mut transaction = Transaction::default();
            stage_files(
                &root,
                &source,
                Role::Master,
                NodeMode::Gpu,
                &mut transaction,
            )
            .unwrap();
            path_links_in(&public, &root, &mut transaction).unwrap();
            assert!(
                fs::read(public.join("sgpu"))
                    .unwrap()
                    .starts_with(b"\x7fELF")
            );
            transaction.rollback().unwrap();
            assert!(
                fs::read_to_string(public.join("sgpu"))
                    .unwrap()
                    .contains("/.venv/bin/sgpu")
            );
            assert_eq!(
                fs::read_to_string(root.join(".venv/preserve")).unwrap(),
                "old environment"
            );
            assert!(!root.join("current").exists());
        }
    }
    #[test]
    fn inherited_public_paths_flags_and_escaping_preserve_settings_without_tokens() {
        let raw = "[Service]\nEnvironment=SLURM_GPU_TUI_AGENT_DIR=/shared/nodes\nEnvironment=SLURM_GPU_TUI_SHARE_LOGS=0\nEnvironment=\"SLURM_GPU_TUI_STATE_DIR=/state/a b%%q\"\nEnvironment=SLURM_GPU_TUI_SLACK_BOT_TOKEN=PRIVATE_TOKEN\n";
        let values = inherited_settings(raw, Role::Master)
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values["SLURM_GPU_TUI_AGENT_DIR"], "/shared/nodes");
        assert_eq!(values["SLURM_GPU_TUI_STATE_DIR"], "/state/a b%q");
        assert_eq!(values["SLURM_GPU_TUI_SHARE_LOGS"], "0");
        assert_eq!(values["SLURM_GPU_TUI_SHARE_JOB_DETAILS"], "0");
        assert_eq!(values["SLURM_GPU_TUI_SHARE_SCRIPTS"], "0");
        assert!(!values.keys().any(|k| k.contains("TOKEN")));
        assert!(
            inherited_settings(
                "Environment=\"SLURM_GPU_TUI_DATA_DIR=unterminated",
                Role::Master
            )
            .is_err()
        );
        assert!(
            inherited_settings("Environment=SLURM_GPU_TUI_STATE_DIR=%h/state", Role::Master)
                .is_err()
        );
    }
    #[test]
    fn units_use_native_paths_and_credential_free_environment() {
        let master = unit(Role::Master, NodeMode::Gpu, Path::new("/opt/sgpu test")).unwrap();
        assert!(master.contains("ExecStart=\"/opt/sgpu test/bin/sgpu-collector\""));
        assert!(master.contains("Restart=always"));
        assert!(!master.contains("SLACK_BOT_TOKEN"));
        assert!(
            unit(Role::Node, NodeMode::Cpu, Path::new("/opt/sgpu"))
                .unwrap()
                .contains("--mode cpu")
        );
        assert!(prefix_path(Some("/".into())).is_err());
        assert!(prefix_path(Some("relative".into())).is_err());
        assert!(escape("a\nb", false).is_err());
        assert_eq!(escape("a%\"$b", true).unwrap(), "a%%\\\"$$b");
    }
    #[test]
    fn rollback_restores_missing_regular_and_symlink_entries() {
        let root = tempfile::tempdir().unwrap();
        runtime::secure_dir(root.path(), 0o700).unwrap();
        let file = root.path().join("file");
        let link = root.path().join("link");
        let fresh = root.path().join("new");
        fs::write(&file, "original").unwrap();
        symlink("before", &link).unwrap();
        let mut transaction = Transaction::default();
        transaction.write(&file, b"changed", 0o600).unwrap();
        transaction.link(&link, Path::new("after")).unwrap();
        transaction.write(&fresh, b"new", 0o644).unwrap();
        transaction.rollback().unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "original");
        assert_eq!(fs::read_link(link).unwrap(), Path::new("before"));
        assert!(!fresh.exists());
    }
}
