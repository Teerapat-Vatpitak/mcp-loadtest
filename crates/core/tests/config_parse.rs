//! Integration tests for `mcp_loadtest_core::config` parsing + validation.
//!
//! Covers DESIGN.md §7 schema. Negative cases assert `ConfigError::Invalid`
//! is returned (not a panic, not `Toml`), which is the contract callers can
//! pattern-match on.

use std::io::Write;
use std::time::Duration;

use mcp_loadtest_core::config::{self, Config, ConfigError};

#[test]
fn parses_minimal_valid() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"
    "#;
    let cfg = Config::from_toml_str(toml_in).expect("minimal config must parse");
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert!(cfg.server.args.is_empty(), "args defaults to empty vec");
    assert_eq!(cfg.server.transport, "stdio", "transport defaults to stdio");
    assert_eq!(
        cfg.server.startup_timeout,
        Duration::from_secs(10),
        "startup_timeout defaults to 10s"
    );
    assert_eq!(cfg.scenario.kind, "sustained");
    // thresholds + output use Default impls
    assert!(cfg.thresholds.p99_latency.is_none());
    assert_eq!(cfg.output.report_dir.to_string_lossy(), "./runs");
}

#[test]
fn parses_full_example() {
    // The contract: example_config() must round-trip.
    let s = config::example_config();
    let cfg = Config::from_toml_str(&s).expect("example_config must parse");

    // Spot-check a few non-default fields to make sure example_config is
    // actually exercising every section.
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert_eq!(cfg.server.args, vec!["-m", "my_mcp"]);
    assert_eq!(
        cfg.server.env.get("LOG_LEVEL").map(String::as_str),
        Some("warn")
    );
    assert_eq!(cfg.scenario.kind, "sustained");
    assert_eq!(cfg.thresholds.p99_latency, Some(Duration::from_millis(500)));
    assert_eq!(cfg.thresholds.error_rate, Some(0.01));
    assert_eq!(
        cfg.output.formats,
        vec!["terminal".to_string(), "markdown".into(), "json".into()]
    );
}

#[test]
fn humantime_durations_parse() {
    let toml_in = r#"
        [server]
        command = "python"
        startup_timeout = "5s"

        [scenario]
        type = "sustained"

        [thresholds]
        p99_latency = "100ms"
        hang_timeout = "2s"
    "#;
    let cfg = Config::from_toml_str(toml_in).expect("humantime durations must parse");
    assert_eq!(cfg.server.startup_timeout, Duration::from_secs(5));
    assert_eq!(cfg.thresholds.p99_latency, Some(Duration::from_millis(100)));
    assert_eq!(cfg.thresholds.hang_timeout, Some(Duration::from_secs(2)));
}

