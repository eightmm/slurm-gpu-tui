use crate::{
    model::Job,
    process::{self, Program},
    runtime, scheduler,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq)]
struct Identity {
    id: String,
    uid: i64,
}
struct Script {
    identity: Identity,
    text: Option<String>,
}
pub struct Fetcher {
    enabled: bool,
    sender: SyncSender<Identity>,
    receiver: Receiver<Script>,
    pending: BTreeMap<String, i64>,
    cache: BTreeMap<String, (i64, String)>,
    retry: BTreeMap<String, (i64, Instant)>,
}
impl Fetcher {
    pub fn new(enabled: bool) -> Self {
        let (sender, requests) = mpsc::sync_channel::<Identity>(32);
        let (results, receiver) = mpsc::sync_channel(32);
        let requests = Arc::new(Mutex::new(requests));
        for _ in 0..2 {
            let requests = requests.clone();
            let results = results.clone();
            std::thread::spawn(move || {
                loop {
                    let task = requests.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(identity) = task else {
                        break;
                    };
                    let text = process::run(
                        Program::Scontrol,
                        &["write", "batch_script", &identity.id, "-"],
                        Duration::from_secs(12),
                    )
                    .ok()
                    .filter(|s| !s.starts_with("job script retrieval failed"))
                    .map(|s| s.chars().take(16384).collect());
                    if results.send(Script { identity, text }).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            enabled,
            sender,
            receiver,
            pending: BTreeMap::new(),
            cache: BTreeMap::new(),
            retry: BTreeMap::new(),
        }
    }
    pub fn update(&mut self, jobs: &mut [Job], clean: bool) {
        if !self.enabled {
            return;
        }
        let live: BTreeSet<_> = jobs
            .iter()
            .filter(|j| clean && j.uid >= 0 && scheduler::valid_job_id(&j.jobid))
            .map(|j| j.jobid.clone())
            .collect();
        self.cache.retain(|id, _| live.contains(id));
        self.retry.retain(|id, _| live.contains(id));
        while let Ok(script) = self.receiver.try_recv() {
            self.pending.remove(&script.identity.id);
            if jobs.iter().any(|j| {
                live.contains(&j.jobid)
                    && j.jobid == script.identity.id
                    && j.uid == script.identity.uid
            }) {
                if let Some(text) = script.text {
                    self.retry.remove(&script.identity.id);
                    self.cache
                        .insert(script.identity.id, (script.identity.uid, text));
                } else {
                    self.retry.insert(
                        script.identity.id,
                        (
                            script.identity.uid,
                            Instant::now() + Duration::from_secs(30),
                        ),
                    );
                }
            }
        }
        for job in jobs {
            job.script.clear();
            if !live.contains(&job.jobid) {
                job.script.clear();
                continue;
            }
            if let Some((uid, text)) = self.cache.get(&job.jobid) {
                if *uid == job.uid {
                    job.script = text.clone();
                    continue;
                }
                self.cache.remove(&job.jobid);
            }
            if self
                .retry
                .get(&job.jobid)
                .is_some_and(|(uid, at)| *uid == job.uid && Instant::now() < *at)
            {
                continue;
            }
            if !self.pending.contains_key(&job.jobid)
                && self.pending.len() < 32
                && self
                    .sender
                    .try_send(Identity {
                        id: job.jobid.clone(),
                        uid: job.uid,
                    })
                    .is_ok()
            {
                self.pending.insert(job.jobid.clone(), job.uid);
            }
        }
    }
    pub fn inflight(&self) -> usize {
        self.pending.len()
    }
}

pub fn read_own(jobid: &str) -> crate::Result<String> {
    if !scheduler::valid_job_id(jobid) {
        return Err("invalid job ID".into());
    }
    let output = process::run(
        Program::Scontrol,
        &["write", "batch_script", jobid, "-"],
        Duration::from_secs(12),
    )?;
    Ok(output.chars().take(16384).collect())
}

pub fn sharing_enabled() -> bool {
    runtime::enabled("SLURM_GPU_TUI_SHARE_SCRIPTS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_failure_is_retried_after_backoff_and_owner_changes_invalidate_scripts() {
        let (sender, requests) = mpsc::sync_channel(32);
        let (results, receiver) = mpsc::sync_channel(32);
        let mut fetcher = Fetcher {
            enabled: true,
            sender,
            receiver,
            pending: BTreeMap::from([("42".into(), 1)]),
            cache: BTreeMap::new(),
            retry: BTreeMap::new(),
        };
        let mut jobs = vec![Job {
            jobid: "42".into(),
            uid: 1,
            ..Default::default()
        }];
        results
            .send(Script {
                identity: Identity {
                    id: "42".into(),
                    uid: 1,
                },
                text: None,
            })
            .unwrap();
        fetcher.update(&mut jobs, true);
        assert!(!fetcher.cache.contains_key("42"));
        assert!(requests.try_recv().is_err());
        fetcher.retry.get_mut("42").unwrap().1 = Instant::now();
        fetcher.update(&mut jobs, true);
        assert_eq!(requests.try_recv().unwrap().uid, 1);
        results
            .send(Script {
                identity: Identity {
                    id: "42".into(),
                    uid: 1,
                },
                text: Some("private script".into()),
            })
            .unwrap();
        jobs[0].uid = 2;
        fetcher.update(&mut jobs, true);
        assert!(jobs[0].script.is_empty());
        assert!(!fetcher.cache.contains_key("42"));
        assert_eq!(requests.try_recv().unwrap().uid, 2);
        fetcher.update(&mut jobs, false);
        assert!(jobs[0].script.is_empty());
    }
}
