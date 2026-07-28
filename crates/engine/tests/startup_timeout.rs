//! Regression: `config.server.startup_timeout` must actually govern the
//! `initialize` handshake budget.
//!
//! Before the fix, the field was parsed from TOML and then dropped —
//! `Session::from_transport` always used a hardcoded 10s constant, so setting
//! `startup_timeout` in config had no effect.
//!
//! The inline server sleeps 3s on `initialize`. That delay sits *between* the
//! test's configured budget (500ms — should fail fast) and the old hardcoded
//! default (10s — would wrongly succeed if the config value were ignored), so
//! each test discriminates the fix from the bug: with the bug, the run would
//! complete in ~3s instead of erroring with `StartupTimeout`.

// `helpers` is shared across integration-test binaries; this file uses only
// `python()`, so `fixture_path` is dead in this compilation unit.
#[expect(
    dead_code,
    reason = "shared helpers module; this binary uses only python(), leaving fixture_path unused here"
)]
mod helpers;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::config::Config;
use mcp_loadtest_engine::RunError;
use mcp_loadtest_engine::scenario::Scenario;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use mcp_loadtest_protocol::{Session, SessionError};
use serde_json::json;

/// Outer guard so a wedged handshake surfaces as a failure, not a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Configured budget under test — well below the 3s init sleep.
const BUDGET: Duration = Duration::from_millis(500);
/// Long enough for a contended Python spawn, but finite while tools/list
/// deliberately withholds its response.
const DISCOVERY_BUDGET: Duration = Duration::from_secs(3);

/// A stdlib-only MCP server that sleeps 3s on `initialize`, then behaves
/// normally. Passed via `python -c`, so there is no fixture-file dependency.
/// (No `format!` here, so single braces need no escaping.)
fn slow_init_server() -> String {
    r#"
import sys, json, time
def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line:
        break
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        time.sleep(3)
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"slow-init","version":"0.0.0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        send({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":json.dumps(args)}]}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
"#
    .to_string()
}

/// Initializes successfully, records proof that the handshake completed, then
/// deliberately withholds tools/list while continuing to read stdin. Closing
/// stdin during timeout cleanup therefore lets the process exit immediately.
fn no_tools_list_response_server() -> String {
    r#"
import sys, json, os
marker = sys.argv[1]
def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line:
        break
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        with open(marker, "w", encoding="utf-8") as f:
            f.write(str(os.getpid()))
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"no-list","version":"0.0.0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        pass
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
"#
    .to_string()
}

/// Unique, auto-cleaned scratch dir (pattern mirrors `tests/stderr_capture.rs`;
/// `tempfile` is not a dev-dep of this crate).
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-startup-{tag}-{}-{nanos}",
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

#[tokio::test]
async fn run_honors_configured_startup_timeout() {
    let py = helpers::python();
    let script = slow_init_server();
    // `startup_timeout = "500ms"` is the whole point: it must override the 10s
    // default and abort the 3s-sleeping handshake.
    let toml = format!(
        r#"
        [server]
        transport = "stdio"
        command = {py:?}
        args = ["-c", {script:?}]
        startup_timeout = "500ms"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let dir = ScratchDir::new("run");
    let scenario: Box<dyn Scenario> = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_string(),
        args: json!({}),
    });
    let run = mcp_loadtest_engine::Run::new(config, scenario, dir.path());

    let result = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run.execute hung (startup_timeout not applied?)");

    // The match below fully discriminates the fix from the bug, so no
    // wall-clock bound is asserted (any such bound is flaky: elapsed includes
    // the Python process spawn, which under a fully parallel suite can take
    // several seconds of CPU contention on its own):
    // - config ignored (old bug) → the 10s default lets the 3s init succeed
    //   and the run returns `Ok(report)` → caught by the match;
    // - wrong budget enforced → `StartupTimeout(other)` → caught by the
    //   `assert_eq!` on the budget.
    match result {
        Err(RunError::Session(SessionError::StartupTimeout(budget))) => {
            assert_eq!(
                budget, BUDGET,
                "the configured 500ms budget must be the one enforced, not the 10s default"
            );
        }
        other => panic!("expected StartupTimeout(500ms), got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_with_timeout_enforces_budget() {
    let py = helpers::python();
    let script = slow_init_server();

    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn_with_timeout(&py, ["-c", &script], SpawnOptions::inherit(), BUDGET),
    )
    .await
    .expect("spawn_with_timeout hung");

    // No wall-clock bound — see run_honors_configured_startup_timeout. A
    // 500ms budget against a 3s init can only end in StartupTimeout(500ms)
    // (enforced) or a ready session (not enforced); the match is sufficient.
    match result {
        Err(SessionError::StartupTimeout(budget)) => assert_eq!(budget, BUDGET),
        // Session is not Debug; map the Ok arm to a printable marker.
        other => panic!(
            "expected StartupTimeout(500ms), got {:?}",
            other.map(|_| "<ready session>")
        ),
    }
}

#[tokio::test]
async fn startup_timeout_includes_required_tools_list_and_cleans_up() {
    let py = helpers::python();
    let script = no_tools_list_response_server();
    let dir = ScratchDir::new("tools-list");
    let marker = dir.path().join("initialized.pid");
    let marker_arg = marker.to_string_lossy();
    let toml = format!(
        r#"
        [server]
        transport = "stdio"
        command = {py:?}
        args = ["-c", {script:?}, {marker_arg:?}]
        startup_timeout = "3s"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let scenario: Box<dyn Scenario> = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_owned(),
        args: json!({}),
    });
    let run = mcp_loadtest_engine::Run::new(config, scenario, dir.path());

    let result = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("tools/list startup timeout hung");
    assert!(
        marker.exists(),
        "fixture must prove initialize completed before tools/list stalled"
    );
    match result {
        Err(RunError::Session(SessionError::StartupTimeout(budget))) => {
            assert_eq!(budget, DISCOVERY_BUDGET);
        }
        other => panic!("expected tools/list StartupTimeout(3s), got {other:?}"),
    }
}

#[tokio::test]
async fn startup_timeout_bounds_sse_headers_stall() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled SSE peer");
    let addr = listener.local_addr().expect("listener address");
    let peer = tokio::spawn(async move {
        let Ok((_socket, _)) = listener.accept().await else {
            return;
        };
        std::future::pending::<()>().await;
    });

    let toml = format!(
        r#"
        [server]
        transport = "sse"
        url = "http://{addr}/events"
        allowed_hosts = ["127.0.0.1"]
        startup_timeout = "500ms"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let dir = ScratchDir::new("sse-headers");
    let scenario: Box<dyn Scenario> = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_owned(),
        args: json!({}),
    });
    let run = mcp_loadtest_engine::Run::new(config, scenario, dir.path());

    let result = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("SSE header stall escaped startup timeout");
    peer.abort();
    let _ = peer.await;

    match result {
        Err(RunError::Session(SessionError::StartupTimeout(budget))) => {
            assert_eq!(budget, BUDGET);
        }
        other => panic!("expected SSE-connect StartupTimeout(500ms), got {other:?}"),
    }
}
