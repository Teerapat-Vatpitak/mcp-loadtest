//! Analysis toolkit (split across core, engine, and output).
//!
//! - `coverage` / `fuzz_report` — pure data, in `mcp-loadtest-core`.
//! - `breaking_point` / `race_detector` — mid-run analyzers, in
//!   `mcp-loadtest-engine`.
//! - `grading` / `regression` — post-run analyzers, in `mcp-loadtest-output`.
pub use mcp_loadtest_core::{coverage, fuzz_report};
pub use mcp_loadtest_engine::{breaking_point, race_detector};
pub use mcp_loadtest_output::{grading, regression};
