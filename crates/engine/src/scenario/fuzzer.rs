//! `fuzzer` scenario — sends malformed-but-plausible payloads at the server
//! and classifies the responses.
//!
//! See DESIGN.md §10.5 differentiator row "Protocol fuzzer". M7 differentiator
//! ownership: Agent U.
//!
//! # M7 minimal: enumerated malformations only
//!
//! The full design (DESIGN.md §10.5) envisions a true RNG-driven combinatorial
//! fuzzer. M7 ships the **enumerated** subset: a fixed list of
//! malformed-but-plausible payload shapes we cycle through. The seed knob is
//! retained so M8 can grow random mutations without changing the public API.
//!
//! ## Two send paths: typed and raw
//!
//! Most payloads route through [`Session::call_tool`] with deliberately weird
//! tool names and argument shapes — covering parser bugs in argument
//! validation, type-confusion in method dispatch (via the tool name), and
//! resource-exhaustion (giant / deeply-nested params).
//!
//! Payloads that must **violate JSON-RPC framing itself** (`EmptyBody`,
//! `InvalidJson`, missing / wrong `jsonrpc` version, missing / duplicate id)
//! cannot be expressed that way. They go out as literal bytes via
//! `Transport::raw_send` (see the `raw` submodule) — but only when the
//! [`RunContext`] carries a `SessionFactory`, because a raw send desyncs the
//! wire and the session must be respawned afterward. Without a factory those
//! variants keep the honest per-iteration skip and never bump
//! `total_calls`.
//!
//! ## Classification
//!
//! Each iteration's outcome maps to a [`FuzzClass`] (see the `classify` /
//! `raw` submodules and [`mcp_loadtest_core::fuzz_report`]): server acceptance →
//! `Accepted`, explicit JSON-RPC `-32700` / `-32600` / `-32601` / `-32602`
//! rejection or `tools/call` `isError: true` → `ProtocolError` (the expected,
//! healthy outcome), other server error (including `-32603`) → `ServerError`,
//! client-side parse / id mismatch → `ParseError`, transport / io failure →
//! `Disconnected`, and no response within budget → `Deadlock`. `Deadlock` /
//! `Disconnected` / malformed `Accepted` are the interesting findings worth
//! surfacing.
//!
//! ## Module layout
//!
//! - `payloads` — the [`FuzzPayload`] enum + its impls + the giant/nested
//!   payload `LazyLock` constants.
//! - `classify` — response classification + report-note rendering helpers.
//! - `raw` — raw-byte payload send, liveness probe, and poisoned-session
//!   respawn (the `Transport::raw_send` path).
//! - this file — the [`Fuzzer`] struct and [`Scenario`] impl.

mod classify;
mod payloads;
mod raw;

use std::time::Duration;

use async_trait::async_trait;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use serde_json::Value;

pub use payloads::FuzzPayload;

use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use classify::{classify_err, is_expected_tool_rejection, push_report_notes};
use mcp_loadtest_core::fuzz_report::{FuzzClass, FuzzFinding, FuzzReport};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};
use mcp_loadtest_protocol::mcp::CallToolResult;

/// Default cap on per-iteration findings retained in the aggregated report.
const DEFAULT_MAX_FINDINGS: usize = 64;

/// Record a completed typed `tools/call` fuzz probe.
///
/// A logical tool error is an explicit, healthy rejection of the deliberately
/// bad input. A normal result means the malformed input was accepted and is an
/// unexpected fuzzer error. Slow acceptance retains `Hang` telemetry but also
/// increments `error_count`, so a mixed cohort cannot render a false PASS.
fn record_completed_probe(
    payload: FuzzPayload,
    result: &CallToolResult,
    duration: Duration,
    slow: bool,
    ctx: &RunContext,
    outcome: &mut ScenarioOutcome,
    findings: &mut Vec<FuzzFinding>,
) {
    if slow {
        outcome.hang_count += 1;
    }

    if is_expected_tool_rejection(result) {
        outcome.successful_calls += 1;
        ctx.metrics.record(duration, CallOutcome::ExpectedRejection);
        findings.push(FuzzFinding {
            payload: payload.label().to_string(),
            class: FuzzClass::ProtocolError,
            code: None,
            note: format!(
                "server rejected malformed tools/call via isError=true in {}ms{}",
                duration.as_millis(),
                if slow { " (slow)" } else { "" }
            ),
        });
    } else {
        outcome.error_count += 1;
        ctx.metrics.record(
            duration,
            if slow {
                CallOutcome::Hang
            } else {
                CallOutcome::Malformed
            },
        );
        findings.push(FuzzFinding {
            payload: payload.label().to_string(),
            class: FuzzClass::Accepted,
            code: None,
            note: format!(
                "server accepted malformed input in {}ms{}",
                duration.as_millis(),
                if slow {
                    " (slow; input validation may be too permissive)"
                } else {
                    " (suspicious — input validation may be too permissive)"
                }
            ),
        });
    }
}

