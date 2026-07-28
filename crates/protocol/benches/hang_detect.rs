//! Microbenchmark for [`hang_detect`] wrap overhead.
//!
//! `hang_detect` wraps every per-call future in the scenarios. Measuring the
//! overhead when the inner future resolves immediately tells us the cost the
//! wrapper itself adds — separately from the underlying call.
//!
//! Two cases:
//!
//! - `hang_detect_immediate_ok` — wraps a future that resolves to
//!   `Ok(CallToolResult)` with a generous threshold so the timer arm never
//!   fires. The result is the pure `select!` + future-pin overhead.
//! - `hang_detect_immediate_err` — same as above but the inner future
//!   resolves to `Err(SessionError)` so we exercise the error path through
//!   `HangOutcome::Err`. Should be in the same ballpark.

// Criterion's `criterion_group!` macro expands to a `pub fn` that the
// missing_docs lint flags. The benches are not part of the public API.
#![allow(missing_docs)]

use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use mcp_loadtest_protocol::mcp::CallToolResult;
use mcp_loadtest_protocol::{HangOutcome, SessionError, hang_detect};
use tokio::runtime::Runtime;

fn empty_result() -> CallToolResult {
    CallToolResult {
        meta: None,
        content: Vec::new(),
        is_error: false,
        structured_content: None,
    }
}

fn bench_hang_detect_immediate_ok(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime must build");
    c.bench_function("hang_detect_immediate_ok", |b| {
        b.iter(|| {
            rt.block_on(async {
                let fut = async { Ok::<CallToolResult, SessionError>(empty_result()) };
                let outcome = hang_detect(
                    fut,
                    black_box(Duration::from_secs(60)),
                    black_box(Duration::from_secs(60)),
                )
                .await;
                debug_assert!(matches!(outcome, HangOutcome::Ok { .. }));
                black_box(outcome);
            });
        });
    });
}

fn bench_hang_detect_immediate_err(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime must build");
    c.bench_function("hang_detect_immediate_err", |b| {
        b.iter(|| {
            rt.block_on(async {
                let fut = async {
                    Err::<CallToolResult, SessionError>(SessionError::IdMismatch {
                        expected: 1,
                        got: 2,
                    })
                };
                let outcome = hang_detect(
                    fut,
                    black_box(Duration::from_secs(60)),
                    black_box(Duration::from_secs(60)),
                )
                .await;
                debug_assert!(matches!(outcome, HangOutcome::Err { .. }));
                black_box(outcome);
            });
        });
    });
}

criterion_group!(
    benches,
    bench_hang_detect_immediate_ok,
    bench_hang_detect_immediate_err
);
criterion_main!(benches);
