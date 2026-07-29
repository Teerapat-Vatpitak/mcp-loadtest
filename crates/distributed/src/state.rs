//! Pure controller and worker state machines.
//!
//! Network orchestration owns timeouts and concurrent I/O. These types own
//! legal frame ordering, job/agent identity checks, readiness gating,
//! cumulative progress replacement, and terminal evidence retention.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::aggregate::AgentEvidence;
use crate::protocol::{
    CancelFrame, CancelledFrame, FailedFrame, FinishedFrame, HeartbeatFrame, HelloFrame,
    PrepareFrame, ProgressFrame, ReadyFrame, StartFrame, WIRE_PROTOCOL, WireFrame, WireMessage,
};

/// Controller's phase for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerAgentPhase {
    /// Waiting for the worker greeting.
    AwaitingHello,
    /// Prepare was sent; target/session readiness is pending.
    Preparing,
    /// Every assigned local session is live.
    Ready,
    /// Start gate was released.
    Running,
    /// Final evidence received.
    Finished,
    /// Worker reported a terminal failure.
    Failed,
    /// Cancellation sent; acknowledgement pending.
    Cancelling,
    /// Worker acknowledged bounded cancellation and teardown.
    Cancelled,
}

struct ControllerAgentState {
    phase: ControllerAgentPhase,
    prepare: PrepareFrame,
    latest_progress: Option<(u64, AgentEvidence)>,
    final_evidence: Option<AgentEvidence>,
    failure: Option<FailedFrame>,
}

/// Cohort-level controller protocol state.
pub struct ControllerJobState {
    job_id: String,
    agents: BTreeMap<String, ControllerAgentState>,
    ready_signature: Option<(String, String)>,
}

impl ControllerJobState {
    /// Build a strict all-agents-required job from one prepare frame per
    /// deterministic shard.
    pub fn new(prepares: Vec<PrepareFrame>) -> Result<Self, ControllerStateError> {
        let job_id = prepares
            .first()
            .ok_or(ControllerStateError::EmptyCohort)?
            .job_id
            .clone();
        let mut agents = BTreeMap::new();
        for prepare in prepares {
            if prepare.job_id != job_id {
                return Err(ControllerStateError::JobMismatch {
                    expected: job_id,
                    got: prepare.job_id,
                });
            }
            let name = prepare.shard.agent_name.clone();
            if agents
                .insert(
                    name.clone(),
                    ControllerAgentState {
                        phase: ControllerAgentPhase::AwaitingHello,
                        prepare,
                        latest_progress: None,
                        final_evidence: None,
                        failure: None,
                    },
                )
                .is_some()
            {
                return Err(ControllerStateError::DuplicateAgent(name));
            }
        }
        Ok(Self {
            job_id,
            agents,
            ready_signature: None,
        })
    }

    /// Unique job id shared by the cohort.
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// Current phase of one inventory agent.
    pub fn phase(&self, agent: &str) -> Option<ControllerAgentPhase> {
        self.agents.get(agent).map(|state| state.phase)
    }

