use crate::model::Snapshot;
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub enum Kind {
    User,
    Partition,
}
pub struct Picker {
    pub kind: Kind,
    pub query: String,
    values: Vec<String>,
}
impl Picker {
    pub fn new(kind: Kind, data: &Snapshot, current: &str, highlight: &str) -> Self {
        let mut values: BTreeSet<String> = match kind {
            Kind::User => data
                .jobs
                .iter()
                .map(|job| job.user.clone())
                .chain(data.pending.iter().map(|job| job.user.clone()))
                .chain(
                    data.nodes
                        .iter()
                        .flat_map(|node| node.jobs.iter())
                        .map(|job| job.user.clone()),
                )
                .chain(
                    data.nodes
                        .iter()
                        .flat_map(|node| node.gpus.iter())
                        .flat_map(|gpu| gpu.users.iter().chain(std::iter::once(&gpu.alloc_user)))
                        .cloned(),
                )
                .chain([highlight.to_string()])
                .collect(),
            Kind::Partition => data
                .nodes
                .iter()
                .map(|node| node.partition.as_str())
                .chain(data.jobs.iter().map(|job| job.partition.as_str()))
                .chain(data.pending.iter().map(|job| job.partition.as_str()))
                .flat_map(|value| value.split(','))
                .map(str::to_string)
                .collect(),
        };
        values.insert(current.into());
        values.remove("");
        Self {
            kind,
            query: String::new(),
            values: std::iter::once(String::new()).chain(values).collect(),
        }
    }
    pub fn choices(&self) -> Vec<&str> {
        let query = self.query.to_lowercase();
        self.values
            .iter()
            .filter(|value| {
                if value.is_empty() {
                    "(all)".contains(&query)
                } else {
                    value.to_lowercase().contains(&query)
                }
            })
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Gpu, Node, PendingJob};
    #[test]
    fn picker_keeps_exact_values_and_pending_only_partitions() {
        let data = Snapshot {
            nodes: vec![Node {
                gpus: vec![Gpu {
                    users: vec!["gpu-only-user".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            pending: vec![PendingJob {
                user: "Alice".into(),
                partition: "gpu,urgent".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut users = Picker::new(Kind::User, &data, "old-user", "me");
        users.query = "ali".into();
        assert_eq!(users.choices(), ["Alice"]);
        users.query = "gpu-only".into();
        assert_eq!(users.choices(), ["gpu-only-user"]);
        let mut partitions = Picker::new(Kind::Partition, &data, "", "");
        partitions.query = "urg".into();
        assert_eq!(partitions.choices(), ["urgent"]);
    }
}
