//! `mcp-loadtest run --config <path>` — load a TOML config, build the
//! requested scenario, drive a `Run`, and emit reports.
//!
//! Split into focused submodules (kept private; the public surface is the
//! [`run_from_config`] entry, its private-Action output override
//! [`run_from_config_with_output`], plus the re-exported [`parse_dur_str`]
//! helper `main.rs` shares with the `deadlock-probe` / `cross` subcommands):
//! - `builder` — scenario `kind` → `Box<dyn Scenario>` dispatch
//! - `params` — generic TOML param plucking + duration parsing
//! - `patterns` — weighted multi-step pattern-config parsing

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use mcp_loadtest::StderrCapture;
use mcp_loadtest::analysis::regression::RegressionThresholds;
use mcp_loadtest::config::{Config, HistoryOutputConfig, OtlpOutputConfig};
use mcp_loadtest::history::{
    HistorySampleV1, HistoryStore, TrendPolicy, TrendStatus, evaluate_and_record,
    render_trend_markdown,
};
use mcp_loadtest::report::Report;
use mcp_loadtest::report::otlp::{OtlpHttpConfig, OtlpHttpExporter};
use mcp_loadtest::report::wire::MetricsDocumentV1;
use mcp_loadtest::run::Run;

use crate::emit::emit_reports;

mod builder;
mod params;
mod patterns;

pub(crate) use builder::build_scenario;
pub use params::parse_dur_str;
pub(crate) use patterns::parse_patterns;

/// Top-level entry for `Cmd::Run`. Loads `path`, builds the scenario, executes
/// the run, emits reports, and surfaces any threshold violations as an error
/// (non-zero exit code — the CI gating contract; see DESIGN.md §15.4).
///
/// `capture_stderr` / `tee_stderr` map to [`StderrCapture`] (the CLI's
/// `--capture-stderr` / `--tee-stderr` flags, wired in `main.rs` by Agent C).
/// `tee_stderr` wins if both are set (tee is a strict superset of capture).
///
/// `trace` maps to [`Run::with_trace`] (the `--trace <file>` flag): record
/// every JSON-RPC frame of the run as `mcp-trace/1` JSONL (ADR 0021).
pub async fn run_from_config(
    path: &Path,
    capture_stderr: bool,
    tee_stderr: bool,
    trace: Option<PathBuf>,
) -> Result<()> {
    run_from_config_with_output(path, capture_stderr, tee_stderr, trace, None, false).await
}

/// Run a config while optionally overriding its report root and redacting its
/// server identity.
///
/// Both controls exist for the composite Action's private execution mode.
/// Ordinary CLI and library callers should use [`run_from_config`], which
/// preserves `output.report_dir` and keeps reports self-describing.
pub async fn run_from_config_with_output(
    path: &Path,
    capture_stderr: bool,
    tee_stderr: bool,
    trace: Option<PathBuf>,
    action_output_dir: Option<PathBuf>,
    redact_server_identity: bool,
) -> Result<()> {
    let config_result = Config::from_file(path);
    let mut config = if redact_server_identity {
        config_result.map_err(|_| {
            anyhow::anyhow!("loading Action config failed (server identity redacted)")
        })?
    } else {
        config_result.with_context(|| format!("loading config {}", path.display()))?
    };
    if let Some(output_dir) = action_output_dir {
        config.output.report_dir = output_dir;
    }
    let scenario = build_scenario(&config.scenario.kind, &config.scenario.params)?;
    let output_dir = config.output.report_dir.clone();
    let formats = config.output.formats.clone();
    let otlp = config.output.otlp.clone();
    let history = config.output.history.clone();
    let is_distributed = config.distributed.is_some();
    let execution_fingerprint = if is_distributed {
        distributed_execution_fingerprint(&config)?
    } else {
        local_execution_fingerprint(&config)
    };

    let capture = if tee_stderr {
        StderrCapture::Tee
    } else if capture_stderr {
        StderrCapture::Capture
    } else {
        StderrCapture::Off
    };

    let report = if is_distributed {
        if capture_stderr || tee_stderr || trace.is_some() {
            anyhow::bail!(
                "distributed runs do not support local stderr capture or JSON-RPC tracing"
            );
        }
        crate::distributed::run_controller(&config).await?
    } else {
        let mut run = Run::new(config, scenario, output_dir.clone()).with_stderr_capture(capture);
        if redact_server_identity {
            run = run.with_redacted_server_identity();
        }
        if let Some(trace_path) = trace {
            run = run.with_trace(trace_path);
        }
        run.execute().await?
    };

    emit_reports(&report, &formats, &output_dir)?;
    let history_gate = match history {
        Some(history) => update_history(&report, &output_dir, &history, execution_fingerprint)?,
        None => None,
    };
    if let Some(otlp) = otlp {
        export_otlp(&report, &otlp).await?;
    }
    if !report.passed() {
        anyhow::bail!(
            "run failed correctness gate — {} threshold violation(s), {} deadlock(s), \
             {} divergence(s), {} incomplete worker(s), {} teardown failure(s), \
             {}/{} successful calls; see report",
            report.threshold_violations.len(),
            report.scenario_outcome.deadlock_count,
            report.scenario_outcome.divergence_count,
            report.scenario_outcome.incomplete_worker_count,
            report.scenario_outcome.teardown_failure_count,
            report.scenario_outcome.successful_calls,
            report.scenario_outcome.total_calls,
        );
    }
    if let Some(message) = history_gate {
        anyhow::bail!("{message}");
    }
    Ok(())
}

