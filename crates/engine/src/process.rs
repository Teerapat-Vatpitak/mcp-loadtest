//! Process-level resource sampling — RSS and CPU% via `sysinfo`.
//!
//! Used by the orchestrator (Agent H's `Run::execute`) to sample the
//! server-under-test's resident set size and CPU usage on a fixed cadence.
//! Final aggregate (peak/final RSS, mean CPU%) plus the raw timeseries
//! land in [`mcp_loadtest_core::report::ProcessStats`].
//!
//! See DESIGN.md §14.3 (`ProcessStats`/`ProcessSample`) and §17.2.
//!
//! # Usage
//!
//! ```ignore
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use mcp_loadtest::metrics::process::ProcessSampler;
//!
//! # async fn _example(server_pid: u32) {
//! let cancel = CancellationToken::new();
//! let sampler = ProcessSampler::spawn(
//!     server_pid,
//!     Duration::from_millis(500),
//!     cancel.clone(),
//! );
//!
//! // ... run scenario ...
//!
//! cancel.cancel();
//! let stats = sampler.finish().await;
//! println!("peak RSS: {:.1} MB", stats.peak_rss_mb);
//! # }
//! ```
//!
//! # Notes on the sysinfo 0.32 API
//!
//! - CPU% in sysinfo requires **two refreshes** for an accurate reading
//!   (it's diff-based). The sampler's first sample's CPU% is therefore
//!   typically `0.0`; subsequent samples are meaningful.
//! - `process.memory()` returns **bytes**; we convert to MiB.
//! - `cpu_usage()` returns total across logical cores — a 4-core box at
//!   100% one-core load reads 100.0, not 25.0.
//! - `process.tasks()` returns `Some(&HashSet<Pid>)` of child task PIDs
//!   **only on Linux** (and only when the `unknown-ci` feature is off).
//!   On macOS / Windows it always returns `None`, so the thread count
//!   reported here will be `0` on those platforms.
//! - sysinfo 0.32 does **not** expose an open-file-descriptor count on
//!   any platform. We attempt a best-effort lookup via `/proc/<pid>/fd`
//!   on Linux and fall back to `0` elsewhere. Windows has
//!   `GetProcessHandleCount` but we don't pull in `windows-sys` for that
//!   here — fd count there is a known gap, called out in `ProcessStats`'
//!   field docs.
//! - When sysinfo can't see the process (it exited, or permission denied),
//!   the sampler logs a warn-level trace event and skips the tick. It does
//!   **not** panic and does **not** abort sampling.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use mcp_loadtest_core::report::{ProcessSample, ProcessStats};

/// Spawned background sampler. Hold until end of run, then call
/// [`ProcessSampler::finish`] to drain the final aggregate.
pub struct ProcessSampler {
    /// Tokio task running the sample loop. Returns the final
    /// [`ProcessStats`] when cancelled / target process exits.
    handle: JoinHandle<ProcessStats>,
    /// Cancellation token shared with the loop. Owned here so
    /// [`ProcessSampler::finish`] can cancel even if the caller didn't.
    cancel: CancellationToken,
    /// Set to true when the loop's first sample lands; lets tests poll
    /// "is the sampler warm?" without busy-looping. Currently unused by
    /// the public API but kept for future debug hooks.
    _started: oneshot::Receiver<()>,
}

impl ProcessSampler {
    /// Spawn a tokio task that samples `pid` every `interval` until
    /// `cancel` fires (or the process disappears).
    ///
    /// Uses `tokio::spawn` — caller must be inside a Tokio runtime.
    pub fn spawn(pid: u32, interval: Duration, cancel: CancellationToken) -> Self {
        let (started_tx, started_rx) = oneshot::channel();
        let cancel_loop = cancel.clone();

        let handle =
            tokio::spawn(
                async move { sample_loop(pid, interval, cancel_loop, Some(started_tx)).await },
            );

        Self {
            handle,
            cancel,
            _started: started_rx,
        }
    }

    /// Cancel sampling and await the final aggregate. The returned
    /// [`ProcessStats`] has `samples` in chronological order.
    ///
    /// If the underlying task panicked, returns a zero-valued
    /// [`ProcessStats`] (the sampler is best-effort and never fails the
    /// run).
    pub async fn finish(self) -> ProcessStats {
        self.cancel.cancel();
        match self.handle.await {
            Ok(stats) => stats,
            Err(err) => {
                tracing::warn!(error = %err, "process sampler task panicked; returning empty stats");
                ProcessStats::default()
            }
        }
    }
}

