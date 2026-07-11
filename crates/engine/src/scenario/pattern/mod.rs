//! Multi-step weighted-random tool-call patterns with think-time.
//!
//! See DESIGN.md §21 for the conceptual model. A `Pattern` describes a
//! reusable sequence of [`PatternStep`]s plus the cadence and error policy
//! the executor should apply when running it. Scenarios that drive
//! continuous load (e.g. [`crate::scenario::sustained::Sustained`]) can hold
//! a `Vec<Pattern>` and pick one per iteration via weighted random selection.
//!
//! # M5 scope
//!
//! - Multiple steps per pattern, each with their own `tool` + `args`.
//! - Per-pattern `weight` for weighted-random selection.
//! - Per-pattern `think_time` slept between consecutive steps.
//! - Per-pattern [`ErrorBehavior`] — `Continue` records and proceeds,
//!   `Abort` short-circuits the rest of this iteration.
//!
//! Template variables (using a previous step's output as the next step's
//! input) are deferred to **M6**.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use mcp_loadtest_protocol::Session;

use crate::scenario::{RunContext, Scenario, ScenarioOutcome};

mod steps;
pub use steps::{StepStats, execute, pick};

/// What to do when a pattern step errors out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorBehavior {
    /// Record the error, run the next step.
    #[default]
    Continue,
    /// Stop running this pattern; advance to the next iteration of the loop.
    Abort,
}

/// One step within a [`Pattern`] — a single `tools/call` invocation.
///
/// Template variable interpolation (e.g. using `step[0].response.id` as the
/// `args` for `step[1]`) is deferred to M6. For now, `args` is rendered
/// verbatim per call.
#[derive(Debug, Clone)]
pub struct PatternStep {
    /// Tool name to invoke.
    pub tool: String,
    /// Arguments JSON for `tool`.
    pub args: Value,
}

/// A reusable named sequence of [`PatternStep`]s.
///
/// Drive a `Pattern` with [`execute`]. To pick one Pattern from a slice by
/// `weight` use [`pick`].
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Human-readable identifier (used in logs, reports).
    pub name: String,
    /// Relative selection weight; passed to [`pick`]. Negative or zero weights
    /// are treated as never-selected.
    pub weight: f64,
    /// Sleep between consecutive steps in this pattern. `Duration::ZERO`
    /// disables it.
    pub think_time: Duration,
    /// What to do if any step in the pattern errors.
    pub on_step_error: ErrorBehavior,
    /// Steps run in order.
    pub steps: Vec<PatternStep>,
}

/// Scenario wrapper around the pattern engine.
///
/// `Pattern` itself is just a reusable sequence of tool calls. This wrapper
/// gives TOML/CLI users a first-class `scenario.type = "pattern"` entry point
/// and also lets `sustained` configs with multiple weighted tool calls reuse
/// the same executor without changing the legacy [`Sustained`] struct.
///
/// [`Sustained`]: crate::scenario::sustained::Sustained
pub struct PatternScenario {
    /// Name surfaced in reports. Use `"pattern"` for explicit pattern runs
    /// and `"sustained"` when adapting legacy sustained configs with
    /// `[[scenario.tool_call]]` / `patterns`.
    pub scenario_name: &'static str,
    /// Declared concurrency target. See `sustained` module docs: currently
    /// informational because a single `Session` serializes calls.
    pub concurrent: u32,
    /// Total time to keep selecting and driving patterns.
    pub duration: Duration,
    /// Weighted pattern set.
    pub patterns: Vec<Pattern>,
}

impl PatternScenario {
    /// Construct an explicit `pattern` scenario.
    pub fn new(concurrent: u32, duration: Duration, patterns: Vec<Pattern>) -> Self {
        Self {
            scenario_name: "pattern",
            concurrent,
            duration,
            patterns,
        }
    }

    /// Adapt a sustained workload that uses weighted patterns/tool calls while
    /// keeping report output labelled as `sustained`.
    pub fn sustained(concurrent: u32, duration: Duration, patterns: Vec<Pattern>) -> Self {
        Self {
            scenario_name: "sustained",
            concurrent,
            duration,
            patterns,
        }
    }
}

#[async_trait]
impl Scenario for PatternScenario {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        crate::scenario::sustained::run_patterns(
            self.concurrent,
            self.duration,
            &self.patterns,
            session,
            ctx,
        )
        .await
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "Pattern",
            "description": "Weighted random multi-step tool-call patterns.",
            "properties": {
                "concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 10
                },
                "duration": {
                    "type": "string",
                    "default": "60s"
                },
                "patterns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "weight": { "type": "number", "default": 1.0 },
                            "think_time": { "type": "string", "default": "0ms" },
                            "on_step_error": {
                                "type": "string",
                                "enum": ["continue", "abort"],
                                "default": "continue"
                            },
                            "steps": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "tool": { "type": "string" },
                                        "args": { "type": "object" }
                                    },
                                    "required": ["tool"]
                                }
                            }
                        },
                        "required": ["steps"]
                    }
                }
            },
            "required": ["patterns"]
        })
    }

    fn name(&self) -> &'static str {
        self.scenario_name
    }
}

impl Pattern {
    /// Convenience: a single-step pattern with weight 1.0 and no think-time.
    /// Useful for the legacy single-tool / single-args [`Sustained`] config
    /// (it folds into a one-element pattern list under the covers).
    ///
    /// [`Sustained`]: crate::scenario::sustained::Sustained
    pub fn single_call(tool: impl Into<String>, args: Value) -> Self {
        let tool = tool.into();
        Self {
            name: format!("single:{tool}"),
            weight: 1.0,
            think_time: Duration::ZERO,
            on_step_error: ErrorBehavior::Continue,
            steps: vec![PatternStep { tool, args }],
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn single_call_helper_creates_one_step_pattern() {
        let p = Pattern::single_call("echo", json!({"x": 1}));
        assert_eq!(p.steps.len(), 1);
        assert_eq!(p.steps[0].tool, "echo");
        assert_eq!(p.steps[0].args, json!({"x": 1}));
        assert!((p.weight - 1.0).abs() < f64::EPSILON);
        assert_eq!(p.think_time, Duration::ZERO);
        assert_eq!(p.on_step_error, ErrorBehavior::Continue);
        assert_eq!(p.name, "single:echo");
    }

    #[test]
    fn pattern_scenario_name_can_preserve_sustained_label() {
        let s = PatternScenario::sustained(
            1,
            Duration::from_secs(1),
            vec![Pattern::single_call("echo", json!({}))],
        );
        assert_eq!(s.name(), "sustained");

        let explicit = PatternScenario::new(
            1,
            Duration::from_secs(1),
            vec![Pattern::single_call("echo", json!({}))],
        );
        assert_eq!(explicit.name(), "pattern");
    }
}
