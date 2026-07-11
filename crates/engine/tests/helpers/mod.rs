//! Shared test helpers. Living under `tests/helpers/` (not `tests/helpers.rs`)
//! so cargo doesn't treat it as another integration-test target.

use std::path::PathBuf;

/// Absolute path to a fixture file (e.g. `mock-normal.py`).
pub(crate) fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Resolve the Python interpreter to use for fixtures.
///
/// Override via the `MCP_LOADTEST_PYTHON` env var when CI / local has Python
/// installed under a non-default name.
pub(crate) fn python() -> String {
    std::env::var("MCP_LOADTEST_PYTHON").unwrap_or_else(|_| "python".to_string())
}
