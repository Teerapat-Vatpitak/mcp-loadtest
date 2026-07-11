//! `mcp-loadtest deadlock-probe` — convenience wrapper around
//! `scenario::DeadlockProbe` that doesn't require a TOML config.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use mcp_loadtest::config::{
    Config, OutputConfig, ScenarioConfig, ServerConfig, split_server_command,
};
use mcp_loadtest::run::Run;
use mcp_loadtest::scenario::deadlock_probe::DeadlockProbe;

use crate::emit::emit_reports;

/// Drive a `DeadlockProbe` scenario against `server` and fail loudly on any
/// signal of trouble (deadlocks, threshold violations, transport errors).
pub async fn run_deadlock_probe(
    server: &str,
    tool: &str,
    concurrent: u32,
    hang_threshold: Duration,
    grace_period: Duration,
    args: &str,
    output_dir: &Path,
) -> Result<()> {
    let args_value: Value =
        serde_json::from_str(args).with_context(|| format!("parsing --args JSON: {args}"))?;

    let (command, cmd_args) =
        split_server_command(server).with_context(|| format!("parsing --server `{server}`"))?;
    let server_cfg = ServerConfig::stdio(command, cmd_args);

    // Inline scenario; not driven via TOML.
    let scenario = DeadlockProbe {
        concurrent,
        hang_threshold,
        grace_period,
        tool: tool.to_string(),
        args: args_value,
    };

    let config = Config::new(server_cfg, ScenarioConfig::new("deadlock_probe", json!({})))
        .with_output(OutputConfig::new(
            output_dir.to_path_buf(),
            vec!["terminal".into(), "markdown".into(), "json".into()],
        ));

    let run = Run::new(config, Box::new(scenario), output_dir.to_path_buf());
    let report = run.execute().await?;

    let formats = vec!["terminal".into(), "markdown".into(), "json".into()];
    emit_reports(&report, &formats, output_dir)?;

    // For `deadlock-probe` specifically, ANY signal of trouble = fail:
    //   - deadlock_count > 0 (the headline check)
    //   - threshold violations (rolled up by Report::passed())
    //   - error_count > 0 (transport/server errors are bad signals for a
    //     probe whose whole point is "is the server healthy under load?")
    let dc = report.scenario_outcome.deadlock_count;
    let ec = report.scenario_outcome.error_count;
    let tv = report.threshold_violations.len();
    if !report.passed() || ec > 0 {
        if dc > 0 {
            anyhow::bail!(
                "DEADLOCK DETECTED — {dc} deadlock(s), {ec} error(s), {tv} threshold violation(s)"
            );
        }
        anyhow::bail!("deadlock-probe failed — {ec} error(s), {tv} threshold violation(s)");
    }
    Ok(())
}
