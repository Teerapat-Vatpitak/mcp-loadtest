//! Exact cross-agent metric aggregation.
//!
//! Percentiles are not composable: averaging two p99 values does not produce
//! the p99 of the combined sample population. Workers therefore transmit an
//! HDR Histogram V2 payload. The controller deserializes and adds those
//! histograms, then computes every percentile once from the merged
//! distribution.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hdrhistogram::Histogram;
use hdrhistogram::serialization::{Deserializer, Serializer, V2Serializer};
use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::AgentShard;

/// Maximum accepted decoded histogram payload per global or per-tool series.
pub const MAX_HISTOGRAM_BYTES: usize = 4 * 1024 * 1024;

/// Portable exact histogram payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistogramEvidence {
    /// Encoding identifier. Currently always `hdr-v2-base64`.
    pub encoding: String,
    /// Base64 without padding of an HDR Histogram V2 binary stream.
    pub payload: String,
    /// Declared sample count, checked against the decoded histogram.
    pub count: u64,
}

impl HistogramEvidence {
    /// Encode a histogram using the interoperable HDR V2 format.
    pub fn from_histogram(histogram: &Histogram<u64>) -> Result<Self, AggregateError> {
        let mut bytes = Vec::new();
        V2Serializer::new()
            .serialize(histogram, &mut bytes)
            .map_err(|error| AggregateError::HistogramEncode(format!("{error:?}")))?;
        if bytes.len() > MAX_HISTOGRAM_BYTES {
            return Err(AggregateError::HistogramTooLarge {
                bytes: bytes.len(),
                limit: MAX_HISTOGRAM_BYTES,
            });
        }
        Ok(Self {
            encoding: "hdr-v2-base64".to_owned(),
            payload: STANDARD_NO_PAD.encode(bytes),
            count: histogram.len(),
        })
    }

    /// Decode and validate an HDR V2 payload.
    pub fn to_histogram(&self) -> Result<Histogram<u64>, AggregateError> {
        if self.encoding != "hdr-v2-base64" {
            return Err(AggregateError::UnsupportedHistogramEncoding(
                self.encoding.clone(),
            ));
        }
        // Four base64 characters represent at most three bytes. Reject an
        // oversized string before asking the decoder to allocate.
        let max_encoded = MAX_HISTOGRAM_BYTES.div_ceil(3) * 4;
        if self.payload.len() > max_encoded {
            return Err(AggregateError::HistogramTooLarge {
                bytes: self.payload.len(),
                limit: max_encoded,
            });
        }
        let bytes = STANDARD_NO_PAD
            .decode(self.payload.as_bytes())
            .map_err(|error| AggregateError::HistogramDecode(error.to_string()))?;
        if bytes.len() > MAX_HISTOGRAM_BYTES {
            return Err(AggregateError::HistogramTooLarge {
                bytes: bytes.len(),
                limit: MAX_HISTOGRAM_BYTES,
            });
        }
        let mut cursor = Cursor::new(bytes);
        let histogram: Histogram<u64> = Deserializer::new()
            .deserialize(&mut cursor)
            .map_err(|error| AggregateError::HistogramDecode(format!("{error:?}")))?;
        if usize::try_from(cursor.position()).ok() != Some(cursor.get_ref().len()) {
            return Err(AggregateError::TrailingHistogramData);
        }
        if histogram.len() != self.count {
            return Err(AggregateError::HistogramCountMismatch {
                declared: self.count,
                decoded: histogram.len(),
            });
        }
        Ok(histogram)
    }
}

/// Mergeable metric evidence from one scope (global or one tool).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsEvidence {
    /// Exact latency histogram for success, hang, and deadlock outcomes.
    pub latency: HistogramEvidence,
    /// Per-outcome counters.
    pub outcomes: OutcomeCounts,
}

