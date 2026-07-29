//! Hidden stdio worker runtime used by ephemeral SSH agents.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use mcp_loadtest::config::Config;
use mcp_loadtest::scenario::{TrafficReadiness, TrafficStartError, TrafficStartGate};
use mcp_loadtest::{Recorder, Run};
use mcp_loadtest_distributed::{
    AgentChannel, AgentDirective, AgentEvidence, AgentPolicy, AgentShard, AgentStateMachine,
    HelloFrame, HistogramEvidence, MetricsEvidence, NdjsonChannel, ReadyFrame, RemoteTarget,
    SupportedScenario,
};
use tokio::io::{stdin, stdout};
use tokio::sync::{mpsc, watch};

use crate::cmd_run::build_scenario;

const CONTROL_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
struct GateTiming {
    readiness: TrafficReadiness,
    traffic_start: Instant,
    start_delay_ms: u64,
}

struct WorkerGate {
    readiness_tx: mpsc::UnboundedSender<TrafficReadiness>,
    start_rx: watch::Receiver<Option<Duration>>,
    timing: Mutex<Option<GateTiming>>,
}

#[async_trait]
impl TrafficStartGate for WorkerGate {
    async fn ready_and_start_at(
        &self,
        readiness: TrafficReadiness,
    ) -> Result<Instant, TrafficStartError> {
        self.readiness_tx
            .send(readiness.clone())
            .map_err(|_| TrafficStartError::Coordinator("control loop stopped".into()))?;
        let mut start_rx = self.start_rx.clone();
        while start_rx.borrow().is_none() {
            start_rx
                .changed()
                .await
                .map_err(|_| TrafficStartError::Cancelled)?;
        }
        let delay = (*start_rx.borrow()).ok_or(TrafficStartError::Cancelled)?;
        let scheduled = Instant::now() + delay;
        tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled)).await;
        let traffic_start = Instant::now();
        let start_delay_ms = u64::try_from(
            traffic_start
                .saturating_duration_since(scheduled)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        *self
            .timing
            .lock()
            .map_err(|_| TrafficStartError::Coordinator("start timing lock poisoned".into()))? =
            Some(GateTiming {
                readiness,
                traffic_start,
                start_delay_ms,
            });
        Ok(traffic_start)
    }
}

/// Run one versioned distributed worker over process stdin/stdout.
pub async fn run_stdio_agent() -> Result<()> {
    let policy = policy_from_environment()?;
    run_agent(NdjsonChannel::new(stdin(), stdout()), policy).await
}

