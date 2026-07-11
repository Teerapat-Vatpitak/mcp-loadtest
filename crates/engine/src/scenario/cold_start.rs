//! `cold_start` scenario — measures the **spawn → `initialize` handshake**
//! plus the first `tools/call` on a brand-new server process.
//!
//! Real cold-start measurement (DESIGN.md §8, activated per §13.1 item 1)
//! needs a *fresh* server per iteration: re-using a warm session would skip
//! exactly the interpreter-startup / import / lazy-init costs the scenario
//! exists to measure. Each iteration therefore spawns its own session through
//! [`RunContext::session_factory`] and shuts it down (bounded) before the
//! next iteration.
//!
//! Per iteration:
//! 1. `factory.spawn()` — the elapsed time is the **handshake duration**,
//!    recorded under [`HANDSHAKE_METRIC`] so it gets its own row in the
//!    per-tool histogram report.
//! 2. One `tools/call` against `tool`, wrapped in [`hang_detect`] (same
//!    classification as `deadlock_probe`) and recorded under the real tool
//!    name.
//! 3. Bounded shutdown of that session. On timeout the child is still reaped
//!    via `kill_on_drop`.
//!
//! With `warmup = true` (default) iteration 0 **still runs** — paying the
//! one-time JIT / import / page-cache costs — but its samples are excluded
//! from metrics. The [`ScenarioOutcome`] counters stay honest and count every
//! iteration that actually ran.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::scenario::{RunContext, Scenario, ScenarioOutcome, classify_error, is_terminal_error};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};

/// Per-tool metric name under which the spawn → `initialize` handshake
/// duration is recorded. The `:` infix keeps it from colliding with any real
/// MCP tool name (tool names are `[a-zA-Z0-9_-]` in practice).
pub const HANDSHAKE_METRIC: &str = "cold_start:handshake";

