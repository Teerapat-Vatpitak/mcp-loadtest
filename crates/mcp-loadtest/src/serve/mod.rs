//! Self-hosted MCP server (`mcp-loadtest serve --mcp`) — DESIGN §21.2.
//!
//! Exposes the load tester's primary verbs as MCP tools so AI agents (Claude
//! Code, Cursor, etc.) can drive load tests directly. Speaks newline-delimited
//! JSON-RPC 2.0 over stdin/stdout — the inverse of `protocol::transport::stdio`.
//! Implements `initialize`, `tools/list`, `tools/call`, and
//! `notifications/initialized`; anything else gets `-32601 method not found`.
//! Loop exits cleanly on stdin EOF.

pub mod tools;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::protocol::jsonrpc::JSONRPC_VERSION;
use crate::protocol::mcp::PROTOCOL_VERSION;

/// JSON-RPC error code for "method not found" (per spec §5.1).
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code for "invalid params".
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code for "internal error" — used when a tool handler fails.
const INTERNAL_ERROR: i64 = -32603;
/// JSON-RPC error code for "parse error" (invalid JSON line).
const PARSE_ERROR: i64 = -32700;

/// Hard cap on a single inbound JSON-RPC line. JSON-RPC has no spec limit, but
/// real MCP messages are < 1 MB; 16 MB leaves slack for pathological inputs
/// (large `tools/call` argument blobs, embedded base64, ...) while preventing
/// a malicious client from OOM-ing the server with one unbounded line. On
/// overflow we emit a JSON-RPC `-32700 parse error` and break the loop rather
/// than truncating silently or hanging on a multi-GB read.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Self-hosted MCP server. Construct with [`McpServer::new`], then call
/// [`McpServer::run_stdio`] to take over the current process's stdio loop.
#[derive(Default)]
pub struct McpServer {
    // Stateless for M7 — handlers spawn their own child runs. Future
    // versions can stash an in-memory result cache here.
}

impl McpServer {
    /// Build a fresh server with no state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take over the current process's stdin/stdout and loop until EOF.
    ///
    /// Reads JSON-RPC requests one line at a time, dispatches synchronously
    /// (handlers themselves can be async), and writes one response line per
    /// request. Notifications (id absent) get no response. Each line is
    /// bounded at `MAX_LINE_BYTES` to prevent an unbounded line from OOM-ing
    /// the server.
    pub async fn run_stdio(self) -> std::io::Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            // Bound the line read at MAX_LINE_BYTES (16 MB). `read_line`
            // itself has no cap; we use `read_bounded_line` which streams via
            // fill_buf/consume so a multi-GB unbounded line is rejected
            // instead of silently buffered.
            match read_bounded_line(&mut reader, &mut line).await {
                Ok(BoundedRead::Line) => {}
                Ok(BoundedRead::Eof) => break,
                Err(BoundedReadError::Overflow) => {
                    // Hostile or buggy client: reply with a parse error and
                    // stop reading from this transport — any continuation
                    // bytes would parse as a fresh (invalid) frame.
                    let resp = error_response(
                        Value::Null,
                        PARSE_ERROR,
                        &format!(
                            "parse error: inbound line exceeds {MAX_LINE_BYTES} bytes; closing"
                        ),
                    );
                    let serialized = serde_json::to_string(&resp).unwrap_or_default();
                    stdout.write_all(serialized.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    break;
                }
                Err(BoundedReadError::Io(e)) => return Err(e),
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(response) = handle_line(trimmed).await {
                let serialized = serde_json::to_string(&response).unwrap_or_else(|_| {
                    // We constructed the response ourselves; serialization
                    // can't fail for well-formed JSON values. If it somehow
                    // does, fall back to an empty error envelope.
                    String::from(
                        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal serialization failed"}}"#,
                    )
                });
                stdout.write_all(serialized.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }
}

/// Process one inbound JSON-RPC line. Returns `Some(response)` for requests,
/// `None` for notifications (which get no reply).
async fn handle_line(line: &str) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            ));
        }
    };

    // Notifications have no `id`; requests do. The spec allows id to be a
    // string, number, or null — we forward it verbatim. We still process
    // `notifications/initialized` as a sanity check, but there's nothing to
    // send back. Unknown notifications are silently ignored per JSON-RPC
    // spec.
    let id = parsed.get("id").cloned()?;
    let method = parsed
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = parsed.get("params").cloned().unwrap_or(json!({}));

    match method.as_str() {
        "initialize" => Some(success_response(id, initialize_result())),
        "tools/list" => Some(success_response(id, json!({ "tools": tools::tool_defs() }))),
        "tools/call" => Some(handle_tools_call(id, &params).await),
        // Resources / prompts aren't exposed by M7 — declare them empty
        // rather than error so generic MCP inspectors don't choke.
        "resources/list" => Some(success_response(id, json!({ "resources": [] }))),
        "prompts/list" => Some(success_response(id, json!({ "prompts": [] }))),
        // No-op for `notifications/*` that got an id anyway, and an
        // affirmative ping for completeness.
        "ping" => Some(success_response(id, json!({}))),
        other => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            &format!("method not found: {other}"),
        )),
    }
}

