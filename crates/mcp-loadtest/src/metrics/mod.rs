//! Re-export of the metrics data model (lives in `mcp-loadtest-core`).
pub use mcp_loadtest_core::metrics::*;

/// Process sampling (lives in `mcp-loadtest-engine`).
pub mod process {
    pub use mcp_loadtest_engine::process::*;
}
