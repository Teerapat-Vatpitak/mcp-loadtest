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
use mcp_loadtest::config::Config;
use mcp_loadtest::run::Run;

use crate::emit::emit_reports;

mod builder;
mod params;
mod patterns;

use builder::build_scenario;
pub use params::parse_dur_str;

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

    let capture = if tee_stderr {
        StderrCapture::Tee
    } else if capture_stderr {
        StderrCapture::Capture
    } else {
        StderrCapture::Off
    };

    let mut run = Run::new(config, scenario, output_dir.clone()).with_stderr_capture(capture);
    if redact_server_identity {
        run = run.with_redacted_server_identity();
    }
    if let Some(trace_path) = trace {
        run = run.with_trace(trace_path);
    }
    let report = run.execute().await?;

    emit_reports(&report, &formats, &output_dir)?;
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
    Ok(())
}
