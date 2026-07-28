//! Integration tests for protocol-version negotiation (ADR 0018).
//!
//! Drives `mock-normal.py` with its `--protocol-version` knob to pin what
//! the server answers in `initialize`, then asserts the negotiation policy:
//! supported revisions are accepted (typed via `Session::negotiated_version`),
//! unknown revisions warn without failing the session, and a strict-mode
//! `Run` gates on them before any scenario traffic.

mod helpers;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::ProtocolVersion;
use mcp_loadtest_core::config::ScenarioConfig;
use mcp_loadtest_core::config::{Config, ServerConfig, ValidationConfig};
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::{Run, RunError};
use mcp_loadtest_protocol::{Session, SessionError};
use serde_json::json;

/// Unique per-test run dir so parallel tests never collide; removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-protover-{tag}-{}-{nanos}",
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

async fn spawn_with_version(version: Option<&str>) -> Session {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    let mut args: Vec<&OsStr> = vec![mock.as_os_str()];
    if let Some(v) = version {
        args.push(OsStr::new("--protocol-version"));
        args.push(OsStr::new(v));
    }
    Session::spawn(&py, args).await.expect("spawn failed")
}

#[tokio::test]
async fn default_handshake_negotiates_supported_version() {
    let session = spawn_with_version(None).await;
    assert_eq!(
        session.advertised_version(),
        ProtocolVersion::DEFAULT_ADVERTISED
    );
    assert_eq!(
        session.negotiated_version(),
        Some(ProtocolVersion::V2025_03_26)
    );
    assert_eq!(session.server_protocol_version, "2025-03-26");
    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn known_but_different_version_is_accepted() {
    let session = spawn_with_version(Some("2025-06-18")).await;
    assert_eq!(
        session.negotiated_version(),
        Some(ProtocolVersion::V2025_06_18)
    );
    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn server_matching_advertised_2025_11_25_negotiates_it() {
    let session = spawn_with_version(Some("2025-11-25")).await;
    assert_eq!(
        session.negotiated_version(),
        Some(ProtocolVersion::V2025_11_25)
    );
    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn unknown_version_warns_but_session_still_works() {
    let mut session = spawn_with_version(Some("9999-12-31")).await;
    // Permissive default (ADR 0018): typed form is None, raw string kept,
    // and the session remains fully usable.
    assert_eq!(session.negotiated_version(), None);
    assert_eq!(session.server_protocol_version, "9999-12-31");

    let tools = session.list_tools().await.expect("list_tools");
    assert!(tools.iter().any(|t| t.name == "echo"));
    let result = session
        .call_tool("echo", &json!({ "msg": "hi" }))
        .await
        .expect("call_tool");
    assert!(!result.is_error);
    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn strict_run_gates_on_unknown_version() {
    let scratch = ScratchDir::new("strict-gate");
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let server = ServerConfig::stdio(
        py,
        vec![
            mock.to_string_lossy().into_owned(),
            "--protocol-version".into(),
            "9999-12-31".into(),
        ],
    );
    let mut validation = ValidationConfig::default();
    validation.strict = true;
    let config = Config::new(
        server,
        ScenarioConfig::new("sustained", json!({ "tool": "echo" })),
    )
    .with_validation(validation);

    let scenario = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_millis(100),
        tool: "echo".to_string(),
        args: json!({}),
    });

    let err = Run::new(config, scenario, scratch.path())
        .execute()
        .await
        .expect_err("strict mode must gate on an unsupported protocol version");
    match err {
        RunError::Session(SessionError::UnsupportedProtocolVersion { got, advertised }) => {
            assert_eq!(got, "9999-12-31");
            assert_eq!(
                advertised,
                ProtocolVersion::DEFAULT_ADVERTISED.as_str(),
                "gate should report what the client advertised"
            );
        }
        other => panic!("expected UnsupportedProtocolVersion, got: {other:?}"),
    }
}
