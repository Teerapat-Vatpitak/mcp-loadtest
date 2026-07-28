# 23. Reconcile the scoped MCP 2026-07-28 implementation to the final tag

Date: 2026-07-29
Status: Accepted — supersedes only the pre-final release-truth annotations in
[ADR 0018](0018-multi-version-protocol.md) and
[ADR 0019](0019-stateless-connection-layer.md); their architectural decisions
and historical RC text remain in force as recorded.

## Context

The `v0.1.0` candidate originally reviewed the then-current MCP
`2026-07-28` draft at specification commit
`7d6c7b86eb2f1442051849ca76429fde3c3008b0`. The official stable
`2026-07-28` tag was subsequently published at commit
`5f5440bb26a62e2cf3440b92da5a667efa03b267`. The official conformance
repository HEAD remains the reviewed commit
`49103de6ed70804e940637bf3e9e29e4a3f54e64`, but that harness still labels
the revision DRAFT/provisional and vendors a pre-final schema sourced from
specification commit `71e306956a4959c9655e5036be215d41986596e6`. It has not
been promoted into a distinct final conformance suite.

The release gate was deliberately fail-closed when the final tag appeared, so
the pre-final passing evidence became stale immediately. Before release we
must establish whether the final schema changes the wire surface implemented
by this client, repin the specification evidence to the final tag, and state
the supported scope without turning a latest-official five-scenario client
check into a final-promoted or full-protocol conformance claim.

## Reconciliation evidence

We compared the candidate's reviewed draft schema at `7d6c7b86` and the
official harness's vendored schema source at `71e30695` with the final
`schema/2026-07-28/schema.json` at `5f5440bb`. The schema definitions consumed
by the implementation are identical across all three snapshots:

- `Result` and `CompleteResult`;
- `CallToolResult` and `ListToolsResult`;
- `DiscoverResult`;
- `UnsupportedProtocolVersionError`, `HeaderMismatchError`, and
  `MissingRequiredClientCapabilityError`;
- `JSONRPCResponse`.

The final schema delta is confined to the subscriptions surface: it replaces
`SubscriptionsListenResultMeta`, adds
`SubscriptionsListenResultMetaObject` and
`SubscriptionsListenResultResponse`, and updates the reference used by
`SubscriptionsListenResult`. `mcp-loadtest` does not implement or advertise
`subscriptions/listen`, so this delta creates no wire change in the supported
subset. The release runner machine-checks this exact definition set and delta
and retains the result as `FINAL_SCHEMA_RECONCILIATION.txt`; this artifact is
the final-spec proof separate from the provisional harness logs.

## Decision

1. Pin final-spec evidence to the official `2026-07-28` tag commit
   `5f5440bb26a62e2cf3440b92da5a667efa03b267`. Retain the reviewed official
   harness pin `49103de6ed70804e940637bf3e9e29e4a3f54e64`, while recording
   that its `2026-07-28` label and vendored schema remain provisional.
2. Keep `2026-07-28` explicit and non-default. The upstream revision is
   stable; the implementation is still labelled experimental because its
   supported and conformance-tested surface is intentionally narrow.
3. Limit the release claim to the implemented client paths:
   per-request protocol/client metadata, `server/discover`,
   `tools/list`/`tools/call`, and the standard/custom request headers exercised
   by the pinned official five-scenario gate. Stateless mode remains limited
   to stdio and Streamable HTTP.
4. Explicitly exclude full-protocol conformance, OAuth/authorization,
   MRTR/request-state, `subscriptions/listen`, schema-reference behavior, and
   the official server and authorization-server roles.
5. Treat every official-conformance result captured before the final tag as
   stale. The release candidate cannot proceed until the repinned runner passes
   and retains its logs, `FINAL_SCHEMA_RECONCILIATION.txt`, final tag/commit
   identity, official scenario inventory, and executed/not-executed scope
   manifest.
6. Report the evidence as two independent facts: a
   **final-spec-reconciled scoped subset** proven by the final tag/schema
   comparison, and a passing run of the **latest official provisional
   harness**. Do not describe the latter as a final-promoted suite.

## Consequences

- No implementation wire change is required for the currently supported
  tools/discovery/request-header subset. The final schema comparison proves
  reconciliation; the fresh official harness run supplies complementary
  behavioral evidence for the five reviewed scenarios.
- Adding `subscriptions/listen`, MRTR, authorization, schema-reference
  handling, or another final-revision feature requires a separate design and
  conformance expansion rather than silently widening this ADR.
- Documentation must distinguish “reconciled to the final revision” from
  “implements the full final revision.” The former is true for the scoped
  subset; the latter is not.
- If the official scenario inventory or pinned conformance behavior changes,
  the gate fails closed until the scope manifest and implementation are
  reviewed again.
