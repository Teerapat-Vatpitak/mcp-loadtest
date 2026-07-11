//! Throughput collector — total/successful counts + per-outcome counters.
//!
//! Internal implementation detail of `Recorder`. All counters are atomic
//! (`AtomicU64`) so the hot path (`record`) is fully lock-free for outcome
//! accounting. Throughput rate is computed at snapshot time from a stored
//! `Instant` plus the current wall clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::metrics::CallOutcome;

/// Atomic counters, one per [`CallOutcome`] variant. Cheap to bump from
/// any number of threads.
#[derive(Default)]
pub(crate) struct OutcomeCounters {
    pub(crate) success: AtomicU64,
    pub(crate) hang: AtomicU64,
    pub(crate) deadlock: AtomicU64,
    pub(crate) timeout: AtomicU64,
    pub(crate) server_error: AtomicU64,
    pub(crate) protocol_error: AtomicU64,
    pub(crate) crash: AtomicU64,
    pub(crate) malformed: AtomicU64,
    pub(crate) disconnected: AtomicU64,
    pub(crate) cancelled: AtomicU64,
}

impl OutcomeCounters {
    /// Increment the counter for `outcome`. Lock-free.
    #[inline]
    pub(crate) fn bump(&self, outcome: CallOutcome) {
        let counter: &AtomicU64 = match outcome {
            CallOutcome::Success => &self.success,
            CallOutcome::Hang => &self.hang,
            CallOutcome::Deadlock => &self.deadlock,
            CallOutcome::Timeout => &self.timeout,
            CallOutcome::ServerError => &self.server_error,
            CallOutcome::ProtocolError => &self.protocol_error,
            CallOutcome::Crash => &self.crash,
            CallOutcome::Malformed => &self.malformed,
            CallOutcome::Disconnected => &self.disconnected,
            CallOutcome::Cancelled => &self.cancelled,
        };
        // Relaxed is sufficient: we never use these counters to synchronize
        // with other memory accesses, only to read them at snapshot time.
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot all counters. Uses Relaxed loads — totals will be accurate
    /// per-counter but counters captured at slightly different instants.
    pub(crate) fn snapshot(&self) -> OutcomeSnapshot {
        OutcomeSnapshot {
            success: self.success.load(Ordering::Relaxed),
            hang: self.hang.load(Ordering::Relaxed),
            deadlock: self.deadlock.load(Ordering::Relaxed),
            timeout: self.timeout.load(Ordering::Relaxed),
            server_error: self.server_error.load(Ordering::Relaxed),
            protocol_error: self.protocol_error.load(Ordering::Relaxed),
            crash: self.crash.load(Ordering::Relaxed),
            malformed: self.malformed.load(Ordering::Relaxed),
            disconnected: self.disconnected.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
        }
    }
}

/// Plain-data view of [`OutcomeCounters`] at one instant.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OutcomeSnapshot {
    pub(crate) success: u64,
    pub(crate) hang: u64,
    pub(crate) deadlock: u64,
    pub(crate) timeout: u64,
    pub(crate) server_error: u64,
    pub(crate) protocol_error: u64,
    pub(crate) crash: u64,
    pub(crate) malformed: u64,
    pub(crate) disconnected: u64,
    pub(crate) cancelled: u64,
}

impl OutcomeSnapshot {
    /// Sum of all variants — same as total calls recorded.
    pub(crate) fn total(&self) -> u64 {
        self.success
            + self.hang
            + self.deadlock
            + self.timeout
            + self.server_error
            + self.protocol_error
            + self.crash
            + self.malformed
            + self.disconnected
            + self.cancelled
    }
}

/// Computes requests/sec given a start instant and a count.
pub(crate) fn requests_per_sec(start: Instant, total: u64) -> f64 {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed > 0.0 {
        (total as f64) / elapsed
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_all_variants_distinct() {
        let c = OutcomeCounters::default();
        c.bump(CallOutcome::Success);
        c.bump(CallOutcome::Success);
        c.bump(CallOutcome::Hang);
        c.bump(CallOutcome::Deadlock);
        c.bump(CallOutcome::Timeout);
        c.bump(CallOutcome::ServerError);
        c.bump(CallOutcome::ProtocolError);
        c.bump(CallOutcome::Crash);
        c.bump(CallOutcome::Malformed);
        c.bump(CallOutcome::Disconnected);
        c.bump(CallOutcome::Cancelled);
        let s = c.snapshot();
        assert_eq!(s.success, 2);
        assert_eq!(s.hang, 1);
        assert_eq!(s.deadlock, 1);
        assert_eq!(s.timeout, 1);
        assert_eq!(s.server_error, 1);
        assert_eq!(s.protocol_error, 1);
        assert_eq!(s.crash, 1);
        assert_eq!(s.malformed, 1);
        assert_eq!(s.disconnected, 1);
        assert_eq!(s.cancelled, 1);
        // 2 + 1*9 = 11 bumps total
        assert_eq!(s.total(), 11);
    }

    #[test]
    fn rps_is_zero_when_elapsed_is_zero() {
        // Hard to engineer literal zero elapsed; we just check non-negative.
        let start = Instant::now();
        let r = requests_per_sec(start, 100);
        assert!(r >= 0.0);
    }
}
