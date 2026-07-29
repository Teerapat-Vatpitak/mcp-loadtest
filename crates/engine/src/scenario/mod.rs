//! Scenario implementations — see DESIGN.md §8.
//!
//! Each scenario lives in its own file: one `impl Scenario` per module,
//! registered by its config `type` string in the CLI scenario builder.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use mcp_loadtest_core::metrics::{CallOutcome, Recorder};
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionFactory;
use mcp_loadtest_protocol::mcp::CallToolResult;
use mcp_loadtest_protocol::session::SessionError;
use mcp_loadtest_protocol::transport::TransportError;

pub mod cold_start;
pub mod deadlock_probe;
pub mod fuzzer;
pub mod pattern;
// Not a scenario: internal session-pool driver shared by pooled scenarios
// (`sustained` today; ramp/spike next). See ADR 0017.
pub(crate) mod pool;
pub mod race_check;
pub mod ramp;
pub mod soak;
pub mod spike;
pub mod sustained;
pub(crate) mod teardown;
pub mod version_matrix;

/// A workload scenario that drives an MCP `Session` and records metrics.
///
/// Implementations live in sibling modules — start from `deadlock_probe`
/// (simplest) or `sustained` when writing a new one.
///
/// Do not modify this trait signature without a CHANGELOG entry: every
/// scenario and external `impl Scenario` depends on it.
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Drive the scenario until completion or until `ctx.cancel_token` fires.
    ///
    /// Records per-call metrics via `ctx.metrics`. Implementations **must**
    /// observe `ctx.cancel_token` for graceful shutdown.
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome;

    /// JSON Schema fragment describing the TOML config block for this scenario.
    /// Used by `mcp-loadtest example-config`.
    fn config_schema(&self) -> Value;

    /// Short, human-readable identifier (used in logs, reports, CLI args).
    fn name(&self) -> &'static str;
}

/// Coordinator-controlled traffic start barrier.
///
/// Distributed workers call this only after every local MCP session has
/// completed startup. Implementations announce readiness, wait for the
/// controller's `Start` frame, and return the local monotonic instant at
/// which traffic should begin.
#[async_trait]
pub trait TrafficStartGate: Send + Sync {
    /// Announce local readiness and return the coordinated local start
    /// instant. Errors fail the run closed before any traffic is generated.
    async fn ready_and_start_at(
        &self,
        readiness: TrafficReadiness,
    ) -> Result<Instant, TrafficStartError>;
}

/// Evidence announced when a local worker reaches the traffic barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficReadiness {
    /// Local sessions that completed target startup.
    pub live_workers: u32,
    /// Local sessions assigned by the coordinator.
    pub requested_workers: u32,
    /// Target protocol revision observed during discovery.
    pub target_protocol_version: String,
    /// SHA-256 of the canonical target tool inventory.
    pub tool_inventory_hash: String,
}

/// Failure while waiting for a coordinated traffic start.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum TrafficStartError {
    /// The controller cancelled the job before traffic started.
    #[error("coordinator cancelled the run before traffic start")]
    Cancelled,
    /// The controller channel or state machine failed.
    #[error("coordinator start gate failed: {0}")]
    Coordinator(String),
}

/// Per-run context shared between scenarios and the orchestrator.
///
/// **Locked for M2.** Construct via [`RunContext::new`] — `#[non_exhaustive]`
/// so adding a field stays non-breaking for external scenario authors.
#[non_exhaustive]
pub struct RunContext {
    /// Time the run started; used to compute elapsed offsets in metrics.
    pub run_start: Instant,
    /// Cancellation token; scenarios must observe this for graceful shutdown.
    pub cancel_token: CancellationToken,
    /// Metrics recorder; scenarios feed per-call durations and outcomes here.
    pub metrics: Recorder,
    /// Per-call hang threshold; passed to `hang_detect`.
    pub hang_threshold: Duration,
    /// Additional time to wait after `hang_threshold` before classifying as deadlock.
    pub grace_period: Duration,
    /// Factory for spawning *fresh* sessions mid-run. `None` when the
    /// context is built directly via [`RunContext::new`] (e.g. library
    /// tests); `Run::execute` always attaches one via
    /// [`RunContext::with_session_factory`]. Scenarios that need a new
    /// server process per measurement (`cold_start`) check this and degrade
    /// with an explanatory note when absent.
    pub session_factory: Option<SessionFactory>,
    /// Optional external barrier used by distributed workers. Local runs
    /// leave this unset and start immediately after their session pool is
    /// ready.
    pub traffic_start_gate: Option<Arc<dyn TrafficStartGate>>,
    /// Optional deterministic base seed for weighted pattern selection.
    /// Distributed workers assign a shard-specific seed and derive one
    /// independent stream per local worker.
    pub rng_seed: Option<u64>,
    /// Protocol revision and canonical tool inventory established by the
    /// orchestrator's startup discovery.
    pub target_identity: Option<(String, String)>,
}

