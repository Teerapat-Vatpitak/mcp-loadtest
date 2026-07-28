//! Semantic validation for [`super::Config`]. Pulled out of `config.rs` so the
//! schema file stays focused on the structs themselves. The free fn shape lets
//! `Config::from_toml_str` call it without an `impl` redirection.

use super::{Config, ConfigError};
use crate::version::ProtocolVersion;
use url::Url;

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

    validate_remote_headers(cfg)?;

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

    if matches!(cfg.server.transport.as_str(), "http" | "sse" | "ws") {
        let raw_url = cfg
            .server
            .url
            .as_deref()
            .expect("remote transports were checked for a URL above");
        validate_remote_endpoint(
            raw_url,
            &cfg.server.transport,
            !cfg.server.headers_from_env.is_empty(),
        )
        .map_err(|message| ConfigError::Invalid(format!("server.url: {message}")))?;
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
        && (!growth.is_finite() || growth < 0.0)
    {
        return Err(ConfigError::Invalid(format!(
            "thresholds.memory_growth_mb: must be finite and >= 0, got {growth}"
        )));
    }
    if let Some(slope) = cfg.thresholds.rss_leak_mb_per_sec
        && (!slope.is_finite() || slope < 0.0)
    {
        return Err(ConfigError::Invalid(format!(
            "thresholds.rss_leak_mb_per_sec: must be finite and >= 0, got {slope}"
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

/// Validate only header names and environment-variable references. Secret
/// values are intentionally not read during config parsing: a config may be
/// validated in a different process/environment from the eventual run.
fn validate_remote_headers(cfg: &Config) -> Result<(), ConfigError> {
    if !cfg.server.headers_from_env.is_empty() && cfg.server.transport == "stdio" {
        return Err(ConfigError::Invalid(
            "server.headers_from_env is only supported for http, sse, and ws transports".into(),
        ));
    }

    let mut names = std::collections::BTreeSet::new();
    for (name, env_name) in &cfg.server.headers_from_env {
        let folded = name.to_ascii_lowercase();
        if !is_http_token(name) {
            return Err(ConfigError::Invalid(format!(
                "server.headers_from_env: `{name}` is not a valid HTTP header name"
            )));
        }
        if is_managed_remote_header(&folded) {
            return Err(ConfigError::Invalid(format!(
                "server.headers_from_env: `{name}` is managed by mcp-loadtest and cannot be overridden"
            )));
        }
        if !names.insert(folded) {
            return Err(ConfigError::Invalid(format!(
                "server.headers_from_env: duplicate header name `{name}` (header names are case-insensitive)"
            )));
        }
        if !is_portable_env_name(env_name) {
            return Err(ConfigError::Invalid(format!(
                "server.headers_from_env: `{env_name}` is not a portable environment-variable name"
            )));
        }
    }
    Ok(())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether an outbound header is owned by HTTP, the MCP protocol, or the
/// transport stack and therefore cannot be supplied through remote-auth
/// environment references.
///
/// Kept in the core config layer so parse-time and transport-time validation
/// share one future-proof denylist.
pub fn is_managed_remote_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || name.starts_with("mcp-")
        || name.starts_with("sec-websocket-")
}

/// Parse and enforce the security policy shared by config validation and all
/// direct remote-transport constructors.
///
/// The returned diagnostics deliberately describe the violated rule without
/// echoing `raw`, because URL userinfo and query strings can contain
/// credentials. Static remote headers are allowed only over TLS.
pub fn validate_remote_endpoint(
    raw: &str,
    transport: &str,
    has_remote_headers: bool,
) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "remote endpoint is not a valid absolute URL")?;

    let scheme_is_valid = match transport {
        "http" | "sse" => matches!(url.scheme(), "http" | "https"),
        "ws" => matches!(url.scheme(), "ws" | "wss"),
        _ => false,
    };
    if !scheme_is_valid {
        return Err(match transport {
            "http" | "sse" => "remote endpoint scheme must be http:// or https://",
            "ws" => "remote endpoint scheme must be ws:// or wss://",
            _ => "remote endpoint transport is unsupported",
        }
        .to_string());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("remote endpoint must include a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("remote endpoint URL userinfo is forbidden".into());
    }
    if url.fragment().is_some() {
        return Err("remote endpoint URL fragments are forbidden".into());
    }
    if has_remote_headers {
        let tls_scheme = match transport {
            "http" | "sse" => "https",
            "ws" => "wss",
            _ => unreachable!("unsupported transport was rejected above"),
        };
        if url.scheme() != tls_scheme {
            return Err(format!(
                "server.headers_from_env requires the {tls_scheme}:// scheme"
            ));
        }
    }

    Ok(url)
}

/// Render a remote endpoint without credential-bearing URL components.
///
/// Userinfo and fragments are removed. If any query is present, the whole
/// query is replaced with the literal marker `redacted`; individual names and
/// values are intentionally not retained. Invalid URLs collapse to a fixed
/// marker so their raw text cannot leak through a report.
pub fn sanitize_remote_endpoint(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "<invalid remote endpoint>".into();
    };
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return "<invalid remote endpoint>".into();
    }
    url.set_fragment(None);
    if url.query().is_some() {
        url.set_query(Some("redacted"));
    }
    url.to_string()
}

fn is_portable_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
