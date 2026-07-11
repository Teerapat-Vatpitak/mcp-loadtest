//! Microbenchmarks for the latency-histogram hot path.
//!
//! `ShardedHistogram` is `pub(crate)` in `mcp_loadtest_core::metrics::histogram`, so we
//! exercise it indirectly through `Recorder::record(_, CallOutcome::Success)`
//! — every `Success` recording routes through `ShardedHistogram::record` plus
//! one extra atomic increment (the outcome counter). The atomic add is < 1ns
//! in practice on modern x86, so the result tracks the histogram cost closely.
//!
//! Two cases:
//!
//! - `histogram_record_single` — one thread, mixed micros values so the
//!   hdrhistogram bucket math doesn't degenerate to a single bucket.
//! - `histogram_record_contended_8` — eight threads via `std::thread::scope`,
//!   each doing 100K records, total time charged to 800K calls. Surfaces
//!   mutex contention across the 16 shards.

#![allow(missing_docs)]

use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use mcp_loadtest_core::metrics::{CallOutcome, Recorder};

fn bench_histogram_record_single(c: &mut Criterion) {
    let rec = Recorder::new();
    let mut group = c.benchmark_group("histogram_record_single");
    group.throughput(Throughput::Elements(1));
    group.bench_function("mixed_micros", |b| {
        let mut i: u64 = 0;
        b.iter(|| {
            // Vary the value so we touch several histogram buckets.
            let micros = 1 + (i % 100_000);
            rec.record(
                black_box(Duration::from_micros(micros)),
                black_box(CallOutcome::Success),
            );
            i = i.wrapping_add(1);
        });
    });
    group.finish();
}

fn bench_histogram_record_contended_8(c: &mut Criterion) {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 100_000;
    const TOTAL: usize = THREADS * PER_THREAD;

    c.bench_function("histogram_record_contended_8", |b| {
        // Criterion's `iter_custom` lets us report wall-clock per logical call
        // (total elapsed / TOTAL) which is what we want for a contention bench.
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                // Fresh recorder each outer iter so shards start cold and we
                // don't drag prior samples into the histogram (avoids
                // unbounded memory growth across iter counts).
                let rec = Recorder::new();
                let start = Instant::now();
                std::thread::scope(|s| {
                    for t in 0..THREADS {
                        let rec = rec.clone();
                        s.spawn(move || {
                            // Use the thread index to seed the values so the
                            // shard hash distributes work — we want contention
                            // on the *shard mutex*, not on cache lines for
                            // identical inputs.
                            let base = (t as u64) * 1000;
                            for i in 0..PER_THREAD as u64 {
                                let micros = 1 + ((base + i) % 100_000);
                                rec.record(
                                    black_box(Duration::from_micros(micros)),
                                    black_box(CallOutcome::Success),
                                );
                            }
                        });
                    }
                });
                let elapsed = start.elapsed();
                total += elapsed / (TOTAL as u32);
            }
            total
        });
    });
}

criterion_group!(
    benches,
    bench_histogram_record_single,
    bench_histogram_record_contended_8
);
criterion_main!(benches);
