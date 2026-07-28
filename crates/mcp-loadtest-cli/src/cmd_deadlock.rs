//! `mcp-loadtest deadlock-probe` — convenience wrapper around
//! `scenario::DeadlockProbe` that doesn't require a TOML config.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use mcp_loadtest::Report;
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
    run_deadlock_probe_with_redaction(
        server,
        tool,
        concurrent,
        hang_threshold,
        grace_period,
        args,
        output_dir,
        false,
    )
    .await
}

/// Drive a deadlock probe with optional Action-only server identity redaction.
///
/// When `redact_server_identity` is true, reports, trace identity, child
/// stderr, and parse/spawn diagnostics omit the supplied server command and
/// argv. Ordinary callers should use [`run_deadlock_probe`].
#[allow(clippy::too_many_arguments)] // mirrors the CLI flags plus one private Action control
pub async fn run_deadlock_probe_with_redaction(
    server: &str,
    tool: &str,
    concurrent: u32,
    hang_threshold: Duration,
    grace_period: Duration,
    args: &str,
    output_dir: &Path,
    redact_server_identity: bool,
) -> Result<()> {
    let args_value: Value = if redact_server_identity {
        serde_json::from_str(args)
            .map_err(|_| anyhow::anyhow!("parsing --args JSON failed (value redacted by Action)"))?
    } else {
        serde_json::from_str(args).with_context(|| format!("parsing --args JSON: {args}"))?
    };

    let server_parts = split_server_command(server);
    let (command, cmd_args) = if redact_server_identity {
        server_parts
            .map_err(|_| anyhow::anyhow!("parsing --server failed (identity redacted by Action)"))?
    } else {
        server_parts.with_context(|| format!("parsing --server `{server}`"))?
    };
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

    let mut run = Run::new(config, Box::new(scenario), output_dir.to_path_buf());
    if redact_server_identity {
        run = run.with_redacted_server_identity();
    }
    let report = run.execute().await?;

    let formats = vec!["terminal".into(), "markdown".into(), "json".into()];
    emit_reports(&report, &formats, output_dir)?;

    // For `deadlock-probe` specifically, ANY signal of trouble = fail:
    //   - deadlock_count > 0 (the headline check)
    //   - hang_count > 0 (a completed call still breached the operator's
    //     diagnostic threshold)
    //   - threshold violations (rolled up by Report::passed())
    //   - error_count > 0 (transport/server errors are bad signals for a
    //     probe whose whole point is "is the server healthy under load?")
    ensure_deadlock_probe_passed(&report)
}

fn ensure_deadlock_probe_passed(report: &Report) -> Result<()> {
    let dc = report.scenario_outcome.deadlock_count;
    let hc = report.scenario_outcome.hang_count;
    let ec = report.scenario_outcome.error_count;
    let tv = report.threshold_violations.len();
    let tf = report.scenario_outcome.teardown_failure_count;
    if !report.passed() || ec > 0 {
        if dc > 0 {
            anyhow::bail!(
                "DEADLOCK DETECTED — {dc} deadlock(s), {hc} slow response(s), {ec} error(s), \
                 {tf} teardown failure(s), {tv} threshold violation(s)"
            );
        }
        anyhow::bail!(
            "deadlock-probe failed — {hc} slow response(s), {ec} error(s), \
             {tf} teardown failure(s), {tv} threshold violation(s)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest::{ProcessStats, ScenarioMetrics, ScenarioOutcome, ServerInfo};

    #[tokio::test]
    async fn action_mode_spawn_error_omits_server_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sentinel = "ACTION_SERVER_SECRET_7F3B";
        let server = format!("no-such-deadlock-binary --token {sentinel}");
        let error = run_deadlock_probe_with_redaction(
            &server,
            "echo",
            1,
            Duration::from_millis(10),
            Duration::from_millis(10),
            "{}",
            tmp.path(),
            true,
        )
        .await
        .expect_err("missing server must fail");
        let diagnostic = format!("{error:#}");
        assert!(
            !diagnostic.contains(&server) && !diagnostic.contains(sentinel),
            "redacted spawn error leaked server identity: {diagnostic}"
        );
        assert!(diagnostic.contains("identity redacted"), "{diagnostic}");
    }

    #[test]
    fn mixed_success_and_slow_probe_is_a_cli_failure() {
        let report = Report {
            run_id: "01TEST".into(),
            started_at: std::time::SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(25),
            scenario_name: "deadlock_probe".into(),
            server_info: ServerInfo {
                command: "fixture".into(),
                args: Vec::new(),
                pid: None,
                protocol_version: None,
            },
            metrics: ScenarioMetrics {
                outcomes: mcp_loadtest::OutcomeCounts {
                    success: 1,
                    hang: 1,
                    ..mcp_loadtest::OutcomeCounts::default()
                },
                ..ScenarioMetrics::default()
            },
            process: ProcessStats::default(),
            scenario_outcome: ScenarioOutcome {
                total_calls: 2,
                successful_calls: 1,
                hang_count: 1,
                ..ScenarioOutcome::default()
            },
            trace_path: None,
            threshold_violations: Vec::new(),
            coverage: None,
        };

        let error = ensure_deadlock_probe_passed(&report)
            .expect_err("one slow cohort member must make the CLI fail");
        assert!(
            error.to_string().contains("1 slow response(s)"),
            "CLI diagnostic must identify the breached hang threshold: {error}"
        );
    }
}