    /// Accept one agent frame and optionally return the immediate controller
    /// response (`Prepare`). Start and cancellation remain cohort operations.
    pub fn accept(
        &mut self,
        agent: &str,
        frame: WireFrame,
    ) -> Result<Option<WireFrame>, ControllerStateError> {
        if frame.protocol != WIRE_PROTOCOL {
            return Err(ControllerStateError::UnsupportedProtocol(frame.protocol));
        }
        let state = self
            .agents
            .get_mut(agent)
            .ok_or_else(|| ControllerStateError::UnknownAgent(agent.to_owned()))?;
        match (state.phase, frame.message) {
            (ControllerAgentPhase::AwaitingHello, WireMessage::Hello(hello)) => {
                validate_hello(agent, &hello, &state.prepare)?;
                state.phase = ControllerAgentPhase::Preparing;
                Ok(Some(WireFrame::new(WireMessage::Prepare(
                    state.prepare.clone(),
                ))))
            }
            (ControllerAgentPhase::Preparing, WireMessage::Ready(ready)) => {
                validate_job(&self.job_id, &ready.job_id)?;
                if ready.agent_name != agent {
                    return Err(ControllerStateError::AgentMismatch {
                        expected: agent.to_owned(),
                        got: ready.agent_name,
                    });
                }
                if ready.requested_workers != state.prepare.shard.concurrency
                    || ready.live_workers != ready.requested_workers
                    || ready.tool_inventory_hash.is_empty()
                    || ready.target_protocol_version.is_empty()
                {
                    state.phase = ControllerAgentPhase::Failed;
                    return Err(ControllerStateError::ReadinessShortfall {
                        agent: agent.to_owned(),
                        requested: state.prepare.shard.concurrency,
                        live: ready.live_workers,
                    });
                }
                let signature = (
                    ready.target_protocol_version.clone(),
                    ready.tool_inventory_hash.clone(),
                );
                if let Some((expected_protocol, expected_inventory)) = &self.ready_signature {
                    if ready.target_protocol_version != *expected_protocol {
                        state.phase = ControllerAgentPhase::Failed;
                        return Err(ControllerStateError::TargetProtocolMismatch {
                            agent: agent.to_owned(),
                            expected: expected_protocol.clone(),
                            got: ready.target_protocol_version,
                        });
                    }
                    if ready.tool_inventory_hash != *expected_inventory {
                        state.phase = ControllerAgentPhase::Failed;
                        return Err(ControllerStateError::ToolInventoryMismatch(
                            agent.to_owned(),
                        ));
                    }
                } else {
                    self.ready_signature = Some(signature);
                }
                state.phase = ControllerAgentPhase::Ready;
                Ok(None)
            }
            (ControllerAgentPhase::Running, WireMessage::Progress(progress)) => {
                validate_job(&self.job_id, &progress.job_id)?;
                validate_evidence_agent(agent, &progress.evidence)?;
                if state
                    .latest_progress
                    .as_ref()
                    .is_none_or(|(sequence, _)| progress.sequence > *sequence)
                {
                    state.latest_progress = Some((progress.sequence, progress.evidence));
                }
                Ok(None)
            }
            (
                ControllerAgentPhase::Preparing
                | ControllerAgentPhase::Ready
                | ControllerAgentPhase::Running,
                WireMessage::Heartbeat(heartbeat),
            ) => {
                validate_job(&self.job_id, &heartbeat.job_id)?;
                Ok(None)
            }
            (ControllerAgentPhase::Running, WireMessage::Finished(finished)) => {
                validate_job(&self.job_id, &finished.job_id)?;
                validate_evidence_agent(agent, &finished.evidence)?;
                state.final_evidence = Some(finished.evidence);
                state.phase = ControllerAgentPhase::Finished;
                Ok(None)
            }
            (ControllerAgentPhase::Cancelling, WireMessage::Cancelled(cancelled)) => {
                validate_job(&self.job_id, &cancelled.job_id)?;
                state.phase = ControllerAgentPhase::Cancelled;
                Ok(None)
            }
            (phase, WireMessage::Failed(failure))
                if !matches!(
                    phase,
                    ControllerAgentPhase::Finished
                        | ControllerAgentPhase::Failed
                        | ControllerAgentPhase::Cancelled
                ) =>
            {
                validate_job(&self.job_id, &failure.job_id)?;
                state.failure = Some(failure);
                state.phase = ControllerAgentPhase::Failed;
                Ok(None)
            }
            (phase, message) => Err(ControllerStateError::UnexpectedFrame {
                agent: agent.to_owned(),
                phase,
                message: message_name(&message),
            }),
        }
    }

    /// True only after every configured agent has reported full readiness.
    pub fn all_ready(&self) -> bool {
        self.agents
            .values()
            .all(|state| state.phase == ControllerAgentPhase::Ready)
    }

    /// Release every ready agent with one common relative lead time.
    pub fn start_all(
        &mut self,
        start_after_ms: u64,
    ) -> Result<Vec<(String, WireFrame)>, ControllerStateError> {
        if !self.all_ready() {
            return Err(ControllerStateError::CohortNotReady);
        }
        Ok(self
            .agents
            .iter_mut()
            .map(|(name, state)| {
                state.phase = ControllerAgentPhase::Running;
                (
                    name.clone(),
                    WireFrame::new(WireMessage::Start(StartFrame {
                        job_id: self.job_id.clone(),
                        start_after_ms,
                    })),
                )
            })
            .collect())
    }

