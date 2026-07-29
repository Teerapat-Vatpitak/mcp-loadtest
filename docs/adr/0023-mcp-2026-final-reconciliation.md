# 23. Implement the final MCP 2026-07-28 client role

Date: 2026-07-29
Status: Accepted — supersedes the pre-final scope limits in ADR 0018 and
ADR 0019. Their historical v0.1.0 text remains accurate for that release.

## Context

The immutable v0.1.0 release implemented an experimental subset of the
then-draft `2026-07-28` revision. For v0.2.0 we repinned the source of truth to
the final specification tag:

- specification: `5f5440bb26a62e2cf3440b92da5a667efa03b267`;
- official conformance harness:
  `49103de6ed70804e940637bf3e9e29e4a3f54e64`.

The v0.2.0 product requires the complete official client-role surface used by
an MCP load generator. Server-role and authorization-server-role conformance
are deliberately outside the claim.

## Decision

1. `protocol_version = "auto"` supports dual-era discovery. It attempts the
   final stateless discovery path and falls back to the legacy initialize
   handshake only when the peer returns protocol evidence permitting that
   fallback. An explicit revision never silently changes.
2. Final HTTP requests implement the required protocol/method/name/parameter
   metadata, validate final-result metadata, and support multi-round tool
   requests with `requestState`, `inputResponses`, and `resultType`.
3. Tool schemas use local JSON Schema Draft 2020-12 evaluation. Local `$ref`
   resolution is supported; network retrieval is disabled so a server schema
   cannot turn validation into SSRF.
4. OAuth implements RFC 9728/RFC 8414 discovery, issuer/resource binding,
   authorization code + PKCE, pre-registration, Client ID Metadata Documents,
   optional dynamic registration, refresh, client credentials, and bounded
   insufficient-scope step-up.
5. OAuth endpoints are resolved and checked against private/reserved address
   ranges, then pinned for the actual request. Redirects, proxy
   re-resolution, URL credentials, fragments, and insecure production
   endpoints fail closed. Secret values and tokens remain opaque and are
   never serialized into config, reports, traces, argv, or distributed plans.
6. The release gate executes every applicable official `2026-07-28` client
   scenario with no expected-failure baseline. The recorded scope is:
   1 multi-round/request-state scenario with 5 checks, 1
   safe-schema-reference scenario, 25 authorization scenarios, and 5
   request/tool/header scenarios in the pinned 32-scenario inventory.

## Consequences

- v0.2.0 may claim full final-revision **client-role** support, but not general
  SDK, server-role, authorization-server-role, or subscription-listener
  conformance.
- Interactive authorization is entered only when explicitly configured. The
  CLI prints the authorization URL and waits on an exact loopback callback; it
  does not open a browser automatically.
- Distributed runs permit client credentials only. Every worker resolves its
  own environment-backed secret and obtains its own issuer/resource-bound
  token; no credential value crosses the control channel.
- Any upstream scenario inventory or pinned behavior change invalidates the
  evidence and fails the release gate until this ADR and implementation are
  reviewed again.