/// Final or cumulative evidence produced by one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvidence {
    /// Inventory name of the producing agent.
    pub agent_name: String,
    /// Deterministic concurrency shard.
    pub shard: AgentShard,
    /// Global call evidence for this agent.
    pub metrics: MetricsEvidence,
    /// Exact evidence keyed by MCP tool name.
    #[serde(default)]
    pub per_tool: BTreeMap<String, MetricsEvidence>,
    /// Structured scenario counters.
    pub scenario_outcome: ScenarioOutcome,
    /// Local measurement interval only, excluding prepare and teardown.
    pub measurement_elapsed_ms: u64,
    /// Local scheduling delay after the relative start timer elapsed.
    pub start_delay_ms: u64,
    /// Target MCP revision observed after discovery.
    pub target_protocol_version: String,
    /// Canonical hash of the target's `tools/list` inventory.
    pub tool_inventory_hash: String,
}

/// Exact aggregate of a complete agent cohort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedEvidence {
    /// Sorted active agent names.
    pub agent_names: Vec<String>,
    /// Total requested global concurrency.
    pub global_concurrency: u32,
    /// Aggregate metrics computed from merged histograms and counters.
    pub metrics: ScenarioMetrics,
    /// Aggregate metrics per tool.
    pub per_tool: BTreeMap<String, ScenarioMetrics>,
    /// Checked sum of agent scenario outcomes.
    pub scenario_outcome: ScenarioOutcome,
    /// Denominator used for aggregate requests per second.
    pub measurement_elapsed_ms: u64,
    /// Difference between the largest and smallest local start delay.
    pub start_skew_ms: u64,
    /// Common target MCP revision.
    pub target_protocol_version: String,
    /// Common target tool-inventory hash.
    pub tool_inventory_hash: String,
}

/// Aggregation or evidence-validation failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AggregateError {
    /// No final evidence was supplied.
    #[error("cannot aggregate an empty agent cohort")]
    EmptyCohort,
    /// Agent names must be unique.
    #[error("duplicate agent evidence for `{0}`")]
    DuplicateAgent(String),
    /// Evidence identity and shard identity disagree.
    #[error("agent `{agent}` evidence has shard for `{shard_agent}`")]
    ShardIdentityMismatch {
        /// Evidence agent name.
        agent: String,
        /// Shard agent name.
        shard_agent: String,
    },
    /// Shard cohort metadata is inconsistent.
    #[error("agent `{agent}` reported invalid shard metadata")]
    InvalidShard {
        /// Agent with invalid metadata.
        agent: String,
    },
    /// Agents observed different MCP revisions.
    #[error("target protocol mismatch: `{expected}` vs `{actual}` on `{agent}`")]
    TargetProtocolMismatch {
        /// First observed revision.
        expected: String,
        /// Conflicting revision.
        actual: String,
        /// Conflicting agent.
        agent: String,
    },
    /// Agents observed different tool inventories.
    #[error("tool inventory mismatch on agent `{0}`")]
    ToolInventoryMismatch(String),
    /// Histogram encoding is unknown.
    #[error("unsupported histogram encoding `{0}`")]
    UnsupportedHistogramEncoding(String),
    /// Histogram encoding failed.
    #[error("histogram encoding failed: {0}")]
    HistogramEncode(String),
    /// Histogram decoding failed.
    #[error("histogram decoding failed: {0}")]
    HistogramDecode(String),
    /// Bytes remained after the single expected histogram payload.
    #[error("histogram payload contains trailing data")]
    TrailingHistogramData,
    /// Histogram evidence exceeded its fixed bound.
    #[error("histogram payload is {bytes} bytes; limit is {limit}")]
    HistogramTooLarge {
        /// Observed size.
        bytes: usize,
        /// Accepted maximum.
        limit: usize,
    },
    /// Declared and decoded histogram counts disagree.
    #[error("histogram declared {declared} samples but decoded {decoded}")]
    HistogramCountMismatch {
        /// Declared sample count.
        declared: u64,
        /// Decoded sample count.
        decoded: u64,
    },
    /// Histogram shape differs across agents.
    #[error("incompatible histogram layout on agent `{agent}` for `{scope}`")]
    HistogramLayoutMismatch {
        /// Agent whose histogram differed.
        agent: String,
        /// `global` or tool name.
        scope: String,
    },
    /// Histogram sample count does not match latency-bearing outcomes.
    #[error(
        "agent `{agent}` scope `{scope}` has {histogram} histogram samples but {outcomes} latency outcomes"
    )]
    LatencyOutcomeMismatch {
        /// Agent with inconsistent evidence.
        agent: String,
        /// `global` or tool name.
        scope: String,
        /// Histogram sample count.
        histogram: u64,
        /// Success + hang + deadlock count.
        outcomes: u64,
    },
    /// Checked counter summation overflowed.
    #[error("counter overflow while merging `{0}`")]
    CounterOverflow(&'static str),
    /// Requests exist but no positive measurement interval was reported.
    #[error("non-empty metrics require a positive measurement interval")]
    MissingMeasurementInterval,
}

