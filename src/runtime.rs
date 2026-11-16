use crate::{Result, model::Snapshot};
use std::{
    collections::{BTreeSet, hash_map::RandomState},
    ffi::{CStr, CString},
    fs::{self, File, OpenOptions},
    hash::BuildHasher,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn env_seconds(name: &str, default: f64, min: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| crate::model::number(&s))
        .filter(|v| *v >= min)
        .unwrap_or(default)
}

pub fn enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|s| {
        !matches!(
            s.trim().to_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| user(euid()).map(|u| u.home))
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("SLURM_GPU_TUI_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp/slurm-gpu-tui".into())
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("SLURM_GPU_TUI_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if euid() == 0 {
                "/var/lib/sgpu".into()
            } else {
                home().join(".sgpu/state")
            }
        })
}

pub fn state_candidates() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("SLURM_GPU_TUI_STATE_DIR") {
        return vec![path.into()];
    }
    vec![
        "/var/lib/sgpu".into(),
        home().join(".sgpu/state"),
        data_dir(),
    ]
}

pub fn agent_dir() -> PathBuf {
    std::env::var_os("SLURM_GPU_TUI_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".sgpu/nodes"))
}

pub fn agent_runtime(name: &str) -> PathBuf {
    if let Some(directory) = std::env::var_os("SLURM_GPU_TUI_AGENT_RUNTIME_DIR") {
        return PathBuf::from(directory).join(format!("sgpu-agent-{}.{name}", euid()));
    }
    if euid() == 0 && Path::new("/run").is_dir() {
        PathBuf::from(format!("/run/sgpu-agent.{name}"))
    } else {
        std::env::temp_dir().join(format!("sgpu-agent-{}.{name}", euid()))
    }
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn check_directory(file: &File, shared: bool) -> Result<()> {
    let st = file.metadata()?;
    if !st.is_dir() || (!shared && (st.uid() != 0 && st.uid() != euid() || st.mode() & 0o022 != 0))
    {
        return Err("unsafe runtime directory ownership or permissions".into());
    }
    if shared && st.mode() & 0o002 != 0 && st.mode() & libc::S_ISVTX == 0 {
        return Err("shared agent directory must have the sticky bit".into());
    }
    Ok(())
}

pub fn secure_dir(path: &Path, mode: u32) -> Result<()> {
    fs::create_dir_all(path)?;
    let file = open_directory(path)?;
    let st = file.metadata()?;
    if st.uid() != euid() && euid() != 0 {
        return Err("runtime directory belongs to another UID".into());
    }
    if euid() == 0 && st.uid() != 0 {
        // SAFETY: file owns an open directory fd; numeric UID/GID are valid.
        if unsafe { libc::fchown(file.as_raw_fd(), 0, !0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    // SAFETY: chmod applies to the already-open directory, not a racing path.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    check_directory(&file, false)
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TEMP_NAMESPACE: OnceLock<u64> = OnceLock::new();

struct NfsIdentity {
    uid: u32,
    gid: u32,
    // Filesystem credentials belong to this thread and must be restored here.
    _thread: std::marker::PhantomData<*mut ()>,
}
impl NfsIdentity {
    fn current() -> (u32, u32) {
        // SAFETY: an invalid ID queries the current IDs without changing them.
        unsafe {
            (
                libc::setfsuid(u32::MAX) as u32,
                libc::setfsgid(u32::MAX) as u32,
            )
        }
    }
    fn anonymous() -> Result<Self> {
        let (uid, gid) = Self::current();
        let guard = Self {
            uid,
            gid,
            _thread: std::marker::PhantomData,
        };
        // Keep process credentials for sampling/signals; change only this
        // thread's filesystem checks for the default NFS squash identity.
        unsafe {
            libc::setfsgid(65534);
            libc::setfsuid(65534);
        }
        if Self::current() != (65534, 65534) {
            return Err("could not select the NFS publication identity".into());
        }
        Ok(guard)
    }
}
impl Drop for NfsIdentity {
    fn drop(&mut self) {
        // SAFETY: these IDs were saved before this scope changed them.
        unsafe {
            libc::setfsuid(self.uid);
            libc::setfsgid(self.gid);
        }
        if Self::current() != (self.uid, self.gid) {
            // Continuing with unexpected credentials could corrupt later work.
            std::process::abort();
        }
    }
}

fn is_nfs(dir: &File) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: dir is open and stat is read only after fstatfs initializes it.
    unsafe {
        libc::fstatfs(dir.as_raw_fd(), stat.as_mut_ptr()) == 0
            && stat.assume_init().f_type == libc::NFS_SUPER_MAGIC
    }
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32, durable: bool, shared: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = open_directory(parent)?;
    check_directory(&dir, shared)?;
    let name = path.file_name().ok_or("missing destination name")?;
    let result = write_at(&dir, name, bytes, mode, durable);
    let permission_denied = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<io::Error>())
        .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
    if shared && permission_denied && euid() == 0 && is_nfs(&dir) {
        let identity = NfsIdentity::anonymous()?;
        // Reuse the checked directory fd: no new path traversal under the
        // temporary identity. Executable updates never enter this branch.
        let result = write_at(&dir, name, bytes, mode, durable);
        drop(identity);
        return result;
    }
    result
}

fn write_at(
    dir: &File,
    name: &std::ffi::OsStr,
    bytes: &[u8],
    mode: u32,
    durable: bool,
) -> Result<()> {
    let target = CString::new(name.as_bytes())?;
    // Shared writers on different hosts can have the same PID and sequence.
    let namespace = TEMP_NAMESPACE.get_or_init(|| RandomState::new().hash_one(()));
    let temporary = CString::new(format!(
        ".sgpu-{}-{namespace:016x}-{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))?;
    // SAFETY: C strings and directory fd stay valid throughout these syscalls.
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if raw < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::PermissionDenied && euid() == 0 && is_nfs(dir) {
            // Exclusive NFS creation can succeed before setting root's
            // attributes fails. Remove only our empty squash-owned partial.
            discard_squashed_partial(dir, &temporary, mode);
        }
        return Err(error.into());
    }
    // SAFETY: successful openat returned a new owned fd.
    let mut file = unsafe { File::from_raw_fd(raw) };
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if durable {
            file.sync_all()?;
        }
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                temporary.as_ptr(),
                dir.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if durable {
            dir.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: only our unique temporary directory entry is removed.
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

fn discard_squashed_partial(dir: &File, name: &CStr, mode: u32) {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory and name stay valid; metadata is used only after
    // fstatat succeeds, without following a potentially substituted symlink.
    unsafe {
        if libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            return;
        }
        let stat = stat.assume_init();
        if stat.st_mode & libc::S_IFMT == libc::S_IFREG
            && stat.st_uid == 65534
            && stat.st_size == 0
            && stat.st_nlink == 1
            && (stat.st_mode & 0o7777 == mode || stat.st_mode & 0o7777 == 0)
        {
            libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0);
        }
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32, durable: bool) -> Result<()> {
    write_atomic(path, bytes, mode, durable, false)
}

pub fn publish_agent(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(path, bytes, 0o644, false, true)
}

pub fn read_regular(
    path: &Path,
    limit: u64,
    owner: Option<u32>,
) -> Result<(Vec<u8>, fs::Metadata)> {
    let start = Instant::now();
    let mut file = loop {
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path);
        match opened {
            // NFS write delegations can briefly reject local nonblocking
            // opens while the kernel recalls the lease. Keep FIFO safety.
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_millis(250) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => break result?,
        }
    };
    let st = file.metadata()?;
    if !st.is_file() || owner.is_some_and(|uid| st.uid() != uid) || st.len() > limit {
        return Err("unsafe or oversized file".into());
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("file grew past read limit".into());
    }
    Ok((bytes, st))
}

pub fn read_snapshot(max_age: Option<f64>) -> Result<Snapshot> {
    let (bytes, st) = read_regular(&data_dir().join("data.json"), 64 * 1024 * 1024, None)?;
    let age = now() - st.mtime() as f64 - st.mtime_nsec() as f64 / 1e9;
    if max_age.is_some_and(|max| age > max || age < -5.0) {
        return Err("collector snapshot is stale".into());
    }
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    if !raw.is_object() {
        return Err("snapshot is not an object".into());
    }
    let snapshot: Snapshot = serde_json::from_value(raw)?;
    if snapshot.version != 1 {
        return Err("unsupported snapshot version".into());
    }
    Ok(snapshot)
}

pub fn trusted_uids(dir: &Path) -> BTreeSet<u32> {
    if let Ok(raw) = std::env::var("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS")
        && !raw.trim().is_empty()
    {
        return raw
            .split([',', ' '])
            .filter_map(|s| s.parse().ok())
            .collect();
    }
    let mut out = BTreeSet::from([0, euid(), 65534]);
    if let Ok(st) = fs::symlink_metadata(dir) {
        out.insert(st.uid());
    }
    out
}

pub struct Lock(File);

pub fn lock_is_held(path: &Path) -> Result<bool> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let st = file.metadata()?;
    if !st.is_file() || st.uid() != euid() {
        return Err("unsafe lock file".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(true)
    } else {
        Err(error.into())
    }
}

impl Lock {
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?;
        let st = file.metadata()?;
        if !st.is_file() || st.uid() != euid() {
            return Err("unsafe lock file".into());
        }
        let start = std::time::Instant::now();
        loop {
            // SAFETY: file owns a live fd; flock modifies only its lock state.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if start.elapsed() >= timeout {
                return Err("another instance holds the lock".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn held(&self) -> bool {
        self.0.metadata().is_ok()
    }
}

#[derive(Clone, Debug)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: PathBuf,
}

pub fn user(uid: u32) -> Option<User> {
    let mut buffer = vec![0u8; 65536];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // SAFETY: buffer and output pointers remain live; initialized entry is read only on success.
    if unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    } != 0
        || result.is_null()
    {
        return None;
    }
    let entry = unsafe { entry.assume_init() };
    Some(User {
        uid,
        gid: entry.pw_gid,
        name: unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned(),
        home: PathBuf::from(
            unsafe { CStr::from_ptr(entry.pw_dir) }
                .to_string_lossy()
                .into_owned(),
        ),
    })
}

pub fn hostname() -> Result<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most the provided buffer length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let end = buf
        .iter()
        .position(|b| *b == 0)
        .ok_or("hostname too long")?;
    let host = std::str::from_utf8(&buf[..end])?
        .split('.')
        .next()
        .unwrap_or("");
    if !valid_host(host) {
        return Err("unsafe hostname".into());
    }
    Ok(host.to_string())
}

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        && host != "."
        && host != ".."
}

pub fn open_dir_at(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}

pub fn open_dir(path: &Path) -> io::Result<File> {
    open_directory(path)
}

pub fn read_at(parent: &File, name: &str, limit: u64) -> Result<Vec<u8>> {
    let name = CString::new(name)?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(raw) };
    if !file.metadata()?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("record too large".into());
    }
    Ok(bytes)
}

pub fn first_line_at(parent: &File, name: &str, limit: usize) -> Result<String> {
    let name = CString::new(name)?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(raw) };
    if !file.metadata()?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = vec![0; limit + 1];
    let size = file.read(&mut bytes)?;
    let end = bytes[..size]
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(size);
    if end > limit {
        return Err("first line too long".into());
    }
    bytes.truncate(end);
    Ok(String::from_utf8(bytes)?)
}

