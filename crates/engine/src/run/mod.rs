//! [`Run`] — top-level orchestrator. Spawns server, drives scenario, samples
//! process metrics, builds [`Report`], evaluates thresholds.
//!
//! See DESIGN.md §4 (architecture) and §15.4 (threshold evaluator).
//!
//! [`Report`]: mcp_loadtest_core::report::Report

use std::path::PathBuf;
use std::time::Duration;

use mcp_loadtest_core::config::Config;
use mcp_loadtest_protocol::session::SessionError;
use thiserror::Error;

use crate::scenario::Scenario;

pub mod factory;

mod connect;
mod executor;
mod thresholds;

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
    /// Capture the server's stderr to `runs/<id>/server-stderr.log`.
    Capture,
    /// Capture to the file **and** mirror it live to the parent's stderr.
    Tee,
}

/// Default per-call hang threshold when neither `Thresholds` nor the scenario
/// specify one. Matches DESIGN.md §15.1's reference.
const DEFAULT_HANG_THRESHOLD: Duration = Duration::from_secs(5);

/// Default best-effort timeout for the post-run session shutdown. Bounded so a
/// wedged server can't hold the run open forever.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// redacted by default (ADR 0021). Sets `Report::trace_path` on the
    /// returned report.
    #[must_use]
    pub fn with_trace(mut self, path: PathBuf) -> Self {
        self.trace_path = Some(path);
        self
    }
}
