//! Subcommand dispatch: routes each parsed [`Cmd`] to its handler and funnels
//! errors through hints.

use anyhow::{Context, Result};

use mcp_loadtest_cli::{cmd_compare, cmd_cross, cmd_deadlock, cmd_doctor, cmd_run};

use crate::Cmd;
use crate::cmd_replay;

/// Stream a rendered report to stdout (pipe-friendly), ensuring a trailing newline.
fn print_stdout(rendered: &str) {
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
}

/// Dispatch a parsed subcommand. Split from `main` so the error→`Hint:`
/// rendering is one funnel for every subcommand.
pub(crate) async fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::ExampleConfig => {
            print!("{}", mcp_loadtest::config::example_config());
            Ok(())
        }
        Cmd::ConfigSchema => {
            print_stdout(&mcp_loadtest::config::config_schema_pretty());
            Ok(())
        }
        Cmd::ListScenarios => {
            println!(
                "sustained        — steady workload; also accepts weighted tool_call/pattern configs"
            );
            println!("deadlock_probe   — Vibe-Trading-bug-class detector");
            println!(
                "cold_start       — respawns a fresh server per iteration; measures spawn→initialize handshake + first call"
            );
            println!("spike            — baseline → burst → cooldown workload");
            println!("ramp             — stepped concurrency + optional breaking-point detection");
            println!("soak             — long steady run with periodic samples and drift notes");
            println!("race_check       — identical calls + response-divergence detection");
            println!("fuzzer           — malformed payload probe");
            println!("pattern          — weighted multi-step tool-call sequences");
            println!(
                "version_matrix   — drives the same server once per MCP protocol revision and diffs the outcomes"
            );
            Ok(())
        }
        Cmd::Run {
            config,
            capture_stderr,
            tee_stderr,
            trace,
            action_output_dir,
            action_redact_server_identity,
        } => {
            cmd_run::run_from_config_with_output(
                &config,
                capture_stderr,
                tee_stderr,
                trace,
                action_output_dir,
                action_redact_server_identity,
            )
            .await
        }
        Cmd::Replay {
            trace_file,
            server,
            url,
            transport,
            allow_hosts,
        } => {
            let summary = cmd_replay::run(cmd_replay::ReplayArgs {
                trace_file,
                server,
                url,
                transport,
                allow_hosts,
            })
            .await?;
            print_stdout(&summary.rendered);
            if summary.diverged > 0 {
                anyhow::bail!(
                    "replay diverged — {} of {} request frame(s) differ from the recording",
                    summary.diverged,
                    summary.total
                );
            }
            Ok(())
        }
        Cmd::Doctor {
            server,
            runs_dir,
            action_redact_server_identity,
        } => {
            cmd_doctor::run_doctor_with_redaction(server, runs_dir, action_redact_server_identity)
                .await
        }
        Cmd::Compare {
            baseline,
            current,
            format,
            max_p99_regression_pct,
            max_error_rate_regression_pp,
            allow_deadlock_increase,
        } => {
            let fmt = cmd_compare::CompareFormat::parse(&format)?;
            let thresholds = cmd_compare::RegressionThresholds {
                p99_pct: max_p99_regression_pct,
                error_rate_pp: max_error_rate_regression_pp,
                deadlock_zero_tolerance: !allow_deadlock_increase,
            };
            let outcome = cmd_compare::run(&baseline, &current, fmt, &thresholds)?;
            print_stdout(&outcome.rendered);
            cmd_compare::gate(&outcome.report)
        }
        Cmd::DeadlockProbe {
            server,
            tool,
            concurrent,
            hang_threshold,
            grace_period,
            args,
            output_dir,
            action_redact_server_identity,
        } => {
            cmd_deadlock::run_deadlock_probe_with_redaction(
                &server,
                &tool,
                concurrent,
                cmd_run::parse_dur_str(&hang_threshold)?,
                cmd_run::parse_dur_str(&grace_period)?,
                &args,
                &output_dir,
                action_redact_server_identity,
            )
            .await
        }
        Cmd::Cross {
            servers,
            tool,
            args,
            duration,
            scenario,
            output_dir,
            action_redact_server_identity,
        } => {
            let cross_args = cmd_cross::CrossArgs {
                servers,
                tool,
                args,
                duration: cmd_run::parse_dur_str(&duration)?,
                scenario: cmd_cross::CrossScenario::parse(&scenario)?,
                output_dir,
                redact_server_identity: action_redact_server_identity,
            };
            let outcome = cmd_cross::run(cross_args).await?;
            print_stdout(&outcome.rendered);
            outcome.gate()
        }
        Cmd::Serve { mcp } => {
            if !mcp {
                anyhow::bail!("only --mcp (stdio) is supported; HTTP/SSE serve modes deferred",);
            }
            mcp_loadtest::serve::McpServer::new()
                .run_stdio()
                .await
                .context("running mcp-loadtest serve --mcp")?;
            Ok(())
        }
        Cmd::DistributedAgent { stdio } => {
            if !stdio {
                anyhow::bail!("distributed agents currently require --stdio");
            }
            mcp_loadtest_cli::distributed::run_stdio_agent().await
        }
    }
}
