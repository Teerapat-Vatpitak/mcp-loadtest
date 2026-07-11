//! Integration tests for the stateless 2026-07-28 connection mode (ADR 0019).
//!
//! Two layers of coverage:
//! - `httpmock`-backed client-contract tests pinning the wire shape: no
//!   `initialize`, `server/discover` at construct, and the RC `_meta` block
//!   on every request.
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
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::transport::HostGuard;
use mcp_loadtest_protocol::transport::http::HttpTransport;
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
                .body_contains("server/discover")
                .body_contains(META_VERSION_KEY)
                .body_contains("2026-07-28");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2026-07-28","protocolVersions":["2026-07-28"]}}"#,
                );
        })
        .await;
    let tools = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .body_contains("tools/list")
                .body_contains(META_VERSION_KEY);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}"#);
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

    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
}

#[tokio::test]
async fn stateless_tolerates_server_without_discover() {
    // RC: discover is optional / a backward-compat probe — a -32601 must not
    // fail construction, and subsequent requests still carry `_meta`.
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
    let session = Session::from_transport_stateless(
        transport,
        Duration::from_secs(10),
        ProtocolVersion::V2026_07_28,
    )
    .await
    .expect("-32601 discover must not fail construction");

    // Unconfirmed but permissive: raw string defaults to what we speak.
    assert_eq!(session.negotiated_version(), None);
    assert_eq!(session.server_protocol_version, "2026-07-28");

    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
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
