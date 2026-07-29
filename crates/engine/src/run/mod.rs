//! [`Run`] — top-level orchestrator. Spawns server, drives scenario, samples
//! process metrics, builds [`Report`], evaluates thresholds.
//!
//! See DESIGN.md §4 (architecture) and §15.4 (threshold evaluator).
//!
//! [`Report`]: mcp_loadtest_core::report::Report

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_core::config::Config;
use mcp_loadtest_core::metrics::{Recorder, ScenarioMetrics};
use mcp_loadtest_core::report::Report;
use mcp_loadtest_protocol::session::SessionError;
use thiserror::Error;

use crate::scenario::{Scenario, TrafficStartGate};

pub mod factory;

mod connect;
mod executor;
mod thresholds;

/// Apply the same global and per-tool pass/fail thresholds used by
/// [`Run::execute`] to a report assembled by a distributed controller.
///
/// Distributed execution merges exact recorder evidence outside the local
/// orchestrator, so it needs this narrow public adapter instead of
/// duplicating threshold semantics in the CLI.
pub fn evaluate_report_thresholds(
    config: &Config,
    report: &mut Report,
    per_tool: &std::collections::BTreeMap<String, ScenarioMetrics>,
) {
    report.threshold_violations = thresholds::evaluate_thresholds(config, report);
    report
        .threshold_violations
        .extend(thresholds::evaluate_tool_slos(config, per_tool));
}

/// How a spawned (stdio) server's stderr is handled for a run.
///
/// Maps the `run --capture-stderr` / `--tee-stderr` CLI flags onto
/// `SpawnOptions`. `Off` is the historical behaviour (inherit the parent's
/// stderr). For HTTP/SSE/WS transports there is no child process, so this is a
/// silent no-op (documented in ADR 0013).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StderrCapture {
    /// Inherit the parent's stderr (default).
    #[default]
    Off,
    /// Capture the initial session to `runs/<id>/server-stderr.log` and each
    /// factory session to a unique file under `runs/<id>/server-stderr/`.
    Capture,
    /// Capture to the per-session files **and** mirror them live to the
    /// parent's stderr.
    Tee,
}

/// Default per-call hang threshold when neither `Thresholds` nor the scenario
/// specify one. Matches DESIGN.md §15.1's reference.
const DEFAULT_HANG_THRESHOLD: Duration = Duration::from_secs(5);

/// Run-level configuration. Build via [`Run::new`].
///
/// **Locked for M3.**
pub struct Run {
    /// Underlying TOML-derived config.
    pub config: Config,
    /// The scenario to drive (constructed from `config.scenario` by the caller).
    pub scenario: Box<dyn Scenario>,
    /// Where to write `runs/<id>/` artifacts.
    pub output_dir: PathBuf,
    /// Disposition of the spawned server's stderr. Defaults to
    /// [`StderrCapture::Off`]; set via [`Run::with_stderr_capture`].
    pub stderr_capture: StderrCapture,
    /// Where to record the run's JSON-RPC frames as an `mcp-trace/1` JSONL
    /// file (ADR 0021). `None` (the default) records nothing; set via
    /// [`Run::with_trace`].
    pub trace_path: Option<PathBuf>,
    /// Suppress the configured server command/URL and parsed argv from
    /// reports, traces, spawn errors, and inherited/captured stderr. This is
    /// reserved for the composite Action; ordinary CLI runs remain
    /// self-describing. Set via [`Run::with_redacted_server_identity`].
    pub redact_server_identity: bool,
    /// Optional caller-owned recorder. Distributed workers retain a clone so
    /// they can serialize exact HDR evidence after `execute` returns.
    pub metrics_recorder: Option<Recorder>,
    /// Optional distributed traffic-start barrier.
    pub traffic_start_gate: Option<Arc<dyn TrafficStartGate>>,
    /// Optional deterministic weighted-pattern seed.
    pub rng_seed: Option<u64>,
}

/// Errors that can fail an entire run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    /// Server-side failure during spawn / handshake.
    #[error("session: {0}")]
    Session(#[from] SessionError),
    /// I/O failure writing artifacts.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Config validation failure.
    #[error("config: {0}")]
    Config(String),
}

impl Run {
    /// Build a Run from a parsed Config + a constructed Scenario.
    ///
    /// The caller passes scenario construction (since scenario types live in
    /// different modules); the constructor stays trivial — the real work is in
    /// [`Run::execute`].
    pub fn new(config: Config, scenario: Box<dyn Scenario>, output_dir: PathBuf) -> Self {
        Self {
            config,
            scenario,
            output_dir,
            stderr_capture: StderrCapture::Off,
            trace_path: None,
            redact_server_identity: false,
            metrics_recorder: None,
            traffic_start_gate: None,
            rng_seed: None,
        }
    }

    /// Set how the spawned server's stderr is handled (capture to a per-run
    /// file, or tee to that file plus the parent's stderr). Defaults to
    /// [`StderrCapture::Off`] (inherit). No-op for HTTP/SSE/WS transports
    /// (no child process) — see ADR 0013.
    #[must_use]
    pub fn with_stderr_capture(mut self, capture: StderrCapture) -> Self {
        self.stderr_capture = capture;
        self
    }

    /// Record every JSON-RPC frame of the run (handshake included) to `path`
    /// as an `mcp-trace/1` JSONL file, replayable via `trace::replay` or the
    /// CLI's `replay` subcommand. Secret-looking `tools/call` arguments are
    /// redacted by default (ADR 0021). The explicitly requested artifact is
    /// fail-closed: creation or finalization failure makes [`Run::execute`]
    /// return an error. `Report::trace_path` is set only after finalization.
    #[must_use]
    pub fn with_trace(mut self, path: PathBuf) -> Self {
        self.trace_path = Some(path);
        self
    }

    /// Redact the configured server identity from Action-generated output.
    ///
    /// Besides replacing report and trace identity fields, this makes server
    /// startup errors generic and discards child stderr so the command or its
    /// argv cannot be echoed back by the child. Ordinary callers should not
    /// enable this: the default keeps reports self-describing.
    #[must_use]
    pub fn with_redacted_server_identity(mut self) -> Self {
        self.redact_server_identity = true;
        self
    }

    /// Use a caller-owned metrics recorder.
    #[must_use]
    pub fn with_metrics_recorder(mut self, recorder: Recorder) -> Self {
        self.metrics_recorder = Some(recorder);
        self
    }

    /// Wait at a controller-managed barrier after local sessions are ready.
    #[must_use]
    pub fn with_traffic_start_gate(mut self, gate: Arc<dyn TrafficStartGate>) -> Self {
        self.traffic_start_gate = Some(gate);
        self
    }

    /// Use deterministic weighted-pattern selection.
    #[must_use]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = Some(seed);
        self
    }
}