async fn run_agent<C>(mut channel: C, policy: AgentPolicy) -> Result<()>
where
    C: AgentChannel,
{
    let hello = HelloFrame {
        agent_name: None,
        binary_version: mcp_loadtest::VERSION.to_owned(),
        scenarios: vec![SupportedScenario::Sustained, SupportedScenario::Pattern],
        max_concurrency: policy.max_concurrency,
    };
    let mut state = AgentStateMachine::new(hello);
    channel.send(&state.hello()).await?;

    let frame = channel
        .receive()
        .await?
        .ok_or_else(|| anyhow!("controller closed before prepare"))?;
    let prepare = match state.accept(frame)? {
        AgentDirective::Prepare(prepare) => prepare,
        _ => bail!("first controller directive was not prepare"),
    };
    let target = match policy.authorize(&prepare) {
        Ok(target) => target,
        Err(_) => {
            let frame = state.fail(
                "policy_denied",
                "worker policy rejected the distributed target or workload",
                false,
            )?;
            channel.send(&frame).await?;
            bail!("worker policy rejected prepare");
        }
    };
    let config = worker_config(&target, &prepare.plan)?;
    let scenario = build_scenario(&config.scenario.kind, &config.scenario.params)?;
    let recorder = Recorder::new();
    let (readiness_tx, mut readiness_rx) = mpsc::unbounded_channel();
    let (start_tx, start_rx) = watch::channel(None);
    let gate = Arc::new(WorkerGate {
        readiness_tx,
        start_rx,
        timing: Mutex::new(None),
    });
    let run = Run::new(
        config,
        scenario,
        std::env::temp_dir().join("mcp-loadtest-agent-runs"),
    )
    .with_metrics_recorder(recorder.clone())
    .with_traffic_start_gate(gate.clone())
    .with_rng_seed(prepare.plan.seed);
    let run_task = tokio::spawn(run.execute());
    let heartbeat_every = Duration::from_millis(prepare.heartbeat_interval_ms);
    let mut next_heartbeat = Instant::now() + heartbeat_every;
    let mut heartbeat_sequence = 0u64;
    let mut sent_ready = false;

    loop {
        if !sent_ready && let Ok(readiness) = readiness_rx.try_recv() {
            let ready = ReadyFrame {
                job_id: prepare.job_id.clone(),
                agent_name: prepare.shard.agent_name.clone(),
                live_workers: readiness.live_workers,
                requested_workers: readiness.requested_workers,
                tool_inventory_hash: readiness.tool_inventory_hash,
                target_protocol_version: readiness.target_protocol_version,
            };
            channel.send(&state.mark_ready(ready)?).await?;
            sent_ready = true;
        }

        if Instant::now() >= next_heartbeat {
            heartbeat_sequence = heartbeat_sequence.saturating_add(1);
            channel.send(&state.heartbeat(heartbeat_sequence)?).await?;
            next_heartbeat = Instant::now() + heartbeat_every;
        }

        if run_task.is_finished() {
            let report = run_task
                .await
                .context("distributed engine task panicked")??;
            let timing = gate
                .timing
                .lock()
                .map_err(|_| anyhow!("start timing lock poisoned"))?
                .clone()
                .ok_or_else(|| anyhow!("run completed without traffic-start evidence"))?;
            let evidence =
                build_evidence(&prepare.shard, &recorder, report.scenario_outcome, &timing)?;
            channel.send(&state.finish(evidence)?).await?;
            channel.close().await?;
            return Ok(());
        }

        match tokio::time::timeout(CONTROL_POLL, channel.receive()).await {
            Ok(Ok(Some(frame))) => match state.accept(frame)? {
                AgentDirective::Start(start) => {
                    start_tx
                        .send(Some(Duration::from_millis(start.start_after_ms)))
                        .map_err(|_| anyhow!("traffic gate stopped"))?;
                }
                AgentDirective::Cancel(cancel) => {
                    run_task.abort();
                    let _ = run_task.await;
                    channel.send(&state.cancelled(cancel.reason)?).await?;
                    channel.close().await?;
                    return Ok(());
                }
                AgentDirective::Prepare(_) => bail!("duplicate prepare directive"),
            },
            Ok(Ok(None)) => {
                run_task.abort();
                bail!("controller closed the control channel");
            }
            Ok(Err(error)) => {
                run_task.abort();
                return Err(error.into());
            }
            Err(_) => {}
        }
    }
}

