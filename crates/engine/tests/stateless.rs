//! Integration tests for the stateless 2026-07-28 connection mode (ADR 0019).
//!
//! Two layers of coverage:
//! - `httpmock`-backed client-contract tests pinning the wire shape: no
//!   `initialize`, `server/discover` at construct, request `_meta`, and the
//!   mandatory HTTP metadata headers.
//! - End-to-end runs against `mock-stateless-http.py` — a stateless server
//!   that *rejects* requests missing `_meta`, including the flagship
//!   deadlock-probe catch over the new protocol (`--lazy-deadlock`).

mod helpers;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use httpmock::prelude::*;
use mcp_loadtest_core::ProtocolVersion;
use mcp_loadtest_core::config::{Config, ScenarioConfig, ServerConfig};
use mcp_loadtest_engine::Run;
use mcp_loadtest_engine::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_protocol::transport::HostGuard;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::{Session, SessionError};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const META_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";

fn loopback_guard() -> HostGuard {
    let mut cfg = ServerConfig::stdio("python".into(), vec![]);
    cfg.allowed_hosts = vec!["127.0.0.1".to_string()];
    HostGuard::from_config(&cfg)
}

fn valid_discover_result() -> serde_json::Value {
    json!({
        "resultType": "complete",
        "supportedVersions": ["2026-07-28"],
        "capabilities": {},
        "ttlMs": 0,
        "cacheScope": "private",
    })
}

fn with_result_field(
    mut result: serde_json::Value,
    key: &str,
    value: serde_json::Value,
) -> serde_json::Value {
    result
        .as_object_mut()
        .expect("test result must be an object")
        .insert(key.to_string(), value);
    result
}

fn success_response(id: u64, result: serde_json::Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

async fn connect_final_stateless(server: &MockServer) -> Result<Session, SessionError> {
    let guard = loopback_guard();
    let transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect");
    tokio::time::timeout(
        TEST_TIMEOUT,
        Session::from_transport_stateless(
            transport,
            Duration::from_secs(10),
            ProtocolVersion::V2026_07_28,
        ),
    )
    .await
    .expect("stateless construction timed out")
}

/// Unique per-test run dir; removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-stateless-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).expect("create scratch dir");
        Self(p)
    }
    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn the stateless python fixture; returns (child, "127.0.0.1:<port>").
/// The child is `kill_on_drop` — the raw `Command` here is the established
/// pattern for HTTP fixtures (see `tests/host_guard.rs`); `Session::spawn`
/// only fits stdio MCP mocks.
async fn spawn_stateless_fixture(extra: &[&str]) -> (tokio::process::Child, String) {
    let py = helpers::python();
    let script = helpers::fixture_path("mock-stateless-http.py");
    let mut child = Command::new(&py)
        .arg(&script)
        .arg("--port")
        .arg("0")
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mock-stateless-http.py");
    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = BufReader::new(stdout).lines();
    let listening = tokio::time::timeout(TEST_TIMEOUT, lines.next_line())
        .await
        .expect("timed out waiting for LISTENING line")
        .expect("read stdout")
        .expect("server closed stdout before announcing");
    let addr = listening
        .strip_prefix("LISTENING: ")
        .unwrap_or_else(|| panic!("unexpected first line: {listening}"))
        .trim()
        .to_string();
    (child, addr)
}

