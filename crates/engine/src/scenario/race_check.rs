//! `race_check` scenario — non-determinism / race detector.
//!
//! Issues `concurrent` identical `tools/call` invocations and feeds the
//! responses to [`crate::race_detector::analyze`]. If two or more
//! responses canonicalize to distinct strings the scenario flags a divergence:
//! the tool is non-deterministic under the same inputs.
//!
//! Calls are driven through the session pool: all sessions finish their
//! handshake first, then a shared start gate releases one call per worker.
//! A bare direct-library [`RunContext`] without a session factory is rejected
//! explicitly instead of silently degrading to a sequential test.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::race_detector::{DivergenceReport, analyze};
use crate::scenario::{
    RunContext, Scenario, ScenarioOutcome, classify_error, is_logical_tool_error, pool, teardown,
};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};
use mcp_loadtest_protocol::mcp::{CallToolResult, Content};

/// Fire N identical tool calls and check for divergent responses.
pub struct RaceCheck {
    /// Number of synchronized, independent sessions to call.
    pub concurrent: u32,
    /// Tool to invoke.
    pub tool: String,
    /// Arguments passed verbatim on every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for RaceCheck {
    async fn drive(&self, _session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        if self.concurrent < 2 {
            return invalid_plan("concurrent must be >= 2 to compare responses");
        }
        if ctx.session_factory.is_none() {
            return invalid_plan(
                "a session_factory is required for synchronized concurrent calls \
                 (Run::execute attaches one automatically)",
            );
        }

        let responses = Arc::new(Mutex::new(Vec::with_capacity(self.concurrent as usize)));
        let shared_responses = Arc::clone(&responses);
        let tool = Arc::new(self.tool.clone());
        let args = Arc::new(self.args.clone());

        let mut outcome = pool::drive_pooled(
            ctx,
            self.concurrent,
            move |iter, mut session, worker_ctx| {
                let responses = Arc::clone(&shared_responses);
                let tool = Arc::clone(&tool);
                let args = Arc::clone(&args);
                async move {
                    let mut worker = ScenarioOutcome::default();
                    if worker_ctx.is_cancelled() {
                        worker.error_count = 1;
                        worker
                            .notes
                            .push(format!("cancelled before synchronized call iter={iter}"));
                        teardown::shutdown_session(
                            session,
                            &mut worker,
                            format!("race_check cancelled worker {iter}"),
                        )
                        .await;
                        return worker;
                    }

                    let started = Instant::now();
                    let result = hang_detect(
                        session.call_tool(&tool, &args),
                        worker_ctx.hang_threshold,
                        worker_ctx.grace_period,
                    )
                    .await;
                    worker.total_calls = 1;
                    match result {
                        HangOutcome::Ok { result, duration } => {
                            if is_logical_tool_error(&result) {
                                worker.error_count = 1;
                                worker_ctx.metrics.record_tool(
                                    &tool,
                                    duration,
                                    CallOutcome::ServerError,
                                );
                            } else {
                                worker.successful_calls = 1;
                                worker_ctx.metrics.record_tool(
                                    &tool,
                                    duration,
                                    CallOutcome::Success,
                                );
                            }
                            store_response(&responses, &mut worker, result);
                        }
                        HangOutcome::Slow { result, duration } => {
                            worker.hang_count = 1;
                            if is_logical_tool_error(&result) {
                                worker.error_count = 1;
                                worker_ctx.metrics.record_tool(
                                    &tool,
                                    duration,
                                    CallOutcome::ServerError,
                                );
                            } else {
                                worker_ctx
                                    .metrics
                                    .record_tool(&tool, duration, CallOutcome::Hang);
                            }
                            store_response(&responses, &mut worker, result);
                        }
                        HangOutcome::Deadlock { hung_for } => {
                            worker.deadlock_count = 1;
                            worker.hung_for_ms.push(hung_for.as_millis());
                            worker_ctx
                                .metrics
                                .record_tool(&tool, hung_for, CallOutcome::Deadlock);
                            worker.notes.push(format!(
                                "deadlock: tool={tool} iter={iter} hung_for={}ms",
                                hung_for.as_millis()
                            ));
                        }
                        HangOutcome::Err(err) => {
                            worker.error_count = 1;
                            worker_ctx.metrics.record_tool(
                                &tool,
                                started.elapsed(),
                                classify_error(&err),
                            );
                            worker
                                .notes
                                .push(format!("error: tool={tool} iter={iter} err={err}"));
                        }
                        other => {
                            worker.error_count = 1;
                            worker.notes.push(format!(
                                "unexpected hang outcome: tool={tool} iter={iter}: {other:?}"
                            ));
                        }
                    }

                    teardown::shutdown_session(
                        session,
                        &mut worker,
                        format!("race_check worker {iter}"),
                    )
                    .await;
                    worker
                }
            },
        )
        .await;

        let responses = match responses.lock() {
            Ok(values) => values.clone(),
            Err(_) => {
                outcome.error_count += 1;
                outcome
                    .notes
                    .push("race_check: response collector lock poisoned".to_owned());
                Vec::new()
            }
        };
        if responses.len() != self.concurrent as usize {
            let missing = (self.concurrent as usize).saturating_sub(responses.len()) as u64;
            // Pool spawn/call failures normally account for this already.
            // `max` keeps the signal present without double-counting the same
            // missing response.
            outcome.error_count = outcome.error_count.max(missing);
            outcome.notes.push(format!(
                "race_check inconclusive: received {}/{} comparable responses",
                responses.len(),
                self.concurrent
            ));
            return outcome;
        }
        let report = analyze(&responses);
        if report.diverged {
            outcome.divergence_count += 1;
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
                    "minimum": 2,
                    "default": 10,
                    "description": "Number of independent sessions released through one synchronization gate. Must be at least 2."
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

fn invalid_plan(message: &str) -> ScenarioOutcome {
    ScenarioOutcome {
        error_count: 1,
        notes: vec![format!("race_check: invalid plan — {message}")],
        ..ScenarioOutcome::default()
    }
}

fn store_response(
    responses: &Mutex<Vec<Value>>,
    outcome: &mut ScenarioOutcome,
    result: CallToolResult,
) {
    match responses.lock() {
        Ok(mut values) => values.push(call_tool_result_to_value(&result)),
        Err(_) => {
            outcome.error_count += 1;
            outcome
                .notes
                .push("response collector lock poisoned".to_owned());
        }
    }
}

/// Manually convert a [`CallToolResult`] to a `Value` — the type only derives
/// `Deserialize`, so we build the JSON shape by hand. We preserve the exact
/// public fields (`content`, `isError`) so the canonicalizer sees the same
/// shape the server actually returned.
fn call_tool_result_to_value(result: &CallToolResult) -> Value {
    let content: Vec<Value> = result.content.iter().map(content_to_value).collect();
    serde_json::json!({
        "_meta": result.meta,
        "content": content,
        "isError": result.is_error,
        "structuredContent": result.structured_content,
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
        // Raw content retains every forward-compatible field (audio,
        // resources, annotations, `_meta`, and vendor extensions), so the
        // race detector cannot collapse distinct valid responses.
        Content::Raw(value) => value.clone(),
        // Programmatic legacy value; wire deserialization never produces it.
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
            concurrent: 2,
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        };
        assert_eq!(s.name(), "race_check");
    }

    #[test]
    fn config_schema_requires_tool() {
        let s = RaceCheck {
            concurrent: 2,
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

    #[test]
    fn structured_content_participates_in_divergence_detection() {
        let first = CallToolResult {
            meta: None,
            content: vec![Content::Text {
                text: "stable".to_owned(),
            }],
            is_error: false,
            structured_content: Some(serde_json::json!({"value": 1})),
        };
        let second = CallToolResult {
            structured_content: Some(serde_json::json!({"value": 2})),
            ..first.clone()
        };

        let report = analyze(&[
            call_tool_result_to_value(&first),
            call_tool_result_to_value(&second),
        ]);
        assert!(report.diverged);
    }

    #[test]
    fn canonical_value_preserves_logical_error_flag() {
        let result = CallToolResult {
            meta: None,
            content: Vec::new(),
            is_error: true,
            structured_content: None,
        };
        assert_eq!(call_tool_result_to_value(&result)["isError"], true);
    }

    #[test]
    fn distinct_forward_compatible_content_is_reported_as_divergence() {
        let cases = [
            (
                serde_json::json!({
                    "type": "resource",
                    "resource": {
                        "uri": "file:///one.txt",
                        "mimeType": "text/plain",
                        "text": "one"
                    }
                }),
                serde_json::json!({
                    "type": "resource",
                    "resource": {
                        "uri": "file:///two.txt",
                        "mimeType": "text/plain",
                        "text": "two"
                    }
                }),
            ),
            (
                serde_json::json!({
                    "type": "audio",
                    "data": "AAAA",
                    "mimeType": "audio/wav"
                }),
                serde_json::json!({
                    "type": "audio",
                    "data": "BBBB",
                    "mimeType": "audio/wav"
                }),
            ),
            (
                serde_json::json!({
                    "type": "resource_link",
                    "uri": "file:///one.txt",
                    "name": "one"
                }),
                serde_json::json!({
                    "type": "resource_link",
                    "uri": "file:///two.txt",
                    "name": "two"
                }),
            ),
            (
                serde_json::json!({
                    "type": "future_vendor_content",
                    "payload": {"value": 1}
                }),
                serde_json::json!({
                    "type": "future_vendor_content",
                    "payload": {"value": 2}
                }),
            ),
        ];

        for (left_content, right_content) in cases {
            let left: CallToolResult = serde_json::from_value(serde_json::json!({
                "content": [left_content],
                "isError": false
            }))
            .expect("left tool result should parse");
            let right: CallToolResult = serde_json::from_value(serde_json::json!({
                "content": [right_content],
                "isError": false
            }))
            .expect("right tool result should parse");

            let report = analyze(&[
                call_tool_result_to_value(&left),
                call_tool_result_to_value(&right),
            ]);
            assert!(
                report.diverged,
                "distinct forward-compatible content collapsed: left={left:?} right={right:?}"
            );
        }
    }

    #[test]
    fn annotations_and_meta_participate_in_divergence_detection() {
        let parse = |priority: f64, cache_key: &str| {
            serde_json::from_value::<CallToolResult>(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": "same payload",
                    "annotations": {"priority": priority},
                    "_meta": {"cacheKey": cache_key}
                }],
                "isError": false
            }))
            .expect("annotated tool result should parse")
        };
        let baseline = parse(0.1, "a");
        for changed in [parse(0.9, "a"), parse(0.1, "b")] {
            let report = analyze(&[
                call_tool_result_to_value(&baseline),
                call_tool_result_to_value(&changed),
            ]);
            assert!(
                report.diverged,
                "annotation-only and _meta-only changes must both diverge"
            );
        }
    }

    #[test]
    fn result_meta_participates_in_divergence_detection() {
        let parse = |request_id: &str| {
            serde_json::from_value::<CallToolResult>(serde_json::json!({
                "_meta": {"requestId": request_id},
                "content": [{"type": "text", "text": "same payload"}],
                "isError": false
            }))
            .expect("tool result with _meta should parse")
        };
        let first = parse("one");
        let second = parse("two");

        let report = analyze(&[
            call_tool_result_to_value(&first),
            call_tool_result_to_value(&second),
        ]);
        assert!(report.diverged);
    }
}
