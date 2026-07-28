//! Raw-byte payload handling for the [`super::Fuzzer`] scenario.
//!
//! The raw-transport-only [`FuzzPayload`] variants (empty body, invalid JSON,
//! missing / wrong `jsonrpc` version, missing / duplicate id) cannot go through
//! `Session::call_tool` — they must violate JSON-RPC framing itself. This
//! module drives them via `Transport::raw_send` (surfaced on `Session` as
//! `Session::raw_send`) and classifies how the server copes.
//!
//! After every raw send the wire is desynced and the child may be wedged, so
//! we treat the session as **poisoned**: we probe once for liveness, then
//! respawn a fresh session through the [`SessionFactory`] before returning to
//! the caller's loop. Split into its own file to keep `fuzzer.rs` under the
//! 300-line production cap.

use std::time::Duration;

use crate::scenario::{RunContext, ScenarioOutcome, teardown};
use mcp_loadtest_core::fuzz_report::{FuzzClass, FuzzFinding, is_expected_rejection_code};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionError;
use mcp_loadtest_protocol::SessionFactory;

use super::FuzzPayload;

/// Send one raw-byte payload, classify the server's reaction, and respawn the
/// (now-poisoned) session in place. Returns `true` when the caller's loop
/// should stop — the only such case is a failed respawn, which leaves no live
/// session to continue with.
///
/// Called only when a [`SessionFactory`] is present; without one the fuzzer
/// keeps the honest skip (a raw send would strand a wedged session with no way
/// to recover). This function owns the `total_calls` increment for the send
/// path, so skipped iterations never bump it (pinned by the fuzzer tests).
pub(super) async fn handle_raw_payload(
    session: &mut Session,
    factory: &SessionFactory,
    payload: FuzzPayload,
    ctx: &RunContext,
    outcome: &mut ScenarioOutcome,
    findings: &mut Vec<FuzzFinding>,
    iter: u32,
) -> bool {
    // Raw variants always carry bytes; stay total (no unwrap) if not.
    let Some(bytes) = payload.raw_bytes() else {
        return false;
    };

    // This iteration actually puts bytes on the wire, so — unlike a skip — it
    // counts.
    outcome.total_calls += 1;

    let (class, kind, dur, code, note) = if let Err(e) = session.raw_send(&bytes).await {
        // The write itself failed: the child is already gone.
        (
            FuzzClass::Disconnected,
            CallOutcome::Disconnected,
            Duration::ZERO,
            None,
            format!("raw send failed (server gone before the write completed): {e}"),
        )
    } else {
        // The write went out, but `raw_send` reads nothing back, so we can't
        // yet tell whether the server rejected-and-survived or crashed. Probe
        // once with a normal request, bounded by hang_threshold + grace_period.
        // Framing is desynced now, so an id mismatch can still prove the child
        // is serving. Invalid JSON proves liveness too, but remains an
        // unexpected malformed response rather than a healthy probe.
        let budget = ctx.hang_threshold + ctx.grace_period;
        match tokio::time::timeout(budget, session.list_tools()).await {
            // No reply within budget: the malformed frame wedged the server.
            Err(_) => (
                FuzzClass::Deadlock,
                CallOutcome::Deadlock,
                budget,
                None,
                format!(
                    "server stopped responding after raw payload (no reply within {}ms — likely wedged on malformed framing)",
                    budget.as_millis()
                ),
            ),
            // Clean reply: server shrugged off the malformed frame and kept
            // serving. This is a healthy probe success, but not an *explicit*
            // rejection, so keep it out of ExpectedRejection.
            Ok(Ok(_tools)) => (
                FuzzClass::ProtocolError,
                CallOutcome::Success,
                Duration::ZERO,
                None,
                "server survived raw malformed frame and kept serving (healthy)".to_string(),
            ),
            // Errored reply: distinguish "server gone" from "alive but desynced".
            Ok(Err(e)) => {
                let code = session_error_code(&e);
                let (class, kind, duration, note) = classify_probe_err(&e);
                (class, kind, duration, code, note)
            }
        }
    };

    // Bump the matching outcome counter (mirrors the typed-path arms).
    if class == FuzzClass::Deadlock {
        outcome.deadlock_count += 1;
        outcome.hung_for_ms.push(dur.as_millis());
    } else if matches!(kind, CallOutcome::Success | CallOutcome::ExpectedRejection) {
        // Explicit rejection and clean survival are both successful probes,
        // but only the explicit JSON-RPC boundary uses ExpectedRejection.
        outcome.successful_calls += 1;
    } else {
        outcome.error_count += 1;
    }
    ctx.metrics.record(dur, kind);
    findings.push(FuzzFinding {
        payload: payload.label().to_string(),
        class,
        code,
        note,
    });

    // The session is poisoned no matter the reaction — respawn before the
    // caller's loop reuses it.
    respawn(session, factory, outcome, iter).await
}

