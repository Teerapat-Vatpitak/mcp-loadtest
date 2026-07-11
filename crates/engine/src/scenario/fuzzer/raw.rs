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

use crate::scenario::{RunContext, ScenarioOutcome};
use mcp_loadtest_core::fuzz_report::{FuzzClass, FuzzFinding};
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

    let (class, kind, dur, note) = if let Err(e) = session.raw_send(&bytes).await {
        // The write itself failed: the child is already gone.
        (
            FuzzClass::Disconnected,
            CallOutcome::Disconnected,
            Duration::ZERO,
            format!("raw send failed (server gone before the write completed): {e}"),
        )
    } else {
        // The write went out, but `raw_send` reads nothing back, so we can't
        // yet tell whether the server rejected-and-survived or crashed. Probe
        // once with a normal request, bounded by hang_threshold + grace_period.
        // Framing is desynced now, so we read the probe purely as alive/dead:
        // *any* bytes back (even an id mismatch or a parse error on a stale
        // line) prove the child is still serving.
        let budget = ctx.hang_threshold + ctx.grace_period;
        match tokio::time::timeout(budget, session.list_tools()).await {
            // No reply within budget: the malformed frame wedged the server.
            Err(_) => (
                FuzzClass::Deadlock,
                CallOutcome::Deadlock,
                budget,
                format!(
                    "server stopped responding after raw payload (no reply within {}ms — likely wedged on malformed framing)",
                    budget.as_millis()
                ),
            ),
            // Clean reply: server shrugged off the malformed frame — healthy.
            Ok(Ok(_tools)) => (
                FuzzClass::ProtocolError,
                CallOutcome::ProtocolError,
                Duration::ZERO,
                "server survived raw malformed frame and kept serving (healthy)".to_string(),
            ),
            // Errored reply: distinguish "server gone" from "alive but desynced".
            Ok(Err(e)) => classify_probe_err(&e),
        }
    };

    // Bump the matching outcome counter (mirrors the typed-path arms).
    if class == FuzzClass::Deadlock {
        outcome.deadlock_count += 1;
        outcome.hung_for_ms.push(dur.as_millis());
    } else {
        outcome.error_count += 1;
    }
    ctx.metrics.record(dur, kind);
    findings.push(FuzzFinding {
        payload: payload.label().to_string(),
        class,
        code: None,
        note,
    });

    // The session is poisoned no matter the reaction — respawn before the
    // caller's loop reuses it.
    respawn(session, factory, outcome, iter).await
}

/// Interpret a probe error after a raw send as alive-vs-dead. The response
/// *content* can't be trusted (it may be a stale reply to the raw frame), only
/// whether the child produced bytes at all.
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
        // A structured server error, a parse error on a stale line, or an id
        // mismatch all mean the child answered — it survived.
        _ => (
            FuzzClass::ProtocolError,
            CallOutcome::ProtocolError,
            Duration::ZERO,
            format!("server survived raw malformed frame (desynced reply observed: {err})"),
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
            // Assigning through the `&mut` drops the old session here; its
            // transport is `kill_on_drop`, reaping a wedged/crashed child.
            *session = fresh;
            false
        }
        Err(e) => {
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
    fn classify_probe_err_maps_id_mismatch_to_survived() {
        // A desynced reply (stale line -> id mismatch) means the server is
        // still up: healthy survival, not a disconnect.
        let (class, kind, _dur, _note) = classify_probe_err(&SessionError::IdMismatch {
            expected: 5,
            got: 1,
        });
        assert_eq!(class, FuzzClass::ProtocolError);
        assert_eq!(kind, CallOutcome::ProtocolError);
    }
}
