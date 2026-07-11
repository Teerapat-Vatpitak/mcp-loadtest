//! `race_check` scenario — non-determinism / race detector.
//!
//! Issues `concurrent` identical `tools/call` invocations and feeds the
//! responses to [`crate::race_detector::analyze`]. If two or more
//! responses canonicalize to distinct strings the scenario flags a divergence:
//! the tool is non-deterministic under the same inputs.
//!
//! # M6 limitation: sequential, not concurrent
//!
//! [`Session::call_tool`] takes `&mut self`, so a single session cannot drive
//! the N calls in parallel. M6 therefore issues them **sequentially** against
//! one session. That reduces the scenario to a non-determinism detector —
//! still valuable: a tool that returns `{"now": <wall-clock>}` will diverge
//! even when called serially, and we surface that.
//!
//! True N-way concurrent firing requires the multi-session pool that lands
//! in M7+. The detector itself is concurrency-agnostic and works either way.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::race_detector::{DivergenceReport, analyze};
use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::mcp::{CallToolResult, Content};

/// Fire N identical tool calls and check for divergent responses.
pub struct RaceCheck {
    /// How many sequential calls to issue against `tool`. (Named `concurrent`
    /// to match the broader scenario vocabulary — see module doc for why
    /// it's actually sequential in M6.)
    pub concurrent: u32,
    /// Tool to invoke.
    pub tool: String,
    /// Arguments passed verbatim on every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for RaceCheck {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();
        let mut responses: Vec<Value> = Vec::with_capacity(self.concurrent as usize);

        for iter in 0..self.concurrent {
            if ctx.is_cancelled() {
                outcome.notes.push(format!("cancelled before iter={iter}"));
                break;
            }

            let started = Instant::now();
            let result = session.call_tool(&self.tool, &self.args).await;
            let duration = started.elapsed();

            outcome.total_calls += 1;
            match result {
                Ok(tool_result) => {
                    outcome.successful_calls += 1;
                    ctx.metrics
                        .record_tool(&self.tool, duration, CallOutcome::Success);
                    responses.push(call_tool_result_to_value(&tool_result));
                }
                Err(e) => {
                    outcome.error_count += 1;
                    ctx.metrics
                        .record_tool(&self.tool, Duration::ZERO, CallOutcome::ServerError);
                    outcome
                        .notes
                        .push(format!("error: tool={} iter={} err={}", self.tool, iter, e));
                }
            }
        }

        // Run the detector on whatever responses we managed to collect.
        let report = analyze(&responses);
        if report.diverged {
            push_divergence_notes(&mut outcome, &self.tool, &report);
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
                    "default": 10,
                    "description": "Number of identical tool calls to issue. M6 issues them sequentially (single-session limit); the detector flags divergence either way."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name to invoke (must be in tools/list)."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed identically to every call."
                }
            },
            "required": ["tool"]
        })
    }

    fn name(&self) -> &'static str {
        "race_check"
    }
}

/// Manually convert a [`CallToolResult`] to a `Value` — the type only derives
/// `Deserialize`, so we build the JSON shape by hand. We preserve the exact
/// public fields (`content`, `isError`) so the canonicalizer sees the same
/// shape the server actually returned.
fn call_tool_result_to_value(result: &CallToolResult) -> Value {
    let content: Vec<Value> = result.content.iter().map(content_to_value).collect();
    serde_json::json!({
        "content": content,
        "isError": result.is_error,
    })
}

fn content_to_value(content: &Content) -> Value {
    match content {
        Content::Text { text } => serde_json::json!({
            "type": "text",
            "text": text,
        }),
        Content::Image { data, mime_type } => serde_json::json!({
            "type": "image",
            "data": data,
            "mimeType": mime_type,
        }),
        // Unknown content types — preserve type marker so they group together
        // by shape rather than disappearing.
        Content::Other => serde_json::json!({ "type": "other" }),
    }
}

/// Push a structured divergence summary into `outcome.notes`.
fn push_divergence_notes(outcome: &mut ScenarioOutcome, tool: &str, report: &DivergenceReport) {
    outcome.notes.push(format!(
        "divergence detected: tool={} total={} unique={}",
        tool, report.total_responses, report.unique_responses
    ));
    // Surface up to the top three groups so reports stay readable. Each
    // sample is `(count, canonical_json)`.
    for (i, (count, canonical)) in report.samples.iter().take(3).enumerate() {
        outcome.notes.push(format!(
            "  group #{idx}: occurrences={count} canonical={}",
            preview(canonical, 200),
            idx = i + 1,
        ));
    }
    if report.samples.len() > 3 {
        outcome.notes.push(format!(
            "  ... {} more group(s) omitted",
            report.samples.len() - 3
        ));
    }
}

/// Truncate a string to `max` chars with an ellipsis marker.
fn preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cutoff = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        let mut out = s[..cutoff].to_string();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let s = RaceCheck {
            concurrent: 1,
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        };
        assert_eq!(s.name(), "race_check");
    }

    #[test]
    fn config_schema_requires_tool() {
        let s = RaceCheck {
            concurrent: 1,
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        };
        let schema = s.config_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v == "tool"));
    }

    #[test]
    fn content_text_to_value_roundtrip() {
        let v = content_to_value(&Content::Text {
            text: "hello".to_string(),
        });
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn content_image_to_value_roundtrip() {
        let v = content_to_value(&Content::Image {
            data: "AAAA".to_string(),
            mime_type: "image/png".to_string(),
        });
        assert_eq!(v["type"], "image");
        assert_eq!(v["mimeType"], "image/png");
    }

    #[test]
    fn preview_truncates_long_strings() {
        let s = "x".repeat(300);
        let p = preview(&s, 100);
        assert!(p.ends_with("..."));
        assert!(p.len() <= 103);
    }
}
