# 0022. Six-crate layered workspace

Date: 2026-07-10
Status: Accepted

## Context

Previously the project was a two-crate workspace: one large library crate
`mcp-loadtest` (protocol, transports, session, scenarios, run orchestration,
metrics, reporting, TUI, serve) plus the `mcp-loadtest-cli` binary. The library
crate had grown past the point where its internal layering was legible or
enforceable: everything could reach everything via `crate::`, so nothing stopped
an upward dependency from forming, and several already had.

A pre-restructure inventory (143 files) found the module graph was not the clean
stack the module names implied. Concretely:

- **`scenario` → `run` inversion.** `SessionFactory` lived in `run::factory`, but
  it is consumed by `scenario` code (cold_start, pools, the fuzzer's raw path).
  Scenarios reached *up* into the orchestration layer for it.
- **`config` needed `protocol`.** `ProtocolVersion` lived in `protocol::mcp`, but
  `config` (and `config::validate`) had to name it to resolve
  `[server] protocol_version` — a lower layer reaching up into the wire layer.
- **`tui` (output) polled `run`-adjacent state.** The live dashboard reads
  `Recorder` snapshots; `Recorder` sat in a module tangled with run-time
  concerns rather than in a pure data layer both producers and renderers could
  depend on.
- **Reporters and the report model were fused.** `Report` (data) and the
  Markdown/JSON/HTML/terminal renderers lived together, so anything constructing
  a `Report` (the engine) transitively pulled the renderers (output).

Left alone, these edges make the crate impossible to split later and let new
inversions form silently. We want a layering the compiler enforces: strictly
downward dependencies, one concern per crate, and public paths preserved so the
change is not a breaking release for library users.

## Decision

Restructure into **six crates** with strictly downward dependencies. Directories,
package names, and the lib name used in code:

| Dir | Package | Lib name | Role |
|---|---|---|---|
| `crates/core` | `mcp-loadtest-core` | `mcp_loadtest_core` | Pure data: config, metric/report/outcome types, coverage, fuzz report, trace on-disk format, `ProtocolVersion` |
| `crates/protocol` | `mcp-loadtest-protocol` | `mcp_loadtest_protocol` | Wire: JSON-RPC, MCP types, schema validator, transports (stdio/http/sse/ws), session, hang detector, `SessionFactory` |
| `crates/engine` | `mcp-loadtest-engine` | `mcp_loadtest_engine` | Scenario engine, run orchestration, process sampler, trace runtime (writer/replay), breaking-point + race analyzers |
| `crates/output` | `mcp-loadtest-output` | `mcp_loadtest_output` | Report renderers, grading, regression policy, live TUI |
| `crates/mcp-loadtest` | `mcp-loadtest` | `mcp_loadtest` | Facade: thin re-export surface over the four crates above + the feature-gated `serve` module |
| `crates/mcp-loadtest-cli` | `mcp-loadtest-cli` | `mcp_loadtest_cli` (bin `mcp-loadtest`) | Command-line interface |

**Dependency direction (enforced by Cargo):**

```
core  ← protocol ← engine
core  ← output
facade (mcp-loadtest) ← core, protocol, engine, output
cli ← facade
```

`core` imports nothing from the workspace; `protocol` imports only `core`;
`engine` imports `core` + `protocol`; `output` imports only `core`; the facade
imports all four; the CLI imports only the facade.

**Type re-homings** — the moves that fix the inverted edges above:

- `ProtocolVersion` → `core::version` (was `protocol::mcp`). Fixes `config` → `protocol`.
- `SessionFactory` → `protocol::factory` (was `run::factory`). Fixes `scenario` → `run`.
- `Recorder` + the metric data model → `core::metrics`. The TUI (output) and the
  engine both depend only on `core` for it.
- `Report` + `Reporter` trait + `format_iso8601_utc` → `core::report`. The engine
  constructs `Report`s; the renderers (output) render them — neither pulls the other.
- `ScenarioOutcome` → `core::outcome`; `coverage`, `fuzz_report`, `ToolSlo` → `core`.
- The `analysis` toolkit splits by lifecycle: `coverage`/`fuzz_report` (mid-run
  data) → core, `breaking_point`/`race_detector` (mid-run/replay logic) → engine,
  `grading`/`regression` (post-run) → output.

**Amends [ADR 0021].** ADR 0021 placed the trace format + writer + replay in
`mcp-loadtest/src/trace/` and explicitly reserved the option to split the format
and replay halves into a crate later "if a cleaner layering ever makes it worth
it." This ADR takes that option: the **on-disk format + `TraceError`** move to
`core::trace` (pure data, no cycle), while the **recording decorator + replay
driver** move to `engine::trace`. The decorator is still cycle-bound to the
`Transport` trait (now in `protocol`) and to `Run::execute` (in `engine`), so it
sits in `engine`, not a standalone crate — the cycle ADR 0021 identified still
rules out a separate `mcp-trace` crate.

**Public API preserved.** The facade re-exports every previously-public path, so
`mcp_loadtest::Session`, `mcp_loadtest::scenario::sustained::Sustained`,
`mcp_loadtest::report::html::HtmlReporter`, etc. keep resolving. This is a
non-breaking change for facade users; only code importing the layer
crates directly needs the new canonical homes.

## Alternatives considered

- **Two-crate module re-layering (no new crates).** Enforce layers with module
  privacy and review discipline instead of crate boundaries. Rejected: `crate::`
  makes every item reachable, so nothing *mechanically* prevents the next
  inversion — the exact failure mode we are fixing.
- **Three-crate split (core + lib + cli).** Pull only the pure data layer out.
  Rejected: leaves scenarios, transports, and renderers fused, so the
  scenario→run and report/renderer edges survive.
- **No-facade variant** (CLI depends on all four layer crates directly). Rejected:
  every existing `mcp_loadtest::...` import in downstream code and in our own
  tests/docs would have to change — a breaking release for no user benefit. The
  facade absorbs the churn and keeps `0.x` compatibility.

## Consequences

- The facade must **track the re-export surface**: anything `pub` in it is THE
  public API, and a change there still needs a CHANGELOG entry.
- `/release-checks` now covers **six crates in dependency order** (core →
  protocol → engine → output → facade → cli) for publish dry-runs.
- `cargo-binstall` / release naming is **unchanged**: the CLI package stays
  `mcp-loadtest-cli`, the binary stays `mcp-loadtest`, the facade package stays
  `mcp-loadtest`.
- File-size and layering invariants are now cheap to audit (per-crate `grep` for
  upward imports; every production `.rs` under the 300-line convention).
- Benches move with their code (`record`/`histogram` → core, `session_loopback`/
  `hang_detect` → protocol); criterion matches baselines by bench name, which is
  unchanged, so pre/post comparison stays valid.

Open question: whether to later promote a stable subset of the layer crates to
their own semver track (publishing them to crates.io independently). Deferred —
today only the facade + CLI are distributed (ADR 0015).
