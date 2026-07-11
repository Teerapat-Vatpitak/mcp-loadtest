# 0016. DNS-rebinding defense: resolver pinning for outbound transports

Date: 2026-06-12
Status: Accepted

## Context

ADR 0012 added the SSRF host-allowlist + always-on private-IP-literal block,
but recorded one **accepted gap — DNS rebinding**: a _hostname_ that
resolves to a private IP (`localtest.me` → `127.0.0.1`, or attacker-controlled
DNS that flips between validation and connect) was not blocked, because the
guard only ever saw the URL string — reqwest / tokio-tungstenite owned the
resolver and the connect socket, so there was no hook to inspect the resolved
address. The gap was pinned by a regression test in `guard.rs` precisely so
that closing it would be "a deliberate, reviewed reversal". ADR 0012 named
the follow-up: a custom connector + resolver pinning (resolve once, reject
private, connect to the pinned IP). This ADR is that follow-up. It **closes
ADR 0012's open question** and **deliberately reverses the permissive pin**
(replaced by `hostname_resolving_to_private_ip_blocked_at_resolve_layer`).

## Decision

Add a resolver layer (`protocol::transport::resolve`) that every outbound
network transport (http / sse / ws) runs at `connect` time, **after** the
ADR 0012 literal layer and **before** any socket is opened:

1. **Resolve once, vet everything, pin the result.**
   `resolve_and_check(url, &HostGuard)` first runs the unchanged
   `HostGuard::check_url` (IP-literal block + allowlist). IP-literal URLs
   short-circuit — the literal _is_ the pin, no DNS involved. Hostname URLs
   are resolved via `tokio::net::lookup_host` (non-blocking); if **any**
   resolved address is private/loopback/link-local/ULA/reserved (the same
   `is_blocked_ip` predicate the literal layer uses, so the two layers can
   never disagree), the whole URL is rejected. The vetted `Vec<SocketAddr>`
   is returned for pinning.
2. **The checked IP is the dialed IP (closes the rebind TOCTOU).**
    - http + sse pin via `reqwest::ClientBuilder::resolve_to_addrs(host,
&vetted_addrs)` — reqwest never consults DNS again for that host.
    - ws dials `tokio::net::TcpStream::connect` to a vetted address itself
      (falling back across the vetted list in resolver order), then completes
      the handshake with `tokio_tungstenite::client_async_tls` using the
      original URL, so TLS SNI and the `Host` header keep the hostname.
    - sse re-runs the full resolve + vet + pin against the server-provided
      `endpoint` POST URL (ADR 0012 point 4) and gives the POSTs their own
      pinned client.
3. **Allowlisted hostnames keep the escape-hatch semantics.** A hostname in
   `[server].allowed_hosts` is explicitly operator-trusted, so its resolution
   may be private (`allowed_hosts = ["localhost"]` for local testing) — the
   exact mirror of what ADR 0012 grants listed IP literals. The result is
   still pinned, so even a trusted name cannot rebind mid-session.
4. **Fail closed.** DNS failure or an empty resolution is an error, never a
   pass-through to the library's own resolver.
5. **Stable rejection contract.** Rejections reuse
   `TransportError::Other(String)` with the same greppable substring
   `blocked host` ADR 0012 standardized (the CLI hint layer keys on it), plus
   the marker `ADR 0016` so resolver-layer rejections are distinguishable
   from literal-layer ones (`ADR 0012`).

No public API changes: `{Http,Sse,Ws}Transport::connect` keep their
signatures; the resolver layer is internal (`pub(crate)`). The resolve
function is seam-injectable (`resolve_and_check_with` takes a resolver fn) so
unit tests drive rebinding scenarios without real DNS.

## Alternatives considered

- **Re-resolve at connect time and check then.** Still a TOCTOU: the check
  and the connect would use two separate lookups, which is exactly the window
  rebinding DNS exploits. Resolve-once-then-pin removes the second lookup.
- **A custom `reqwest` connector / `dns_resolver` service.** More invasive
  (tower service plumbing, hyper connector types) for no additional benefit
  over `resolve_to_addrs`, which is a stable, first-class reqwest API that
  achieves the same pin.
- **Keep `connect_async` for ws and pre-check only.** `connect_async` does
  its own `lookup_host` internally — a second, unvetted resolution. Dialing
  the TCP stream ourselves and handing it to `client_async_tls` is the only
  way to guarantee the vetted address is the dialed one.
- **Block private resolutions even for allowlisted hostnames.** Rejected:
  it would break the documented local-testing escape hatch
  (`allowed_hosts = ["localhost"]`) and diverge from ADR 0012's semantics for
  listed literals. The allowlist is exact-match and operator-authored;
  trusting it is the design.
- **A new `TransportError` variant.** Rejected for the same reason as in
  ADR 0012: the enum is `#[non_exhaustive]`-locked and the substring contract
  already serves the hint layer.

## Consequences

- **Makes easy:** the `localtest.me`-class bypass and flip-after-check
  rebinding are closed on all three network transports with zero config;
  hostname rejections are greppable (`blocked host` + `ADR 0016`); rebinding
  logic is unit-testable offline via the resolver seam.
- **Makes hard / commits us to:** `connect` now performs one DNS resolution
  up front for hostname URLs (network I/O moved earlier; one extra lookup for
  the SSE endpoint URL). Pinning means a host whose DNS legitimately changes
  mid-session is not re-resolved until the next `connect` — acceptable for
  load-test runs, which construct transports per run. Long-lived multi-A-record
  rotation (DNS-based load balancing) is intentionally frozen per connection.
- **Residual risk (by design):** operator-allowlisted hosts are trusted —
  a hostname in `allowed_hosts` may resolve anywhere, including private
  space. Time-of-use re-resolution by intermediate proxies (if an operator
  routes through one) is outside our control. `SECURITY.md` is updated
  accordingly.
