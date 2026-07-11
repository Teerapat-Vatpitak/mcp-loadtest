//! `spike` scenario — baseline load, sudden burst to peak, then return to baseline.
//!
//! See DESIGN.md §8 (the `spike` row). Complementary to [`crate::scenario::ramp`]:
//! ramp is a gradual linear concurrency increase; spike is the opposite — a
//! sustained low baseline with a sharp burst in the middle, modelling the
//! classic "Black Friday traffic spike" pattern.
//!
//! ```text
//! concurrency
//!   ^
//!   |          +--------+
//!   |          |        |
//!   |__________|        |__________
//!   0----time------------------------>
//!        warmup   spike    cooldown
//! ```
//!
//! # M8: real concurrency via a session pool
//!
//! When the [`RunContext`] carries a session factory (always true under
//! `Run::execute`), each phase drives its declared concurrency as a real
//! pool via `crate::scenario::pool::drive_pooled`: fresh sessions, one
//! worker task per session, looping calls until the phase deadline. The
//! burst phase is the big worker count — and because the pool joins **every**
//! worker before returning, phase boundaries are clean: no burst worker can
//! bleed into cooldown. All phases go pooled (even baseline=1) so
//! phase-to-phase measurements stay comparable; sessions are respawned per
//! phase and that spawn cost counts against the phase duration (disclosed in
//! the notes). See `spike/pooled.rs` and ADR 0017.
//!
//! # Sequential fallback (no session factory)
//!
//! With a bare [`RunContext::new`] (direct library use, tests) the M5
//! behavior is preserved: a single `&mut Session`, where concurrent calls
//! would serialize on the `&mut` borrow, so "baseline" and "peak"
//! concurrency degrade to **iterations-per-phase** budgets rather than true
//! parallelism: at peak concurrency `N` we issue up to `N` sequential calls
//! per inner tick during the spike window. The shape of the resulting
//! per-call timeseries still captures the spike pattern (latency / error
//! rate climbs during the spike window); the outcome notes disclose the
//! degradation.
//!
//! # Algorithm
//!
//! Three back-to-back phases (pooled: each against its own pool of fresh
//! sessions; sequential: all against the same session):
//!
//! 1. **warmup** — `baseline_concurrent`, for `warmup`.
//! 2. **spike**  — `spike_concurrent`, for `spike_duration`.
//! 3. **cooldown** — back to `baseline_concurrent`, for `cooldown`.
//!
//! Each phase appends one summary note to `ScenarioOutcome::notes` so the
//! report can call out which phase the metrics belong to. A final
//! `spike: peak=… warmup=…ms spike=…ms cooldown=…ms` line summarises the
//! plan that was driven.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_protocol::Session;

mod phase;
mod pooled;
use phase::drive_phase;

/// Spike scenario. See module docs for the algorithm and limitations.
pub struct Spike {
    /// Baseline concurrent worker count during warmup + cooldown. Must be at
    /// least 1.
    pub baseline_concurrent: u32,
    /// Peak concurrent worker count during the spike window. Should be at
    /// least `baseline_concurrent` to actually be a spike; smaller values
    /// still run but emit a note pointing out the inverted shape.
    pub spike_concurrent: u32,
    /// How long to run baseline before the spike fires.
    pub warmup: Duration,
    /// How long the spike holds at peak.
    pub spike_duration: Duration,
    /// How long to run baseline AFTER the spike (cooldown).
    pub cooldown: Duration,
    /// Tool to invoke on every call.
    pub tool: String,
    /// JSON args passed to every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for Spike {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();

        if self.baseline_concurrent == 0 || self.spike_concurrent == 0 {
            outcome.notes.push(format!(
                "spike: invalid plan baseline={} peak={}; concurrency must be >= 1",
                self.baseline_concurrent, self.spike_concurrent
            ));
            return outcome;
        }

        if self.spike_concurrent < self.baseline_concurrent {
            outcome.notes.push(format!(
                "spike: peak ({}) < baseline ({}) — running anyway, but the \
                 shape is inverted from the canonical Black-Friday pattern",
                self.spike_concurrent, self.baseline_concurrent,
            ));
        }

