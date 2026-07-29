//! Versioned controller/worker wire protocol.
//!
//! Frames are JSON objects separated by newlines. Every frame carries the
//! exact [`WIRE_PROTOCOL`] identifier; peers reject unknown identifiers
//! instead of attempting a best-effort downgrade.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::aggregate::AgentEvidence;
use mcp_loadtest_core::config::TokenEndpointAuthMethod;

/// Exact distributed control-plane wire identifier for the v0.2 MVP.
pub const WIRE_PROTOCOL: &str = "mcp-loadtest-dist/1";

/// A protocol envelope containing exactly one controller or agent message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFrame {
    /// Versioned control-plane protocol identifier.
    pub protocol: String,
    /// Tagged message payload.
    #[serde(flatten)]
    pub message: WireMessage,
}

impl WireFrame {
    /// Wrap a message in the current protocol envelope.
    pub fn new(message: WireMessage) -> Self {
        Self {
            protocol: WIRE_PROTOCOL.to_owned(),
            message,
        }
    }

    /// Return the job id when this message belongs to a prepared job.
    pub fn job_id(&self) -> Option<&str> {
        self.message.job_id()
    }
}

/// All messages permitted on an agent control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WireMessage {
    /// Worker capability greeting. Always the first agent-to-controller frame.
    Hello(HelloFrame),
    /// Controller job preparation request.
    Prepare(PrepareFrame),
    /// Worker readiness response after all local sessions are live.
    Ready(ReadyFrame),
    /// Controller start-gate release.
    Start(StartFrame),
    /// Cumulative checkpoint emitted during a running workload.
    Progress(ProgressFrame),
    /// Lightweight worker liveness signal.
    Heartbeat(HeartbeatFrame),
    /// Successful terminal result containing exact merge evidence.
    Finished(FinishedFrame),
    /// Sanitized terminal error.
    Failed(FailedFrame),
    /// Controller cancellation request.
    Cancel(CancelFrame),
    /// Worker cancellation acknowledgement.
    Cancelled(CancelledFrame),
}

impl WireMessage {
    /// Return the job id for every post-greeting message.
    pub fn job_id(&self) -> Option<&str> {
        match self {
            Self::Hello(_) => None,
            Self::Prepare(frame) => Some(&frame.job_id),
            Self::Ready(frame) => Some(&frame.job_id),
            Self::Start(frame) => Some(&frame.job_id),
            Self::Progress(frame) => Some(&frame.job_id),
            Self::Heartbeat(frame) => Some(&frame.job_id),
            Self::Finished(frame) => Some(&frame.job_id),
            Self::Failed(frame) => Some(&frame.job_id),
            Self::Cancel(frame) => Some(&frame.job_id),
            Self::Cancelled(frame) => Some(&frame.job_id),
        }
    }
}

/// Controller-originated message subset.
#[derive(Debug, Clone, PartialEq)]
pub enum ControllerMessage {
    /// Prepare a normalized, already-sharded job.
    Prepare(Box<PrepareFrame>),
    /// Release the workload start gate.
    Start(StartFrame),
    /// Cancel a prepared or running job.
    Cancel(CancelFrame),
}

impl From<ControllerMessage> for WireFrame {
    fn from(message: ControllerMessage) -> Self {
        let message = match message {
            ControllerMessage::Prepare(frame) => WireMessage::Prepare(*frame),
            ControllerMessage::Start(frame) => WireMessage::Start(frame),
            ControllerMessage::Cancel(frame) => WireMessage::Cancel(frame),
        };
        Self::new(message)
    }
}

/// Worker greeting and capability declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloFrame {
    /// Optional worker-local identity.
    ///
    /// Ephemeral SSH workers normally send `None`: only the controller knows
    /// the inventory slot before [`PrepareFrame`] arrives. A locally pinned
    /// name is accepted only when it matches that slot.
    pub agent_name: Option<String>,
    /// Full `mcp-loadtest` binary version.
    pub binary_version: String,
    /// Scenario kinds accepted by the worker.
    pub scenarios: Vec<SupportedScenario>,
    /// Maximum local session concurrency allowed by worker policy.
    pub max_concurrency: u32,
}

/// Controller request to prepare all local sessions without starting traffic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrepareFrame {
    /// Controller-generated unique job id.
    pub job_id: String,
    /// Digest of the canonical unsharded config for mismatch detection.
    pub config_digest: String,
    /// Remote MCP endpoint. No literal credential values are carried.
    pub target: RemoteTarget,
    /// Normalized agent-local workload.
    pub plan: AgentWorkloadPlan,
    /// Deterministic global-to-local concurrency assignment.
    pub shard: AgentShard,
    /// Required interval between heartbeat frames while preparing or running.
    pub heartbeat_interval_ms: u64,
}