fn local_execution_fingerprint(config: &Config) -> String {
    let concurrency = config
        .scenario
        .params
        .get("concurrent")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(10);
    format!(
        "local/transport={}/concurrency={concurrency}",
        config.server.transport
    )
}

fn distributed_execution_fingerprint(config: &Config) -> Result<String> {
    let distributed = config
        .distributed
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing distributed config"))?;
    let plan = crate::distributed::workload_plan(config)?;
    let names = distributed
        .agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "distributed/transport={}/concurrency={}/agents={names}",
        config.server.transport, plan.global_concurrency
    ))
}

fn update_history(
    report: &Report,
    output_dir: &Path,
    config: &HistoryOutputConfig,
    execution_fingerprint: String,
) -> Result<Option<String>> {
    let document = MetricsDocumentV1::from(report);
    let sample = HistorySampleV1::from_metrics(
        config.series.clone(),
        &document,
        Some(execution_fingerprint),
    )
    .context("building baseline history sample")?;
    let policy = TrendPolicy {
        window: config.window,
        min_samples: config.min_samples,
        regression: RegressionThresholds {
            p99_pct: config.max_p99_regression_pct,
            error_rate_pp: config.max_error_rate_regression_pp,
            deadlock_zero_tolerance: config.deadlock_zero_tolerance,
        },
        max_rps_drop_pct: config.max_rps_drop_pct,
    };
    let update = evaluate_and_record(&HistoryStore::new(&config.directory), &sample, &policy)
        .context("evaluating and recording baseline history")?;

    let trend_path = output_dir.join(&report.run_id).join("trend.md");
    std::fs::write(&trend_path, render_trend_markdown(&update.trend))
        .with_context(|| format!("writing {}", trend_path.display()))?;

    if update.trend.has_regression {
        let metrics = update
            .trend
            .regressions
            .iter()
            .map(|metric| metric.metric.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(Some(format!(
            "baseline history regression: {metrics}; see {}",
            trend_path.display()
        )));
    }
    if config.require_history && update.trend.status == TrendStatus::WarmingUp {
        return Ok(Some(format!(
            "baseline history requires {} comparable samples but found {}; current evidence was recorded",
            update.trend.required_sample_count, update.trend.baseline_sample_count
        )));
    }
    Ok(None)
}

async fn export_otlp(report: &Report, config: &OtlpOutputConfig) -> Result<()> {
    let exporter = OtlpHttpExporter::new(
        OtlpHttpConfig::new(config.endpoint.clone())
            .with_headers_from_env(config.headers_from_env.clone())
            .with_timeout(config.timeout)
            .with_fail_on_error(config.fail_on_error)
            .with_allowed_hosts(config.allowed_hosts.clone())
            .with_max_attempts(config.max_attempts),
    )
    .context("configuring OTLP export")?;
    let outcome = exporter
        .export(report)
        .await
        .context("exporting OTLP metrics")?;
    if !outcome.accepted {
        eprintln!(
            "warning: OTLP collector did not accept metrics after {} attempt(s) ({})",
            outcome.attempts,
            outcome.diagnostic.as_deref().unwrap_or("sanitized failure")
        );
    }
    Ok(())
}