pub fn daemonize(log: &Path) -> Result<bool> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(log)?;
    let st = file.metadata()?;
    if !st.is_file() || st.uid() != euid() {
        return Err("unsafe daemon log".into());
    }
    let null = File::open("/dev/null")?;
    // Called at program startup, before spawning worker threads. Child branches
    // return normally into main instead of unwinding any shared thread runtime.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid > 0 {
        return Ok(false);
    }
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid > 0 {
        return Ok(false);
    }
    for (source, target) in [
        (null.as_raw_fd(), 0),
        (file.as_raw_fd(), 1),
        (file.as_raw_fd(), 2),
    ] {
        if unsafe { libc::dup2(source, target) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    std::env::set_current_dir("/")?;
    Ok(true)
}

pub fn owned_dir_fd(path: &Path) -> Result<OwnedFd> {
    let name = cstring(path)?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        fs::MetadataExt,
        fs::{PermissionsExt, symlink},
    };

    #[test]
    #[ignore = "requires root; scripts/check.sh runs this with sudo when available"]
    fn root_nfs_identity_is_thread_local_and_restores_after_failure() {
        assert_eq!(euid(), 0);
        let original = NfsIdentity::current();
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let dir = open_directory(tmp.path()).unwrap();
        fs::write(tmp.path().join("root-owned"), b"keep").unwrap();
        fs::write(tmp.path().join("root-empty"), b"").unwrap();
        let (request, receive) = std::sync::mpsc::channel();
        let (answer, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            receive.recv().unwrap();
            answer.send(NfsIdentity::current()).unwrap();
        });
        {
            let _identity = NfsIdentity::anonymous().unwrap();
            assert_eq!(NfsIdentity::current(), (65534, 65534));
            assert_eq!(euid(), 0);
            request.send(()).unwrap();
            assert_eq!(result.recv().unwrap(), original);
            write_at(
                &dir,
                std::ffi::OsStr::new("telemetry"),
                b"published",
                0o644,
                false,
            )
            .unwrap();
            write_at(&dir, std::ffi::OsStr::new("partial"), b"", 0o644, false).unwrap();
            write_at(&dir, std::ffi::OsStr::new("linked"), b"", 0o644, false).unwrap();
            write_at(
                &dir,
                std::ffi::OsStr::new("nonempty"),
                b"keep",
                0o644,
                false,
            )
            .unwrap();
            write_at(
                &dir,
                std::ffi::OsStr::new("different-mode"),
                b"",
                0o600,
                false,
            )
            .unwrap();
            assert!(
                write_at(
                    &dir,
                    std::ffi::OsStr::new("root-owned"),
                    b"replace",
                    0o644,
                    false
                )
                .is_err()
            );
        }
        worker.join().unwrap();
        assert_eq!(NfsIdentity::current(), original);
        let st = fs::metadata(tmp.path().join("telemetry")).unwrap();
        assert_eq!(
            (st.uid(), st.gid(), st.mode() & 0o777),
            (65534, 65534, 0o644)
        );
        assert_eq!(fs::read(tmp.path().join("root-owned")).unwrap(), b"keep");
        fs::hard_link(tmp.path().join("linked"), tmp.path().join("alias")).unwrap();
        for name in [
            "partial",
            "nonempty",
            "different-mode",
            "root-owned",
            "root-empty",
            "linked",
        ] {
            discard_squashed_partial(&dir, &CString::new(name).unwrap(), 0o644);
        }
        assert!(!tmp.path().join("partial").exists());
        assert_eq!(fs::read(tmp.path().join("nonempty")).unwrap(), b"keep");
        assert!(tmp.path().join("different-mode").exists());
        assert!(tmp.path().join("root-owned").exists());
        assert!(tmp.path().join("root-empty").exists());
        assert!(tmp.path().join("linked").exists());
        let panic = std::panic::catch_unwind(|| {
            let _identity = NfsIdentity::anonymous().unwrap();
            panic!("publication failure");
        });
        assert!(panic.is_err());
        assert_eq!(NfsIdentity::current(), original);
    }

    #[test]
    fn atomic_publication_replaces_symlinks_and_defeats_umask() {
        let tmp = tempfile::tempdir().unwrap();
        secure_dir(tmp.path(), 0o700).unwrap();
        let dest = tmp.path().join("data");
        let other = tmp.path().join("other");
        fs::write(&other, "original").unwrap();
        symlink(&other, &dest).unwrap();
        atomic_write(&dest, b"published", 0o644, true).unwrap();
        assert_eq!(fs::read(&other).unwrap(), b"original");
        assert_eq!(fs::metadata(dest).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn regular_reads_retry_lease_recalls_and_bound_an_unreleased_lease() {
        use std::sync::{Arc, atomic::AtomicBool};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("payload");
        let mut owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        owner.write_all(b"payload").unwrap();
        let notified = Arc::new(AtomicBool::new(false));
        let handler = signal_hook::flag::register(libc::SIGIO, notified.clone()).unwrap();
        // SAFETY: owner is an open, regular file owned by this test user.
        assert_eq!(
            unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) },
            0
        );
        let release = owner.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            // SAFETY: release keeps the leased file descriptor alive.
            assert_eq!(
                unsafe { libc::fcntl(release.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) },
                0
            );
        });
        let read = read_regular(&path, 32, None);
        let released = worker.join();
        // SAFETY: the first read has closed its descriptor before reacquiring.
        assert_eq!(
            unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) },
            0
        );
        let start = Instant::now();
        let blocked = read_regular(&path, 32, None);
        let elapsed = start.elapsed();
        // SAFETY: owner still owns this lease; release it before assertions.
        unsafe {
            libc::fcntl(owner.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK);
        }
        signal_hook::low_level::unregister(handler);
        released.unwrap();
        assert_eq!(read.unwrap().0, b"payload");
        assert!(notified.load(Ordering::Relaxed));
        assert!(
            blocked
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn unsafe_directory_and_unbounded_reads_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(atomic_write(&tmp.path().join("x"), b"x", 0o644, false).is_err());
        secure_dir(tmp.path(), 0o755).unwrap();
        fs::write(tmp.path().join("x"), "long").unwrap();
        assert!(read_regular(&tmp.path().join("x"), 2, None).is_err());
        symlink(tmp.path().join("x"), tmp.path().join("link")).unwrap();
        assert!(read_regular(&tmp.path().join("link"), 20, None).is_err());
        let fifo = tmp.path().join("fifo");
        let raw = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: raw is a valid NUL-terminated path in the test directory.
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        let (answer, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            answer.send(read_regular(&fifo, 20, None).is_err()).unwrap()
        });
        assert!(result.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn locks_do_not_truncate_or_follow_links() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        fs::write(&path, "keep").unwrap();
        let first = Lock::acquire(&path, Duration::ZERO).unwrap();
        assert!(first.held());
        assert!(Lock::acquire(&path, Duration::ZERO).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "keep");
    }
}
