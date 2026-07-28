//! Session-pool driver — real N-worker concurrency for load scenarios.
//!
//! [`drive_pooled`] spawns N fresh sessions through
//! [`RunContext::session_factory`] (concurrently), runs one tokio task per
//! successfully-spawned session executing a caller-supplied worker loop,
//! joins **every** task, and merges the per-worker [`ScenarioOutcome`]s into
//! one. See ADR 0017 for why pooling lives inside the scenario rather than
//! in the `Scenario` trait or the orchestrator.
//!
//! The locked `Scenario::drive(&self, &mut Session, &RunContext)` surface is
//! untouched: the borrowed session a scenario receives cannot move into
//! worker tasks (tasks need owned, `'static` sessions), so pooled scenarios
//! leave it idle and drive factory-spawned sessions instead.
//!
//! # Caller contract
//!
//! - The `per_worker` closure receives `(worker_index, owned Session,
//!   worker RunContext)` and returns that worker's whole async loop. The
//!   worker context shares the caller's cancel token, metrics recorder and
//!   hang/grace thresholds, so the loop body can be byte-for-byte the same
//!   code as the sequential fallback path (`sustained` does exactly this).
//! - The worker loop owns its session including teardown: shut it down
//!   (bounded) before returning and propagate lifecycle uncertainty through
//!   `ScenarioOutcome::teardown_failure_count`.
//! - Callers should check `ctx.session_factory.is_some()` first and use
//!   their sequential path otherwise; if called without a factory this
//!   helper returns an empty outcome with an explanatory note (no panic).
//!
//! # Spawn-failure policy
//!
//! Sessions that fail to spawn are reported (`error_count` +1 each, plus a
//! note) and the pool proceeds with the survivors so evidence is retained.
//! The outcome also records `incomplete_worker_count`, which makes every
//! partial pool fail closed at [`mcp_loadtest_core::report::Report::passed`].
//! A session that spawned and joined successfully still counts as incomplete
//! when its worker returned without exercising even one call; otherwise calls
//! from another worker or phase could hide the unexercised concurrency slot.
//! If **all** spawns fail the merged outcome carries `error_count ==
//! requested` and an explanatory note — never a panic. Spawn failures are
//! *not* counted in `total_calls` (no call was ever issued). The summary note
//! `pool: N workers (M requested)` always discloses the real pool size.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::scenario::{RunContext, ScenarioOutcome};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::session::SessionError;

