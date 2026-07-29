//! Editor-facing JSON Schema for the TOML configuration file.
//!
//! The checked-in document is the canonical contract. Embedding it keeps the
//! CLI's `config-schema` output byte-for-byte deterministic and lets editors
//! use the same schema that release artifacts publish.

use serde_json::Value;

/// Draft 2020-12 JSON Schema for an mcp-loadtest v0.2 configuration.
pub const CONFIG_SCHEMA_JSON: &str = include_str!("../../../../docs/schema/config.v1.json");

/// Parse the embedded configuration schema.
///
/// The document is validated by the core crate's tests, so a parse failure
/// here indicates a broken build artifact rather than user input.
#[must_use]
pub fn config_schema() -> Value {
    serde_json::from_str(CONFIG_SCHEMA_JSON)
        .expect("docs/schema/config.v1.json must contain valid JSON")
}

/// Render the embedded schema in stable pretty-printed form with a final LF.
#[must_use]
pub fn config_schema_pretty() -> String {
    let mut rendered =
        serde_json::to_string_pretty(&config_schema()).expect("JSON values always serialize");
    rendered.push('\n');
    rendered
}
