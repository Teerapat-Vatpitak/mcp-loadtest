//! SSH controller runtime for one fail-closed distributed run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use mcp_loadtest::config::{Config, sanitize_remote_endpoint};
use mcp_loadtest::report::{ProcessStats, Report, ServerInfo};
use mcp_loadtest::run::evaluate_report_thresholds;
use mcp_loadtest_distributed::{
    AgentChannel, ControllerAgentPhase, ControllerJobState, PrepareFrame, SshAgentProcess,
    SshAgentSpec, SshLauncher, WireFrame, aggregate_evidence, plan_shards,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{config_digest, remote_target, workload_plan};

const PEER_POLL: Duration = Duration::from_millis(50);
const MAX_SSH_DIAGNOSTIC_BYTES: usize = 16 * 1024;

enum PeerEvent {
    Frame {
        agent: String,
        frame: Box<WireFrame>,
    },
    Closed {
        agent: String,
        diagnostic: String,
    },
}

struct Peer {
    commands: mpsc::UnboundedSender<WireFrame>,
    task: JoinHandle<()>,
}

/// Execute a distributed workload and return one exact aggregate report.
pub(crate) async fn run_controller(config: &Config) -> Result<Report> {
    let distributed = config
        .distributed
        .as_ref()
        .ok_or_else(|| anyhow!("distributed controller requires [distributed]"))?;
    let started_at = SystemTime::now();
    let wall_start = Instant::now();
    let plan = workload_plan(config)?;
    let target = remote_target(config)?;
    let digest = config_digest(config)?;
    let names: Vec<String> = distributed
        .agents
        .iter()
        .map(|agent| agent.name.clone())
        .collect();
    let shards = plan_shards(plan.global_concurrency, &names)?;
    let agents_by_name: BTreeMap<_, _> = distributed
        .agents
        .iter()
        .map(|agent| (agent.name.clone(), agent))
        .collect();
    let job_id = ulid::Ulid::new().to_string();
    let heartbeat_interval = distributed.heartbeat_timeout.div_f64(3.0);
    let heartbeat_interval_ms = millis(heartbeat_interval.max(Duration::from_millis(250)))?;

    let prepares: Vec<PrepareFrame> = shards
        .iter()
        .map(|shard| PrepareFrame {
            job_id: job_id.clone(),
            config_digest: digest.clone(),
            target: target.clone(),
            plan: plan.for_shard(shard),
            shard: shard.clone(),
            heartbeat_interval_ms,
        })
        .collect();
    let mut state = ControllerJobState::new(prepares)?;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut peers = BTreeMap::new();

    for name in &names {
        let inventory = agents_by_name
            .get(name)
            .ok_or_else(|| anyhow!("missing distributed inventory entry `{name}`"))?;
        let spec = SshAgentSpec {
            name: name.clone(),
            destination: inventory.ssh_host.clone(),
            port: inventory.ssh_port,
            identity_file: inventory.identity_file.clone(),
            known_hosts_file: inventory.known_hosts_file.clone(),
            connect_timeout: distributed.connect_timeout,
        };
        let process = SshLauncher::new()
            .launch(&spec)
            .with_context(|| format!("launching distributed agent `{name}`"))?;
        let (commands, task) = spawn_peer(name.clone(), process, events_tx.clone());
        peers.insert(name.clone(), Peer { commands, task });
    }
    drop(events_tx);

    let result = drive_controller(
        &mut state,
        &mut peers,
        &mut events_rx,
        distributed.connect_timeout,
        distributed.ready_timeout,
        distributed.heartbeat_timeout,
        distributed.start_lead,
        plan.duration_ms,
    )
    .await;

    if let Err(error) = result {
        cancel_and_stop(&mut state, &mut peers, "distributed run failed").await;
        return Err(error);
    }

    stop_peers(&mut peers).await;
    let evidence: Vec<_> = state.final_evidence()?.into_iter().cloned().collect();
    let aggregate = aggregate_evidence(&evidence)?;
    let mut outcome = aggregate.scenario_outcome.clone();
    outcome.notes.push(format!(
        "distributed agents: {}; global concurrency: {}; observed start skew: {}ms",
        aggregate.agent_names.join(", "),
        aggregate.global_concurrency,
        aggregate.start_skew_ms
    ));
    let mut report = Report {
        run_id: job_id,
        started_at,
        duration: wall_start.elapsed(),
        scenario_name: config.scenario.kind.clone(),
        server_info: ServerInfo {
            command: sanitize_remote_endpoint(config.server.url.as_deref().unwrap_or_default()),
            args: Vec::new(),
            pid: None,
            protocol_version: Some(aggregate.target_protocol_version.clone()),
        },
        metrics: aggregate.metrics,
        process: ProcessStats::default(),
        scenario_outcome: outcome,
        trace_path: None,
        threshold_violations: Vec::new(),
        // The v1 wire carries a canonical inventory hash, not the inventory
        // names themselves. Reporting only exercised tools as "registered"
        // would create a false 100% coverage claim.
        coverage: None,
    };
    evaluate_report_thresholds(config, &mut report, &aggregate.per_tool);
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn drive_controller(
    state: &mut ControllerJobState,
    peers: &mut BTreeMap<String, Peer>,
    events: &mut mpsc::UnboundedReceiver<PeerEvent>,
    connect_timeout: Duration,
    ready_timeout: Duration,
    heartbeat_timeout: Duration,
    start_lead: Duration,
    measurement_ms: u64,
) -> Result<()> {
    let connect_deadline = Instant::now() + connect_timeout;
    let mut seen = BTreeSet::new();
    while seen.len() < peers.len() {
        let (agent, frame) = receive_until(events, connect_deadline, "connecting agents").await?;
        if state.phase(&agent) != Some(ControllerAgentPhase::AwaitingHello) {
            bail!("agent `{agent}` sent an unexpected frame while connecting");
        }
        let response = state.accept(&agent, frame)?;
        seen.insert(agent.clone());
        if let Some(prepare) = response {
            send_to(peers, &agent, prepare)?;
        }
    }

    let ready_deadline = Instant::now() + ready_timeout;
    while !state.all_ready() {
        let (agent, frame) = receive_until(events, ready_deadline, "preparing agents").await?;
        if let Some(response) = state.accept(&agent, frame)? {
            send_to(peers, &agent, response)?;
        }
        if state.has_failure() {
            bail!("agent `{agent}` failed while preparing");
        }
    }

    let starts = state.start_all(millis(start_lead)?)?;
    for (agent, frame) in starts {
        send_to(peers, &agent, frame)?;
    }

    let maximum_run = Duration::from_millis(measurement_ms)
        .saturating_add(start_lead)
        .saturating_add(heartbeat_timeout)
        .saturating_add(Duration::from_secs(10));
    let run_deadline = Instant::now() + maximum_run;
    let mut last_seen: BTreeMap<String, Instant> = peers
        .keys()
        .map(|name| (name.clone(), Instant::now()))
        .collect();

    while !state.is_terminal() {
        if Instant::now() >= run_deadline {
            bail!("distributed run exceeded its bounded completion deadline");
        }
        match tokio::time::timeout(PEER_POLL, events.recv()).await {
            Ok(Some(PeerEvent::Frame { agent, frame })) => {
                last_seen.insert(agent.clone(), Instant::now());
                state.accept(&agent, *frame)?;
                if state.has_failure() {
                    bail!("agent `{agent}` reported a terminal failure");
                }
            }
            Ok(Some(PeerEvent::Closed { agent, diagnostic })) => {
                bail!("agent `{agent}` disconnected ({diagnostic})");
            }
            Ok(None) => bail!("all distributed agent channels closed"),
            Err(_) => {}
        }
        for (agent, last) in &last_seen {
            let phase = state.phase(agent);
            if matches!(
                phase,
                Some(
                    ControllerAgentPhase::Preparing
                        | ControllerAgentPhase::Ready
                        | ControllerAgentPhase::Running
                )
            ) && last.elapsed() > heartbeat_timeout
            {
                bail!("agent `{agent}` missed its heartbeat deadline");
            }
        }
    }
    if state.has_failure() {
        bail!("distributed run did not complete successfully");
    }
    Ok(())
}

async fn receive_until(
    events: &mut mpsc::UnboundedReceiver<PeerEvent>,
    deadline: Instant,
    phase: &str,
) -> Result<(String, WireFrame)> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| anyhow!("timeout while {phase}"))?;
    match tokio::time::timeout(remaining, events.recv()).await {
        Ok(Some(PeerEvent::Frame { agent, frame })) => Ok((agent, *frame)),
        Ok(Some(PeerEvent::Closed { agent, diagnostic })) => {
            bail!("agent `{agent}` disconnected while {phase} ({diagnostic})")
        }
        Ok(None) => bail!("all agent channels closed while {phase}"),
        Err(_) => bail!("timeout while {phase}"),
    }
}