/// Budget for the per-iteration session shutdown. Bounded so one wedged
/// server can't stall the remaining iterations; on timeout the `Session` is
/// dropped and the child is reaped via `kill_on_drop`.
const ITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Cold-start scenario: respawn a fresh server per iteration and measure the
/// handshake plus the first tool call. See module docs for the algorithm.
pub struct ColdStart {
    /// How many cold-start iterations to perform.
    pub iterations: u32,
    /// If `true` (default), iteration 0 runs but its samples are discarded
    /// from metrics (JIT / import warm-up).
    pub warmup: bool,
    /// Tool to invoke once per fresh session.
    pub tool: String,
    /// Arguments passed to the tool on every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for ColdStart {
    async fn drive(&self, _session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        // `_session` is intentionally unused: the orchestrator spawned it
        // *before* `drive` was called, so its handshake already happened —
        // anything measured on it would be a warm measurement. Cold-start
        // spawns its own fresh process per iteration via the factory;
        // `Run::execute` shuts the original session down as usual afterwards.
        let mut outcome = ScenarioOutcome::default();

        let Some(factory) = ctx.session_factory.clone() else {
            // Direct-library callers that built a bare RunContext get a
            // no-op with an honest note instead of a panic.
            outcome.notes.push(
                "cold_start: skipped — RunContext has no session_factory (attach one via \
                 RunContext::with_session_factory; Run::execute does this automatically)"
                    .to_owned(),
            );
            return outcome;
        };

        if self.warmup && self.iterations > 0 {
            outcome.notes.push(
                "warmup=true: iteration 0 runs but its samples are excluded from metrics"
                    .to_owned(),
            );
        }

        for iter in 0..self.iterations {
            if ctx.is_cancelled() {
                outcome.notes.push(format!("cancelled before iter={iter}"));
                break;
            }
            let discard = self.warmup && iter == 0;

            // Phase 1: spawn + initialize — the cold-start measurement.
            // Raced against cancellation so a hung handshake can't pin the
            // run past shutdown (dropping the spawn future kills any
            // already-spawned child via kill_on_drop).
            let spawn_start = Instant::now();
            let spawned = tokio::select! {
                biased;
                _ = ctx.cancel_token.cancelled() => {
                    outcome.notes.push(format!("cancelled during spawn iter={iter}"));
                    break;
                }
                res = factory.spawn() => res,
            };
            let handshake = spawn_start.elapsed();

            let mut session = match spawned {
                Ok(session) => session,
                Err(e) => {
                    outcome.error_count += 1;
                    if !discard {
                        ctx.metrics
                            .record_tool(HANDSHAKE_METRIC, handshake, classify_error(&e));
                    }
                    outcome
                        .notes
                        .push(format!("spawn failed: iter={iter} err={e}"));
                    if is_terminal_error(&e) {
                        outcome
                            .notes
                            .push(format!("terminal spawn error — stopping at iter={iter}"));
                        break;
                    }
                    continue;
                }
            };
            if !discard {
                ctx.metrics
                    .record_tool(HANDSHAKE_METRIC, handshake, CallOutcome::Success);
            }

            // Phase 2: first tools/call on the cold session, hang-classified
            // exactly like deadlock_probe.
            outcome.total_calls += 1;
            let call_fut = session.call_tool(&self.tool, &self.args);
            let hang_outcome = hang_detect(call_fut, ctx.hang_threshold, ctx.grace_period).await;
            let mut terminal = false;
            match hang_outcome {
                HangOutcome::Ok { duration, .. } => {
                    outcome.successful_calls += 1;
                    if !discard {
                        ctx.metrics
                            .record_tool(&self.tool, duration, CallOutcome::Success);
                    }
                }
                HangOutcome::Slow { duration, .. } => {
                    outcome.hang_count += 1;
                    if !discard {
                        ctx.metrics
                            .record_tool(&self.tool, duration, CallOutcome::Hang);
                    }
                    outcome.notes.push(format!(
                        "slow first call: tool={} iter={iter} took={}ms",
                        self.tool,
                        duration.as_millis()
                    ));
                }
                HangOutcome::Deadlock { hung_for } => {
                    outcome.deadlock_count += 1;
                    outcome.hung_for_ms.push(hung_for.as_millis());
                    if !discard {
                        ctx.metrics
                            .record_tool(&self.tool, hung_for, CallOutcome::Deadlock);
                    }
                    outcome.notes.push(format!(
                        "deadlock on first call: tool={} iter={iter} hung_for={}ms",
                        self.tool,
                        hung_for.as_millis()
                    ));
                    // Unlike deadlock_probe we do NOT break: the wedged
                    // session is discarded below and the next iteration gets
                    // a fresh process — exactly the lazy-init reproduction
                    // this scenario exists for.
                }
                HangOutcome::Err(e) => {
                    outcome.error_count += 1;
                    if !discard {
                        ctx.metrics
                            .record_tool(&self.tool, Duration::ZERO, classify_error(&e));
                    }
                    outcome
                        .notes
                        .push(format!("error: tool={} iter={iter} err={e}", self.tool));
                    terminal = is_terminal_error(&e);
                }
                // `HangOutcome` is `#[non_exhaustive]` (mcp-loadtest-protocol):
                // a cross-crate wildcard is mandatory. Only Ok/Slow/Deadlock/
                // Err exist today; count any future variant as an error so it
                // is never silently dropped from the outcome.
                other => {
                    outcome.error_count += 1;
                    outcome.notes.push(format!(
                        "unexpected hang outcome: tool={} iter={iter}: {other:?}",
                        self.tool
                    ));
                }
            }

            // Phase 3: bounded shutdown so child processes never pile up
            // across iterations. On timeout the Session (and transport) is
            // dropped — the child is kill_on_drop, so it is reaped anyway.
            match tokio::time::timeout(ITER_SHUTDOWN_TIMEOUT, session.shutdown()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(iter, error = %e, "cold_start: per-iteration shutdown errored");
                }
                Err(_) => {
                    tracing::warn!(
                        iter,
                        "cold_start: per-iteration shutdown exceeded {ITER_SHUTDOWN_TIMEOUT:?}; \
                         child reaped via kill_on_drop"
                    );
                }
            }

            if terminal {
                outcome
                    .notes
                    .push(format!("terminal call error — stopping at iter={iter}"));
                break;
            }
        }

        outcome
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "ColdStart",
            "description": "Respawn a fresh server per iteration; measure spawn → initialize handshake time plus the first tools/call.",
            "properties": {
                "iterations": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 5,
                    "description": "How many cold-start iterations to perform."
                },
                "warmup": {
                    "type": "boolean",
                    "default": true,
                    "description": "Run iteration 0 but discard its samples as JIT/import warm-up."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool to invoke once per fresh session (must be in tools/list)."
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
        "cold_start"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario() -> ColdStart {
        ColdStart {
            iterations: 3,
            warmup: true,
            tool: "echo".to_string(),
            args: json!({}),
        }
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(scenario().name(), "cold_start");
    }

    #[test]
    fn config_schema_requires_tool_only() {
        let schema = scenario().config_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        // iterations/warmup/args all have builder defaults; only `tool` is
        // mandatory (mirrors cmd_run/builder.rs).
        assert_eq!(required.len(), 1);
        assert!(required.iter().any(|v| v == "tool"));
    }

    #[test]
    fn handshake_metric_name_is_pinned() {
        // Reports and downstream tooling key on this exact string.
        assert_eq!(HANDSHAKE_METRIC, "cold_start:handshake");
    }
}
