# mcp-loadtest

[![CI](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml/badge.svg)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Teerapat-Vatpitak/mcp-loadtest)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](rust-toolchain.toml)

> Load tester **and bug detector** for MCP (Model Context Protocol) servers.
> Find deadlocks, hangs, response races, and performance regressions before
> they reach users.

`mcp-loadtest` drives a real MCP server through the protocol instead of calling
its internals. It is built for reliability checks that can fail CI
deterministically: no successful calls, deadlocks, response divergences,
terminal transport failures, and configured SLO violations all produce a
non-zero exit code.

## What it catches

Lazy initialization can pass `initialize` and `tools/list`, then deadlock on
the first concurrent `tools/call`. The focused probe reproduces that failure
class directly:

```bash
mcp-loadtest deadlock-probe \
  --server "python -m my_mcp" \
  --tool get_market_data \
  --args '{"ticker":"AAPL"}'
```

```text
Status: FAIL (1 deadlock)
Deadlocks: 1   Hangs: 0   Errors: 0
Error: DEADLOCK DETECTED
```

This is the bug class behind
[HKUDS/Vibe-Trading PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85).
The repository includes a
[regression test](crates/engine/tests/vibe_trading_regression.rs) pinned to the
affected revision.

## Install

After the immutable `v0.2.0` GitHub Release and its checksums exist, download
its prebuilt binary or install the exact tag with Cargo:

```bash
cargo install \
  --git https://github.com/Teerapat-Vatpitak/mcp-loadtest \
  --tag v0.2.0 \
  --locked \
  mcp-loadtest-cli
```

Then verify the installation:

```bash
mcp-loadtest --version
mcp-loadtest doctor
```

