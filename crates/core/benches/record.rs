//! Microbenchmarks for `Recorder::record` and `Recorder::record_tool`.
//!
//! Backs the "Recorder::record < 50µs" claim in DESIGN.md §19 and the
//! corresponding CHANGELOG line. Run with `cargo bench --bench record`.
//!
//! Three cases:
//!
//! - `record_success_single_thread` — bare global path (one `AtomicU64` bump
//!   + one sharded histogram record). The baseline.
//! - `record_tool_first_sight` — every iter passes a fresh tool name so the
//!   per-tool map always hits the write-lock + insert slow path. Worst case.
//! - `record_tool_warm` — same tool name every iter (steady state). Read-lock
//!   + `Arc::clone`; this is the real-world cost once a run has been going.
//!
//! The `Recorder` is constructed *outside* `iter` so setup cost is not
//! charged. Each closure uses `black_box` to prevent the compiler from
//! constant-folding the measured call away.

#![allow(missing_docs)]

use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use mcp_loadtest_core::metrics::{CallOutcome, Recorder};

fn bench_record_success_single_thread(c: &mut Criterion) {
    let rec = Recorder::new();
    c.bench_function("record_success_single_thread", |b| {
        b.iter(|| {
            rec.record(
                black_box(Duration::from_micros(100)),
                black_box(CallOutcome::Success),
            );
        });
    });
}

fn bench_record_tool_first_sight(c: &mut Criterion) {
    // Use a fresh Recorder for each batch so the per-tool map starts empty;
    // each iter then inserts a new tool name (slow path: write-lock + insert).
    c.bench_function("record_tool_first_sight", |b| {
        let mut i: u64 = 0;
        let rec = Recorder::new();
        b.iter(|| {
            let name = format!("tool-{i}");
            rec.record_tool(
                black_box(&name),
                black_box(Duration::from_micros(100)),
                black_box(CallOutcome::Success),
            );
            i = i.wrapping_add(1);
        });
    });
}

fn bench_record_tool_warm(c: &mut Criterion) {
    let rec = Recorder::new();
    // Seed the per-tool map once so every measured call hits the fast path
    // (read-lock + `Arc::clone`).
    rec.record_tool("warm-tool", Duration::from_micros(1), CallOutcome::Success);
    c.bench_function("record_tool_warm", |b| {
        b.iter(|| {
            rec.record_tool(
                black_box("warm-tool"),
                black_box(Duration::from_micros(100)),
                black_box(CallOutcome::Success),
            );
        });
    });
}

criterion_group!(
    benches,
    bench_record_success_single_thread,
    bench_record_tool_first_sight,
    bench_record_tool_warm
);
criterion_main!(benches);