/// Worker response after target discovery and all requested local sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadyFrame {
    /// Job id copied from [`PrepareFrame`].
    pub job_id: String,
    /// Agent inventory name copied from the greeting.
    pub agent_name: String,
    /// Number of local sessions that completed target startup.
    pub live_workers: u32,
    /// Number of sessions assigned by the shard.
    pub requested_workers: u32,
    /// Hash of the canonical `tools/list` inventory.
    pub tool_inventory_hash: String,
    /// Target MCP revision observed by this worker.
    pub target_protocol_version: String,
}

/// Controller release of the cross-machine start gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartFrame {
    /// Prepared job id.
    pub job_id: String,
    /// Relative lead time from receipt to local gate release.
    ///
    /// Relative time avoids relying on synchronized wall clocks. The
    /// controller sends start frames concurrently and reports observed skew.
    pub start_after_ms: u64,
}

/// Cumulative, replace-by-sequence running checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressFrame {
    /// Running job id.
    pub job_id: String,
    /// Strictly increasing sequence number for idempotent replacement.
    pub sequence: u64,
    /// Latest cumulative evidence. It is never added to an earlier checkpoint
    /// from the same agent.
    pub evidence: AgentEvidence,
}

/// Lightweight liveness frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatFrame {
    /// Active job id.
    pub job_id: String,
    /// Monotonic heartbeat sequence number.
    pub sequence: u64,
}

/// Successful worker completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishedFrame {
    /// Completed job id.
    pub job_id: String,
    /// Final cumulative evidence replacing all prior progress checkpoints.
    pub evidence: AgentEvidence,
}

/// Sanitized worker-side failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedFrame {
    /// Affected job id.
    pub job_id: String,
    /// Stable machine-readable error code.
    pub code: String,
    /// Sanitized description that must not contain credentials.
    pub message: String,
    /// Whether retrying before the global start gate could succeed.
    pub retryable: bool,
}

/// Controller cancellation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelFrame {
    /// Affected job id.
    pub job_id: String,
    /// Sanitized cancellation reason.
    pub reason: String,
}

/// Worker acknowledgement that cancellation and bounded teardown completed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelledFrame {
    /// Cancelled job id.
    pub job_id: String,
    /// Sanitized final disposition.
    pub reason: String,
}

/// Scenarios with locked distributed semantics in the v0.2 MVP.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SupportedScenario {
    /// Single-tool or weighted-pattern constant load.
    Sustained,
    /// Explicit weighted multi-step patterns.
    Pattern,
}

/// Controller-side, global concurrency workload before sharding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkloadPlan {
    /// Supported scenario kind.
    pub scenario: SupportedScenario,
    /// Total concurrency across every active agent.
    pub global_concurrency: u32,
    /// Common measurement duration on every agent.
    pub duration_ms: u64,
    /// Normalized weighted patterns. A single-tool sustained run is a
    /// one-pattern, one-step plan.
    pub patterns: Vec<PatternPlan>,
    /// Deterministic base seed; agents derive a shard-specific seed.
    pub seed: u64,
}

impl WorkloadPlan {
    /// Produce an agent-local plan from a validated shard.
    pub fn for_shard(&self, shard: &AgentShard) -> AgentWorkloadPlan {
        AgentWorkloadPlan {
            scenario: self.scenario,
            concurrency: shard.concurrency,
            duration_ms: self.duration_ms,
            patterns: self.patterns.clone(),
            seed: self.seed ^ u64::from(shard.index),
        }
    }
}

/// Worker-local workload after deterministic sharding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentWorkloadPlan {
    /// Supported scenario kind.
    pub scenario: SupportedScenario,
    /// Local session concurrency.
    pub concurrency: u32,
    /// Measurement duration, shared by all agents.
    pub duration_ms: u64,
    /// Normalized weighted patterns.
    pub patterns: Vec<PatternPlan>,
    /// Shard-specific deterministic random seed.
    pub seed: u64,
}

/// One normalized weighted pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatternPlan {
    /// Human-readable pattern name.
    pub name: String,
    /// Positive relative selection weight.
    pub weight: f64,
    /// Delay between consecutive steps.
    pub think_time_ms: u64,
    /// Whether a failed step continues or aborts this pattern iteration.
    pub on_step_error: PatternErrorPolicy,
    /// Ordered MCP tool calls.
    pub steps: Vec<PatternStepPlan>,
}

