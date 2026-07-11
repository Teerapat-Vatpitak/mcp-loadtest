//! Integration tests for the SSRF host-allowlist guard (Feature 1, ADR 0012)
//! and the DNS-rebinding resolver-pinning layer on top of it (ADR 0016).
//!
//! These exercise the guard through the *public* `Config` + `Run` surface so
//! we cover the real wiring (`build_session` → `HostGuard::from_config` →
//! `*Transport::connect`), not just the unit-tested `guard` module.
//!
//! The reject path needs **no network**: the guard fails on the parsed URL
//! before any socket is opened, so a `169.254.169.254` config errors out
//! deterministically. The allow path spawns the stdlib `mock-http-server.py`
//! fixture on `127.0.0.1` and proves the operator escape hatch lets a run
//! complete.

mod helpers;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::config::{Config, ConfigError};
use mcp_loadtest_engine::Run;
use mcp_loadtest_engine::scenario::Scenario;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Outer guard so a wedged run surfaces as a failure, not a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the `Sustained` scenario the TOML below describes (kept in lockstep
/// with the `[scenario]` block so `Run` drives a real, short workload).
fn sustained_scenario() -> Box<dyn Scenario> {
    Box::new(Sustained {
        concurrent: 1,
        duration: Duration::from_secs(1),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    })
}

/// A unique, auto-cleaned scratch dir under the OS temp dir for `Run`'s
/// `runs/<ulid>/` output. `tempfile` is not a dev-dep of this crate, so we
/// roll a tiny RAII dir from pid + a monotonic-ish nanos suffix. `Drop`
/// best-effort removes it so repeated test runs don't accumulate dirs.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "mcp-loadtest-host-guard-{tag}-{}-{nanos}",
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

/// Render the whole error chain (`Display` of the error + every `source()`)
/// into one string so substring assertions catch the message regardless of
/// which wrapper layer it surfaced through.
fn chain_string(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut src = err.source();
    while let Some(e) = src {
        out.push_str(" | ");
        out.push_str(&e.to_string());
        src = e.source();
    }
    out
}

#[tokio::test]
async fn run_rejects_link_local_http_url() {
    // The classic cloud-metadata SSRF target. No `allowed_hosts`, so the
    // always-on IP-literal block must reject it before any connect.
    let toml = r#"
        [server]
        transport = "http"
        url = "http://169.254.169.254/latest/meta-data/"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
    "#;
    let config = Config::from_toml_str(toml).expect("config must parse");
    let tmp = ScratchDir::new("reject");

    let run = Run::new(config, sustained_scenario(), tmp.path());
    let result = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run should fail fast (guard rejects before any network I/O)");

    let err = result.expect_err("link-local IP URL must be rejected by the SSRF guard");
    let chain = chain_string(&err);
    assert!(
        chain.contains("blocked host"),
        "error chain should carry the stable `blocked host` marker, got: {chain}"
    );
    assert!(
        chain.contains("169.254.169.254"),
        "error chain should name the offending host, got: {chain}"
    );
}

#[tokio::test]
async fn run_rejects_hostname_resolving_to_loopback() {
    // ADR 0016 resolver pinning, wired end-to-end: `localhost` resolves to
    // loopback via the hosts file / OS stack (no external DNS involved), and
    // with no `allowed_hosts` the resolver layer must reject it before any
    // connect. This closes the DNS-rebinding gap ADR 0012 accepted
    // (ADR 0012 → ADR 0016).
    let toml = r#"
        [server]
        transport = "http"
        url = "http://localhost:1/"
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
    "#;
    let config = Config::from_toml_str(toml).expect("config must parse");
    let tmp = ScratchDir::new("reject-dns");

    let run = Run::new(config, sustained_scenario(), tmp.path());
    let result = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run should fail fast (resolver guard rejects before any connect)");

    let err = result.expect_err("loopback-resolving hostname must be rejected (ADR 0016)");
    let chain = chain_string(&err);
    assert!(
        chain.contains("blocked host"),
        "error chain should carry the stable `blocked host` marker, got: {chain}"
    );
    assert!(
        chain.contains("ADR 0016"),
        "error chain should cite the resolver-pinning ADR, got: {chain}"
    );
    assert!(
        chain.contains("localhost"),
        "error chain should name the offending host, got: {chain}"
    );
}

#[tokio::test]
async fn run_allows_loopback_when_escape_hatch_set() {
    let py = helpers::python();
    let script = helpers::fixture_path("mock-http-server.py");

    // Spawn the stdlib HTTP mock; it prints `LISTENING: 127.0.0.1:<port>` on
    // stdout once bound (OS-assigned port via --port 0).
    let mut child = Command::new(&py)
        .arg(&script)
        .arg("--port")
        .arg("0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mock-http-server.py");

    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = BufReader::new(stdout).lines();
    // 30s, not 10s: a cold Python + stdlib http.server bind on a loaded
    // macOS GitHub runner can edge past 10s (observed on CI); locally this
    // resolves in ~2s. Generous bound trades nothing for CI determinism.
    let listening = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("timed out waiting for LISTENING line")
        .expect("read stdout")
        .expect("server closed stdout before announcing");
    let addr = listening
        .strip_prefix("LISTENING: ")
        .unwrap_or_else(|| panic!("unexpected first line: {listening}"))
        .trim()
        .to_string();

    // `allowed_hosts = ["127.0.0.1"]` is the operator escape hatch: it both
    // satisfies the allowlist AND overrides the loopback IP-literal block.
    let toml = format!(
        r#"
        [server]
        transport = "http"
        url = "http://{addr}/"
        allowed_hosts = ["127.0.0.1"]
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let tmp = ScratchDir::new("allow");

    let run = Run::new(config, sustained_scenario(), tmp.path());
    let report = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run timed out")
        .expect("run should complete: escape hatch allows the loopback literal");

    // The guard must not have interfered — assert traffic actually flowed.
    assert!(
        report.metrics.throughput.total_requests > 0,
        "expected the run to make at least one call against the mock, got {report:?}"
    );

    // Best-effort: stop the child (kill_on_drop is the backstop).
    let _ = child.start_kill();
}

#[tokio::test]
async fn run_allows_hostname_escape_hatch_with_pinning() {
    // The hostname twin of the loopback escape hatch: `allowed_hosts =
    // ["localhost"]` lets a loopback-*resolving* hostname through the ADR
    // 0016 resolver layer, and the vetted addresses are pinned into the
    // client. `localhost` resolves via the hosts file / OS stack — no
    // external DNS. The mock listens on 127.0.0.1 only, so a resolver that
    // yields `::1` first also exercises pinned-address fallback.
    let py = helpers::python();
    let script = helpers::fixture_path("mock-http-server.py");

    let mut child = Command::new(&py)
        .arg(&script)
        .arg("--port")
        .arg("0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mock-http-server.py");

    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = BufReader::new(stdout).lines();
    let listening = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("timed out waiting for LISTENING line")
        .expect("read stdout")
        .expect("server closed stdout before announcing");
    let addr = listening
        .strip_prefix("LISTENING: ")
        .unwrap_or_else(|| panic!("unexpected first line: {listening}"))
        .trim()
        .to_string();
    let port = addr
        .rsplit(':')
        .next()
        .unwrap_or_else(|| panic!("no port in addr: {addr}"));

    let toml = format!(
        r#"
        [server]
        transport = "http"
        url = "http://localhost:{port}/"
        allowed_hosts = ["localhost"]
        [scenario]
        type = "sustained"
        duration = "1s"
        concurrent = 1
        tool = "echo"
        "#
    );
    let config = Config::from_toml_str(&toml).expect("config must parse");
    let tmp = ScratchDir::new("allow-dns");

    let run = Run::new(config, sustained_scenario(), tmp.path());
    let report = tokio::time::timeout(TEST_TIMEOUT, run.execute())
        .await
        .expect("run timed out")
        .expect("run should complete: allowlisted hostname resolves, pins, and connects");

    assert!(
        report.metrics.throughput.total_requests > 0,
        "expected the run to make at least one call against the mock, got {report:?}"
    );

    let _ = child.start_kill();
}

#[test]
fn config_rejects_malformed_allowed_hosts_entry() {
    // A full URL is the most common operator mistake — it can never match
    // `Url::host_str()`, so validation rejects it up front.
    let toml = r#"
        [server]
        command = "python"
        allowed_hosts = ["http://x"]
        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml).expect_err("malformed allowed_hosts must be rejected");
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("allowed_hosts")),
        "expected ConfigError::Invalid mentioning allowed_hosts, got: {err:?}"
    );
}

#[test]
fn config_rejects_allowed_hosts_entry_with_port() {
    let toml = r#"
        [server]
        command = "python"
        allowed_hosts = ["api.example.com:8080"]
        [scenario]
        type = "sustained"
    "#;
    let err = Config::from_toml_str(toml).expect_err("host:port entry must be rejected");
    assert!(matches!(err, ConfigError::Invalid(ref m) if m.contains("allowed_hosts")));
}

#[test]
fn config_accepts_bare_hostname_allowed_hosts() {
    let toml = r#"
        [server]
        command = "python"
        allowed_hosts = ["api.example.com", "127.0.0.1"]
        [scenario]
        type = "sustained"
    "#;
    let cfg = Config::from_toml_str(toml).expect("bare hostnames must parse");
    assert_eq!(
        cfg.server.allowed_hosts,
        vec!["api.example.com".to_string(), "127.0.0.1".to_string()]
    );
}
