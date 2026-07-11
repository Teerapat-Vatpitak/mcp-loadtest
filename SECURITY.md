# Security policy

`mcp-loadtest` is a load-testing tool that spawns user-supplied processes, parses JSON-RPC over stdio/HTTP/SSE, and exposes a self-hosted MCP server (`serve --mcp`). Security issues in this tool can affect operators running it — not the end users of MCP servers being tested. Vulnerabilities in the MCP servers under test are the responsibility of those servers' authors.

## How to report

Email **teerapatv.c@gmail.com** with a description of the issue and reproduction steps. Please do **not** file a public GitHub issue for security bugs.

I aim to acknowledge reports within 7 days and to coordinate a fix and public disclosure within 90 days of the initial report. Credit is given in the release notes unless you ask otherwise.

## Scope

In scope:

- Child-process spawning and argv handling for the stdio transport (`Session::spawn`, command/args parsing).
- JSON-RPC framing and parsing (line reader, message dispatch).
- HTTP and SSE transports — URL handling, redirect policy, SSRF surface.
- `serve --mcp` mode — path traversal, unbounded stdin reads, OOM via large payloads, request validation.
- Supply chain — Cargo dependencies (`cargo audit` / `cargo deny` findings, advisory follow-up).

Out of scope:

- Vulnerabilities in MCP servers being tested. Report those to the server's maintainers.
- Social engineering of maintainers or contributors.
- DoS against the tool itself when run with deliberately malicious CLI input (e.g. `--server "obviously malicious string"`). We trust the operator's local CLI input.
- Issues that require pre-existing local code execution on the operator's machine.

## Recent hardening

Shipped in commit `bae92c2`:

- **Path-traversal block in `compare_runs`** — file arguments are canonicalized and rejected if they escape the expected runs directory.
- **16 MB line-read cap on stdio transport** — protects against memory exhaustion from a malicious or buggy server emitting an unbounded line.
- **Redirect policy set to `none`** on HTTP/SSE transports — blocks redirect-based SSRF and prevents silent redirection to unintended hosts.

Shipped since:

- **SSRF host-allowlist + always-on private-IP-literal block** (ADR 0012) — opt-in exact-match `[server].allowed_hosts`, plus an unconditional block of private/loopback/link-local/ULA/reserved IP-literal URLs on the HTTP/SSE/WS transports. The SSE server-provided `endpoint` URL is re-checked.
- **DNS-rebinding defense via resolver pinning** (ADR 0016) — hostnames are resolved once at connect time, every resolved address is vetted against the same private-IP blocklist, and the vetted addresses are pinned for the actual connection (reqwest `resolve_to_addrs` for HTTP/SSE; a self-dialed, vetted TCP socket for WS). This closes the hostname→private-IP gap (e.g. `localtest.me` → `127.0.0.1`, or DNS that flips between check and connect) that ADR 0012 documented as a residual risk.

### Residual risk

- **Operator-allowlisted hosts are trusted by design.** A hostname or IP literal listed in `[server].allowed_hosts` may point or resolve anywhere — including loopback/private space. That is the documented escape hatch for local testing (`allowed_hosts = ["localhost"]`); only list hosts you control.
- Pinning freezes the address set per connection: a host whose DNS legitimately rotates is not re-resolved until the next connect. This is a deliberate trade for rebinding safety (ADR 0016).
- Egress through an operator-configured proxy resolves names outside the tool's control and is not covered by the pin.

## Supported versions

Only the latest `0.x` release line receives security fixes. Once `1.0` ships, this policy will be revisited.

| Version    | Supported |
| ---------- | --------- |
| latest 0.x | yes       |
| older 0.x  | no        |
