//! Pooled step driver for the [`crate::scenario::ramp`] scenario (M8).
//!
//! Engaged by [`super::Ramp::drive`] whenever the [`RunContext`] carries a
//! session factory. Every step drives its declared concurrency level as a
//! real pool via [`pool::drive_pooled`]: `concurrent` fresh sessions, one
//! worker task per session, all joined before the step ends.
//!
//! Sessions are **not** reused across steps — `drive_pooled` spawns and
//! tears down per invocation, and stretching it to keep sessions alive
//! across calls would contort its API. Each step therefore pays its own
//! spawn cost, and that cost counts against `step_duration` (the step
//! deadline is anchored *before* the spawn phase, mirroring `sustained`).
//!
//! The per-step delta-[`Recorder`] contract from the sequential path is
//! preserved: workers record every call into a fresh per-step recorder
//! (alongside the shared `ctx.metrics`), so the breaking-point detector
//! still sees exactly one step's data per observation — never cumulative
//! numbers. Spawn failures never reach the recorder (no call was issued);
//! they surface as pool notes + `error_count`, exactly like `sustained`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::yield_now;
use tokio::time::sleep;

use super::Ramp;
use crate::breaking_point::BreakingPointDetector;
use crate::scenario::{RunContext, ScenarioOutcome, pool, teardown};
use mcp_loadtest_core::metrics::{CallOutcome, Recorder};
use mcp_loadtest_protocol::Session;

/// Drive the full ramp with one session pool per step. See the module docs
/// for the cost model and the per-step recorder contract.
pub(super) async fn drive_ramp_pooled(ramp: &Ramp, ctx: &RunContext) -> ScenarioOutcome {
    let mut outcome = ScenarioOutcome::default();
    let mut detector = ramp.breaking_point.clone().map(BreakingPointDetector::new);

    outcome.notes.push(
        "ramp: pooled — each step spawns its declared concurrency as fresh sessions via \
         the run's session factory; sessions are not reused across steps, and per-step \
         spawn cost counts against step_duration"
            .to_owned(),
    );

    let tool: Arc<str> = Arc::from(ramp.tool.as_str());
    let args = Arc::new(ramp.args.clone());

    let mut concurrent = ramp.from_concurrent;
    let mut early_break = false;

    while concurrent <= ramp.to_concurrent {
        if ctx.is_cancelled() {
            outcome
                .notes
                .push("ramp: cancelled via ctx.cancel_token".to_owned());
            break;
        }

        // Fresh recorder per step so the detector sees this step's delta
        // only — same contract as the sequential path. Recorder is
        // Arc-backed, so clones inside the worker closures share it.
        let step_recorder = Recorder::new();
        // Anchored before the spawn phase: pool spin-up cost counts against
        // the step window, mirroring `sustained`'s pooled path.
        let step_deadline = Instant::now() + ramp.step_duration;

        let step_outcome = {
            let tool = Arc::clone(&tool);
            let args = Arc::clone(&args);
            let step_recorder = step_recorder.clone();
            pool::drive_pooled(ctx, concurrent, move |_idx, mut session, worker_ctx| {
                let tool = Arc::clone(&tool);
                let args = Arc::clone(&args);
                let step_recorder = step_recorder.clone();
                async move {
                    let mut outcome = worker_call_loop(
                        step_deadline,
                        &tool,
                        &args,
                        &step_recorder,
                        &mut session,
                        &worker_ctx,
                    )
                    .await;
                    teardown::shutdown_session(session, &mut outcome, "ramp pooled worker").await;
                    outcome
                }
            })
            .await
        };

        // Spawn failures bump `error_count` without `total_calls` (no call
        // was ever issued), and a cancelled/empty pool reports zero of both
        // — so this combination means the step had no usable session at all.
        let no_usable_sessions = step_outcome.total_calls == 0 && step_outcome.error_count > 0;
        merge_step(&mut outcome, concurrent, step_outcome);

        if no_usable_sessions {
            outcome.notes.push(format!(
                "ramp: step concurrent={concurrent} drove no calls (all session spawns \
                 failed); aborting ramp"
            ));
            break;
        }

        // Hold out the rest of the step window so step boundaries stay
        // aligned even when every worker exited early (terminal errors).
        // Healthy workers run until the deadline, so this is usually ~zero.
        if !ctx.is_cancelled() {
            let remaining = step_deadline.saturating_duration_since(Instant::now());
            if remaining > Duration::ZERO {
                tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => {}
                    _ = sleep(remaining) => {}
                }
            }
        }

        if let Some(det) = detector.as_mut() {
            det.observe(concurrent, step_recorder.snapshot());
            let report = det.breaking_point();
            if let Some(broke_at) = report.broke_at_concurrent {
                let trigger = report
                    .trigger
                    .unwrap_or_else(|| "unknown trigger".to_owned());
                outcome.notes.push(format!(
                    "ramp: breaking point detected at concurrent={broke_at}; \
                     last_known_good={:?}; {trigger}",
                    report.last_known_good
                ));
                early_break = true;
                break;
            }
        }

        concurrent = concurrent.saturating_add(ramp.step_increment);
    }

    if !early_break && detector.is_some() {
        outcome
            .notes
            .push("ramp: completed full ramp without breaking point".to_owned());
    }

    outcome
}

