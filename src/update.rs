use crate::{Result, agent::Mode, runtime};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_BINARY: u64 = 128 * 1024 * 1024;
const CHANNEL: &str = ".sgpu-updates";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    protocol: u32,
    version: String,
    arch: String,
    bytes: u64,
}
fn version(raw: &str) -> Option<[u64; 3]> {
    let parts = raw
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<Vec<u64>, _>>()
        .ok()?;
    parts.try_into().ok()
}
fn architecture() -> &'static str {
    std::env::consts::ARCH
}

// Every directory is checked through its open fd. Only root-owned sticky shared
// ancestors may be writable: other users cannot replace root's next component.
fn trusted_dir(path: &Path, create: bool, owner: u32) -> Result<File> {
    if !path.is_absolute() {
        return Err("update path must be absolute".into());
    }
    let mut dir = runtime::open_dir(Path::new("/"))?;
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => {
                if create {
                    use std::os::unix::ffi::OsStrExt;
                    let name = std::ffi::CString::new(name.as_bytes())?;
                    let result = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) };
                    if result != 0
                        && std::io::Error::last_os_error().kind()
                            != std::io::ErrorKind::AlreadyExists
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                dir = runtime::open_dir_at(&dir, name)?;
                let st = dir.metadata()?;
                if (st.uid() != 0 && st.uid() != owner)
                    || (st.mode() & 0o022 != 0
                        && !(st.uid() == 0 && st.mode() & libc::S_ISVTX != 0))
                {
                    return Err("update path has untrusted ownership or write permissions".into());
                }
            }
            _ => return Err("unsafe update path component".into()),
        }
    }
    let st = dir.metadata()?;
    if st.uid() != owner || st.mode() & 0o022 != 0 {
        return Err("update directory must be owned by root and not group/world writable".into());
    }
    Ok(dir)
}
fn owned_read(dir: &File, name: &str, limit: u64, owner: u32) -> Result<Vec<u8>> {
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    use std::os::fd::FromRawFd;
    let file = unsafe { File::from_raw_fd(fd) };
    let st = file.metadata()?;
    if !st.is_file() || st.uid() != owner || st.mode() & 0o022 != 0 || st.len() > limit {
        return Err("untrusted or oversized update file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("update file exceeded limit".into());
    }
    Ok(bytes)
}
fn elf(bytes: &[u8]) -> bool {
    let machine = match architecture() {
        "x86_64" => 62,
        "aarch64" => 183,
        "x86" => 3,
        "arm" => 40,
        "riscv64" => 243,
        _ => return false,
    };
    bytes.len() >= 64
        && bytes[..4] == *b"\x7fELF"
        && bytes[5] == 1
        && u16::from_le_bytes([bytes[18], bytes[19]]) == machine
}
fn channel() -> PathBuf {
    runtime::agent_dir().join(CHANNEL).join(architecture())
}

pub fn enabled() -> bool {
    std::env::var("SLURM_GPU_TUI_AGENT_AUTO_UPDATE")
        .map(|s| !matches!(s.to_lowercase().as_str(), "0" | "false" | "off" | "no"))
        .unwrap_or(true)
}

pub fn publish(binary: &Path) -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    if runtime::euid() != 0 {
        return Err("only root may publish executable updates".into());
    }
    publish_at(binary, &channel(), 0)
}
fn publish_at(binary: &Path, path: &Path, owner: u32) -> Result<()> {
    let (bytes, st) = runtime::read_regular(binary, MAX_BINARY, Some(owner))?;
    if st.mode() & 0o022 != 0 || !elf(&bytes) {
        return Err("invalid agent executable".into());
    }
    trusted_dir(path, true, owner)?;
    let name = format!("{}.elf", crate::VERSION);
    runtime::atomic_write(&path.join(name), &bytes, 0o644, true)?;
    runtime::atomic_write(
        &path.join("latest.json"),
        &serde_json::to_vec(&Release {
            protocol: 1,
            version: crate::VERSION.into(),
            arch: architecture().into(),
            bytes: bytes.len() as u64,
        })?,
        0o644,
        true,
    )?;
    Ok(())
}

