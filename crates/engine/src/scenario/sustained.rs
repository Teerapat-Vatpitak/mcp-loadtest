//! `sustained` scenario — constant load against one session for a fixed duration.
//!
//! See [`crate::scenario`] for the trait surface and [DESIGN.md §8][1] for the
//! conceptual description.
//!
//! # M5: pattern-driven loop
//!
//! As of M5 the loop is a thin wrapper over the [`pattern`] engine: the
//! legacy `tool` + `args` config folds into a one-element
//! `Pattern::single_call` list; `run_patterns` drives caller-provided
//! weighted patterns with the same `concurrent` + `duration` knobs.
//!
//! # M8: real concurrency via a session pool
//!
//! The `Scenario` trait still hands `drive` one `&mut Session` (locked
//! surface), so concurrency comes from *inside* the scenario: when
//! [`RunContext::session_factory`] is `Some` (always true under
//! `Run::execute`) **and** `concurrent > 1`, `drive` spawns `concurrent`
//! fresh sessions through `crate::scenario::pool::drive_pooled` and runs
//! the same pattern loop on each — one tokio task per session, every handle
//! joined. RPS/latency then reflect a genuine N-client rate. The borrowed
//! `&mut Session` stays **idle** on that path: it cannot move into worker
//! tasks (they need owned, `'static` sessions), and a special-cased borrowed
//! "worker 0" would force a heterogeneous loop for zero measurement gain.
//!
//! Without a factory (bare [`RunContext::new`], e.g. direct library use or
//! tests) or with `concurrent <= 1`, the loop runs **sequentially** on the
//! provided session and the outcome notes disclose that `concurrent` was not
//! multiplexed. Trade-offs (per-worker child processes vs. the process
//! sampler, N-spawn start-up cost) are recorded in ADR 0017.
//!
//! [1]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#8-built-in-scenarios

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Value, json};
use tokio::task::yield_now;

use crate::scenario::pattern::{self, Pattern};
use crate::scenario::{RunContext, Scenario, ScenarioOutcome, pool, teardown};
use mcp_loadtest_protocol::Session;

/// Sustained constant-load scenario.
///
/// Drives `session` in a tight loop for `duration`, calling `tool` with `args`
/// on every iteration, until either the duration elapses or
/// `ctx.cancel_token` fires.
///
/// To run weighted multi-step patterns instead of a single tool, see
/// `run_patterns`.
pub struct Sustained {
    /// Concurrency target. **Real** when the [`RunContext`] carries a
    /// session factory and this is `> 1`: a pool of `concurrent` fresh
    /// sessions, one worker task per session (see module docs). Otherwise
    /// the loop falls back to driving the single provided [`Session`]
    /// sequentially and says so in the outcome notes.
    pub concurrent: u32,
    /// How long to keep driving load.
    pub duration: Duration,
    /// Tool to invoke on every iteration.
    pub tool: String,
    /// Arguments JSON for `tool`.
    pub args: Value,
}

#[async_trait]
impl Scenario for Sustained {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        // Legacy single-tool path: fold the (tool, args) pair into a one-step
        // pattern so the loop body stays uniform with the multi-pattern path.
        let patterns = vec![Pattern::single_call(self.tool.clone(), self.args.clone())];
        run_patterns(self.concurrent, self.duration, &patterns, session, ctx).await
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "Sustained",
            "description": "Constant-rate load over a fixed duration.",
            "properties": {
                "concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Target concurrency. Real session pool when the run provides a session factory (Run::execute always does); sequential fallback on one session otherwise."
                },
                "duration": {
                    "type": "string",
                    "description": "Total run duration as a humantime string (e.g. \"30s\", \"1m\")."
                },
                "tool": {
                    "type": "string",
                    "description": "MCP tool name to invoke on every iteration."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to `tool`."
                },
                "patterns": {
                    "type": "array",
                    "description": "Optional weighted multi-step patterns (used via run_patterns).",
                    "items": { "type": "object" }
                }
            },
            "required": ["concurrent", "duration", "tool", "args"]
        })
    }

    fn name(&self) -> &'static str {
        "sustained"
    }
}

