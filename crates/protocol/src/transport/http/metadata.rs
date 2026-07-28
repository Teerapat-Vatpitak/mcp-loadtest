//! MCP 2026-07-28 HTTP request metadata (SEP-2243).

use std::collections::{HashMap, HashSet};

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

use crate::transport::TransportError;

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_SCHEMA_NODES: usize = 4_096;

#[derive(Deserialize)]
struct WireRequest {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone, Debug)]
enum ParamKind {
    String,
    Integer,
    Boolean,
}

#[derive(Clone, Debug)]
struct ParamHeader {
    path: Vec<String>,
    name: String,
    kind: ParamKind,
}

#[derive(Default)]
pub(super) struct ToolHeaderRegistry {
    tools: HashMap<String, Vec<ParamHeader>>,
}

pub(super) struct PreparedHeaders {
    pub(super) method: String,
    pub(super) headers: HeaderMap,
}

impl ToolHeaderRegistry {
    pub(super) fn prepare(&self, body: &str) -> Result<PreparedHeaders, TransportError> {
        let request: WireRequest = serde_json::from_str(body).map_err(|error| {
            TransportError::Other(format!(
                "cannot derive required MCP HTTP headers from JSON-RPC request: {error}"
            ))
        })?;
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "Mcp-Method", &request.method)?;

        let name = match request.method.as_str() {
            "tools/call" | "prompts/get" => request.params.get("name").and_then(Value::as_str),
            "resources/read" => request.params.get("uri").and_then(Value::as_str),
            _ => None,
        };
        if matches!(
            request.method.as_str(),
            "tools/call" | "prompts/get" | "resources/read"
        ) {
            let name = name.ok_or_else(|| {
                TransportError::Other(format!(
                    "{} request is missing the string field mirrored by Mcp-Name",
                    request.method
                ))
            })?;
            insert_header(&mut headers, "Mcp-Name", &encode_value(name))?;
        }

        if request.method == "tools/call"
            && let Some(tool) = request.params.get("name").and_then(Value::as_str)
            && let Some(specs) = self.tools.get(tool)
        {
            let arguments = request.params.get("arguments").unwrap_or(&Value::Null);
            for spec in specs {
                let Some(value) = value_at_path(arguments, &spec.path) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                let encoded = encode_param_value(value, &spec.kind, tool, &spec.path)?;
                insert_header(&mut headers, &spec.name, &encode_value(&encoded))?;
            }
        }

        Ok(PreparedHeaders {
            method: request.method,
            headers,
        })
    }

    /// Cache valid `x-mcp-header` annotations and remove invalid tool
    /// definitions from `tools/list`, as SEP-2243 requires of HTTP clients.
    pub(super) fn process_tools_list(&mut self, body: String) -> Result<String, TransportError> {
        let mut envelope: Value = serde_json::from_str(&body).map_err(|error| {
            TransportError::Other(format!("invalid tools/list response JSON: {error}"))
        })?;
        let Some(tools) = envelope
            .get_mut("result")
            .and_then(|result| result.get_mut("tools"))
            .and_then(Value::as_array_mut)
        else {
            return Ok(body);
        };

        let mut accepted = Vec::with_capacity(tools.len());
        for tool in tools.drain(..) {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                accepted.push(tool);
                continue;
            };
            let schema = tool.get("inputSchema").unwrap_or(&Value::Null);
            match collect_tool_headers(schema) {
                Ok(specs) => {
                    self.tools.insert(name.to_owned(), specs);
                    accepted.push(tool);
                }
                Err(reason) => {
                    self.tools.remove(name);
                    tracing::warn!(
                        tool = name,
                        reason,
                        "excluding tool with invalid x-mcp-header annotation"
                    );
                }
            }
        }
        *tools = accepted;
        serde_json::to_string(&envelope).map_err(|error| {
            TransportError::Other(format!("cannot rewrite tools/list response: {error}"))
        })
    }
}

fn collect_tool_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut found = Vec::new();
    let mut nodes = 0usize;
    walk_schema(
        schema,
        &mut Vec::new(),
        true,
        false,
        0,
        &mut nodes,
        &mut found,
    )?;
    let mut names = HashSet::new();
    for header in &found {
        if !names.insert(header.name.to_ascii_lowercase()) {
            return Err(format!(
                "duplicate case-insensitive x-mcp-header `{}`",
                header.name.trim_start_matches("Mcp-Param-")
            ));
        }
    }
    Ok(found)
}

