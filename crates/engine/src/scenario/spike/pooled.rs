//! Pooled phase driver for the [`crate::scenario::spike`] scenario (M8).
//!
//! Engaged by [`super::Spike::drive`] whenever the [`RunContext`] carries a
//! session factory. Each phase (warmup → spike → cooldown) drives its
//! declared concurrency as a real pool via [`pool::drive_pooled`]: fresh
//! sessions, one worker task per session.
//!
//! **Phase boundaries are clean by construction**: `drive_pooled` joins
//! every worker task before returning, so no burst worker can bleed into the
//! cooldown phase. Sessions are not reused across phases (the pool helper
//! spawns/tears down per invocation); each phase pays its own spawn cost,
//! which counts against the phase duration (deadline anchored before the
//! spawn phase, mirroring `sustained`).
//!
//! Unlike the sequential path — where a terminal transport error kills the
//! single shared session and later phases *cannot* run — pooled phases get
//! fresh sessions, so a worker dying mid-phase does not stop the plan. Only
//! a phase that spawned **no usable session at all** (every spawn failed)
//! skips the remaining phases, since they would fail identically.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::yield_now;

use super::Spike;
use crate::scenario::{RunContext, ScenarioOutcome, pool};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;

/// Per-worker graceful-shutdown budget. Bounded so one wedged server can't
/// stall the phase join; on timeout the dropped `Session` is reaped via
/// `kill_on_drop` (same policy as `sustained`'s pooled path).
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Drive all three phases, each through its own session pool. Takes the
/// outcome already carrying the validation notes; appends phase notes and
/// the final plan summary exactly like the sequential path.
pub(super) async fn drive_pooled_phases(
    spike: &Spike,
    ctx: &RunContext,
    mut outcome: ScenarioOutcome,
) -> ScenarioOutcome {
    outcome.notes.push(
        "spike: pooled — each phase spawns its declared concurrency as fresh sessions via \
         the run's session factory; spawn cost counts against the phase duration, and all \
         of a phase's workers are joined before the next phase starts"
            .to_owned(),
    );

    let tool: Arc<str> = Arc::from(spike.tool.as_str());
    let args = Arc::new(spike.args.clone());

    let phases: [(&str, u32, Duration); 3] = [
        ("warmup", spike.baseline_concurrent, spike.warmup),
        ("spike", spike.spike_concurrent, spike.spike_duration),
        ("cooldown", spike.baseline_concurrent, spike.cooldown),
    ];

    for (phase_name, workers, phase_duration) in phases {
        if ctx.is_cancelled() {
            outcome.notes.push(format!(
                "spike.{phase_name}: cancelled via ctx.cancel_token"
            ));
            break;
        }
        let stop = drive_phase_pooled(
            ctx,
            &mut outcome,
            phase_name,
            workers,
            phase_duration,
            &tool,
            &args,
        )
        .await;
        if stop {
            break;
        }
    }

    super::push_summary(&mut outcome, spike);
    outcome
}

/// Drive one phase as a pool of `workers` fresh sessions until the phase
/// deadline. Returns `true` if the remaining phases should be skipped (the
/// phase had no usable session at all — every spawn failed).
async fn drive_phase_pooled(
    ctx: &RunContext,
    outcome: &mut ScenarioOutcome,
    phase_name: &str,
    workers: u32,
    phase_duration: Duration,
    tool: &Arc<str>,
    args: &Arc<Value>,
) -> bool {
    let phase_started = Instant::now();
    // Anchored before the spawn phase: pool spin-up cost counts against the
    // phase window, mirroring `sustained`'s pooled path.
    let phase_deadline = phase_started + phase_duration;

    let phase_outcome = {
        let tool = Arc::clone(tool);
        let args = Arc::clone(args);
        pool::drive_pooled(ctx, workers, move |_idx, mut session, worker_ctx| {
            let tool = Arc::clone(&tool);
            let args = Arc::clone(&args);
            async move {
                let outcome =
                    worker_call_loop(phase_deadline, &tool, &args, &mut session, &worker_ctx).await;
                shutdown_worker(session).await;
                outcome
            }
        })
        .await
    };

    let phase_calls = phase_outcome.total_calls;
    let phase_errors = phase_outcome.error_count;
    // Spawn failures bump `error_count` without `total_calls` (no call was
    // ever issued) — this combination means no worker got a session.
    let no_usable_sessions = phase_calls == 0 && phase_errors > 0;

    merge_phase(outcome, phase_name, phase_outcome);
    outcome.notes.push(format!(
        "spike.{phase_name}: ran {phase_calls} calls ({phase_errors} errors) in {:?} \
         at workers={workers}",
        phase_started.elapsed(),
    ));

    if no_usable_sessions {
        outcome.notes.push(format!(
            "spike.{phase_name}: no usable sessions (all spawns failed); skipping \
             remaining phases"
        ));
        return true;
    }
    false
}

