# 19. Stateless connection layer for MCP 2026-07-28

Date: 2026-07-07
Status: Accepted — implemented against the **release candidate** on
2026-07-07 (user directed full-plan execution ahead of the final spec).
A reconciliation pass against the final text + RC→final changelog is
scheduled for 2026-07-29; every "per RC" statement below is re-verified then.

## Context

The MCP **2026-07-28** revision (final spec lands 2026-07-28; drafted here from
the release candidate) is the largest protocol change to date. Per the RC:

- The `initialize` / `notifications/initialized` handshake is **removed**.
- Protocol version, client info, and client capabilities travel in **`_meta`
  on every request**; a new **`server/discover`** method returns server
  capabilities on demand.
- The protocol-level session (`Mcp-Session-Id`) is **removed** — any request
  can land on any server instance.
- Extensions are negotiated via an `extensions` capability map (reverse-DNS
  ids); W3C Trace Context key names in `_meta` are documented.

Everything in our `Session` assumes the handshake model: constructors run
`initialize` before returning, `cold_start` measures spawn→`initialize`, and
`Run` captures the negotiated version from the handshake. ADR 0018 committed
us to testing servers across revisions, so 2026-07-28 must coexist with the
handshake revisions — not replace them.

## Decision

1. **The seam is the session layer, not the transport layer.** Transports
   stay byte pipes (`request`/`notify` on raw JSON-RPC bodies). A new
   internal strategy — `session/connection.rs` — owns the *conversation
   shape*:
   - `Handshake` (default): today's behavior, byte-for-byte, for
     2025-03-26 / 2025-06-18 / 2025-11-25 across all four transports.
   - `Stateless` (2026-07-28): no construct-time handshake; every outgoing
     request's params are wrapped with the spec's `_meta` block; server
     capabilities are fetched lazily via `server/discover` (memoized;
     re-fetchable for `cold_start`); `notifications/initialized` is never
     sent.
2. **`Session`'s public API does not change.** `spawn*` / `from_transport*`
   constructors, `list_tools` / `call_tool` / `shutdown`, and the version
   accessors keep their signatures; `ServerConfig.protocol_version =
   "2026-07-28"` (via `SessionFactory`, ADR 0017/0018) selects the stateless
   connection. Scenario code stays version-blind (ADR 0018 decision 4).
3. **`_meta` injection preserves ADR 0006 zero-copy.** A serialize-time
   wrapper — `struct WithMeta<'a, P: ?Sized + Serialize> {
   #[serde(flatten)] params: &'a P, "_meta": MetaBlock<'a> }` — borrows the
   caller's params; no intermediate `Value` tree. Constraint (acceptable):
   `flatten` requires map-shaped params, which every MCP method's params
   are. Exact `_meta` key names/shape come from the final spec.
4. **`cold_start` semantics in stateless mode** (ADR 0018 decision 5): the
   `cold_start:handshake` metric records spawn/connect → first
   `server/discover` response; the report's honest-disclosure note states
   which edge was measured.
5. **Scope for v0.0.1:** stateless mode ships for **stdio + Streamable HTTP**
   only (the transports the stateless core is designed around). SSE (legacy
   transport) and WS keep handshake-mode only; extensions negotiation, Tasks,
   and MCP Apps are explicitly out of scope (revisit v0.2+).
6. **Version reporting:** with no handshake there is no server-negotiated
   version string; `Report.server_info.protocol_version` sources from the
   configured revision, and strict-mode version gating (ADR 0018 decision 2)
   degrades to config validation only, unless the final spec gives servers a
   version-echo channel (open question).

## Alternatives considered

| Option | Why rejected |
| ------ | ------------- |
| **Separate `StatelessSession` type** | Doubles the public API; every scenario is written against `Session`, so the split would leak into all scenario code — exactly what ADR 0018 forbids. |
| **Inject `_meta` at the transport layer** | `_meta` is protocol-layer data; transports would need to parse/rewrite JSON bodies, breaking the byte-pipe contract and re-adding the `to_value` round-trip ADR 0006 removed. |
| **Model stateless as "the new normal" and emulate the handshake for old revisions** | Highest-risk option: it rewrites the code path that the existing 441-test suite pins; handshake servers remain the installed base for years. |
| **Defer 2026-07-28 support entirely to v0.2** | A load tester that can't speak the current protocol decays fast (plan Phase 1 rationale); the RC is stable enough to design against now. |

## Consequences

- `session/` grows `connection.rs` (+ `stateless.rs`); each new file stays
  under the 300-line convention. `session.rs` itself changes minimally
  (constructor delegation already exists from T0.3).
- `ProtocolVersion` gains `V2026_07_28`; `"2026-07-28"` becomes valid for
  `[server] protocol_version` but is rejected with a clear error for SSE/WS
  configs (scope decision 5).
- The T1.3 read-path work also absorbs the deferred server-initiated-request
  robustness fix (audit row 13): unknown server→client requests get a
  `-32601` reply instead of poisoning response correlation.
- New fixture `mock-stateless-http.py` + `tests/stateless.rs` (plan T1.4)
  become the regression net; `version_matrix` (plan T1.5) spans handshake and
  stateless modes through the same `SessionFactory` seam.

## Open questions (blocked on the final spec, 2026-07-28)

- Exact `_meta` field names for protocol version / client info / capabilities,
  and whether `MCP-Protocol-Version` HTTP header semantics change.
- `server/discover` request/response schema; whether any version echo exists
  for strict-mode gating (decision 6).
- Whether stdio remains a first-class stateless transport in the final text
  or the stateless core is HTTP-only — adjust scope decision 5 accordingly.
- RC→final diff: audit before Part B (first step of the implementation
  sprint).
