//! Trace on-disk format (ADR 0021, plan task T3.3).
//!
//! The record + replay machinery (`mcp_loadtest::trace::writer` /
//! `mcp_loadtest::trace::replay`) needs a Tokio runtime and `Session`/
//! `Transport` types, so it stays in the `mcp-loadtest` crate. This crate
//! owns just the pure pieces both sides share:
//!
//! - [`mod@format`] — the on-disk JSONL schema ([`format::TraceHeader`] +
//!   [`format::TraceFrame`]) and the default-on secret redaction.
//! - [`TraceError`] — errors from trace recording, parsing, or replay.

pub mod format;

use thiserror::Error;

/// Errors from trace recording, parsing, or replay.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TraceError {
    /// I/O failure reading or writing the trace file.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure on a header or an outgoing frame.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The file is not a parseable mcp-trace JSONL document.
    #[error("trace format: {0}")]
    Format(String),
    /// The header declares a format version this build doesn't read.
    #[error("unsupported trace format `{got}` (this build reads `{expected}`)")]
    UnsupportedFormat {
        /// The `format` value found in the header line.
        got: String,
        /// The format version this build supports.
        expected: &'static str,
    },
}
