//! Weighted multi-step pattern-config parsing.
//!
//! Turns the `patterns` / `tool_call` / single-`tool` config shapes into the
//! [`Pattern`] model the pattern engine drives. Legacy `tool_call` arrays are
//! still accepted so older configs keep working (see DESIGN.md §11).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use mcp_loadtest::scenario::pattern::{ErrorBehavior, Pattern, PatternStep};

use super::params::parse_dur_field;

/// Resolve the pattern set from a config blob: explicit `patterns`, legacy
/// `tool_call[s]`, or a single `tool` + `args` collapsed into one pattern.
pub(crate) fn parse_patterns(params: &Value) -> Result<Vec<Pattern>> {
    if let Some(v) = params.get("patterns") {
        return parse_pattern_array(v);
    }
    if let Some(v) = params.get("tool_call").or_else(|| params.get("tool_calls")) {
        return parse_tool_call_array(v);
    }
    if let Some(tool) = params.get("tool").and_then(Value::as_str) {
        let args = params.get("args").cloned().unwrap_or(json!({}));
        return Ok(vec![Pattern::single_call(tool, args)]);
    }
    Err(anyhow!(
        "scenario.patterns is required (or provide scenario.tool + scenario.args for a single-step pattern)"
    ))
}

fn parse_pattern_array(v: &Value) -> Result<Vec<Pattern>> {
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("scenario.patterns must be an array"))?;
    if arr.is_empty() {
        return Err(anyhow!("scenario.patterns must not be empty"));
    }

    let mut patterns = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| anyhow!("scenario.patterns[{idx}] must be an object"))?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("pattern-{}", idx + 1));
        let weight = match obj.get("weight") {
            Some(v) => v
                .as_f64()
                .ok_or_else(|| anyhow!("scenario.patterns[{idx}].weight must be a number"))?,
            None => 1.0,
        };
        let think_time = parse_dur_field(obj.get("think_time"), Duration::ZERO)
            .with_context(|| format!("parsing scenario.patterns[{idx}].think_time"))?;
        let on_step_error = parse_error_behavior(obj.get("on_step_error"), idx)?;
        let steps = match obj.get("steps") {
            Some(steps) => parse_steps(steps, &format!("scenario.patterns[{idx}].steps"))?,
            None => {
                if obj.get("tool").is_some() {
                    vec![parse_step(
                        item,
                        &format!("scenario.patterns[{idx}]"),
                        "tool",
                    )?]
                } else {
                    return Err(anyhow!("scenario.patterns[{idx}].steps is required"));
                }
            }
        };
        patterns.push(Pattern {
            name,
            weight,
            think_time,
            on_step_error,
            steps,
        });
    }
    require_selectable_pattern(&patterns)?;
    Ok(patterns)
}

fn parse_tool_call_array(v: &Value) -> Result<Vec<Pattern>> {
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("scenario.tool_call must be an array"))?;
    if arr.is_empty() {
        return Err(anyhow!("scenario.tool_call must not be empty"));
    }

    let mut patterns = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let step = parse_step(item, &format!("scenario.tool_call[{idx}]"), "name")?;
        let weight = item.get("weight").and_then(Value::as_f64).unwrap_or(1.0);
        patterns.push(Pattern {
            name: format!("tool:{}", step.tool),
            weight,
            think_time: Duration::ZERO,
            on_step_error: ErrorBehavior::Continue,
            steps: vec![step],
        });
    }
    require_selectable_pattern(&patterns)?;
    Ok(patterns)
}

fn require_selectable_pattern(patterns: &[Pattern]) -> Result<()> {
    if patterns
        .iter()
        .any(|pattern| pattern.weight.is_finite() && pattern.weight > 0.0)
    {
        Ok(())
    } else {
        Err(anyhow!(
            "scenario.patterns must contain at least one finite positive weight"
        ))
    }
}

fn parse_steps(v: &Value, path: &str) -> Result<Vec<PatternStep>> {
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("{path} must be an array"))?;
    if arr.is_empty() {
        return Err(anyhow!("{path} must not be empty"));
    }
    arr.iter()
        .enumerate()
        .map(|(idx, item)| parse_step(item, &format!("{path}[{idx}]"), "tool"))
        .collect()
}

fn parse_step(v: &Value, path: &str, preferred_tool_key: &str) -> Result<PatternStep> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("{path} must be an object"))?;
    let fallback_key = if preferred_tool_key == "tool" {
        "name"
    } else {
        "tool"
    };
    let tool = obj
        .get(preferred_tool_key)
        .or_else(|| obj.get(fallback_key))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{path}.{preferred_tool_key} (string) is required"))?;
    let args = obj.get("args").cloned().unwrap_or(json!({}));
    Ok(PatternStep { tool, args })
}

fn parse_error_behavior(v: Option<&Value>, idx: usize) -> Result<ErrorBehavior> {
    match v {
        None => Ok(ErrorBehavior::Continue),
        Some(Value::String(s)) if s == "continue" => Ok(ErrorBehavior::Continue),
        Some(Value::String(s)) if s == "abort" => Ok(ErrorBehavior::Abort),
        Some(Value::String(s)) => Err(anyhow!(
            "scenario.patterns[{idx}].on_step_error must be `continue` or `abort`, got `{s}`"
        )),
        Some(_) => Err(anyhow!(
            "scenario.patterns[{idx}].on_step_error must be a string"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_tool_collapses_to_one_pattern() {
        let p = parse_patterns(&json!({ "tool": "echo", "args": { "x": 1 } })).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].steps.len(), 1);
        assert_eq!(p[0].steps[0].tool, "echo");
    }

    #[test]
    fn legacy_tool_call_array_maps_each_entry() {
        let p = parse_patterns(&json!({
            "tool_call": [
                { "name": "a", "weight": 2.0 },
                { "name": "b" }
            ]
        }))
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].weight, 2.0);
        assert_eq!(p[1].name, "tool:b");
    }

    #[test]
    fn empty_patterns_array_is_rejected() {
        assert!(parse_patterns(&json!({ "patterns": [] })).is_err());
    }

    #[test]
    fn invalid_on_step_error_is_rejected() {
        let err = parse_patterns(&json!({
            "patterns": [{ "name": "p", "on_step_error": "explode",
                           "steps": [{ "tool": "echo" }] }]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("on_step_error"));
    }

    #[test]
    fn all_non_positive_weights_are_rejected() {
        let err = parse_patterns(&json!({
            "patterns": [
                { "weight": 0.0, "steps": [{ "tool": "echo" }] },
                { "weight": -1.0, "steps": [{ "tool": "echo" }] }
            ]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("positive weight"), "got {err:#}");
    }
}