fn send_to(peers: &BTreeMap<String, Peer>, agent: &str, frame: WireFrame) -> Result<()> {
    peers
        .get(agent)
        .ok_or_else(|| anyhow!("unknown distributed peer `{agent}`"))?
        .commands
        .send(frame)
        .map_err(|_| anyhow!("agent `{agent}` control task stopped"))
}

fn spawn_peer(
    agent: String,
    mut process: SshAgentProcess,
    events: mpsc::UnboundedSender<PeerEvent>,
) -> (mpsc::UnboundedSender<WireFrame>, JoinHandle<()>) {
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<WireFrame>();
    let stderr = process.take_stderr();
    let task = tokio::spawn(async move {
        let diagnostic = Arc::new(tokio::sync::Mutex::new(String::new()));
        let stderr_task = stderr.map(|stderr| {
            let diagnostic = Arc::clone(&diagnostic);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let mut output = diagnostic.lock().await;
                    let remaining = MAX_SSH_DIAGNOSTIC_BYTES.saturating_sub(output.len());
                    if remaining == 0 {
                        break;
                    }
                    let sanitized = sanitize_ssh_diagnostic(&line);
                    push_bounded_utf8(&mut output, &sanitized, remaining);
                    line.clear();
                }
            })
        });

        loop {
            while let Ok(frame) = commands_rx.try_recv() {
                if process.channel_mut().send(&frame).await.is_err() {
                    break;
                }
            }
            match tokio::time::timeout(PEER_POLL, process.channel_mut().receive()).await {
                Ok(Ok(Some(frame))) => {
                    if events
                        .send(PeerEvent::Frame {
                            agent: agent.clone(),
                            frame: Box::new(frame),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) => break,
                Err(_) => {
                    if commands_rx.is_closed() {
                        break;
                    }
                }
            }
        }
        if let Some(task) = stderr_task {
            let _ = task.await;
        }
        let diagnostic = diagnostic.lock().await.clone();
        let diagnostic = if diagnostic.trim().is_empty() {
            "control channel closed".to_owned()
        } else {
            diagnostic.trim().to_owned()
        };
        let _ = events.send(PeerEvent::Closed { agent, diagnostic });
        let _ = process.kill().await;
    });
    (commands_tx, task)
}

