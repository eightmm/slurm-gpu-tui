use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

macro_rules! model {
    ($name:ident { $($field:ident: $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug, Serialize, PartialEq)]
        pub struct $name {
            $(pub $field: $ty,)*
        }
        impl Default for $name {
            fn default() -> Self { Self { $($field: $default,)* } }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = Value::deserialize(deserializer)?;
                let mut model = Self::default();
                if let Some(fields) = raw.as_object() {
                    $(if let Some(value) = fields.get(stringify!($field)) {
                        if let Ok(value) = serde_json::from_value::<$ty>(value.clone()) {
                            model.$field = value;
                        }
                    })*
                }
                Ok(model)
            }
        }
    }
}

model!(Gpu {
    index: String = String::new(), minor: String = String::new(), uuid: String = String::new(),
    pci_bus: String = String::new(), slot: String = String::new(), serial: String = String::new(),
    name: String = String::new(), util: String = String::new(), mem_used: String = String::new(),
    mem_total: String = String::new(), temp: String = String::new(), power: String = String::new(),
    power_cap: String = String::new(), ecc: String = String::new(), sm_clock: String = String::new(),
    mem_clock: String = String::new(), clock_reasons: String = String::new(),
    recovery_action: String = String::new(), health_observed_at: f64 = 0.0,
    pids: Vec<String> = Vec::new(), users: Vec<String> = Vec::new(),
    pid_mem: BTreeMap<String, String> = BTreeMap::new(),
    pid_jobid: BTreeMap<String, String> = BTreeMap::new(),
    alloc_jobid: String = String::new(), alloc_user: String = String::new(),
    idle_sec: u64 = 0, parked_sec: u64 = 0
});

model!(Job {
    jobid: String = String::new(), user: String = String::new(), partition: String = String::new(),
    jobname: String = String::new(), elapsed: String = String::new(), node: String = String::new(),
    gpu_count: u32 = 0, cpu_count: u32 = 0, gres_raw: String = String::new(),
    time_limit: String = String::new(), mem: String = String::new(), script: String = String::new(),
    log_out: String = String::new(), log_err: String = String::new(), detail: String = String::new(),
    log_status: BTreeMap<String, String> = BTreeMap::new(), uid: i64 = -1,
    qos: String = String::new(), telemetry: BTreeMap<String, Value> = BTreeMap::new()
});

model!(PendingJob {
    jobid: String = String::new(),
    user: String = String::new(),
    partition: String = String::new(),
    jobname: String = String::new(),
    time_limit: String = String::new(),
    gpu_count: u32 = 0,
    reason: String = String::new(),
    priority: String = String::new(),
    start_time: String = String::new(),
    detail: String = String::new(),
    submit_time: String = String::new(),
    eligible_time: String = String::new(),
    dependency: String = String::new(),
    qos: String = String::new(),
    cpu_count: u32 = 0,
    mem: String = String::new()
});

model!(Node {
    name: String = String::new(), state: String = String::new(), partition: String = String::new(),
    source: String = String::new(), has_gpu: bool = true, cpus: String = String::new(),
    cpu_alloc: String = String::new(), cpu_load: String = String::new(),
    mem_total: String = String::new(), mem_free: String = String::new(), mem_alloc: String = String::new(),
    cpu_power: String = String::new(), ram_power: String = String::new(), sys_power: String = String::new(),
    gres: String = String::new(), gpus: Vec<Gpu> = Vec::new(), jobs: Vec<Job> = Vec::new(),
    error: String = String::new(), mem_used: String = String::new(), mem_avail: String = String::new(),
    stale: bool = false, error_kind: String = String::new(), observed_at: f64 = 0.0,
    data_age_sec: f64 = -1.0, scheduler_age_sec: f64 = -1.0,
    scheduler_mem_total: String = String::new(), scheduler_available: bool = true,
    cpu_util: String = String::new(), pressure: BTreeMap<String, String> = BTreeMap::new(),
    telemetry_observed_at: f64 = 0.0, jobs_truncated: bool = false,
    host_io: BTreeMap<String, String> = BTreeMap::new()
});

model!(SchedulerStatus {
    job_backend: String = String::new(),
    fallback_reason: String = String::new(),
    error: String = String::new(),
    rejected_records: usize = 0,
    observed_at: f64 = 0.0,
    age_sec: f64 = -1.0
});

model!(Snapshot {
    version: u32 = 1, release: String = crate::VERSION.into(), build: String = crate::VERSION.into(),
    ts: String = String::new(), nodes: Vec<Node> = Vec::new(), jobs: Vec<Job> = Vec::new(),
    pending: Vec<PendingJob> = Vec::new(), stale_nodes: Vec<String> = Vec::new(),
    untrusted_payloads: BTreeMap<String, u32> = BTreeMap::new(),
    scheduler: SchedulerStatus = SchedulerStatus::default(), errors: String = String::new(),
    collector: Value = Value::Null, controller: Value = Value::Null
});

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub total: String,
    pub used: String,
    pub avail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentPayload {
    pub agent_version: u32,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub package_build: String,
    pub agent_build: String,
    pub ts: f64,
    pub hostname: String,
    pub node_kind: String,
    pub gpus: Vec<Gpu>,
    pub mem: Memory,
    #[serde(default)]
    pub telemetry: Value,
    #[serde(default)]
    pub power: BTreeMap<String, String>,
}

pub fn number(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().filter(|n| n.is_finite())
}

pub fn json_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(number))
        .filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_degrade_independently_and_unknown_fields_are_ignored() {
        let gpu: Gpu = serde_json::from_value(
            json!({"index":"0", "pids":"wrong", "idle_sec":"wrong", "new_field":1}),
        )
        .unwrap();
        assert_eq!(gpu.index, "0");
        assert!(gpu.pids.is_empty());
        assert_eq!(gpu.idle_sec, 0);
        assert!(serde_json::from_value::<Node>(json!({})).unwrap().has_gpu);
        let job: Job = serde_json::from_value(json!({"uid": "malformed"})).unwrap();
        assert_eq!(job.uid, -1);
        let node: Node = serde_json::from_value(json!({"has_gpu": "malformed"})).unwrap();
        assert!(node.has_gpu);
    }

    #[test]
    fn codec_preserves_nested_attribution_and_power() {
        let raw = json!({"name":"gpu1", "sys_power":"900", "gpus":[{"index":"0", "sm_clock":"2100", "pid_jobid":{"111":"42"}}], "jobs":[{"jobid":"42", "uid":1001}]});
        let node: Node = serde_json::from_value(raw).unwrap();
        assert_eq!(node.gpus[0].pid_jobid["111"], "42");
        assert_eq!(node.sys_power, "900");
        let roundtrip: Node = serde_json::from_value(serde_json::to_value(&node).unwrap()).unwrap();
        assert_eq!(node, roundtrip);
    }
}
