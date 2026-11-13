#![forbid(unsafe_op_in_unsafe_fn)]

pub mod agent;
pub mod batch;
pub mod benchmark;
pub mod cli;
pub mod collector;
pub mod history;
pub mod host;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod node_telemetry;
pub mod notify;
pub mod packaging;
pub mod process;
pub mod quickview;
pub mod rules;
pub mod runtime;
pub mod scheduler;
pub mod ui;
pub mod update;
pub mod usage;
pub mod users;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const AGENT_PAYLOAD_VERSION: u32 = 9;
