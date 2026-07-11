# 0012. SSRF host-allowlist + always-on private-IP-literal block

Date: 2026-05-17
Status: Accepted

## Context

`mcp-loadtest` drives operator-supplied URLs over the http / sse / ws
transports. ADR 0007 ("Transport security posture") established the redirect
posture (`reqwest::redirect::Policy::none()` so a server cannot bounce us into
`169.254.169.254` or a localhost-bound admin endpoint) and frame caps, but it
**explicitly deferred a host-allowlist** as an open item — at the time the tool
was treated as a single-operator local utility, and the redirect block closed
the most obvious server-driven SSRF vector.

We are about to publish the first release. Once published, the tool is no
longer "just my local script": configs get shared, pasted into CI, and run
against URLs the operator did not personally vet. A malicious or mistaken
`url = "http://169.254.169.254/latest/meta-data/"` (or `http://127.0.0.1:<admin
port>/`) in a `[server]` block should not silently exfiltrate cloud-instance
credentials or poke internal services. The redirect block does not help when
the _initial_ URL is already internal. This is the right moment to add the
allowlist: doing it now folds the (breaking) `connect` signature change into
the initial release for free, with no released version to break.

## Decision

Add a transport-layer guard (`protocol::transport::guard::HostGuard`) that runs
**before any socket is opened** — every transport parses its URL, then calls
`HostGuard::check_url` before building the reqwest client / dialing the WS:

1. **Opt-in exact-match `[server].allowed_hosts`.** A new
   `#[serde(default)] allowed_hosts: Vec<String>` on `ServerConfig`
   (`#[non_exhaustive]` + serde-default ⇒ old configs still parse, no breaking
   change to the config schema). Matching is exact and ASCII-case-insensitive
   with **no wildcard / suffix logic**, so `api.example.com.attacker.com` can
   never satisfy an entry of `api.example.com`. Entries must be bare hostnames
   (no scheme / port / path / whitespace); `config::validate` rejects
   malformed entries up front with an actionable message.
2. **Always-on private/loopback/link-local/ULA/reserved IP-literal block.** If
   the URL host is an IP literal in 127/8, 10/8, 172.16/12, 192.168/16,
   169.254/16, `0.0.0.0`, `255.255.255.255`, `::1`, `::`, `fc00::/7`,
   `fe80::/10`, or an IPv4-mapped form of any of those, it is rejected — even
   if `allowed_hosts` is empty. The operator **escape hatch** is to list the
   exact literal (e.g. `allowed_hosts = ["127.0.0.1"]` for local testing); a
   listed literal is allowed regardless of being loopback/private.
3. **Empty `allowed_hosts` ⇒ allow any _public_ host** (the IP-literal block
   still applies). Security is opt-in to stay backward-compatible with the
   many existing configs that point at public endpoints by hostname.
4. **The SSE server-provided `endpoint` URL is re-checked.** A hostile SSE
   server could hand back a POST URL pointing at internal infrastructure;
   `SseTransport::connect` runs the guard against the joined `post_url` too.
5. Rejections reuse the existing `TransportError::Other(String)` (the
   `TransportError` enum is locked `#[non_exhaustive]`; no new variant). Every
   rejection message contains the stable literal substring `blocked host` and
   the marker `ADR 0012`, so the CLI hint layer (Feature 3) can detect SSRF
   rejections by substring without coupling to a typed variant.

`{Http,Sse,Ws}Transport::connect` gain a `&HostGuard` parameter (breaking, but
absorbed pre-publish per the release plan). `run::build_session` constructs the
guard via `HostGuard::from_config(&config.server)` and threads it in.

This **reverses ADR 0007's deferral** of the host-allowlist and **supersedes
the host-allowlist Open item of ADR 0007**. Per the append-only ADR policy,
0007 is not edited; this ADR records the reversal.

## Alternatives considered

- **Resolve the hostname and block private resolved IPs (full anti-SSRF).**
  The correct long-term fix, but `reqwest` and `tokio-tungstenite` own the DNS
  resolver and the connect socket; there is no stable hook in
  `connect_async` / the default reqwest connector to inspect the resolved
  address before connecting. Doing this properly requires a custom connector +
  resolver pinning (resolve once, pin the IP, reject if private, dial the
  pinned IP). That is a meaningful surface and is deferred to a follow-up
  rather than blocking the initial publish.
- **Wildcard / suffix allowlist (`*.example.com`).** Rejected: suffix matching
  is the classic source of allowlist-bypass bugs (`evil-example.com`,
  `example.com.attacker.net`). Exact match is unambiguous and sufficient for
  the load-test use case where the target set is small and known.
- **Block-by-default (deny all unless allow-listed).** Rejected for the initial release: it
  would break every existing public-hostname config on upgrade with no
  security win for the common case (public endpoint over TLS). Opt-in
  allowlist + always-on IP-literal block is the pragmatic balance; operators
  who want strict mode set `allowed_hosts`.
- **Add a new `TransportError` variant.** Rejected: the enum is locked
  `#[non_exhaustive]` for M4; a stable substring in `Other` gives the hint
  layer everything it needs without an API change.

## Consequences

- **Makes easy:** safe defaults against the cloud-metadata / localhost-admin
  SSRF classes for IP-literal URLs, with zero config; a one-line opt-in
  (`allowed_hosts`) for strict host pinning; a stable, greppable rejection
  message for tooling.
- **Makes hard / commits us to:** the `connect` signature now carries a
  `&HostGuard`; future transports must thread it. The exact-match policy means
  operators with many subdomains must list each one (acceptable for the
  load-test use case).
- **Open question / accepted gap — DNS rebinding.** A _hostname_ that
  resolves to a private IP (`localtest.me` → `127.0.0.1`, or attacker-
  controlled DNS that flips post-validation) is **not** blocked here,
  because we never see the resolved address. `allowed_hosts` is the mitigation
  (a hostname not on the list is rejected before any lookup), and the existing
  `Policy::none()` closes the redirect vector. This residual risk is
  documented here, in `SECURITY.md`, and in the CHANGELOG. **Follow-up:**
  a custom reqwest connector + resolver-pinning (resolve, reject private,
  connect to the pinned IP) to close the rebinding gap end-to-end. A
  regression test in `guard.rs` pinned the permissive behavior so the
  follow-up change is a deliberate, reviewed reversal.
