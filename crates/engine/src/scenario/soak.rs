//! `soak` scenario — long-duration steady load with periodic metric snapshots.
//!
//! Drives the server at constant load for `duration` (production: 30m–several
//! hours; tests: seconds) while sampling [`mcp_loadtest_core::metrics::Recorder`] every
//! `sample_interval`. The timeseries lets a downstream analyser look for
//! memory leaks, latency drift, or throughput collapse over time that a short
//! [`crate::scenario::sustained::Sustained`] run would miss.
//!
//! M6 added a linear-regression leak heuristic ([`detect_leak`], extracted in
//! M8 to `soak/leak_detect.rs`). The scenario can't see RSS directly — that
//! lives in [`crate::process::ProcessSampler`] wired up by the run
//! orchestrator — so the scenario applies `detect_leak` to its own
//! **latency-mean** trajectory (threshold:
//! [`Soak::latency_drift_ms_per_sec`]). The orchestrator applies the same
//! function to the sampled **RSS** series (`run/thresholds.rs`) when the
//! opt-in `thresholds.rss_leak_mb_per_sec` is set — see
//! [`mcp_loadtest_core::config::ThresholdsConfig::rss_leak_mb_per_sec`].
//!
//! Architecture mirrors `Sustained`: single session, single driver loop,
//! cancellation checked before each call, sampling via `tokio::select!`
//! (keeps snapshot history single-owner; no `JoinHandle` leak risk).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::yield_now;

use crate::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_core::metrics::{CallOutcome, ScenarioMetrics};
use mcp_loadtest_protocol::Session;

mod leak_detect;
// Re-export so `mcp_loadtest::scenario::soak::detect_leak` keeps resolving
// for the integration test `tests/soak.rs`, which compiles as an external
// crate and so requires the symbol on the public API surface.
pub use leak_detect::detect_leak;

/// Default slope above which [`Soak`] flags mean-latency drift as suspicious.
///
/// Units: **milliseconds-per-second** (mean call latency growth rate). A
/// healthy steady-state server holds mean latency within ±0.5 ms/sec on a
/// long soak; > 5 ms/sec sustained is almost certainly a regression (often
/// downstream of a memory leak — handlers slow down as GC pressure mounts).
///
/// Real RSS-trajectory leak detection lives in [`detect_leak`] called against
/// `ProcessStats::samples` by the run orchestrator's threshold evaluation —
/// opt-in via [`mcp_loadtest_core::config::ThresholdsConfig::rss_leak_mb_per_sec`]; this
/// latency-based sentinel just gives soak runs an in-scenario signal when RSS
/// sampling isn't available (Windows fd quirks, container without /proc,
/// etc.) or the RSS threshold wasn't configured.
pub(crate) const DEFAULT_LATENCY_DRIFT_MS_PER_SEC: f64 = 5.0;

/// Long-running steady-load scenario with periodic metric snapshots.
///
/// See module docs for the M5 → M6 split.
pub struct Soak {
    /// Declared concurrency target. Currently informational; see
    /// [`crate::scenario::sustained`] module docs for why.
    pub concurrent: u32,
    /// Total time the soak runs. Production: 30m–several hours; tests: seconds.
    pub duration: Duration,
    /// Tool to invoke on every iteration of the inner driver loop.
    pub tool: String,
    /// Arguments JSON for `tool`.
    pub args: Value,
    /// How often to snapshot `ctx.metrics` while the soak runs.
    pub sample_interval: Duration,
    /// Mean-latency drift (milliseconds-per-second) above which the soak
    /// flags the run as drifting. Defaults to
    /// `DEFAULT_LATENCY_DRIFT_MS_PER_SEC`.
    ///
    /// Note: this is **not** an RSS-based leak signal — that lives in the
    /// run orchestrator's RSS regression on `ProcessStats::samples`, opt-in
    /// via [`mcp_loadtest_core::config::ThresholdsConfig::rss_leak_mb_per_sec`]. This
    /// threshold catches the downstream symptom (handlers slow down) when
    /// process sampling isn't available.
    pub latency_drift_ms_per_sec: f64,
}

impl Default for Soak {
    fn default() -> Self {
        Self {
            concurrent: 1,
            duration: Duration::from_secs(60),
            tool: String::new(),
            args: Value::Null,
            sample_interval: Duration::from_secs(10),
            latency_drift_ms_per_sec: DEFAULT_LATENCY_DRIFT_MS_PER_SEC,
        }
    }
}

#[async_trait]
impl Scenario for Soak {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut total_calls: u64 = 0;
        let mut successful_calls: u64 = 0;
        let mut error_count: u64 = 0;

        let start = Instant::now();
        let deadline = start + self.duration;
        let mut next_sample = start + self.sample_interval;
        let mut samples: Vec<(Duration, ScenarioMetrics)> = Vec::new();
        let mut notes = Vec::new();

        if self.concurrent > 1 {
            notes.push(format!(
                "soak: M5 runs sequentially on one session; concurrent={} \
                 is recorded but not multiplexed",
                self.concurrent
            ));
        }