/// One pooled worker's call loop: drive `tool` until the phase deadline.
/// Same `classify_error` / `is_terminal_error` semantics as the sequential
/// phase loop in `spike/phase.rs`.
async fn worker_call_loop(
    deadline: Instant,
    tool: &str,
    args: &Value,
    session: &mut Session,
    ctx: &RunContext,
) -> ScenarioOutcome {
    let mut outcome = ScenarioOutcome::default();

    loop {
        if ctx.is_cancelled() {
            outcome
                .notes
                .push("cancelled via ctx.cancel_token".to_owned());
            break;
        }
        if Instant::now() >= deadline {
            break;
        }

        let call_start = Instant::now();
        let call_fut = session.call_tool(tool, args);
        let result = tokio::select! {
            biased;
            _ = ctx.cancel_token.cancelled() => {
                let elapsed = call_start.elapsed();
                ctx.metrics.record_tool(tool, elapsed, CallOutcome::Cancelled);
                outcome.total_calls += 1;
                outcome.error_count += 1;
                break;
            }
            r = call_fut => r,
        };
        let elapsed = call_start.elapsed();
        outcome.total_calls += 1;

        match result {
            Ok(_) => {
                outcome.successful_calls += 1;
                ctx.metrics.record_tool(tool, elapsed, CallOutcome::Success);
            }
            Err(err) => {
                outcome.error_count += 1;
                let kind = crate::scenario::classify_error(&err);
                ctx.metrics.record_tool(tool, elapsed, kind);
                if crate::scenario::is_terminal_error(&err) {
                    outcome.notes.push(format!("terminal error: {err}"));
                    break;
                }
            }
        }

        // Yield so cancellation can preempt fast servers.
        yield_now().await;
    }

    outcome
}

/// Polite bounded teardown of one worker session; on timeout/error the child
/// is still reaped via `kill_on_drop` when `session` drops.
async fn shutdown_worker(session: Session) {
    match tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, session.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "spike: pooled worker shutdown errored");
        }
        Err(_) => {
            tracing::warn!(
                "spike: pooled worker shutdown exceeded {WORKER_SHUTDOWN_TIMEOUT:?}; \
                 child reaped via kill_on_drop"
            );
        }
    }
}

/// Fold one phase's merged pool outcome into the scenario total, keeping the
/// pool notes attributable to their phase.
fn merge_phase(into: &mut ScenarioOutcome, phase_name: &str, from: ScenarioOutcome) {
    into.total_calls += from.total_calls;
    into.successful_calls += from.successful_calls;
    into.hang_count += from.hang_count;
    into.deadlock_count += from.deadlock_count;
    into.error_count += from.error_count;
    into.hung_for_ms.extend(from.hung_for_ms);
    into.notes.extend(
        from.notes
            .into_iter()
            .map(|n| format!("spike.{phase_name}: {n}")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_phase_sums_counters_and_prefixes_notes_with_phase() {
        let mut total = ScenarioOutcome {
            total_calls: 1,
            successful_calls: 1,
            ..Default::default()
        };
        let phase = ScenarioOutcome {
            total_calls: 4,
            successful_calls: 3,
            hang_count: 0,
            deadlock_count: 0,
            error_count: 1,
            notes: vec!["pool: 2 workers (2 requested)".to_owned()],
            hung_for_ms: vec![],
        };
        merge_phase(&mut total, "spike", phase);
        assert_eq!(total.total_calls, 5);
        assert_eq!(total.successful_calls, 4);
        assert_eq!(total.error_count, 1);
        assert_eq!(
            total.notes,
            vec!["spike.spike: pool: 2 workers (2 requested)"]
        );
    }
}
