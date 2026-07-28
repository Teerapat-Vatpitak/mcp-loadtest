# 18. Multi-version MCP protocol strategy

Date: 2026-07-07
Status: Accepted (user approved proceeding to Phase 1, 2026-07-07)

**Final-reconciliation annotation (2026-07-29):** the final `2026-07-28`
specification is published at commit
`5f5440bb26a62e2cf3440b92da5a667efa03b267`. ADR 0023 reconciles the
implemented tools/discovery/request-metadata/header subset to that final
revision. The implementation remains explicit and experimental because its
scope is intentionally narrow; it is not a full-protocol, auth, MRTR,
subscriptions, or schema-reference claim. Official conformance commit
`49103de6ed70804e940637bf3e9e29e4a3f54e64` remains DRAFT/provisional for
this revision, so ADR 0023 treats final tag/schema reconciliation and the
latest official harness run as separate evidence. This annotation supersedes
the pre-final release-truth paragraph immediately below without rewriting the
historical decision.

**Historical release-truth annotation (2026-07-28; superseded by
[ADR 0023](0023-mcp-2026-final-reconciliation.md)):** the dated `2026-07-28` final
specification had not been published when `v0.1.0` was prepared. That path is
experimental, explicitly selected, and never the default/final-support claim.
It is pinned to official spec commit
`7d6c7b86eb2f1442051849ca76429fde3c3008b0` and conformance commit
`49103de6ed70804e940637bf3e9e29e4a3f54e64`. References to `v0.0.1` below are
historical planning text; that version was never tagged or released.

## Context

The MCP spec now moves faster than our release cadence:

- The client advertises a pinned `PROTOCOL_VERSION = "2025-03-26"`
  (`protocol/mcp.rs`) and stores whatever version string the server answers
  with, without validating it (`session.rs`).
- The crate already *parses* 2025-06-18 additions (`Tool::output_schema`,
  `CallToolResult::structured_content`) but never advertises that revision.
- **2025-11-25** is the current stable revision.
- **2026-07-28** (final spec lands 2026-07-28) is the largest revision to date:
  the `initialize`/`initialized` handshake and protocol-level session are
  removed; protocol version, client info, and capabilities travel in `_meta`
  on every request; a new `server/discover` method returns server
  capabilities; extensions are negotiated via reverse-DNS ids.

A load tester that cannot speak the revision a server under test speaks is not
credible, and servers speaking 2025-x revisions will remain in the wild for
years — we need to test *across* revisions, not chase the newest one.

## Decision

1. **Supported-version set, not a single pin.** v0.0.1 supports
   `{2025-03-26, 2025-06-18, 2025-11-25}` over the existing handshake path,
   plus `2026-07-28` as a distinct *stateless* connection mode (design in
   ADR 0019). The set is represented by a `ProtocolVersion` enum
   (`#[non_exhaustive]`) in `protocol::mcp`; wire structs keep `String`
   fields so serde and the zero-copy hot path (ADR 0006) are untouched.
2. **Negotiation policy.** The client advertises the newest supported
   handshake revision. If the server answers with a different version:
   in the supported set → accept and record; unknown → **warn by default,
   keep running** (default behavior stays permissive, matching today's
   accept-anything), and **gate the run only under
   `[validation] strict = true`** — the same opt-in gating philosophy as
   ADR 0010.
3. **Config surface.** New optional key
   `[server] protocol_version = "auto" | "<rev>"` (default `"auto"` =
   advertise newest supported). Pinning a revision lets a CI matrix test a
   server against each revision it claims to support; `"2026-07-28"`
   selects the stateless connection path once ADR 0019 lands.
4. **Version-specific behavior lives behind the session layer.** Scenarios
   never branch on protocol version. `Session` keeps its public API;
   `SessionFactory` (ADR 0017) is the seam that picks the connection
   behavior per version. This is what makes a `version_matrix` scenario
   (plan T1.5) possible without touching every scenario.
5. **Metric names survive the semantic shift.** `cold_start:handshake`
   keeps its name in stateless mode but measures spawn/connect → first
   `server/discover` response; the report notes the measured edge per
   version (honest-disclosure precedent).

## Alternatives considered

| Option | Why rejected |
| ------ | ------------- |
| **Chase latest only** (bump the pin each revision) | Breaks testing of the installed base; a load tester must speak what deployed servers speak. |
| **Version-specific code paths in scenarios** | N scenarios × M versions combinatorial sprawl; scenarios should measure behavior, not speak wire dialects. |
| **Cargo feature flags per revision** | Feature flags are for optional *dependencies* (`serve`/`tui`), not runtime protocol choices; a CI matrix needs all revisions in one binary. |
| **Separate binary per revision** | Distribution complexity for no isolation benefit; negotiation is a runtime concern. |

## Consequences

- Commits us to keeping ≥ 3 handshake revisions green in CI — the
  `version_matrix` scenario and per-revision fixtures (plan T1.4) become the
  regression net.
- Unknown-version warn-by-default means existing configs keep working
  against servers speaking future revisions; strict mode becomes the CI
  gate for version drift.
- `ProtocolVersion` being `#[non_exhaustive]` lets us add revisions in 0.x
  without a breaking change; removing support for one remains breaking and
  needs a CHANGELOG entry per the 0.x stability policy.
- The stateless 2026-07-28 mode needs its own design record (ADR 0019:
  `Connection` seam under `Session`, `_meta` injection without giving up
  zero-copy, `server/discover` types) before implementation.

Open questions

- Whether to also accept `2024-11-05` (the first public revision) responses:
  deferred until a real server in the wild answers with it; the warn path
  covers it meanwhile.
- Extensions negotiation (2026-07-28 `extensions` capability map) is out of
  scope for v0.0.1; revisit when a differentiator needs it (e.g. Tasks-aware
  load patterns).