/// Merge a complete cohort into exact global and per-tool summaries.
///
/// Callers must pass final evidence exactly once per active agent. Progress
/// checkpoints are cumulative and replace earlier checkpoints from the same
/// agent; they must not be appended to this slice.
pub fn aggregate_evidence(
    evidence: &[AgentEvidence],
) -> Result<AggregatedEvidence, AggregateError> {
    let first = evidence.first().ok_or(AggregateError::EmptyCohort)?;
    let expected_count = u32::try_from(evidence.len())
        .map_err(|_| AggregateError::CounterOverflow("agent_count"))?;
    let expected_protocol = first.target_protocol_version.clone();
    let expected_inventory = first.tool_inventory_hash.clone();

    let mut names = BTreeSet::new();
    let mut shard_indices = BTreeSet::new();
    let mut global_concurrency = 0u32;
    let mut min_start_delay = u64::MAX;
    let mut max_start_delay = 0u64;
    let mut measurement_elapsed_ms = 0u64;
    let mut merged_histogram: Option<Histogram<u64>> = None;
    let mut outcomes = OutcomeCounts::default();
    let mut scenario_outcome = ScenarioOutcome::default();
    let mut per_tool_histograms: BTreeMap<String, Histogram<u64>> = BTreeMap::new();
    let mut per_tool_outcomes: BTreeMap<String, OutcomeCounts> = BTreeMap::new();

    for agent in evidence {
        if !names.insert(agent.agent_name.clone()) {
            return Err(AggregateError::DuplicateAgent(agent.agent_name.clone()));
        }
        if agent.agent_name != agent.shard.agent_name {
            return Err(AggregateError::ShardIdentityMismatch {
                agent: agent.agent_name.clone(),
                shard_agent: agent.shard.agent_name.clone(),
            });
        }
        if agent.shard.agent_count != expected_count
            || agent.shard.index >= expected_count
            || agent.shard.concurrency == 0
            || !shard_indices.insert(agent.shard.index)
        {
            return Err(AggregateError::InvalidShard {
                agent: agent.agent_name.clone(),
            });
        }
        if agent.target_protocol_version != expected_protocol {
            return Err(AggregateError::TargetProtocolMismatch {
                expected: expected_protocol,
                actual: agent.target_protocol_version.clone(),
                agent: agent.agent_name.clone(),
            });
        }
        if agent.tool_inventory_hash != expected_inventory {
            return Err(AggregateError::ToolInventoryMismatch(
                agent.agent_name.clone(),
            ));
        }

        global_concurrency = global_concurrency
            .checked_add(agent.shard.concurrency)
            .ok_or(AggregateError::CounterOverflow("global_concurrency"))?;
        min_start_delay = min_start_delay.min(agent.start_delay_ms);
        max_start_delay = max_start_delay.max(agent.start_delay_ms);
        measurement_elapsed_ms = measurement_elapsed_ms.max(agent.measurement_elapsed_ms);

        let histogram = decode_validated_metrics(&agent.agent_name, "global", &agent.metrics)?;
        merge_histogram(
            &mut merged_histogram,
            histogram,
            &agent.agent_name,
            "global",
        )?;
        add_outcomes(&mut outcomes, &agent.metrics.outcomes)?;
        add_scenario_outcome(
            &mut scenario_outcome,
            &agent.scenario_outcome,
            &agent.agent_name,
        )?;

        for (tool, metrics) in &agent.per_tool {
            let histogram = decode_validated_metrics(&agent.agent_name, tool, metrics)?;
            if let Some(into) = per_tool_histograms.get_mut(tool) {
                ensure_compatible(into, &histogram, &agent.agent_name, tool)?;
                into.add(&histogram)
                    .map_err(|_| AggregateError::HistogramLayoutMismatch {
                        agent: agent.agent_name.clone(),
                        scope: tool.clone(),
                    })?;
            } else {
                per_tool_histograms.insert(tool.clone(), histogram);
            }
            add_outcomes(
                per_tool_outcomes.entry(tool.clone()).or_default(),
                &metrics.outcomes,
            )?;
        }
    }

    let histogram = merged_histogram.ok_or(AggregateError::EmptyCohort)?;
    let total_requests = outcome_total(&outcomes)?;
    if total_requests > 0 && measurement_elapsed_ms == 0 {
        return Err(AggregateError::MissingMeasurementInterval);
    }
    let metrics = summarize(&histogram, outcomes, measurement_elapsed_ms)?;

    let mut per_tool = BTreeMap::new();
    for (tool, histogram) in per_tool_histograms {
        let tool_outcomes = per_tool_outcomes.remove(&tool).unwrap_or_default();
        per_tool.insert(
            tool,
            summarize(&histogram, tool_outcomes, measurement_elapsed_ms)?,
        );
    }

    let mut agent_names: Vec<String> = names.into_iter().collect();
    agent_names.sort();
    Ok(AggregatedEvidence {
        agent_names,
        global_concurrency,
        metrics,
        per_tool,
        scenario_outcome,
        measurement_elapsed_ms,
        start_skew_ms: max_start_delay.saturating_sub(min_start_delay),
        target_protocol_version: first.target_protocol_version.clone(),
        tool_inventory_hash: first.tool_inventory_hash.clone(),
    })
}