#[test]
fn rejects_unknown_scenario() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "bogus"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("unknown scenario must be rejected");
    match err {
        ConfigError::Invalid(msg) => {
            assert!(
                msg.contains("scenario.type") && msg.contains("bogus"),
                "error message should mention the offending field + value, got: {msg}"
            );
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_invalid_error_rate() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"

        [thresholds]
        error_rate = 1.5
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("error_rate > 1.0 must be rejected");
    match err {
        ConfigError::Invalid(msg) => {
            assert!(
                msg.contains("error_rate"),
                "error message should mention error_rate, got: {msg}"
            );
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_negative_error_rate() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"

        [thresholds]
        error_rate = -0.1
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("error_rate < 0 must be rejected");
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn from_file_reads_disk() {
    // Use std::env::temp_dir() + UUID-ish suffix to avoid pulling in tempfile
    // for one test (per Agent E task notes).
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        // nanos gives us enough entropy for parallel test runs; we don't
        // need cryptographic uniqueness here.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = std::env::temp_dir().join(format!("mcp-loadtest-config-{suffix}.toml"));

    let body = r#"
        [server]
        command = "python"
        args = ["-m", "demo"]

        [scenario]
        type = "deadlock_probe"
    "#;

    {
        let mut f = std::fs::File::create(&path).expect("create tmp file");
        f.write_all(body.as_bytes()).expect("write tmp file");
    }

    let cfg = Config::from_file(&path).expect("from_file must read + parse");
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert_eq!(cfg.server.args, vec!["-m", "demo"]);
    assert_eq!(cfg.scenario.kind, "deadlock_probe");

    // Best-effort cleanup; Windows tempdir doesn't auto-purge.
    let _ = std::fs::remove_file(&path);
}

#[test]
fn from_file_missing_path_is_io_error() {
    let path = std::env::temp_dir().join("mcp-loadtest-does-not-exist.toml");
    // Belt and suspenders: make sure it really doesn't exist.
    let _ = std::fs::remove_file(&path);
    let err = Config::from_file(&path).expect_err("missing file must error");
    assert!(
        matches!(err, ConfigError::Io(_)),
        "expected ConfigError::Io, got {err:?}"
    );
}

#[test]
fn rejects_unknown_transport() {
    let toml_in = r#"
        [server]
        command = "python"
        transport = "carrier-pigeon"

        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("unknown transport must be rejected");
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn rejects_ws_without_url() {
    // `ws` is a URL transport like http/sse — a config without `server.url`
    // can never connect and must be rejected at load, not at runtime.
    let toml_in = r#"
        [server]
        transport = "ws"

        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("ws without url must be rejected");
    match err {
        ConfigError::Invalid(msg) => {
            assert!(
                msg.contains("server.url") && msg.contains("ws"),
                "error message should mention server.url + the transport, got: {msg}"
            );
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn syntactic_toml_error_is_toml_variant() {
    // Unbalanced brackets: triggers the parser, not the validator.
    let toml_in = r#"
        [server
        command = "python"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("malformed TOML must error");
    assert!(
        matches!(err, ConfigError::Toml(_)),
        "expected ConfigError::Toml for syntax errors, got {err:?}"
    );
}

#[test]
fn parses_remote_headers_as_environment_references() {
    let toml_in = r#"
        [server]
        transport = "http"
        url = "https://mcp.example.test/mcp"
        headers_from_env = { Authorization = "MCP_AUTHORIZATION", "X-Tenant" = "MCP_TENANT" }

        [scenario]
        type = "sustained"
    "#;
    let cfg = Config::from_toml_str(toml_in).expect("header env references must parse");
    assert_eq!(
        cfg.server.headers_from_env.get("Authorization"),
        Some(&"MCP_AUTHORIZATION".to_string())
    );
    assert_eq!(
        cfg.server.headers_from_env.get("X-Tenant"),
        Some(&"MCP_TENANT".to_string())
    );
}

#[test]
fn rejects_remote_headers_for_stdio() {
    let toml_in = r#"
        [server]
        command = "python"
        headers_from_env = { Authorization = "MCP_AUTHORIZATION" }

        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("stdio headers must be rejected");
    assert!(
        matches!(err, ConfigError::Invalid(ref msg) if msg.contains("headers_from_env")),
        "unexpected error: {err}"
    );
}

#[test]
fn rejects_reserved_or_unsafe_remote_header_names() {
    for name in [
        "MCP-Protocol-Version",
        "Mcp-Method",
        "Mcp-Param-Tenant",
        "Mcp-Session-Id",
        "Content-Type",
        "Connection",
        "Keep-Alive",
        "Proxy-Authorization",
        "TE",
        "Trailer",
        "Transfer-Encoding",
        "Upgrade",
        "Bad Header",
    ] {
        let toml_in = format!(
            r#"
                [server]
                transport = "http"
                url = "https://mcp.example.test/mcp"
                [server.headers_from_env]
                "{name}" = "MCP_SECRET"

                [scenario]
                type = "sustained"
            "#
        );
        let err =
            Config::from_toml_str(&toml_in).expect_err("reserved/unsafe header must be rejected");
        assert!(
            matches!(err, ConfigError::Invalid(ref msg) if msg.contains("headers_from_env")),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn rejects_nonportable_header_environment_name() {
    let toml_in = r#"
        [server]
        transport = "http"
        url = "https://mcp.example.test/mcp"
        headers_from_env = { Authorization = "BAD=ENV" }

        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("invalid env name must be rejected");
    assert!(
        matches!(err, ConfigError::Invalid(ref msg) if msg.contains("environment-variable")),
        "unexpected error: {err}"
    );
}

#[test]
fn remote_url_policy_rejects_userinfo_without_echoing_credentials() {
    const SECRET: &str = "credential-sentinel-never-print";
    for (transport, scheme) in [("http", "https"), ("sse", "https"), ("ws", "wss")] {
        let toml_in = format!(
            r#"
                [server]
                transport = "{transport}"
                url = "{scheme}://operator:{SECRET}@mcp.example.test/rpc"

                [scenario]
                type = "sustained"
            "#
        );
        let err = Config::from_toml_str(&toml_in)
            .expect_err("URL userinfo must be rejected for every remote transport");
        let diagnostic = err.to_string();
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "{transport}: unexpected error: {diagnostic}"
        );
        assert!(diagnostic.contains("userinfo"), "{transport}: {diagnostic}");
        assert!(
            !diagnostic.contains(SECRET),
            "{transport}: credential leaked in diagnostic: {diagnostic}"
        );
    }
}

#[test]
fn remote_headers_require_tls_for_every_remote_transport() {
    for (transport, url) in [
        ("http", "http://mcp.example.test/rpc"),
        ("sse", "http://mcp.example.test/events"),
        ("ws", "ws://mcp.example.test/socket"),
    ] {
        let toml_in = format!(
            r#"
                [server]
                transport = "{transport}"
                url = "{url}"
                headers_from_env = {{ Authorization = "MCP_AUTHORIZATION" }}

                [scenario]
                type = "sustained"
            "#
        );
        let err =
            Config::from_toml_str(&toml_in).expect_err("secret-backed headers must require TLS");
        let diagnostic = err.to_string();
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "{transport}: unexpected error: {diagnostic}"
        );
        assert!(
            diagnostic.contains(if transport == "ws" {
                "wss://"
            } else {
                "https://"
            }),
            "{transport}: {diagnostic}"
        );
    }
}

#[test]
fn remote_url_policy_validates_scheme_host_and_fragment() {
    for (transport, url) in [
        ("http", "ftp://mcp.example.test/rpc"),
        ("sse", "https://"),
        ("ws", "wss://mcp.example.test/socket#credential-fragment"),
    ] {
        let toml_in = format!(
            r#"
                [server]
                transport = "{transport}"
                url = "{url}"

                [scenario]
                type = "sustained"
            "#
        );
        let err =
            Config::from_toml_str(&toml_in).expect_err("unsafe remote endpoint must be rejected");
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "{transport}: unexpected error: {err}"
        );
    }
}

#[test]
fn unauthenticated_plaintext_and_authenticated_tls_configs_remain_supported() {
    for (transport, plaintext, tls) in [
        (
            "http",
            "http://mcp.example.test/rpc",
            "https://mcp.example.test/rpc",
        ),
        (
            "sse",
            "http://mcp.example.test/events",
            "https://mcp.example.test/events",
        ),
        (
            "ws",
            "ws://mcp.example.test/socket",
            "wss://mcp.example.test/socket",
        ),
    ] {
        let unauthenticated = format!(
            r#"
                [server]
                transport = "{transport}"
                url = "{plaintext}"

                [scenario]
                type = "sustained"
            "#
        );
        Config::from_toml_str(&unauthenticated)
            .unwrap_or_else(|err| panic!("{transport}: plaintext without headers failed: {err}"));

        let authenticated = format!(
            r#"
                [server]
                transport = "{transport}"
                url = "{tls}"
                headers_from_env = {{ Authorization = "MCP_AUTHORIZATION" }}

                [scenario]
                type = "sustained"
            "#
        );
        Config::from_toml_str(&authenticated)
            .unwrap_or_else(|err| panic!("{transport}: TLS with headers failed: {err}"));
    }
}

#[test]
fn endpoint_display_drops_userinfo_fragment_and_whole_query() {
    const RAW: &str =
        "https://operator:credential@mcp.example.test/rpc?token=secret&tenant=private#fragment";
    let display = config::sanitize_remote_endpoint(RAW);
    assert_eq!(
        display, "https://mcp.example.test/rpc?redacted",
        "sanitizer should retain only non-secret endpoint identity"
    );
    for forbidden in [
        "operator",
        "credential",
        "token",
        "secret",
        "tenant",
        "private",
        "fragment",
        "#",
    ] {
        assert!(
            !display.contains(forbidden),
            "sanitized endpoint leaked `{forbidden}`: {display}"
        );
    }
    assert_eq!(
        config::sanitize_remote_endpoint("not a URL?secret=value"),
        "<invalid remote endpoint>"
    );
}

#[test]
fn unknown_server_auth_fields_fail_without_echoing_literal_values() {
    const SECRET: &str = "literal-credential-sentinel";
    for unknown in [
        format!(r#"headers = {{ Authorization = "{SECRET}" }}"#),
        format!(r#"header_from_env = {{ Authorization = "{SECRET}" }}"#),
        format!(
            r#"[server.headers]
                Authorization = "{SECRET}""#
        ),
    ] {
        let toml_in = format!(
            r#"
                [server]
                transport = "http"
                url = "https://mcp.example.test/rpc"
                {unknown}

                [scenario]
                type = "sustained"
            "#
        );
        let err = Config::from_toml_str(&toml_in).expect_err("unknown server auth field must fail");
        let diagnostic = err.to_string();
        assert!(
            matches!(err, ConfigError::Toml(_)),
            "expected a strict serde error, got: {diagnostic}"
        );
        assert!(
            !diagnostic.contains(SECRET),
            "unknown-field diagnostic leaked a literal value: {diagnostic}"
        );
    }
}
