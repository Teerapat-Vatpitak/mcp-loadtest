//! Report renderers, grading, regression policy, and the live TUI for
//! mcp-loadtest.
//!
//! Everything here is post-run (or live-polling, for the TUI) presentation
//! over the data model in `mcp-loadtest-core`; this crate depends only on
//! core.

pub mod grading;
pub mod regression;
pub mod report;

/// Live-polling Ratatui dashboard for the CLI's `--watch` modes.
#[cfg(feature = "tui")]
pub mod tui;
