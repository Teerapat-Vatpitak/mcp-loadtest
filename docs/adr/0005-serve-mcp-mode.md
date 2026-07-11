# 5. Serve mode: expose load tester as MCP server over stdio

Date: 2026-05-11
Status: Accepted

## Context

M7 shipped `mcp-loadtest serve --mcp`, which exposes the load tester itself as an MCP server over stdio JSON-RPC. AI agents (Claude Code, Cursor, etc.) call `deadlock_probe` / `sustained_load` / `compare_runs` as MCP tools — they no longer have to shell out to the CLI, parse `--help`, and re-implement argument plumbing.

This is the project's most-touted differentiator (README §"AI-agent friendly", DESIGN §21.2) and the main reason an agent would prefer `mcp-loadtest` over reaatech's TS implementation. The decision has four moving parts:

- **Transport choice.** stdio JSON-RPC vs HTTP server vs WebSocket.
- **Tool surface.** Which subset of the CLI to expose — every scenario, or a curated set.
- **Trust model.** The operator's MCP client is trusted to spawn subprocesses on the operator's behalf.
- **Path-traversal hardening on `compare_runs`** — fixed pre-publish in commit `bae92c2`.

## Decision

- **Transport: stdio JSON-RPC.** Matches every other MCP transport in the ecosystem; zero network surface; the operator's MCP client manages the lifetime via subprocess control.
- **Tool surface: three high-value tools.** `deadlock_probe`, `sustained_load`, `compare_runs`. Curated, not auto-generated from the CLI — each tool's JSON schema is hand-written so agents get useful argument descriptions and bounded enums.
- **Trust model: operator-driven.** The operator chooses to invoke `mcp-loadtest serve` and explicitly trusts both the spawned subprocess and the calling MCP client. The serve mode does not introduce any new privilege boundary beyond what the operator already granted by running the binary.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **HTTP server transport** | Adds a network surface; requires auth + TLS to be safe; not aligned with how MCP clients discover servers in practice. |
| **Auto-generate one MCP tool per CLI subcommand** | Floods the agent's tool list, exposes flags the agent has no business setting (e.g. `--seed`), and forces every CLI flag change to ripple into the MCP schema. |
| **Sandbox the spawned subprocess** | Out of scope for the first release — operator is already trusting the binary. Documented as future work in DESIGN §21.2. |

## Consequences

**Positive:**
- New public surface that competitors lack (reaatech ships TypeScript only; no MCP-server mode).
- Curated tool surface keeps the agent's context window cheap.
- stdio transport means zero network exposure — operator threat model is unchanged from CLI use.

**Negative:**
- Three tools is a manual surface; adding a new scenario means hand-writing a new MCP tool definition rather than auto-deriving one.
- `serve` pulls in the JSON-RPC framing layer; future work should feature-gate it so library-only consumers don't pay the dependency cost.

**Open:**
- Whether to add `running_load_test` as a long-poll tool (currently scenarios run synchronously and block the MCP call). Deferred until an agent actually asks for it.
- Sandbox / capability-token model for restricted callers. Deferred to a later release.
