//! Scenario engine and run orchestration for mcp-loadtest.

pub mod breaking_point;
pub mod process;
pub mod race_detector;
pub mod run;
pub mod scenario;
pub mod trace;

pub use run::{Run, RunError, StderrCapture};
pub use scenario::{RunContext, Scenario, ScenarioOutcome};
