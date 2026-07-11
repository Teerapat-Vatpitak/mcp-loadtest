//! Enumerated malformed payload shapes used by the [`Fuzzer`](super::Fuzzer)
//! scenario.
//!
//! Split out of `fuzzer.rs` in M8: the enum + its impls + the two LazyLock
//! payload constants total ~200 LoC and have no dependency on the rest of the
//! fuzzer's driver/classification logic, so they live here. See
//! [`super`] for the driver and report wiring.

use std::sync::LazyLock;

use serde_json::{Value, json};

/// Pre-baked malformed-but-plausible payload kinds.
///
/// The variant names map 1:1 to the strings used in
/// [`mcp_loadtest_core::fuzz_report::FuzzFinding::payload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FuzzPayload {
    /// Empty request body (`""`). **Skipped in M7** — requires raw-byte transport.
    EmptyBody,
    /// `{}` — empty JSON object, no jsonrpc/method/id. **Skipped in M7**.
    EmptyObject,
    /// Truncated JSON (`{not json`). **Skipped in M7**.
    InvalidJson,
    /// `{"id":1,"method":"tools/list"}` — no `jsonrpc` field. **Skipped in M7**.
    MissingJsonRpcVersion,
    /// `{"jsonrpc":"3.0",...}` — non-2.0 version. **Skipped in M7**.
    WrongJsonRpcVersion,
    /// Valid request, valid method format, but the method doesn't exist.
    /// Routed via `tools/call` with a bogus tool name.
    UnknownMethod,
    /// "Method" sent as a number — but at this layer we tunnel via
    /// `tools/call` and provide a numeric *tool name*, which serializes as a
    /// string but exercises the server's name-validation path. Documented as
    /// a partial substitute for true method-type-confusion.
    NumericMethod,
    /// Request without `id`. **Skipped in M7** — `Session::request` always assigns one.
    MissingId,
    /// Two requests with the same id. **Skipped in M7** — needs raw transport.
    DuplicateId,
    /// ~1 MB params payload.
    GiantPayload,
    /// Method/tool name with embedded control chars (`\x00`).
    ControlChars,
    /// 100-deep nested params object.
    Nested,
    /// `params: null` — sent as `arguments: Value::Null` on a tools/call.
    NullParams,
    /// `params: "string-not-object"` — sent as `arguments: Value::String(...)`.
    StringParams,
}

