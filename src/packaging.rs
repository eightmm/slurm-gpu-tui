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
    if runtime::euid() == 0 {
        "/opt/sgpu".into()
    } else {
        runtime::home().join(".sgpu/app")
    }
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
            let (bytes, _) = runtime::read_regular(&path, 65536, Some(runtime::euid()))?;
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
            ("SHARE_LOGS", runtime::euid() == 0),
            ("SHARE_JOB_DETAILS", runtime::euid() == 0),
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
        if runtime::euid() == 0 {
            "multi-user.target"
        } else {
            "default.target"
        }
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
    if runtime::euid() != 0 {
        command.arg("--user");
    }
    command.args(args);
    process::capture(&mut command, Duration::from_secs(75), 1024 * 1024)?;
    Ok(())
}
fn service_available() -> bool {
    if runtime::euid() == 0 {
        Path::new("/run/systemd/system").is_dir()
    } else {
        systemctl(&["show-environment"]).is_ok()
    }
}
fn service_dir() -> PathBuf {
    if runtime::euid() == 0 {
        "/etc/systemd/system".into()
    } else {
        runtime::home().join(".config/systemd/user")
    }
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
                    include_bytes!("../sgpu-gpu-persistence.service"),
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
    if runtime::euid() == 0 {
        let bin = Path::new("/usr/local/bin");
        for name in BINARIES {
            let path = bin.join(name);
            if let Ok(st) = fs::symlink_metadata(&path)
                && (!st.file_type().is_symlink() || !fs::read_link(&path)?.starts_with(prefix))
            {
                return Err(format!("refusing to replace unrelated {}", path.display()).into());
            }
            transaction.link(&path, &prefix.join("bin").join(name))?;
        }
    } else {
        let file = runtime::home().join(".profile");
        let existing = match fs::symlink_metadata(&file) {
            Ok(st) if !st.is_file() || st.uid() != runtime::euid() => {
                return Err("unsafe shell profile; add sgpu/bin to PATH explicitly".into());
            }
            Ok(_) => String::from_utf8(
                runtime::read_regular(&file, 1024 * 1024, Some(runtime::euid()))?.0,
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let line = format!(
            "export PATH={}:\"$PATH\" # sgpu native",
            crate::cli::quote(&prefix.join("bin").to_string_lossy())
        );
        if !existing.lines().any(|l| l == line) {
            let st = fs::symlink_metadata(runtime::home())?;
            if st.uid() != runtime::euid() || st.mode() & 0o022 != 0 {
                return Err(
                    "profile directory writable by other users; add sgpu/bin to PATH explicitly"
                        .into(),
                );
            }
            transaction.write(
                &file,
                format!("{}\n{line}\n", existing.trim_end()).as_bytes(),
                0o644,
            )?;
        }
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
                if runtime::read_regular(&path, 65536, Some(0)).is_ok_and(|(bytes, _)| {
                    bytes == include_bytes!("../sgpu-gpu-persistence.service")
                }) {
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
        if !no_service && runtime::euid() != 0 {
            let profile = runtime::home().join(".profile");
            if let Ok((bytes, st)) =
                runtime::read_regular(&profile, 1024 * 1024, Some(runtime::euid()))
            {
                let line = format!(
                    "export PATH={}:\"$PATH\" # sgpu native",
                    crate::cli::quote(&prefix.join("bin").to_string_lossy())
                );
                let content = String::from_utf8(bytes)?;
                let filtered = content
                    .lines()
                    .filter(|l| *l != line)
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                runtime::atomic_write(&profile, filtered.as_bytes(), st.mode() & 0o777, true)?;
            }
        }
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
    match args.command {
        Action::Install {
            role,
            mode,
            prefix,
            bin_dir,
            no_service,
        } => install(
            &prefix_path(prefix)?,
            &source_dir(bin_dir)?,
            role,
            mode,
            no_service,
        ),
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