#[tokio::test]
async fn stateless_construct_sends_discover_with_meta_and_no_initialize() {
    let server = MockServer::start_async().await;
    let discover = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "server/discover")
                .body_contains("server/discover")
                .body_contains(META_VERSION_KEY)
                .body_contains("2026-07-28");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"public","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fixture","version":"1"}}}}"#,
                );
        })
        .await;
    let tools = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/list")
                .body_contains("tools/list")
                .body_contains(META_VERSION_KEY);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}"#);
        })
        .await;

    let guard = loopback_guard();
    let transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect");
    let mut session = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::from_transport_stateless(
            transport,
            Duration::from_secs(10),
            ProtocolVersion::V2026_07_28,
        ),
    )
    .await
    .expect("construct timed out")
    .expect("stateless construct failed");

    assert_eq!(discover.hits_async().await, 1);
    assert_eq!(
        session.negotiated_version(),
        Some(ProtocolVersion::V2026_07_28)
    );
    assert_eq!(session.server_protocol_version, "2026-07-28");

    let listed = session.list_tools().await.expect("list_tools");
    assert!(listed.iter().any(|t| t.name == "echo"));
    assert_eq!(tools.hits_async().await, 1);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn stateless_discover_rejects_invalid_final_result_metadata_without_retry() {
    let base = valid_discover_result();

    let mut missing_result_type = base.clone();
    missing_result_type
        .as_object_mut()
        .expect("fixture object")
        .remove("resultType");
    let mut missing_ttl = base.clone();
    missing_ttl
        .as_object_mut()
        .expect("fixture object")
        .remove("ttlMs");
    let mut missing_scope = base.clone();
    missing_scope
        .as_object_mut()
        .expect("fixture object")
        .remove("cacheScope");

    let cases = vec![
        ("missing-result-type", missing_result_type, "resultType"),
        (
            "non-string-result-type",
            with_result_field(base.clone(), "resultType", json!(1)),
            "resultType",
        ),
        (
            "non-complete-result-type",
            with_result_field(base.clone(), "resultType", json!("streaming")),
            "resultType",
        ),
        ("missing-ttl", missing_ttl, "ttlMs"),
        (
            "negative-ttl",
            with_result_field(base.clone(), "ttlMs", json!(-1)),
            "ttlMs",
        ),
        (
            "fractional-ttl",
            with_result_field(base.clone(), "ttlMs", json!(1.5)),
            "ttlMs",
        ),
        ("missing-cache-scope", missing_scope, "cacheScope"),
        (
            "unknown-cache-scope",
            with_result_field(base.clone(), "cacheScope", json!("shared")),
            "cacheScope",
        ),
        (
            "non-string-cache-scope",
            with_result_field(base, "cacheScope", json!(false)),
            "cacheScope",
        ),
    ];

    for (name, result, expected_field) in cases {
        let server = MockServer::start_async().await;
        let response = success_response(1, result);
        let discover = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("server/discover");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(response);
            })
            .await;

        let error = match connect_final_stateless(&server).await {
            Err(error) => error,
            Ok(_) => panic!("{name}: invalid discover result must fail construction"),
        };
        assert!(
            matches!(&error, SessionError::ResponseShape(_)),
            "{name}: expected ResponseShape, got {error:?}"
        );
        assert!(
            error.to_string().contains(expected_field),
            "{name}: diagnostic should identify {expected_field}: {error}"
        );
        assert_eq!(
            discover.hits_async().await,
            1,
            "{name}: malformed success must not be retried"
        );
    }
}

#[tokio::test]
async fn stateless_supported_methods_reject_invalid_final_result_metadata() {
    let cases = vec![
        (
            "list-missing-result-type",
            "tools/list",
            json!({
                "ttlMs": 0,
                "cacheScope": "private",
                "tools": [],
            }),
            "resultType",
        ),
        (
            "list-negative-ttl",
            "tools/list",
            json!({
                "resultType": "complete",
                "ttlMs": -1,
                "cacheScope": "private",
                "tools": [],
            }),
            "ttlMs",
        ),
        (
            "list-invalid-cache-scope",
            "tools/list",
            json!({
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "shared",
                "tools": [],
            }),
            "cacheScope",
        ),
        (
            "call-missing-result-type",
            "tools/call",
            json!({ "content": [] }),
            "resultType",
        ),
        (
            "call-non-string-result-type",
            "tools/call",
            json!({ "resultType": true, "content": [] }),
            "resultType",
        ),
        (
            "call-non-complete-result-type",
            "tools/call",
            json!({ "resultType": "streaming", "content": [] }),
            "resultType",
        ),
    ];

    for (name, method, result, expected_field) in cases {
        let server = MockServer::start_async().await;
        let discover_response = success_response(1, valid_discover_result());
        let discover = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("server/discover");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(discover_response);
            })
            .await;
        let method_response = success_response(2, result);
        let response = server
            .mock_async(move |when, then| {
                when.method(POST).path("/mcp").body_contains(method);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(method_response);
            })
            .await;

        let mut session = connect_final_stateless(&server)
            .await
            .unwrap_or_else(|error| panic!("{name}: valid discover failed: {error}"));
        let error = match method {
            "tools/list" => match session.list_tools().await {
                Err(error) => error,
                Ok(_) => panic!("{name}: invalid tools/list result must fail"),
            },
            "tools/call" => match session.call_tool("echo", &json!({})).await {
                Err(error) => error,
                Ok(_) => panic!("{name}: invalid tools/call result must fail"),
            },
            _ => unreachable!("test table only contains supported methods"),
        };

        assert!(
            matches!(&error, SessionError::ResponseShape(_)),
            "{name}: expected ResponseShape, got {error:?}"
        );
        assert!(
            error.to_string().contains(expected_field),
            "{name}: diagnostic should identify {expected_field}: {error}"
        );
        assert_eq!(discover.hits_async().await, 1, "{name}: discover count");
        assert_eq!(
            response.hits_async().await,
            1,
            "{name}: malformed success must not be retried"
        );
    }
}

