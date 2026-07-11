//! `sustained_load` tool — handler + JSON schema definition.
//!
//! See [`crate::scenario::sustained::Sustained`] for the underlying scenario.
//! Split out of `tools.rs` in M8 to keep per-tool files under the 300-LoC
//! convention.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Config, OutputConfig, ScenarioConfig, ServerConfig};
use crate::run::Run;
use crate::scenario::sustained::Sustained;

use super::{ToolError, required_str, split_server_command};

pub(super) fn sustained_load_def() -> Value {
    json!({
        "name": "sustained_load",
        "description":
            "Run a sustained constant-load workload against an MCP server for the \
             given duration. Returns latency percentiles + error rate + throughput.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Shell-split server command."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name to invoke on every iteration."
                },
                "concurrent": {
                    "type": "integer",
                    "default": 10,
                    "description": "Declared concurrency (M2: serialized on one session)."
                },
                "duration_ms": {
                    "type": "integer",
                    "description": "Total run duration, milliseconds."
                },
                "args": {
                    "type": "object",
                    "description": "Optional JSON args object passed to every tool call."
                }
            },
            "required": ["server_command", "tool", "duration_ms"]
        }
    })
}

pub(super) async fn sustained_load(args: &Value) -> Result<Value, ToolError> {
    let server_command = required_str(args, "server_command")?;
    let tool = required_str(args, "tool")?;
    let concurrent = args.get("concurrent").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = args
        .get("duration_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::InvalidArgs("duration_ms is required".into()))?;
    let call_args = args.get("args").cloned().unwrap_or(json!({}));

    let (command, cmd_args) = split_server_command(&server_command)?;
    let server_cfg = ServerConfig::stdio(command, cmd_args);
    let scenario = Sustained {
        concurrent,
        duration: Duration::from_millis(duration_ms),
        tool,
        args: call_args,
    };
    // `Config`/`ScenarioConfig`/`OutputConfig` are `#[non_exhaustive]` and
    // now live in `mcp-loadtest-core` — build via the constructors + builders
    // rather than an exhaustive struct literal (rejected across the crate
    // boundary).
    let config = Config::new(server_cfg, ScenarioConfig::new("sustained", json!({})))
        .with_output(OutputConfig::new(PathBuf::from("./runs"), vec![]));

    let run = Run::new(config, Box::new(scenario), PathBuf::from("./runs"));
    let report = run
        .execute()
        .await
        .map_err(|e| ToolError::Run(e.to_string()))?;

    let total = report.metrics.throughput.total_requests;
    let success = report.metrics.throughput.successful_requests;
    let errors = total.saturating_sub(success);
    let error_rate = if total == 0 {
        0.0
    } else {
        errors as f64 / total as f64
    };

    Ok(json!({
        "p50_ms": duration_to_ms(report.metrics.latency.p50),
        "p95_ms": duration_to_ms(report.metrics.latency.p95),
        "p99_ms": duration_to_ms(report.metrics.latency.p99),
        "p999_ms": duration_to_ms(report.metrics.latency.p999),
        "error_rate": error_rate,
        "requests_per_sec": report.metrics.throughput.requests_per_sec,
        "total_requests": total,
        "successful_requests": success,
        "run_id": report.run_id,
    }))
}

fn duration_to_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
