//! HDR-histogram-backed latency recorder.
//!
//! Internal implementation detail of `Recorder`. We use a sharded design:
//! N shards each holding `Mutex<Histogram<u64>>`. Workers pick a shard via
//! a thread-local counter to spread contention. At snapshot time we merge
//! all shards into one histogram for percentile reads.
//!
//! Microsecond resolution. Values larger than `MAX_LATENCY_US` are clamped
//! before recording so we never panic on extreme outliers.

// reason: the SHARD thread-local below uses `= const { ... }` already, which
// is the fix `missing_const_for_thread_local` documents — but clippy 1.95
// still flags it (a known false positive on `Cell<Option<_>>` init).
#![allow(clippy::missing_const_for_thread_local)]

use std::sync::Mutex;

use hdrhistogram::Histogram;

/// Maximum recordable latency (1 hour in microseconds).
///
/// Anything larger is clamped to this value. Hangs/deadlocks that exceed
/// this are still counted in the outcome counters; only the latency
/// histogram is clamped.
pub(crate) const MAX_LATENCY_US: u64 = 3_600_000_000;

/// Number of histogram shards. Picked to be larger than typical worker
/// counts (16) on a developer laptop while keeping merge cheap.
pub(crate) const NUM_SHARDS: usize = 16;

/// Sharded histogram. Each shard is a separate mutex so concurrent
/// workers contend on different locks.
pub(crate) struct ShardedHistogram {
    shards: [Mutex<Histogram<u64>>; NUM_SHARDS],
}

impl ShardedHistogram {
    /// Construct a fresh sharded histogram with microsecond resolution
    /// (1µs..=1h, 3 significant digits → ~2KB per shard).
    pub(crate) fn new() -> Self {
        // SAFETY (no `unsafe` used): we initialize the array via array::from_fn.
        let shards = std::array::from_fn(|_| {
            // 3 significant digits is the hdrhistogram sweet spot — enough
            // precision for percentiles, cheap memory.
            Mutex::new(
                Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)
                    .expect("histogram bounds are valid by construction"),
            )
        });
        Self { shards }
    }

    /// Record a single value into one shard. Picks a shard by a cheap hash
    /// of the current thread id so workers spread across shards.
    #[inline]
    pub(crate) fn record(&self, value_us: u64) {
        let value = value_us.clamp(1, MAX_LATENCY_US);
        let shard_idx = shard_index_for_current_thread();
        // Best-effort record: if a shard's mutex is poisoned (panicking in
        // another thread held it), just drop the sample rather than
        // propagate panic into the hot path.
        if let Ok(mut h) = self.shards[shard_idx].lock() {
            // record_correct accepts u64 already; saturate again just in case.
            let _ = h.record(value);
        }
    }

    /// Merge all shards into a fresh histogram. Used by `Recorder::snapshot`.
    pub(crate) fn merged(&self) -> Histogram<u64> {
        let mut merged = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)
            .expect("histogram bounds are valid by construction");
        for shard in &self.shards {
            if let Ok(h) = shard.lock() {
                // `add` returns Result; on error (range mismatch) it's a programmer
                // bug since we constructed all with the same bounds.
                merged
                    .add(&*h)
                    .expect("histogram bounds match by construction");
            }
        }
        merged
    }
}

impl Default for ShardedHistogram {
    fn default() -> Self {
        Self::new()
    }
}

// Per-thread shard cache. Module-level `#![allow]` at the top of this file
// silences the clippy 1.95 false positive — see the comment up there.
thread_local! {
    static SHARD: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Cheap shard picker. We use the thread id's hash modulo NUM_SHARDS.
/// The hashing is done once per thread via a thread-local cell.
fn shard_index_for_current_thread() -> usize {
    SHARD.with(|c| {
        if let Some(idx) = c.get() {
            return idx;
        }
        // Hash the thread id once and cache.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        let idx = (hasher.finish() as usize) % NUM_SHARDS;
        c.set(Some(idx));
        idx
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_clamps_zero_to_one() {
        let h = ShardedHistogram::new();
        h.record(0);
        let m = h.merged();
        assert_eq!(m.len(), 1);
        assert!(m.min() >= 1);
    }

    #[test]
    fn record_clamps_above_max() {
        let h = ShardedHistogram::new();
        h.record(u64::MAX);
        let m = h.merged();
        // We must have recorded the sample (clamp succeeded — no panic).
        assert_eq!(m.len(), 1);
        // hdrhistogram bucket precision (3 sig digits) means the reported
        // max can be slightly larger than the recorded value. The point is
        // it didn't panic — the raw u64::MAX was clamped before record().
        // Reported max should still be in the same order of magnitude as
        // MAX_LATENCY_US (well below u64::MAX).
        assert!(m.max() < u64::MAX / 1024);
    }

    #[test]
    fn merged_aggregates_all_shards() {
        let h = ShardedHistogram::new();
        for i in 1..=1000u64 {
            h.record(i);
        }
        let m = h.merged();
        assert_eq!(m.len(), 1000);
    }

    /// Smoke-test that `shard_index_for_current_thread` actually distributes
    /// across multiple shards rather than collapsing every thread to shard 0.
    ///
    /// The current implementation hashes `ThreadId` via `DefaultHasher`. Tokio
    /// worker `ThreadId`s tend to be consecutive integers, so a poor hash
    /// (or a regression to e.g. `id_as_u64 % NUM_SHARDS` on a Tokio runtime)
    /// could land every worker on one shard and silently hide all contention
    /// the sharding is supposed to spread. Asserting "at least half the shards
    /// see samples" + "no single shard owns >60%" catches that without being
    /// flaky on the hash function we happen to use today.
    #[test]
    fn shard_index_distributes_across_threads() {
        use std::sync::{Arc, Mutex};

        const N_THREADS: usize = 50;

        let samples: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::with_capacity(N_THREADS)));

        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let samples = Arc::clone(&samples);
                std::thread::spawn(move || {
                    let idx = shard_index_for_current_thread();
                    samples.lock().unwrap().push(idx);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("worker thread panicked");
        }

        let samples = samples.lock().unwrap();
        assert_eq!(samples.len(), N_THREADS);

        // Count distinct shard indices touched.
        let mut counts = [0usize; NUM_SHARDS];
        for &i in samples.iter() {
            assert!(i < NUM_SHARDS, "shard idx {} out of range", i);
            counts[i] += 1;
        }
        let distinct = counts.iter().filter(|c| **c > 0).count();
        assert!(
            distinct >= NUM_SHARDS / 2,
            "expected at least {} distinct shards across {} threads, got {} (counts={:?})",
            NUM_SHARDS / 2,
            N_THREADS,
            distinct,
            counts,
        );

        // No single shard should hoard more than 60% of the samples — that
        // would mean the hash function is effectively constant.
        let max_share = *counts.iter().max().unwrap();
        let max_fraction = max_share as f64 / N_THREADS as f64;
        assert!(
            max_fraction <= 0.60,
            "one shard absorbed {:.0}% of samples (>60%): counts={:?}",
            max_fraction * 100.0,
            counts,
        );
    }
}