    /// Cancel every non-terminal agent. The controller should send returned
    /// frames concurrently and enforce an outer teardown deadline.
    pub fn cancel_all(&mut self, reason: impl Into<String>) -> Vec<(String, WireFrame)> {
        let reason = reason.into();
        self.agents
            .iter_mut()
            .filter_map(|(name, state)| {
                if matches!(
                    state.phase,
                    ControllerAgentPhase::Finished
                        | ControllerAgentPhase::Failed
                        | ControllerAgentPhase::Cancelled
                ) {
                    return None;
                }
                if state.phase == ControllerAgentPhase::AwaitingHello {
                    // The peer does not know the job id yet, so no protocol
                    // cancellation can be valid. Closing the SSH process is
                    // the controller's cancellation mechanism here.
                    state.phase = ControllerAgentPhase::Cancelled;
                    return None;
                }
                state.phase = ControllerAgentPhase::Cancelling;
                Some((
                    name.clone(),
                    WireFrame::new(WireMessage::Cancel(CancelFrame {
                        job_id: self.job_id.clone(),
                        reason: reason.clone(),
                    })),
                ))
            })
            .collect()
    }

    /// True when every agent reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.agents.values().all(|state| {
            matches!(
                state.phase,
                ControllerAgentPhase::Finished
                    | ControllerAgentPhase::Failed
                    | ControllerAgentPhase::Cancelled
            )
        })
    }

    /// True if any agent reported a failure or did not complete normally.
    pub fn has_failure(&self) -> bool {
        self.agents
            .values()
            .any(|state| state.phase == ControllerAgentPhase::Failed)
    }

    /// Borrow latest cumulative evidence, preferring the final result.
    pub fn latest_evidence(&self, agent: &str) -> Option<&AgentEvidence> {
        let state = self.agents.get(agent)?;
        state
            .final_evidence
            .as_ref()
            .or_else(|| state.latest_progress.as_ref().map(|(_, evidence)| evidence))
    }

    /// Final evidence in stable agent-name order.
    ///
    /// Returns an error unless every configured agent finished normally.
    pub fn final_evidence(&self) -> Result<Vec<&AgentEvidence>, ControllerStateError> {
        self.agents
            .iter()
            .map(|(name, state)| {
                state
                    .final_evidence
                    .as_ref()
                    .ok_or_else(|| ControllerStateError::MissingFinalEvidence(name.clone()))
            })
            .collect()
    }
}

/// Worker-side protocol phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    /// Greeting can be sent; waiting for prepare.
    AwaitingPrepare,
    /// Engine/session preparation is in progress.
    Preparing,
    /// Local sessions are live; waiting for start.
    Ready,
    /// Workload is running.
    Running,
    /// Final evidence was sent.
    Finished,
    /// Failure was sent.
    Failed,
    /// Local cancellation/teardown is in progress.
    Cancelling,
    /// Cancellation acknowledgement was sent.
    Cancelled,
}

/// Side effect requested by a legal controller frame.
#[derive(Debug, Clone)]
pub enum AgentDirective {
    /// Prepare target sessions and later call
    /// [`AgentStateMachine::mark_ready`].
    Prepare(Box<PrepareFrame>),
    /// Release the engine start gate after the relative lead.
    Start(StartFrame),
    /// Cancel the engine token and perform bounded teardown.
    Cancel(CancelFrame),
}

/// Worker-side frame-order state.
pub struct AgentStateMachine {
    hello: HelloFrame,
    phase: AgentPhase,
    job_id: Option<String>,
    bound_agent_name: Option<String>,
}

impl AgentStateMachine {
    /// Create a worker state machine from locally-derived capabilities.
    pub fn new(hello: HelloFrame) -> Self {
        Self {
            hello,
            phase: AgentPhase::AwaitingPrepare,
            job_id: None,
            bound_agent_name: None,
        }
    }

    /// Current worker phase.
    pub fn phase(&self) -> AgentPhase {
        self.phase
    }

    /// Greeting that must be the first outbound frame.
    pub fn hello(&self) -> WireFrame {
        WireFrame::new(WireMessage::Hello(self.hello.clone()))
    }

