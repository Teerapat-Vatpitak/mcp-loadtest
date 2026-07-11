# 10. Opt-in strict MCP schema validation

Date: 2026-05-16
Status: Accepted

## Context

The protocol-aware-assertions differentiator (ADR 0004) wants the load
tester to catch MCP-semantic problems — a server whose `tools/call` result
doesn't match its advertised `inputSchema`, or a scenario sending args that
violate it. But ADR 0005 deliberately chose **lenient, forward-compatible**
parsing: unknown fields are tolerated so a newer server doesn't break an
older tester. Flipping to strict-by-default would reverse that contract and
would false-positive on servers that legitimately add fields over time.

We also need a JSON Schema validator. A full crate (`jsonschema`,
Draft 2020-12) pulls a transitive tree we cannot `cargo deny`-audit in every
dev environment (cargo-deny isn't installed locally; supply-chain review is
required in CI per `deny.toml`), and conflicts with the slim-binary goal
(ADR 0008). MCP `inputSchema`s in practice use only a small, stable slice of
JSON Schema.

## Decision

1. **Strict validation is opt-in.** `[validation] strict = true` in the TOML
   config (default `false`). With it off, behaviour is byte-for-byte what it
   was before — ADR 0005's forward-compat contract is preserved; strictness
   is additive policy, not a protocol change.
2. **Dependency-free subset validator** in `protocol::schema`: handles
   `type` / `properties` / `required` / `enum` / `items` and nesting. Any
   keyword it does not model is **skipped, never failed** — the validator is
   itself forward-compatible.
3. **The gate decision is isolated** in
   `classify_schema_violation(site, violations) -> SchemaPolicy`. Mechanical
   validation ("does this match the subset?") is separate from the product
   decision ("does a mismatch here fail the run?"). `Fail` maps to
   `CallOutcome::ProtocolError` (gates the run); `Warn` / `Ignore` do not.

## Alternatives considered

| Option                                  | Why rejected                                                                                                                                                                        |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `jsonschema` crate (full Draft 2020-12) | Unauditable transitive deps in local dev; binary-size cost (ADR 0008); MCP only uses a small subset — full coverage is unused weight.                                               |
| Strict-by-default                       | Reverses ADR 0005; servers that add result fields over time would trip false regressions.                                                                                           |
| Validate args only, not results         | Throws away the differentiator — asserting on server _behaviour_ is the point. Keep both sides; let the policy weight them differently.                                             |
| Bake the policy into the validator      | Conflates a CI/product trade-off with mechanical validation, making the decision invisible and hard to tune. Keeping it a tiny standalone function makes it reviewable and ownable. |

## Consequences

**Positive:**

- Delivers the protocol-aware-assertions differentiator without breaking the deliberate forward-compat stance.
- No new dependencies; fully verifiable in every dev environment.
- The entire policy is one small function — easy to review, test, and tune.

**Negative:**

- The subset validator won't catch exotic JSON Schema constraints (`pattern`, `minimum`, `oneOf`, …). Acceptable: MCP `inputSchema`s don't use them, and unmodeled keywords are skipped rather than mishandled.
- Strict mode's value depends on `classify_schema_violation` being implemented with a deliberate policy — the placeholder ships as the safe "never gate" default so an unfinished policy can't silently fail runs.

**Open:**

- Enforcement wiring point: args validated pre-send, result validated post-receive — the exact hook in the scenario/session layer is the remaining integration step and is intentionally landed after the policy is decided.
- Per-tool strictness overrides: deferred until a user asks (mirrors the ADR 0009 posture).

**Update:** both enforcement hooks are now wired. Args validate
pre-send in `Session::call_tool` (landed first); results validate
post-receive against the tool's advertised `outputSchema` (`structuredContent`
per the 2025-06-18 MCP spec) under the non-gating
`ValidationSite::ToolCallResult` → `SchemaPolicy::Warn` arm. Per-tool
strictness overrides remain open.