fn decode_validated_metrics(
    agent: &str,
    scope: &str,
    metrics: &MetricsEvidence,
) -> Result<Histogram<u64>, AggregateError> {
    let histogram = metrics.latency.to_histogram()?;
    let latency_outcomes = metrics
        .outcomes
        .success
        .checked_add(metrics.outcomes.hang)
        .and_then(|value| value.checked_add(metrics.outcomes.deadlock))
        .ok_or(AggregateError::CounterOverflow("latency_outcomes"))?;
    if histogram.len() != latency_outcomes {
        return Err(AggregateError::LatencyOutcomeMismatch {
            agent: agent.to_owned(),
            scope: scope.to_owned(),
            histogram: histogram.len(),
            outcomes: latency_outcomes,
        });
    }
    Ok(histogram)
}

fn merge_histogram(
    into: &mut Option<Histogram<u64>>,
    histogram: Histogram<u64>,
    agent: &str,
    scope: &str,
) -> Result<(), AggregateError> {
    if let Some(into) = into {
        ensure_compatible(into, &histogram, agent, scope)?;
        into.add(&histogram)
            .map_err(|_| AggregateError::HistogramLayoutMismatch {
                agent: agent.to_owned(),
                scope: scope.to_owned(),
            })?;
    } else {
        *into = Some(histogram);
    }
    Ok(())
}

fn ensure_compatible(
    left: &Histogram<u64>,
    right: &Histogram<u64>,
    agent: &str,
    scope: &str,
) -> Result<(), AggregateError> {
    if left.low() != right.low() || left.high() != right.high() || left.sigfig() != right.sigfig() {
        return Err(AggregateError::HistogramLayoutMismatch {
            agent: agent.to_owned(),
            scope: scope.to_owned(),
        });
    }
    Ok(())
}