fn walk_schema(
    node: &Value,
    path: &mut Vec<String>,
    statically_reachable: bool,
    is_property: bool,
    depth: usize,
    nodes: &mut usize,
    found: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    *nodes += 1;
    if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
        return Err("x-mcp-header schema traversal limit exceeded".into());
    }
    if let Some(array) = node.as_array() {
        for child in array {
            walk_schema(child, path, false, false, depth + 1, nodes, found)?;
        }
        return Ok(());
    }
    let Some(object) = node.as_object() else {
        return Ok(());
    };
    if let Some(annotation) = object.get("x-mcp-header") {
        if !statically_reachable || !is_property {
            return Err("x-mcp-header is not on a statically reachable property".into());
        }
        let suffix = annotation
            .as_str()
            .ok_or_else(|| "x-mcp-header must be a string".to_string())?;
        if !is_http_token(suffix) {
            return Err(format!("invalid x-mcp-header name `{suffix}`"));
        }
        let kind = match object.get("type").and_then(Value::as_str) {
            Some("string") => ParamKind::String,
            Some("integer") => ParamKind::Integer,
            Some("boolean") => ParamKind::Boolean,
            _ => {
                return Err(format!(
                    "x-mcp-header `{suffix}` is only valid on string, integer, or boolean properties"
                ));
            }
        };
        found.push(ParamHeader {
            path: path.clone(),
            name: format!("Mcp-Param-{suffix}"),
            kind,
        });
    }

    for (key, value) in object {
        if key == "properties" {
            if let Some(properties) = value.as_object() {
                for (property, child) in properties {
                    path.push(property.clone());
                    walk_schema(
                        child,
                        path,
                        statically_reachable,
                        statically_reachable,
                        depth + 1,
                        nodes,
                        found,
                    )?;
                    path.pop();
                }
            }
        } else if key != "x-mcp-header" {
            walk_schema(value, path, false, false, depth + 1, nodes, found)?;
        }
    }
    Ok(())
}

fn value_at_path<'a>(mut value: &'a Value, path: &[String]) -> Option<&'a Value> {
    for segment in path {
        value = value.get(segment)?;
    }
    Some(value)
}

fn encode_param_value(
    value: &Value,
    kind: &ParamKind,
    tool: &str,
    path: &[String],
) -> Result<String, TransportError> {
    let invalid = || {
        TransportError::Other(format!(
            "tools/call argument `{}` for tool `{tool}` does not match its x-mcp-header primitive type",
            path.join(".")
        ))
    };
    match kind {
        ParamKind::String => value.as_str().map(str::to_owned).ok_or_else(invalid),
        ParamKind::Boolean => value.as_bool().map(|v| v.to_string()).ok_or_else(invalid),
        ParamKind::Integer => {
            let number = value.as_i64().ok_or_else(invalid)?;
            if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&number) {
                return Err(TransportError::Other(format!(
                    "tools/call argument `{}` for tool `{tool}` is outside the x-mcp-header safe integer range",
                    path.join(".")
                )));
            }
            Ok(number.to_string())
        }
    }
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), TransportError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| TransportError::Other("invalid derived MCP header name".into()))?;
    let value = HeaderValue::from_str(value)
        .map_err(|_| TransportError::Other("invalid derived MCP header value".into()))?;
    headers.insert(name, value);
    Ok(())
}

fn encode_value(value: &str) -> String {
    let safe = value.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && value.trim() == value
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    if safe {
        value.to_owned()
    } else {
        let encoded = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
        format!("=?base64?{encoded}?=")
    }
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_and_sentinel_values_use_base64_sentinel() {
        assert_eq!(encode_value("plain ASCII"), "plain ASCII");
        assert_eq!(
            encode_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn rejects_annotations_outside_static_property_paths() {
        let schema = serde_json::json!({
            "type": "object",
            "oneOf": [{
                "properties": {
                    "tenant": {"type": "string", "x-mcp-header": "Tenant"}
                }
            }]
        });
        let error = collect_tool_headers(&schema).expect_err("oneOf path must be rejected");
        assert!(error.contains("statically reachable"));
    }

    #[test]
    fn nested_properties_are_collected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "object",
                    "properties": {
                        "tenant": {"type": "string", "x-mcp-header": "Tenant"}
                    }
                }
            }
        });
        let headers = collect_tool_headers(&schema).expect("valid nested property");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].path, ["context", "tenant"]);
        assert_eq!(headers[0].name, "Mcp-Param-Tenant");
    }

    #[test]
    fn duplicate_names_are_case_insensitive() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "x-mcp-header": "Tenant"},
                "b": {"type": "string", "x-mcp-header": "tenant"}
            }
        });
        assert!(collect_tool_headers(&schema).is_err());
    }
}