#[tokio::test]
async fn legacy_handshake_keeps_accepting_results_without_final_only_fields() {
    let server = MockServer::start_async().await;
    let initialize = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp").body_contains("\"initialize\"");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"legacy","version":"1"}}}"#,
                );
        })
        .await;
    let initialized = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .body_contains("notifications/initialized");
            then.status(204);
        })
        .await;
    let tools = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp").body_contains("tools/list");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}"#,
                );
        })
        .await;

    let guard = loopback_guard();
    let transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect");
    let mut session = Session::from_transport(transport)
        .await
        .expect("legacy initialize result must not require final resultType/cache fields");
    let listed = session
        .list_tools()
        .await
        .expect("legacy tools/list result must remain accepted");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "echo");
    assert_eq!(initialize.hits_async().await, 1);
    assert_eq!(initialized.hits_async().await, 1);
    assert_eq!(tools.hits_async().await, 1);
}

#[tokio::test]
async fn pinned_stateless_rejects_server_without_discover() {
    // Servers MUST implement discover. This constructor is an explicit
    // modern-version pin, not the optional auto/fallback probe.
    let server = MockServer::start_async().await;
    let _discover = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp").body_contains("server/discover");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#);
        })
        .await;

    let guard = loopback_guard();
    let transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect");
    let error = match Session::from_transport_stateless(
        transport,
        Duration::from_secs(10),
        ProtocolVersion::V2026_07_28,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("-32601 discover must fail an explicitly pinned modern session"),
    };
    assert!(error.to_string().contains("-32601"), "unexpected: {error}");
}

#[tokio::test]
async fn stateless_run_end_to_end_against_python_fixture() {
    // Full Run::execute over the stateless fixture, which REJECTS requests
    // missing `_meta` — traffic flowing proves the block is on every call.
    let (child, addr) = spawn_stateless_fixture(&[]).await;
    let toml = format!(
        r#"
        [server]
        transport = "http"
        url = "http://{addr}/"
        allowed_hosts = ["127.0.0.1"]
        protocol_version = "2026-07-28"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let scratch = ScratchDir::new("e2e");
    let scenario = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    });

    let report = tokio::time::timeout(
        TEST_TIMEOUT,
        Run::new(config, scenario, scratch.path()).execute(),
    )
    .await
    .expect("run timed out")
    .expect("stateless run should complete");

    assert!(
        report.metrics.throughput.total_requests > 0,
        "expected traffic against the stateless mock, got {report:?}"
    );
    assert_eq!(
        report.metrics.outcomes.server_error, 0,
        "every call must carry _meta (fixture rejects otherwise): {report:?}"
    );
    assert_eq!(
        report.server_info.protocol_version.as_deref(),
        Some("2026-07-28")
    );
    drop(child);
}

#[tokio::test]
async fn deadlock_probe_catches_stateless_lazy_deadlock() {
    // The flagship bug class, stateless edition: hang_detect must classify
    // the wedged tools/call as a deadlock over the 2026-07-28 mode too.
    let (child, addr) = spawn_stateless_fixture(&["--lazy-deadlock"]).await;
    let toml = format!(
        r#"
        [server]
        transport = "http"
        url = "http://{addr}/"
        allowed_hosts = ["127.0.0.1"]
        protocol_version = "2026-07-28"
        [scenario]
        type = "deadlock_probe"
        tool = "echo"
        [thresholds]
        hang_timeout = "300ms"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let scratch = ScratchDir::new("deadlock");
    let scenario = Box::new(DeadlockProbe {
        concurrent: 3,
        hang_threshold: Duration::from_millis(300),
        grace_period: Duration::from_millis(600),
        tool: "echo".to_string(),
        args: json!({}),
    });

    let report = tokio::time::timeout(
        TEST_TIMEOUT,
        Run::new(config, scenario, scratch.path()).execute(),
    )
    .await
    .expect("run timed out")
    .expect("run should complete with a detected deadlock");

    assert!(
        report.scenario_outcome.deadlock_count >= 1,
        "expected the stateless lazy-init deadlock to be caught: {report:?}"
    );
    drop(child);
}

#[test]
fn stateless_config_is_rejected_on_sse_and_ws() {
    for transport in ["sse", "ws"] {
        let toml = format!(
            r#"
            [server]
            transport = "{transport}"
            url = "wss://example.com/mcp"
            protocol_version = "2026-07-28"
            [scenario]
            type = "sustained"
            tool = "echo"
            "#
        );
        let err = Config::from_toml_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("stateless"),
            "{transport}: expected the ADR 0019 scope error, got: {err}"
        );
    }
}

#[test]
fn version_matrix_default_set_stays_handshake_only() {
    // The stateless revision joins the matrix only when explicitly listed —
    // the default set must not silently start spawning stateless sessions.
    let cfg = ScenarioConfig::new("version_matrix", json!({ "tool": "echo" }));
    assert_eq!(cfg.kind, "version_matrix");
    assert!(
        !ProtocolVersion::SUPPORTED.contains(&ProtocolVersion::V2026_07_28),
        "SUPPORTED (the version_matrix default) must stay handshake-only"
    );
}