fn summarize(
    histogram: &Histogram<u64>,
    outcomes: OutcomeCounts,
    measurement_elapsed_ms: u64,
) -> Result<ScenarioMetrics, AggregateError> {
    let count = histogram.len();
    let latency = if count == 0 {
        LatencyStats::default()
    } else {
        LatencyStats {
            p50: Duration::from_micros(histogram.value_at_quantile(0.50)),
            p95: Duration::from_micros(histogram.value_at_quantile(0.95)),
            p99: Duration::from_micros(histogram.value_at_quantile(0.99)),
            p999: Duration::from_micros(histogram.value_at_quantile(0.999)),
            mean: Duration::from_micros(histogram.mean() as u64),
            min: Duration::from_micros(histogram.min()),
            max: Duration::from_micros(histogram.max()),
            count,
        }
    };
    let total_requests = outcome_total(&outcomes)?;
    let successful_requests = outcomes
        .success
        .checked_add(outcomes.expected_rejection)
        .ok_or(AggregateError::CounterOverflow("successful_requests"))?;
    let requests_per_sec = if measurement_elapsed_ms == 0 {
        0.0
    } else {
        total_requests as f64 / (measurement_elapsed_ms as f64 / 1_000.0)
    };
    Ok(ScenarioMetrics {
        latency,
        throughput: ThroughputStats {
            total_requests,
            successful_requests,
            requests_per_sec,
        },
        outcomes,
    })
}

fn outcome_total(outcomes: &OutcomeCounts) -> Result<u64, AggregateError> {
    [
        ("success", outcomes.success),
        ("hang", outcomes.hang),
        ("deadlock", outcomes.deadlock),
        ("timeout", outcomes.timeout),
        ("server_error", outcomes.server_error),
        ("protocol_error", outcomes.protocol_error),
        ("crash", outcomes.crash),
        ("malformed", outcomes.malformed),
        ("disconnected", outcomes.disconnected),
        ("cancelled", outcomes.cancelled),
        ("expected_rejection", outcomes.expected_rejection),
    ]
    .into_iter()
    .try_fold(0u64, |total, (name, value)| {
        total
            .checked_add(value)
            .ok_or(AggregateError::CounterOverflow(name))
    })
}

fn add_outcomes(into: &mut OutcomeCounts, from: &OutcomeCounts) -> Result<(), AggregateError> {
    macro_rules! add {
        ($field:ident) => {
            into.$field = into
                .$field
                .checked_add(from.$field)
                .ok_or(AggregateError::CounterOverflow(stringify!($field)))?;
        };
    }
    add!(success);
    add!(hang);
    add!(deadlock);
    add!(timeout);
    add!(server_error);
    add!(protocol_error);
    add!(crash);
    add!(malformed);
    add!(disconnected);
    add!(cancelled);
    add!(expected_rejection);
    Ok(())
}