/// Spawn `requested` fresh sessions via `ctx.session_factory` and drive one
/// `per_worker` loop per successfully-spawned session. Returns the merged
/// outcome. See the module docs for the caller contract and failure policy.
pub(crate) async fn drive_pooled<F, Fut>(
    ctx: &RunContext,
    requested: u32,
    per_worker: F,
) -> ScenarioOutcome
where
    F: Fn(u32, Session, RunContext) -> Fut,
    Fut: Future<Output = ScenarioOutcome> + Send + 'static,
{
    let mut outcome = ScenarioOutcome::default();

    let Some(factory) = ctx.session_factory.clone() else {
        outcome.incomplete_worker_count = u64::from(requested);
        outcome.notes.push(
            "pool: no session_factory on RunContext — nothing driven (callers should take \
             their sequential fallback path instead)"
                .to_owned(),
        );
        return outcome;
    };
    if requested == 0 {
        outcome
            .notes
            .push("pool: 0 workers requested — nothing driven".to_owned());
        return outcome;
    }
    if ctx.is_cancelled() {
        outcome.incomplete_worker_count = u64::from(requested);
        outcome
            .notes
            .push("pool: cancelled before any session was spawned".to_owned());
        return outcome;
    }

    // Phase 1: spawn all sessions concurrently. Once started, each
    // constructor is allowed to finish its own bounded handshake so
    // cancellation never drops a half-constructed stdio session and relies
    // on kill-on-drop without proving process reap. A cancellation that
    // arrives during this phase is observed by the worker immediately after
    // construction; that worker then performs explicit bounded teardown.
    let mut spawns: JoinSet<(u32, Result<Session, SessionError>)> = JoinSet::new();
    for idx in 0..requested {
        let factory = factory.clone();
        spawns.spawn(async move { (idx, factory.spawn().await) });
    }

    let mut sessions: Vec<(u32, Session)> = Vec::new();
    while let Some(joined) = spawns.join_next().await {
        match joined {
            Ok((idx, Ok(session))) => sessions.push((idx, session)),
            Ok((idx, Err(e))) => {
                outcome.error_count += 1;
                ctx.metrics
                    .record(Duration::ZERO, crate::scenario::classify_error(&e));
                outcome
                    .notes
                    .push(format!("pool: worker {idx} failed to spawn: {e}"));
            }
            // Join failure = the spawn task itself panicked/was aborted.
            // Neither happens in lib code, but stay total: count + note.
            Err(e) => {
                outcome.error_count += 1;
                ctx.metrics.record(Duration::ZERO, CallOutcome::Crash);
                outcome.notes.push(format!("pool: spawn task failed: {e}"));
            }
        }
    }
    outcome.incomplete_worker_count = u64::from(requested).saturating_sub(sessions.len() as u64);
    if sessions.is_empty() {
        outcome.notes.push(format!(
            "pool: all {requested} session spawns failed — nothing driven"
        ));
        return outcome;
    }
    // Deterministic worker numbering in notes regardless of join order.
    sessions.sort_by_key(|(idx, _)| *idx);
    let spawned = sessions.len();

    // Phase 2: one task per live session, all handles kept in the JoinSet
    // and every one awaited below (cancellation is observed *inside* each
    // worker loop via the shared token in its worker context).
    let mut workers: JoinSet<(u32, ScenarioOutcome)> = JoinSet::new();
    // Every worker is spawned before this retained watch value flips to
    // `true`. Besides making pool start-up deterministic, this is the
    // synchronization gate RaceCheck relies on to put identical calls
    // in flight together rather than merely scheduling them one by one.
    // `watch` (instead of Barrier) remains correct when only a subset of
    // requested sessions spawned successfully.
    let (start_tx, start_rx) = watch::channel(false);
    for (idx, session) in sessions {
        let fut = per_worker(idx, session, worker_context(ctx));
        let mut start_rx = start_rx.clone();
        workers.spawn(async move {
            let _ = start_rx.wait_for(|released| *released).await;
            (idx, fut.await)
        });
    }
    // Release all live workers only after every worker task exists. Receivers
    // created above retain the value, so even a task first polled after this
    // send observes `true` immediately.
    let _ = start_tx.send(true);
    while let Some(joined) = workers.join_next().await {
        match joined {
            Ok((idx, worker_outcome)) => merge_worker_outcome(&mut outcome, idx, worker_outcome),
            Err(e) => {
                outcome.error_count += 1;
                outcome.incomplete_worker_count += 1;
                ctx.metrics.record(Duration::ZERO, CallOutcome::Crash);
                outcome.notes.push(format!("pool: worker task failed: {e}"));
            }
        }
    }

    outcome
        .notes
        .push(format!("pool: {spawned} workers ({requested} requested)"));
    outcome
}

/// Per-worker [`RunContext`]: shares the caller's cancel token, (Arc-backed)
/// metrics recorder and hang/grace thresholds. Deliberately carries **no**
/// session factory — workers drive the one session they were handed.
fn worker_context(ctx: &RunContext) -> RunContext {
    RunContext::new(
        ctx.run_start,
        ctx.cancel_token.clone(),
        ctx.metrics.clone(),
        ctx.hang_threshold,
        ctx.grace_period,
    )
}

