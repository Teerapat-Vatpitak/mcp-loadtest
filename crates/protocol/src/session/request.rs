//! The request/response surface of [`Session`]: `list_tools`, `call_tool`,
//! the strict-schema setters, `raw_send`, and the private `request`/`notify`
//! JSON-RPC plumbing.
//!
//! Split out of `session/mod.rs` to keep that file within the size
//! convention. `request`/`notify` are `pub(super)` so the sibling `lifecycle`
//! (initialize) and `connection` (discover) modules can drive the wire.

use std::collections::HashMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Session, SessionError, connection, strict};
use crate::jsonrpc::{
    JSONRPC_VERSION, OutgoingNotification, OutgoingRequest, ResponseEnvelope, ResponsePayload,
};
use crate::mcp::{CallToolParams, CallToolResult, ListToolsResult, Tool};

impl Session {
    /// Call `tools/list` and return the server's tool registry.
    pub async fn list_tools(&mut self) -> Result<Vec<Tool>, SessionError> {
        let result: ListToolsResult = self.request("tools/list", &serde_json::json!({})).await?;
        Ok(result.tools)
    }

    /// Turn on strict args validation for subsequent [`Session::call_tool`]
    /// calls. `schemas` maps each tool's name to its advertised
    /// `inputSchema`. Call once at run start (the caller already has the
    /// `tools/list` result in hand). A tool absent from `schemas` is never
    /// validated — a server that doesn't advertise a schema is not failed
    /// on that ground (forward-compatible, ADR 0005).
    pub fn set_strict_tool_schemas(&mut self, schemas: HashMap<String, Value>) {
        self.tool_schemas = Some(schemas);
    }

    /// Turn on result-side validation for subsequent [`Session::call_tool`]
    /// calls: each successful result's `structuredContent` is checked
    /// against the tool's advertised `outputSchema`. A tool absent from
    /// `schemas` is never result-validated (ADR 0005). Under the current
    /// policy mismatches — including a missing `structuredContent` — are
    /// **non-gating**: they warn and never fail the call (DESIGN §13.1).
    pub fn set_strict_tool_output_schemas(&mut self, schemas: HashMap<String, Value>) {
        self.tool_output_schemas = Some(schemas);
    }

    /// Call `tools/call` with the given tool name and arguments.
    ///
    /// Both `name` and `arguments` are borrowed — callers driving in a hot
    /// loop (sustained / ramp / spike / soak / pattern scenarios) can pass
    /// `&self.tool` and `&self.args` directly with zero per-iteration clone.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: &Value,
    ) -> Result<CallToolResult, SessionError> {
        // Opt-in strict args validation. When `tool_schemas` is `None`
        // (default) this is a single branch and the hot path is unchanged.
        if let Some(schemas) = &self.tool_schemas {
            strict::check_args(schemas, name, arguments)?;
        }

        let params = CallToolParams { name, arguments };
        let result: CallToolResult = self.request("tools/call", &params).await?;

        // Opt-in result-side validation (DESIGN §13.1 item 2). Same
        // single-`Option`-branch hot-path discipline as args (ADR 0006).
        // Non-gating under the current policy: `classify_schema_violation`
        // maps `ToolCallResult` to `Warn`, so mismatches log and never
        // alter or fail the returned result.
        if let Some(schemas) = &self.tool_output_schemas {
            strict::check_result(schemas, name, &result)?;
        }
        Ok(result)
    }

    /// Send **raw, unframed bytes** straight to the transport, bypassing
    /// JSON-RPC serialization — the fuzzer's escape hatch for
    /// malformed-frame payloads ([`Transport::raw_send`]). No response is
    /// read: after a raw send the wire may be desynced, so callers must
    /// treat the session as poisoned and respawn before the next typed call.
    ///
    /// [`Transport::raw_send`]: crate::transport::Transport::raw_send
    pub async fn raw_send(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        self.transport.raw_send(bytes).await?;
        Ok(())
    }

    pub(super) async fn request<P, R>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, SessionError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // Borrowed wrapper — `method` and `params` are referenced directly so
        // `serde_json::to_string` walks the caller's values without first
        // materializing an intermediate `Value` tree (the old code did
        // `serde_json::to_value(params)?` then re-serialized). The stateless
        // mode (ADR 0019) additionally flattens a `_meta` block next to the
        // params, still borrow-only.
        let body = match &self.stateless {
            Some(meta) => {
                let params = connection::WithMeta::new(params, meta);
                serde_json::to_string(&OutgoingRequest {
                    jsonrpc: JSONRPC_VERSION,
                    id,
                    method,
                    params: &params,
                })?
            }
            None => serde_json::to_string(&OutgoingRequest {
                jsonrpc: JSONRPC_VERSION,
                id,
                method,
                params,
            })?,
        };
        let response_body = self.transport.request(&body).await?;

        let env: ResponseEnvelope = serde_json::from_str(&response_body)?;
        if env.id != id {
            return Err(SessionError::IdMismatch {
                expected: id,
                got: env.id,
            });
        }
        match env.payload {
            ResponsePayload::Ok { result } => Ok(serde_json::from_value(result)?),
            ResponsePayload::Err { error } => Err(SessionError::Server(error)),
        }
    }

    pub(super) async fn notify<P: Serialize + ?Sized>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<(), SessionError> {
        let notif = OutgoingNotification {
            jsonrpc: JSONRPC_VERSION,
            method,
            params,
        };
        let body = serde_json::to_string(&notif)?;
        self.transport.notify(&body).await?;
        Ok(())
    }
}
