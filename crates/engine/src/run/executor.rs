//! [`Run::execute`] — the end-to-end orchestration body (spawn → drive →
//! sample → report → threshold-eval → shutdown).
//!
//! Split out of `run/mod.rs` to keep that file within the size convention.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use mcp_loadtest_core::config::{ServerConfig, sanitize_remote_endpoint};
use mcp_loadtest_core::coverage::CoverageReport;
use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_core::report::{ProcessStats, Report, ServerInfo, format_iso8601_utc};
use mcp_loadtest_core::trace::format::TraceHeader;
use mcp_loadtest_protocol::mcp::Tool;
use mcp_loadtest_protocol::session::{Session, SessionError};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use super::connect::{build_session, shutdown_after_session_error, trace_to_run_error};
use super::{DEFAULT_HANG_THRESHOLD, Run, RunError, StderrCapture, factory, thresholds};
use crate::process::{DEFAULT_SAMPLE_INTERVAL, ProcessSampler};
use crate::scenario::{RunContext, teardown};
use crate::trace::TraceWriter;

const REDACTED_SERVER_IDENTITY: &str = "[REDACTED]";

fn report_server_identity(
    server: &ServerConfig,
    redact_server_identity: bool,
) -> (String, Vec<String>) {
    if redact_server_identity {
        return (REDACTED_SERVER_IDENTITY.to_owned(), Vec::new());
    }
    match server.transport.as_str() {
        "stdio" => (
            server.command.clone().unwrap_or_default(),
            server.args.clone(),
        ),
        "http" | "sse" | "ws" => (
            sanitize_remote_endpoint(server.url.as_deref().unwrap_or_default()),
            Vec::new(),
        ),
        _ => (server.transport.clone(), Vec::new()),
    }
}

fn trace_server_identity(server: &ServerConfig, redact_server_identity: bool) -> String {
    let (command, args) = report_server_identity(server, redact_server_identity);
    if args.is_empty() {
        command
    } else {
        format!("{command} {}", args.join(" "))
    }
}

fn redacted_server_error() -> RunError {
    RunError::Config("server startup failed (identity redacted by Action)".into())
}

fn redacted_stderr_path() -> PathBuf {
    PathBuf::from(if cfg!(windows) { "NUL" } else { "/dev/null" })
}

fn finalize_requested_trace(writer: Option<&TraceWriter>) -> Result<(), RunError> {
    match writer {
        Some(writer) => writer.finish().map_err(trace_to_run_error),
        None => Ok(()),
    }
}