impl RunContext {
    /// Construct a run context. External scenario authors and tests use
    /// this instead of a struct literal (the struct is `#[non_exhaustive]`).
    pub fn new(
        run_start: Instant,
        cancel_token: CancellationToken,
        metrics: Recorder,
        hang_threshold: Duration,
        grace_period: Duration,
    ) -> Self {
        Self {
            run_start,
            cancel_token,
            metrics,
            hang_threshold,
            grace_period,
            session_factory: None,
            traffic_start_gate: None,
            rng_seed: None,
            target_identity: None,
        }
    }

    /// Attach a [`SessionFactory`] so scenarios can respawn fresh sessions
    /// (`cold_start` requires this; others ignore it). Additive builder —
    /// [`RunContext::new`]'s signature is unchanged, per the
    /// `#[non_exhaustive]` contract above.
    #[must_use]
    pub fn with_session_factory(mut self, factory: SessionFactory) -> Self {
        self.session_factory = Some(factory);
        self
    }

    /// Attach a controller-managed traffic start barrier.
    #[must_use]
    pub fn with_traffic_start_gate(mut self, gate: Arc<dyn TrafficStartGate>) -> Self {
        self.traffic_start_gate = Some(gate);
        self
    }

    /// Attach a deterministic weighted-pattern seed.
    #[must_use]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = Some(seed);
        self
    }

    /// Attach the target revision and canonical tool-inventory hash used by
    /// a distributed readiness frame.
    #[must_use]
    pub fn with_target_identity(
        mut self,
        protocol_version: String,
        tool_inventory_hash: String,
    ) -> Self {
        self.target_identity = Some((protocol_version, tool_inventory_hash));
        self
    }

    /// Convenience: time elapsed since run start.
    pub fn elapsed(&self) -> Duration {
        self.run_start.elapsed()
    }

    /// Convenience: true if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }
}

/// What a scenario reports back to the orchestrator after `drive()` returns.
///
/// **Locked for M2.** Field additions are non-breaking; field removal is.
/// Lives in `mcp-loadtest-core`.
pub use mcp_loadtest_core::outcome::ScenarioOutcome;

