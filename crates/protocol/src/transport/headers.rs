//! Secret-safe outbound headers for remote transports.
//!
//! Values are loaded from environment variables at connect time and are
//! intentionally never exposed through `Debug` or error messages. This is a
//! deliberately small authentication surface: static bearer/API-key/tenant
//! headers are supported; OAuth discovery, login, refresh, and token storage
//! are not.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mcp_loadtest_core::config::is_managed_remote_header;
use reqwest::header::{HeaderName, HeaderValue};

use super::TransportError;

/// Validated outbound headers whose values came from environment variables.
#[derive(Clone, Default)]
pub struct RemoteHeaders {
    entries: Vec<(HeaderName, HeaderValue)>,
}

impl RemoteHeaders {
    /// Resolve `header name -> environment variable name` references.
    ///
    /// Errors mention the header or environment-variable name, never the
    /// resolved secret value. Header values are restricted to RFC 9110-safe
    /// ASCII (plus interior horizontal tab) so they cannot inject a second
    /// header or be normalized differently by the HTTP and WebSocket stacks.
    pub fn from_env(references: &BTreeMap<String, String>) -> Result<Self, TransportError> {
        // Validate and case-fold the complete name set before consulting the
        // environment. Callers that construct this map directly (rather than
        // through Config) receive the same duplicate-name protection.
        let mut names = BTreeSet::new();
        let mut validated = Vec::with_capacity(references.len());
        for (name, env_name) in references {
            if !is_http_token(name) || is_managed_remote_header(name) {
                return Err(TransportError::Other(format!(
                    "remote header `{name}` is invalid or managed by mcp-loadtest"
                )));
            }
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(TransportError::Other(format!(
                    "remote header `{name}` duplicates another header name case-insensitively"
                )));
            }
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                TransportError::Other("remote header name is not a valid HTTP header name".into())
            })?;
            validated.push((name, env_name));
        }

        let mut entries = Vec::with_capacity(validated.len());
        for (name, env_name) in validated {
            let value = std::env::var(env_name).map_err(|error| match error {
                std::env::VarError::NotPresent => TransportError::Other(format!(
                    "environment variable `{env_name}` required for remote header `{}` is not set",
                    name.as_str()
                )),
                std::env::VarError::NotUnicode(_) => TransportError::Other(format!(
                    "environment variable `{env_name}` required for remote header `{}` is not valid Unicode",
                    name.as_str()
                )),
            })?;
            if !is_safe_header_value(&value) {
                return Err(TransportError::Other(format!(
                    "environment variable `{env_name}` contains a value that cannot be used as an HTTP header"
                )));
            }
            let mut value = HeaderValue::from_str(&value).map_err(|_| {
                TransportError::Other(format!(
                    "environment variable `{env_name}` contains a value that cannot be used as an HTTP header"
                ))
            })?;
            value.set_sensitive(true);
            entries.push((name, value));
        }
        Ok(Self { entries })
    }

    pub(crate) fn apply_reqwest(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in &self.entries {
            request = request.header(name.clone(), value.clone());
        }
        request
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&HeaderName, &HeaderValue)> {
        self.entries.iter().map(|(name, value)| (name, value))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    fn for_test(name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("test header name");
        let mut value = HeaderValue::from_str(value).expect("test header value");
        value.set_sensitive(true);
        Self {
            entries: vec![(name, value)],
        }
    }
}

impl fmt::Debug for RemoteHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteHeaders")
            .field(
                "names",
                &self
                    .entries
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("values", &"<redacted>")
            .finish()
    }
}