async fn list_tools_before_startup_deadline(
    session: &mut Session,
    deadline: TokioInstant,
    configured_budget: Duration,
) -> Result<Vec<Tool>, SessionError> {
    if TokioInstant::now() >= deadline {
        return Err(SessionError::StartupTimeout(configured_budget));
    }
    let result = tokio::time::timeout_at(deadline, session.list_tools()).await;
    if TokioInstant::now() >= deadline {
        return Err(SessionError::StartupTimeout(configured_budget));
    }
    match result {
        Ok(result) => result,
        Err(_) => Err(SessionError::StartupTimeout(configured_budget)),
    }
}

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
    /// 7. Snapshot metrics and capture the pre-shutdown server metadata.
    /// 8. Shutdown the session (bounded, report-gating on uncertainty).
    /// 9. Build [`Report`] and evaluate thresholds.
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
            redact_server_identity,
        } = self;

        // Builders intentionally remain infallible for API compatibility,
        // but public library callers can mutate their fields after
        // construction. Re-run the same semantic gate as TOML parsing at the
        // execution boundary so invalid thresholds, transport-only options,
        // and unsupported protocol combinations cannot bypass validation and
        // produce a false PASS.
        config
            .validate()
            .map_err(|error| RunError::Config(error.to_string()))?;

        // 1. Allocate run dir.
        let run_id = ulid::Ulid::new().to_string();
        let run_dir = output_dir.join(&run_id);
        tokio::fs::create_dir_all(&run_dir).await?;

        // 1b. Resolve where (if anywhere) the server's stderr should be
        // captured. The orchestrator's initial session keeps the historical
        // root file. Factory-spawned sessions get immutable unique files:
        // sharing one File::create target lets concurrent workers truncate
        // each other's evidence.
        let is_stdio = config.server.transport == "stdio";
        let captures_stdio = is_stdio
            && !redact_server_identity
            && matches!(stderr_capture, StderrCapture::Capture | StderrCapture::Tee);
        let stderr_log: Option<PathBuf> = if redact_server_identity && is_stdio {
            // A child can print its own argv. Action mode must therefore
            // discard stderr rather than inherit, tee, or retain it as an
            // artifact, regardless of user-supplied capture flags.
            Some(redacted_stderr_path())
        } else if captures_stdio {
            Some(run_dir.join("server-stderr.log"))
        } else {
            None
        };
        let factory_stderr_dir = captures_stdio.then(|| run_dir.join("server-stderr"));
        if let Some(directory) = &factory_stderr_dir {
            tokio::fs::create_dir_all(directory).await?;
        }
        // Count every factory construction attempt, not only captured
        // sessions. The sequence both names immutable log files and tells
        // process-threshold evaluation whether the initial PID covered all
        // stdio workload processes.
        let factory_session_sequence = Arc::new(AtomicU64::new(0));
        let tee_stderr = captures_stdio && matches!(stderr_capture, StderrCapture::Tee);

        // 2. Spawn / connect to the server. Capture the wall-clock start
        // *before* spawn so the reported `duration` covers the full lifecycle
        // (handshake + scenario + shutdown).
        let started_at = SystemTime::now();
        let run_start = Instant::now();

        // 2a. ADR 0021 — when `with_trace` was set, open the JSONL trace
        //     writer before the spawn so the `initialize` handshake is
        //     recorded too. Creation fails immediately; later per-frame
        //     failures are latched and checked after session teardown.
        let trace_writer: Option<Arc<TraceWriter>> = match &trace_path {
            Some(path) => {
                let server = trace_server_identity(&config.server, redact_server_identity);
                let header = TraceHeader::new(&run_id, &server, &format_iso8601_utc(started_at));
                let writer = TraceWriter::create(path, &header, run_start, true)
                    .map_err(trace_to_run_error)?;
                Some(Arc::new(writer))
            }
            None => None,
        };

        let startup_deadline = TokioInstant::now()
            .checked_add(config.server.startup_timeout)
            .ok_or_else(|| RunError::Config("server.startup_timeout is too large".into()))?;
        let mut session = match build_session(
            &config,
            stderr_log.as_deref(),
            tee_stderr,
            trace_writer.clone(),
            startup_deadline,
        )
        .await
        {
            Ok(session) => session,
            Err(_) if redact_server_identity => return Err(redacted_server_error()),
            Err(err) => return Err(err),
        };

        // 2b. Capture the `tools/list` registry once at the start of the run,
        //     before any scenario traffic. Discovery is a protocol
        //     precondition in every mode: accepting a failed list call and
        //     later driving a known tool would otherwise let a broken MCP
        //     discovery surface produce PASS. Strict mode additionally reuses
        //     the registry for input/output schema validation.
        let registered_tools: Vec<String> = match list_tools_before_startup_deadline(
            &mut session,
            startup_deadline,
            config.server.startup_timeout,
        )
        .await
        {
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
                let err =
                    shutdown_after_session_error(session, err, "tools/list startup cleanup").await;
                let run_error = if redact_server_identity {
                    redacted_server_error()
                } else {
                    RunError::Session(err)
                };
                return Err(run_error);
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
            let factory_stderr_dir = factory_stderr_dir.clone();
            let factory_session_sequence = Arc::clone(&factory_session_sequence);
            // Respawned sessions (pools, cold_start) record into the same
            // trace file, but never the same stderr artifact (ADR 0013/0021).
            let trace_writer = trace_writer.clone();
            // Version-aware recipe (ADR 0018): a `with_version` override —
            // e.g. from the `version_matrix` scenario — replaces the config's
            // advertised revision for that spawn only.
            factory::SessionFactory::new_versioned(move |version_override| {
                let mut config = config.clone();
                if let Some(v) = version_override {
                    config.server.protocol_version = Some(v.as_str().to_owned());
                }
                let sequence = factory_session_sequence.fetch_add(1, Ordering::Relaxed) + 1;
                let stderr_log = if let Some(directory) = &factory_stderr_dir {
                    Some(directory.join(format!("session-{sequence:06}.log")))
                } else {
                    // Action mode deliberately sends every child to the OS
                    // null device; Off/remote modes remain None.
                    stderr_log.clone()
                };
                let trace_writer = trace_writer.clone();
                async move {
                    let startup_deadline = TokioInstant::now()
                        .checked_add(config.server.startup_timeout)
                        .ok_or_else(|| {
                            factory::run_error_to_session_error(RunError::Config(
                                "server.startup_timeout is too large".into(),
                            ))
                        })?;
                    let mut session = match build_session(
                        &config,
                        stderr_log.as_deref(),
                        tee_stderr,
                        trace_writer,
                        startup_deadline,
                    )
                    .await
                    {
                        Ok(session) => session,
                        Err(_) if redact_server_identity => {
                            return Err(factory::run_error_to_session_error(
                                redacted_server_error(),
                            ));
                        }
                        Err(err) => {
                            return Err(factory::run_error_to_session_error(err));
                        }
                    };

                    // Pooled scenarios drive these fresh sessions instead of
                    // the orchestrator's initial session. Strict validation
                    // therefore needs the worker's own tools/list registry;
                    // otherwise concurrent > 1 silently bypasses the exact
                    // schema checks that concurrent = 1 enforces.
                    if config.validation.strict {
                        let tools = match list_tools_before_startup_deadline(
                            &mut session,
                            startup_deadline,
                            config.server.startup_timeout,
                        )
                        .await
                        {
                            Ok(tools) => tools,
                            Err(error) => {
                                return Err(shutdown_after_session_error(
                                    session,
                                    error,
                                    "strict worker tools/list cleanup",
                                )
                                .await);
                            }
                        };
                        session.set_strict_tool_schemas(
                            tools
                                .iter()
                                .map(|tool| (tool.name.clone(), tool.input_schema.clone()))
                                .collect(),
                        );
                        session.set_strict_tool_output_schemas(
                            tools
                                .into_iter()
                                .filter_map(|tool| {
                                    tool.output_schema.map(|schema| (tool.name, schema))
                                })
                                .collect(),
                        );
                    }

                    Ok(session)
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
        let mut scenario_outcome = scenario.drive(&mut session, &ctx).await;
        let factory_session_count = factory_session_sequence.load(Ordering::Relaxed);

        // 6. Cancel sampler, collect ProcessStats.
        cancel_token.cancel();
        let mut process: ProcessStats = match sampler {
            Some(s) => s.finish().await,
            None => ProcessStats::default(),
        };
        if is_stdio && factory_session_count > 0 {
            // The sampler follows one PID. Factory-backed stdio scenarios
            // put the real workload in separate child processes, so keeping
            // the idle initial child's values would be actively misleading.
            // Clear them and disclose the scope gap; configured process
            // thresholds then use their existing missing-evidence
            // fail-closed path.
            process = ProcessStats::default();
            scenario_outcome.notes.push(format!(
                "process metrics unavailable: workload used {factory_session_count} \
                 factory-spawned stdio session(s) outside the single-PID sampler"
            ));
        }

        // 7. Snapshot metrics and capture metadata needed after shutdown.
        let metrics_snapshot = metrics.snapshot();

        // For non-stdio transports the "command" is the URL — surface that in
        // the Report so reports stay self-describing when read in isolation.
        let (info_command, info_args) =
            report_server_identity(&config.server, redact_server_identity);
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

        // 8. A successful report requires a known session lifecycle. The
        // shared helper leaves enough margin above stdio's composed internal
        // kill/reap/pump budgets and records any uncertainty as a typed gate.
        teardown::shutdown_session(session, &mut scenario_outcome, "run session").await;

        // 8b. An explicitly requested trace is part of the run contract.
        // Per-frame serialization/I/O failures are latched because the
        // Transport methods have no separate artifact-error channel. Observe
        // that latch only after all sessions have shut down, and never return
        // a successful report/path for an incomplete trace.
        finalize_requested_trace(trace_writer.as_deref())?;
        let duration = run_start.elapsed();

        // 9. Build the final report only after shutdown and trace
        // finalization so duration covers the full lifecycle, teardown
        // failures participate in passed(), and trace_path is truthful.
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

        // Evaluate thresholds — global + per-tool SLO entries.
        report.threshold_violations = thresholds::evaluate_thresholds(&config, &report);
        report
            .threshold_violations
            .extend(thresholds::evaluate_tool_slos(&config, &per_tool));

        // 10. Return Report.
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use async_trait::async_trait;
    use mcp_loadtest_core::config::{Config, ScenarioConfig, ThresholdsConfig};
    use mcp_loadtest_core::trace::format::{Direction, TraceHeader};
    use serde_json::{Value, json};

    use super::*;
    use crate::scenario::{RunContext, Scenario, ScenarioOutcome};

    struct NeverDriven;

    struct FailAfterHeader {
        header_flushed: bool,
    }

    impl Write for FailAfterHeader {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.header_flushed {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected finalization failure",
                ))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.header_flushed {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected finalization failure",
                ))
            } else {
                self.header_flushed = true;
                Ok(())
            }
        }
    }

    #[async_trait]
    impl Scenario for NeverDriven {
        async fn drive(
            &self,
            _session: &mut mcp_loadtest_protocol::Session,
            _ctx: &RunContext,
        ) -> ScenarioOutcome {
            panic!("invalid config must gate before server construction or scenario traffic")
        }

        fn config_schema(&self) -> Value {
            json!({})
        }

        fn name(&self) -> &'static str {
            "sustained"
        }
    }

    #[tokio::test]
    async fn execute_revalidates_programmatic_config_before_creating_artifacts() {
        let server = ServerConfig::stdio("must-not-be-spawned".into(), Vec::new());
        let mut thresholds = ThresholdsConfig::default();
        thresholds.memory_growth_mb = Some(f64::NAN);
        let config = Config::new(
            server,
            ScenarioConfig::new("sustained", json!({ "tool": "echo" })),
        )
        .with_thresholds(thresholds);
        let artifact_root =
            std::env::temp_dir().join(format!("mcp-loadtest-invalid-{}", ulid::Ulid::new()));

        let error = Run::new(config, Box::new(NeverDriven), artifact_root.clone())
            .execute()
            .await
            .expect_err("non-finite programmatic threshold must fail closed");
        assert!(
            matches!(error, RunError::Config(ref message) if message.contains("memory_growth_mb") && message.contains("finite")),
            "unexpected error: {error:?}"
        );
        assert!(
            !artifact_root.exists(),
            "validation must run before allocating the run-artifact root"
        );
    }

    #[test]
    fn requested_trace_failure_maps_to_run_error_before_report_construction() {
        let writer = TraceWriter::create_with_test_sink(
            PathBuf::from("injected-trace.jsonl").as_path(),
            &TraceHeader::new("01TEST", "server", "2026-07-29T00:00:00Z"),
            Instant::now(),
            true,
            FailAfterHeader {
                header_flushed: false,
            },
        )
        .unwrap();
        writer.record(
            Direction::ClientToServer,
            Some("tools/list"),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        );

        let error = finalize_requested_trace(Some(&writer)).unwrap_err();
        assert!(
            matches!(error, RunError::Io(ref source) if source.to_string().contains("injected finalization failure")),
            "trace finalization must be a typed run error, got: {error:?}"
        );
    }

    #[test]
    fn remote_report_and_trace_identity_sanitize_every_transport() {
        for (transport, url, expected) in [
            (
                "http",
                "https://operator:password@mcp.example.test/rpc?token=secret&tenant=private#fragment",
                "https://mcp.example.test/rpc?redacted",
            ),
            (
                "sse",
                "https://operator:password@mcp.example.test/events?api_key=secret#fragment",
                "https://mcp.example.test/events?redacted",
            ),
            (
                "ws",
                "wss://operator:password@mcp.example.test/socket?ticket=secret#fragment",
                "wss://mcp.example.test/socket?redacted",
            ),
        ] {
            let mut server = ServerConfig::stdio("unused".into(), Vec::new());
            server.transport = transport.into();
            server.url = Some(url.into());

            let (command, args) = report_server_identity(&server, false);
            assert_eq!(command, expected, "{transport}");
            assert!(args.is_empty(), "{transport}");
            assert_eq!(
                trace_server_identity(&server, false),
                expected,
                "{transport}"
            );
            for forbidden in [
                "operator", "password", "token", "secret", "tenant", "private", "api_key",
                "ticket", "fragment", "#",
            ] {
                assert!(
                    !command.contains(forbidden),
                    "{transport}: endpoint display leaked `{forbidden}`: {command}"
                );
            }
        }
    }

    #[test]
    fn stdio_identity_keeps_command_and_args() {
        let server = ServerConfig::stdio("python".into(), vec!["-m".into(), "demo".into()]);
        assert_eq!(
            report_server_identity(&server, false),
            (
                "python".to_string(),
                vec!["-m".to_string(), "demo".to_string()]
            )
        );
        assert_eq!(trace_server_identity(&server, false), "python -m demo");
    }

    #[test]
    fn action_mode_redacts_report_trace_and_stderr_identity() {
        let sentinel = "ACTION_SERVER_SECRET_7F3B";
        let server = ServerConfig::stdio(
            "python".into(),
            vec!["-m".into(), "demo".into(), sentinel.into()],
        );

        let (command, args) = report_server_identity(&server, true);
        assert_eq!(command, REDACTED_SERVER_IDENTITY);
        assert!(args.is_empty());
        let trace = trace_server_identity(&server, true);
        assert_eq!(trace, REDACTED_SERVER_IDENTITY);
        assert!(!trace.contains(sentinel));

        let null_path = redacted_stderr_path();
        if cfg!(windows) {
            assert_eq!(null_path, PathBuf::from("NUL"));
        } else {
            assert_eq!(null_path, PathBuf::from("/dev/null"));
        }
    }
}
