//! `mcp-loadtest replay <trace-file>` — re-send a recorded `mcp-trace/1`
//! JSONL trace (written by `run --trace`) against a fresh server and diff
//! every response against the recording (ADR 0021).
//!
//! The replay is Session-less: recorded client frames go straight through a
//! bare [`Transport`] (fresh sequential JSON-RPC ids, canonical-JSON diff
//! with ids ignored — see `mcp_loadtest::trace::replay`). The caller
//! (`main.rs`) prints the rendered summary and exits non-zero when any frame
//! diverged.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use mcp_loadtest::Transport;
use mcp_loadtest::config::{ServerConfig, split_server_command};
use mcp_loadtest::protocol::transport::HostGuard;
use mcp_loadtest::protocol::transport::http::HttpTransport;
use mcp_loadtest::protocol::transport::sse::SseTransport;
use mcp_loadtest::protocol::transport::stdio::StdioTransport;
use mcp_loadtest::protocol::transport::ws::WsTransport;
use mcp_loadtest::trace::ReplayReport;
use mcp_loadtest::trace::replay::replay_file;

/// Per-request stall bound: a hung server surfaces as a divergence on that
/// frame instead of hanging the whole replay.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort bound on the post-replay transport shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Parsed CLI args for the subcommand.
#[derive(Debug)]
pub(crate) struct ReplayArgs {
    /// Path to the `mcp-trace/1` JSONL file (recorded via `run --trace`).
    pub trace_file: PathBuf,
    /// Shell-style server command for the stdio transport.
    pub server: Option<String>,
    /// Endpoint URL for the http / sse / ws transports.
    pub url: Option<String>,
    /// Transport name: `stdio` (default) | `http` | `sse` | `ws`.
    pub transport: String,
    /// SSRF-guard allowlist entries for URL transports (ADR 0012).
    pub allow_hosts: Vec<String>,
}

/// What `main.rs` needs to print and exit on.
#[derive(Debug)]
pub(crate) struct ReplaySummary {
    /// Human-readable summary (one line + one line per divergence).
    pub rendered: String,
    /// Request frames scored.
    pub total: usize,
    /// Frames that diverged — non-zero means a non-zero exit.
    pub diverged: usize,
}

/// Run the `replay` subcommand: connect the target transport, replay the
/// trace through it, shut the transport down (best-effort, bounded), and
/// return the summary for `main.rs` to print / gate on.
pub(crate) async fn run(args: ReplayArgs) -> Result<ReplaySummary> {
    let mut transport = build_transport(&args).await?;
    let report = replay_file(&args.trace_file, transport.as_mut(), REQUEST_TIMEOUT)
        .await
        .with_context(|| format!("replaying {}", args.trace_file.display()))?;
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, transport.shutdown()).await;

    Ok(ReplaySummary {
        rendered: render(&report),
        total: report.total,
        diverged: report.diverged.len(),
    })
}

/// Connect the transport named by `--transport`, without any `Session`
/// handshake (the trace carries the recorded handshake frames itself).
async fn build_transport(args: &ReplayArgs) -> Result<Box<dyn Transport>> {
    match args.transport.as_str() {
        "stdio" => {
            let server = args
                .server
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--server is required for the stdio transport"))?;
            let (command, cmd_args) = split_server_command(server)
                .with_context(|| format!("parsing --server `{server}`"))?;
            let t = StdioTransport::spawn(&command, cmd_args)
                .await
                .with_context(|| format!("spawning `{server}`"))?;
            Ok(Box::new(t))
        }
        kind @ ("http" | "sse" | "ws") => {
            let url = args
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--url is required for the {kind} transport"))?;
            let guard = host_guard(&args.allow_hosts);
            let t: Box<dyn Transport> = match kind {
                "http" => Box::new(HttpTransport::connect(url, &guard).await?),
                "sse" => Box::new(SseTransport::connect(url, &guard).await?),
                _ => Box::new(WsTransport::connect(url, &guard).await?),
            };
            Ok(t)
        }
        other => anyhow::bail!("unknown --transport `{other}` (expected stdio|http|sse|ws)"),
    }
}

/// Build the SSRF [`HostGuard`] (ADR 0012) from `--allow-host` entries —
/// same semantics as `[server].allowed_hosts` in a run config (e.g.
/// `--allow-host 127.0.0.1` for local replay).
fn host_guard(allow_hosts: &[String]) -> HostGuard {
    let mut cfg = ServerConfig::stdio(String::new(), Vec::new());
    cfg.allowed_hosts = allow_hosts.to_vec();
    HostGuard::from_config(&cfg)
}

/// Render the replay outcome: one summary line, then one line per
/// divergence (`#<index> <method>: <note>`).
fn render(report: &ReplayReport) -> String {
    let mut out = format!(
        "replay: {} request frame(s) — {} matched, {} diverged\n",
        report.total,
        report.matched,
        report.diverged.len()
    );
    for d in &report.diverged {
        let method = d.method.as_deref().unwrap_or("?");
        out.push_str(&format!("  #{} {method}: {}\n", d.index, d.note));
    }
    out
}

#[cfg(test)]
mod tests {
    use mcp_loadtest::trace::Divergence;

    use super::*;

    #[test]
    fn render_clean_report_is_one_line() {
        let r = ReplayReport {
            total: 5,
            matched: 5,
            diverged: Vec::new(),
        };
        let s = render(&r);
        assert_eq!(s, "replay: 5 request frame(s) — 5 matched, 0 diverged\n");
    }

    #[test]
    fn render_lists_each_divergence() {
        let r = ReplayReport {
            total: 2,
            matched: 1,
            diverged: vec![Divergence {
                index: 1,
                method: Some("tools/call".into()),
                note: "response differs from recording (ids ignored)".into(),
            }],
        };
        let s = render(&r);
        assert!(s.contains("1 diverged"));
        assert!(s.contains("#1 tools/call: response differs"));
    }

    /// `Box<dyn Transport>` has no `Debug`, so `expect_err` can't be used.
    async fn build_err(args: &ReplayArgs) -> anyhow::Error {
        match build_transport(args).await {
            Err(err) => err,
            Ok(_) => panic!("build_transport must fail for {args:?}"),
        }
    }

    #[tokio::test]
    async fn stdio_requires_server_flag() {
        let args = ReplayArgs {
            trace_file: PathBuf::from("t.jsonl"),
            server: None,
            url: None,
            transport: "stdio".into(),
            allow_hosts: Vec::new(),
        };
        let err = build_err(&args).await;
        assert!(err.to_string().contains("--server"));
    }

    #[tokio::test]
    async fn url_transports_require_url_flag() {
        for kind in ["http", "sse", "ws"] {
            let args = ReplayArgs {
                trace_file: PathBuf::from("t.jsonl"),
                server: None,
                url: None,
                transport: kind.into(),
                allow_hosts: Vec::new(),
            };
            let err = build_err(&args).await;
            assert!(err.to_string().contains("--url"), "{kind}: {err}");
        }
    }

    #[tokio::test]
    async fn unknown_transport_is_rejected() {
        let args = ReplayArgs {
            trace_file: PathBuf::from("t.jsonl"),
            server: None,
            url: None,
            transport: "carrier-pigeon".into(),
            allow_hosts: Vec::new(),
        };
        let err = build_err(&args).await;
        assert!(err.to_string().contains("carrier-pigeon"));
    }
}
