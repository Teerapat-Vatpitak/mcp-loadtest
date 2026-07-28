//! Inner phase driver for the [`crate::scenario::spike`] scenario.
//!
//! Split out of `spike.rs` so the orchestration and the per-phase loop can
//! evolve independently while staying under the 300-line file convention.

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::yield_now;
use tokio::time::sleep;

use crate::scenario::{RunContext, ScenarioOutcome};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;

/// Drive one phase of the spike against the shared session for `phase_duration`.
///
/// Returns `true` if a terminal transport error fired and the outer scenario
/// should stop driving further phases. Cancellation is signalled via
/// `ctx.is_cancelled()` and observed by the caller after this returns.
#[expect(
    clippy::too_many_arguments,
    reason = "8 args is one over clippy's default 7-arg limit; bundling them into a PhaseSpec struct only this private helper uses adds a type for no gain — keep flat"
)]
pub(super) async fn drive_phase(
    session: &mut Session,
    ctx: &RunContext,
    outcome: &mut ScenarioOutcome,
    phase_name: &str,
    concurrent: u32,
    phase_duration: Duration,
    tool: &str,
    args: &Value,
) -> bool {
    let phase_started = Instant::now();
    let phase_deadline = phase_started + phase_duration;
    let mut phase_calls: u64 = 0;
    let mut phase_errors: u64 = 0;

    'phase: while Instant::now() < phase_deadline {
        if ctx.is_cancelled() {
            outcome.notes.push(format!(
                "spike.{phase_name}: cancelled via ctx.cancel_token"
            ));
            break;
        }

        // Each "tick" drives `concurrent` sequential calls (the M5 stand-in
        // for true parallelism — see module docs).
        let mut tick_iters: u32 = 0;
        while tick_iters < concurrent && Instant::now() < phase_deadline {
            if ctx.is_cancelled() {
                break 'phase;
            }

            let call_start = Instant::now();
            let call_fut = session.call_tool(tool, args);
            let result = tokio::select! {
                biased;
                _ = ctx.cancel_token.cancelled() => {
                    let elapsed = call_start.elapsed();
                    // `record_tool` already bumps the global aggregate
                    // internally — recording both would double-count.
                    ctx.metrics.record_tool(tool, elapsed, CallOutcome::Cancelled);
                    outcome.total_calls += 1;
                    outcome.error_count += 1;
                    phase_calls += 1;
                    phase_errors += 1;
                    break 'phase;
                }
                r = call_fut => r,
            };
            let elapsed = call_start.elapsed();
            outcome.total_calls += 1;
            phase_calls += 1;
            tick_iters += 1;

            match result {
                Ok(result) => {
                    let kind = if super::super::is_logical_tool_error(&result) {
                        outcome.error_count += 1;
                        phase_errors += 1;
                        CallOutcome::ServerError
                    } else {
                        outcome.successful_calls += 1;
                        CallOutcome::Success
                    };
                    ctx.metrics.record_tool(tool, elapsed, kind);
                }
                Err(err) => {
                    outcome.error_count += 1;
                    phase_errors += 1;
                    let kind = super::super::classify_error(&err);
                    ctx.metrics.record_tool(tool, elapsed, kind);
                    if super::super::is_terminal_error(&err) {
                        outcome.notes.push(format!(
                            "spike.{phase_name}: terminal error after {phase_calls} calls: {err}"
                        ));
                        return true;
                    }
                }
            }

            // Yield so cancellation can preempt fast servers.
            yield_now().await;
        }

        // If we burned through this tick faster than the phase deadline,
        // briefly yield to the runtime so the busy-loop has a fair chance
        // to observe cancellation between ticks. We do NOT sleep for the
        // remainder of the phase here — phase_duration bounds the total
        // wall-clock; ticks pack densely inside it.
        if Instant::now() >= phase_deadline {
            break;
        }
        // A tiny sleep keeps us from spinning when the server is faster
        // than scheduler tick granularity. Skip if cancellation fired.
        tokio::select! {
            biased;
            _ = ctx.cancel_token.cancelled() => break,
            _ = sleep(Duration::from_millis(1)) => {}
        }
    }

    outcome.notes.push(format!(
        "spike.{phase_name}: ran {phase_calls} calls ({phase_errors} errors) in {:?} \
         at iterations-per-tick={concurrent}",
        phase_started.elapsed(),
    ));

    false
}
