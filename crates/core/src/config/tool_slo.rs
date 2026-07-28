//! Per-tool latency SLO — a single entry in [`super::ThresholdsConfig::tool_slos`].

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-tool latency SLO. Used by [`super::ThresholdsConfig::tool_slos`].
///
/// A SLO ties a tool name to a p99 latency budget. The run orchestrator
/// (in the `mcp-loadtest` crate) evaluates each SLO against the per-tool
/// latency snapshot from `Recorder::snapshot_per_tool`; when actual p99
/// exceeds the budget a `ThresholdViolation` is appended to the report.
/// Missing metrics or zero latency samples also produce a fail-closed
/// violation; configuring an SLO requires exercising that tool.
///
/// TOML shape:
///
/// ```toml
/// [[thresholds.tool_slos]]
/// tool = "echo"
/// p99_latency = "50ms"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSlo {
    /// Tool name (must match the name advertised in `tools/list` and the
    /// name scenarios pass to `record_tool`).
    pub tool: String,
    /// p99 latency budget for this tool. Parsed via humantime
    /// (`"50ms"`, `"1s"`, ...).
    #[serde(with = "humantime_serde")]
    pub p99_latency: Duration,
}
