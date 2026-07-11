//! Real-time terminal UI dashboard for live runs.
//!
//! See DESIGN.md §10.5 (differentiator entry) and §21.4 (`--explain` flag).
//!
//! M6 ownership: Agent Q. Other agents leave this alone.
//!
//! # Usage
//!
//! ```ignore
//! use mcp_loadtest::tui::Dashboard;
//! use mcp_loadtest::metrics::Recorder;
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn _example() {
//! let metrics = Recorder::new();
//! let cancel = CancellationToken::new();
//! let dashboard = Dashboard::new(
//!     metrics.clone(),
//!     cancel.clone(),
//!     "sustained".to_string(),
//!     "python -m my_mcp".to_string(),
//! );
//! // Spawn alongside the Run::execute() future. Cancellation flows both ways.
//! let _ = tokio::spawn(dashboard.run());
//! # }
//! ```
//!
//! The dashboard polls `Recorder::snapshot()` every ~250 ms and redraws.
//! Quit on `q` or `Esc` — quitting cancels the shared `CancellationToken`
//! so the run terminates gracefully.

pub mod dashboard;

pub use dashboard::{Dashboard, render_frame};
