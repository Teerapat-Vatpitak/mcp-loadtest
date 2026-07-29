//! Distributed load-generation primitives for `mcp-loadtest`.
//!
//! The v0.2 control plane deliberately uses short-lived workers launched over
//! OpenSSH. SSH supplies encryption, host authentication, and user
//! authentication; this crate supplies only a versioned, transport-neutral
//! control protocol over the worker's stdin/stdout. The same protocol can be
//! carried by another [`AgentChannel`] in a future release without changing
//! workload or aggregation semantics.
//!
//! This crate does not execute an engine run itself yet. It owns the
//! contracts that the engine and CLI integrate with:
//! shard planning, normalized remote-only plans, bounded NDJSON framing,
//! controller/agent state machines, exact HDR-histogram aggregation, and a
//! safe OpenSSH launcher.

pub mod aggregate;
pub mod channel;
pub mod launcher;
pub mod policy;
pub mod protocol;
pub mod shard;
pub mod state;

pub use aggregate::{
    AgentEvidence, AggregateError, AggregatedEvidence, HistogramEvidence, MetricsEvidence,
    aggregate_evidence,
};
pub use channel::{AgentChannel, ChannelError, NdjsonChannel};
pub use launcher::{SshAgentProcess, SshAgentSpec, SshCommand, SshLaunchError, SshLauncher};
pub use policy::{AgentPolicy, PolicyError};
pub use protocol::{
    AgentShard, AgentWorkloadPlan, CancelFrame, CancelledFrame, ControllerMessage, FailedFrame,
    FinishedFrame, HeartbeatFrame, HelloFrame, PatternErrorPolicy, PatternPlan, PatternStepPlan,
    PrepareFrame, ProgressFrame, ReadyFrame, RemoteClientCredentialsAuth, RemoteTarget,
    RemoteTransport, StartFrame, SupportedScenario, WIRE_PROTOCOL, WireFrame, WireMessage,
    WorkloadPlan,
};
pub use shard::{ShardError, plan_shards};
pub use state::{
    AgentDirective, AgentPhase, AgentStateError, AgentStateMachine, ControllerAgentPhase,
    ControllerJobState, ControllerStateError,
};
