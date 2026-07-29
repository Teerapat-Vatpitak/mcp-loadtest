//! Report renderers (Markdown / JSON / HTML / terminal). The `Report` data
//! model, the `Reporter` trait, and `format_iso8601_utc` live in
//! `mcp-loadtest-core` and are re-exported here for the renderers' internal
//! convenience.

pub mod html;
pub mod json;
pub mod junit;
pub mod markdown;
pub mod otlp;
pub mod prometheus;
pub mod terminal;
pub mod wire;

pub(crate) mod common;

pub use mcp_loadtest_core::report::*;