        loop {
            if ctx.is_cancelled() {
                notes.push("soak: cancelled via ctx.cancel_token".to_owned());
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                break;
            }

            // Take a snapshot whenever we cross a sample boundary. This runs
            // before every call so a slow call can't push the next sample
            // past the deadline.
            while now >= next_sample {
                let elapsed = now.saturating_duration_since(start);
                samples.push((elapsed, ctx.metrics.snapshot()));
                next_sample += self.sample_interval;
            }

            let call_start = Instant::now();
            let call_fut = session.call_tool(&self.tool, &self.args);
            let result = tokio::select! {
                biased;
                _ = ctx.cancel_token.cancelled() => {
                    let elapsed = call_start.elapsed();
                    ctx.metrics.record_tool(&self.tool, elapsed, CallOutcome::Cancelled);
                    notes.push("soak: call interrupted by cancellation".to_owned());
                    total_calls += 1;
                    error_count += 1;
                    break;
                }
                r = call_fut => r,
            };

            let elapsed = call_start.elapsed();
            total_calls += 1;
            match result {
                Ok(_) => {
                    successful_calls += 1;
                    ctx.metrics
                        .record_tool(&self.tool, elapsed, CallOutcome::Success);
                }
                Err(err) => {
                    error_count += 1;
                    ctx.metrics
                        .record_tool(&self.tool, elapsed, super::classify_error(&err));
                    if super::is_terminal_error(&err) {
                        notes.push(format!(
                            "soak: terminal error after {total_calls} calls: {err}"
                        ));
                        break;
                    }
                }
            }

            // Yield so cancellation has a fair chance even on a fast server.
            yield_now().await;
        }

        // Final snapshot at end-of-run so consumers always see at least the
        // closing state, even on very short soak durations.
        let final_elapsed = start.elapsed();
        samples.push((final_elapsed, ctx.metrics.snapshot()));

        // Summary line + per-sample lines.
        //
        // We still emit `soak.sample` notes because downstream consumers
        // (the integration tests in `tests/soak.rs`, plus the
        // `compare`/`reporter` agents) parse them. M6's leak heuristic
        // adds one extra `soak: leak detected: slope=…` line when the
        // per-`ScenarioMetrics` throughput trajectory implies an
        // accelerating in-flight backlog. RSS-based detection is done by
        // [`detect_leak`] on the [`mcp_loadtest_core::report::ProcessStats::samples`]
        // timeseries — invoked by the orchestrator's threshold evaluation
        // (`run/thresholds.rs`) when `thresholds.rss_leak_mb_per_sec` is set.
        notes.push(format!(
            "soak: {} samples over {:.1}s (interval={:?})",
            samples.len(),
            final_elapsed.as_secs_f64(),
            self.sample_interval,
        ));
        for (offset, snap) in &samples {
            notes.push(format!(
                "soak.sample t={:.1}s requests={} success={} p99={:?}",
                offset.as_secs_f64(),
                snap.throughput.total_requests,
                snap.throughput.successful_requests,
                snap.latency.p99,
            ));
        }

        // Run the leak heuristic on the in-process latency-mean trajectory
        // as a stand-in for an RSS series. The mean climbs monotonically
        // on the canonical leak pattern (request handlers leaking memory
        // serve responses progressively slower as GC pressure mounts), so
        // it's a useful sentinel even when ProcessSampler couldn't read
        // RSS (Windows fd quirks, container without /proc, etc.). Real
        // RSS-based detection happens via [`detect_leak`] called against
        // `ProcessStats::samples` by the orchestrator, when the opt-in
        // `thresholds.rss_leak_mb_per_sec` is configured.
        let mean_series: Vec<(f64, f64)> = samples
            .iter()
            .map(|(offset, snap)| {
                (
                    offset.as_secs_f64(),
                    snap.latency.mean.as_secs_f64() * 1_000.0, // ms
                )
            })
            .collect();
        if let Some(slope_ms_per_sec) = detect_leak(&mean_series)
            && slope_ms_per_sec > self.latency_drift_ms_per_sec
        {
            let predicted_ms = slope_ms_per_sec * final_elapsed.as_secs_f64();
            notes.push(format!(
                "latency drift detected: mean grows {slope_ms_per_sec:.2} ms/sec, \
                 predicted +{predicted_ms:.2}ms over {:.1}s — investigate for memory leak \
                 or GC pressure (see ProcessStats RSS series for ground truth)",
                final_elapsed.as_secs_f64()
            ));
        }

        ScenarioOutcome {
            total_calls,
            successful_calls,
            hang_count: 0,
            deadlock_count: 0,
            error_count,
            notes,
            hung_for_ms: Vec::new(),
        }
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "title": "Soak",
            "description": "Long-running steady load with periodic metric snapshots and \
                            linear-regression leak detection on the RSS / latency timeseries.",
            "properties": {
                "concurrent": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Target concurrency (M5: serialized on one session, like Sustained)."
                },
                "duration": {
                    "type": "string",
                    "description": "Total soak duration as a humantime string (e.g. \"1h\", \"30m\")."
                },
                "tool": {
                    "type": "string",
                    "description": "MCP tool name to invoke on every iteration."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to `tool`."
                },
                "sample_interval": {
                    "type": "string",
                    "description": "How often to snapshot metrics, as a humantime string (e.g. \"10s\")."
                },
                "latency_drift_ms_per_sec": {
                    "type": "number",
                    "minimum": 0.0,
                    "description": "Mean-latency drift threshold (ms/sec) to flag a soak. Default 5.0. RSS-based leak detection is separate and lives in the run orchestrator."
                }
            },
            "required": ["concurrent", "duration", "tool", "args", "sample_interval"]
        })
    }

    fn name(&self) -> &'static str {
        "soak"
    }
}
