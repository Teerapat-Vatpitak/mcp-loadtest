//! [`Run::execute`] — the end-to-end orchestration body (spawn → drive →
//! sample → report → threshold-eval → shutdown).
//!
//! Split out of `run/mod.rs` to keep that file within the size convention.

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use mcp_loadtest_core::coverage::CoverageReport;
use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_core::report::{ProcessStats, Report, ServerInfo, format_iso8601_utc};
use mcp_loadtest_core::trace::format::TraceHeader;
use tokio_util::sync::CancellationToken;

use super::connect::{build_session, trace_to_run_error};
use super::{
    DEFAULT_HANG_THRESHOLD, Run, RunError, SHUTDOWN_TIMEOUT, StderrCapture, factory, thresholds,
};
use crate::process::{DEFAULT_SAMPLE_INTERVAL, ProcessSampler};
use crate::scenario::RunContext;
use crate::trace::TraceWriter;

impl Run {
    /// Execute the run end-to-end.
    ///
    /// Steps (per DESIGN.md §4 architecture):
    /// 1. Allocate a `runs/<ulid>/` directory under `output_dir`.
    /// 2. Spawn the server (`Session::spawn`) with config.server params.
    /// 3. Construct [`RunContext`] (Recorder, cancel token, hang/grace from thresholds).
    /// 4. Spawn a process-sampler task that periodically reads sysinfo into a
    ///    [`ProcessStats`] accumulator. Cancellation-aware.
    /// 5. Call `scenario.drive(&mut session, &ctx).await`.
    /// 6. Cancel sampler, collect [`ProcessStats`].
    /// 7. Snapshot metrics. Build [`Report`].
    /// 8. Evaluate thresholds → populate `report.threshold_violations`.
    /// 9. Shutdown session (best-effort, bounded).
    /// 10. Return Report.
    ///
    /// Trace / report serialization to disk is the integration agent's job
    /// (CLI hooks the reporters); this function returns the in-memory
    /// [`Report`] so callers can either serialize it themselves or feed it to
    /// `Reporter::render`.
    pub async fn execute(self) -> Result<Report, RunError> {
        let Self {
            config,
            scenario,
            output_dir,
            stderr_capture,
            trace_path,
        } = self;

        // 1. Allocate run dir.
        let run_id = ulid::Ulid::new().to_string();
        let run_dir = output_dir.join(&run_id);
        tokio::fs::create_dir_all(&run_dir).await?;

        // 1b. Resolve where (if anywhere) the server's stderr should be
        // captured. The run dir exists by now, so the pump's
        // `File::create(server-stderr.log)` has a valid parent. Only relevant
        // for the stdio transport (`build_session` ignores it otherwise).
        let stderr_log: Option<std::path::PathBuf> = match stderr_capture {
            StderrCapture::Off => None,
            StderrCapture::Capture | StderrCapture::Tee => Some(run_dir.join("server-stderr.log")),
        };
        let tee_stderr = matches!(stderr_capture, StderrCapture::Tee);

        // 2. Spawn / connect to the server. Capture the wall-clock start
        // *before* spawn so the reported `duration` covers the full lifecycle
        // (handshake + scenario + shutdown).
        let started_at = SystemTime::now();
        let run_start = Instant::now();

        // 2a. ADR 0021 — when `with_trace` was set, open the JSONL trace
        //     writer before the spawn so the `initialize` handshake is
        //     recorded too. Unlike per-frame writes (best-effort), failing to
        //     *create* an explicitly requested artifact fails the run.
        let trace_writer: Option<Arc<TraceWriter>> = match &trace_path {
            Some(path) => {
                let server = match config.server.transport.as_str() {
                    "stdio" => {
                        let cmd = config.server.command.clone().unwrap_or_default();
                        if config.server.args.is_empty() {
                            cmd
                        } else {
                            format!("{cmd} {}", config.server.args.join(" "))
                        }
                    }
                    _ => config
                        .server
                        .url
                        .clone()
                        .unwrap_or_else(|| config.server.transport.clone()),
                };
                let header = TraceHeader::new(&run_id, &server, &format_iso8601_utc(started_at));
                let writer = TraceWriter::create(path, &header, run_start, true)
                    .map_err(trace_to_run_error)?;
                Some(Arc::new(writer))
            }
            None => None,
        };

        let mut session = build_session(
            &config,
            stderr_log.as_deref(),
            tee_stderr,
            trace_writer.clone(),
        )
        .await?;

        // 2b. M7 — capture the `tools/list` registry once at the start of the
        //     run, before any scenario traffic. Used at end-of-run to compute
        //     coverage. Failures degrade gracefully: an unreachable
        //     `tools/list` (server doesn't advertise any, or transport hiccup)
        //     just leaves the registered set empty, so coverage is reported
        //     vacuously rather than failing the whole run.
        let registered_tools: Vec<String> = match session.list_tools().await {
            Ok(tools) => {
                // Opt-in strict args validation reuses this same registry —
                // no extra `tools/list` round-trip (ADR 0010 / 0006).
                if config.validation.strict {
                    session.set_strict_tool_schemas(
                        tools
                            .iter()
                            .map(|t| (t.name.clone(), t.input_schema.clone()))
                            .collect(),
                    );
                    // Result-side registry from the same `tools/list` — only
                    // tools that advertise an `outputSchema` are validated
                    // (non-gating Warn policy; see `protocol::schema`).
                    session.set_strict_tool_output_schemas(
                        tools
                            .iter()
                            .filter_map(|t| t.output_schema.clone().map(|s| (t.name.clone(), s)))
                            .collect(),
                    );
                }
                tools.into_iter().map(|t| t.name).collect()
            }
            Err(err) => {
                tracing::warn!(error = %err, "tools/list failed; coverage will be empty");
                Vec::new()
            }
        };

        // 3. Build the RunContext.
        let hang_threshold = config
            .thresholds
            .hang_timeout
            .unwrap_or(DEFAULT_HANG_THRESHOLD);
        let grace_period = hang_threshold.saturating_mul(2);
        let cancel_token = CancellationToken::new();
        let metrics = Recorder::new();

        // 3b. Session factory — attached to every scenario's context (cheap:
        //     one Arc). `cold_start` respawns a fresh server per iteration
        //     through it; future session-pool work reuses the same handle.
        //     Captures owned copies of everything `build_session` needs so
        //     each invocation is independent and `'static`.
        let session_factory = {
            let config = config.clone();
            let stderr_log = stderr_log.clone();
            // Respawned sessions (pools, cold_start) record into the same
            // trace file, threaded like stderr_log (ADR 0021).
            let trace_writer = trace_writer.clone();
            // Version-aware recipe (ADR 0018): a `with_version` override —
            // e.g. from the `version_matrix` scenario — replaces the config's
            // advertised revision for that spawn only.
            factory::SessionFactory::new_versioned(move |version_override| {
                let mut config = config.clone();
                if let Some(v) = version_override {
                    config.server.protocol_version = Some(v.as_str().to_owned());
                }
                let stderr_log = stderr_log.clone();
                let trace_writer = trace_writer.clone();
                async move {
                    build_session(&config, stderr_log.as_deref(), tee_stderr, trace_writer)
                        .await
                        .map_err(factory::run_error_to_session_error)
                }
            })
        };

        let ctx = RunContext::new(
            run_start,
            cancel_token.clone(),
            metrics.clone(),
            hang_threshold,
            grace_period,
        )
        .with_session_factory(session_factory);

        // 4. Spawn the process sampler if we can resolve a PID. The sampler
        //    is best-effort: if it can't see the PID (already exited, sysinfo
        //    permission issue) it just returns empty samples — the run still
        //    completes cleanly with default `ProcessStats`.
        let server_pid = session.pid();
        let sampler = server_pid.map(|pid| {
            ProcessSampler::spawn(pid, DEFAULT_SAMPLE_INTERVAL, cancel_token.child_token())
        });
        if sampler.is_none() {
            tracing::warn!(
                "could not resolve server PID immediately after spawn; \
                 Report::process will be ProcessStats::default()"
            );
        }

        // 5. Drive the scenario.
        let scenario_outcome = scenario.drive(&mut session, &ctx).await;

        // 6. Cancel sampler, collect ProcessStats.
        cancel_token.cancel();
        let process: ProcessStats = match sampler {
            Some(s) => s.finish().await,
            None => ProcessStats::default(),
        };

        // 7. Snapshot metrics. Build Report.
        let metrics_snapshot = metrics.snapshot();
        let duration = run_start.elapsed();

        // For non-stdio transports the "command" is the URL — surface that in
        // the Report so reports stay self-describing when read in isolation.
        let (info_command, info_args) = match config.server.transport.as_str() {
            "stdio" => (
                config.server.command.clone().unwrap_or_default(),
                config.server.args.clone(),
            ),
            "http" | "sse" => (config.server.url.clone().unwrap_or_default(), Vec::new()),
            _ => (config.server.transport.clone(), Vec::new()),
        };
        let server_info = ServerInfo {
            command: info_command,
            args: info_args,
            // PID captured before scenario drive — child may have exited by now
            // (e.g. mock-broken hangs but mock-crash exits), but the historical
            // PID is what makes the report cross-referenceable with sampler logs.
            pid: server_pid,
            protocol_version: Some(session.server_protocol_version.clone()),
        };

        let scenario_name = scenario.name().to_string();

        // 7b. M7 — snapshot per-tool metrics for coverage + per-tool SLO eval.
        let per_tool = metrics.snapshot_per_tool();
        let exercised: std::collections::BTreeMap<String, u64> = per_tool
            .iter()
            .map(|(k, v)| (k.clone(), v.throughput.total_requests))
            .collect();
        let coverage = CoverageReport::build(registered_tools, exercised);

        let mut report = Report {
            run_id,
            started_at,
            duration,
            scenario_name,
            server_info,
            metrics: metrics_snapshot,
            process,
            scenario_outcome,
            // Set when `with_trace` recorded this run (ADR 0021); the file
            // itself was written incrementally by the TracingTransport.
            trace_path,
            threshold_violations: Vec::new(),
            coverage: Some(coverage),
        };

        // 8. Evaluate thresholds — global + per-tool SLO entries.
        report.threshold_violations = thresholds::evaluate_thresholds(&config, &report);
        report
            .threshold_violations
            .extend(thresholds::evaluate_tool_slos(&config, &per_tool));

        // 9. Shutdown session, best-effort and bounded. Both timeout and
        // shutdown errors are swallowed: the run already produced its
        // metrics; a wedged or already-dead server shouldn't fail the report.
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "session shutdown returned error");
            }
            Err(_) => {
                tracing::warn!("session shutdown exceeded {SHUTDOWN_TIMEOUT:?}");
            }
        }

        // 10. Return Report.
        Ok(report)
    }
}
