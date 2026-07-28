//! `deadlock_probe` tool — handler + JSON schema definition.
//!
//! See [`crate::scenario::deadlock_probe::DeadlockProbe`] for the underlying
//! scenario. Split out of `tools.rs` in M8 to keep per-tool files under the
//! 300-LoC convention.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Config, OutputConfig, ScenarioConfig, ServerConfig};
use crate::run::Run;
use crate::scenario::deadlock_probe::DeadlockProbe;

use super::{ToolError, required_str, split_server_command};

pub(super) fn deadlock_probe_def() -> Value {
    json!({
        "name": "deadlock_probe",
        "description":
            "Probe an MCP server for the Vibe-Trading-style deadlock bug class. \
             Spawns the target, fires N tool calls, and classifies each as \
             success / hang / deadlock / error. Returns a fail-closed `passed` \
             signal including worker and teardown completeness.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Shell-split server command (e.g. \"python -m my_mcp\")."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name to invoke on the target."
                },
                "concurrent": {
                    "type": "integer",
                    "default": 5,
                    "description": "Number of probe iterations."
                },
                "hang_threshold_ms": {
                    "type": "integer",
                    "default": 2000,
                    "description": "Per-call ms after which a call counts as hung."
                },
                "grace_period_ms": {
                    "type": "integer",
                    "default": 5000,
                    "description": "Extra ms before classifying a hang as a deadlock."
                },
                "args": {
                    "type": "object",
                    "description": "Optional JSON args object passed to every tool call."
                }
            },
            "required": ["server_command", "tool"]
        }
    })
}

pub(super) async fn deadlock_probe(args: &Value) -> Result<Value, ToolError> {
    let server_command = required_str(args, "server_command")?;
    let tool = required_str(args, "tool")?;
    let concurrent = args.get("concurrent").and_then(Value::as_u64).unwrap_or(5) as u32;
    let hang_threshold_ms = args
        .get("hang_threshold_ms")
        .and_then(Value::as_u64)
        .unwrap_or(2_000);
    let grace_period_ms = args
        .get("grace_period_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5_000);
    let call_args = args.get("args").cloned().unwrap_or(json!({}));

    let (command, cmd_args) = split_server_command(&server_command)?;

    let server_cfg = ServerConfig::stdio(command, cmd_args);
    let scenario = DeadlockProbe {
        concurrent,
        hang_threshold: Duration::from_millis(hang_threshold_ms),
        grace_period: Duration::from_millis(grace_period_ms),
        tool,
        args: call_args,
    };
    // `Config`/`ScenarioConfig`/`OutputConfig` are `#[non_exhaustive]` and
    // now live in `mcp-loadtest-core` — build via the constructors + builders
    // rather than an exhaustive struct literal (rejected across the crate
    // boundary).
    let config = Config::new(server_cfg, ScenarioConfig::new("deadlock_probe", json!({})))
        .with_output(OutputConfig::new(PathBuf::from("./runs"), vec![]));

    let run = Run::new(config, Box::new(scenario), PathBuf::from("./runs"));
    let report = run
        .execute()
        .await
        .map_err(|e| ToolError::Run(e.to_string()))?;

    // Read the deadlock durations from the typed `ScenarioOutcome` field
    // rather than re-parsing the human-readable notes. The note format is for
    // humans and can change freely without silently emptying this output.
    let hung_for_ms = report.scenario_outcome.hung_for_ms.clone();

    Ok(json!({
        "passed": report.passed(),
        "deadlock_count": report.scenario_outcome.deadlock_count,
        "hang_count": report.scenario_outcome.hang_count,
        "successful_calls": report.scenario_outcome.successful_calls,
        "total_calls": report.scenario_outcome.total_calls,
        "error_count": report.scenario_outcome.error_count,
        "incomplete_worker_count": report.scenario_outcome.incomplete_worker_count,
        "teardown_failure_count": report.scenario_outcome.teardown_failure_count,
        "threshold_violation_count": report.threshold_violations.len(),
        "hung_for_ms": hung_for_ms,
        "scenario_outcome": {
            "notes": report.scenario_outcome.notes,
        },
        "run_id": report.run_id,
    }))
}
