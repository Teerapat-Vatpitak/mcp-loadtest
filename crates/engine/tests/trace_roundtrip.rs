//! Record → replay round-trip integration tests for the trace module
//! (ADR 0021, plan task T3.3).
//!
//! Coverage:
//! - `roundtrip_against_mock_normal_matches` — a short sustained `Run` with
//!   `.with_trace` writes an `mcp-trace/1` file (>= 3 frames, including a
//!   `tools/call`); replaying it against a fresh `mock-normal.py` matches
//!   every scored request (the mock echoes deterministically).
//! - `replay_against_mock_error_diverges` — the same style of recording
//!   replayed against `mock-error.py` (every `tools/call` errors) reports
//!   divergence.
//! - `recording_redacts_sensitive_arguments` — secret-looking `tools/call`
//!   argument values never reach the trace's client→server frames.

mod helpers;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::config::{Config, ScenarioConfig, ServerConfig};
use mcp_loadtest_core::trace::format::{Direction, FORMAT_VERSION, parse_trace};
use mcp_loadtest_engine::Run;
use mcp_loadtest_engine::scenario::Scenario;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::trace::replay::replay_file;
use mcp_loadtest_protocol::Transport;
use mcp_loadtest_protocol::transport::stdio::StdioTransport;
use serde_json::{Value, json};

/// Outer guard so a wedged spawn/replay surfaces as a failure, not a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-request bound during replay (mock fixtures answer in ~1ms).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on post-replay transport shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Unique, auto-cleaned scratch dir under the OS temp dir. `tempfile` is not
/// a dev-dep of this crate, so we roll a tiny RAII dir from pid + nanos —
/// same stanza as `tests/stderr_capture.rs`.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-trace-{tag}-{}-{nanos}",
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

/// Drive a short sustained run against `mock-normal.py` with `.with_trace`,
/// returning the trace file path. `args` is the per-iteration `tools/call`
/// arguments object.
async fn record_trace(dir: &ScratchDir, tag: &str, args: Value) -> PathBuf {
    let py = helpers::python();
    let mock = helpers::fixture_path("mock-normal.py");
    let server = ServerConfig::stdio(py, vec![mock.to_string_lossy().into_owned()]);
    let config = Config::new(server, ScenarioConfig::new("sustained", json!({})));

    let scenario: Box<dyn Scenario> = Box::new(Sustained {
        concurrent: 1, // sequential — one deterministic frame interleaving
        duration: Duration::from_millis(700),
        tool: "echo".to_string(),
        args,
    });

    let trace_path = dir.path().join(format!("{tag}.jsonl"));
    let run = Run::new(config, scenario, dir.path()).with_trace(trace_path.clone());
    let report = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("recording run timed out")
        .expect("recording run failed");
    assert_eq!(
        report.trace_path.as_deref(),
        Some(trace_path.as_path()),
        "Report::trace_path must point at the recording"
    );
    trace_path
}

/// Spawn a fixture over a bare stdio transport (no Session handshake — the
/// trace carries the recorded handshake frames). The transport is
/// `kill_on_drop`, so a panic still requests child termination; normal tests
/// explicitly await bounded shutdown/reap.
async fn spawn_replay_target(fixture: &str) -> Box<dyn Transport> {
    let py = helpers::python();
    let mock = helpers::fixture_path(fixture);
    let t = tokio::time::timeout(TEST_TIMEOUT, StdioTransport::spawn(&py, [mock.as_os_str()]))
        .await
        .expect("spawn timed out")
        .expect("spawn failed");
    Box::new(t)
}

#[tokio::test]
async fn roundtrip_against_mock_normal_matches() {
    let dir = ScratchDir::new("roundtrip");
    let trace_path = record_trace(&dir, "trace", json!({ "msg": "roundtrip" })).await;

    // The recording itself: header + at least handshake, tools/list, and one
    // tools/call, with a response for every request.
    let text = tokio::fs::read_to_string(&trace_path)
        .await
        .expect("trace file must exist");
    let (header, frames) = parse_trace(&text).expect("trace must parse");
    assert_eq!(header.format, FORMAT_VERSION);
    assert!(!header.run_id.is_empty());
    assert!(header.server.contains("mock-normal.py"));
    assert!(
        frames.len() >= 3,
        "expected >= 3 frames, got {}",
        frames.len()
    );
    assert!(
        frames.iter().any(
            |f| f.dir == Direction::ClientToServer && f.method.as_deref() == Some("tools/call")
        ),
        "trace must contain a recorded tools/call request"
    );

    // Replay against a fresh mock-normal: deterministic echo → full match.
    let mut transport = spawn_replay_target("mock-normal.py").await;
    let report = tokio::time::timeout(
        TEST_TIMEOUT,
        replay_file(&trace_path, transport.as_mut(), REQUEST_TIMEOUT),
    )
    .await
    .expect("replay timed out")
    .expect("replay failed");
    tokio::time::timeout(SHUTDOWN_TIMEOUT, transport.shutdown())
        .await
        .expect("replay target shutdown timed out")
        .expect("replay target shutdown failed");

    assert!(report.total >= 3, "initialize + tools/list + tools/call");
    assert_eq!(
        report.matched, report.total,
        "mock-normal echoes deterministically; diverged: {:?}",
        report.diverged
    );
    assert!(report.diverged.is_empty());
}

#[tokio::test]
async fn replay_against_mock_error_diverges() {
    let dir = ScratchDir::new("diverge");
    let trace_path = record_trace(&dir, "trace", json!({ "msg": "diverge" })).await;

    // mock-error answers every tools/call with a JSON-RPC error (and a
    // different tools/list description), so the replay must diverge.
    let mut transport = spawn_replay_target("mock-error.py").await;
    let report = tokio::time::timeout(
        TEST_TIMEOUT,
        replay_file(&trace_path, transport.as_mut(), REQUEST_TIMEOUT),
    )
    .await
    .expect("replay timed out")
    .expect("replay failed");
    tokio::time::timeout(SHUTDOWN_TIMEOUT, transport.shutdown())
        .await
        .expect("replay target shutdown timed out")
        .expect("replay target shutdown failed");

    assert!(
        !report.diverged.is_empty(),
        "replaying a mock-normal trace against mock-error must diverge"
    );
    assert!(report.matched < report.total);
    assert_eq!(report.matched + report.diverged.len(), report.total);
    assert!(
        report
            .diverged
            .iter()
            .any(|d| d.method.as_deref() == Some("tools/call")),
        "tools/call responses must be among the divergences: {:?}",
        report.diverged
    );
}

#[tokio::test]
async fn recording_redacts_sensitive_arguments() {
    let dir = ScratchDir::new("redact");
    let trace_path = record_trace(
        &dir,
        "trace",
        json!({ "msg": "hello", "api_key": "sekrit-value" }),
    )
    .await;

    let text = tokio::fs::read_to_string(&trace_path)
        .await
        .expect("trace file must exist");
    let (_, frames) = parse_trace(&text).expect("trace must parse");

    let c2s_with_secret = frames
        .iter()
        .filter(|f| f.dir == Direction::ClientToServer)
        .any(|f| f.body.contains("sekrit-value"));
    assert!(
        !c2s_with_secret,
        "client→server frames must have `api_key` redacted (ADR 0021)"
    );
    assert!(
        frames
            .iter()
            .filter(|f| f.dir == Direction::ClientToServer)
            .any(|f| f.body.contains("[REDACTED]")),
        "the redaction placeholder must appear in a recorded tools/call"
    );
}