    /// Validate one controller frame and return the engine-side directive.
    pub fn accept(&mut self, frame: WireFrame) -> Result<AgentDirective, AgentStateError> {
        if frame.protocol != WIRE_PROTOCOL {
            return Err(AgentStateError::UnsupportedProtocol(frame.protocol));
        }
        match (self.phase, frame.message) {
            (AgentPhase::AwaitingPrepare, WireMessage::Prepare(prepare)) => {
                if self
                    .hello
                    .agent_name
                    .as_ref()
                    .is_some_and(|name| *name != prepare.shard.agent_name)
                {
                    return Err(AgentStateError::AgentMismatch {
                        expected: self
                            .hello
                            .agent_name
                            .clone()
                            .expect("checked as Some above"),
                        got: prepare.shard.agent_name,
                    });
                }
                self.job_id = Some(prepare.job_id.clone());
                self.bound_agent_name = Some(prepare.shard.agent_name.clone());
                self.phase = AgentPhase::Preparing;
                Ok(AgentDirective::Prepare(Box::new(prepare)))
            }
            (AgentPhase::Ready, WireMessage::Start(start)) => {
                self.validate_job(&start.job_id)?;
                self.phase = AgentPhase::Running;
                Ok(AgentDirective::Start(start))
            }
            (phase, WireMessage::Cancel(cancel))
                if !matches!(
                    phase,
                    AgentPhase::Finished
                        | AgentPhase::Failed
                        | AgentPhase::Cancelled
                        | AgentPhase::AwaitingPrepare
                ) =>
            {
                self.validate_job(&cancel.job_id)?;
                self.phase = AgentPhase::Cancelling;
                Ok(AgentDirective::Cancel(cancel))
            }
            (phase, message) => Err(AgentStateError::UnexpectedFrame {
                phase,
                message: message_name(&message),
            }),
        }
    }

    /// Mark successful local session readiness and build the response frame.
    pub fn mark_ready(&mut self, ready: ReadyFrame) -> Result<WireFrame, AgentStateError> {
        self.require_phase(AgentPhase::Preparing)?;
        self.validate_job(&ready.job_id)?;
        let expected = self.required_agent_name()?;
        if ready.agent_name != expected {
            return Err(AgentStateError::AgentMismatch {
                expected: expected.to_owned(),
                got: ready.agent_name,
            });
        }
        self.phase = AgentPhase::Ready;
        Ok(WireFrame::new(WireMessage::Ready(ready)))
    }

    /// Build a cumulative progress checkpoint.
    pub fn progress(
        &self,
        sequence: u64,
        evidence: AgentEvidence,
    ) -> Result<WireFrame, AgentStateError> {
        self.require_phase(AgentPhase::Running)?;
        validate_evidence_agent(self.required_agent_name()?, &evidence)
            .map_err(|error| AgentStateError::Evidence(error.to_string()))?;
        Ok(WireFrame::new(WireMessage::Progress(ProgressFrame {
            job_id: self.required_job()?.to_owned(),
            sequence,
            evidence,
        })))
    }

    /// Build a heartbeat frame.
    pub fn heartbeat(&self, sequence: u64) -> Result<WireFrame, AgentStateError> {
        if !matches!(
            self.phase,
            AgentPhase::Preparing | AgentPhase::Ready | AgentPhase::Running
        ) {
            return Err(AgentStateError::WrongPhase {
                expected: AgentPhase::Running,
                actual: self.phase,
            });
        }
        Ok(WireFrame::new(WireMessage::Heartbeat(HeartbeatFrame {
            job_id: self.required_job()?.to_owned(),
            sequence,
        })))
    }

    /// Mark normal completion and build the final frame.
    pub fn finish(&mut self, evidence: AgentEvidence) -> Result<WireFrame, AgentStateError> {
        self.require_phase(AgentPhase::Running)?;
        validate_evidence_agent(self.required_agent_name()?, &evidence)
            .map_err(|error| AgentStateError::Evidence(error.to_string()))?;
        self.phase = AgentPhase::Finished;
        Ok(WireFrame::new(WireMessage::Finished(FinishedFrame {
            job_id: self.required_job()?.to_owned(),
            evidence,
        })))
    }

