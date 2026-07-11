//! Pure data layer for mcp-loadtest: configuration, metric/report/outcome
//! types, and the trace on-disk format. No tokio, no I/O beyond config file
//! loading.

pub mod config;
pub mod coverage;
pub mod fuzz_report;
/// Metrics recording layer — [`metrics::Recorder`], outcome/latency/
/// throughput value types, and the sharded HDR histogram they're built on.
pub mod metrics;
pub mod outcome;
pub mod report;
pub mod trace;
pub mod version;

pub use config::{
    Config, ConfigError, OutputConfig, ScenarioConfig, ServerConfig, ThresholdsConfig,
    ValidationConfig, example_config, split_server_command,
};
pub use metrics::{
    CallOutcome, LatencyStats, OutcomeCounts, Recorder, ScenarioMetrics, ThroughputStats,
};
pub use outcome::ScenarioOutcome;
pub use report::{
    ProcessSample, ProcessStats, Report, ReportError, Reporter, ServerInfo, ThresholdKind,
    ThresholdViolation,
};
pub use version::ProtocolVersion;
