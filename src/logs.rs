use crate::{
    Result,
    model::Job,
    process, runtime,
    scheduler::{self, LogSpec},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

pub fn owned_tail(path: &Path, owner: u32, limit: usize) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.uid() != owner {
        return Err("unsafe log source".into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let st = file.metadata()?;
    if !st.is_file() || st.uid() != owner || st.dev() != before.dev() || st.ino() != before.ino() {
        return Err("log source identity changed".into());
    }
    let limit = limit.min(1024 * 1024);
    file.seek(SeekFrom::Start(st.len().saturating_sub(limit as u64)))?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn helper_dispatch() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_none_or(|a| a != "__read-owned-log") {
        return false;
    }
    let result = (|| -> Result<()> {
        if args.len() != 4 {
            return Err("invalid helper arguments".into());
        }
        let limit = args[3]
            .to_str()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n <= 1024 * 1024)
            .ok_or("invalid helper bound")?;
        let bytes = owned_tail(Path::new(&args[2]), runtime::euid(), limit)?;
        std::io::stdout().write_all(&bytes)?;
        Ok(())
    })();
    std::process::exit(if result.is_ok() { 0 } else { 3 })
}

fn groups(user: &runtime::User) -> Result<Vec<libc::gid_t>> {
    let name = CString::new(user.name.as_str())?;
    let mut count = 32;
    let mut groups = vec![0; 32];
    let result =
        unsafe { libc::getgrouplist(name.as_ptr(), user.gid, groups.as_mut_ptr(), &mut count) };
    if result < 0 {
        if !(1..=65536).contains(&count) {
            return Err("invalid group count".into());
        }
        groups.resize(count as usize, 0);
        if unsafe { libc::getgrouplist(name.as_ptr(), user.gid, groups.as_mut_ptr(), &mut count) }
            < 0
        {
            return Err("group lookup failed".into());
        }
    }
    groups.truncate(count as usize);
    Ok(groups)
}

pub fn tail_as_owner(path: &Path, owner: u32, limit: usize) -> Result<Vec<u8>> {
    if runtime::euid() != 0 {
        return owned_tail(path, owner, limit);
    }
    let user = runtime::user(owner).ok_or("job owner is unavailable")?;
    let groups = groups(&user)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__read-owned-log")
        .arg(path)
        .arg(limit.min(1024 * 1024).to_string())
        .env_clear()
        .env("LANG", "C.UTF-8")
        .current_dir("/");
    // Only async-signal-safe credential syscalls run after fork. All lookups,
    // allocations and supplementary-group construction happen in the parent.
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                || libc::setgid(user.gid) != 0
                || libc::setuid(owner) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    process::capture(&mut command, Duration::from_secs(8), limit.min(1024 * 1024))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Seed {
    pub uid: u32,
    pub spec: LogSpec,
}

struct Task {
    jobid: String,
    seed: Seed,
    token: u64,
    limit: usize,
}
struct ReadResult {
    jobid: String,
    seed: Seed,
    token: u64,
    streams: Vec<(&'static str, &'static str, Option<Vec<u8>>)>,
}

struct Entry {
    seed: Option<Seed>,
    token: u64,
    next: f64,
    delay: f64,
    published: BTreeMap<String, String>,
    status: BTreeMap<String, String>,
}

pub struct Mirror {
    directory: PathBuf,
    enabled: bool,
    entries: BTreeMap<String, Entry>,
    inflight: BTreeSet<String>,
    sender: SyncSender<Task>,
    receiver: Receiver<ReadResult>,
    cursor: usize,
    next_token: u64,
}

impl Mirror {
    pub fn new(state: &Path, enabled: bool) -> Result<Self> {
        let directory = state.join("logs");
        runtime::secure_dir(&directory, 0o755)?;
        let (sender, requests) = mpsc::sync_channel::<Task>(32);
        let requests = Arc::new(Mutex::new(requests));
        let (results, receiver) = mpsc::sync_channel(32);
        for _ in 0..2 {
            let requests = requests.clone();
            let results = results.clone();
            std::thread::spawn(move || {
                loop {
                    let task = { requests.lock().unwrap_or_else(|e| e.into_inner()).recv() };
                    let Ok(task) = task else {
                        break;
                    };
                    let mut streams = Vec::new();
                    for (suffix, path) in
                        [("out", &task.seed.spec.out), ("err", &task.seed.spec.err)]
                    {
                        if path.as_os_str().is_empty() {
                            streams.push((
                                suffix,
                                if suffix == "err" && task.seed.spec.merged {
                                    "merged"
                                } else {
                                    "not-configured"
                                },
                                None,
                            ));
                        } else {
                            match tail_as_owner(path, task.seed.uid, task.limit) {
                                Ok(bytes) => streams.push((suffix, "mirrored", Some(bytes))),
                                Err(_) => streams.push((suffix, "unreadable", None)),
                            }
                        }
                    }
                    if results
                        .send(ReadResult {
                            jobid: task.jobid,
                            seed: task.seed,
                            token: task.token,
                            streams,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        let mut mirror = Self {
            directory,
            enabled,
            entries: BTreeMap::new(),
            inflight: BTreeSet::new(),
            sender,
            receiver,
            cursor: 0,
            next_token: 0,
        };
        if !enabled {
            mirror.clean_disk(&BTreeSet::new())?;
        }
        Ok(mirror)
    }

    fn stream_path(&self, id: &str, suffix: &str) -> PathBuf {
        self.directory.join(format!("{id}.{suffix}"))
    }
    fn unpublish(&self, id: &str) -> Result<()> {
        for suffix in ["out", "err"] {
            let path = self.stream_path(id, suffix);
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn clean_disk(&mut self, live: &BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(&self.directory)?.take(524288) {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((id, suffix)) = name.rsplit_once('.') else {
                continue;
            };
            if matches!(suffix, "out" | "err")
                && id.len() <= 200
                && scheduler::valid_job_id(id)
                && !live.contains(id)
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    pub fn inflight(&self) -> usize {
        self.inflight.len()
    }

    pub fn process(
        &mut self,
        jobs: &mut [Job],
        seeds: &BTreeMap<String, Seed>,
        now: f64,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let live: BTreeSet<_> = jobs
            .iter()
            .filter(|j| j.jobid.len() <= 200 && scheduler::valid_job_id(&j.jobid))
            .map(|j| j.jobid.clone())
            .collect();
        let gone: Vec<_> = self
            .entries
            .keys()
            .filter(|id| !live.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            self.entries.remove(&id);
            self.unpublish(&id)?;
        }
        for id in &live {
            let seed = seeds.get(id).cloned();
            if self.entries.get(id).is_none_or(|entry| entry.seed != seed) {
                self.next_token += 1;
                self.unpublish(id)?;
                self.entries.insert(
                    id.clone(),
                    Entry {
                        seed,
                        token: self.next_token,
                        next: 0.0,
                        delay: runtime::env_seconds("SLURM_GPU_TUI_LOG_MIRROR_SEC", 10.0, 0.1),
                        published: BTreeMap::new(),
                        status: BTreeMap::new(),
                    },
                );
            }
        }
        while let Ok(result) = self.receiver.try_recv() {
            self.inflight.remove(&result.jobid);
            if !self
                .entries
                .get(&result.jobid)
                .is_some_and(|e| e.token == result.token && e.seed.as_ref() == Some(&result.seed))
            {
                continue;
            }
            let mut unchanged = true;
            for (suffix, status, bytes) in result.streams {
                let path = self.stream_path(&result.jobid, suffix);
                if let Some(bytes) = bytes {
                    let same = runtime::read_regular(&path, 1024 * 1024, Some(runtime::euid()))
                        .is_ok_and(|(current, st)| current == bytes && st.mode() & 0o777 == 0o644);
                    if !same {
                        runtime::atomic_write(&path, &bytes, 0o644, false)?;
                        unchanged = false;
                    }
                    self.entries
                        .get_mut(&result.jobid)
                        .unwrap()
                        .published
                        .insert(suffix.into(), path.to_string_lossy().into_owned());
                } else {
                    match fs::remove_file(path) {
                        Ok(()) => (),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e.into()),
                    }
                    self.entries
                        .get_mut(&result.jobid)
                        .unwrap()
                        .published
                        .remove(suffix);
                }
                self.entries
                    .get_mut(&result.jobid)
                    .unwrap()
                    .status
                    .insert(suffix.into(), status.into());
            }
            let entry = self.entries.get_mut(&result.jobid).unwrap();
            let base = runtime::env_seconds("SLURM_GPU_TUI_LOG_MIRROR_SEC", 10.0, 0.1);
            entry.delay = if unchanged {
                (entry.delay * 2.0).min(runtime::env_seconds(
                    "SLURM_GPU_TUI_LOG_MIRROR_MAX_SEC",
                    60.0,
                    base,
                ))
            } else {
                base
            };
            entry.next = now + entry.delay;
        }
        let ids: Vec<_> = live.into_iter().collect();
        let start = self.cursor % ids.len().max(1);
        for offset in 0..ids.len() {
            let index = (start + offset) % ids.len();
            let id = &ids[index];
            let entry = self.entries.get(id).unwrap();
            if entry.seed.is_none()
                || entry.next > now
                || self.inflight.contains(id)
                || self.inflight.len() >= 32
            {
                continue;
            }
            let task = Task {
                jobid: id.clone(),
                seed: entry.seed.clone().unwrap(),
                token: entry.token,
                limit: runtime::env_seconds("SLURM_GPU_TUI_LOG_TAIL_BYTES", 65536.0, 0.0)
                    .min(1048576.0) as usize,
            };
            match self.sender.try_send(task) {
                Ok(()) => {
                    self.inflight.insert(id.clone());
                    self.cursor = index + 1;
                }
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => {
                    return Err("log reader workers unavailable".into());
                }
            }
        }
        for job in jobs {
            if let Some(entry) = self.entries.get(&job.jobid) {
                job.log_out = entry.published.get("out").cloned().unwrap_or_default();
                job.log_err = entry.published.get("err").cloned().unwrap_or_default();
                job.log_status = entry.status.clone();
                if entry.seed.is_none() {
                    job.log_status
                        .insert("out".into(), "metadata-unavailable".into());
                    job.log_status
                        .insert("err".into(), "metadata-unavailable".into());
                }
            }
        }
        Ok(())
    }
}

pub fn read_view(path: &Path, shared: bool, limit: usize) -> Result<String> {
    let owner = if shared { None } else { Some(runtime::euid()) };
    if shared {
        let (bytes, _) = runtime::read_regular(path, limit as u64, owner)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(String::from_utf8_lossy(&owned_tail(path, runtime::euid(), limit)?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn private_tail_requires_numeric_owner_and_refuses_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log");
        fs::write(&path, b"0123456789").unwrap();
        assert_eq!(owned_tail(&path, runtime::euid(), 4).unwrap(), b"6789");
        assert!(owned_tail(&path, runtime::euid().wrapping_add(1), 4).is_err());
        symlink(&path, tmp.path().join("link")).unwrap();
        assert!(owned_tail(&tmp.path().join("link"), runtime::euid(), 4).is_err());
    }
    #[test]
    fn invalidated_metadata_removes_public_tail_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        runtime::secure_dir(tmp.path(), 0o700).unwrap();
        let mut mirror = Mirror::new(tmp.path(), true).unwrap();
        fs::write(mirror.stream_path("42", "out"), "old data").unwrap();
        let mut jobs = vec![Job {
            jobid: "42".into(),
            ..Default::default()
        }];
        mirror.process(&mut jobs, &BTreeMap::new(), 0.0).unwrap();
        assert!(!mirror.stream_path("42", "out").exists());
        assert_eq!(jobs[0].log_status["out"], "metadata-unavailable");
        assert!(jobs[0].log_out.is_empty());
    }
}