fn add_scenario_outcome(
    into: &mut ScenarioOutcome,
    from: &ScenarioOutcome,
    agent_name: &str,
) -> Result<(), AggregateError> {
    macro_rules! add_u64 {
        ($field:ident) => {
            into.$field = into
                .$field
                .checked_add(from.$field)
                .ok_or(AggregateError::CounterOverflow(stringify!($field)))?;
        };
    }
    into.hang_count = into
        .hang_count
        .checked_add(from.hang_count)
        .ok_or(AggregateError::CounterOverflow("hang_count"))?;
    into.deadlock_count = into
        .deadlock_count
        .checked_add(from.deadlock_count)
        .ok_or(AggregateError::CounterOverflow("deadlock_count"))?;
    add_u64!(total_calls);
    add_u64!(successful_calls);
    add_u64!(error_count);
    add_u64!(divergence_count);
    add_u64!(incomplete_worker_count);
    add_u64!(teardown_failure_count);
    into.hung_for_ms.extend(from.hung_for_ms.iter().copied());
    into.notes.extend(
        from.notes
            .iter()
            .map(|note| format!("agent {agent_name}: {note}")),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram(value_us: u64, count: u64) -> Histogram<u64> {
        let mut histogram = Histogram::<u64>::new_with_bounds(1, 3_600_000_000, 3).unwrap();
        histogram.record_n(value_us, count).unwrap();
        histogram
    }

    fn agent(name: &str, index: u32, latency_us: u64, count: u64) -> AgentEvidence {
        let histogram = histogram(latency_us, count);
        AgentEvidence {
            agent_name: name.to_owned(),
            shard: AgentShard {
                agent_name: name.to_owned(),
                index,
                agent_count: 2,
                concurrency: 1,
            },
            metrics: MetricsEvidence {
                latency: HistogramEvidence::from_histogram(&histogram).unwrap(),
                outcomes: OutcomeCounts {
                    success: count,
                    ..OutcomeCounts::default()
                },
            },
            per_tool: BTreeMap::new(),
            scenario_outcome: ScenarioOutcome {
                total_calls: count,
                successful_calls: count,
                ..ScenarioOutcome::default()
            },
            measurement_elapsed_ms: 1_000,
            start_delay_ms: u64::from(index) * 3,
            target_protocol_version: "2025-11-25".to_owned(),
            tool_inventory_hash: "sha256:inventory".to_owned(),
        }
    }

    #[test]
    fn merged_p99_is_not_the_average_of_agent_p99s() {
        let fast = agent("fast", 0, 1_000, 1_000);
        let slow = agent("slow", 1, 1_000_000, 1);
        let fast_p99 = fast
            .metrics
            .latency
            .to_histogram()
            .unwrap()
            .value_at_quantile(0.99);
        let slow_p99 = slow
            .metrics
            .latency
            .to_histogram()
            .unwrap()
            .value_at_quantile(0.99);
        let naive_average = (fast_p99 + slow_p99) / 2;

        let aggregate = aggregate_evidence(&[fast, slow]).unwrap();
        let merged_p99 = u64::try_from(aggregate.metrics.latency.p99.as_micros()).unwrap();
        assert!(
            merged_p99 < naive_average / 10,
            "exact merged p99 {merged_p99} must differ materially from naive {naive_average}"
        );
        assert_eq!(aggregate.metrics.latency.count, 1_001);
        assert_eq!(aggregate.metrics.throughput.total_requests, 1_001);
        assert!((aggregate.metrics.throughput.requests_per_sec - 1_001.0).abs() < 1e-9);
        assert_eq!(aggregate.start_skew_ms, 3);
    }

    #[test]
    fn aggregate_is_independent_of_arrival_order() {
        let east = agent("east", 0, 2_000, 10);
        let west = agent("west", 1, 4_000, 20);
        let forward = aggregate_evidence(&[east.clone(), west.clone()]).unwrap();
        let reverse = aggregate_evidence(&[west, east]).unwrap();

        assert_eq!(forward.agent_names, reverse.agent_names);
        assert_eq!(forward.global_concurrency, reverse.global_concurrency);
        assert_eq!(
            forward.metrics.throughput.total_requests,
            reverse.metrics.throughput.total_requests
        );
        assert_eq!(forward.metrics.latency.p99, reverse.metrics.latency.p99);
        assert_eq!(forward.scenario_outcome.total_calls, 30);
    }

    #[test]
    fn rejects_protocol_and_inventory_mismatch() {
        let east = agent("east", 0, 2_000, 1);
        let mut west = agent("west", 1, 2_000, 1);
        west.target_protocol_version = "2026-07-28".to_owned();
        assert!(matches!(
            aggregate_evidence(&[east.clone(), west]),
            Err(AggregateError::TargetProtocolMismatch { .. })
        ));

        let mut west = agent("west", 1, 2_000, 1);
        west.tool_inventory_hash = "sha256:different".to_owned();
        assert!(matches!(
            aggregate_evidence(&[east, west]),
            Err(AggregateError::ToolInventoryMismatch(_))
        ));
    }

    #[test]
    fn rejects_histogram_counter_tampering() {
        let mut east = agent("east", 0, 2_000, 1);
        let west = agent("west", 1, 2_000, 1);
        east.metrics.outcomes.success = 2;
        assert!(matches!(
            aggregate_evidence(&[east, west]),
            Err(AggregateError::LatencyOutcomeMismatch { .. })
        ));
    }
}
