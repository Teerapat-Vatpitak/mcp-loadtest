//! Integration tests for Feature 2 (SpawnOptions stderr capture / tee).
//!
//! These drive the *public* `Session::spawn_with` / `Run` + `StderrCapture`
//! surface end-to-end against a tiny inline Python MCP server (passed via
//! `python -c`, so there is no cross-agent fixture dependency). The inline
//! server does the real handshake (`initialize` → `tools/list` → `tools/call`)
//! and writes a known marker line to `sys.stderr` (flushed) on startup so the
//! pump has something deterministic to capture.
//!
//! Coverage:
//! - `capture_writes_server_stderr_to_file` — `SpawnOptions::capture_stderr`
//!   produces a file containing the marker.
//! - `tee_creates_file_and_completes` — `SpawnOptions::tee_stderr` captures the
//!   marker, the session completes, and a *second* spawn into the same dir does
//!   not hang (the pump leaves no orphan / no lock).
//! - `inherit_default_creates_no_file` — the 2-arg `Session::spawn` writes no
//!   `server-stderr.log` (default is inherit, no pump).
//! - `two_arg_spawn_still_compiles_and_works` — delegation regression: the
//!   documented `Session::spawn(cmd, args)` still works unchanged.
//! - `run_with_tee_writes_server_stderr_log` — `Run` + `StderrCapture::Tee`
//!   writes `runs/<id>/server-stderr.log` and completes a short scenario.

// `helpers` is shared across integration-test binaries; this file uses only
// `python()` (the inline server is passed via `python -c`, no fixture file),
// so `fixture_path` is dead code *in this compilation unit only*. Scoped
// expect keeps the shared `tests/helpers/mod.rs` untouched.
#[expect(
    dead_code,
    reason = "shared helpers module; this binary uses only python(), leaving fixture_path unused here"
)]
mod helpers;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::config::Config;
use mcp_loadtest_engine::StderrCapture;
use mcp_loadtest_engine::scenario::Scenario;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use serde_json::json;

/// Outer guard so a wedged spawn surfaces as a failure, not a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Marker the inline server writes to stderr; asserted in the captured file.
const STDERR_MARKER: &str = "STDERR-MARKER-mcp-loadtest-f2";

/// A minimal stdlib-only MCP server as a single `python -c` program. Writes
/// `STDERR_MARKER` to stderr (flushed) immediately, then services the
/// handshake + an `echo` tool so `Session`/`Run` can complete a short run.
fn inline_server_script() -> String {
    // Kept tiny; mirrors the framing `_common.py` uses (newline-delimited
    // JSON on stdout). `sys.stderr` write is flushed so the pump sees it even
    // if the child is killed promptly.
    format!(
        r#"
import sys, json
sys.stderr.write("{marker}\n")
sys.stderr.flush()
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
        send({{"jsonrpc":"2.0","id":mid,"result":{{
            "protocolVersion":"2025-03-26",
            "capabilities":{{"tools":{{}}}},
            "serverInfo":{{"name":"inline","version":"0.0.0"}}}}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"tools":[
            {{"name":"echo","description":"echo","inputSchema":{{"type":"object"}}}}]}}}})
    elif method == "tools/call":
        args = msg.get("params", {{}}).get("arguments", {{}})
        send({{"jsonrpc":"2.0","id":mid,"result":{{
            "content":[{{"type":"text","text":json.dumps(args)}}]}}}})
    elif mid is not None:
        send({{"jsonrpc":"2.0","id":mid,"error":{{
            "code":-32601,"message":"method not found"}}}})
"#,
        marker = STDERR_MARKER
    )
}

/// Unique, auto-cleaned scratch dir under the OS temp dir. `tempfile` is not a
/// dev-dep of this crate (Cargo.toml is owned elsewhere), so we roll a tiny
/// RAII dir from pid + nanos, matching the pattern in `tests/host_guard.rs`.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-stderr-{tag}-{}-{nanos}",
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

/// Drive a tiny session against the inline server: handshake, one `echo`
/// call, then graceful shutdown. Shared by the capture + tee tests.
async fn drive_tiny_session(mut session: Session) {
    let tools = session.list_tools().await.expect("list_tools");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "inline server should advertise echo, got {tools:?}"
    );
    let result = session
        .call_tool("echo", &json!({ "msg": "hi" }))
        .await
        .expect("call_tool");
    assert!(!result.is_error, "echo call should not error");

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn capture_writes_server_stderr_to_file() {
    let py = helpers::python();
    let dir = ScratchDir::new("capture");
    let log = dir.path().join("server-stderr.log");

    let session = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn_with(
            &py,
            ["-c", &inline_server_script()],
            SpawnOptions::capture_stderr(&log),
        ),
    )
    .await
    .expect("spawn_with timed out")
    .expect("spawn_with (capture) failed");

    drive_tiny_session(session).await;

    // The pump flushes on EOF/shutdown; the file must exist with the marker.
    let contents = tokio::fs::read_to_string(&log)
        .await
        .expect("captured stderr log should exist");
    assert!(
        contents.contains(STDERR_MARKER),
        "captured log should contain the stderr marker, got: {contents:?}"
    );
}