/// Error policy for a normalized multi-step pattern.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatternErrorPolicy {
    /// Record the error and continue with the next step.
    Continue,
    /// Stop the current pattern iteration after the first failed step.
    Abort,
}

/// One normalized MCP `tools/call` step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatternStepPlan {
    /// MCP tool name.
    pub tool: String,
    /// Tool arguments object.
    pub args: Value,
}

/// Remote MCP transport accepted by a distributed worker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTransport {
    /// Streamable HTTP.
    Http,
    /// Legacy HTTP plus server-sent events.
    Sse,
    /// WebSocket transport.
    Ws,
}

/// Secret-free remote target recipe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Remote transport.
    pub transport: RemoteTransport,
    /// Endpoint URL.
    pub url: String,
    /// Target startup/connect/discovery budget.
    pub startup_timeout_ms: u64,
    /// MCP revision pin or `auto`.
    pub protocol_version: Option<String>,
    /// Header name to environment-variable name.
    ///
    /// Only names cross the control channel; each worker resolves values from
    /// its own environment immediately before connecting.
    pub headers_from_env: BTreeMap<String, String>,
    /// Target-host entries passed to the core SSRF guard.
    ///
    /// These never widen the independent agent-local policy.
    pub allowed_hosts: Vec<String>,
    /// Enable strict tool input/output validation.
    pub strict_validation: bool,
    /// Optional secret-free recipe for distributed client-credentials OAuth.
    ///
    /// Secret and token values never cross the control channel; the worker
    /// resolves `client_secret_env` from its own environment.
    pub auth: Option<RemoteClientCredentialsAuth>,
}

/// Secret-free, pre-registered OAuth client-credentials recipe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteClientCredentialsAuth {
    /// Public OAuth client identifier.
    pub client_id: String,
    /// Environment variable containing the client secret on each worker.
    pub client_secret_env: String,
    /// Token endpoint authentication method.
    pub token_endpoint_auth_method: TokenEndpointAuthMethod,
    /// Initial requested scopes.
    pub scopes: Vec<String>,
    /// Must remain false for distributed client credentials.
    pub offline_access: bool,
    /// Bounded insufficient-scope retry count.
    pub max_step_up_retries: u8,
}

/// Deterministic assignment of global concurrency to one agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentShard {
    /// Agent inventory name.
    pub agent_name: String,
    /// Zero-based position after stable name sorting.
    pub index: u32,
    /// Total number of active agents.
    pub agent_count: u32,
    /// Local concurrency assigned to this agent.
    pub concurrency: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_error_policy_round_trips_without_semantic_loss() {
        let plan = PatternPlan {
            name: "abort-on-write-error".to_owned(),
            weight: 1.0,
            think_time_ms: 10,
            on_step_error: PatternErrorPolicy::Abort,
            steps: vec![PatternStepPlan {
                tool: "write".to_owned(),
                args: serde_json::json!({"value": 1}),
            }],
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains(r#""on_step_error":"abort""#));
        let decoded: PatternPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.on_step_error, PatternErrorPolicy::Abort);
        assert_eq!(decoded, plan);
    }

    #[test]
    fn client_credentials_wire_recipe_contains_names_not_secret_values() {
        let auth = RemoteClientCredentialsAuth {
            client_id: "load-generator".to_owned(),
            client_secret_env: "MCP_CLIENT_SECRET".to_owned(),
            token_endpoint_auth_method: TokenEndpointAuthMethod::ClientSecretBasic,
            scopes: vec!["mcp:read".to_owned()],
            offline_access: false,
            max_step_up_retries: 2,
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.contains("MCP_CLIENT_SECRET"));
        assert!(!json.contains("actual-secret-value"));
        let decoded: RemoteClientCredentialsAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, auth);
    }

    #[test]
    fn unbound_hello_round_trips() {
        let frame = WireFrame::new(WireMessage::Hello(HelloFrame {
            agent_name: None,
            binary_version: "0.2.0".to_owned(),
            scenarios: vec![SupportedScenario::Sustained],
            max_concurrency: 100,
        }));
        let json = serde_json::to_string(&frame).unwrap();
        let decoded: WireFrame = serde_json::from_str(&json).unwrap();
        let WireMessage::Hello(hello) = decoded.message else {
            panic!("expected hello");
        };
        assert_eq!(hello.agent_name, None);
    }
}
