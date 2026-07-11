//! Generic TOML-param plucking + duration parsing for the scenario builder.
//!
//! These are the type-coercion helpers shared by [`super::builder`] and
//! [`super::patterns`]: pull a typed value out of the untyped `params` blob
//! produced by the TOML config, erroring with a human-readable
//! `scenario.<field>` message on the wrong shape.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use mcp_loadtest::analysis::breaking_point::BreakingPointConfig;
use mcp_loadtest::scenario::fuzzer::FuzzPayload;

/// Extract a required string field from a `params` blob, erroring with a
/// human-readable message if missing or the wrong shape.
pub(crate) fn required_str(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("scenario.{key} (string) is required"))
}

/// True when the config carries any of the pattern-style keys (`patterns`,
/// `tool_call`, `tool_calls`) — i.e. it should drive the weighted-pattern
/// engine instead of the single-`tool` path.
pub(crate) fn has_pattern_config(params: &Value) -> bool {
    params.get("patterns").is_some()
        || params.get("tool_call").is_some()
        || params.get("tool_calls").is_some()
}

/// Read an optional `u32` field, falling back to `default` when absent.
pub(crate) fn u32_field(params: &Value, key: &str, default: u32) -> Result<u32> {
    match params.get(key) {
        Some(v) => value_as_u32(v, key),
        None => Ok(default),
    }
}

/// Read a required `u32` field, accepting any of `keys` as aliases.
pub(crate) fn required_u32_alias(params: &Value, keys: &[&str]) -> Result<u32> {
    for key in keys {
        if let Some(v) = params.get(*key) {
            return value_as_u32(v, key);
        }
    }
    Err(anyhow!("scenario.{} (integer) is required", keys[0]))
}

fn value_as_u32(v: &Value, key: &str) -> Result<u32> {
    let n = v
        .as_u64()
        .ok_or_else(|| anyhow!("scenario.{key} must be a non-negative integer"))?;
    u32::try_from(n).with_context(|| format!("scenario.{key} exceeds u32::MAX"))
}

/// Read an optional `u64` field, falling back to `default` when absent.
pub(crate) fn u64_field(params: &Value, key: &str, default: u64) -> Result<u64> {
    match params.get(key) {
        Some(v) => v
            .as_u64()
            .ok_or_else(|| anyhow!("scenario.{key} must be a non-negative integer")),
        None => Ok(default),
    }
}

/// Read an optional `f64` field, falling back to `default` when absent.
pub(crate) fn f64_field(params: &Value, key: &str, default: f64) -> Result<f64> {
    match params.get(key) {
        Some(v) => v
            .as_f64()
            .ok_or_else(|| anyhow!("scenario.{key} must be a number")),
        None => Ok(default),
    }
}

/// Read a required duration field, accepting any of `keys` as aliases.
pub(crate) fn required_dur_alias(params: &Value, keys: &[&str]) -> Result<Duration> {
    for key in keys {
        if let Some(v) = params.get(*key) {
            return parse_dur_field(Some(v), Duration::ZERO)
                .with_context(|| format!("parsing scenario.{key}"));
        }
    }
    Err(anyhow!(
        "scenario.{} (duration string like '5s') is required",
        keys[0]
    ))
}

/// Parse the optional `breaking_point` sub-object inside a `ramp` config.
/// Absent → `None` (no breaking-point detection). `window_secs` defaults to
/// the ramp's `step_duration` so each step is judged over its own window.
pub(crate) fn parse_breaking_point(
    params: &Value,
    step_duration: Duration,
) -> Result<Option<BreakingPointConfig>> {
    let Some(v) = params.get("breaking_point") else {
        return Ok(None);
    };
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("scenario.breaking_point must be an object"))?;
    let max_p99_latency = parse_dur_field(
        obj.get("max_p99_latency")
            .or_else(|| obj.get("p99_latency")),
        Duration::MAX,
    )
    .context("parsing scenario.breaking_point.max_p99_latency")?;
    let max_error_rate = match obj.get("max_error_rate").or_else(|| obj.get("error_rate")) {
        Some(v) => v
            .as_f64()
            .ok_or_else(|| anyhow!("scenario.breaking_point.max_error_rate must be a number"))?,
        None => f64::INFINITY,
    };
    let window_secs = match obj.get("window_secs") {
        Some(v) => v
            .as_f64()
            .ok_or_else(|| anyhow!("scenario.breaking_point.window_secs must be a number"))?,
        None => step_duration.as_secs_f64(),
    };
    Ok(Some(BreakingPointConfig {
        max_p99_latency,
        max_error_rate,
        window_secs,
    }))
}

/// Parse the optional `payloads` array of a `fuzzer` config into the
/// corresponding [`FuzzPayload`] variants (case-insensitive label match).
pub(crate) fn parse_fuzz_payloads(params: &Value) -> Result<Vec<FuzzPayload>> {
    let Some(v) = params.get("payloads") else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("scenario.payloads must be an array of strings"))?;
    let all = FuzzPayload::all();
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let label = item
            .as_str()
            .ok_or_else(|| anyhow!("scenario.payloads entries must be strings"))?;
        let payload = all
            .iter()
            .copied()
            .find(|p| p.label() == label || p.label().eq_ignore_ascii_case(label))
            .ok_or_else(|| anyhow!("unknown fuzzer payload label: {label}"))?;
        out.push(payload);
    }
    Ok(out)
}

/// Parse a `params.<field>` slot as a humantime duration string, falling
/// back to `default` if the slot is absent and erroring on wrong types.
pub(crate) fn parse_dur_field(v: Option<&Value>, default: Duration) -> Result<Duration> {
    match v {
        Some(Value::String(s)) => parse_dur_str(s),
        None => Ok(default),
        _ => Err(anyhow!("duration must be a string like '60s'")),
    }
}

/// Parse a CLI / TOML duration string (`"60s"`, `"500ms"`, ...) into a
/// [`std::time::Duration`]. Wraps `humantime::parse_duration` with context.
pub fn parse_dur_str(s: &str) -> Result<Duration> {
    humantime::parse_duration(s).with_context(|| format!("parsing duration {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_str_errors_when_missing() {
        let err = required_str(&json!({}), "tool").unwrap_err();
        assert!(err.to_string().contains("scenario.tool"));
    }

    #[test]
    fn u32_field_falls_back_to_default() {
        assert_eq!(u32_field(&json!({}), "concurrent", 7).unwrap(), 7);
        assert_eq!(
            u32_field(&json!({"concurrent": 3}), "concurrent", 7).unwrap(),
            3
        );
    }

    #[test]
    fn value_as_u32_rejects_overflow_and_negatives() {
        assert!(value_as_u32(&json!(-1), "x").is_err());
        assert!(value_as_u32(&json!(u64::from(u32::MAX) + 1), "x").is_err());
    }

    #[test]
    fn parse_dur_field_defaults_and_type_checks() {
        assert_eq!(
            parse_dur_field(None, Duration::from_secs(5)).unwrap(),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_dur_field(Some(&json!("250ms")), Duration::ZERO).unwrap(),
            Duration::from_millis(250)
        );
        assert!(parse_dur_field(Some(&json!(5)), Duration::ZERO).is_err());
    }

    #[test]
    fn required_dur_alias_accepts_any_alias() {
        let p = json!({"ramp_step": "1s"});
        let d = required_dur_alias(&p, &["step_duration", "step", "ramp_step"]).unwrap();
        assert_eq!(d, Duration::from_secs(1));
    }
}