    /// Mark a sanitized terminal failure.
    pub fn fail(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<WireFrame, AgentStateError> {
        let job_id = self.required_job()?.to_owned();
        self.phase = AgentPhase::Failed;
        Ok(WireFrame::new(WireMessage::Failed(FailedFrame {
            job_id,
            code: code.into(),
            message: message.into(),
            retryable,
        })))
    }

    /// Acknowledge cancellation after engine teardown completes.
    pub fn cancelled(&mut self, reason: impl Into<String>) -> Result<WireFrame, AgentStateError> {
        self.require_phase(AgentPhase::Cancelling)?;
        let job_id = self.required_job()?.to_owned();
        self.phase = AgentPhase::Cancelled;
        Ok(WireFrame::new(WireMessage::Cancelled(CancelledFrame {
            job_id,
            reason: reason.into(),
        })))
    }

    fn require_phase(&self, expected: AgentPhase) -> Result<(), AgentStateError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(AgentStateError::WrongPhase {
                expected,
                actual: self.phase,
            })
        }
    }

    fn required_job(&self) -> Result<&str, AgentStateError> {
        self.job_id
            .as_deref()
            .ok_or(AgentStateError::JobNotPrepared)
    }

    fn required_agent_name(&self) -> Result<&str, AgentStateError> {
        self.bound_agent_name
            .as_deref()
            .ok_or(AgentStateError::JobNotPrepared)
    }

    fn validate_job(&self, got: &str) -> Result<(), AgentStateError> {
        let expected = self.required_job()?;
        if got == expected {
            Ok(())
        } else {
            Err(AgentStateError::JobMismatch {
                expected: expected.to_owned(),
                got: got.to_owned(),
            })
        }
    }
}

/// Controller state-machine failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ControllerStateError {
    /// No agents were supplied.
    #[error("controller job requires at least one agent")]
    EmptyCohort,
    /// Inventory names must be unique.
    #[error("duplicate controller agent `{0}`")]
    DuplicateAgent(String),
    /// A frame referenced an unknown agent.
    #[error("unknown controller agent `{0}`")]
    UnknownAgent(String),
    /// Wire identifier mismatch.
    #[error("unsupported distributed protocol `{0}`")]
    UnsupportedProtocol(String),
    /// Job id mismatch.
    #[error("job id mismatch: expected `{expected}`, got `{got}`")]
    JobMismatch {
        /// Expected job id.
        expected: String,
        /// Received job id.
        got: String,
    },
    /// Agent identity mismatch.
    #[error("agent mismatch: expected `{expected}`, got `{got}`")]
    AgentMismatch {
        /// Inventory name.
        expected: String,
        /// Frame name.
        got: String,
    },
    /// Worker does not implement the requested scenario/concurrency.
    #[error("agent `{0}` does not satisfy requested capabilities")]
    CapabilityMismatch(String),
    /// Worker failed to prepare every assigned local session.
    #[error("agent `{agent}` prepared {live}/{requested} requested workers")]
    ReadinessShortfall {
        /// Agent name.
        agent: String,
        /// Assigned workers.
        requested: u32,
        /// Live workers.
        live: u32,
    },
    /// Ready agents observed different target MCP revisions.
    #[error("agent `{agent}` target protocol mismatch: expected `{expected}`, got `{got}`")]
    TargetProtocolMismatch {
        /// Agent with the mismatch.
        agent: String,
        /// First ready agent's target revision.
        expected: String,
        /// Conflicting target revision.
        got: String,
    },
    /// Ready agents observed different tool inventories.
    #[error("agent `{0}` observed a different target tool inventory")]
    ToolInventoryMismatch(String),
    /// Frame ordering violation.
    #[error("unexpected `{message}` from `{agent}` while in {phase:?}")]
    UnexpectedFrame {
        /// Agent name.
        agent: String,
        /// Current phase.
        phase: ControllerAgentPhase,
        /// Message tag.
        message: &'static str,
    },
    /// Start was requested before every worker was ready.
    #[error("cannot start before every configured agent is ready")]
    CohortNotReady,
    /// Final aggregation was requested without a normal result.
    #[error("agent `{0}` has no final evidence")]
    MissingFinalEvidence(String),
    /// Evidence claimed a different agent.
    #[error("invalid evidence identity: {0}")]
    Evidence(String),
}

/// Worker state-machine failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentStateError {
    /// Wire identifier mismatch.
    #[error("unsupported distributed protocol `{0}`")]
    UnsupportedProtocol(String),
    /// Job id mismatch.
    #[error("job id mismatch: expected `{expected}`, got `{got}`")]
    JobMismatch {
        /// Prepared id.
        expected: String,
        /// Frame id.
        got: String,
    },
    /// Prepare/evidence identity mismatch.
    #[error("agent mismatch: expected `{expected}`, got `{got}`")]
    AgentMismatch {
        /// Local inventory name.
        expected: String,
        /// Frame name.
        got: String,
    },
    /// No prepare frame has established a job.
    #[error("agent has not prepared a job")]
    JobNotPrepared,
    /// A method was called in the wrong phase.
    #[error("operation requires {expected:?}, agent is {actual:?}")]
    WrongPhase {
        /// Required phase.
        expected: AgentPhase,
        /// Actual phase.
        actual: AgentPhase,
    },
    /// Frame ordering violation.
    #[error("unexpected `{message}` while in {phase:?}")]
    UnexpectedFrame {
        /// Current phase.
        phase: AgentPhase,
        /// Message tag.
        message: &'static str,
    },
    /// Evidence identity validation failed.
    #[error("invalid agent evidence: {0}")]
    Evidence(String),
}

