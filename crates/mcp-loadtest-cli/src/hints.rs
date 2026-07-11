//! Actionable error hints (DESIGN §21.3).
//!
//! The library half of `mcp-loadtest` deliberately keeps its error enums
//! clean `thiserror` types with no UX prose baked in. This module lives in
//! the CLI crate, at the `anyhow` boundary, and maps each known typed error
//! to a single stable `&'static str` "next step" the way an LLM agent (or a
//! human) can act on — turning `BrokenPipe(Os { code: 32, .. })` into
//! "server closed the pipe — likely crashed at startup; capture it with
//! `--capture-stderr`".
//!
//! See [ADR 0014](../../../docs/adr/0014-error-hints-explain-doctor.md).

use mcp_loadtest::{ConfigError, ReportError, RunError, SessionError, TraceError, TransportError};

/// A typed error that can suggest a concrete next step.
///
/// `hint` returns `Some(msg)` when there is a specific, actionable
/// remediation for the variant, or `None` when the error is self-explanatory
/// (or wraps another typed error whose own `hint` should be consulted — those
/// arms delegate to the inner error's `hint`).
pub trait ErrorHint {
    /// The actionable next step for this error, if any.
    fn hint(&self) -> Option<&'static str>;
}

impl ErrorHint for TransportError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            // F1's SSRF guard rejects with a stable `Other(_)` message that
            // contains "blocked host" (and an "ADR 0012" cite). Match the
            // substring rather than a variant — the enum is `#[non_exhaustive]`
            // and `Other` is the agreed carrier (see ADR 0012 / 0014).
            TransportError::Other(m) if m.contains("blocked host") => Some(
                "host blocked by the SSRF guard — add it to `[server].allowed_hosts` \
                 if you trust it (ADR 0012)",
            ),
            // A torn pipe / closed connection is, in practice, the server
            // dying during the initialize handshake.
            TransportError::Io(_) | TransportError::Closed => Some(
                "server closed the pipe — likely crashed at startup; capture it with \
                 `--capture-stderr` (or `--tee-stderr` to see it live)",
            ),
            TransportError::Timeout(_) => Some(
                "transport timed out — the server is slow or hung; re-run with \
                 `--tee-stderr` and check the server's own logs",
            ),
            TransportError::Http(_) => Some(
                "HTTP transport error — verify the server URL and that the endpoint \
                 speaks MCP over HTTP; `mcp-loadtest run --explain` describes the config",
            ),
            _ => None,
        }
    }
}

impl ErrorHint for SessionError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            SessionError::StartupTimeout(_) => Some(
                "server did not answer `initialize` in time — it likely crashed or \
                 hung during startup; re-run with `--tee-stderr` or read \
                 `runs/<id>/server-stderr.log`",
            ),
            // Pipe gone before/during the handshake.
            SessionError::Io(_) => Some(
                "server closed the pipe — likely crashed at startup; capture it with \
                 `--capture-stderr` (or `--tee-stderr` to see it live)",
            ),
            SessionError::Json(_) => Some(
                "could not parse the server's reply as JSON-RPC — the server may be \
                 emitting non-protocol output on stdout; capture stderr with \
                 `--capture-stderr`",
            ),
            SessionError::IdMismatch { .. } => Some(
                "server replied with a mismatched JSON-RPC id — it is not correlating \
                 request/response ids correctly (a server bug)",
            ),
            SessionError::SchemaViolation { .. } => Some(
                "tool arguments failed strict schema validation — fix the args or \
                 disable `[validation] strict`; `mcp-loadtest run --explain` covers it",
            ),
            // Delegate through wrapped typed errors so the most specific hint
            // (e.g. the SSRF one on a wrapped TransportError) still surfaces.
            SessionError::Transport(t) => t.hint(),
            SessionError::Server(_) => None,
            _ => None,
        }
    }
}

impl ErrorHint for RunError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            // Delegate to the wrapped session error (which itself delegates
            // to a wrapped transport error where relevant).
            RunError::Session(s) => s.hint(),
            RunError::Io(_) => Some(
                "I/O error writing run artifacts — check the output directory exists \
                 and is writable",
            ),
            RunError::Config(_) => Some(
                "invalid run config — `mcp-loadtest example-config` prints a \
                 known-good template",
            ),
            _ => None,
        }
    }
}

impl ErrorHint for ConfigError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            ConfigError::Toml(_) => {
                Some("malformed TOML — run `mcp-loadtest example-config` for a known-good template")
            }
            ConfigError::Invalid(_) => Some(
                "a config field is invalid — `mcp-loadtest <cmd> --explain` describes \
                 the expected inputs",
            ),
            ConfigError::Io(_) => {
                Some("could not read the config file — check the path and permissions")
            }
            _ => None,
        }
    }
}

impl ErrorHint for TraceError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            TraceError::Io(_) => Some(
                "could not read/write the trace file — check the path exists and is \
                 readable/writable",
            ),
            TraceError::Format(_) => Some(
                "the file is not an mcp-trace/1 JSONL trace — record one with \
                 `mcp-loadtest run --config <cfg> --trace <file>`",
            ),
            TraceError::UnsupportedFormat { .. } => Some(
                "the trace was written in a format version this build doesn't read — \
                 re-record it with this build (`mcp-loadtest run --trace <file>`)",
            ),
            _ => None,
        }
    }
}

impl ErrorHint for ReportError {
    fn hint(&self) -> Option<&'static str> {
        match self {
            ReportError::Io(_) => Some(
                "I/O error writing the report — check the output directory exists and \
                 is writable",
            ),
            ReportError::Json(_) => {
                Some("failed to serialize the report as JSON — please file a bug")
            }
            _ => None,
        }
    }
}

