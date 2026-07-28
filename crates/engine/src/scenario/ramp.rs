//! `ramp` scenario — step concurrency from `from` to `to`, optionally
//! detecting the breaking point along the way.
//!
//! See DESIGN.md §8 (the `ramp` row) and the `analysis::breaking_point` module
//! for the auto-detection helper.
//!
//! # M8: real concurrency via a session pool
//!
//! When the [`RunContext`] carries a session factory (always true under
//! `Run::execute`), **every** step drives its declared level as a real pool:
//! `concurrent` fresh sessions spawned via
//! `crate::scenario::pool::drive_pooled`, one worker task per session,
//! every handle joined before the step ends. Worker loops run until the step
//! deadline, so level `N` is a true N-in-flight load. All steps go pooled
//! (even level 1) so per-step measurements stay apples-to-apples — a mixed
//! warm-borrowed-session step would skew the breaking-point comparison.
//! Sessions are respawned per step (the pool helper spawns/tears down per
//! invocation); that spawn cost counts against `step_duration` and is
//! disclosed in the notes. See `ramp/pooled.rs` and ADR 0017.
//!
//! # Sequential fallback (no session factory)
//!
//! With a bare [`RunContext::new`] (direct library use, tests) the M5
//! behavior is preserved: a single `&mut Session`, where concurrent calls
//! would serialize on the `&mut` borrow, so "concurrent" degrades to
//! **iterations per step** rather than true parallelism — at concurrency `N`
//! we drive `N` sequential calls during the step. Still useful for
//! breaking-point regressions because per-call latency grows naturally as
//! the server's per-process workload grows; the outcome notes disclose the
//! degradation.
//!
//! # Algorithm
//!
//! For each `concurrent` in `from..=to` stepping by `step_increment`:
//!
//! - Drive the step (pooled: `concurrent` workers until `step_duration`
//!   elapses; sequential: up to `concurrent` calls within `step_duration`),
//!   recording every call via `ctx.metrics`.
//! - After the step, if `breaking_point` is configured, snapshot the
//!   side-channel per-step `Recorder` — on both paths a fresh recorder per
//!   step, fed only that step's calls — feed it to a
//!   [`BreakingPointDetector`], and break out of the ramp early if a break
//!   point was detected.
//!
//! Per-step metrics fed to the detector are the **delta** since the previous
//! step — a fresh `Recorder` per step so the detector sees one step's data,
//! not cumulative numbers.
//!
//! Finally, append a summary line to `ScenarioOutcome::notes` describing the
//! detected break (or "completed without break" otherwise).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::yield_now;
use tokio::time::sleep;

use crate::breaking_point::{BreakingPointConfig, BreakingPointDetector};
use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_core::metrics::{CallOutcome, Recorder};
use mcp_loadtest_protocol::Session;

mod pooled;

/// Linear-stepped ramp. See module docs for the algorithm and limitations.
pub struct Ramp {
    /// Starting concurrency (inclusive). Must be >= 1.
    pub from_concurrent: u32,
    /// Ending concurrency (inclusive). Must be >= `from_concurrent`.
    pub to_concurrent: u32,
    /// Wall-clock budget per concurrency level. Acts as a hard cap on the
    /// per-step driving loop; if the step finishes its iteration budget
    /// before the duration elapses we still wait out the remaining time
    /// to keep step boundaries aligned in trace output.
    pub step_duration: Duration,
    /// How much to bump `concurrent` between steps (>= 1).
    pub step_increment: u32,
    /// MCP tool to invoke on every iteration.
    pub tool: String,
    /// Arguments JSON for the tool.
    pub args: Value,
    /// Optional breaking-point detector configuration. When set, a
    /// per-step [`BreakingPointDetector`] watches the metrics and aborts
    /// the ramp once a violation is observed.
    pub breaking_point: Option<BreakingPointConfig>,
}

#[async_trait]
impl Scenario for Ramp {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();

        if self.from_concurrent == 0
            || self.to_concurrent < self.from_concurrent
            || self.step_increment == 0
        {
            outcome.notes.push(format!(
                "ramp: invalid step plan from={} to={} step={}; nothing to drive",
                self.from_concurrent, self.to_concurrent, self.step_increment
            ));
            return outcome;
        }

        if ctx.session_factory.is_some() {
            // Pooled path (M8): every step drives its level as a real pool
            // of fresh sessions. The borrowed `session` stays idle — it
            // can't move into worker tasks (they need owned, `'static`
            // sessions); Run::execute shuts it down as usual afterwards.
            return pooled::drive_ramp_pooled(self, ctx).await;
        }

        let mut detector = self.breaking_point.clone().map(BreakingPointDetector::new);

        outcome.notes.push(
            "ramp: sequential on one session; concurrent values are recorded as \
             iterations-per-step, not multiplexed (pooled execution needs a \
             session_factory on the RunContext; Run::execute attaches one \
             automatically — see module docs)"
                .to_string(),
        );

        let mut concurrent = self.from_concurrent;
        let mut early_break = false;