/// Drive the sustained loop against a list of weighted patterns.
///
/// Same semantics as [`Sustained::drive`] except each iteration picks a
/// [`Pattern`] by weighted-random selection (see [`pattern::pick`]) and runs
/// all of its steps with the pattern's configured think-time + error policy.
///
/// `concurrent` and `duration` mirror [`Sustained`]'s knobs. With a session
/// factory and `concurrent > 1`, this uses the same real N-session pool as
/// [`Sustained::drive`]; otherwise it drives the borrowed session
/// sequentially and discloses an unmet concurrency request in the notes.
///
/// Returns the same [`ScenarioOutcome`] shape as `drive`. Per-call metrics
/// are recorded into `ctx.metrics` exactly the same way — every step in
/// every pattern shows up in the outcome breakdown.
pub(crate) async fn run_patterns(
    concurrent: u32,
    duration: Duration,
    patterns: &[Pattern],
    session: &mut Session,
    ctx: &RunContext,
) -> ScenarioOutcome {
    if concurrent == 0 {
        return invalid_plan("concurrent must be >= 1");
    }
    if duration.is_zero() {
        return invalid_plan("duration must be > 0");
    }
    if concurrent > 1 && ctx.session_factory.is_some() {
        // The borrowed `session` stays idle on the pooled path: it cannot be
        // moved into `'static` worker tasks. Run::execute shuts it down after
        // the scenario returns.
        return drive_pooled_patterns(concurrent, duration, patterns.to_vec(), ctx).await;
    }
    run_loop(concurrent, duration, patterns, session, ctx).await
}

fn invalid_plan(message: &str) -> ScenarioOutcome {
    ScenarioOutcome {
        error_count: 1,
        notes: vec![format!("sustained: invalid plan — {message}")],
        ..ScenarioOutcome::default()
    }
}

/// Pooled path: spawn `workers` fresh sessions via the context's factory and
/// run [`drive_until_deadline`] — the *same* loop body as the sequential
/// path — on each, one task per session. See [`pool::drive_pooled`] for the
/// spawn-failure policy and outcome merging.
async fn drive_pooled_patterns(
    workers: u32,
    duration: Duration,
    patterns: Vec<Pattern>,
    ctx: &RunContext,
) -> ScenarioOutcome {
    let patterns = Arc::new(patterns);
    pool::drive_pooled(ctx, workers, move |_idx, mut session, worker_ctx| {
        let patterns = Arc::clone(&patterns);
        async move {
            // `drive_pooled` resets `run_start` to the local coordinated
            // traffic instant after every local session is ready. Pool
            // startup and distributed SSH preparation therefore never eat
            // into the requested measurement window.
            let deadline = worker_ctx.run_start + duration;
            let mut outcome =
                drive_until_deadline(deadline, &patterns, &mut session, &worker_ctx).await;
            teardown::shutdown_session(session, &mut outcome, "sustained pooled worker").await;
            outcome
        }
    })
    .await
}