fn is_safe_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_values() {
        let headers = RemoteHeaders::for_test("Authorization", "Bearer very-secret");
        let debug = format!("{headers:?}");
        assert!(debug.contains("authorization"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("very-secret"));
    }

    #[test]
    fn header_value_validation_rejects_injection_bytes() {
        assert!(is_safe_header_value("Bearer abc.def"));
        assert!(!is_safe_header_value("secret\r\nInjected: yes"));
        assert!(!is_safe_header_value("non-ascii: สวัสดี"));
    }

    #[test]
    fn protocol_and_hop_by_hop_headers_are_reserved() {
        for name in [
            "Mcp-Session-Id",
            "Mcp-Future-Protocol-Field",
            "Connection",
            "Keep-Alive",
            "Proxy-Authorization",
            "TE",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
        ] {
            let references = BTreeMap::from([(name.to_owned(), "MCP_SECRET".to_owned())]);
            let err = RemoteHeaders::from_env(&references)
                .expect_err("managed header must be rejected before env lookup");
            assert!(
                err.to_string().contains("managed by mcp-loadtest"),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn direct_from_env_rejects_casefolded_duplicates_before_value_lookup() {
        let references = BTreeMap::from([
            ("Authorization".to_owned(), "MCP_SECRET_ONE".to_owned()),
            ("authorization".to_owned(), "MCP_SECRET_TWO".to_owned()),
        ]);
        let err = RemoteHeaders::from_env(&references)
            .expect_err("case-insensitive duplicate must fail before env lookup");
        assert!(err.to_string().contains("case-insensitively"), "{err}");
        assert!(!err.to_string().contains("not set"), "{err}");
    }

    #[test]
    fn from_env_resolves_and_applies_without_leaking_value() {
        const CHILD_FLAG: &str = "MCP_LOADTEST_HEADER_TEST_CHILD";
        const SECRET_NAME: &str = "MCP_LOADTEST_HEADER_TEST_SECRET";
        const TEST_NAME: &str =
            "transport::headers::tests::from_env_resolves_and_applies_without_leaking_value";

        if std::env::var(CHILD_FLAG).as_deref() != Ok("1") {
            let status = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args(["--exact", TEST_NAME])
            .env(CHILD_FLAG, "1")
            .env(SECRET_NAME, "Bearer synthetic-test-secret")
            .status()
            .expect("spawn isolated env test");
            assert!(status.success(), "isolated env test failed");
            return;
        }

        let references = BTreeMap::from([("Authorization".into(), SECRET_NAME.into())]);
        let headers = RemoteHeaders::from_env(&references).expect("resolve child environment");
        let debug = format!("{headers:?}");
        assert!(!debug.contains("synthetic-test-secret"));

        let request = headers
            .apply_reqwest(reqwest::Client::new().get("https://example.test/"))
            .build()
            .expect("build request");
        let request_debug = format!("{request:?}");
        assert!(
            !request_debug.contains("synthetic-test-secret"),
            "reqwest Request Debug leaked a sensitive header: {request_debug}"
        );
        assert_eq!(
            request
                .headers()
                .get("Authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer synthetic-test-secret")
        );
        assert!(
            request
                .headers()
                .get("Authorization")
                .is_some_and(HeaderValue::is_sensitive),
            "sensitivity marker must survive request construction"
        );

        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut ws_request = "wss://example.test/"
            .into_client_request()
            .expect("build WebSocket request");
        for (name, value) in headers.iter() {
            ws_request.headers_mut().insert(name.clone(), value.clone());
        }
        let ws_debug = format!("{ws_request:?}");
        assert!(
            !ws_debug.contains("synthetic-test-secret"),
            "WebSocket Request Debug leaked a sensitive header: {ws_debug}"
        );
        assert!(
            ws_request
                .headers()
                .get("Authorization")
                .is_some_and(HeaderValue::is_sensitive),
            "sensitivity marker must survive WebSocket request construction"
        );
    }

    #[tokio::test]
    async fn every_direct_remote_constructor_rejects_userinfo_before_io() {
        use mcp_loadtest_core::config::ServerConfig;

        use super::super::guard::HostGuard;
        use super::super::http::HttpTransport;
        use super::super::sse::SseTransport;
        use super::super::ws::WsTransport;

        const SECRET: &str = "url-password-sentinel";
        let guard = HostGuard::from_config(&ServerConfig::stdio("test".into(), Vec::new()));

        let http =
            HttpTransport::connect(format!("https://operator:{SECRET}@127.0.0.1:9/rpc"), &guard)
                .await
                .err()
                .expect("HTTP userinfo must be rejected");
        let sse = SseTransport::connect(
            format!("https://operator:{SECRET}@127.0.0.1:9/events"),
            &guard,
        )
        .await
        .err()
        .expect("SSE userinfo must be rejected");
        let ws = WsTransport::connect(
            &format!("wss://operator:{SECRET}@127.0.0.1:9/socket"),
            &guard,
        )
        .await
        .expect_err("WS userinfo must be rejected");

        for (transport, error) in [("http", http), ("sse", sse), ("ws", ws)] {
            let diagnostic = error.to_string();
            assert!(
                diagnostic.contains("userinfo"),
                "{transport}: wrong pre-I/O rejection: {diagnostic}"
            );
            assert!(
                !diagnostic.contains(SECRET),
                "{transport}: URL credential leaked: {diagnostic}"
            );
        }
    }

    #[tokio::test]
    async fn every_direct_remote_constructor_rejects_headers_over_plaintext_before_io() {
        use mcp_loadtest_core::config::ServerConfig;

        use super::super::guard::HostGuard;
        use super::super::http::HttpTransport;
        use super::super::sse::SseTransport;
        use super::super::ws::WsTransport;

        let guard = HostGuard::from_config(&ServerConfig::stdio("test".into(), Vec::new()));
        let headers = RemoteHeaders::for_test("Authorization", "Bearer test-secret");

        let http =
            HttpTransport::connect_with_headers("http://127.0.0.1:9/rpc", &guard, headers.clone())
                .await
                .err()
                .expect("HTTP headers over plaintext must be rejected");
        let sse = SseTransport::connect_with_headers(
            "http://127.0.0.1:9/events",
            &guard,
            headers.clone(),
        )
        .await
        .err()
        .expect("SSE headers over plaintext must be rejected");
        let ws = WsTransport::connect_with_headers("ws://127.0.0.1:9/socket", &guard, headers)
            .await
            .expect_err("WS headers over plaintext must be rejected");

        for (transport, error) in [("http", http), ("sse", sse), ("ws", ws)] {
            let diagnostic = error.to_string();
            assert!(
                diagnostic.contains(if transport == "ws" {
                    "wss://"
                } else {
                    "https://"
                }),
                "{transport}: wrong pre-I/O rejection: {diagnostic}"
            );
            assert!(
                !diagnostic.contains("test-secret"),
                "{transport}: header value leaked: {diagnostic}"
            );
        }
    }
}
