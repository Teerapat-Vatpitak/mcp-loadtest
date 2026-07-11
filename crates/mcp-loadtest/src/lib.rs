//! `mcp-loadtest` — load tester for MCP (Model Context Protocol) servers.
//!
//! Detects deadlocks, hangs, and perf regressions that unit tests miss.
//!
//! Drives an MCP server under concurrent / ramp / soak / spike / pattern
//! load and surfaces the failure classes unit tests miss — lazy-init
//! **deadlocks**, non-determinism **races**, **hangs**, memory leaks, and
//! latency / throughput **regressions** — resolving every run to a
//! CI-gating pass/fail plus machine-readable reports. Strict MCP-schema
//! validation is available opt-in (see [`config::ValidationConfig`]).
//!
//! Most users drive it via the `mcp-loadtest` CLI (`mcp-loadtest run
//! --config bench.toml`); the library API below is for embedding it in a
//! Rust test/CI harness. See the [README] and [DESIGN] for the full story.
//!
//! [README]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/README.md
//! [DESIGN]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md
//!
//! # Library example
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//! use std::time::Duration;
//!
//! use mcp_loadtest::{Config, Run};
//! use mcp_loadtest::scenario::sustained::Sustained;
//!
//! # async fn _example() -> Result<(), Box<dyn std::error::Error>> {
//! // Parse a run config (server + thresholds + optional strict validation).
//! let config = Config::from_toml_str(
//!     r#"
//!         [server]
//!         command = "python"
//!         args = ["-m", "my_mcp"]
//!         [scenario]
//!         type = "sustained"
//!         tool = "echo"
//!     "#,
//! )?;
//!
//! // Drive a constant-load workload; the run is gated on the config's
//! // `[thresholds]` and exits non-zero (here: `report.passed()` is false)
//! // on any breach — drop it straight into CI.
//! let scenario = Box::new(Sustained {
//!     concurrent: 10,
//!     duration: Duration::from_secs(10),
//!     tool: "echo".to_string(),
//!     args: Default::default(),
//! });
//!
//! let report = Run::new(config, scenario, PathBuf::from("./runs"))
//!     .execute()
//!     .await?;
//! assert!(report.passed(), "thresholds breached");
//! # Ok(())
//! # }
//! ```

#![doc(html_root_url = "https://docs.rs/mcp-loadtest/0.0.1")]

pub mod analysis;
pub mod config;
pub mod hang_detector;
pub mod metrics;
pub mod protocol;
pub mod report;
pub mod run;
pub mod scenario;
pub mod session;
pub mod trace;

/// Self-hosted MCP server mode (`mcp-loadtest serve --mcp`).
///
/// Gated behind the `serve` cargo feature so library-only consumers don't
/// pay for the dependency. See [ADR 0005](../../docs/adr/0005-serve-mcp-mode.md).
#[cfg(feature = "serve")]
pub mod serve;

/// Live-polling Ratatui dashboard for the CLI's `--watch` modes.
///
/// Gated behind the `tui` cargo feature — only the CLI binary needs it.
/// Lives in `mcp-loadtest-output`.
#[cfg(feature = "tui")]
pub use mcp_loadtest_output::tui;

pub use config::{
    Config, ConfigError, OutputConfig, ScenarioConfig, ServerConfig, ThresholdsConfig,
    ValidationConfig,
};
pub use hang_detector::{HangOutcome, hang_detect};
pub use metrics::{
    CallOutcome, LatencyStats, OutcomeCounts, Recorder, ScenarioMetrics, ThroughputStats,
};
pub use protocol::mcp::{CallToolResult, Content, ProtocolVersion, Tool};
pub use protocol::transport::spawn_options::{SpawnOptions, StderrMode};
pub use protocol::transport::{Transport, TransportError};
pub use report::{
    ProcessSample, ProcessStats, Report, ReportError, Reporter, ServerInfo, ThresholdKind,
    ThresholdViolation,
};
pub use run::factory::SessionFactory;
pub use run::{Run, RunError, StderrCapture};
pub use scenario::{RunContext, Scenario, ScenarioOutcome};
pub use session::{Session, SessionError};
pub use trace::{ReplayReport, TraceError, TraceWriter, TracingTransport};

/// The mcp-loadtest version (facade crate; all workspace crates share it).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver_like() {
        // VERSION comes from env! at compile time so emptiness check is redundant.
        // Sanity-check it parses as semver-ish (major.minor.patch).
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "VERSION should be major.minor.patch, got {VERSION}"
        );
        for p in &parts {
            assert!(
                p.chars()
                    .all(|c| c.is_ascii_digit() || c == '-' || c.is_ascii_alphabetic())
            );
        }
    }
}
