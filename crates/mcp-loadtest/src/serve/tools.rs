//! Tool registry + dispatch for the self-hosted MCP server.
//!
//! Each tool lives in its own submodule (`deadlock_probe`, `sustained_load`,
//! `compare_runs`) and exposes a `*_def()` constructor + a handler async fn
//! via `pub(super)`. This file keeps the small public surface — `ToolError`,
//! `ToolDef`, `tool_defs`, `dispatch` — plus the helpers shared across tools.
//!
//! See DESIGN.md §21.2 for the motivation. M7 ships three tools:
//!
//! 1. `deadlock_probe` — wraps [`crate::scenario::deadlock_probe::DeadlockProbe`].
//! 2. `sustained_load` — wraps [`crate::scenario::sustained::Sustained`].
//! 3. `compare_runs`   — reads two `metrics.json` files and diffs them.
//!
//! Handlers return `Result<Value, ToolError>`; the caller (in `mod.rs`) maps
//! errors to JSON-RPC error objects.
//!
//! M7 ownership: Agent W. M8 split: per-tool files.

use serde_json::Value;
use thiserror::Error;

use crate::config;

mod compare_runs;
mod deadlock_probe;
mod sustained_load;

/// Errors a tool handler can produce. The server transport translates these
/// into JSON-RPC error objects.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Caller passed bad arguments (missing field, wrong type, ...).
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// The underlying run failed.
    #[error("run failed: {0}")]
    Run(String),
    /// I/O error reading a metrics.json file in compare_runs.
    #[error("io: {0}")]
    Io(String),
}

/// Public description of a tool — what we surface in `tools/list`.
pub struct ToolDef {
    /// Tool name (called via `tools/call`).
    pub name: &'static str,
    /// One-line summary used as the MCP `description`.
    pub description: &'static str,
    /// JSON-schema-flavored input description for the tool's args.
    pub input_schema: Value,
}

/// Return the full registry as plain JSON values (the shape `tools/list`
/// expects under `result.tools`).
pub fn tool_defs() -> Vec<Value> {
    vec![
        deadlock_probe::deadlock_probe_def(),
        sustained_load::sustained_load_def(),
        compare_runs::compare_runs_def(),
    ]
}

/// Route a `tools/call` invocation to the right handler.
pub async fn dispatch(name: &str, arguments: &Value) -> Result<Value, ToolError> {
    match name {
        "deadlock_probe" => deadlock_probe::deadlock_probe(arguments).await,
        "sustained_load" => sustained_load::sustained_load(arguments).await,
        "compare_runs" => compare_runs::compare_runs(arguments).await,
        other => Err(ToolError::InvalidArgs(format!("unknown tool: {other}"))),
    }
}

// ---- shared helpers -----------------------------------------------------

pub(super) fn required_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidArgs(format!("{key} (string) is required")))
}

/// Thin adapter around [`config::split_server_command`] that translates the
/// `ConfigError` into a `ToolError::InvalidArgs` so the JSON-RPC error mapping
/// stays right.
pub(super) fn split_server_command(s: &str) -> Result<(String, Vec<String>), ToolError> {
    config::split_server_command(s).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_defs_contain_the_three_m7_tools() {
        let defs = tool_defs();
        let names: Vec<&str> = defs
            .iter()
            .filter_map(|d| d.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"deadlock_probe"));
        assert!(names.contains(&"sustained_load"));
        assert!(names.contains(&"compare_runs"));
    }

    #[test]
    fn split_server_command_parses_python_invocation() {
        let (cmd, args) = split_server_command("python -m my_mcp --foo bar").unwrap();
        assert_eq!(cmd, "python");
        assert_eq!(args, vec!["-m", "my_mcp", "--foo", "bar"]);
    }

    #[test]
    fn split_server_command_rejects_empty() {
        assert!(split_server_command("   ").is_err());
    }

    #[test]
    fn required_str_returns_invalid_args_when_missing() {
        let v = json!({});
        let err = required_str(&v, "tool").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_invalid_args() {
        let err = dispatch("not_a_tool", &json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }
}
