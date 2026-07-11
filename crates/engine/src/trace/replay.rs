//! Replay a recorded trace against a fresh transport and diff responses
//! (ADR 0021 §4).
//!
//! Replay is deliberately Session-less: a [`mcp_loadtest_protocol::Session`] runs its own
//! `initialize` handshake and mints its own ids, so it cannot reproduce the
//! recorded conversation. The recorded client frames are pushed through a
//! bare [`Transport`] in recorded order instead:
//!
//! - **requests** (frames with an `id`) get fresh sequential JSON-RPC ids
//!   (1, 2, 3, …); each response is diffed against the recorded one via
//!   [`crate::race_detector`] canonicalization with the top-level
//!   `id` stripped from both sides;
//! - **notifications** are re-sent as-is for protocol-state fidelity (e.g.
//!   `notifications/initialized`) but produce nothing to diff;
//! - transport errors and per-request timeouts count as divergence;
//! - a request with no recorded response (truncated trace) is re-sent but
//!   excluded from scoring.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::race_detector;
use mcp_loadtest_protocol::transport::Transport;

use super::TraceError;
use super::format::{self, Direction, TraceFrame};

/// Outcome of replaying one trace: `matched + diverged.len() == total`.
#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    /// Request frames scored (sent, with a recorded response to diff against).
    pub total: usize,
    /// Frames whose replayed response matched the recording (ids ignored).
    pub matched: usize,
    /// Frames that differed, errored, or timed out.
    pub diverged: Vec<Divergence>,
}

/// One diverging request frame.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// 0-based index among the scored request frames.
    pub index: usize,
    /// JSON-RPC method of the request, when recorded.
    pub method: Option<String>,
    /// What went differently.
    pub note: String,
}

/// Read `path`, parse it as `mcp-trace/1`, and [`replay_frames`] it through
/// `transport`.
pub async fn replay_file(
    path: &Path,
    transport: &mut dyn Transport,
    request_timeout: Duration,
) -> Result<ReplayReport, TraceError> {
    let text = tokio::fs::read_to_string(path).await?;
    let (_header, frames) = format::parse_trace(&text)?;
    replay_frames(&frames, transport, request_timeout).await
}

/// Re-send the client frames of `frames` through `transport` and diff each
/// request's response against the recorded one (see module docs for the
/// exact semantics). `request_timeout` bounds every send — a hung server
/// shows up as a divergence, never as a hung replay.
pub async fn replay_frames(
    frames: &[TraceFrame],
    transport: &mut dyn Transport,
    request_timeout: Duration,
) -> Result<ReplayReport, TraceError> {
    // Recorded response ids, pre-parsed once. Correlation is a forward scan
    // consuming each response at most once, so duplicate ids from
    // session-factory respawns within one trace pair up correctly.
    let s2c_ids: Vec<Option<Value>> = frames
        .iter()
        .map(|f| match f.dir {
            Direction::ServerToClient => body_id(&f.body),
            Direction::ClientToServer => None,
        })
        .collect();
    let mut consumed = vec![false; frames.len()];

    let mut report = ReplayReport::default();
    let mut next_id: u64 = 1;

    for (i, frame) in frames.iter().enumerate() {
        if frame.dir != Direction::ClientToServer {
            continue;
        }
        let Ok(mut body) = serde_json::from_str::<Value>(&frame.body) else {
            tracing::warn!(index = i, "replay: skipping unparseable client frame");
            continue;
        };
        if body.get("method").is_none() {
            tracing::warn!(index = i, "replay: skipping client frame without a method");
            continue;
        }
        let Some(recorded_id) = body.get("id").cloned() else {
            // Notification — re-send for state fidelity; nothing to diff.
            match tokio::time::timeout(request_timeout, transport.notify(&frame.body)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(index = i, error = %err, "replay: notification failed");
                }
                Err(_) => tracing::warn!(index = i, "replay: notification timed out"),
            }
            continue;
        };

        // Correlate with the first unconsumed recorded response carrying the
        // same id, scanning forward from the request.
        let resp_idx = (i + 1..frames.len())
            .find(|&j| !consumed[j] && s2c_ids[j].as_ref() == Some(&recorded_id));
        if let Some(j) = resp_idx {
            consumed[j] = true;
        }

        // Rewrite the id sequentially and send.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("id".to_owned(), Value::from(next_id));
        }
        next_id += 1;
        let outgoing = serde_json::to_string(&body)?;
        let sent = tokio::time::timeout(request_timeout, transport.request(&outgoing)).await;

        let Some(j) = resp_idx else {
            tracing::warn!(
                index = i,
                "replay: request has no recorded response (truncated trace?); sent but not scored"
            );
            continue;
        };

        let index = report.total;
        report.total += 1;
        let method = frame.method.clone();
        match sent {
            Err(_) => report.diverged.push(Divergence {
                index,
                method,
                note: format!("no response within {request_timeout:?}"),
            }),
            Ok(Err(err)) => report.diverged.push(Divergence {
                index,
                method,
                note: format!("transport error: {err}"),
            }),
            Ok(Ok(replayed)) => {
                if responses_match(&frames[j].body, &replayed) {
                    report.matched += 1;
                } else {
                    report.diverged.push(Divergence {
                        index,
                        method,
                        note: "response differs from recording (ids ignored)".to_owned(),
                    });
                }
            }
        }
    }
    Ok(report)
}