/// Drive a [`Session`] with malformed payloads and classify the responses.
pub struct Fuzzer {
    /// How many iterations to run.
    pub iterations: u32,
    /// Seed for the RNG used to pick payloads. Fixed seed → reproducible run.
    pub seed: u64,
    /// Payload variants to draw from. Empty → all
    /// [`FuzzPayload::exercisable`] variants.
    pub payloads: Vec<FuzzPayload>,
}

impl Default for Fuzzer {
    fn default() -> Self {
        Self {
            iterations: 50,
            seed: 0xC0FFEE,
            payloads: Vec::new(),
        }
    }
}

impl Fuzzer {
    /// Build the resolved payload list (using defaults when empty).
    fn resolved_payloads(&self) -> Vec<FuzzPayload> {
        if self.payloads.is_empty() {
            FuzzPayload::all()
        } else {
            self.payloads.clone()
        }
    }
}

#[async_trait]
impl Scenario for Fuzzer {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();
        let mut findings: Vec<FuzzFinding> = Vec::with_capacity(self.iterations as usize);

        let pool = self.resolved_payloads();
        if pool.is_empty() {
            outcome
                .notes
                .push("fuzzer: empty payload pool — nothing to drive".to_string());
            return outcome;
        }

        let mut rng = StdRng::seed_from_u64(self.seed);

