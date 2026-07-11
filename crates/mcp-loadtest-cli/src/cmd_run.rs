//! `mcp-loadtest run --config <path>` — load a TOML config, build the
//! requested scenario, drive a `Run`, and emit reports.
//!
//! Split into focused submodules (kept private; the public surface is the
//! [`run_from_config`] entry plus the re-exported [`parse_dur_str`] helper
//! `main.rs` shares with the `deadlock-probe` / `cross` subcommands):
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
    let config =
        Config::from_file(path).with_context(|| format!("loading config {}", path.display()))?;
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
    if let Some(trace_path) = trace {
        run = run.with_trace(trace_path);
    }
    let report = run.execute().await?;

    emit_reports(&report, &formats, &output_dir)?;
    if !report.passed() {
        anyhow::bail!(
            "{} threshold violation(s) — see report",
            report.threshold_violations.len()
        );
    }
    Ok(())
}
