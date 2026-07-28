//! Scenario `kind` → `Box<dyn Scenario>` dispatch.
//!
//! Pure function over the (untyped) TOML `params` blob — the single place
//! where a config's `scenario.kind` string is mapped to a concrete scenario,
//! plucking per-kind parameters via [`super::params`] / [`super::patterns`].

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use mcp_loadtest::ProtocolVersion;
use mcp_loadtest::scenario::Scenario;
use mcp_loadtest::scenario::cold_start::ColdStart;
use mcp_loadtest::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest::scenario::fuzzer::Fuzzer;
use mcp_loadtest::scenario::pattern::PatternScenario;
use mcp_loadtest::scenario::race_check::RaceCheck;
use mcp_loadtest::scenario::ramp::Ramp;
use mcp_loadtest::scenario::soak::Soak;
use mcp_loadtest::scenario::spike::Spike;
use mcp_loadtest::scenario::sustained::Sustained;
use mcp_loadtest::scenario::version_matrix::VersionMatrix;

use super::params::{
    f64_field, has_pattern_config, parse_breaking_point, parse_dur_field, parse_fuzz_payloads,
    required_dur_alias, required_str, required_u32_alias, u32_field, u64_field,
};
use super::patterns::parse_patterns;

/// Dispatch on the scenario `kind` string from a TOML config, plucking
/// per-kind parameters out of the (untyped) `params` blob.
pub(crate) fn build_scenario(kind: &str, params: &Value) -> Result<Box<dyn Scenario>> {
    match kind {
        "sustained" => {
            let concurrent = positive_u32("concurrent", u32_field(params, "concurrent", 10)?)?;
            let duration = positive_duration(
                "duration",
                parse_dur_field(params.get("duration"), Duration::from_secs(60))?,
            )?;
            if has_pattern_config(params) {
                let patterns = parse_patterns(params)?;
                return Ok(Box::new(PatternScenario::sustained(
                    concurrent, duration, patterns,
                )));
            }
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            Ok(Box::new(Sustained {
                concurrent,
                duration,
                tool,
                args,
            }))
        }
        "deadlock_probe" => {
            let concurrent = positive_u32("concurrent", u32_field(params, "concurrent", 20)?)?;
            let hang_threshold = positive_duration(
                "hang_threshold",
                parse_dur_field(params.get("hang_threshold"), Duration::from_secs(5))?,
            )?;
            let grace_period =
                parse_dur_field(params.get("grace_period"), Duration::from_secs(10))?;
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            Ok(Box::new(DeadlockProbe {
                concurrent,
                hang_threshold,
                grace_period,
                tool,
                args,
            }))
        }
        "cold_start" => {
            let iterations = positive_u32("iterations", u32_field(params, "iterations", 5)?)?;
            let warmup = params
                .get("warmup")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if warmup && iterations < 2 {
                return Err(anyhow!(
                    "scenario.iterations must be >= 2 when scenario.warmup = true; \
                     the warm-up iteration is excluded from measured evidence"
                ));
            }
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            Ok(Box::new(ColdStart {
                iterations,
                warmup,
                tool,
                args,
            }))
        }
        "spike" => {
            let baseline_concurrent = positive_u32(
                "baseline_concurrent",
                u32_field(params, "baseline_concurrent", 5)?,
            )?;
            let spike_concurrent = positive_u32(
                "spike_concurrent",
                u32_field(params, "spike_concurrent", 50)?,
            )?;
            let warmup = parse_dur_field(params.get("warmup"), Duration::from_secs(30))?;
            let spike_duration = positive_duration(
                "spike_duration",
                parse_dur_field(params.get("spike_duration"), Duration::from_secs(30))?,
            )?;
            let cooldown = parse_dur_field(params.get("cooldown"), Duration::from_secs(30))?;
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            Ok(Box::new(Spike {
                baseline_concurrent,
                spike_concurrent,
                warmup,
                spike_duration,
                cooldown,
                tool,
                args,
            }))
        }
        "ramp" => {
            let from_concurrent = positive_u32(
                "from_concurrent",
                required_u32_alias(params, &["from_concurrent", "ramp_from", "from"])?,
            )?;
            let to_concurrent = positive_u32(
                "to_concurrent",
                required_u32_alias(params, &["to_concurrent", "ramp_to", "to"])?,
            )?;
            if to_concurrent < from_concurrent {
                return Err(anyhow!(
                    "scenario.to_concurrent must be >= scenario.from_concurrent"
                ));
            }
            let step_duration = positive_duration(
                "step_duration",
                required_dur_alias(params, &["step_duration", "step", "ramp_step"])?,
            )?;
            let step_increment =
                positive_u32("step_increment", u32_field(params, "step_increment", 1)?)?;
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            let breaking_point = parse_breaking_point(params, step_duration)?;
            Ok(Box::new(Ramp {
                from_concurrent,
                to_concurrent,
                step_duration,
                step_increment,
                tool,
                args,
                breaking_point,
            }))
        }
        "soak" => {
            let defaults = Soak::default();
            let concurrent = positive_u32(
                "concurrent",
                u32_field(params, "concurrent", defaults.concurrent)?,
            )?;
            if concurrent != 1 {
                return Err(anyhow!(
                    "scenario.concurrent: soak currently supports exactly 1; \
                     use sustained for pooled concurrency"
                ));
            }
            let duration = positive_duration(
                "duration",
                parse_dur_field(params.get("duration"), defaults.duration)?,
            )?;
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            let sample_interval = positive_duration(
                "sample_interval",
                parse_dur_field(params.get("sample_interval"), defaults.sample_interval)?,
            )?;
            let latency_drift_ms_per_sec = f64_field(
                params,
                "latency_drift_ms_per_sec",
                defaults.latency_drift_ms_per_sec,
            )?;
            if !latency_drift_ms_per_sec.is_finite() || latency_drift_ms_per_sec < 0.0 {
                return Err(anyhow!(
                    "scenario.latency_drift_ms_per_sec must be finite and >= 0"
                ));
            }
            Ok(Box::new(Soak {
                concurrent,
                duration,
                tool,
                args,
                sample_interval,
                latency_drift_ms_per_sec,
            }))
        }
        "race_check" => {
            let concurrent = u32_field(params, "concurrent", 10)?;
            if concurrent < 2 {
                return Err(anyhow!(
                    "scenario.concurrent: race_check requires at least 2 synchronized calls"
                ));
            }
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            Ok(Box::new(RaceCheck {
                concurrent,
                tool,
                args,
            }))
        }
        "fuzzer" => {
            let iterations = positive_u32(
                "iterations",
                u32_field(params, "iterations", Fuzzer::default().iterations)?,
            )?;
            let seed = u64_field(params, "seed", Fuzzer::default().seed)?;
            let payloads = parse_fuzz_payloads(params)?;
            Ok(Box::new(Fuzzer {
                iterations,
                seed,
                payloads,
            }))
        }
        "pattern" => {
            let concurrent = positive_u32("concurrent", u32_field(params, "concurrent", 10)?)?;
            let duration = positive_duration(
                "duration",
                parse_dur_field(params.get("duration"), Duration::from_secs(60))?,
            )?;
            let patterns = parse_patterns(params)?;
            Ok(Box::new(PatternScenario::new(
                concurrent, duration, patterns,
            )))
        }
        "version_matrix" => {
            let calls_per_version = positive_u32(
                "calls_per_version",
                u32_field(params, "calls_per_version", 10)?,
            )?;
            let tool = required_str(params, "tool")?;
            let args = params.get("args").cloned().unwrap_or(json!({}));
            let versions = parse_versions(params)?;
            Ok(Box::new(VersionMatrix {
                versions,
                calls_per_version,
                tool,
                args,
            }))
        }
        other => Err(anyhow!("unknown scenario kind: {other}")),
    }
}