/// Map a [`SessionError`] to the [`CallOutcome`] bucket used by the metrics
/// recorder. Shared by `pattern`, `ramp`, and `soak` — when a new error case
/// is added to `SessionError` it must be added here exactly once.
pub(crate) fn classify_error(err: &SessionError) -> CallOutcome {
    use SessionError as E;
    use TransportError as T;
    match err {
        // Standard JSON-RPC failures mean a normal workload did not complete
        // the protocol exchange it asked for. Keep implementation-defined
        // `-32000..=-32099` failures in ServerError, but fail closed for the
        // five standard codes so a permissive error-rate threshold cannot
        // turn a protocol mismatch into PASS.
        E::Server(obj) if obj.code == -32700 || (-32603..=-32600).contains(&obj.code) => {
            CallOutcome::ProtocolError
        }
        E::Server(_) => CallOutcome::ServerError,
        E::Json(_) | E::ResponseShape(_) | E::IdMismatch { .. } | E::InvalidResponseId { .. } => {
            CallOutcome::Malformed
        }
        E::MismatchedSuccessResponse { .. } | E::MismatchedErrorResponse { .. } => {
            CallOutcome::Malformed
        }
        E::Io(_) => CallOutcome::Disconnected,
        E::Transport(T::Closed) | E::Transport(T::Io(_)) => CallOutcome::Disconnected,
        E::Transport(T::Timeout(_)) | E::StartupTimeout(_) => CallOutcome::Timeout,
        E::Transport(T::Http(_)) | E::Transport(T::Other(_)) => CallOutcome::ServerError,
        // Strict-validation reject: the call never reached the server, the
        // session is healthy — it's a protocol-level mismatch.
        E::SchemaViolation { .. } => CallOutcome::ProtocolError,
        E::InvalidJsonRpcVersion { .. } => CallOutcome::ProtocolError,
        // Strict-mode version gate (ADR 0018): produced by the run
        // orchestrator at spawn time, so scenario loops normally never see
        // it — but if one does (e.g. a pooled respawn), it's a
        // protocol-level mismatch like SchemaViolation.
        E::UnsupportedProtocolVersion { .. } => CallOutcome::ProtocolError,
        // `SessionError` is `#[non_exhaustive]` and now lives in another
        // crate (mcp-loadtest-protocol), so a cross-crate wildcard is
        // mandatory. Every variant known today is mapped explicitly above
        // (the tests below pin each one); a future variant falls back to the
        // generic server-error bucket rather than failing to compile here.
        _ => CallOutcome::ServerError,
    }
}

/// Whether a successful JSON-RPC `tools/call` envelope represents an MCP
/// logical tool failure (`isError: true`).
///
/// Every normal workload scenario uses this helper before incrementing
/// `successful_calls`; otherwise an all-tool-errors workload can report PASS.
pub(crate) fn is_logical_tool_error(result: &CallToolResult) -> bool {
    result.is_error
}

/// `true` when `err` indicates the underlying session is gone and the
/// scenario should stop driving this worker (rather than retry the next call).
pub(crate) fn is_terminal_error(err: &SessionError) -> bool {
    use SessionError as E;
    use TransportError as T;
    matches!(
        err,
        E::Transport(T::Closed) | E::Transport(T::Io(_)) | E::Io(_) | E::StartupTimeout(_)
    )
}

#[cfg(test)]
mod tests {
    //! Exhaustive coverage for [`classify_error`] / [`is_terminal_error`].
    //!
    //! The classifier tables in this module are the single source of truth
    //! used by every scenario; a silent miscategorization of a new
    //! `SessionError` or `TransportError` variant would skew error metrics
    //! and bypass terminal-error early-stop in worker loops. These tests
    //! pin the expected mapping for **every** known variant so adding a new
    //! one without updating the classifier fails compilation here.
    use super::*;
    use mcp_loadtest_protocol::jsonrpc::ErrorObject;
    use serde_json::json;

