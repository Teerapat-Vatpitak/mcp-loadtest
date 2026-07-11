//! On-disk JSONL schema for `mcp-trace/1` + default-on secret redaction
//! (ADR 0021 §2–3).
//!
//! A trace file is line-delimited JSON: the first line is a [`TraceHeader`],
//! every following line one [`TraceFrame`]. Frame bodies are stored as raw
//! strings — not embedded JSON — so the exact wire bytes survive round-trips
//! and future non-JSON payloads (T3.1 raw frames) stay representable without
//! a format bump.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::TraceError;

/// Format tag written to (and required in) every trace header.
pub const FORMAT_VERSION: &str = "mcp-trace/1";

/// Replacement value for redacted argument entries.
pub const REDACTED: &str = "[REDACTED]";

/// Key markers that trigger redaction, matched as substrings of the
/// ASCII-lowercased key (so `API_KEY`, `Authorization`, `refreshToken` all
/// match). See ADR 0021 §3.
const SENSITIVE_KEY_MARKERS: &[&str] = &[
    "secret",
    "token",
    "password",
    "api_key",
    "apikey",
    "authorization",
];

/// Direction of a recorded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Client → server (requests and notifications).
    #[serde(rename = "c2s")]
    ClientToServer,
    /// Server → client (responses).
    #[serde(rename = "s2c")]
    ServerToClient,
}

/// First line of every trace file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceHeader {
    /// Always [`FORMAT_VERSION`] for files this build writes.
    pub format: String,
    /// ULID of the recording run.
    pub run_id: String,
    /// Server under test — the stdio command line, or the URL.
    pub server: String,
    /// Wall-clock run start, ISO 8601 UTC (e.g. `2026-07-07T12:00:00Z`).
    pub started_at: String,
}

impl TraceHeader {
    /// Build a header stamped with the current [`FORMAT_VERSION`].
    pub fn new(run_id: &str, server: &str, started_at: &str) -> Self {
        Self {
            format: FORMAT_VERSION.to_owned(),
            run_id: run_id.to_owned(),
            server: server.to_owned(),
            started_at: started_at.to_owned(),
        }
    }
}

/// One recorded frame (one line of the file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFrame {
    /// Who sent it.
    pub dir: Direction,
    /// Milliseconds since run start (monotonic clock).
    pub elapsed_ms: u64,
    /// JSON-RPC method when parseable. Response frames carry the method of
    /// the request they answer (the wire response has no `method` field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The raw JSON-RPC object exactly as it crossed the wire (post-redaction
    /// for client→server frames when redaction is on — ADR 0021 §3).
    pub body: String,
}

/// Parse a whole trace document: header line first, frames after. Blank
/// lines are tolerated (skipped); anything else that fails to parse is an
/// error carrying its 1-based line number.
pub fn parse_trace(text: &str) -> Result<(TraceHeader, Vec<TraceFrame>), TraceError> {
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());

    let (header_no, header_line) = lines
        .next()
        .ok_or_else(|| TraceError::Format("empty trace file (missing header line)".into()))?;
    let header: TraceHeader = serde_json::from_str(header_line).map_err(|e| {
        TraceError::Format(format!("line {}: not a trace header: {e}", header_no + 1))
    })?;
    if header.format != FORMAT_VERSION {
        return Err(TraceError::UnsupportedFormat {
            got: header.format,
            expected: FORMAT_VERSION,
        });
    }

    let mut frames = Vec::new();
    for (no, line) in lines {
        let frame: TraceFrame = serde_json::from_str(line)
            .map_err(|e| TraceError::Format(format!("line {}: not a trace frame: {e}", no + 1)))?;
        frames.push(frame);
    }
    Ok((header, frames))
}

/// Extract the JSON-RPC `method` of a raw body, if it parses to an object
/// carrying one (responses don't). Used to label frames for recording
/// (`TraceWriter`) and to report the method of a diverging replay frame.
pub fn method_of(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct MethodProbe {
        method: Option<String>,
    }
    serde_json::from_str::<MethodProbe>(body)
        .ok()
        .and_then(|p| p.method)
}

/// Apply the default redaction policy (ADR 0021 §3) to a raw client→server
/// body: values under sensitive-looking keys inside `params.arguments` are
/// replaced with [`REDACTED`], recursively.
///
/// Returns `Cow::Borrowed` when nothing needed redaction — the common case —
/// so clean frames are written byte-for-byte. A body that doesn't parse, or
/// has no `params.arguments`, is returned unchanged.
pub fn redact_body(body: &str) -> Cow<'_, str> {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return Cow::Borrowed(body);
    };
    let Some(arguments) = value.pointer_mut("/params/arguments") else {
        return Cow::Borrowed(body);
    };
    if !redact_value(arguments) {
        return Cow::Borrowed(body);
    }
    match serde_json::to_string(&value) {
        Ok(redacted) => Cow::Owned(redacted),
        // Re-serializing a Value can't realistically fail; keep the original
        // rather than dropping the frame if it somehow does.
        Err(_) => Cow::Borrowed(body),
    }
}

