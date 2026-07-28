//! `deadlock_probe` scenario — Vibe-Trading-bug-class detector.
//!
//! See DESIGN.md §15.2 for the reference algorithm. This scenario fires N
//! `tools/call` invocations and wraps each one with [`hang_detect`] to
//! classify it as success / hang / deadlock / error.
//!
//! For `concurrent > 1`, every invocation owns an independent session from
//! [`RunContext::session_factory`]. The shared pool start gate releases all
//! calls only after every live worker is ready, matching DESIGN §15.2's
//! concurrency requirement. A bare direct-library context without a factory
//! is rejected explicitly rather than silently serializing the probe.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::scenario::{
    RunContext, Scenario, ScenarioOutcome, classify_error, is_logical_tool_error, pool, teardown,
};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};

/// Probe a server for the deadlock bug class.
///
/// Issues `concurrent` synchronized tool calls against independent sessions
/// and classifies each via [`hang_detect`].
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
        if self.concurrent == 0 {
            return invalid_plan("concurrent must be >= 1");
        }
        if self.hang_threshold.is_zero() {
            return invalid_plan("hang_threshold must be > 0");
        }
        if self.concurrent == 1 {
            return drive_one(
                0,
                session,
                &self.tool,
                &self.args,
                self.hang_threshold,
                self.grace_period,
                ctx,
            )
            .await;
        }
        if ctx.session_factory.is_none() {
            return invalid_plan(
                "a session_factory is required for synchronized concurrent calls \
                 (Run::execute attaches one automatically)",
            );
        }

        let tool = Arc::new(self.tool.clone());
        let args = Arc::new(self.args.clone());
        let hang_threshold = self.hang_threshold;
        let grace_period = self.grace_period;
        pool::drive_pooled(
            ctx,
            self.concurrent,
            move |iter, mut session, worker_ctx| {
                let tool = Arc::clone(&tool);
                let args = Arc::clone(&args);
                async move {
                    let mut outcome = drive_one(
                        iter,
                        &mut session,
                        &tool,
                        &args,
                        hang_threshold,
                        grace_period,
                        &worker_ctx,
                    )
                    .await;
                    teardown::shutdown_session(
                        session,
                        &mut outcome,
                        format!("deadlock_probe worker {iter}"),
                    )
                    .await;
                    outcome
                }
            },
        )
        .await
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

fn invalid_plan(message: &str) -> ScenarioOutcome {
    ScenarioOutcome {
        error_count: 1,
        notes: vec![format!("deadlock_probe: invalid plan — {message}")],
        ..ScenarioOutcome::default()
    }
}

async fn drive_one(
    iter: u32,
    session: &mut Session,
    tool: &str,
    args: &Value,
    hang_threshold: Duration,
    grace_period: Duration,
    ctx: &RunContext,
) -> ScenarioOutcome {
    let mut outcome = ScenarioOutcome::default();
    if ctx.is_cancelled() {
        outcome.error_count = 1;
        ctx.metrics
            .record_tool(tool, Duration::ZERO, CallOutcome::Cancelled);
        outcome.notes.push(format!("cancelled before iter={iter}"));
        return outcome;
    }

    let hang_outcome =
        hang_detect(session.call_tool(tool, args), hang_threshold, grace_period).await;
    outcome.total_calls = 1;
    match hang_outcome {
        HangOutcome::Ok { result, duration } => {
            if is_logical_tool_error(&result) {
                outcome.error_count = 1;
                ctx.metrics
                    .record_tool(tool, duration, CallOutcome::ServerError);
            } else {
                outcome.successful_calls = 1;
                ctx.metrics
                    .record_tool(tool, duration, CallOutcome::Success);
            }
        }
        HangOutcome::Slow { result, duration } => {
            outcome.hang_count = 1;
            if is_logical_tool_error(&result) {
                outcome.error_count = 1;
                ctx.metrics
                    .record_tool(tool, duration, CallOutcome::ServerError);
            } else {
                ctx.metrics.record_tool(tool, duration, CallOutcome::Hang);
            }
            outcome.notes.push(format!(
                "slow response: tool={tool} iter={iter} took={}ms",
                duration.as_millis()
            ));
        }
        HangOutcome::Deadlock { hung_for } => {
            outcome.deadlock_count = 1;
            outcome.hung_for_ms.push(hung_for.as_millis());
            ctx.metrics
                .record_tool(tool, hung_for, CallOutcome::Deadlock);
            outcome.notes.push(format!(
                "deadlock detected: tool={tool} iter={iter} hung_for={}ms",
                hung_for.as_millis()
            ));
        }
        HangOutcome::Err(err) => {
            outcome.error_count = 1;
            ctx.metrics
                .record_tool(tool, Duration::ZERO, classify_error(&err));
            outcome
                .notes
                .push(format!("error: tool={tool} iter={iter} err={err}"));
        }
        other => {
            outcome.error_count = 1;
            outcome
                .notes
                .push(format!("unexpected hang outcome at iter={iter}: {other:?}"));
        }
    }
    outcome
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
