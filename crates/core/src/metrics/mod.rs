//! Metrics recording layer — lock-free per-worker accumulators, merged at end.
//!
//! See DESIGN.md §15 (algorithms) and §14.3 (types). `Recorder` is
//! `Arc<RecorderInner>` so it clones cheaply across worker tasks; outcome
//! counters are `AtomicU64` per [`CallOutcome`] variant; latency goes into a
//! 16-shard `hdrhistogram` (see `histogram` submodule).
//!
//! Only Success / Hang / Deadlock contribute to latency — protocol / server /
//! transport errors don't have a meaningful "duration" to report. Perf target:
//! `record` < 50µs (DESIGN.md §19); typical cost is dominated by hdrhistogram
//! bucket math (a few hundred ns).
//!
//! Module layout: `types` (public value types), `histogram` (sharded
//! wrapper), `throughput` (atomic counters). Process (RSS/CPU) sampling
//! stays in `mcp_loadtest::metrics::process` until it moves to the engine
//! crate in a later restructure step.

pub mod histogram;
mod per_tool;
pub mod throughput;
mod types;

pub use types::{CallOutcome, LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::metrics::histogram::ShardedHistogram;
use crate::metrics::throughput::{OutcomeCounters, requests_per_sec};

/// Records per-call latency and outcome. Cheap-clone; one per worker.
///
/// **Locked for M2** — Agent C implements the body; other agents consume.
///
/// Hot path (`record`) must be lock-free. Snapshot is called once at end.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<RecorderInner>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared internal state. Workers hold `Arc<RecorderInner>` via `Recorder`.
struct RecorderInner {
    /// When the recorder was created — used for requests-per-sec.
    start: Instant,
    /// Sharded latency histogram (microseconds).
    latency: ShardedHistogram,
    /// Atomic per-outcome counters.
    outcomes: OutcomeCounters,
    /// Per-tool state map. Keyed by tool name. Held behind an `RwLock` —
    /// the steady-state lookup (after a tool's entry exists) takes a read
    /// lock so N workers recording into N different tools never block each
    /// other. Only the first record for a given tool needs the write lock
    /// to insert the entry. Per-tool counters/histograms inside each entry
    /// are themselves lock-free / sharded, identical to the global ones.
    ///
    /// M7 additive: coverage tracking + per-tool SLO assertions. The global
    /// `latency` / `outcomes` counters above are unchanged — they continue to
    /// aggregate every call regardless of tool, preserving the existing
    /// `snapshot()` shape.
    per_tool: RwLock<BTreeMap<String, Arc<PerToolState>>>,
}

// `PerToolState` moved to `metrics/per_tool.rs` during the M8 file-split.
use per_tool::PerToolState;

impl Recorder {
    /// Construct a fresh recorder. Captures `Instant::now()` as the run
    /// start, used for requests-per-sec computation in `snapshot`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                start: Instant::now(),
                latency: ShardedHistogram::new(),
                outcomes: OutcomeCounters::default(),
                per_tool: RwLock::new(BTreeMap::new()),
            }),
        }
    }

    /// Record a single completed call.
    ///
    /// Hot path: bumps the outcome's `AtomicU64` (lock-free) and, for
    /// outcomes that carry a meaningful duration (Success / Hang / Deadlock),
    /// records the latency into the sharded histogram.
    ///
    /// `duration` is converted to microseconds and clamped to the
    /// histogram's recordable range.
    ///
    /// **Back-compat note.** This records only into the *global* aggregate
    /// counters (the original M2 surface). Per-tool coverage tracking and
    /// per-tool SLO checks require calling [`Recorder::record_tool`] instead.
    pub fn record(&self, duration: Duration, outcome: CallOutcome) {
        self.inner.outcomes.bump(outcome);

        if outcome.contributes_to_latency() {
            // saturating_cast for very long durations
            let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
            self.inner.latency.record(micros);
        }
    }

    /// Record a single completed call together with the tool name that drove
    /// it. Updates *both* the global aggregate (so the existing
    /// [`Recorder::snapshot`] shape is unchanged) *and* the per-tool state so
    /// [`Recorder::snapshot_per_tool`] can return per-tool latency / outcome
    /// breakdowns.
    ///
    /// Use this in scenarios that want per-tool coverage + SLO enforcement
    /// (M7 differentiator). Existing scenarios that don't care can keep
    /// calling [`Recorder::record`].
    ///
    /// Locking strategy: fast-path takes a **read** lock on the per-tool map
    /// (so N tools record concurrently without contending). Slow-path (first
    /// time we see a tool name) drops to a write lock to insert. After
    /// insertion, the per-tool state's counters / histogram are lock-free
    /// just like the global ones.
    pub fn record_tool(&self, tool: &str, duration: Duration, outcome: CallOutcome) {
        // Always update the global counters first so back-compat snapshots
        // keep their existing aggregates.
        self.record(duration, outcome);

        // Fast path: take a read lock and look up the tool. The common case
        // is "tool already seen at least once", so most calls only contend
        // with other readers (i.e. don't contend at all).
        let state: Arc<PerToolState> = {
            let map = match self.inner.per_tool.read() {
                Ok(g) => g,
                // Poisoned lock on the per-tool map shouldn't poison the run
                // — best-effort: drop the per-tool sample and keep going. The
                // global counters above still saw it.
                Err(_) => return,
            };
            if let Some(state) = map.get(tool) {
                Arc::clone(state)
            } else {
                // Drop the read lock before acquiring a write lock to avoid
                // upgrade deadlocks. Re-check under write since another
                // worker may have inserted the entry while we waited.
                drop(map);
                let mut map = match self.inner.per_tool.write() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                Arc::clone(
                    map.entry(tool.to_string())
                        .or_insert_with(|| Arc::new(PerToolState::new())),
                )
            }
        };

        state.outcomes.bump(outcome);
        if outcome.contributes_to_latency() {
            let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
            state.latency.record(micros);
        }
    }

    /// Snapshot per-tool aggregates. Returns one [`ScenarioMetrics`] per
    /// distinct tool name that's been recorded via [`Recorder::record_tool`].
    ///
    /// Tools that were only ever recorded via [`Recorder::record`] (no tool
    /// name) won't appear here. Tools registered via `tools/list` but never
    /// exercised also won't appear — those are reported as "unexercised" in
    /// the `mcp_loadtest::analysis::coverage::CoverageReport` (that analysis
    /// stays in the engine crate).
    ///
    /// Like [`Recorder::snapshot`], this is intended for end-of-run
    /// reporting, not hot-path use.
    pub fn snapshot_per_tool(&self) -> BTreeMap<String, ScenarioMetrics> {
        let map = match self.inner.per_tool.read() {
            Ok(g) => g,
            Err(_) => return BTreeMap::new(),
        };
        let mut out = BTreeMap::new();
        for (name, state) in map.iter() {
            out.insert(name.clone(), per_tool_snapshot(state));
        }
        out
    }

    /// Return the exact merged latency histogram used by [`Self::snapshot`].
    ///
    /// Distributed workers serialize this evidence with the interoperable
    /// HDR Histogram V2 codec so the coordinator can merge the underlying
    /// distributions. Percentiles must never be averaged across workers.
    pub fn latency_histogram(&self) -> Histogram<u64> {
        self.inner.latency.merged()
    }

    /// Return exact merged latency histograms keyed by tool name.
    ///
    /// The keys match [`Self::snapshot_per_tool`]. Only tools recorded with
    /// [`Self::record_tool`] are present.
    pub fn per_tool_latency_histograms(&self) -> BTreeMap<String, Histogram<u64>> {
        let map = match self.inner.per_tool.read() {
            Ok(guard) => guard,
            Err(_) => return BTreeMap::new(),
        };
        map.iter()
            .map(|(name, state)| (name.clone(), state.latency.merged()))
            .collect()
    }

    /// Snapshot the current state into a readable summary for reporting.
    ///
    /// Holds each shard's mutex briefly to merge histograms; this is
    /// expected to be called once at end-of-run, not on the hot path.
    pub fn snapshot(&self) -> ScenarioMetrics {
        let outcomes_snap = self.inner.outcomes.snapshot();
        let merged = self.inner.latency.merged();

        let count = merged.len();
        let latency = if count == 0 {
            LatencyStats::default()
        } else {
            LatencyStats {
                p50: us_to_duration(merged.value_at_quantile(0.50)),
                p95: us_to_duration(merged.value_at_quantile(0.95)),
                p99: us_to_duration(merged.value_at_quantile(0.99)),
                p999: us_to_duration(merged.value_at_quantile(0.999)),
                mean: Duration::from_micros(merged.mean() as u64),
                min: us_to_duration(merged.min()),
                max: us_to_duration(merged.max()),
                count,
            }
        };

        let total = outcomes_snap.total();
        let throughput = ThroughputStats {
            total_requests: total,
            successful_requests: outcomes_snap.success + outcomes_snap.expected_rejection,
            requests_per_sec: requests_per_sec(self.inner.start, total),
        };

        let outcomes = OutcomeCounts {
            success: outcomes_snap.success,
            hang: outcomes_snap.hang,
            deadlock: outcomes_snap.deadlock,
            timeout: outcomes_snap.timeout,
            server_error: outcomes_snap.server_error,
            protocol_error: outcomes_snap.protocol_error,
            crash: outcomes_snap.crash,
            malformed: outcomes_snap.malformed,
            disconnected: outcomes_snap.disconnected,
            cancelled: outcomes_snap.cancelled,
            expected_rejection: outcomes_snap.expected_rejection,
        };

        ScenarioMetrics {
            latency,
            throughput,
            outcomes,
        }
    }
}

