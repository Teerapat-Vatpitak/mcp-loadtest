//! `deadlock_probe` scenario — Vibe-Trading-bug-class detector.
//!
//! See DESIGN.md §15.2 for the reference algorithm. This scenario fires N
//! `tools/call` invocations and wraps each one with [`hang_detect`] to
//! classify it as success / hang / deadlock / error.
//!
//! # M2 limitation
//!
//! [`Session::call_tool`] takes `&mut self`, so a single session cannot drive
//! the N calls truly concurrently. The DESIGN.md §15.2 spec requires a
//! synchronization barrier across N **independent** sessions, which depends on
//! a session pool the orchestrator hasn't built yet (M3).
//!
//! For M2 we therefore issue N **sequential** calls against the single given
//! session and rely on `hang_detect` to classify each. This is enough to
//! catch the lazy-init pattern (the offending call still hangs forever on the
//! buggy server) — it just isn't the highest-pressure form of the test. The
//! `concurrent` knob is honored verbatim once the session pool lands.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};

/// Probe a server for the deadlock bug class.
///
/// Issues `concurrent` tool calls against `tool` (sequential in M2 — see
/// module-level docs) and classifies each via [`hang_detect`]. Reports back
/// a [`ScenarioOutcome`] tallying success / hang / deadlock / error counts.
pub struct DeadlockProbe {
    /// Number of `tools/call` invocations to issue.
    pub concurrent: u32,
    /// Per-call hang threshold (forwarded to [`hang_detect`]).
    pub hang_threshold: Duration,
    /// Grace period after the threshold before classifying as deadlock.
    pub grace_period: Duration,
    /// Tool to invoke.
    pub tool: String,
    /// Arguments passed to the tool on every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for DeadlockProbe {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();

        for iter in 0..self.concurrent {
            if ctx.is_cancelled() {
                outcome.notes.push(format!("cancelled before iter={iter}"));
                break;
            }

            let call_fut = session.call_tool(&self.tool, &self.args);
            let hang_outcome = hang_detect(call_fut, self.hang_threshold, self.grace_period).await;

            outcome.total_calls += 1;
            match hang_outcome {
                HangOutcome::Ok { duration, .. } => {
                    outcome.successful_calls += 1;
                    ctx.metrics
                        .record_tool(&self.tool, duration, CallOutcome::Success);
                }
                HangOutcome::Slow { duration, .. } => {
                    outcome.hang_count += 1;
                    ctx.metrics
                        .record_tool(&self.tool, duration, CallOutcome::Hang);
                    outcome.notes.push(format!(
                        "slow response: tool={} iter={} took={}ms",
                        self.tool,
                        iter,
                        duration.as_millis()
                    ));
                }
                HangOutcome::Deadlock { hung_for } => {
                    outcome.deadlock_count += 1;
                    outcome.hung_for_ms.push(hung_for.as_millis());
                    ctx.metrics
                        .record_tool(&self.tool, hung_for, CallOutcome::Deadlock);
                    outcome.notes.push(format!(
                        "deadlock detected: tool={} iter={} hung_for={}ms",
                        self.tool,
                        iter,
                        hung_for.as_millis()
                    ));
                    // After a deadlock the underlying session is wedged: the
                    // hung request still occupies stdin/stdout. Bail rather
                    // than try further calls that will also hang.
                    break;
                }
                HangOutcome::Err(e) => {
                    outcome.error_count += 1;
                    // Best-effort classification — a richer mapping lives in §18,
                    // but for M2 we collapse all transport/server errors into one bucket.
                    ctx.metrics
                        .record_tool(&self.tool, Duration::ZERO, CallOutcome::ServerError);
                    outcome
                        .notes
                        .push(format!("error: tool={} iter={} err={}", self.tool, iter, e));
                }
                // `HangOutcome` is `#[non_exhaustive]` (mcp-loadtest-protocol):
                // a cross-crate wildcard is mandatory. Only Ok/Slow/Deadlock/
                // Err exist today; count any future variant as an error so it
                // is never silently dropped from the outcome.
                other => {
                    outcome.error_count += 1;
                    outcome
                        .notes
                        .push(format!("unexpected hang outcome at iter={iter}: {other:?}"));
                }
            }
        }

        outcome
    }

    fn config_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 20,
                    "description": "Number of tool calls to issue."
                },
                "hang_threshold": {
                    "type": "string",
                    "default": "5s",
                    "description": "Per-call duration after which the call is considered hanging."
                },
                "grace_period": {
                    "type": "string",
                    "default": "10s",
                    "description": "Extra wait after hang_threshold before classifying as deadlock."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name to invoke (must be in tools/list)."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to the tool on every call."
                }
            },
            "required": ["tool"]
        })
    }

    fn name(&self) -> &'static str {
        "deadlock_probe"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let probe = DeadlockProbe {
            concurrent: 1,
            hang_threshold: Duration::from_millis(10),
            grace_period: Duration::from_millis(10),
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        };
        assert_eq!(probe.name(), "deadlock_probe");
    }

    #[test]
    fn config_schema_advertises_required_tool() {
        let probe = DeadlockProbe {
            concurrent: 1,
            hang_threshold: Duration::from_millis(10),
            grace_period: Duration::from_millis(10),
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        };
        let schema = probe.config_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v == "tool"));
    }
}
