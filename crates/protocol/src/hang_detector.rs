//! Per-call watchdog — distinguishes Success / Hang / Deadlock / Error.
//!
//! See DESIGN.md §15.1 for the algorithm spec.

use std::future::Future;
use std::time::{Duration, Instant};

use crate::mcp::CallToolResult;
use crate::session::SessionError;

/// Outcome of [`hang_detect`].
///
/// **Locked for M2.**
#[derive(Debug)]
#[non_exhaustive]
pub enum HangOutcome {
    /// Call returned within `hang_threshold`.
    Ok {
        /// Successful tool result returned by the server.
        result: CallToolResult,
        /// Wall-clock time the call took.
        duration: Duration,
    },
    /// Call exceeded `hang_threshold` but returned within `grace_period`.
    Slow {
        /// Successful tool result (eventually) returned by the server.
        result: CallToolResult,
        /// Total wall-clock time before the response arrived.
        duration: Duration,
    },
    /// Call exceeded `hang_threshold + grace_period`. Classified as deadlock.
    Deadlock {
        /// How long we waited before giving up.
        hung_for: Duration,
    },
    /// Call returned an error (server-side or transport).
    Err(SessionError),
}

/// Wrap a tool call with a hang watchdog.
///
/// Algorithm (DESIGN.md §15.1):
/// 1. Race `call_fut` against `hang_threshold`. If it returns first → `Ok` or `Err`.
/// 2. Otherwise the call is "hanging". Continue waiting up to `grace_period`.
/// 3. If the call completes during grace → `Slow`. If grace expires → `Deadlock`.
///
/// **Locked for M2.**
pub async fn hang_detect<F>(
    call_fut: F,
    hang_threshold: Duration,
    grace_period: Duration,
) -> HangOutcome
where
    F: Future<Output = Result<CallToolResult, SessionError>>,
{
    let start = Instant::now();
    let deadlock_threshold = hang_threshold.saturating_add(grace_period);

    // Pin the future so we can poll it across two `select!` blocks.
    tokio::pin!(call_fut);

    // Phase 1: race the call against `hang_threshold`.
    tokio::select! {
        biased;
        res = &mut call_fut => {
            let duration = start.elapsed();
            return match res {
                Ok(result) if duration < hang_threshold => {
                    HangOutcome::Ok { result, duration }
                }
                Ok(_) if duration >= deadlock_threshold => {
                    // A stalled executor can make the response future and
                    // both watchdog timers ready in the same poll. The
                    // future-first select must not turn an over-budget call
                    // into a false-green success.
                    HangOutcome::Deadlock { hung_for: duration }
                }
                Ok(result) => HangOutcome::Slow { result, duration },
                Err(e) => HangOutcome::Err(e),
            };
        }
        _ = tokio::time::sleep(hang_threshold) => {
            // Fall through to the grace-period wait.
        }
    }

    // Phase 2: the call is hanging. Wait up to `grace_period` for a late response.
    tokio::select! {
        biased;
        res = &mut call_fut => {
            let duration = start.elapsed();
            match res {
                Ok(_) if duration >= deadlock_threshold => {
                    HangOutcome::Deadlock { hung_for: duration }
                }
                Ok(result) => HangOutcome::Slow { result, duration },
                Err(e) => HangOutcome::Err(e),
            }
        }
        _ = tokio::time::sleep(grace_period) => {
            HangOutcome::Deadlock { hung_for: start.elapsed() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn ok_result() -> CallToolResult {
        CallToolResult {
            meta: None,
            content: Vec::new(),
            is_error: false,
            structured_content: None,
        }
    }

    #[tokio::test]
    async fn ok_returns_immediately() {
        let fut = async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok::<CallToolResult, SessionError>(ok_result())
        };
        let outcome =
            hang_detect(fut, Duration::from_millis(100), Duration::from_millis(500)).await;
        match outcome {
            HangOutcome::Ok { duration, .. } => {
                assert!(
                    duration < Duration::from_millis(100),
                    "expected fast Ok, took {duration:?}"
                );
            }
            other => panic!("expected HangOutcome::Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slow_returns_within_grace() {
        let fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<CallToolResult, SessionError>(ok_result())
        };
        let outcome = hang_detect(fut, Duration::from_millis(10), Duration::from_millis(200)).await;
        match outcome {
            HangOutcome::Slow { duration, .. } => {
                assert!(
                    duration >= Duration::from_millis(50),
                    "Slow duration should reflect total wait, got {duration:?}"
                );
                assert!(
                    duration < Duration::from_millis(210),
                    "Slow should have arrived inside grace, got {duration:?}"
                );
            }
            other => panic!("expected HangOutcome::Slow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn executor_stall_cannot_turn_an_over_threshold_call_into_ok() {
        let hang_threshold = Duration::from_millis(5);
        let grace_period = Duration::from_millis(100);
        let deadlock_threshold = hang_threshold + grace_period;
        let fut = async {
            // Simulate the runtime worker being unable to poll the watchdog
            // while the call itself occupies that poll. When select resumes,
            // both the threshold timer and this future are ready.
            std::thread::sleep(Duration::from_millis(30));
            Ok::<CallToolResult, SessionError>(ok_result())
        };
        let outcome = hang_detect(fut, hang_threshold, grace_period).await;
        match outcome {
            HangOutcome::Slow { duration, .. } => assert!(
                duration >= hang_threshold && duration < deadlock_threshold,
                "Slow must stay inside the grace budget: {duration:?}"
            ),
            HangOutcome::Deadlock { hung_for } => assert!(
                hung_for >= deadlock_threshold,
                "Deadlock must consume the threshold and grace budget: {hung_for:?}"
            ),
            other => panic!("elapsed wall time above the threshold must fail closed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn executor_stall_past_grace_fails_closed_as_deadlock() {
        let fut = async {
            std::thread::sleep(Duration::from_millis(40));
            Ok::<CallToolResult, SessionError>(ok_result())
        };
        let outcome = hang_detect(fut, Duration::from_millis(5), Duration::from_millis(10)).await;
        match outcome {
            HangOutcome::Deadlock { hung_for } => assert!(
                hung_for >= Duration::from_millis(15),
                "Deadlock must consume the threshold and grace budget: {hung_for:?}"
            ),
            other => {
                panic!("elapsed wall time beyond threshold + grace must be a deadlock: {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn deadlock_when_no_response() {
        // Future that never resolves — simulates deadlocked server.
        let fut = async {
            tokio::time::sleep(Duration::from_secs(999)).await;
            Ok::<CallToolResult, SessionError>(ok_result())
        };
        let outcome = hang_detect(fut, Duration::from_millis(10), Duration::from_millis(50)).await;
        match outcome {
            HangOutcome::Deadlock { hung_for } => {
                assert!(
                    hung_for >= Duration::from_millis(60),
                    "Deadlock should report >= hang_threshold + grace_period, got {hung_for:?}"
                );
            }
            other => panic!("expected HangOutcome::Deadlock, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn err_propagates() {
        let fut = async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Err::<CallToolResult, SessionError>(SessionError::Io(io::Error::other("boom")))
        };
        let outcome =
            hang_detect(fut, Duration::from_millis(100), Duration::from_millis(500)).await;
        match outcome {
            HangOutcome::Err(SessionError::Io(_)) => {}
            other => panic!("expected HangOutcome::Err(Io), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn err_propagates_during_grace() {
        // Fail after hang_threshold but before grace expires → still HangOutcome::Err.
        let fut = async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Err::<CallToolResult, SessionError>(SessionError::Transport(
                crate::transport::TransportError::Closed,
            ))
        };
        let outcome = hang_detect(fut, Duration::from_millis(10), Duration::from_millis(200)).await;
        match outcome {
            HangOutcome::Err(SessionError::Transport(crate::transport::TransportError::Closed)) => {
            }
            other => panic!("expected HangOutcome::Err(Transport(Closed)), got {other:?}"),
        }
    }
}