        if ctx.session_factory.is_some() {
            // Pooled path (M8): each phase drives its concurrency as a real
            // pool of fresh sessions, all workers joined before the next
            // phase starts. The borrowed `session` stays idle — it can't
            // move into worker tasks (they need owned, `'static` sessions);
            // Run::execute shuts it down as usual afterwards.
            return pooled::drive_pooled_phases(self, ctx, outcome).await;
        }

        outcome.notes.push(
            "spike: sequential on one session; baseline/peak concurrency is \
             recorded as iterations-per-tick, not multiplexed (pooled execution \
             needs a session_factory on the RunContext; Run::execute attaches \
             one automatically — see module docs)"
                .to_string(),
        );

        // Phase 1: warmup at baseline.
        let warmup_terminal = drive_phase(
            session,
            ctx,
            &mut outcome,
            "warmup",
            self.baseline_concurrent,
            self.warmup,
            &self.tool,
            &self.args,
        )
        .await;
        if warmup_terminal || ctx.is_cancelled() {
            push_summary(&mut outcome, self);
            return outcome;
        }

        // Phase 2: the spike itself.
        let spike_terminal = drive_phase(
            session,
            ctx,
            &mut outcome,
            "spike",
            self.spike_concurrent,
            self.spike_duration,
            &self.tool,
            &self.args,
        )
        .await;
        if spike_terminal || ctx.is_cancelled() {
            push_summary(&mut outcome, self);
            return outcome;
        }

        // Phase 3: cooldown back to baseline.
        let _ = drive_phase(
            session,
            ctx,
            &mut outcome,
            "cooldown",
            self.baseline_concurrent,
            self.cooldown,
            &self.tool,
            &self.args,
        )
        .await;

        push_summary(&mut outcome, self);
        outcome
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "Spike",
            "description": "Baseline load, sudden burst to peak, then return to baseline — the canonical Black-Friday traffic shape. Each phase is a real session pool when the run provides a session factory (Run::execute always does); sequential iterations-per-tick fallback otherwise.",
            "properties": {
                "baseline_concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Concurrency during warmup and cooldown."
                },
                "spike_concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Concurrency during the spike window."
                },
                "warmup": {
                    "type": "string",
                    "description": "Baseline duration before the spike fires (humantime, e.g. \"30s\")."
                },
                "spike_duration": {
                    "type": "string",
                    "description": "How long to hold the spike at peak concurrency (humantime)."
                },
                "cooldown": {
                    "type": "string",
                    "description": "Baseline duration AFTER the spike (humantime)."
                },
                "tool": {
                    "type": "string",
                    "description": "MCP tool name to invoke on every iteration."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to `tool`."
                }
            },
            "required": [
                "baseline_concurrent",
                "spike_concurrent",
                "warmup",
                "spike_duration",
                "cooldown",
                "tool",
                "args"
            ]
        })
    }

    fn name(&self) -> &'static str {
        "spike"
    }
}

/// Append the final plan summary line to the outcome.
fn push_summary(outcome: &mut ScenarioOutcome, spike: &Spike) {
    outcome.notes.push(format!(
        "spike: peak={} warmup={}ms spike={}ms cooldown={}ms",
        spike.spike_concurrent,
        spike.warmup.as_millis(),
        spike.spike_duration.as_millis(),
        spike.cooldown.as_millis(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let s = Spike {
            baseline_concurrent: 1,
            spike_concurrent: 4,
            warmup: Duration::from_millis(100),
            spike_duration: Duration::from_millis(100),
            cooldown: Duration::from_millis(100),
            tool: "echo".to_string(),
            args: json!({}),
        };
        assert_eq!(s.name(), "spike");
    }

    #[test]
    fn config_schema_lists_required_fields() {
        let s = Spike {
            baseline_concurrent: 1,
            spike_concurrent: 4,
            warmup: Duration::from_millis(100),
            spike_duration: Duration::from_millis(100),
            cooldown: Duration::from_millis(100),
            tool: "echo".to_string(),
            args: json!({}),
        };
        let schema = s.config_schema();
        let req = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required");
        assert!(req.iter().any(|v| v == "baseline_concurrent"));
        assert!(req.iter().any(|v| v == "spike_concurrent"));
        assert!(req.iter().any(|v| v == "warmup"));
        assert!(req.iter().any(|v| v == "spike_duration"));
        assert!(req.iter().any(|v| v == "cooldown"));
        assert!(req.iter().any(|v| v == "tool"));
    }
}