fn validate_hello(
    agent: &str,
    hello: &HelloFrame,
    prepare: &PrepareFrame,
) -> Result<(), ControllerStateError> {
    if hello
        .agent_name
        .as_ref()
        .is_some_and(|hello_name| hello_name != agent)
    {
        return Err(ControllerStateError::AgentMismatch {
            expected: agent.to_owned(),
            got: hello.agent_name.clone().expect("checked as Some above"),
        });
    }
    if !hello.scenarios.contains(&prepare.plan.scenario)
        || hello.max_concurrency < prepare.plan.concurrency
    {
        return Err(ControllerStateError::CapabilityMismatch(agent.to_owned()));
    }
    Ok(())
}

fn validate_job(expected: &str, got: &str) -> Result<(), ControllerStateError> {
    if expected == got {
        Ok(())
    } else {
        Err(ControllerStateError::JobMismatch {
            expected: expected.to_owned(),
            got: got.to_owned(),
        })
    }
}

fn validate_evidence_agent(
    expected: &str,
    evidence: &AgentEvidence,
) -> Result<(), ControllerStateError> {
    if evidence.agent_name == expected && evidence.shard.agent_name == expected {
        Ok(())
    } else {
        Err(ControllerStateError::Evidence(format!(
            "expected `{expected}`, got evidence `{}` / shard `{}`",
            evidence.agent_name, evidence.shard.agent_name
        )))
    }
}

