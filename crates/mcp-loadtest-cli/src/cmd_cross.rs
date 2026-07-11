//! `cross` subcommand — run the same workload against N servers and print a
//! side-by-side comparison.
//!
//! Each `--server` arg gets its own [`Run`]; results are then formatted into
//! a Markdown table with rows for latency p50/p95/p99/max, requests/sec,
//! error rate, deadlock count, and the overall letter grade computed via
//! [`mcp_loadtest::analysis::grading::grade`].
//!
//! Designed to be invoked from `main.rs` and from integration tests:
//! `cmd_cross::run(args)` returns the rendered markdown so tests can assert
//! on it without spawning a subprocess.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use mcp_loadtest::config::{
    Config, OutputConfig, ScenarioConfig, ServerConfig, split_server_command,
};
use mcp_loadtest::report::Report;
use mcp_loadtest::run::Run;
use mcp_loadtest::scenario::Scenario;
use mcp_loadtest::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest::scenario::sustained::Sustained;
use serde_json::{Value, json};

mod render;
use render::render_markdown;

/// Cap on how many servers we spawn in parallel from `cross`. Each spawn
/// invokes `tokio::process::Command::spawn`; on Windows hitting the
/// JobObject limit when N is large causes spawns to fail. 8 chosen so a
/// 16-core box still has headroom and Windows JobObject limits aren't
/// hit; revisit if benchmarks justify higher.
const MAX_PARALLEL_SERVERS: usize = 8;

/// Which workload to drive against each server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossScenario {
    /// Sustained constant-load. See [`Sustained`].
    Sustained,
    /// Deadlock probe. See [`DeadlockProbe`].
    DeadlockProbe,
}

impl CrossScenario {
    /// Parse the `--scenario` flag. Accepts the canonical names plus a couple
    /// of common shorthands.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "sustained" => Ok(Self::Sustained),
            "deadlock_probe" | "deadlock-probe" | "deadlock" => Ok(Self::DeadlockProbe),
            other => Err(anyhow!(
                "unknown --scenario `{other}` (expected sustained|deadlock_probe)"
            )),
        }
    }
}

/// Parsed args for the subcommand. Mirrors the clap flags in `main.rs`.
#[derive(Debug, Clone)]
pub struct CrossArgs {
    /// List of `--server` strings (shell-style commands).
    pub servers: Vec<String>,
    /// Tool name to invoke on every iteration.
    pub tool: String,
    /// Tool args as a JSON string. Parsed once and reused per server.
    pub args: String,
    /// Total duration per server for `sustained`.
    pub duration: Duration,
    /// Which scenario shape to drive.
    pub scenario: CrossScenario,
    /// Where to put per-run dirs. Defaults to `./runs`.
    pub output_dir: PathBuf,
}

/// One row in the side-by-side comparison: the server command + its [`Report`].
#[derive(Debug)]
struct ServerRow {
    /// Original `--server` string (used as the column header).
    command: String,
    /// Either the finished report or a stringified error explaining the run failed.
    result: Result<Report>,
}