/// Inner sample loop. Runs on a tokio task. Owns the `sysinfo::System` so
/// no allocations happen on the per-tick hot path beyond what sysinfo
/// itself does internally.
async fn sample_loop(
    pid_u32: u32,
    interval: Duration,
    cancel: CancellationToken,
    mut started_tx: Option<oneshot::Sender<()>>,
) -> ProcessStats {
    let pid = Pid::from_u32(pid_u32);
    let mut system = System::new();
    let started_at = Instant::now();
    let mut samples: Vec<ProcessSample> = Vec::new();

    // First refresh — establishes the baseline for the next CPU% delta.
    // We don't push this as a sample: CPU% on a fresh System is always
    // 0.0 and would skew the average downward.
    refresh_pid(&mut system, pid);

    let mut ticker = tokio::time::interval(interval);
    // Skip the immediate first-tick fire so we don't double up with the
    // baseline refresh above. The next `tick().await` waits one full
    // `interval`.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Discard the immediate tick.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {
                refresh_pid(&mut system, pid);
                match system.process(pid) {
                    Some(proc) => {
                        let rss_bytes = proc.memory();
                        let rss_mb = bytes_to_mib(rss_bytes);
                        let cpu_pct = f64::from(proc.cpu_usage());
                        // `tasks()` is Linux-only — None on macOS / Windows.
                        let threads = proc.tasks().map(|t| t.len() as u64).unwrap_or(0);
                        // `open_files()` doesn't exist on sysinfo 0.32. Use a
                        // platform-conditional fallback that reads /proc on
                        // Linux. On Windows / macOS this stays at 0; that's a
                        // known limitation documented on `ProcessSample::fd`.
                        let fd = best_effort_fd_count(pid_u32);
                        let at_secs = started_at.elapsed().as_secs_f64();
                        samples.push(ProcessSample {
                            at_secs,
                            rss_mb,
                            cpu_pct,
                            fd,
                            threads,
                        });
                        if let Some(tx) = started_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    None => {
                        // Process is gone (exited, or sysinfo lost track of it).
                        // Log once, then keep ticking — caller decides when to stop.
                        tracing::trace!(
                            pid = pid_u32,
                            "process not visible to sysinfo this tick; skipping"
                        );
                    }
                }
            }
        }
    }

    aggregate(samples)
}

/// Best-effort open-file-descriptor count.
///
/// - **Linux**: counts entries under `/proc/<pid>/fd`. Includes regular
///   files, sockets, pipes, devices — everything `lsof` would see.
/// - **macOS / Windows**: returns `0`. sysinfo 0.32 doesn't expose this
///   and we don't want to pull in `windows-sys` / `libproc` just for the
///   leak heuristic. Documented in `ProcessSample::fd`.
fn best_effort_fd_count(pid: u32) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/fd");
        match std::fs::read_dir(&path) {
            Ok(rd) => rd.filter_map(|e| e.ok()).count() as u64,
            Err(err) => {
                tracing::trace!(pid, %err, "fd count: /proc/<pid>/fd read failed");
                0
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid; // silence unused on non-linux
        tracing::trace!("fd count not available on this platform; reporting 0");
        0
    }
}

/// Refresh just the one PID we care about, including CPU + memory.
/// Returns silently if sysinfo can't see the process (caller checks
/// `system.process(pid)`).
fn refresh_pid(system: &mut System, pid: Pid) {
    // `remove_dead_processes = true` so a vanished PID gets cleared from
    // the internal map (sysinfo otherwise would keep stale entries).
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::everything(),
    );
}

/// Aggregate raw samples into a [`ProcessStats`].
///
/// - `peak_rss_mb` / `peak_fd` / `peak_threads` — max over samples
/// - `final_rss_mb` / `final_fd` / `final_threads` — last sample's value
/// - `baseline_rss_mb` — first sample's RSS (start-of-run reference)
/// - `avg_cpu_pct` — arithmetic mean (0 if no samples)
fn aggregate(samples: Vec<ProcessSample>) -> ProcessStats {
    if samples.is_empty() {
        return ProcessStats::default();
    }

    let peak_rss_mb = samples.iter().map(|s| s.rss_mb).fold(0.0_f64, f64::max);
    let final_rss_mb = samples.last().map(|s| s.rss_mb).unwrap_or(0.0);
    let baseline_rss_mb = samples.first().map(|s| s.rss_mb).unwrap_or(0.0);

    let cpu_sum: f64 = samples.iter().map(|s| s.cpu_pct).sum();
    let avg_cpu_pct = cpu_sum / (samples.len() as f64);

    let peak_fd = samples.iter().map(|s| s.fd).max().unwrap_or(0);
    let final_fd = samples.last().map(|s| s.fd).unwrap_or(0);
    let peak_threads = samples.iter().map(|s| s.threads).max().unwrap_or(0);
    let final_threads = samples.last().map(|s| s.threads).unwrap_or(0);

    ProcessStats {
        peak_rss_mb,
        final_rss_mb,
        baseline_rss_mb,
        avg_cpu_pct,
        peak_fd,
        final_fd,
        peak_threads,
        final_threads,
        samples,
    }
}

