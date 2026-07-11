//! Re-export of the report layer (data model in core, renderers in output).
pub use mcp_loadtest_core::report::*;
pub use mcp_loadtest_output::report::{html, json, markdown, terminal};