/// Fold one worker's outcome into the pool total: sum every counter, append
/// `hung_for_ms`, and keep the worker's notes attributable by prefixing them
/// with its index.
///
/// A joined worker that made no call is itself incomplete. Worker-provided
/// incompleteness is retained (for example, from a nested pool), and the
/// zero-call guard marks this outer worker separately. Spawn and join deficits
/// never reach this function and remain counted exactly once by
/// [`drive_pooled`].
fn merge_worker_outcome(into: &mut ScenarioOutcome, idx: u32, from: ScenarioOutcome) {
    let zero_call_worker = from.total_calls == 0;

    into.total_calls += from.total_calls;
    into.successful_calls += from.successful_calls;
    into.hang_count += from.hang_count;
    into.deadlock_count += from.deadlock_count;
    into.error_count += from.error_count;
    into.divergence_count += from.divergence_count;
    into.incomplete_worker_count += from.incomplete_worker_count;
    into.teardown_failure_count += from.teardown_failure_count;
    into.hung_for_ms.extend(from.hung_for_ms);
    into.notes
        .extend(from.notes.into_iter().map(|n| format!("worker {idx}: {n}")));
    if zero_call_worker {
        into.incomplete_worker_count += 1;
        into.notes.push(format!(
            "pool: worker {idx} completed without exercising a call"
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use mcp_loadtest_core::metrics::Recorder;
    use mcp_loadtest_protocol::SessionFactory;
    use mcp_loadtest_protocol::transport::TransportError;

    fn bare_ctx() -> RunContext {
        RunContext::new(
            Instant::now(),
            CancellationToken::new(),
            Recorder::new(),
            Duration::from_millis(200),
            Duration::from_millis(500),
        )
    }

    /// Factory whose every spawn fails; counts invocations.
    fn failing_factory(counter: Arc<AtomicU32>) -> SessionFactory {
        SessionFactory::new(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<Session, SessionError>(SessionError::Transport(TransportError::Closed))
            }
        })
    }

    async fn noop_worker(_idx: u32, _session: Session, _ctx: RunContext) -> ScenarioOutcome {
        ScenarioOutcome::default()
    }

    #[tokio::test]
    async fn no_factory_returns_explanatory_note() {
        let ctx = bare_ctx();
        let outcome = drive_pooled(&ctx, 4, noop_worker).await;
        assert_eq!(outcome.total_calls, 0);
        assert_eq!(outcome.error_count, 0);
        assert!(
            outcome
                .notes
                .iter()
                .any(|n| n.contains("no session_factory")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn zero_workers_requested_is_a_noop() {
        let calls = Arc::new(AtomicU32::new(0));
        let ctx = bare_ctx().with_session_factory(failing_factory(calls.clone()));
        let outcome = drive_pooled(&ctx, 0, noop_worker).await;
        assert_eq!(outcome.total_calls, 0);
        assert_eq!(outcome.error_count, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "factory must not be hit");
        assert!(
            outcome
                .notes
                .iter()
                .any(|n| n.contains("0 workers requested")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn all_spawn_failures_reported_without_panic() {
        let calls = Arc::new(AtomicU32::new(0));
        let ctx = bare_ctx().with_session_factory(failing_factory(calls.clone()));
        let outcome = drive_pooled(&ctx, 3, noop_worker).await;
        assert_eq!(outcome.total_calls, 0, "got {outcome:?}");
        assert_eq!(outcome.incomplete_worker_count, 3, "got {outcome:?}");
        assert_eq!(
            outcome.error_count, 3,
            "one error per failed spawn: {outcome:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(
            outcome
                .notes
                .iter()
                .any(|n| n.contains("all 3 session spawns failed")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn pre_fired_cancellation_skips_spawning_entirely() {
        let calls = Arc::new(AtomicU32::new(0));
        let ctx = bare_ctx().with_session_factory(failing_factory(calls.clone()));
        ctx.cancel_token.cancel();
        let outcome = drive_pooled(&ctx, 4, noop_worker).await;
        assert_eq!(outcome.total_calls, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "factory must not be hit");
        assert!(
            outcome.notes.iter().any(|n| n.contains("cancelled")),
            "got {outcome:?}"
        );
    }

    #[test]
    fn merge_sums_counters_and_prefixes_notes() {
        let mut total = ScenarioOutcome {
            total_calls: 1,
            successful_calls: 1,
            ..Default::default()
        };
        let worker = ScenarioOutcome {
            total_calls: 5,
            successful_calls: 3,
            hang_count: 1,
            deadlock_count: 1,
            error_count: 2,
            divergence_count: 1,
            incomplete_worker_count: 1,
            teardown_failure_count: 1,
            notes: vec!["terminal error after 5 calls".to_owned()],
            hung_for_ms: vec![1234],
        };
        merge_worker_outcome(&mut total, 7, worker);
        assert_eq!(total.total_calls, 6);
        assert_eq!(total.successful_calls, 4);
        assert_eq!(total.hang_count, 1);
        assert_eq!(total.deadlock_count, 1);
        assert_eq!(total.error_count, 2);
        assert_eq!(total.divergence_count, 1);
        assert_eq!(total.incomplete_worker_count, 1);
        assert_eq!(total.teardown_failure_count, 1);
        assert_eq!(total.hung_for_ms, vec![1234]);
        assert_eq!(total.notes, vec!["worker 7: terminal error after 5 calls"]);
    }

    #[test]
    fn merge_marks_zero_call_worker_and_propagates_nested_incompleteness() {
        let mut total = ScenarioOutcome::default();
        merge_worker_outcome(&mut total, 2, ScenarioOutcome::default());

        assert_eq!(total.total_calls, 0);
        assert_eq!(total.incomplete_worker_count, 1);
        assert_eq!(
            total.notes,
            vec!["pool: worker 2 completed without exercising a call"]
        );

        let already_incomplete = ScenarioOutcome {
            incomplete_worker_count: 1,
            notes: vec!["cancelled before call".to_owned()],
            ..Default::default()
        };
        merge_worker_outcome(&mut total, 3, already_incomplete);

        assert_eq!(
            total.incomplete_worker_count, 3,
            "retain the nested deficit and mark the outer zero-call worker"
        );
        assert_eq!(
            total.notes,
            vec![
                "pool: worker 2 completed without exercising a call",
                "worker 3: cancelled before call",
                "pool: worker 3 completed without exercising a call",
            ]
        );
    }
}