/// Sequential entry shared by [`Sustained::drive`] (fallback path) and
/// [`run_patterns`]: drives [`drive_until_deadline`] on the one provided
/// session and discloses the unmet `concurrent` request in the notes.
async fn run_loop(
    concurrent: u32,
    duration: Duration,
    patterns: &[Pattern],
    session: &mut Session,
    ctx: &RunContext,
) -> ScenarioOutcome {
    let traffic_start = match &ctx.traffic_start_gate {
        Some(gate) => {
            let (target_protocol_version, tool_inventory_hash) = ctx
                .target_identity
                .clone()
                .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
            let start = tokio::select! {
                result = gate.ready_and_start_at(crate::scenario::TrafficReadiness {
                    live_workers: 1,
                    requested_workers: 1,
                    target_protocol_version,
                    tool_inventory_hash,
                }) => result,
                _ = ctx.cancel_token.cancelled() => {
                    Err(crate::scenario::TrafficStartError::Cancelled)
                }
            };
            match start {
                Ok(start) => start,
                Err(error) => {
                    return ScenarioOutcome {
                        incomplete_worker_count: 1,
                        notes: vec![format!("sustained: traffic start gate failed: {error}")],
                        ..ScenarioOutcome::default()
                    };
                }
            }
        }
        None => Instant::now(),
    };
    if traffic_start > Instant::now() {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(traffic_start)) => {}
            _ = ctx.cancel_token.cancelled() => {
                return ScenarioOutcome {
                    incomplete_worker_count: 1,
                    notes: vec![
                        "sustained: cancelled while waiting for coordinated traffic start".into()
                    ],
                    ..ScenarioOutcome::default()
                };
            }
        }
    }
    let deadline = traffic_start + duration;
    let mut outcome = drive_until_deadline(deadline, patterns, session, ctx).await;
    if concurrent > 1 {
        outcome.notes.insert(
            0,
            format!(
                "sustained: sequential on one session; concurrent={concurrent} requested but \
                 not multiplexed on this path (pooled execution needs a session_factory on \
                 the RunContext; Run::execute attaches one automatically)"
            ),
        );
    }
    outcome
}

/// The actual call loop: drive weighted patterns on `session` until
/// `deadline` or cancellation. Runs unchanged on the sequential path and
/// inside each pooled worker, so both paths share identical
/// `classify_error` / `is_terminal_error` semantics.
async fn drive_until_deadline(
    deadline: Instant,
    patterns: &[Pattern],
    session: &mut Session,
    ctx: &RunContext,
) -> ScenarioOutcome {
    let mut total_calls: u64 = 0;
    let mut successful_calls: u64 = 0;
    let mut error_count: u64 = 0;

    let mut notes = Vec::new();

    if patterns.is_empty() {
        notes.push("sustained: empty patterns list — nothing to drive".to_owned());
        return ScenarioOutcome {
            notes,
            ..ScenarioOutcome::default()
        };
    }

    // `Send`-friendly RNG seeded from entropy. `ThreadRng` would be ideal but
    // is `!Send`, and `Scenario::drive` returns a `Send` future via
    // `async_trait`. `StdRng` (ChaCha12) is fine for weighted-random picks.
    let mut rng = match ctx.rng_seed {
        Some(seed) => StdRng::seed_from_u64(seed),
        None => StdRng::from_os_rng(),
    };

    loop {
        // Check cancellation / deadline *before* each iteration.
        if ctx.is_cancelled() {
            notes.push("sustained: cancelled via ctx.cancel_token".to_owned());
            break;
        }
        if Instant::now() >= deadline {
            break;
        }

        let stats = pattern::execute(patterns, session, ctx, &mut rng).await;
        total_calls += stats.steps_attempted;
        successful_calls += stats.steps_succeeded;
        error_count += stats.errors;

        // No work attempted → picker had no candidate (e.g. all weights
        // non-positive). Bail rather than spinning.
        if stats.steps_attempted == 0 {
            notes.push("sustained: pattern picker returned no candidate".to_owned());
            break;
        }

        // Transport-fatal error inside the pattern (closed pipe, IO, startup
        // timeout). Further calls would all fail; fast-exit and let the
        // caller observe via metrics.
        if stats.terminal_error {
            notes.push(format!(
                "sustained: terminal error after {total_calls} calls (transport closed)"
            ));
            break;
        }

        // Yield to the runtime so cancellation has a fair chance even when
        // the server is fast enough that we'd otherwise busy-spin.
        // `yield_now` instead of `sleep(Duration::ZERO)` to avoid
        // registering a no-op reactor timer per iteration.
        yield_now().await;
    }

    ScenarioOutcome {
        total_calls,
        successful_calls,
        error_count,
        notes,
        ..ScenarioOutcome::default()
    }
}