fn session_error_code(err: &SessionError) -> Option<i64> {
    match err {
        SessionError::Server(error) | SessionError::MismatchedErrorResponse { error, .. } => {
            Some(error.code)
        }
        _ => None,
    }
}

/// Interpret a probe error after a raw send. A mismatched structured error can
/// be a healthy rejection of the fuzz frame; a mismatched success means the
/// malformed request was accepted and therefore fails closed.
fn classify_probe_err(err: &SessionError) -> (FuzzClass, CallOutcome, Duration, String) {
    use mcp_loadtest_protocol::transport::TransportError as T;
    match err {
        // Pipe closed / IO failure / never handshook again: the child died.
        SessionError::Transport(T::Closed)
        | SessionError::Transport(T::Io(_))
        | SessionError::Transport(T::Timeout(_))
        | SessionError::Io(_)
        | SessionError::StartupTimeout(_) => (
            FuzzClass::Disconnected,
            CallOutcome::Disconnected,
            Duration::ZERO,
            format!("server closed the connection after raw payload (likely crashed): {err}"),
        ),
        // A raw-frame rejection normally has the raw request's id (or null),
        // so the following liveness probe surfaces it as a mismatched
        // response. Preserve and inspect that error instead of throwing its
        // classification away at the id check.
        SessionError::MismatchedErrorResponse { error, .. }
            if is_expected_rejection_code(error.code) =>
        {
            (
                FuzzClass::ProtocolError,
                CallOutcome::ExpectedRejection,
                Duration::ZERO,
                format!(
                    "server explicitly rejected raw malformed frame with JSON-RPC code {}",
                    error.code
                ),
            )
        }
        SessionError::MismatchedErrorResponse { error, .. } => (
            FuzzClass::ServerError,
            CallOutcome::ServerError,
            Duration::ZERO,
            format!(
                "server returned unexpected JSON-RPC error {} after raw payload: {}",
                error.code, error.message
            ),
        ),
        // A normal result for the raw frame means the malformed input was
        // accepted. Liveness is not enough to make that protocol bug green.
        SessionError::MismatchedSuccessResponse { .. } | SessionError::ResponseShape(_) => (
            FuzzClass::Accepted,
            CallOutcome::Malformed,
            Duration::ZERO,
            format!("server accepted raw malformed frame and returned a success result: {err}"),
        ),
        // Matched structured errors are uncommon on this poisoned stream, but
        // keep the same explicit expected-rejection boundary.
        SessionError::Server(obj) if is_expected_rejection_code(obj.code) => (
            FuzzClass::ProtocolError,
            CallOutcome::ExpectedRejection,
            Duration::ZERO,
            format!(
                "server explicitly rejected raw malformed frame with JSON-RPC code {}",
                obj.code
            ),
        ),
        SessionError::Server(obj) => (
            FuzzClass::ServerError,
            CallOutcome::ServerError,
            Duration::ZERO,
            format!(
                "server returned unexpected JSON-RPC error {} after raw payload: {}",
                obj.code, obj.message
            ),
        ),
        // Legacy/programmatically constructed mismatch variants do not retain
        // the response payload. Fail closed because we cannot prove whether
        // the raw input was rejected or accepted.
        SessionError::IdMismatch { .. } | SessionError::InvalidResponseId { .. } => (
            FuzzClass::Other,
            CallOutcome::Malformed,
            Duration::ZERO,
            format!("ambiguous mismatched response after raw malformed frame: {err}"),
        ),
        // Producing bytes is not enough for a healthy result when those bytes
        // are themselves invalid JSON. Surface the malformed response instead
        // of allowing liveness alone to hide a protocol failure.
        SessionError::Json(_) => (
            FuzzClass::ParseError,
            CallOutcome::Malformed,
            Duration::ZERO,
            format!("server emitted malformed JSON after raw fuzz payload: {err}"),
        ),
        SessionError::InvalidJsonRpcVersion { .. } => (
            FuzzClass::ParseError,
            CallOutcome::Malformed,
            Duration::ZERO,
            format!("server emitted a non-2.0 JSON-RPC response after raw fuzz payload: {err}"),
        ),
        // Anything else is neither an explicit rejection nor reliable
        // liveness evidence.
        _ => (
            FuzzClass::Other,
            CallOutcome::ServerError,
            Duration::ZERO,
            format!("unexpected probe failure after raw malformed frame: {err}"),
        ),
    }
}