/// Recursively replace values under sensitive keys. Returns `true` when
/// anything was changed.
fn redact_value(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *entry = Value::String(REDACTED.to_owned());
                    changed = true;
                } else {
                    changed |= redact_value(entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                changed |= redact_value(item);
            }
        }
        _ => {}
    }
    changed
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = TraceHeader::new("01RUN", "python mock.py", "2026-07-07T12:00:00Z");
        let line = serde_json::to_string(&h).unwrap();
        let back: TraceHeader = serde_json::from_str(&line).unwrap();
        assert_eq!(back.format, FORMAT_VERSION);
        assert_eq!(back.run_id, "01RUN");
        assert_eq!(back.server, "python mock.py");
        assert_eq!(back.started_at, "2026-07-07T12:00:00Z");
    }

    #[test]
    fn parse_trace_happy_path() {
        let text = concat!(
            r#"{"format":"mcp-trace/1","run_id":"01R","server":"python m.py","started_at":"2026-07-07T00:00:00Z"}"#,
            "\n",
            r#"{"dir":"c2s","elapsed_ms":1,"method":"initialize","body":"{}"}"#,
            "\n\n",
            r#"{"dir":"s2c","elapsed_ms":2,"method":"initialize","body":"{}"}"#,
            "\n",
        );
        let (header, frames) = parse_trace(text).unwrap();
        assert_eq!(header.run_id, "01R");
        assert_eq!(frames.len(), 2, "blank line must be skipped");
        assert_eq!(frames[0].dir, Direction::ClientToServer);
        assert_eq!(frames[1].dir, Direction::ServerToClient);
        assert_eq!(frames[0].method.as_deref(), Some("initialize"));
    }

    #[test]
    fn parse_trace_rejects_unknown_version() {
        let text = r#"{"format":"mcp-trace/9","run_id":"x","server":"y","started_at":"z"}"#;
        match parse_trace(text) {
            Err(TraceError::UnsupportedFormat { got, expected }) => {
                assert_eq!(got, "mcp-trace/9");
                assert_eq!(expected, FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }

    #[test]
    fn parse_trace_rejects_empty_and_garbage() {
        assert!(matches!(parse_trace(""), Err(TraceError::Format(_))));
        assert!(matches!(
            parse_trace("this is not json"),
            Err(TraceError::Format(_))
        ));

        let text = concat!(
            r#"{"format":"mcp-trace/1","run_id":"x","server":"y","started_at":"z"}"#,
            "\nnot-a-frame\n",
        );
        match parse_trace(text) {
            Err(TraceError::Format(msg)) => {
                assert!(msg.contains("line 2"), "line number in error, got: {msg}")
            }
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    #[test]
    fn method_of_parses_requests_and_ignores_responses() {
        assert_eq!(
            method_of(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#).as_deref(),
            Some("tools/call")
        );
        assert_eq!(method_of(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), None);
        assert_eq!(method_of("not json"), None);
    }

    #[test]
    fn redact_replaces_sensitive_keys_including_nested() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"hi","api_key":"sekrit","auth":{"refresh_token":"t0k3n"}}}}"#;
        let redacted = redact_body(body);
        assert!(!redacted.contains("sekrit"), "got: {redacted}");
        assert!(!redacted.contains("t0k3n"), "got: {redacted}");
        assert!(redacted.contains(REDACTED));
        assert!(
            redacted.contains(r#""msg":"hi""#),
            "non-sensitive keys untouched, got: {redacted}"
        );
    }

    #[test]
    fn redact_is_case_insensitive_on_keys() {
        let body = r#"{"method":"tools/call","params":{"arguments":{"Authorization":"Bearer x","API_KEY":"y","ApiKey":"z"}}}"#;
        let redacted = redact_body(body);
        assert!(!redacted.contains("Bearer x"));
        assert!(!redacted.contains(r#":"y""#));
        assert!(!redacted.contains(r#":"z""#));
    }

    #[test]
    fn redact_leaves_clean_body_borrowed() {
        let body = r#"{"method":"tools/call","params":{"arguments":{"msg":"hello"}}}"#;
        match redact_body(body) {
            Cow::Borrowed(b) => assert_eq!(b, body),
            Cow::Owned(o) => panic!("clean body must not be re-serialized, got: {o}"),
        }
    }

    #[test]
    fn redact_ignores_unparseable_and_argumentless_bodies() {
        assert!(matches!(redact_body("raw \x01 bytes"), Cow::Borrowed(_)));
        assert!(matches!(
            redact_body(r#"{"jsonrpc":"2.0","id":1,"result":{"password":"p"}}"#),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn redact_walks_arrays_inside_arguments() {
        let body = r#"{"method":"tools/call","params":{"arguments":{"items":[{"secret":"s1"},{"plain":1}]}}}"#;
        let redacted = redact_body(body);
        assert!(!redacted.contains("s1"));
        assert!(redacted.contains(r#""plain":1"#));
    }
}