fn message_name(message: &WireMessage) -> &'static str {
    match message {
        WireMessage::Hello(_) => "hello",
        WireMessage::Prepare(_) => "prepare",
        WireMessage::Ready(_) => "ready",
        WireMessage::Start(_) => "start",
        WireMessage::Progress(_) => "progress",
        WireMessage::Heartbeat(_) => "heartbeat",
        WireMessage::Finished(_) => "finished",
        WireMessage::Failed(_) => "failed",
        WireMessage::Cancel(_) => "cancel",
        WireMessage::Cancelled(_) => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::protocol::{
        AgentShard, AgentWorkloadPlan, PatternPlan, PatternStepPlan, RemoteTarget, RemoteTransport,
        SupportedScenario,
    };

    fn prepare(name: &str, index: u32) -> PrepareFrame {
        PrepareFrame {
            job_id: "job-1".to_owned(),
            config_digest: "sha256:config".to_owned(),
            target: RemoteTarget {
                transport: RemoteTransport::Http,
                url: "https://api.example/mcp".to_owned(),
                startup_timeout_ms: 10_000,
                protocol_version: Some("2025-11-25".to_owned()),
                headers_from_env: BTreeMap::new(),
                allowed_hosts: Vec::new(),
                strict_validation: true,
                auth: None,
            },
            plan: AgentWorkloadPlan {
                scenario: SupportedScenario::Sustained,
                concurrency: 1,
                duration_ms: 1_000,
                patterns: vec![PatternPlan {
                    name: "echo".to_owned(),
                    weight: 1.0,
                    think_time_ms: 0,
                    on_step_error: crate::protocol::PatternErrorPolicy::Continue,
                    steps: vec![PatternStepPlan {
                        tool: "echo".to_owned(),
                        args: json!({}),
                    }],
                }],
                seed: u64::from(index),
            },
            shard: AgentShard {
                agent_name: name.to_owned(),
                index,
                agent_count: 2,
                concurrency: 1,
            },
            heartbeat_interval_ms: 1_000,
        }
    }

    fn hello(_name: &str) -> WireFrame {
        WireFrame::new(WireMessage::Hello(HelloFrame {
            // Ephemeral SSH workers do not know this controller-local
            // inventory name until Prepare binds the channel.
            agent_name: None,
            binary_version: "0.2.0".to_owned(),
            scenarios: vec![SupportedScenario::Sustained, SupportedScenario::Pattern],
            max_concurrency: 100,
        }))
    }

    fn ready(name: &str, live: u32) -> WireFrame {
        WireFrame::new(WireMessage::Ready(ReadyFrame {
            job_id: "job-1".to_owned(),
            agent_name: name.to_owned(),
            live_workers: live,
            requested_workers: 1,
            tool_inventory_hash: "sha256:tools".to_owned(),
            target_protocol_version: "2025-11-25".to_owned(),
        }))
    }

    #[test]
    fn controller_releases_start_only_after_entire_cohort_ready() {
        let mut controller =
            ControllerJobState::new(vec![prepare("east", 0), prepare("west", 1)]).unwrap();
        for name in ["east", "west"] {
            let response = controller.accept(name, hello(name)).unwrap().unwrap();
            assert!(matches!(response.message, WireMessage::Prepare(_)));
        }

        controller.accept("east", ready("east", 1)).unwrap();
        assert!(!controller.all_ready());
        assert!(matches!(
            controller.start_all(500),
            Err(ControllerStateError::CohortNotReady)
        ));

        controller.accept("west", ready("west", 1)).unwrap();
        let starts = controller.start_all(500).unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(
            controller.phase("east"),
            Some(ControllerAgentPhase::Running)
        );
        assert_eq!(
            controller.phase("west"),
            Some(ControllerAgentPhase::Running)
        );
    }

    #[test]
    fn readiness_shortfall_fails_closed() {
        let mut controller = ControllerJobState::new(vec![prepare("east", 0)]).unwrap();
        controller.accept("east", hello("east")).unwrap();
        assert!(matches!(
            controller.accept("east", ready("east", 0)),
            Err(ControllerStateError::ReadinessShortfall { .. })
        ));
        assert_eq!(controller.phase("east"), Some(ControllerAgentPhase::Failed));
        assert!(controller.has_failure());
    }

    #[test]
    fn target_mismatch_fails_before_start() {
        let mut controller =
            ControllerJobState::new(vec![prepare("east", 0), prepare("west", 1)]).unwrap();
        for name in ["east", "west"] {
            controller.accept(name, hello(name)).unwrap();
        }
        controller.accept("east", ready("east", 1)).unwrap();
        let mut west = match ready("west", 1).message {
            WireMessage::Ready(frame) => frame,
            _ => unreachable!(),
        };
        west.target_protocol_version = "2026-07-28".to_owned();
        assert!(matches!(
            controller.accept("west", WireFrame::new(WireMessage::Ready(west))),
            Err(ControllerStateError::TargetProtocolMismatch { .. })
        ));
        assert!(!controller.all_ready());
    }

    #[test]
    fn agent_enforces_prepare_ready_start_order() {
        let greeting = HelloFrame {
            agent_name: None,
            binary_version: "0.2.0".to_owned(),
            scenarios: vec![SupportedScenario::Sustained],
            max_concurrency: 100,
        };
        let mut agent = AgentStateMachine::new(greeting);
        assert!(matches!(agent.hello().message, WireMessage::Hello(_)));
        assert!(matches!(
            agent.accept(WireFrame::new(WireMessage::Start(StartFrame {
                job_id: "job-1".to_owned(),
                start_after_ms: 100,
            }))),
            Err(AgentStateError::UnexpectedFrame { .. })
        ));
        assert!(matches!(
            agent
                .accept(WireFrame::new(WireMessage::Prepare(prepare("east", 0))))
                .unwrap(),
            AgentDirective::Prepare(_)
        ));
        let mut spoofed = match ready("east", 1).message {
            WireMessage::Ready(frame) => frame,
            _ => unreachable!(),
        };
        spoofed.agent_name = "west".to_owned();
        assert!(matches!(
            agent.mark_ready(spoofed),
            Err(AgentStateError::AgentMismatch { .. })
        ));
        agent
            .mark_ready(match ready("east", 1).message {
                WireMessage::Ready(frame) => frame,
                _ => unreachable!(),
            })
            .unwrap();
        assert!(matches!(
            agent
                .accept(WireFrame::new(WireMessage::Start(StartFrame {
                    job_id: "job-1".to_owned(),
                    start_after_ms: 100,
                })))
                .unwrap(),
            AgentDirective::Start(_)
        ));
        assert_eq!(agent.phase(), AgentPhase::Running);
    }
}
