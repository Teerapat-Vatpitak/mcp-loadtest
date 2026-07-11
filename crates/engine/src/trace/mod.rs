//! Trace runtime (recording decorator + replay driver). The on-disk format
//! and [`TraceError`] live in `mcp-loadtest-core`.
//!
//! - [`writer`] — [`TraceWriter`] (the append side) and [`TracingTransport`]
//!   (a decorator recording through any
//!   [`Transport`](mcp_loadtest_protocol::transport::Transport)).
//! - [`replay`] — re-send recorded client frames through a fresh transport and
//!   diff responses via [`crate::race_detector`] canonicalization.
//!
//! See ADR 0021 for the format, redaction, and replay decisions.

pub mod replay;
pub mod writer;

pub use mcp_loadtest_core::trace::{TraceError, format};
pub use replay::{Divergence, ReplayReport};
pub use writer::{TraceWriter, TracingTransport};
