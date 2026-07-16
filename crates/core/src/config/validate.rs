//! Semantic validation for [`super::Config`]. Pulled out of `config.rs` so the
//! schema file stays focused on the structs themselves. The free fn shape lets
//! `Config::from_toml_str` call it without an `impl` redirection.

use super::{Config, ConfigError};
use crate::version::ProtocolVersion;

/// Transports recognized by the parser + runtime.
pub(super) const KNOWN_TRANSPORTS: &[&str] = &["stdio", "http", "sse", "ws"];

/// Scenario kinds the parser knows about. Mirrors §8 of DESIGN.md and the
/// modules actually shipped under `src/scenario/`. Keep this in sync with the
/// `pub mod` lines in `scenario/mod.rs` so TOML configs documented in the
/// README parse cleanly.
pub(super) const KNOWN_SCENARIOS: &[&str] = &[
    "sustained",
    "deadlock_probe",
    "cold_start",
    "ramp",
    "soak",
    "spike",
    "fuzzer",
    "race_check",
    "pattern",
    "version_matrix",
];

/// Run semantic checks on an already-parsed `Config`. Pulled out so tests
/// can poke individual rules without re-parsing.
pub(super) fn validate(cfg: &Config) -> Result<(), ConfigError> {
    // server.transport must be a known string.
    if !KNOWN_TRANSPORTS.contains(&cfg.server.transport.as_str()) {
        return Err(ConfigError::Invalid(format!(
            "server.transport: unknown transport `{}` (expected one of: {})",
            cfg.server.transport,
            KNOWN_TRANSPORTS.join(", ")
        )));
    }

    // Transport-specific field requirements.
    match cfg.server.transport.as_str() {
        "stdio" if cfg.server.command.is_none() => {
            return Err(ConfigError::Invalid(
                "server.command is required when transport = \"stdio\"".into(),
            ));
        }
        "http" | "sse" | "ws" if cfg.server.url.is_none() => {
            return Err(ConfigError::Invalid(format!(
                "server.url is required when transport = \"{}\"",
                cfg.server.transport
            )));
        }
        _ => {}
    }

    // allowed_hosts entries must be bare hostnames / IP literals — no
    // scheme, port, path, or whitespace. A non-bare entry would never match
    // `Url::host_str()` and is almost certainly an operator mistake (e.g.
    // pasting a full URL). Reject early with an actionable message
    // (SSRF guard, ADR 0012).
    for e in &cfg.server.allowed_hosts {
        let bad = e.is_empty()
            || e.contains("://")
            || e.contains('/')
            || e.contains(':')
            || e.chars().any(char::is_whitespace);
        if bad {
            return Err(ConfigError::Invalid(format!(
                "allowed_hosts entry `{e}` must be a bare hostname (no scheme/port/path)"
            )));
        }
    }

    // protocol_version must be "auto" or a known revision (ADR 0018).
    if let Some(v) = &cfg.server.protocol_version
        && v != "auto"
    {
        let Some(parsed) = ProtocolVersion::parse(v) else {
            let known: Vec<&str> = ProtocolVersion::ALL.iter().map(|p| p.as_str()).collect();
            return Err(ConfigError::Invalid(format!(
                "server.protocol_version: unsupported revision `{v}` (expected \"auto\" or one of: {})",
                known.join(", ")
            )));
        };
        // Stateless mode ships for stdio + Streamable HTTP only
        // (ADR 0019 scope): SSE is the legacy transport the stateless core
        // retires, WS support is deferred.
        if parsed.is_stateless() && matches!(cfg.server.transport.as_str(), "sse" | "ws") {
            return Err(ConfigError::Invalid(format!(
                "server.protocol_version: `{v}` (stateless) is not supported on the `{}` \
                 transport — use \"stdio\" or \"http\" (ADR 0019)",
                cfg.server.transport
            )));
        }
    }

    // scenario.type must be a known scenario kind.
    if !KNOWN_SCENARIOS.contains(&cfg.scenario.kind.as_str()) {
        return Err(ConfigError::Invalid(format!(
            "scenario.type: unknown scenario `{}` (expected one of: {})",
            cfg.scenario.kind,
            KNOWN_SCENARIOS.join(", ")
        )));
    }

    // Thresholds: every Some(_) value must be sensible.
    if let Some(rate) = cfg.thresholds.error_rate
        && !(0.0..=1.0).contains(&rate)
    {
        return Err(ConfigError::Invalid(format!(
            "thresholds.error_rate: must be in [0.0, 1.0], got {rate}"
        )));
    }
    if let Some(growth) = cfg.thresholds.memory_growth_mb
        && growth < 0.0
    {
        return Err(ConfigError::Invalid(format!(
            "thresholds.memory_growth_mb: must be >= 0, got {growth}"
        )));
    }
    if let Some(slope) = cfg.thresholds.rss_leak_mb_per_sec
        && slope < 0.0
    {
        return Err(ConfigError::Invalid(format!(
            "thresholds.rss_leak_mb_per_sec: must be >= 0, got {slope}"
        )));
    }
    // Duration is unsigned (`std::time::Duration`) so it can't be negative;
    // we don't need to check for negative latencies. We do check for the
    // pathological `0` case on the per-call hang timeout which would make
    // every call instantly hang.
    if let Some(d) = cfg.thresholds.hang_timeout
        && d.is_zero()
    {
        return Err(ConfigError::Invalid(
            "thresholds.hang_timeout: must be > 0".to_string(),
        ));
    }

    Ok(())
}