/// Bytes → MiB. sysinfo reports memory in bytes (across all platforms in
/// 0.32+; older versions used KB on some OSes). Float-domain so small
/// processes don't truncate to 0.
fn bytes_to_mib(bytes: u64) -> f64 {
    (bytes as f64) / (1024.0 * 1024.0)
}

/// Default sample interval recommended for production runs.
///
/// 500ms is a good balance: fine enough to catch sub-second RSS spikes
/// during `spike` scenarios, coarse enough to keep sysinfo overhead well
/// under 1% CPU on a 4-core laptop.
pub(crate) const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_empty_returns_default() {
        let stats = aggregate(Vec::new());
        assert_eq!(stats.peak_rss_mb, 0.0);
        assert_eq!(stats.final_rss_mb, 0.0);
        assert_eq!(stats.baseline_rss_mb, 0.0);
        assert_eq!(stats.avg_cpu_pct, 0.0);
        assert_eq!(stats.peak_fd, 0);
        assert_eq!(stats.final_fd, 0);
        assert_eq!(stats.peak_threads, 0);
        assert_eq!(stats.final_threads, 0);
        assert!(stats.samples.is_empty());
    }

    #[test]
    fn aggregate_three_samples_picks_peak_final_avg() {
        let samples = vec![
            ProcessSample {
                at_secs: 0.0,
                rss_mb: 10.0,
                cpu_pct: 1.0,
                fd: 12,
                threads: 4,
            },
            ProcessSample {
                at_secs: 0.5,
                rss_mb: 50.0,
                cpu_pct: 9.0,
                fd: 20,
                threads: 7,
            },
            ProcessSample {
                at_secs: 1.0,
                rss_mb: 30.0,
                cpu_pct: 5.0,
                fd: 15,
                threads: 5,
            },
        ];
        let stats = aggregate(samples);
        assert_eq!(stats.peak_rss_mb, 50.0);
        assert_eq!(stats.final_rss_mb, 30.0);
        // baseline is the FIRST sample's RSS (start-of-run reference)
        assert_eq!(stats.baseline_rss_mb, 10.0);
        assert!((stats.avg_cpu_pct - 5.0).abs() < 1e-9);
        assert_eq!(stats.peak_fd, 20);
        assert_eq!(stats.final_fd, 15);
        assert_eq!(stats.peak_threads, 7);
        assert_eq!(stats.final_threads, 5);
        assert_eq!(stats.samples.len(), 3);
    }

    #[test]
    fn aggregate_handles_zero_fd_and_threads() {
        // Regression: on platforms where fd / threads are unavailable
        // (Windows + macOS for fd; non-Linux for threads) every sample
        // reports 0. Aggregate must not divide-by-zero or panic.
        let samples = vec![
            ProcessSample {
                at_secs: 0.0,
                rss_mb: 10.0,
                cpu_pct: 1.0,
                fd: 0,
                threads: 0,
            },
            ProcessSample {
                at_secs: 0.5,
                rss_mb: 12.0,
                cpu_pct: 2.0,
                fd: 0,
                threads: 0,
            },
        ];
        let stats = aggregate(samples);
        assert_eq!(stats.peak_fd, 0);
        assert_eq!(stats.final_fd, 0);
        assert_eq!(stats.peak_threads, 0);
        assert_eq!(stats.final_threads, 0);
    }

    #[test]
    fn bytes_to_mib_correct() {
        assert_eq!(bytes_to_mib(0), 0.0);
        // 1 MiB exactly
        assert!((bytes_to_mib(1024 * 1024) - 1.0).abs() < 1e-9);
        // 100 MiB
        assert!((bytes_to_mib(100 * 1024 * 1024) - 100.0).abs() < 1e-9);
    }

    /// Smoke test that the loop exits promptly when cancelled, even if
    /// no samples were ever taken. Uses `u32::MAX` as a deliberately
    /// invalid PID so sysinfo never sees the process.
    #[tokio::test]
    async fn cancel_returns_promptly_on_invalid_pid() {
        let cancel = CancellationToken::new();
        let sampler = ProcessSampler::spawn(u32::MAX, Duration::from_millis(50), cancel.clone());

        // Let one tick interval pass so we exercise the "process gone"
        // branch at least once.
        tokio::time::sleep(Duration::from_millis(120)).await;

        let start = Instant::now();
        let stats = sampler.finish().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "finish should return promptly after cancel; took {elapsed:?}"
        );
        // No samples could have been taken for an invalid PID.
        assert!(stats.samples.is_empty());
        assert_eq!(stats.peak_rss_mb, 0.0);
        assert_eq!(stats.final_rss_mb, 0.0);
        assert_eq!(stats.avg_cpu_pct, 0.0);

        // Ensure the (unused) `cancel` we cloned can be cancelled twice
        // without panicking — finish() also calls cancel.cancel().
        cancel.cancel();
    }
}