impl FuzzPayload {
    /// Stable label used in [`mcp_loadtest_core::fuzz_report::FuzzFinding::payload`]
    /// and in CLI / report output.
    pub fn label(&self) -> &'static str {
        match self {
            FuzzPayload::EmptyBody => "EmptyBody",
            FuzzPayload::EmptyObject => "EmptyObject",
            FuzzPayload::InvalidJson => "InvalidJson",
            FuzzPayload::MissingJsonRpcVersion => "MissingJsonRpcVersion",
            FuzzPayload::WrongJsonRpcVersion => "WrongJsonRpcVersion",
            FuzzPayload::UnknownMethod => "UnknownMethod",
            FuzzPayload::NumericMethod => "NumericMethod",
            FuzzPayload::MissingId => "MissingId",
            FuzzPayload::DuplicateId => "DuplicateId",
            FuzzPayload::GiantPayload => "GiantPayload",
            FuzzPayload::ControlChars => "ControlChars",
            FuzzPayload::Nested => "Nested",
            FuzzPayload::NullParams => "NullParams",
            FuzzPayload::StringParams => "StringParams",
        }
    }

    /// True iff this payload requires raw-byte transport (and so is skipped
    /// in M7). The full enumeration is preserved so the public surface
    /// matches the design doc; the runtime simply records a `skipped` note
    /// per iteration that lands on one of these.
    pub fn requires_raw_transport(&self) -> bool {
        matches!(
            self,
            FuzzPayload::EmptyBody
                | FuzzPayload::EmptyObject
                | FuzzPayload::InvalidJson
                | FuzzPayload::MissingJsonRpcVersion
                | FuzzPayload::WrongJsonRpcVersion
                | FuzzPayload::MissingId
                | FuzzPayload::DuplicateId
        )
    }

    /// All variants in a stable order (used for default rotation + tests).
    pub fn all() -> Vec<FuzzPayload> {
        vec![
            FuzzPayload::EmptyBody,
            FuzzPayload::EmptyObject,
            FuzzPayload::InvalidJson,
            FuzzPayload::MissingJsonRpcVersion,
            FuzzPayload::WrongJsonRpcVersion,
            FuzzPayload::UnknownMethod,
            FuzzPayload::NumericMethod,
            FuzzPayload::MissingId,
            FuzzPayload::DuplicateId,
            FuzzPayload::GiantPayload,
            FuzzPayload::ControlChars,
            FuzzPayload::Nested,
            FuzzPayload::NullParams,
            FuzzPayload::StringParams,
        ]
    }

    /// Only the variants currently exercisable through
    /// [`mcp_loadtest_protocol::Session::call_tool`].
    pub fn exercisable() -> Vec<FuzzPayload> {
        Self::all()
            .into_iter()
            .filter(|p| !p.requires_raw_transport())
            .collect()
    }

    /// Build the `(tool_name, arguments)` pair to send via `tools/call`.
    ///
    /// Returns `None` for variants that require raw-byte transport (see
    /// [`Self::requires_raw_transport`]).
    pub fn to_call_args(self) -> Option<(String, Value)> {
        match self {
            FuzzPayload::EmptyBody
            | FuzzPayload::EmptyObject
            | FuzzPayload::InvalidJson
            | FuzzPayload::MissingJsonRpcVersion
            | FuzzPayload::WrongJsonRpcVersion
            | FuzzPayload::MissingId
            | FuzzPayload::DuplicateId => None,
            FuzzPayload::UnknownMethod => Some(("totally_unknown_tool_xyz".to_string(), json!({}))),
            FuzzPayload::NumericMethod => Some(("42".to_string(), json!({}))),
            FuzzPayload::GiantPayload => {
                // ~1 MB of repeated 'A' inside a `payload` field. Built once
                // in a static so iterating the fuzzer N times doesn't
                // re-allocate the megabyte payload per iteration — Value's
                // Clone is cheap (Arc / refcount for strings).
                Some(("echo".to_string(), GIANT_PAYLOAD.clone()))
            }
            FuzzPayload::ControlChars => {
                // Embed NUL + bell + form feed in the tool name. Servers that
                // assume printable-ASCII names mis-route here.
                Some(("ec\x00ho\x07\x0c".to_string(), json!({})))
            }
            FuzzPayload::Nested => Some(("echo".to_string(), NESTED_PAYLOAD.clone())),
            FuzzPayload::NullParams => Some(("echo".to_string(), Value::Null)),
            FuzzPayload::StringParams => Some((
                "echo".to_string(),
                Value::String("string-not-object".to_string()),
            )),
        }
    }

    /// Raw JSON-RPC frame bytes for the raw-transport-only variants — the
    /// actually-malformed bytes the fuzzer puts on the wire via
    /// [`mcp_loadtest_protocol::transport::Transport::raw_send`].
    ///
    /// Returns `None` for the variants that route through `tools/call`
    /// ([`Self::to_call_args`] returns `Some` for those instead), so the two
    /// methods partition the enum: exactly the [`Self::requires_raw_transport`]
    /// variants yield bytes here. The stdio transport appends a single trailing
    /// newline, so these are the frame *content* without the delimiter.
    pub fn raw_bytes(self) -> Option<Vec<u8>> {
        let frame: &[u8] = match self {
            // Empty line: not valid JSON — a line-framed parser chokes on it.
            FuzzPayload::EmptyBody => b"",
            // Structurally valid JSON but missing every JSON-RPC field.
            FuzzPayload::EmptyObject => b"{}",
            // Truncated object — invalid JSON mid-token.
            FuzzPayload::InvalidJson => b"{not json",
            // Well-formed object, but no `jsonrpc` version field.
            FuzzPayload::MissingJsonRpcVersion => br#"{"id":1,"method":"tools/list"}"#,
            // Wrong `jsonrpc` version (must be exactly "2.0").
            FuzzPayload::WrongJsonRpcVersion => {
                br#"{"jsonrpc":"3.0","id":1,"method":"tools/list"}"#
            }
            // Request shape with no `id` — indistinguishable from a
            // notification, so a correlating client can wedge waiting for a
            // reply that never comes.
            FuzzPayload::MissingId => br#"{"jsonrpc":"2.0","method":"tools/list"}"#,
            // Two frames sharing id=1 in one send. Built at call time so the
            // embedded frame separator is a real newline byte (a raw string
            // literal would keep `\n` literal). `raw_send` appends the final
            // newline after the second frame.
            FuzzPayload::DuplicateId => {
                let one = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
                let mut v = Vec::with_capacity(one.len() * 2 + 1);
                v.extend_from_slice(one);
                v.push(b'\n');
                v.extend_from_slice(one);
                return Some(v);
            }
            // Exercisable variants go through `to_call_args`, not raw bytes.
            _ => return None,
        };
        Some(frame.to_vec())
    }
}

/// 1 MB string payload — built once on first access. Cloning a `Value` only
/// bumps refcounts on the underlying `Arc`-backed string, so reusing this
/// across fuzz iterations avoids re-allocating the megabyte each time.
static GIANT_PAYLOAD: LazyLock<Value> = LazyLock::new(|| {
    let big = "A".repeat(1024 * 1024);
    json!({ "payload": big })
});

/// 100-level nested object payload — same lazy-once treatment as `GIANT_PAYLOAD`.
static NESTED_PAYLOAD: LazyLock<Value> = LazyLock::new(|| nested_object(100));

/// Build a 100-level nested object: `{"x": {"x": {"x": ...}}}`. Used to
/// initialize [`NESTED_PAYLOAD`] once. Exposed to the parent module for
/// the depth-correctness test.
pub(super) fn nested_object(depth: usize) -> Value {
    let mut v = json!("leaf");
    for _ in 0..depth {
        v = json!({ "x": v });
    }
    v
}
