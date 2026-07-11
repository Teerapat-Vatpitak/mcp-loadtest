//! The pattern executor: [`pick`] (weighted-random pattern selection),
//! [`execute`] (drive one iteration's steps), and the per-iteration
//! [`StepStats`] tally.
//!
//! Split out of `pattern/mod.rs` to keep that file within the size convention.

use rand::Rng;
use rand::seq::IndexedRandom;
use tokio::time::sleep;

use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;

use super::{ErrorBehavior, Pattern};
use crate::scenario::{RunContext, classify_error, is_terminal_error};

/// Stats from a single [`execute`] call (one pattern iteration).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StepStats {
    /// Steps the executor attempted to run.
    pub steps_attempted: u64,
    /// Steps that returned a successful response.
    pub steps_succeeded: u64,
    /// Steps that returned an error.
    pub errors: u64,
    /// `true` if a step hit a transport-fatal error (closed pipe, IO error,
    /// startup timeout) — the calling scenario should stop driving this
    /// session and break its outer loop.
    pub terminal_error: bool,
}

/// Pick a [`Pattern`] from `patterns` by weighted-random selection.
///
/// Returns `None` if `patterns` is empty or all weights are non-positive.
pub fn pick<'a, R: Rng + ?Sized>(patterns: &'a [Pattern], rng: &mut R) -> Option<&'a Pattern> {
    if patterns.is_empty() {
        return None;
    }
    // SAFETY: we don't unwrap on the choose result; if all weights are
    // non-positive `choose_weighted` returns an error and we fall through to
    // `None`.
    patterns.choose_weighted(rng, |p| p.weight.max(0.0)).ok()
}

/// Drive one iteration: pick a pattern, run its steps with think-time and
/// error handling, record per-step metrics into `ctx.metrics`.
///
/// Returns the per-iteration tally; the calling scenario sums these into its
/// final [`crate::scenario::ScenarioOutcome`].
///
/// The caller is responsible for the outer loop and for honouring
/// `ctx.cancel_token` between iterations. `execute` itself respects
/// cancellation between steps so a hung server can't pin the loop past
/// shutdown.
pub async fn execute<R: Rng + ?Sized>(
    patterns: &[Pattern],
    session: &mut Session,
    ctx: &RunContext,
    rng: &mut R,
) -> StepStats {
    let mut stats = StepStats::default();
    let Some(pattern) = pick(patterns, rng) else {
        return stats;
    };

    for (idx, step) in pattern.steps.iter().enumerate() {
        if ctx.is_cancelled() {
            break;
        }

        // Think-time between steps (skip before step 0). Race against
        // cancellation so we exit promptly on shutdown.
        if idx > 0 && !pattern.think_time.is_zero() {
            tokio::select! {
                biased;
                _ = ctx.cancel_token.cancelled() => break,
                () = sleep(pattern.think_time) => {}
            }
            if ctx.is_cancelled() {
                break;
            }
        }

        stats.steps_attempted += 1;

        let call_start = std::time::Instant::now();
        let call_fut = session.call_tool(&step.tool, &step.args);
        let result = tokio::select! {
            biased;
            _ = ctx.cancel_token.cancelled() => {
                let elapsed = call_start.elapsed();
                ctx.metrics.record_tool(&step.tool, elapsed, CallOutcome::Cancelled);
                stats.errors += 1;
                break;
            }
            r = call_fut => r,
        };

        let elapsed = call_start.elapsed();
        match result {
            Ok(_) => {
                stats.steps_succeeded += 1;
                ctx.metrics
                    .record_tool(&step.tool, elapsed, CallOutcome::Success);
            }
            Err(err) => {
                stats.errors += 1;
                ctx.metrics
                    .record_tool(&step.tool, elapsed, classify_error(&err));
                if is_terminal_error(&err) {
                    // Transport-fatal: signal the caller to stop driving
                    // this session entirely; further steps would all fail.
                    stats.terminal_error = true;
                    break;
                }
                if matches!(pattern.on_step_error, ErrorBehavior::Abort) {
                    break;
                }
                // Otherwise continue to the next step.
            }
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use serde_json::json;

    use super::*;

    #[test]
    fn pick_returns_none_on_empty_slice() {
        let mut rng = StdRng::seed_from_u64(0);
        let p: Vec<Pattern> = vec![];
        assert!(pick(&p, &mut rng).is_none());
    }

    #[test]
    fn pick_returns_some_for_single_pattern() {
        let mut rng = StdRng::seed_from_u64(0);
        let patterns = vec![Pattern::single_call("echo", json!({}))];
        let chosen = pick(&patterns, &mut rng).expect("should pick the only pattern");
        assert_eq!(chosen.name, "single:echo");
    }
}