/// Build the `initialize` result envelope.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "mcp-loadtest",
            "version": crate::VERSION,
        }
    })
}

/// Run `tools/call` against the registry and shape the result the way MCP
/// clients expect (`content: [{ type: "text", ... }]`).
async fn handle_tools_call(id: Value, params: &Value) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => {
            return error_response(id, INVALID_PARAMS, "tools/call requires `name`");
        }
    };
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    match tools::dispatch(name, &arguments).await {
        Ok(value) => {
            // MCP `tools/call` results carry `content`, not raw JSON.
            // Stuff the structured result into a text block; clients that
            // want machine-readable output can json-parse the text.
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            success_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                    "structuredContent": value,
                }),
            )
        }
        Err(e) => {
            let code = match e {
                tools::ToolError::InvalidArgs(_) => INVALID_PARAMS,
                tools::ToolError::Run(_) | tools::ToolError::Io(_) => INTERNAL_ERROR,
            };
            error_response(id, code, &e.to_string())
        }
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

/// Outcome of a successful bounded line read.
enum BoundedRead {
    /// One full line was read into the caller's buffer.
    Line,
    /// EOF before any bytes were read.
    Eof,
}

/// Error returned by [`read_bounded_line`].
enum BoundedReadError {
    /// Underlying I/O error from the reader.
    Io(std::io::Error),
    /// The line would exceed `MAX_LINE_BYTES` — caller should respond with a
    /// parse error and close the transport.
    Overflow,
}

impl From<std::io::Error> for BoundedReadError {
    fn from(e: std::io::Error) -> Self {
        BoundedReadError::Io(e)
    }
}

/// Read a single newline-terminated line into `out`, but reject any input
/// whose length would exceed `MAX_LINE_BYTES`. The cap is enforced by
/// streaming via `fill_buf`/`consume` rather than a single `read_line` call
/// so we never buffer more than the cap.
async fn read_bounded_line<R>(
    reader: &mut R,
    out: &mut String,
) -> Result<BoundedRead, BoundedReadError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut total = 0usize;
    loop {
        // Borrow `reader` only long enough to extract owned bytes; release the
        // borrow before calling `consume`.
        let (chunk, found_newline) = {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                if total == 0 {
                    return Ok(BoundedRead::Eof);
                }
                return Ok(BoundedRead::Line);
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(pos) => (buf[..=pos].to_vec(), true),
                None => (buf.to_vec(), false),
            }
        };
        if total + chunk.len() > MAX_LINE_BYTES {
            return Err(BoundedReadError::Overflow);
        }
        out.push_str(&String::from_utf8_lossy(&chunk));
        let n = chunk.len();
        reader.consume(n);
        total += n;
        if found_newline {
            return Ok(BoundedRead::Line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = handle_line(line).await.expect("initialize must respond");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "mcp-loadtest");
    }

    #[tokio::test]
    async fn tools_list_advertises_three_tools() {
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = handle_line(line).await.expect("tools/list must respond");
        let tools = resp["result"]["tools"]
            .as_array()
            .expect("tools must be an array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"deadlock_probe"));
        assert!(names.contains(&"sustained_load"));
        assert!(names.contains(&"compare_runs"));
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let line = r#"{"jsonrpc":"2.0","id":3,"method":"floof"}"#;
        let resp = handle_line(line).await.expect("must respond");
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notification_returns_no_response() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        assert!(handle_line(line).await.is_none());
    }

    #[tokio::test]
    async fn parse_error_returns_parse_code() {
        let resp = handle_line("not json").await.expect("must respond");
        assert_eq!(resp["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_invalid_params() {
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#;
        let resp = handle_line(line).await.expect("must respond");
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn tools_call_missing_name_returns_invalid_params() {
        let line = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{}}"#;
        let resp = handle_line(line).await.expect("must respond");
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    /// S-H2 regression: a 17 MB single line (no newline) must abort the read
    /// with `Overflow` instead of OOM-ing the process.
    #[tokio::test]
    async fn read_bounded_line_rejects_oversized_input() {
        // 17 MB > MAX_LINE_BYTES (16 MB), and crucially no newline so the
        // bounded reader walks the full cap before bailing.
        let payload = vec![b'a'; 17 * 1024 * 1024];
        let cursor = std::io::Cursor::new(payload);
        // Cursor<Vec<u8>>: AsyncRead — wrap to get AsyncBufRead.
        let mut reader = BufReader::new(cursor);
        let mut out = String::new();
        let result = read_bounded_line(&mut reader, &mut out).await;
        assert!(
            matches!(result, Err(BoundedReadError::Overflow)),
            "expected Overflow, got Ok or Io error"
        );
        // The partial buffer must not have ballooned past the cap.
        assert!(
            out.len() <= MAX_LINE_BYTES,
            "out buffer exceeded MAX_LINE_BYTES: {}",
            out.len()
        );
    }
}