fn worker_config(
    target: &RemoteTarget,
    plan: &mcp_loadtest_distributed::AgentWorkloadPlan,
) -> Result<Config> {
    let transport = match target.transport {
        mcp_loadtest_distributed::RemoteTransport::Http => "http",
        mcp_loadtest_distributed::RemoteTransport::Sse => "sse",
        mcp_loadtest_distributed::RemoteTransport::Ws => "ws",
    };
    let patterns: Vec<_> = plan
        .patterns
        .iter()
        .map(|pattern| {
            serde_json::json!({
                "name": pattern.name,
                "weight": pattern.weight,
                "think_time": format!("{}ms", pattern.think_time_ms),
                "on_step_error": match pattern.on_step_error {
                    mcp_loadtest_distributed::PatternErrorPolicy::Continue => "continue",
                    mcp_loadtest_distributed::PatternErrorPolicy::Abort => "abort",
                },
                "steps": pattern.steps.iter().map(|step| serde_json::json!({
                    "tool": step.tool,
                    "args": step.args,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut server = serde_json::json!({
        "transport": transport,
        "url": target.url,
        "startup_timeout": format!("{}ms", target.startup_timeout_ms),
        "allowed_hosts": target.allowed_hosts,
        "protocol_version": target.protocol_version,
        "headers_from_env": target.headers_from_env,
    });
    if let Some(auth) = &target.auth {
        server["auth"] = serde_json::json!({
            "type": "oauth",
            "flow": "client_credentials",
            "registration": "pre_registered",
            "client_id": auth.client_id,
            "client_secret_env": auth.client_secret_env,
            "token_endpoint_auth_method": auth.token_endpoint_auth_method,
            "scopes": auth.scopes,
            "offline_access": auth.offline_access,
            "max_step_up_retries": auth.max_step_up_retries,
        });
    }
    let kind = match plan.scenario {
        SupportedScenario::Sustained => "sustained",
        SupportedScenario::Pattern => "pattern",
    };
    let document = serde_json::json!({
        "server": server,
        "scenario": {
            "type": kind,
            "concurrent": plan.concurrency,
            "duration": format!("{}ms", plan.duration_ms),
            "patterns": patterns,
        },
        "validation": { "strict": target.strict_validation },
    });
    let config: Config =
        serde_json::from_value(document).context("decoding normalized worker config")?;
    config
        .validate()
        .context("validating normalized worker config")?;
    Ok(config)
}

fn build_evidence(
    shard: &AgentShard,
    recorder: &Recorder,
    scenario_outcome: mcp_loadtest::ScenarioOutcome,
    timing: &GateTiming,
) -> Result<AgentEvidence> {
    let snapshot = recorder.snapshot();
    let per_tool_snapshots = recorder.snapshot_per_tool();
    let per_tool_histograms = recorder.per_tool_latency_histograms();
    let mut per_tool = BTreeMap::new();
    for (tool, metrics) in per_tool_snapshots {
        let histogram = per_tool_histograms
            .get(&tool)
            .ok_or_else(|| anyhow!("missing exact histogram for tool `{tool}`"))?;
        per_tool.insert(
            tool,
            MetricsEvidence {
                latency: HistogramEvidence::from_histogram(histogram)?,
                outcomes: metrics.outcomes,
            },
        );
    }
    Ok(AgentEvidence {
        agent_name: shard.agent_name.clone(),
        shard: shard.clone(),
        metrics: MetricsEvidence {
            latency: HistogramEvidence::from_histogram(&recorder.latency_histogram())?,
            outcomes: snapshot.outcomes,
        },
        per_tool,
        scenario_outcome,
        measurement_elapsed_ms: u64::try_from(timing.traffic_start.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1),
        start_delay_ms: timing.start_delay_ms,
        target_protocol_version: timing.readiness.target_protocol_version.clone(),
        tool_inventory_hash: timing.readiness.tool_inventory_hash.clone(),
    })
}

fn policy_from_environment() -> Result<AgentPolicy> {
    let allowed_target_hosts = std::env::var("MCP_LOADTEST_AGENT_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let allow_plaintext = env_bool("MCP_LOADTEST_AGENT_ALLOW_PLAINTEXT")?;
    let max_concurrency = env_u32("MCP_LOADTEST_AGENT_MAX_CONCURRENCY")?.unwrap_or(10_000);
    let max_duration = env_duration("MCP_LOADTEST_AGENT_MAX_DURATION")?
        .unwrap_or(Duration::from_secs(24 * 60 * 60));
    let max_startup_timeout = env_duration("MCP_LOADTEST_AGENT_MAX_STARTUP_TIMEOUT")?
        .unwrap_or(Duration::from_secs(5 * 60));
    Ok(AgentPolicy {
        allowed_target_hosts,
        allow_plaintext,
        max_concurrency,
        max_duration,
        max_startup_timeout,
    })
}

fn env_bool(name: &str) -> Result<bool> {
    match std::env::var(name) {
        Ok(value) => match value.as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" | "" => Ok(false),
            _ => Err(anyhow!("{name} must be true/false")),
        },
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(_) => Err(anyhow!("{name} is not valid Unicode")),
    }
}

fn env_u32(name: &str) -> Result<Option<u32>> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| anyhow!("{name} must be a positive integer")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(anyhow!("{name} is not valid Unicode")),
    }
}

fn env_duration(name: &str) -> Result<Option<Duration>> {
    match std::env::var(name) {
        Ok(value) => humantime::parse_duration(&value)
            .map(Some)
            .map_err(|_| anyhow!("{name} must be a positive duration")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(anyhow!("{name} is not valid Unicode")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Stdio;

    use mcp_loadtest_distributed::{
        AgentWorkloadPlan, PatternErrorPolicy, PatternPlan, PatternStepPlan, PrepareFrame,
        RemoteTransport, StartFrame, WireFrame, WireMessage, aggregate_evidence,
    };
    use tokio::io::{AsyncBufReadExt, BufReader, duplex, split};
    use tokio::process::Command;

    use super::*;

    #[tokio::test]
    async fn two_local_workers_coordinate_and_merge_exact_evidence() {
        let mut server = spawn_mock_http().await;
        let address = read_listening_address(&mut server).await;
        let target = RemoteTarget {
            transport: RemoteTransport::Http,
            url: format!("http://{address}/"),
            startup_timeout_ms: 5_000,
            protocol_version: Some("2025-03-26".to_owned()),
            headers_from_env: BTreeMap::new(),
            allowed_hosts: vec!["127.0.0.1".to_owned()],
            strict_validation: false,
            auth: None,
        };
        let plan = AgentWorkloadPlan {
            scenario: SupportedScenario::Sustained,
            concurrency: 1,
            duration_ms: 150,
            patterns: vec![PatternPlan {
                name: "echo".to_owned(),
                weight: 1.0,
                think_time_ms: 0,
                on_step_error: PatternErrorPolicy::Continue,
                steps: vec![PatternStepPlan {
                    tool: "echo".to_owned(),
                    args: serde_json::json!({"distributed": true}),
                }],
            }],
            seed: 42,
        };
        let policy = AgentPolicy {
            allowed_target_hosts: BTreeSet::from(["127.0.0.1".to_owned()]),
            allow_plaintext: true,
            max_concurrency: 4,
            max_duration: Duration::from_secs(5),
            max_startup_timeout: Duration::from_secs(10),
        };

        let mut controllers = Vec::new();
        let mut workers = Vec::new();
        for (index, name) in ["east", "west"].into_iter().enumerate() {
            let (worker_io, controller_io) = duplex(16 * 1024 * 1024);
            let (worker_read, worker_write) = split(worker_io);
            let (controller_read, controller_write) = split(controller_io);
            workers.push(tokio::spawn(run_agent(
                NdjsonChannel::new(worker_read, worker_write),
                policy.clone(),
            )));
            controllers.push((
                name.to_owned(),
                u32::try_from(index).unwrap(),
                NdjsonChannel::new(controller_read, controller_write),
            ));
        }

        for (name, index, channel) in &mut controllers {
            let hello = channel.receive().await.unwrap().unwrap();
            assert!(matches!(hello.message, WireMessage::Hello(_)));
            let shard = AgentShard {
                agent_name: name.clone(),
                index: *index,
                agent_count: 2,
                concurrency: 1,
            };
            channel
                .send(&WireFrame::new(WireMessage::Prepare(PrepareFrame {
                    job_id: "01LOCALDISTTEST".to_owned(),
                    config_digest: "test-digest".to_owned(),
                    target: target.clone(),
                    plan: plan.clone(),
                    shard,
                    heartbeat_interval_ms: 50,
                })))
                .await
                .unwrap();
        }

        for (_, _, channel) in &mut controllers {
            loop {
                let frame = channel.receive().await.unwrap().unwrap();
                if matches!(frame.message, WireMessage::Ready(_)) {
                    break;
                }
            }
        }
        for (_, _, channel) in &mut controllers {
            channel
                .send(&WireFrame::new(WireMessage::Start(StartFrame {
                    job_id: "01LOCALDISTTEST".to_owned(),
                    start_after_ms: 50,
                })))
                .await
                .unwrap();
        }

        let mut evidence = Vec::new();
        for (_, _, channel) in &mut controllers {
            loop {
                let frame = channel.receive().await.unwrap().unwrap();
                if let WireMessage::Finished(finished) = frame.message {
                    evidence.push(finished.evidence);
                    break;
                }
            }
        }
        let aggregate = aggregate_evidence(&evidence).unwrap();
        assert_eq!(aggregate.agent_names, vec!["east", "west"]);
        assert_eq!(aggregate.global_concurrency, 2);
        assert!(aggregate.metrics.throughput.total_requests > 0);
        assert_eq!(
            aggregate.metrics.latency.count,
            aggregate.metrics.outcomes.success + aggregate.metrics.outcomes.hang
        );
        assert!(aggregate.per_tool.contains_key("echo"));

        for worker in workers {
            worker.await.unwrap().unwrap();
        }
        let _ = server.kill().await;
    }

    async fn spawn_mock_http() -> tokio::process::Child {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../engine/tests/fixtures/mock-http-server.py");
        let python = if cfg!(windows) { "python" } else { "python3" };
        Command::new(python)
            .arg(script)
            .arg("--port")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn Python MCP HTTP fixture")
    }

    async fn read_listening_address(server: &mut tokio::process::Child) -> String {
        let stdout = server.stdout.take().expect("fixture stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .expect("fixture listening timeout")
            .expect("fixture stdout read");
        line.trim()
            .strip_prefix("LISTENING: ")
            .expect("fixture listening line")
            .to_owned()
    }
}