fn positive_u32(field: &str, value: u32) -> Result<u32> {
    if value == 0 {
        Err(anyhow!("scenario.{field} must be >= 1"))
    } else {
        Ok(value)
    }
}

fn positive_duration(field: &str, value: Duration) -> Result<Duration> {
    if value.is_zero() {
        Err(anyhow!("scenario.{field} must be > 0"))
    } else {
        Ok(value)
    }
}

/// Pluck the optional `versions` string array and parse each entry into a
/// [`ProtocolVersion`]. Empty / absent means "all supported revisions"
/// (resolved inside the scenario).
fn parse_versions(params: &Value) -> Result<Vec<ProtocolVersion>> {
    let Some(raw) = params.get("versions") else {
        return Ok(Vec::new());
    };
    let entries = raw
        .as_array()
        .ok_or_else(|| anyhow!("versions: expected an array of revision strings"))?;
    entries
        .iter()
        .map(|entry| {
            let s = entry
                .as_str()
                .ok_or_else(|| anyhow!("versions: entries must be strings, got {entry}"))?;
            ProtocolVersion::parse(s).ok_or_else(|| {
                let supported: Vec<&str> = ProtocolVersion::SUPPORTED
                    .iter()
                    .map(|v| v.as_str())
                    .collect();
                anyhow!(
                    "versions: unsupported revision `{s}` (expected one of: {})",
                    supported.join(", ")
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest::config::Config;

    #[test]
    fn build_scenario_supports_every_validated_kind() {
        let cases = [
            (
                "sustained",
                json!({"tool": "echo", "duration": "1s"}),
                "sustained",
            ),
            (
                "deadlock_probe",
                json!({"tool": "echo", "hang_threshold": "10ms", "grace_period": "10ms"}),
                "deadlock_probe",
            ),
            (
                "cold_start",
                json!({"iterations": 2, "tool": "echo"}),
                "cold_start",
            ),
            (
                "spike",
                json!({"tool": "echo", "warmup": "1ms", "spike_duration": "1ms", "cooldown": "1ms"}),
                "spike",
            ),
            (
                "ramp",
                json!({"from_concurrent": 1, "to_concurrent": 2, "step_duration": "1ms", "tool": "echo"}),
                "ramp",
            ),
            (
                "soak",
                json!({"tool": "echo", "duration": "1ms", "sample_interval": "1ms"}),
                "soak",
            ),
            ("race_check", json!({"tool": "echo"}), "race_check"),
            ("fuzzer", json!({"iterations": 1}), "fuzzer"),
            (
                "pattern",
                json!({
                    "duration": "1s",
                    "patterns": [{
                        "name": "read",
                        "steps": [{ "tool": "echo", "args": { "x": 1 } }]
                    }]
                }),
                "pattern",
            ),
            (
                "version_matrix",
                json!({"tool": "echo", "versions": ["2025-03-26", "2025-11-25"]}),
                "version_matrix",
            ),
        ];

        for (kind, params, expected_name) in cases {
            let scenario = build_scenario(kind, &params)
                .unwrap_or_else(|err| panic!("building {kind} failed: {err:#}"));
            assert_eq!(scenario.name(), expected_name);
        }
    }

    #[test]
    fn sustained_accepts_legacy_tool_call_array() {
        let scenario = build_scenario(
            "sustained",
            &json!({
                "duration": "1s",
                "tool_call": [
                    { "name": "echo", "args": { "x": 1 }, "weight": 1.0 },
                    { "name": "echo", "args": { "x": 2 }, "weight": 0.5 }
                ]
            }),
        )
        .expect("legacy tool_call sustained config should build");

        assert_eq!(scenario.name(), "sustained");
    }

    #[test]
    fn example_config_builds_runnable_scenario() {
        let cfg = Config::from_toml_str(&mcp_loadtest::config::example_config())
            .expect("example config should parse");
        let scenario = build_scenario(&cfg.scenario.kind, &cfg.scenario.params)
            .expect("example config scenario should build");

        assert_eq!(scenario.name(), "sustained");
    }

    #[test]
    fn cold_start_requires_tool() {
        // cold_start drives a real first call per fresh session, so `tool`
        // is required (same contract as deadlock_probe).
        let err = match build_scenario("cold_start", &json!({"iterations": 2})) {
            Ok(_) => panic!("cold_start without tool must error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("tool"), "got: {err:#}");
    }

    #[test]
    fn version_matrix_rejects_unsupported_revision() {
        let err = match build_scenario(
            "version_matrix",
            &json!({"tool": "echo", "versions": ["2019-01-01"]}),
        ) {
            Ok(_) => panic!("unsupported revision must error"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("unsupported revision"),
            "got: {err:#}"
        );
    }

    #[test]
    fn fuzzer_rejects_unknown_payload_label() {
        let err = match build_scenario("fuzzer", &json!({"payloads": ["NotARealPayload"]})) {
            Ok(_) => panic!("unknown payload must error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("unknown fuzzer payload"));
    }

    #[test]
    fn rejects_zero_or_impossible_scenario_bounds() {
        let cases = [
            (
                "sustained",
                json!({"concurrent": 0, "tool": "echo"}),
                "concurrent",
            ),
            (
                "sustained",
                json!({"duration": "0s", "tool": "echo"}),
                "duration",
            ),
            (
                "deadlock_probe",
                json!({"concurrent": 0, "tool": "echo"}),
                "concurrent",
            ),
            (
                "cold_start",
                json!({"iterations": 0, "tool": "echo"}),
                "iterations",
            ),
            (
                "cold_start",
                json!({"iterations": 1, "warmup": true, "tool": "echo"}),
                ">= 2",
            ),
            (
                "ramp",
                json!({
                    "from_concurrent": 2,
                    "to_concurrent": 1,
                    "step_duration": "1s",
                    "tool": "echo"
                }),
                "to_concurrent",
            ),
            (
                "soak",
                json!({
                    "concurrent": 1,
                    "duration": "1s",
                    "sample_interval": "0s",
                    "tool": "echo"
                }),
                "sample_interval",
            ),
            (
                "soak",
                json!({
                    "concurrent": 2,
                    "duration": "1s",
                    "sample_interval": "1s",
                    "tool": "echo"
                }),
                "exactly 1",
            ),
            (
                "race_check",
                json!({"concurrent": 1, "tool": "echo"}),
                "at least 2",
            ),
            (
                "pattern",
                json!({
                    "duration": "0s",
                    "patterns": [{"steps": [{"tool": "echo"}]}]
                }),
                "duration",
            ),
            ("fuzzer", json!({"iterations": 0}), "iterations"),
            (
                "version_matrix",
                json!({"calls_per_version": 0, "tool": "echo"}),
                "calls_per_version",
            ),
        ];

        for (kind, params, expected) in cases {
            let err = match build_scenario(kind, &params) {
                Ok(_) => panic!("{kind} accepted invalid params {params}"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains(expected),
                "{kind}: expected {expected:?} in {err:#}"
            );
        }
    }
}