/// One pooled worker's call loop: drive `tool` until the step deadline,
/// recording every call into both the shared run metrics and this step's
/// delta recorder. Same `classify_error` / `is_terminal_error` semantics as
/// the sequential step loop in `ramp.rs`.
async fn worker_call_loop(
    deadline: Instant,
    tool: &str,
    args: &Value,
    step_recorder: &Recorder,
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
                step_recorder.record(elapsed, CallOutcome::Cancelled);
                outcome.total_calls += 1;
                outcome.error_count += 1;
                break;
            }
            r = call_fut => r,
        };
        let elapsed = call_start.elapsed();
        outcome.total_calls += 1;

        match result {
            Ok(result) => {
                let kind = if crate::scenario::is_logical_tool_error(&result) {
                    outcome.error_count += 1;
                    CallOutcome::ServerError
                } else {
                    outcome.successful_calls += 1;
                    CallOutcome::Success
                };
                ctx.metrics.record_tool(tool, elapsed, kind);
                step_recorder.record(elapsed, kind);
            }
            Err(err) => {
                outcome.error_count += 1;
                let kind = crate::scenario::classify_error(&err);
                ctx.metrics.record_tool(tool, elapsed, kind);
                step_recorder.record(elapsed, kind);
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

/// Fold one step's merged pool outcome into the ramp total, keeping the pool
/// notes attributable to their step level.
fn merge_step(into: &mut ScenarioOutcome, concurrent: u32, from: ScenarioOutcome) {
    into.total_calls += from.total_calls;
    into.successful_calls += from.successful_calls;
    into.hang_count += from.hang_count;
    into.deadlock_count += from.deadlock_count;
    into.error_count += from.error_count;
    into.divergence_count += from.divergence_count;
    into.incomplete_worker_count += from.incomplete_worker_count;
    into.teardown_failure_count += from.teardown_failure_count;
    into.hung_for_ms.extend(from.hung_for_ms);
    into.notes.extend(
        from.notes
            .into_iter()
            .map(|n| format!("ramp[c={concurrent}]: {n}")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_step_sums_counters_and_prefixes_notes_with_level() {
        let mut total = ScenarioOutcome {
            total_calls: 2,
            successful_calls: 2,
            ..Default::default()
        };
        let step = ScenarioOutcome {
            total_calls: 7,
            successful_calls: 5,
            hang_count: 1,
            deadlock_count: 1,
            error_count: 2,
            divergence_count: 1,
            incomplete_worker_count: 1,
            teardown_failure_count: 1,
            notes: vec!["pool: 3 workers (4 requested)".to_owned()],
            hung_for_ms: vec![999],
        };
        merge_step(&mut total, 4, step);
        assert_eq!(total.total_calls, 9);
        assert_eq!(total.successful_calls, 7);
        assert_eq!(total.hang_count, 1);
        assert_eq!(total.deadlock_count, 1);
        assert_eq!(total.error_count, 2);
        assert_eq!(total.divergence_count, 1);
        assert_eq!(total.incomplete_worker_count, 1);
        assert_eq!(total.teardown_failure_count, 1);
        assert_eq!(total.hung_for_ms, vec![999]);
        assert_eq!(
            total.notes,
            vec!["ramp[c=4]: pool: 3 workers (4 requested)"]
        );
    }
}