/// Print `err` (and its full source chain) to stderr, then — if any link in
/// the chain is a known typed error with an actionable [`ErrorHint`] — append
/// a single `Hint: <…>` line.
///
/// Only the **first** (most-specific) hint found while walking the chain is
/// printed: the chain is ordered outermost-first, and the wrapper arms in the
/// `ErrorHint` impls already delegate inward, so the first `Some` is the most
/// precise advice available. A bare `anyhow::anyhow!("…")` (no typed source)
/// yields no hint by design — that is how `doctor` reports its own summary
/// failure without a spurious `Hint:`.
pub fn print_with_hint(err: &anyhow::Error) {
    eprintln!("Error: {err}");
    for cause in err.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }

    if let Some(hint) = first_hint(err) {
        eprintln!("Hint: {hint}");
    }
}

/// Walk the `anyhow` chain outermost-first and return the first actionable
/// hint from a known typed error. Kept separate so it is unit-testable
/// without capturing stderr.
fn first_hint(err: &anyhow::Error) -> Option<&'static str> {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<RunError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
        if let Some(e) = cause.downcast_ref::<SessionError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
        if let Some(e) = cause.downcast_ref::<TransportError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
        if let Some(e) = cause.downcast_ref::<ConfigError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
        if let Some(e) = cause.downcast_ref::<ReportError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
        if let Some(e) = cause.downcast_ref::<TraceError>()
            && let Some(h) = e.hint()
        {
            return Some(h);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn startup_timeout_has_hint() {
        let e = SessionError::StartupTimeout(Duration::from_secs(10));
        let h = e.hint().expect("startup timeout should have a hint");
        assert!(h.contains("initialize"));
        assert!(h.contains("server-stderr.log") || h.contains("--tee-stderr"));
    }

    #[test]
    fn transport_closed_has_hint() {
        let e = TransportError::Closed;
        let h = e.hint().expect("closed transport should have a hint");
        assert!(h.contains("--capture-stderr"));
    }

    #[test]
    fn blocked_host_other_maps_to_ssrf_hint() {
        let e = TransportError::Other(
            "blocked host `169.254.169.254`: link-local address (SSRF guard, ADR 0012)".into(),
        );
        let h = e
            .hint()
            .expect("blocked-host Other should map to the SSRF hint");
        assert!(
            h.contains("allowed_hosts"),
            "SSRF hint should point at `[server].allowed_hosts`, got: {h}"
        );
    }

    #[test]
    fn toml_error_has_hint() {
        // Produce a real `ConfigError::Toml` via the library's own parser
        // (the CLI crate doesn't depend on `toml` directly, and constructing
        // a `toml::de::Error` by hand isn't possible without it). Broken
        // TOML syntax routes through `#[from] toml::de::Error`.
        let e = mcp_loadtest::Config::from_toml_str("this is = not = valid = toml")
            .expect_err("broken TOML must fail to parse");
        assert!(
            matches!(e, ConfigError::Toml(_)),
            "expected ConfigError::Toml, got {e:?}"
        );
        let h = e.hint().expect("a TOML parse error should have a hint");
        assert!(h.contains("example-config"));
    }

    #[test]
    fn invalid_config_has_hint() {
        let e = ConfigError::Invalid("scenario `nope` is unknown".into());
        let h = e
            .hint()
            .expect("an invalid-config error should have a hint");
        assert!(h.contains("--explain"));
    }

    #[test]
    fn run_error_delegates_to_wrapped_session_then_transport() {
        // RunError -> SessionError::Transport -> the SSRF Other(_) hint.
        let inner = TransportError::Other("blocked host `x`: (SSRF guard, ADR 0012)".into());
        let e = RunError::Session(SessionError::Transport(inner));
        let h = e
            .hint()
            .expect("RunError should delegate inward to the SSRF hint");
        assert!(h.contains("allowed_hosts"));
    }

    #[test]
    fn server_error_variant_has_no_hint() {
        // A structured server JSON-RPC error is self-explanatory; no hint.
        use mcp_loadtest::protocol::jsonrpc::ErrorObject;
        let e = SessionError::Server(ErrorObject {
            code: -32601,
            message: "method not found".into(),
            data: None,
        });
        assert!(e.hint().is_none());
    }

    #[test]
    fn first_hint_walks_anyhow_chain() {
        let typed = SessionError::StartupTimeout(Duration::from_secs(10));
        let wrapped: anyhow::Error =
            anyhow::Error::new(typed).context("running deadlock-probe against `python -m foo`");
        let h = first_hint(&wrapped).expect("chain walk should find the inner hint");
        assert!(h.contains("initialize"));
    }

    #[test]
    fn trace_format_errors_have_hints() {
        let e = TraceError::Format("line 2: not a trace frame".into());
        let h = e.hint().expect("format error should have a hint");
        assert!(h.contains("--trace"));

        let e = TraceError::UnsupportedFormat {
            got: "mcp-trace/9".into(),
            expected: "mcp-trace/1",
        };
        let h = e.hint().expect("version mismatch should have a hint");
        assert!(h.contains("re-record"));
    }

    #[test]
    fn bare_anyhow_message_yields_no_hint() {
        // doctor's own summary failure must NOT trigger a spurious Hint.
        let bare = anyhow::anyhow!("doctor: 2 check(s) failed");
        assert!(first_hint(&bare).is_none());
    }
}