/// Compare two raw response bodies, ignoring their JSON-RPC `id`s. Reuses
/// the race detector's canonicalization (sorted keys, preserved array order)
/// so key order / whitespace never count as divergence. When either side
/// fails to parse, only byte equality can match.
fn responses_match(recorded: &str, replayed: &str) -> bool {
    match (normalize_response(recorded), normalize_response(replayed)) {
        (Some(a), Some(b)) => !race_detector::analyze(&[a, b]).diverged,
        _ => recorded == replayed,
    }
}

/// Parse a raw response and strip its top-level `id`.
fn normalize_response(body: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(body).ok()?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("id");
    }
    Some(value)
}

/// Top-level `id` of a raw JSON-RPC body, if any.
fn body_id(body: &str) -> Option<Value> {
    serde_json::from_str::<Value>(body).ok()?.get("id").cloned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use mcp_loadtest_protocol::transport::TransportError;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(2);

    fn frame(dir: Direction, method: Option<&str>, body: &str) -> TraceFrame {
        TraceFrame {
            dir,
            elapsed_ms: 0,
            method: method.map(str::to_owned),
            body: body.to_owned(),
        }
    }

    /// Boxed request→response script for [`Scripted`].
    type RespondFn = Box<dyn Fn(&str) -> Result<String, TransportError> + Send>;

    /// Scripted transport: `respond` maps a request body to a response body
    /// (or an error); notifications are counted.
    struct Scripted {
        respond: RespondFn,
        notified: Arc<AtomicUsize>,
    }

    impl Scripted {
        fn new(respond: impl Fn(&str) -> Result<String, TransportError> + Send + 'static) -> Self {
            Self {
                respond: Box::new(respond),
                notified: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Transport for Scripted {
        async fn request(&mut self, body: &str) -> Result<String, TransportError> {
            (self.respond)(body)
        }
        async fn notify(&mut self, _body: &str) -> Result<(), TransportError> {
            self.notified.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// Echo-style deterministic responder: same `result` for every request,
    /// with the request's (rewritten) id echoed back.
    fn echo_ok(body: &str) -> Result<String, TransportError> {
        let v: Value = serde_json::from_str(body).expect("test body parses");
        Ok(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"ok":true}}}}"#,
            v["id"]
        ))
    }

    #[tokio::test]
    async fn matches_identical_responses_despite_different_ids() {
        // Recorded ids 41/42; replay rewrites to 1/2 — ids must be ignored.
        let frames = vec![
            frame(
                Direction::ClientToServer,
                Some("tools/list"),
                r#"{"jsonrpc":"2.0","id":41,"method":"tools/list","params":{}}"#,
            ),
            frame(
                Direction::ServerToClient,
                Some("tools/list"),
                r#"{"jsonrpc":"2.0","id":41,"result":{"ok":true}}"#,
            ),
            frame(
                Direction::ClientToServer,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"echo"}}"#,
            ),
            frame(
                Direction::ServerToClient,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#,
            ),
        ];
        let mut t = Scripted::new(echo_ok);
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.matched, 2, "diverged: {:?}", report.diverged);
        assert!(report.diverged.is_empty());
    }

    #[tokio::test]
    async fn key_order_is_not_a_divergence() {
        // Recorded response has keys in a different order than the replayed
        // one — canonicalization must group them.
        let frames = vec![
            frame(
                Direction::ClientToServer,
                Some("tools/list"),
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
            ),
            frame(
                Direction::ServerToClient,
                Some("tools/list"),
                r#"{"result":{"ok":true},"id":1,"jsonrpc":"2.0"}"#,
            ),
        ];
        let mut t = Scripted::new(echo_ok);
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.matched, 1);
    }

    #[tokio::test]
    async fn flags_differing_result_as_divergence() {
        let frames = vec![
            frame(
                Direction::ClientToServer,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
            ),
            frame(
                Direction::ServerToClient,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":1,"result":{"ok":false}}"#,
            ),
        ];
        let mut t = Scripted::new(echo_ok);
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.matched, 0);
        assert_eq!(report.diverged.len(), 1);
        assert_eq!(report.diverged[0].index, 0);
        assert_eq!(report.diverged[0].method.as_deref(), Some("tools/call"));
    }

    #[tokio::test]
    async fn transport_error_counts_as_divergence() {
        let frames = vec![
            frame(
                Direction::ClientToServer,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
            ),
            frame(
                Direction::ServerToClient,
                Some("tools/call"),
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            ),
        ];
        let mut t = Scripted::new(|_| Err(TransportError::Closed));
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.diverged.len(), 1);
        assert!(report.diverged[0].note.contains("transport error"));
    }

    #[tokio::test]
    async fn notifications_are_resent_but_not_scored() {
        let frames = vec![frame(
            Direction::ClientToServer,
            Some("notifications/initialized"),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        )];
        let mut t = Scripted::new(echo_ok);
        let notified = t.notified.clone();
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.total, 0);
        assert_eq!(notified.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn truncated_trace_request_is_sent_but_not_scored() {
        // Request with no recorded response — sent for state fidelity, but
        // excluded from total/matched/diverged.
        let frames = vec![frame(
            Direction::ClientToServer,
            Some("tools/call"),
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{}}"#,
        )];
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let mut t = Scripted::new(move |body| {
            seen.fetch_add(1, Ordering::SeqCst);
            echo_ok(body)
        });
        let report = replay_frames(&frames, &mut t, TIMEOUT).await.unwrap();
        assert_eq!(report.total, 0);
        assert!(report.diverged.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "request must still go out");
    }

    #[test]
    fn normalize_response_strips_top_level_id_only() {
        let a = normalize_response(r#"{"jsonrpc":"2.0","id":7,"result":{"id":3}}"#).unwrap();
        let b = normalize_response(r#"{"jsonrpc":"2.0","id":900,"result":{"id":3}}"#).unwrap();
        assert_eq!(a, b, "top-level id stripped");
        let c = normalize_response(r#"{"jsonrpc":"2.0","id":7,"result":{"id":4}}"#).unwrap();
        assert_ne!(a, c, "nested ids are payload, not envelope");
    }

    #[test]
    fn responses_match_falls_back_to_byte_equality_when_unparseable() {
        assert!(responses_match("garbage", "garbage"));
        assert!(!responses_match("garbage", "other-garbage"));
    }
}