fn fetch(
    path: &Path,
    current: &str,
    blocked: &str,
    owner: u32,
) -> Result<Option<(Release, Vec<u8>)>> {
    let dir = trusted_dir(path, false, owner)?;
    let release: Release = serde_json::from_slice(&owned_read(&dir, "latest.json", 4096, owner)?)?;
    let target = version(&release.version).ok_or("invalid update version")?;
    if release.protocol != 1 || release.arch != architecture() || release.bytes > MAX_BINARY {
        return Err("unsupported update protocol, architecture or size".into());
    }
    if target <= version(current).ok_or("invalid current version")?
        || version(blocked).is_some_and(|failed| target <= failed)
    {
        return Ok(None);
    }
    let bytes = owned_read(&dir, &format!("{}.elf", release.version), MAX_BINARY, owner)?;
    if bytes.len() as u64 != release.bytes || !elf(&bytes) {
        return Err("incomplete or incompatible update".into());
    }
    Ok(Some((release, bytes)))
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    current: String,
    previous: String,
    blocked: String,
}
impl State {
    fn block(&mut self, release: &str) {
        if version(&self.blocked) < version(release) {
            self.blocked = release.into();
        }
    }
}
fn cached(root: &Path, release: &str) -> PathBuf {
    root.join(release).join("sgpu-agent")
}
fn save(root: &Path, state: &State) -> Result<()> {
    runtime::atomic_write(
        &root.join("state.json"),
        &serde_json::to_vec(state)?,
        0o600,
        true,
    )?;
    // Only version directories containing our one executable are eligible.
    for entry in std::fs::read_dir(root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if version(&name).is_none()
            || [
                crate::VERSION,
                &state.current,
                &state.previous,
                &state.blocked,
            ]
            .contains(&name.as_str())
        {
            continue;
        }
        if trusted_dir(&entry.path(), false, 0).is_err() {
            continue;
        }
        let binary = entry.path().join("sgpu-agent");
        if std::fs::symlink_metadata(&binary).is_ok_and(|s| s.is_file() && s.uid() == 0) {
            let _ = std::fs::remove_file(binary);
            let _ = std::fs::remove_dir(entry.path());
        }
    }
    Ok(())
}
fn stage(root: &Path, release: &str, bytes: &[u8], owner: u32) -> Result<PathBuf> {
    if version(release).is_none() {
        return Err("invalid cached version".into());
    }
    let parent = root.join(release);
    trusted_dir(&parent, true, owner)?;
    let path = parent.join("sgpu-agent");
    runtime::atomic_write(&path, bytes, 0o755, true)?;
    Ok(path)
}
struct Worker {
    child: Child,
    ready: bool,
    last_publish: Instant,
}
impl Worker {
    fn start(path: &Path, mode: Mode) -> Result<Self> {
        let child = Command::new(path)
            .args([
                "--worker",
                "--mode",
                if mode == Mode::Gpu { "gpu" } else { "cpu" },
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let fd = child
            .stdout
            .as_ref()
            .ok_or("missing readiness pipe")?
            .as_raw_fd();
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            child,
            ready: false,
            last_publish: Instant::now(),
        })
    }
    fn observe(&mut self) -> Result<bool> {
        {
            let mut signal = [0u8; 128];
            match self
                .child
                .stdout
                .as_mut()
                .ok_or("missing readiness pipe")?
                .read(&mut signal)
            {
                Ok(n) if signal[..n].contains(&b'R') => {
                    self.ready = true;
                    self.last_publish = Instant::now();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                _ => (),
            }
        }
        Ok(self.child.try_wait()?.is_none())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn supervise(mode: Mode, stopped: Arc<AtomicBool>) -> Result<()> {
    let root = std::env::var_os("SLURM_GPU_TUI_AGENT_UPDATE_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("SLURM_GPU_TUI_AGENT_RUNTIME_DIR")
                .map(|p| PathBuf::from(p).join("updates"))
        })
        .unwrap_or_else(|| "/var/lib/sgpu-agent".into());
    trusted_dir(&root, true, 0)?;
    let mut own = File::open("/proc/self/exe")?;
    let mut bytes = Vec::new();
    (&mut own).take(MAX_BINARY + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BINARY || !elf(&bytes) {
        return Err("invalid supervisor executable".into());
    }
    stage(&root, crate::VERSION, &bytes, 0)?;
    let mut state = trusted_dir(&root, false, 0)
        .and_then(|d| owned_read(&d, "state.json", 4096, 0))
        .ok()
        .and_then(|b| serde_json::from_slice::<State>(&b).ok())
        .unwrap_or_default();
    if version(&state.current).is_none_or(|v| v < version(crate::VERSION).unwrap()) {
        state.current = crate::VERSION.into();
        state.previous = crate::VERSION.into();
    }
    let valid_cache = |release: &str| -> bool {
        version(release).is_some()
            && trusted_dir(&root.join(release), false, 0)
                .and_then(|d| owned_read(&d, "sgpu-agent", MAX_BINARY, 0))
                .is_ok_and(|b| elf(&b))
    };
    if !valid_cache(&state.current) {
        state.current = crate::VERSION.into();
    }
    if !valid_cache(&state.previous) {
        state.previous = crate::VERSION.into();
    }
    let mut active = state.current.clone();
    let mut trial = active != crate::VERSION;
    let mut worker = match Worker::start(&cached(&root, &active), mode) {
        Ok(worker) => worker,
        Err(error) if active != crate::VERSION => {
            eprintln!("[update] cached {active} cannot start: {error}; using previous agent");
            state.block(&active);
            active = if state.previous != active && valid_cache(&state.previous) {
                state.previous.clone()
            } else {
                crate::VERSION.into()
            };
            state.current = active.clone();
            state.previous = crate::VERSION.into();
            save(&root, &state)?;
            trial = false;
            Worker::start(&cached(&root, &active), mode)?
        }
        Err(error) => return Err(error),
    };
    let mut next = Instant::now() + Duration::from_secs(5);
    let interval = Duration::from_secs_f64(runtime::env_seconds(
        "SLURM_GPU_TUI_AGENT_UPDATE_SEC",
        60.0,
        1.0,
    ));
    let startup = Duration::from_secs_f64(runtime::env_seconds(
        "SLURM_GPU_TUI_AGENT_UPDATE_STARTUP_SEC",
        120.0,
        1.0,
    ));
    while !stopped.load(Ordering::Relaxed) {
        let alive = worker.observe()?;
        if !alive || (worker.last_publish.elapsed() >= startup && active != crate::VERSION) {
            if active != crate::VERSION {
                let fallback = if trial && state.current != active {
                    state.current.clone()
                } else {
                    state.previous.clone()
                };
                let fallback = if fallback != active && valid_cache(&fallback) {
                    fallback
                } else {
                    crate::VERSION.into()
                };
                eprintln!("[update] {active} failed; rolling back to {}", fallback);
                state.block(&active);
                active = fallback;
                state.current = active.clone();
                if !trial {
                    state.previous = crate::VERSION.into();
                }
                save(&root, &state)?;
                trial = false;
            } else if !alive {
                return Err("baseline agent worker stopped".into());
            }
            drop(worker);
            worker = Worker::start(&cached(&root, &active), mode)?;
        }
        if trial && worker.ready {
            if state.current != active {
                state.previous = state.current.clone();
            }
            state.current = active.clone();
            save(&root, &state)?;
            trial = false;
            eprintln!("[update] agent {active} is publishing successfully");
        }
        if Instant::now() >= next && !trial {
            next = Instant::now() + interval;
            match fetch(&channel(), &active, &state.blocked, 0) {
                Ok(Some((release, bytes))) => {
                    match stage(&root, &release.version, &bytes, 0) {
                        Ok(path) => {
                            // Loaders/ABI failures are caught before stopping the live worker.
                            let probe = crate::process::capture(
                                Command::new(&path).arg("--version"),
                                Duration::from_secs(5),
                                4096,
                            );
                            if !probe.is_ok_and(|s| {
                                String::from_utf8_lossy(&s).trim()
                                    == format!("sgpu {}", release.version)
                            }) {
                                eprintln!("[update] incompatible executable {}", release.version);
                                state.block(&release.version);
                                save(&root, &state)?;
                                continue;
                            }
                            drop(worker);
                            match Worker::start(&path, mode) {
                                Ok(new) => {
                                    worker = new;
                                    active = release.version;
                                    trial = true;
                                }
                                Err(error) => {
                                    eprintln!("[update] start failed: {error}");
                                    state.block(&release.version);
                                    save(&root, &state)?;
                                    worker = Worker::start(&cached(&root, &state.current), mode)?;
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("[update] staging failed; keeping {active}: {error}")
                        }
                    }
                }
                Ok(None) => (),
                Err(error) => eprintln!("[update] keeping {active}: {error}"),
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub fn ready() -> Result<()> {
    // Bypass stdout buffering: each successful publication is one heartbeat.
    if unsafe { libc::write(libc::STDOUT_FILENO, b"R".as_ptr().cast(), 1) } == 1 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(())
    } else {
        Err(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn fixture() -> (tempfile::TempDir, Release, Vec<u8>) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[5] = 1;
        let machine: u16 = match architecture() {
            "x86_64" => 62,
            "aarch64" => 183,
            "x86" => 3,
            "arm" => 40,
            "riscv64" => 243,
            _ => panic!("test architecture unsupported"),
        };
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        let release = Release {
            protocol: 1,
            version: "9.0.0".into(),
            arch: architecture().into(),
            bytes: 64,
        };
        fs::write(root.path().join("9.0.0.elf"), &bytes).unwrap();
        fs::write(
            root.path().join("latest.json"),
            serde_json::to_vec(&release).unwrap(),
        )
        .unwrap();
        for name in ["latest.json", "9.0.0.elf"] {
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o644)).unwrap();
        }
        (root, release, bytes)
    }
    #[test]
    fn channel_checks_version_architecture_size_and_never_accepts_a_downgrade() {
        let (root, mut release, _) = fixture();
        let get = |current, blocked| fetch(root.path(), current, blocked, runtime::euid());
        assert!(get("2.2.0", "").unwrap().is_some());
        assert!(get("9.0.0", "").unwrap().is_none());
        assert!(get("10.0.0", "").unwrap().is_none());
        assert!(get("2.2.0", "9.0.0").unwrap().is_none());
        release.arch = "wrong-architecture".into();
        fs::write(
            root.path().join("latest.json"),
            serde_json::to_vec(&release).unwrap(),
        )
        .unwrap();
        assert!(get("2.2.0", "").is_err());
        release.arch = architecture().into();
        release.bytes = 65;
        fs::write(
            root.path().join("latest.json"),
            serde_json::to_vec(&release).unwrap(),
        )
        .unwrap();
        assert!(get("2.2.0", "").is_err());
        for name in ["../9.0.0", "9.0.0/evil", "9.0", "9.0.0.0"] {
            assert!(version(name).is_none());
        }
    }
    #[test]
    fn publication_round_trips_and_refuses_untrusted_executables_without_changing_latest() {
        let (root, _, bytes) = fixture();
        let channel = root.path().join("channel");
        let binary = root.path().join("9.0.0.elf");
        publish_at(&binary, &channel, runtime::euid()).unwrap();
        let (release, received) = fetch(&channel, "1.0.0", "", runtime::euid())
            .unwrap()
            .unwrap();
        assert_eq!(release.version, crate::VERSION);
        assert_eq!(received, bytes);
        let before = fs::read(channel.join("latest.json")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(publish_at(&binary, &channel, runtime::euid()).is_err());
        assert_eq!(before, fs::read(channel.join("latest.json")).unwrap());
    }
    #[test]
    fn channel_rejects_writable_symlink_and_wrong_owner_files_and_ancestors() {
        let (root, _, _) = fixture();
        let file = root.path().join("9.0.0.elf");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(fetch(root.path(), "2.2.0", "", runtime::euid()).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fetch(root.path(), "2.2.0", "", runtime::euid().wrapping_add(1)).is_err());
        fs::rename(&file, root.path().join("other")).unwrap();
        symlink("other", &file).unwrap();
        assert!(fetch(root.path(), "2.2.0", "", runtime::euid()).is_err());
        fs::remove_file(&file).unwrap();
        fs::rename(root.path().join("other"), &file).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(fetch(root.path(), "2.2.0", "", runtime::euid()).is_err());
    }
}