fn sanitize_ssh_diagnostic(input: &str) -> String {
    let lowered = input.to_ascii_lowercase();
    if ["authorization", "bearer", "token", "secret", "password"]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        "[redacted ssh diagnostic]\n".to_owned()
    } else {
        input
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .collect()
    }
}

fn push_bounded_utf8(output: &mut String, input: &str, maximum_bytes: usize) {
    let mut end = input.len().min(maximum_bytes);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&input[..end]);
}

async fn cancel_and_stop(
    state: &mut ControllerJobState,
    peers: &mut BTreeMap<String, Peer>,
    reason: &str,
) {
    for (agent, frame) in state.cancel_all(reason) {
        let _ = send_to(peers, &agent, frame);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop_peers(peers).await;
}

async fn stop_peers(peers: &mut BTreeMap<String, Peer>) {
    for (_, peer) in std::mem::take(peers) {
        drop(peer.commands);
        peer.task.abort();
        let _ = peer.task.await;
    }
}

fn millis(duration: Duration) -> Result<u64> {
    u64::try_from(duration.as_millis()).map_err(|_| anyhow!("duration is too large"))
}

#[cfg(test)]
mod tests {
    use super::{push_bounded_utf8, sanitize_ssh_diagnostic};

    #[test]
    fn bounded_diagnostic_truncation_preserves_utf8() {
        let mut output = String::new();
        push_bounded_utf8(&mut output, "ok-ภาษาไทย", 7);
        assert_eq!(output, "ok-ภ");
        assert!(output.is_char_boundary(output.len()));
    }

    #[test]
    fn sensitive_ssh_diagnostic_is_replaced() {
        assert_eq!(
            sanitize_ssh_diagnostic("Bearer secret-value\n"),
            "[redacted ssh diagnostic]\n"
        );
    }
}