If that exact release is not yet available, use the immutable
[`v0.1.0` GitHub Release](https://github.com/Teerapat-Vatpitak/mcp-loadtest/releases/tag/v0.1.0)
instead. The project is not distributed through crates.io.

## New in v0.2.0

- Distributed load generation through short-lived OpenSSH workers
- Editor-ready Draft 2020-12 config schema
- JUnit, Prometheus, and OTLP outputs
- Rolling baseline history and multi-run regression gates
- Full final MCP `2026-07-28` client-role support
- OAuth authorization code, client credentials, CIMD, and dynamic registration

Treat `v0.2.0` as published only when the exact tag, GitHub Release, platform
archives, and matching checksum sidecars all exist.

## Run a load test

Generate a starter configuration:

```bash
mcp-loadtest example-config > bench.toml
mcp-loadtest config-schema > mcp-loadtest.schema.json
```

A minimal sustained-load configuration looks like this:

```toml
[server]
command = "python"
args = ["-m", "my_mcp"]
transport = "stdio"

[scenario]
type = "sustained"
duration = "60s"
concurrent = 50
tool = "get_market_data"
args = { ticker = "AAPL" }

[thresholds]
p99_latency = "500ms"
error_rate = 0.01
hang_timeout = "5s"

[output]
report_dir = "./runs"
formats = ["terminal", "markdown", "json"]
```

Run it:

```bash
mcp-loadtest run --config bench.toml
```

Each run can produce terminal, Markdown, JSON, self-contained HTML, JUnit XML,
and Prometheus text reports. OTLP/HTTP export is configured separately.
Use the schema-stable `metrics.json` for CI comparisons:

```bash
mcp-loadtest compare \
  runs/baseline/metrics.json \
  runs/current/metrics.json
```

## Use it in GitHub Actions

Pin both the Action and binary to the immutable release:

```yaml
- name: Probe MCP server for deadlocks
  uses: Teerapat-Vatpitak/mcp-loadtest@v0.2.0
  with:
    version: v0.2.0
    server: "python -m my_mcp"
    args: '["--tool","get_market_data","--concurrent","10","--args","{\"ticker\":\"AAPL\"}"]'
```

The Action downloads a prebuilt binary, verifies its SHA-256 checksum, and
fails the job when the command or an optional baseline comparison fails.
Its `args` input is a JSON array of literal arguments; shell syntax is not
evaluated. See the complete
[CI integration guide](docs/examples/ci-integration.md) for reports,
thresholds, and baseline gating.

## What it covers

- **Correctness under concurrency** — synchronized deadlock and race probes
  across independent sessions.
- **Load profiles** — sustained, ramp, spike, soak, cold-start, and weighted
  multi-step workloads.
- **MCP-aware metrics** — p50/p95/p99/p999 latency, throughput, error classes,
  process sampling, coverage, and per-tool SLOs.
- **Record and replay** — capture versioned JSONL traces, replay them against a
  fresh server, and fail on response divergence.
- **Regression analysis** — compare two reports or run the same workload
  against multiple servers; rolling history detects multi-run trends.
- **Distributed load** — shard one global concurrency budget across SSH
  workers and merge exact latency distributions.
- **CI and observability** — JSON, JUnit, Prometheus, and OTLP outputs.
- **Agent access** — `mcp-loadtest serve --mcp` exposes deadlock, sustained
  load, and comparison tools over MCP.

## Built-in scenarios

| Scenario | Purpose |
| --- | --- |
| `cold_start` | Measure spawn, initialization, and first-call latency |
| `sustained` | Track steady-state latency, throughput, and errors |
| `ramp` | Find the concurrency breaking point |
| `spike` | Exercise baseline, burst, and cooldown phases |
| `soak` | Detect long-running resource drift |
| `deadlock_probe` | Reproduce lazy-init and first-call deadlocks |
| `race_check` | Detect non-deterministic responses |
| `fuzzer` | Probe malformed protocol input handling |
| `pattern` | Run weighted multi-step tool-call sequences |
| `version_matrix` | Compare behavior across MCP protocol revisions |

Run `mcp-loadtest list-scenarios` for the CLI summary. Configuration details
and design rationale live in [DESIGN.md](DESIGN.md).

## Transports and security

The client supports `stdio`, Streamable HTTP, SSE, and WebSocket transports.
Remote targets require an explicit host allowlist. Credentials are read from
environment variables through `headers_from_env`; literal remote credentials
in TOML and URL userinfo are rejected. Credential-bearing remote connections
require HTTPS or WSS.

For OAuth, `[server.auth]` supports authorization-code + PKCE and
client-credentials flows. Discovery, refresh, and bounded scope step-up are
handled automatically; client secrets are referenced by environment-variable
name. Distributed mode permits non-interactive client credentials only, with
each worker resolving its own secret and acquiring its own token.

See the [transport security ADR](docs/adr/0007-transport-security-posture.md)
for resolver pinning, header restrictions, redaction boundaries, and response
size limits.

## Protocol compatibility

`protocol_version = "auto"` discovers the compatible MCP era and falls back
only when the peer supplies protocol evidence. The final `2026-07-28`
client-role surface is supported, while `2025-11-25`, `2025-06-18`, and
`2025-03-26` remain available for pinned compatibility testing. The scoped
official client conformance matrix is documented in
[ADR 0023](docs/adr/0023-mcp-2026-final-reconciliation.md).

## Documentation

- [Cookbook examples](docs/examples/) — CI integration, custom scenarios, and
  deadlock debugging
- [DESIGN.md](DESIGN.md) — architecture, behavior, and product positioning
- [Architecture decisions](docs/adr/) — security and implementation rationale
- [Metrics schema](docs/schema/metrics.v1.json) — versioned JSON report contract
- [Config schema](docs/schema/config.v1.json) — Draft 2020-12 editor contract
- [CHANGELOG.md](CHANGELOG.md) — release history
- [SECURITY.md](SECURITY.md) — vulnerability reporting and security posture

## Development

```bash
git clone https://github.com/Teerapat-Vatpitak/mcp-loadtest
cd mcp-loadtest
bash scripts/ci-checks.sh
# Windows: pwsh scripts/ci-checks.ps1
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## Release status

The source version is `0.2.0`, but source text alone does not prove release
availability. Consider `v0.2.0` published only after its tag, checksums,
platform assets, and GitHub Release are verified. The immutable `v0.1.0`
release remains available under `MIT OR Apache-2.0`; later development is
Apache-2.0.

## License

Development after `v0.1.0` is licensed under the
[Apache License 2.0](LICENSE). The immutable `v0.1.0` release retains its
original `MIT OR Apache-2.0` terms.
