//! Public metric value types — outcome enum + snapshot structs.
//!
//! These are the data shapes that `Recorder::snapshot()` /
//! `Recorder::snapshot_per_tool()` return and that reporters / analyses
//! consume. They're intentionally separated from the runtime recorder so the
//! types can be referenced (and tested) without pulling in the histogram /
//! atomics machinery.
//!
//! Public paths (`mcp_loadtest::metrics::CallOutcome`, etc.) keep resolving
//! via the `pub use` re-exports in `metrics/mod.rs`.

use std::time::Duration;

/// Per-call outcome class. Maps to the error taxonomy in DESIGN.md §18.
///
/// **Locked for M2.** New variants are non-breaking only if added at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum CallOutcome {
    /// Call completed within `hang_threshold`.
    Success,
    /// Call exceeded `hang_threshold` but completed before `grace_period` ran out.
    Hang,
    /// Call did not return even after `hang_threshold + grace_period`.
    Deadlock,
    /// Client-side timeout (separate from `hang_threshold`).
    Timeout,
    /// Server returned a JSON-RPC error in the server-defined range.
    ServerError,
    /// Server returned a JSON-RPC error in the protocol/spec range.
    ProtocolError,
    /// Server process exited unexpectedly.
    Crash,
    /// Server returned a malformed response (non-JSON / wrong shape).
    Malformed,
    /// Transport closed mid-request.
    Disconnected,
    /// Caller-side cancellation.
    Cancelled,
}

impl CallOutcome {
    /// Whether this outcome carries a meaningful latency measurement.
    /// Errors that fail before reaching the server (Cancelled, Disconnected,
    /// Malformed) are excluded — their durations would skew the histogram.
    pub(crate) fn contributes_to_latency(self) -> bool {
        matches!(
            self,
            CallOutcome::Success | CallOutcome::Hang | CallOutcome::Deadlock
        )
    }
}

/// Aggregate metrics produced by `Recorder::snapshot()`.
///
/// **Locked for M2.** Field additions OK; removal/type-change requires sync.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScenarioMetrics {
    /// Latency stats (P50/P95/P99/etc.).
    pub latency: LatencyStats,
    /// Throughput stats.
    pub throughput: ThroughputStats,
    /// Outcome breakdown by class.
    pub outcomes: OutcomeCounts,
}

/// Latency percentile distribution (microseconds resolution).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct LatencyStats {
    /// 50th percentile latency.
    pub p50: Duration,
    /// 95th percentile latency.
    pub p95: Duration,
    /// 99th percentile latency.
    pub p99: Duration,
    /// 99.9th percentile latency.
    pub p999: Duration,
    /// Arithmetic mean.
    pub mean: Duration,
    /// Smallest recorded duration.
    pub min: Duration,
    /// Largest recorded duration.
    pub max: Duration,
    /// Total samples recorded.
    pub count: u64,
}

/// Throughput summary.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThroughputStats {
    /// Total calls attempted (success + failures).
    pub total_requests: u64,
    /// Calls that returned `CallOutcome::Success`.
    pub successful_requests: u64,
    /// Mean requests-per-second over the run.
    pub requests_per_sec: f64,
}

/// Per-[`CallOutcome`] counts.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutcomeCounts {
    /// Count of [`CallOutcome::Success`].
    pub success: u64,
    /// Count of [`CallOutcome::Hang`].
    pub hang: u64,
    /// Count of [`CallOutcome::Deadlock`].
    pub deadlock: u64,
    /// Count of [`CallOutcome::Timeout`].
    pub timeout: u64,
    /// Count of [`CallOutcome::ServerError`].
    pub server_error: u64,
    /// Count of [`CallOutcome::ProtocolError`].
    pub protocol_error: u64,
    /// Count of [`CallOutcome::Crash`].
    pub crash: u64,
    /// Count of [`CallOutcome::Malformed`].
    pub malformed: u64,
    /// Count of [`CallOutcome::Disconnected`].
    pub disconnected: u64,
    /// Count of [`CallOutcome::Cancelled`].
    pub cancelled: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_stats_default_is_zeroed() {
        let s = LatencyStats::default();
        assert_eq!(s.count, 0);
        assert_eq!(s.p50, Duration::ZERO);
        assert_eq!(s.p95, Duration::ZERO);
        assert_eq!(s.p99, Duration::ZERO);
        assert_eq!(s.p999, Duration::ZERO);
        assert_eq!(s.mean, Duration::ZERO);
        assert_eq!(s.min, Duration::ZERO);
        assert_eq!(s.max, Duration::ZERO);
    }

    #[test]
    fn throughput_stats_default_is_zeroed() {
        let s = ThroughputStats::default();
        assert_eq!(s.total_requests, 0);
        assert_eq!(s.successful_requests, 0);
        assert_eq!(s.requests_per_sec, 0.0);
    }

    #[test]
    fn outcome_counts_default_is_zeroed() {
        let s = OutcomeCounts::default();
        assert_eq!(s.success, 0);
        assert_eq!(s.hang, 0);
        assert_eq!(s.deadlock, 0);
        assert_eq!(s.timeout, 0);
        assert_eq!(s.server_error, 0);
        assert_eq!(s.protocol_error, 0);
        assert_eq!(s.crash, 0);
        assert_eq!(s.malformed, 0);
        assert_eq!(s.disconnected, 0);
        assert_eq!(s.cancelled, 0);
    }

    #[test]
    fn contributes_to_latency_only_for_success_hang_deadlock() {
        assert!(CallOutcome::Success.contributes_to_latency());
        assert!(CallOutcome::Hang.contributes_to_latency());
        assert!(CallOutcome::Deadlock.contributes_to_latency());
        for o in [
            CallOutcome::Timeout,
            CallOutcome::ServerError,
            CallOutcome::ProtocolError,
            CallOutcome::Crash,
            CallOutcome::Malformed,
            CallOutcome::Disconnected,
            CallOutcome::Cancelled,
        ] {
            assert!(!o.contributes_to_latency(), "{o:?} should not contribute");
        }
    }
}