/// Microseconds → `Duration`. Helper for percentile reads.
fn us_to_duration(us: u64) -> Duration {
    Duration::from_micros(us)
}

/// Snapshot one [`PerToolState`] into a [`ScenarioMetrics`]. Mirrors
/// [`Recorder::snapshot`]'s body but scoped to a single tool's counters.
fn per_tool_snapshot(state: &PerToolState) -> ScenarioMetrics {
    let outcomes_snap = state.outcomes.snapshot();
    let merged = state.latency.merged();

    let count = merged.len();
    let latency = if count == 0 {
        LatencyStats::default()
    } else {
        LatencyStats {
            p50: us_to_duration(merged.value_at_quantile(0.50)),
            p95: us_to_duration(merged.value_at_quantile(0.95)),
            p99: us_to_duration(merged.value_at_quantile(0.99)),
            p999: us_to_duration(merged.value_at_quantile(0.999)),
            mean: Duration::from_micros(merged.mean() as u64),
            min: us_to_duration(merged.min()),
            max: us_to_duration(merged.max()),
            count,
        }
    };

    let total = outcomes_snap.total();
    let throughput = ThroughputStats {
        total_requests: total,
        successful_requests: outcomes_snap.success + outcomes_snap.expected_rejection,
        requests_per_sec: requests_per_sec(state.start, total),
    };

    let outcomes = OutcomeCounts {
        success: outcomes_snap.success,
        hang: outcomes_snap.hang,
        deadlock: outcomes_snap.deadlock,
        timeout: outcomes_snap.timeout,
        server_error: outcomes_snap.server_error,
        protocol_error: outcomes_snap.protocol_error,
        crash: outcomes_snap.crash,
        malformed: outcomes_snap.malformed,
        disconnected: outcomes_snap.disconnected,
        cancelled: outcomes_snap.cancelled,
        expected_rejection: outcomes_snap.expected_rejection,
    };

    ScenarioMetrics {
        latency,
        throughput,
        outcomes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    /// Allowed jitter (in microseconds) for percentile reads. hdrhistogram
    /// quantizes to ~3 sig digits so tiny adjustments are expected.
    const PERCENTILE_TOLERANCE_US: u64 = 5;

    fn approx_eq_us(a: Duration, expected_us: u64, tol: u64) {
        let actual = a.as_micros() as u64;
        let lo = expected_us.saturating_sub(tol);
        let hi = expected_us + tol;
        assert!(
            actual >= lo && actual <= hi,
            "expected ~{expected_us}us (±{tol}), got {actual}us"
        );
    }

    #[test]
    fn record_then_snapshot_returns_counts() {
        let r = Recorder::new();
        r.record(Duration::from_micros(10), CallOutcome::Success);
        r.record(Duration::from_micros(20), CallOutcome::Success);
        r.record(Duration::from_micros(30), CallOutcome::Hang);
        r.record(Duration::from_micros(0), CallOutcome::ServerError);
        r.record(Duration::from_micros(0), CallOutcome::Crash);
        r.record(Duration::from_micros(0), CallOutcome::Cancelled);
        r.record(Duration::from_micros(0), CallOutcome::ExpectedRejection);

        let snap = r.snapshot();

        // Outcome counts
        assert_eq!(snap.outcomes.success, 2);
        assert_eq!(snap.outcomes.hang, 1);
        assert_eq!(snap.outcomes.server_error, 1);
        assert_eq!(snap.outcomes.crash, 1);
        assert_eq!(snap.outcomes.cancelled, 1);
        assert_eq!(snap.outcomes.expected_rejection, 1);
        assert_eq!(snap.outcomes.deadlock, 0);

        // Throughput
        assert_eq!(snap.throughput.total_requests, 7);
        assert_eq!(snap.throughput.successful_requests, 3);

        // Latency: only Success/Hang/Deadlock contribute → 3 samples
        assert_eq!(snap.latency.count, 3);
    }

    #[test]
    fn latency_percentiles_correct() {
        let r = Recorder::new();
        for i in 1..=1000u64 {
            r.record(Duration::from_micros(i), CallOutcome::Success);
        }
        let snap = r.snapshot();
        assert_eq!(snap.latency.count, 1000);

        // p50 ≈ 500us, p95 ≈ 950us, p99 ≈ 990us, p999 ≈ 999us
        approx_eq_us(snap.latency.p50, 500, PERCENTILE_TOLERANCE_US);
        approx_eq_us(snap.latency.p95, 950, PERCENTILE_TOLERANCE_US);
        approx_eq_us(snap.latency.p99, 990, PERCENTILE_TOLERANCE_US);
        approx_eq_us(snap.latency.p999, 999, PERCENTILE_TOLERANCE_US);
        approx_eq_us(snap.latency.min, 1, PERCENTILE_TOLERANCE_US);
        approx_eq_us(snap.latency.max, 1000, PERCENTILE_TOLERANCE_US);
    }

    #[test]
    fn recorder_clones_share_state() {
        let r1 = Recorder::new();
        let r2 = r1.clone();
        r1.record(Duration::from_micros(100), CallOutcome::Success);
        r2.record(Duration::from_micros(200), CallOutcome::Success);
        r2.record(Duration::from_micros(0), CallOutcome::ServerError);

        let snap = r1.snapshot();
        assert_eq!(snap.outcomes.success, 2);
        assert_eq!(snap.outcomes.server_error, 1);
        assert_eq!(snap.latency.count, 2);
        assert_eq!(snap.throughput.total_requests, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_record_does_not_lose_samples() {
        let r = StdArc::new(Recorder::new());
        let mut handles = Vec::with_capacity(4);
        for t in 0..4u64 {
            let r = r.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..1000u64 {
                    // Vary duration so we don't hit one bucket only.
                    let d = Duration::from_micros(1 + (t * 1000 + i) % 5000);
                    r.record(d, CallOutcome::Success);
                }
            }));
        }
        for h in handles {
            h.await.expect("worker task should not panic");
        }
        let snap = r.snapshot();
        assert_eq!(snap.outcomes.success, 4000);
        assert_eq!(snap.throughput.total_requests, 4000);
        assert_eq!(snap.latency.count, 4000);
    }

    /// Smoke perf check: 10k records on one thread should be well under
    /// 50ms even on slow CI (target is <50µs per call → 10k records < 500ms).
    /// This is not a real benchmark — for that, see DESIGN.md §19 / `benches/`.
    ///
    /// Criterion-style benchmark sketch (not run as a test):
    ///
    /// ```ignore
    /// use criterion::*;
    /// fn bench_record(c: &mut Criterion) {
    ///     let r = Recorder::new();
    ///     c.bench_function("record/success", |b| {
    ///         b.iter(|| r.record(Duration::from_micros(100), CallOutcome::Success));
    ///     });
    /// }
    /// ```
    #[test]
    fn record_perf_smoke() {
        let r = Recorder::new();
        let n = 10_000u64;
        let start = Instant::now();
        for i in 0..n {
            r.record(Duration::from_micros(1 + (i % 5000)), CallOutcome::Success);
        }
        let elapsed = start.elapsed();
        // Generous bound: 500ms on slow CI. The actual target (DESIGN.md §19)
        // is <50µs per call (= <500ms for 10k); a real bench measures it
        // properly.
        assert!(
            elapsed < Duration::from_millis(500),
            "10k records took {elapsed:?}; target <50µs each (<500ms total)"
        );
        let snap = r.snapshot();
        assert_eq!(snap.latency.count, n);
    }

    #[test]
    fn empty_snapshot_has_default_latency() {
        let r = Recorder::new();
        let snap = r.snapshot();
        assert_eq!(snap.latency.count, 0);
        assert_eq!(snap.outcomes.success, 0);
        assert_eq!(snap.throughput.total_requests, 0);
    }

    #[test]
    fn latency_excluded_outcomes_dont_contribute() {
        let r = Recorder::new();
        // None of these should land in the latency histogram.
        for o in [
            CallOutcome::Timeout,
            CallOutcome::ServerError,
            CallOutcome::ProtocolError,
            CallOutcome::Crash,
            CallOutcome::Malformed,
            CallOutcome::Disconnected,
            CallOutcome::Cancelled,
            CallOutcome::ExpectedRejection,
        ] {
            r.record(Duration::from_micros(100), o);
        }
        let snap = r.snapshot();
        assert_eq!(snap.latency.count, 0);
        assert_eq!(snap.throughput.total_requests, 8);
    }

    #[test]
    fn record_tool_updates_global_and_per_tool() {
        let r = Recorder::new();
        r.record_tool("echo", Duration::from_micros(100), CallOutcome::Success);
        r.record_tool("echo", Duration::from_micros(200), CallOutcome::Success);
        r.record_tool("compute", Duration::from_micros(500), CallOutcome::Success);
        r.record_tool(
            "compute",
            Duration::from_micros(0),
            CallOutcome::ServerError,
        );

        // Global snapshot sees every call (back-compat).
        let snap = r.snapshot();
        assert_eq!(snap.throughput.total_requests, 4);
        assert_eq!(snap.outcomes.success, 3);
        assert_eq!(snap.outcomes.server_error, 1);
        assert_eq!(snap.latency.count, 3); // only Success contributes

        // Per-tool snapshots split the call breakdown by tool name.
        let per = r.snapshot_per_tool();
        assert_eq!(per.len(), 2);
        let echo = per.get("echo").expect("echo bucket present");
        assert_eq!(echo.throughput.total_requests, 2);
        assert_eq!(echo.outcomes.success, 2);
        assert_eq!(echo.latency.count, 2);
        let compute = per.get("compute").expect("compute bucket present");
        assert_eq!(compute.throughput.total_requests, 2);
        assert_eq!(compute.outcomes.success, 1);
        assert_eq!(compute.outcomes.server_error, 1);
        assert_eq!(compute.latency.count, 1);
    }

    #[test]
    fn snapshot_per_tool_empty_when_no_per_tool_calls() {
        let r = Recorder::new();
        // Plain `record()` doesn't carry a tool name.
        r.record(Duration::from_micros(100), CallOutcome::Success);
        assert!(r.snapshot_per_tool().is_empty());
        // But the global snapshot still records it for back-compat.
        assert_eq!(r.snapshot().throughput.total_requests, 1);
    }

    #[test]
    fn exact_histogram_evidence_matches_readable_snapshots() {
        let r = Recorder::new();
        r.record_tool("echo", Duration::from_micros(100), CallOutcome::Success);
        r.record_tool("echo", Duration::from_micros(300), CallOutcome::Hang);
        r.record_tool("compute", Duration::from_micros(500), CallOutcome::Success);

        let global = r.latency_histogram();
        assert_eq!(global.len(), r.snapshot().latency.count);
        assert_eq!(global.min(), 100);
        assert_eq!(global.max(), 500);

        let per_tool = r.per_tool_latency_histograms();
        assert_eq!(per_tool["echo"].len(), 2);
        assert_eq!(per_tool["compute"].len(), 1);
        assert_eq!(
            per_tool["echo"].len(),
            r.snapshot_per_tool()["echo"].latency.count
        );
    }
}