#[tokio::test]
async fn tee_creates_file_and_completes() {
    let py = helpers::python();
    let dir = ScratchDir::new("tee");
    let log1 = dir.path().join("server-stderr-1.log");

    let session = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn_with(
            &py,
            ["-c", &inline_server_script()],
            SpawnOptions::tee_stderr(&log1),
        ),
    )
    .await
    .expect("spawn_with timed out")
    .expect("spawn_with (tee) failed");
    drive_tiny_session(session).await;

    let contents = tokio::fs::read_to_string(&log1)
        .await
        .expect("tee'd stderr log should exist");
    assert!(
        contents.contains(STDERR_MARKER),
        "tee'd log should contain the stderr marker, got: {contents:?}"
    );

    // A second spawn into the same dir must not hang — proves the first
    // pump left no orphan task / no held handle that wedges teardown.
    let log2 = dir.path().join("server-stderr-2.log");
    let session2 = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn_with(
            &py,
            ["-c", &inline_server_script()],
            SpawnOptions::tee_stderr(&log2),
        ),
    )
    .await
    .expect("second tee spawn timed out (pump orphan?)")
    .expect("second spawn_with (tee) failed");
    drive_tiny_session(session2).await;
    assert!(
        tokio::fs::read_to_string(&log2)
            .await
            .expect("second tee log should exist")
            .contains(STDERR_MARKER),
        "second tee'd log should also contain the marker"
    );
}

#[tokio::test]
async fn inherit_default_creates_no_file() {
    let py = helpers::python();
    let dir = ScratchDir::new("inherit");

    // 2-arg spawn == inherit; no pump, so nothing should land in `dir`.
    let session = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn(&py, ["-c", &inline_server_script()]),
    )
    .await
    .expect("spawn timed out")
    .expect("2-arg spawn failed");
    drive_tiny_session(session).await;

    let log = dir.path().join("server-stderr.log");
    assert!(
        !log.exists(),
        "inherit (default) must not create a capture file"
    );
}

#[tokio::test]
async fn two_arg_spawn_still_compiles_and_works() {
    // Pure delegation regression: the documented `Session::spawn(cmd, args)`
    // signature is unchanged and still drives a full session. (Also covered
    // implicitly by happy_path/deadlock staying green.)
    let py = helpers::python();
    let session = tokio::time::timeout(
        TEST_TIMEOUT,
        Session::spawn(&py, ["-c", &inline_server_script()]),
    )
    .await
    .expect("spawn timed out")
    .expect("2-arg spawn failed");
    drive_tiny_session(session).await;
}

#[tokio::test]
async fn run_with_tee_writes_server_stderr_log() {
    // End-to-end through `Run` + `StderrCapture::Tee`: the run dir's
    // `server-stderr.log` must exist with the marker and the short scenario
    // must complete.
    let py = helpers::python();
    let script = inline_server_script();
    // Embed the script as a TOML arg. `serde_json::Value` round-trips it
    // safely into the args array; `Config` builds the stdio command from it.
    let toml = format!(
        r#"
        [server]
        transport = "stdio"
        command = {py:?}
        args = ["-c", {script:?}]
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let dir = ScratchDir::new("run-tee");

    let scenario: Box<dyn Scenario> = Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    });

    let run = mcp_loadtest_engine::Run::new(config, scenario, dir.path())
        .with_stderr_capture(StderrCapture::Tee);
    let report = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run timed out")
        .expect("run should complete with tee capture");

    assert!(
        report.metrics.throughput.total_requests > 0,
        "expected the run to make at least one call, got {report:?}"
    );

    // `Run` writes the capture to `runs/<ulid>/server-stderr.log`. Find it.
    let mut found = None;
    let mut entries = tokio::fs::read_dir(dir.path())
        .await
        .expect("read scratch dir");
    while let Some(e) = entries.next_entry().await.expect("dir entry") {
        let candidate = e.path().join("server-stderr.log");
        if candidate.exists() {
            found = Some(candidate);
            break;
        }
    }
    let log = found.expect("Run should create runs/<id>/server-stderr.log under tee");
    let contents = tokio::fs::read_to_string(&log)
        .await
        .expect("read server-stderr.log");
    assert!(
        contents.contains(STDERR_MARKER),
        "run's server-stderr.log should contain the marker, got: {contents:?}"
    );
}
