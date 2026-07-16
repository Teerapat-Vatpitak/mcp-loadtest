//! `mcp-loadtest` CLI — entry point. Subcommands are the `Cmd` variants
//! below (their `///` docs are the `--help` text): `example-config`, `run`,
//! `deadlock-probe`, `compare`, `replay`, `cross`, `list-scenarios`,
//! `doctor`, `serve`.
//!
//! Global `--explain` prints a static per-subcommand description (DESIGN
//! §21.4), serviced pre-clap so it works for subcommands with required args.
//! Errors print with an actionable `Hint:` where one applies (DESIGN §21.3).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use mcp_loadtest_cli::{cmd_compare, explain, hints};

// Binary-local modules (not part of the `mcp_loadtest_cli` lib surface):
// `cmd_replay` has no pure logic other crates need to reuse yet; `dispatch`
// routes each parsed subcommand to its handler.
mod cmd_replay;
mod dispatch;

/// clap value parser: accept only a finite, strictly-positive f64. A
/// non-positive regression threshold would invert the regression direction,
/// so reject it at the CLI boundary with a clear message instead of
/// silently mis-gating.
fn positive_f64(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err("must be a finite number greater than 0".to_string())
    }
}

#[derive(Parser)]
#[command(name = "mcp-loadtest", version, about = "Load tester for MCP servers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Print a static description of the chosen subcommand and exit (DESIGN
    /// §21.4). Serviced by a pre-clap scan (`explain::maybe_handle_explain`)
    /// so it works for subcommands with required args; registered here only
    /// so `--help` advertises it and the normal path tolerates a stray
    /// `--explain`.
    #[arg(long, global = true)]
    explain: bool,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Print a sample TOML config to stdout.
    ExampleConfig,

    /// List built-in scenario kinds and a one-line description of each.
    ListScenarios,

    /// Run a workload from a TOML config.
    Run {
        /// Path to the TOML config file.
        #[arg(short, long)]
        config: PathBuf,
        /// Capture the server's stderr to `runs/<id>/server-stderr.log`
        /// instead of inheriting it. No-op for http/sse/ws. See ADR 0013.
        #[arg(long)]
        capture_stderr: bool,
        /// Like `--capture-stderr`, but also mirror the stderr live to this
        /// process's stderr. Wins over `--capture-stderr` if both are set.
        #[arg(long)]
        tee_stderr: bool,
        /// Record every JSON-RPC frame of the run to this file as
        /// `mcp-trace/1` JSONL, replayable via the `replay` subcommand.
        /// Secret-looking tool arguments are redacted (ADR 0021).
        #[arg(long)]
        trace: Option<PathBuf>,
    },

    /// Diagnose common setup problems; print a ✅/❌ checklist with a
    /// one-line fix per ❌. Exits non-zero on any ❌ (DESIGN §21.6).
    Doctor {
        /// Optional shell-style server command (`"python -m my_mcp"`) to run
        /// an `initialize` smoke against; on failure the captured stderr is
        /// reported. Omit to skip the server check.
        #[arg(long)]
        server: Option<String>,
        /// Directory scanned for stale run accumulation.
        #[arg(long, default_value = "./runs")]
        runs_dir: PathBuf,
    },

    /// Compare two `metrics.json` reports and emit a regression diff.
    Compare {
        /// Path to the baseline `metrics.json`.
        baseline: PathBuf,
        /// Path to the current `metrics.json`.
        current: PathBuf,
        /// Output format: `markdown` (default, human-readable) or `json` (CI-friendly).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// p99 latency growth (percent) that flags a regression. Must be > 0.
        #[arg(long, default_value_t = cmd_compare::P99_REGRESSION_PCT, value_parser = positive_f64)]
        max_p99_regression_pct: f64,
        /// Error-rate growth (percentage points) that flags a regression. Must be > 0.
        #[arg(long, default_value_t = cmd_compare::ERROR_RATE_REGRESSION_PP, value_parser = positive_f64)]
        max_error_rate_regression_pp: f64,
        /// Don't flag a regression when the deadlock count increases.
        #[arg(long, default_value_t = false)]
        allow_deadlock_increase: bool,
    },

    /// Replay a recorded trace (`run --trace`) against a server and diff
    /// every response against the recording. Exits non-zero on divergence.
    Replay {
        /// Path to the `mcp-trace/1` JSONL file recorded via `run --trace`.
        trace_file: PathBuf,
        /// Server command for the stdio transport, parsed shell-style:
        /// `"python -m my_mcp"`.
        #[arg(long)]
        server: Option<String>,
        /// Endpoint URL for the http / sse / ws transports.
        #[arg(long)]
        url: Option<String>,
        /// Transport to replay over: `stdio` (default) | `http` | `sse` | `ws`.
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// SSRF-guard allowlist entry for URL transports (repeatable), e.g.
        /// `--allow-host 127.0.0.1` for local replay. See ADR 0012.
        #[arg(long = "allow-host", action = clap::ArgAction::Append)]
        allow_hosts: Vec<String>,
    },

    /// Quick deadlock probe — convenience wrapper around `DeadlockProbe`.
    DeadlockProbe {
        /// Server command, parsed shell-style: `"python -m my_mcp"` or `"node dist/server.js"`.
        #[arg(short, long)]
        server: String,
        /// Tool name to call.
        #[arg(long, default_value = "echo")]
        tool: String,
        /// Number of sequential `tools/call` probes (quick default; the
        /// `run` config form of this scenario defaults to 20).
        #[arg(long, default_value_t = 5)]
        concurrent: u32,
        /// Per-call hang threshold (humantime: `2s`, `500ms`, ...).
        #[arg(long, default_value = "2s")]
        hang_threshold: String,
        /// Grace period after threshold before classifying as deadlock.
        #[arg(long, default_value = "5s")]
        grace_period: String,
        /// Tool arguments as a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
        /// Where to write the per-run dir.
        #[arg(long, default_value = "./runs")]
        output_dir: PathBuf,
    },

    /// Run the same workload against N servers and emit a side-by-side comparison.
    Cross {
        /// MCP server commands to compare. Repeat the flag once per server,
        /// e.g. `--server "python -m a" --server "python -m b"`.
        #[arg(long = "server", num_args = 1.., action = clap::ArgAction::Append, required = true)]
        servers: Vec<String>,
        /// Tool name to call on every iteration.
        #[arg(long, default_value = "echo")]
        tool: String,
        /// Tool arguments as a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
        /// Per-server run duration (humantime: `30s`, `1m`, ...).
        #[arg(long, default_value = "10s")]
        duration: String,
        /// Which scenario to drive: `sustained` (default) or `deadlock_probe`.
        #[arg(long, default_value = "sustained")]
        scenario: String,
        /// Where to write per-run dirs (one per server). Comparison itself
        /// is printed to stdout.
        #[arg(long, default_value = "./runs")]
        output_dir: PathBuf,
    },

    /// Expose mcp-loadtest itself AS an MCP server over stdio.
    ///
    /// Lets AI agents (Claude Code, Cursor, ...) drive load tests by calling
    /// `deadlock_probe` / `sustained_load` / `compare_runs` tools. See
    /// DESIGN §21.2.
    Serve {
        /// Speak the MCP protocol over stdio. Default and currently the only
        /// supported variant — HTTP/SSE serve modes are deferred.
        #[arg(long, default_value_t = true)]
        mcp: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Serviced before clap parses (see `explain` module doc / ADR 0014):
    // required-arg subcommands would otherwise reject before any handler ran.
    if explain::maybe_handle_explain() {
        return Ok(());
    }

    // Lightweight default tracing — overridable via RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    // Funnel every error through `print_with_hint` (source chain + at most
    // one actionable `Hint:`, DESIGN §21.3) and exit explicitly non-zero, so
    // the hint reliably reaches stderr instead of the default `Debug` print.
    if let Err(e) = dispatch::dispatch(cli.cmd).await {
        hints::print_with_hint(&e);
        std::process::exit(1);
    }
    Ok(())
}