        for iter in 0..self.iterations {
            if ctx.is_cancelled() {
                outcome.notes.push(format!("cancelled before iter={iter}"));
                break;
            }

            // `choose` returns None only on empty slice; we already early-returned above.
            let payload = match pool.choose(&mut rng) {
                Some(p) => *p,
                None => break,
            };

            // Raw-transport-only payloads (`EmptyBody`, `InvalidJson`, missing
            // / wrong `jsonrpc` version, missing / duplicate id) can't be
            // expressed through `call_tool` — they must break JSON-RPC framing
            // itself. With a session factory we send the real malformed bytes
            // via `Transport::raw_send` and respawn the poisoned session (see
            // `raw::handle_raw_payload`, which owns the `total_calls`
            // increment). Without a factory we keep the honest skip: skips are
            // NOT counted against total_calls or the metrics recorder — they
            // didn't exercise the server, so polluting the Cancelled bucket
            // would skew downstream error rates.
            let Some((tool_name, args)) = payload.to_call_args() else {
                match &ctx.session_factory {
                    Some(factory) => {
                        if raw::handle_raw_payload(
                            session,
                            factory,
                            payload,
                            ctx,
                            &mut outcome,
                            &mut findings,
                            iter,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    None => findings.push(FuzzFinding {
                        payload: payload.label().to_string(),
                        class: FuzzClass::Other,
                        code: None,
                        note: "skipped: raw-byte payload needs a session factory to \
                               recover the poisoned connection (none attached)"
                            .to_string(),
                    }),
                }
                continue;
            };

            outcome.total_calls += 1;

            let call_fut = session.call_tool(&tool_name, &args);
            let hang_outcome = hang_detect(call_fut, ctx.hang_threshold, ctx.grace_period).await;

            match hang_outcome {
                HangOutcome::Ok { result, duration } => record_completed_probe(
                    payload,
                    &result,
                    duration,
                    false,
                    ctx,
                    &mut outcome,
                    &mut findings,
                ),
                HangOutcome::Slow { result, duration } => record_completed_probe(
                    payload,
                    &result,
                    duration,
                    true,
                    ctx,
                    &mut outcome,
                    &mut findings,
                ),
                HangOutcome::Deadlock { hung_for } => {
                    outcome.deadlock_count += 1;
                    ctx.metrics.record(hung_for, CallOutcome::Deadlock);
                    findings.push(FuzzFinding {
                        payload: payload.label().to_string(),
                        class: FuzzClass::Deadlock,
                        code: None,
                        note: format!(
                            "deadlock on payload={}: hung_for={}ms",
                            payload.label(),
                            hung_for.as_millis()
                        ),
                    });
                    // After a deadlock the session is wedged — further calls
                    // will all hang. Break out, matching DeadlockProbe.
                    outcome
                        .notes
                        .push(format!("fuzzer: stopping after deadlock at iter={iter}"));
                    break;
                }
                HangOutcome::Err(e) => {
                    let (class, code, note) = classify_err(&e);
                    let metric_outcome = match class {
                        FuzzClass::ProtocolError => CallOutcome::ExpectedRejection,
                        FuzzClass::ServerError => CallOutcome::ServerError,
                        FuzzClass::ParseError => CallOutcome::Malformed,
                        FuzzClass::Disconnected => CallOutcome::Disconnected,
                        _ => CallOutcome::ServerError,
                    };
                    // A protocol-level rejection is the expected healthy
                    // response to malformed fuzz input. Count the probe as a
                    // scenario success and preserve it as ExpectedRejection
                    // in metrics. Crashes, client-side protocol failures,
                    // malformed replies and server errors remain failures.
                    if class == FuzzClass::ProtocolError {
                        outcome.successful_calls += 1;
                    } else {
                        outcome.error_count += 1;
                    }
                    ctx.metrics.record(Duration::ZERO, metric_outcome);
                    findings.push(FuzzFinding {
                        payload: payload.label().to_string(),
                        class,
                        code,
                        note,
                    });
                    // Disconnect = the server is gone; further sends will all
                    // fail. Bail out so we don't churn the loop.
                    if class == FuzzClass::Disconnected {
                        outcome.notes.push(format!(
                            "fuzzer: stopping after transport closed at iter={iter}"
                        ));
                        break;
                    }
                }
                // `HangOutcome` is `#[non_exhaustive]` (mcp-loadtest-protocol):
                // a cross-crate wildcard is mandatory. Only Ok/Slow/Deadlock/
                // Err exist today; count any future variant as an error so it
                // is never silently dropped from the outcome.
                other => {
                    outcome.error_count += 1;
                    outcome.notes.push(format!(
                        "fuzzer: unexpected hang outcome at iter={iter}: {other:?}"
                    ));
                }
            }
        }

        // Aggregate findings into a structured report and surface the
        // headline numbers + a few interesting rows in `outcome.notes` so
        // callers without a custom report renderer still see them.
        let report = FuzzReport::from_findings(&findings, DEFAULT_MAX_FINDINGS);
        push_report_notes(&mut outcome, &report);

        outcome
    }

    fn config_schema(&self) -> Value {
        classify::config_schema()
    }

    fn name(&self) -> &'static str {
        "fuzzer"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use mcp_loadtest_core::metrics::Recorder;
    use mcp_loadtest_protocol::SessionError;
    use mcp_loadtest_protocol::mcp::Content;
    use payloads::nested_object;
    use tokio_util::sync::CancellationToken;

    fn tool_result(is_error: bool) -> CallToolResult {
        CallToolResult {
            content: vec![Content::Text {
                text: "result".to_owned(),
            }],
            is_error,
            structured_content: None,
            meta: None,
        }
    }

    #[test]
    fn name_is_stable() {
        let f = Fuzzer::default();
        assert_eq!(f.name(), "fuzzer");
    }

    #[test]
    fn config_schema_advertises_iterations_required() {
        let f = Fuzzer::default();
        let schema = f.config_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v == "iterations"));
    }

    #[test]
    fn payload_labels_unique_and_stable() {
        let labels: Vec<&str> = FuzzPayload::all().iter().map(|p| p.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "duplicate labels: {labels:?}");
    }

    #[test]
    fn exercisable_payloads_serialize_to_call_args() {
        for p in FuzzPayload::exercisable() {
            let (tool, args) = p
                .to_call_args()
                .unwrap_or_else(|| panic!("{:?} should be exercisable", p));
            // Tool name is allowed to be weird; just verify it's some string.
            assert!(!tool.is_empty() || matches!(p, FuzzPayload::ControlChars));
            // Args must serialize to JSON.
            let _ = serde_json::to_string(&args).expect("args must serialize");
        }
    }

    #[test]
    fn raw_transport_payloads_skip_at_runtime() {
        for p in FuzzPayload::all() {
            if p.requires_raw_transport() {
                assert!(
                    p.to_call_args().is_none(),
                    "{:?} should not produce call args",
                    p
                );
            }
        }
    }

    #[test]
    fn raw_bytes_partitions_the_enum_against_call_args() {
        // Exactly the raw-transport variants yield raw bytes; every other
        // variant yields call args. The two methods must never both fire (or
        // both stay silent) for a single variant.
        for p in FuzzPayload::all() {
            let raw = p.raw_bytes();
            let call = p.to_call_args();
            if p.requires_raw_transport() {
                assert!(raw.is_some(), "{p:?} must have raw bytes");
                assert!(call.is_none(), "{p:?} must not have call args");
            } else {
                assert!(raw.is_none(), "{p:?} must not have raw bytes");
                assert!(call.is_some(), "{p:?} must have call args");
            }
        }
    }

    #[test]
    fn duplicate_id_raw_bytes_carry_two_frames_split_by_a_newline() {
        let bytes = FuzzPayload::DuplicateId
            .raw_bytes()
            .expect("DuplicateId is a raw variant");
        // One embedded newline separates two frames (the transport adds the
        // trailing one).
        assert_eq!(
            bytes.iter().filter(|&&b| b == b'\n').count(),
            1,
            "expected exactly one embedded frame separator"
        );
        let text = String::from_utf8(bytes).expect("frames are ascii");
        let frames: Vec<&str> = text.split('\n').collect();
        assert_eq!(frames.len(), 2, "two frames");
        assert_eq!(frames[0], frames[1], "both frames share the same id");
        assert!(frames[0].contains("\"id\":1"));
    }

    #[test]
    fn nested_object_depth_is_correct() {
        let v = nested_object(5);
        // Walk down five levels.
        let mut cur = &v;
        for _ in 0..5 {
            cur = cur.get("x").expect("nested chain should continue");
        }
        assert_eq!(cur.as_str(), Some("leaf"));
    }

    #[test]
    fn classify_err_protocol_range() {
        let err = SessionError::Server(mcp_loadtest_protocol::jsonrpc::ErrorObject {
            code: -32601,
            message: "method not found".to_string(),
            data: None,
        });
        let (class, code, _note) = classify_err(&err);
        assert_eq!(class, FuzzClass::ProtocolError);
        assert_eq!(code, Some(-32601));
    }

    #[test]
    fn classify_err_server_range() {
        let err = SessionError::Server(mcp_loadtest_protocol::jsonrpc::ErrorObject {
            code: -32000,
            message: "tool failed".to_string(),
            data: None,
        });
        let (class, code, _note) = classify_err(&err);
        assert_eq!(class, FuzzClass::ServerError);
        assert_eq!(code, Some(-32000));
    }

    #[test]
    fn classify_err_transport_is_disconnected() {
        let err = SessionError::Transport(mcp_loadtest_protocol::transport::TransportError::Closed);
        let (class, code, _note) = classify_err(&err);
        assert_eq!(class, FuzzClass::Disconnected);
        assert_eq!(code, None);
    }

    #[test]
    fn classify_err_client_side_schema_failure_is_not_expected_rejection() {
        let err = SessionError::SchemaViolation {
            tool: "echo".to_owned(),
            summary: "required property missing".to_owned(),
        };
        let (class, code, note) = classify_err(&err);
        assert_eq!(class, FuzzClass::Other);
        assert_eq!(code, None);
        assert!(note.contains("client-side"), "got: {note}");
    }

    #[test]
    fn mixed_expected_rejection_and_slow_acceptance_fails_closed() {
        let recorder = Recorder::new();
        let ctx = RunContext::new(
            Instant::now(),
            CancellationToken::new(),
            recorder.clone(),
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        let mut outcome = ScenarioOutcome {
            total_calls: 2,
            ..ScenarioOutcome::default()
        };
        let mut findings = Vec::new();

        record_completed_probe(
            FuzzPayload::UnknownMethod,
            &tool_result(true),
            Duration::from_millis(1),
            false,
            &ctx,
            &mut outcome,
            &mut findings,
        );
        record_completed_probe(
            FuzzPayload::UnknownMethod,
            &tool_result(false),
            Duration::from_millis(20),
            true,
            &ctx,
            &mut outcome,
            &mut findings,
        );

        let metrics = recorder.snapshot();
        assert_eq!(outcome.successful_calls, 1);
        assert_eq!(
            outcome.error_count, 1,
            "slow acceptance must make a mixed fuzzer cohort fail closed"
        );
        assert_eq!(outcome.hang_count, 1);
        assert_eq!(metrics.outcomes.expected_rejection, 1);
        assert_eq!(
            metrics.outcomes.hang, 1,
            "slow accepted input must retain Hang telemetry"
        );
        assert_eq!(metrics.throughput.total_requests, 2);
        assert_eq!(metrics.throughput.successful_requests, 1);
        assert_eq!(findings[0].class, FuzzClass::ProtocolError);
        assert_eq!(findings[1].class, FuzzClass::Accepted);
    }
}