/// Run the `cross` subcommand. Spawns each server in turn, drives the same
/// workload, and returns a rendered Markdown comparison.
///
/// Errors are per-server: if one server fails to spawn or its scenario panics,
/// we record that failure and continue with the rest so the user still sees
/// the comparison for the servers that did work. Only an *empty* server list
/// (which clap should reject anyway) bubbles up as an outer error.
pub async fn run(args: CrossArgs) -> Result<String> {
    if args.servers.is_empty() {
        return Err(anyhow!("at least one --server must be provided to `cross`",));
    }

    let args_value: Value = serde_json::from_str(&args.args)
        .with_context(|| format!("parsing --args JSON: {}", args.args))?;

    // Drive servers in parallel — independent processes, no shared state, so
    // running them concurrently cuts wall-clock from `Σ duration` to roughly
    // `max(duration)`. We cap concurrency at `MAX_PARALLEL_SERVERS` so a
    // user passing dozens of `--server` flags doesn't trip Windows
    // JobObject spawn limits or thrash the host. `buffer_unordered` does
    // NOT preserve input order, so we tag each task with its input index
    // and sort the rows back at the end — report column labels and the
    // `Servers` list must match `args.servers` positions.
    let args_ref = &args;
    let args_value_ref = &args_value;
    let mut rows: Vec<(usize, ServerRow)> =
        futures::stream::iter(args.servers.iter().enumerate().map(|(idx, server)| {
            let server = server.clone();
            async move {
                let result = run_one(&server, args_ref, args_value_ref).await;
                (
                    idx,
                    ServerRow {
                        command: server,
                        result,
                    },
                )
            }
        }))
        .buffer_unordered(MAX_PARALLEL_SERVERS)
        .collect::<Vec<_>>()
        .await;
    rows.sort_by_key(|(idx, _)| *idx);
    let rows: Vec<ServerRow> = rows.into_iter().map(|(_, row)| row).collect();

    Ok(render_markdown(&rows, &args))
}

/// Drive a single server through one [`Run`]. Returns the `Report` on success
/// or a contextualized error.
async fn run_one(server: &str, args: &CrossArgs, args_value: &Value) -> Result<Report> {
    let (command, cmd_args) =
        split_server_command(server).with_context(|| format!("parsing --server `{server}`"))?;
    let server_cfg = ServerConfig::stdio(command, cmd_args);

    let scenario: Box<dyn Scenario> = match args.scenario {
        CrossScenario::Sustained => Box::new(Sustained {
            concurrent: 1,
            duration: args.duration,
            tool: args.tool.clone(),
            args: args_value.clone(),
        }),
        CrossScenario::DeadlockProbe => Box::new(DeadlockProbe {
            // Reasonable default that matches the CLI's `deadlock-probe`
            // command. Cross-server testing rarely needs more than a handful
            // of iterations to detect the bug class.
            concurrent: 5,
            hang_threshold: Duration::from_secs(2),
            grace_period: Duration::from_secs(5),
            tool: args.tool.clone(),
            args: args_value.clone(),
        }),
    };

    let scenario_kind = match args.scenario {
        CrossScenario::Sustained => "sustained",
        CrossScenario::DeadlockProbe => "deadlock_probe",
    };

    // Skip writing artifacts for cross-runs — the comparison itself is the
    // output. Callers wanting per-server reports run individual
    // `mcp-loadtest run` invocations.
    let config = Config::new(server_cfg, ScenarioConfig::new(scenario_kind, json!({})))
        .with_output(OutputConfig::new(args.output_dir.clone(), Vec::new()));

    let run = Run::new(config, scenario, args.output_dir.clone());
    let report = run
        .execute()
        .await
        .with_context(|| format!("running cross workload against `{server}`"))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_scenario_parses_sustained() {
        assert_eq!(
            CrossScenario::parse("sustained").unwrap(),
            CrossScenario::Sustained
        );
    }

    #[test]
    fn cross_scenario_parses_deadlock_aliases() {
        assert_eq!(
            CrossScenario::parse("deadlock_probe").unwrap(),
            CrossScenario::DeadlockProbe
        );
        assert_eq!(
            CrossScenario::parse("deadlock-probe").unwrap(),
            CrossScenario::DeadlockProbe
        );
        assert_eq!(
            CrossScenario::parse("deadlock").unwrap(),
            CrossScenario::DeadlockProbe
        );
    }

    #[test]
    fn cross_scenario_rejects_unknown() {
        assert!(CrossScenario::parse("burst").is_err());
    }

    #[test]
    fn split_server_command_basic() {
        let (cmd, args) = split_server_command("python -m foo").unwrap();
        assert_eq!(cmd, "python");
        assert_eq!(args, vec!["-m".to_string(), "foo".to_string()]);
    }

    #[test]
    fn split_server_command_empty_errors() {
        assert!(split_server_command("").is_err());
    }
}