/// Replace the poisoned session with a fresh one from the factory. Returns
/// `true` (stop) only when the respawn fails.
async fn respawn(
    session: &mut Session,
    factory: &SessionFactory,
    outcome: &mut ScenarioOutcome,
    iter: u32,
) -> bool {
    match factory.spawn().await {
        Ok(fresh) => {
            // Replace first so the borrowed slot always contains a usable
            // session, then explicitly close/kill/reap the poisoned one.
            // Drop-only kill requests cannot prove lifecycle completion and
            // must not become an unreported false-green.
            let poisoned = std::mem::replace(session, fresh);
            teardown::shutdown_session(
                poisoned,
                outcome,
                format!("fuzzer poisoned session iter={iter}"),
            )
            .await;
            false
        }
        Err(e) => {
            outcome.error_count += 1;
            outcome.notes.push(format!(
                "fuzzer: could not respawn after raw payload at iter={iter} ({e}); stopping"
            ));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_probe_err_maps_transport_close_to_disconnected() {
        use mcp_loadtest_protocol::transport::TransportError;
        let (class, kind, _dur, _note) =
            classify_probe_err(&SessionError::Transport(TransportError::Closed));
        assert_eq!(class, FuzzClass::Disconnected);
        assert_eq!(kind, CallOutcome::Disconnected);
    }

    #[test]
    fn classify_probe_err_fails_closed_for_ambiguous_legacy_id_mismatch() {
        let (class, kind, _dur, _note) = classify_probe_err(&SessionError::IdMismatch {
            expected: 5,
            got: 1,
        });
        assert_eq!(class, FuzzClass::Other);
        assert_eq!(kind, CallOutcome::Malformed);

        let (class, kind, _dur, _note) = classify_probe_err(&SessionError::InvalidResponseId {
            expected: 5,
            got: serde_json::Value::Null,
        });
        assert_eq!(class, FuzzClass::Other);
        assert_eq!(kind, CallOutcome::Malformed);
    }

    #[test]
    fn classify_probe_err_keeps_malformed_response_unexpected() {
        let json_error = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("fixture must be invalid JSON");
        let (class, kind, _dur, _note) = classify_probe_err(&SessionError::Json(json_error));
        assert_eq!(class, FuzzClass::ParseError);
        assert_eq!(kind, CallOutcome::Malformed);
    }

    #[test]
    fn classify_probe_err_flags_success_for_raw_frame_as_acceptance() {
        let shape_error = serde_json::from_value::<Vec<String>>(serde_json::json!({"ok": true}))
            .expect_err("fixture must not match the requested result shape");
        let (class, kind, _dur, _note) =
            classify_probe_err(&SessionError::ResponseShape(shape_error));
        assert_eq!(class, FuzzClass::Accepted);
        assert_eq!(kind, CallOutcome::Malformed);

        let (class, kind, _dur, _note) =
            classify_probe_err(&SessionError::MismatchedSuccessResponse {
                expected: 5,
                got: serde_json::json!(1),
                result: serde_json::json!({"tools": []}),
            });
        assert_eq!(class, FuzzClass::Accepted);
        assert_eq!(kind, CallOutcome::Malformed);
    }

    #[test]
    fn classify_probe_err_keeps_internal_error_unexpected() {
        use mcp_loadtest_protocol::jsonrpc::ErrorObject;

        for code in [-32700, -32600, -32601, -32602] {
            let (class, kind, _, _) = classify_probe_err(&SessionError::MismatchedErrorResponse {
                expected: 5,
                got: serde_json::Value::Null,
                error: ErrorObject {
                    code,
                    message: "expected rejection".to_owned(),
                    data: None,
                },
            });
            assert_eq!(class, FuzzClass::ProtocolError, "code {code}");
            assert_eq!(kind, CallOutcome::ExpectedRejection, "code {code}");
        }

        let (class, kind, _, _) = classify_probe_err(&SessionError::MismatchedErrorResponse {
            expected: 5,
            got: serde_json::Value::Null,
            error: ErrorObject {
                code: -32603,
                message: "internal error".to_owned(),
                data: None,
            },
        });
        assert_eq!(class, FuzzClass::ServerError);
        assert_eq!(kind, CallOutcome::ServerError);
    }
}
