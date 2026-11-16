use crate::{
    model::{Job, PendingJob},
    quickview, rules,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Clone, Copy)]
pub enum Entry<'a> {
    Running(&'a Job),
    Pending(&'a PendingJob),
}
impl<'a> Entry<'a> {
    pub fn id(self) -> &'a str {
        match self {
            Self::Running(j) => &j.jobid,
            Self::Pending(j) => &j.jobid,
        }
    }
    pub fn user(self) -> &'a str {
        match self {
            Self::Running(j) => &j.user,
            Self::Pending(j) => &j.user,
        }
    }
    pub fn partition(self) -> &'a str {
        match self {
            Self::Running(j) => &j.partition,
            Self::Pending(j) => &j.partition,
        }
    }
    pub fn gpus(self) -> u32 {
        match self {
            Self::Running(j) => j.gpu_count,
            Self::Pending(j) => j.gpu_count,
        }
    }
    pub fn cpus(self) -> u32 {
        match self {
            Self::Running(j) => j.cpu_count,
            Self::Pending(j) => j.cpu_count,
        }
    }
    pub fn priority(self) -> Option<u64> {
        match self {
            Self::Running(j) => &j.priority,
            Self::Pending(j) => &j.priority,
        }
        .parse()
        .ok()
    }
    pub fn wait(self, now: f64) -> Option<f64> {
        match self {
            Self::Pending(j) => rules::pending_wait(j, now),
            Self::Running(_) => None,
        }
    }
    pub fn left(self) -> Option<u64> {
        match self {
            Self::Running(j) => rules::duration(&j.time_limit)
                .zip(rules::duration(&j.elapsed))
                .map(|(limit, elapsed)| limit.saturating_sub(elapsed)),
            Self::Pending(_) => None,
        }
    }
    pub fn fields(self) -> [&'a str; 7] {
        match self {
            Self::Running(j) => [
                &j.jobid,
                &j.user,
                &j.jobname,
                &j.node,
                &j.partition,
                &j.qos,
                "RUNNING",
            ],
            Self::Pending(j) => [
                &j.jobid,
                &j.user,
                &j.jobname,
                &j.reason,
                &j.partition,
                &j.qos,
                "PENDING",
            ],
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    #[default]
    All,
    Running,
    Pending,
}
impl Filter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Running,
            Self::Running => Self::Pending,
            Self::Pending => Self::All,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Running => "running",
            Self::Pending => "pending",
        }
    }
    pub fn includes(self, entry: Entry<'_>) -> bool {
        match self {
            Self::All => true,
            Self::Running => matches!(entry, Entry::Running(_)),
            Self::Pending => matches!(entry, Entry::Pending(_)),
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Priority,
    User,
    Gpu,
    Cpu,
    Wait,
    Left,
}
impl Sort {
    pub fn next(self) -> Self {
        match self {
            Self::Priority => Self::User,
            Self::User => Self::Gpu,
            Self::Gpu => Self::Cpu,
            Self::Cpu => Self::Wait,
            Self::Wait => Self::Left,
            Self::Left => Self::Priority,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::User => "user",
            Self::Gpu => "GPU",
            Self::Cpu => "CPU",
            Self::Wait => "wait",
            Self::Left => "left",
        }
    }
    pub fn descending(self) -> bool {
        !matches!(self, Self::User | Self::Left)
    }
    pub fn compare(self, a: Entry<'_>, b: Entry<'_>, now: f64, reverse: bool) -> Ordering {
        let descending = self.descending() ^ reverse;
        match self {
            Self::Priority => optional(a.priority(), b.priority(), descending),
            Self::User => optional(Some(a.user()), Some(b.user()), descending),
            Self::Gpu => optional(Some(a.gpus()), Some(b.gpus()), descending),
            Self::Cpu => optional(Some(a.cpus()), Some(b.cpus()), descending),
            Self::Wait => optional(a.wait(now), b.wait(now), descending),
            Self::Left => optional(a.left(), b.left(), descending),
        }
        .then_with(|| optional(a.priority(), b.priority(), true))
        .then_with(|| a.user().cmp(b.user()))
        .then_with(|| quickview::natural(a.id(), b.id()))
    }
}

// Missing observations stay last in both directions; zero is a measured value.
fn optional<T: PartialOrd>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            let order = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
            if descending { order.reverse() } else { order }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_priority_keeps_unknown_last_and_user_groups_keep_priority_order() {
        let jobs: Vec<Job> = [
            ("10", "bob", "9"),
            ("2", "alice", "100"),
            ("3", "alice", "10"),
            ("4", "alice", ""),
            ("5", "bob", "18446744073709551615"),
        ]
        .into_iter()
        .map(|(id, user, priority)| Job {
            jobid: id.into(),
            user: user.into(),
            priority: priority.into(),
            ..Default::default()
        })
        .collect();
        let mut entries: Vec<_> = jobs.iter().map(Entry::Running).collect();
        let ids = |rows: &[Entry<'_>]| rows.iter().map(|e| e.id().to_string()).collect::<Vec<_>>();
        entries.sort_by(|a, b| Sort::Priority.compare(*a, *b, 0.0, false));
        assert_eq!(ids(&entries), ["5", "2", "3", "10", "4"]);
        entries.sort_by(|a, b| Sort::Priority.compare(*a, *b, 0.0, true));
        assert_eq!(ids(&entries), ["10", "3", "2", "5", "4"]);
        entries.sort_by(|a, b| Sort::User.compare(*a, *b, 0.0, false));
        assert_eq!(ids(&entries), ["2", "3", "4", "5", "10"]);
        entries.sort_by(|a, b| Sort::User.compare(*a, *b, 0.0, true));
        assert_eq!(ids(&entries), ["5", "10", "2", "3", "4"]);
    }

    #[test]
    fn time_sorting_distinguishes_pending_wait_running_time_left_and_missing_values() {
        let start = "2026-11-16T00:00:00";
        let now = rules::parse_timestamp(start).unwrap() + 3600.0;
        let pending = PendingJob {
            jobid: "2".into(),
            submit_time: start.into(),
            ..Default::default()
        };
        let running = Job {
            jobid: "10".into(),
            time_limit: "01:00:00".into(),
            elapsed: "00:55:00".into(),
            ..Default::default()
        };
        let missing = Job {
            jobid: "20".into(),
            ..Default::default()
        };
        let later = Job {
            jobid: "30".into(),
            time_limit: "02:00:00".into(),
            elapsed: running.elapsed.clone(),
            ..Default::default()
        };
        let newer = PendingJob {
            jobid: "3".into(),
            submit_time: "2026-11-16T00:30:00".into(),
            ..Default::default()
        };
        let mut entries = [
            Entry::Running(&running),
            Entry::Pending(&pending),
            Entry::Running(&missing),
            Entry::Running(&later),
            Entry::Pending(&newer),
        ];
        assert_eq!(entries[0].left(), Some(300));
        assert_eq!(entries[1].wait(now), Some(3600.0));
        for reverse in [false, true] {
            entries.sort_by(|a, b| Sort::Wait.compare(*a, *b, now, reverse));
            assert_eq!(entries[0].id(), if reverse { "3" } else { "2" });
            assert_eq!(entries[1].id(), if reverse { "2" } else { "3" });
            entries.sort_by(|a, b| Sort::Left.compare(*a, *b, now, reverse));
            assert_eq!(entries[0].id(), if reverse { "30" } else { "10" });
            assert_eq!(entries[1].id(), if reverse { "10" } else { "30" });
        }
    }
}