    fn io_err() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed")
    }

    fn server_err() -> ErrorObject {
        ErrorObject {
            code: -32000,
            message: "server error".to_string(),
            data: Some(json!({})),
        }
    }

    fn protocol_err(code: i64) -> ErrorObject {
        ErrorObject {
            code,
            message: "standard JSON-RPC error".to_string(),
            data: None,
        }
    }

    fn json_err() -> serde_json::Error {
        serde_json::from_str::<Value>("not json").unwrap_err()
    }

    #[test]
    fn classify_error_maps_every_session_error_variant() {
        // Direct SessionError variants.
        assert_eq!(
            classify_error(&SessionError::Server(server_err())),
            CallOutcome::ServerError,
        );
        for code in [-32700, -32600, -32601, -32602, -32603] {
            assert_eq!(
                classify_error(&SessionError::Server(protocol_err(code))),
                CallOutcome::ProtocolError,
                "standard JSON-RPC code {code} must fail closed",
            );
        }
        assert_eq!(
            classify_error(&SessionError::Json(json_err())),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::ResponseShape(json_err())),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::IdMismatch {
                expected: 1,
                got: 2,
            }),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::InvalidResponseId {
                expected: 1,
                got: Value::Null,
            }),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::MismatchedSuccessResponse {
                expected: 1,
                got: json!(2),
                result: json!({"tools": []}),
            }),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::MismatchedErrorResponse {
                expected: 1,
                got: Value::Null,
                error: protocol_err(-32600),
            }),
            CallOutcome::Malformed,
        );
        assert_eq!(
            classify_error(&SessionError::Io(io_err())),
            CallOutcome::Disconnected,
        );
        assert_eq!(
            classify_error(&SessionError::StartupTimeout(Duration::from_secs(10))),
            CallOutcome::Timeout,
        );
        assert_eq!(
            classify_error(&SessionError::SchemaViolation {
                tool: "echo".to_string(),
                summary: "args.x: expected type `string`".to_string(),
            }),
            CallOutcome::ProtocolError,
        );
        assert_eq!(
            classify_error(&SessionError::UnsupportedProtocolVersion {
                got: "9999-12-31".to_string(),
                advertised: "2025-03-26".to_string(),
            }),
            CallOutcome::ProtocolError,
        );
        assert_eq!(
            classify_error(&SessionError::InvalidJsonRpcVersion {
                got: "3.0".to_owned(),
            }),
            CallOutcome::ProtocolError,
        );

        // Every TransportError variant tunneled through SessionError::Transport.
        assert_eq!(
            classify_error(&SessionError::Transport(TransportError::Closed)),
            CallOutcome::Disconnected,
        );
        assert_eq!(
            classify_error(&SessionError::Transport(TransportError::Io(io_err()))),
            CallOutcome::Disconnected,
        );
        assert_eq!(
            classify_error(&SessionError::Transport(TransportError::Timeout(
                Duration::from_secs(5),
            ))),
            CallOutcome::Timeout,
        );
        assert_eq!(
            classify_error(&SessionError::Transport(TransportError::Http(
                "500 internal".to_string(),
            ))),
            CallOutcome::ServerError,
        );
        assert_eq!(
            classify_error(&SessionError::Transport(TransportError::Other(
                "weird".to_string(),
            ))),
            CallOutcome::ServerError,
        );
    }

    #[test]
    fn is_terminal_error_classifies_every_variant() {
        // Terminal: session is gone, worker should stop.
        assert!(is_terminal_error(&SessionError::Io(io_err())));
        assert!(is_terminal_error(&SessionError::StartupTimeout(
            Duration::from_secs(10),
        )));
        assert!(is_terminal_error(&SessionError::Transport(
            TransportError::Closed,
        )));
        assert!(is_terminal_error(&SessionError::Transport(
            TransportError::Io(io_err()),
        )));

        // Non-terminal: the session may still be usable; worker should not bail.
        assert!(!is_terminal_error(&SessionError::Server(server_err())));
        assert!(!is_terminal_error(&SessionError::Json(json_err())));
        assert!(!is_terminal_error(&SessionError::ResponseShape(json_err())));
        assert!(!is_terminal_error(&SessionError::IdMismatch {
            expected: 1,
            got: 2,
        }));
        assert!(!is_terminal_error(&SessionError::InvalidResponseId {
            expected: 1,
            got: Value::Null,
        }));
        assert!(!is_terminal_error(
            &SessionError::MismatchedSuccessResponse {
                expected: 1,
                got: json!(2),
                result: json!({"tools": []}),
            }
        ));
        assert!(!is_terminal_error(&SessionError::MismatchedErrorResponse {
            expected: 1,
            got: Value::Null,
            error: protocol_err(-32600),
        }));
        assert!(!is_terminal_error(&SessionError::Transport(
            TransportError::Timeout(Duration::from_secs(5)),
        )));
        assert!(!is_terminal_error(&SessionError::Transport(
            TransportError::Http("500 internal".to_string()),
        )));
        assert!(!is_terminal_error(&SessionError::Transport(
            TransportError::Other("weird".to_string()),
        )));
        assert!(!is_terminal_error(&SessionError::SchemaViolation {
            tool: "echo".to_string(),
            summary: "args.x: expected type `string`".to_string(),
        }));
        assert!(!is_terminal_error(&SessionError::InvalidJsonRpcVersion {
            got: "3.0".to_owned(),
        }));
    }
}