        while concurrent <= self.to_concurrent {
            if ctx.is_cancelled() {
                outcome
                    .notes
                    .push("ramp: cancelled via ctx.cancel_token".to_owned());
                break;
            }

            // Per-step recorder so the detector sees only this step's data,
            // not cumulative numbers. The shared `ctx.metrics` still gets
            // every call recorded for the final report.
            let step_recorder = Recorder::new();
            let step_started = Instant::now();
            let step_deadline = step_started + self.step_duration;

            let mut step_iters: u32 = 0;
            let mut terminal = false;
            while step_iters < concurrent && Instant::now() < step_deadline {
                if ctx.is_cancelled() {
                    break;
                }

                let call_start = Instant::now();
                let call_fut = session.call_tool(&self.tool, &self.args);
                let result = tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => {
                        let elapsed = call_start.elapsed();
                        ctx.metrics.record_tool(&self.tool, elapsed, CallOutcome::Cancelled);
                        step_recorder.record(elapsed, CallOutcome::Cancelled);
                        outcome.error_count += 1;
                        outcome.total_calls += 1;
                        terminal = true;
                        break;
                    }
                    r = call_fut => r,
                };
                let elapsed = call_start.elapsed();
                outcome.total_calls += 1;
                step_iters += 1;

                match result {
                    Ok(result) => {
                        let kind = if super::is_logical_tool_error(&result) {
                            outcome.error_count += 1;
                            CallOutcome::ServerError
                        } else {
                            outcome.successful_calls += 1;
                            CallOutcome::Success
                        };
                        ctx.metrics.record_tool(&self.tool, elapsed, kind);
                        step_recorder.record(elapsed, kind);
                    }
                    Err(err) => {
                        outcome.error_count += 1;
                        let kind = super::classify_error(&err);
                        ctx.metrics.record_tool(&self.tool, elapsed, kind);
                        step_recorder.record(elapsed, kind);
                        if super::is_terminal_error(&err) {
                            outcome.notes.push(format!(
                                "ramp: terminal error at concurrent={concurrent}: {err}"
                            ));
                            terminal = true;
                            break;
                        }
                    }
                }

                // Yield so cancellation can preempt fast servers.
                yield_now().await;
            }

            // Hold the rest of the step duration so per-step boundaries
            // line up with wall-clock plans (matches the DESIGN intent of
            // a "linear ramp over duration"). Skip if cancellation fired
            // or we hit a terminal error.
            if !terminal && !ctx.is_cancelled() {
                let remaining = step_deadline.saturating_duration_since(Instant::now());
                if remaining > Duration::ZERO {
                    tokio::select! {
                        biased;
                        _ = ctx.cancel_token.cancelled() => {}
                        _ = sleep(remaining) => {}
                    }
                }
            }

            // Hand the step's metrics to the detector (if configured) and
            // see if we should bail.
            if let Some(det) = detector.as_mut() {
                det.observe(concurrent, step_recorder.snapshot());
                let report = det.breaking_point();
                if let Some(broke_at) = report.broke_at_concurrent {
                    let trigger = report
                        .trigger
                        .unwrap_or_else(|| "unknown trigger".to_owned());
                    outcome.notes.push(format!(
                        "ramp: breaking point detected at concurrent={broke_at}; \
                         last_known_good={:?}; {trigger}",
                        report.last_known_good
                    ));
                    early_break = true;
                    break;
                }
            }

            if terminal {
                break;
            }

            concurrent = concurrent.saturating_add(self.step_increment);
        }

        if !early_break && detector.is_some() {
            outcome
                .notes
                .push("ramp: completed full ramp without breaking point".to_owned());
        }

        outcome
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "Ramp",
            "description": "Step concurrency linearly from `from` to `to`, optionally detecting the breaking point. Each step is a real session pool when the run provides a session factory (Run::execute always does); sequential iterations-per-step fallback otherwise.",
            "properties": {
                "from_concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Starting concurrency level (inclusive)."
                },
                "to_concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Ending concurrency level (inclusive)."
                },
                "step_duration": {
                    "type": "string",
                    "description": "Wall-clock time to hold each concurrency level (humantime, e.g. \"5s\")."
                },
                "step_increment": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "Concurrency increase per step."
                },
                "tool": {
                    "type": "string",
                    "description": "MCP tool name to invoke on every iteration."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to `tool`."
                },
                "breaking_point": {
                    "type": "object",
                    "description": "Optional auto-stop when latency or error-rate budget is exceeded.",
                    "properties": {
                        "max_p99_latency": { "type": "string" },
                        "max_error_rate": { "type": "number" },
                        "window_secs": { "type": "number" }
                    }
                }
            },
            "required": ["from_concurrent", "to_concurrent", "step_duration", "tool", "args"]
        })
    }

    fn name(&self) -> &'static str {
        "ramp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let s = Ramp {
            from_concurrent: 1,
            to_concurrent: 5,
            step_duration: Duration::from_millis(100),
            step_increment: 1,
            tool: "echo".to_string(),
            args: json!({}),
            breaking_point: None,
        };
        assert_eq!(s.name(), "ramp");
    }

    #[test]
    fn config_schema_lists_required_fields() {
        let s = Ramp {
            from_concurrent: 1,
            to_concurrent: 5,
            step_duration: Duration::from_millis(100),
            step_increment: 1,
            tool: "echo".to_string(),
            args: json!({}),
            breaking_point: None,
        };
        let schema = s.config_schema();
        let req = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required");
        assert!(req.iter().any(|v| v == "from_concurrent"));
        assert!(req.iter().any(|v| v == "to_concurrent"));
        assert!(req.iter().any(|v| v == "tool"));
    }
}
