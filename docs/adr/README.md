# Architecture Decision Records

Append-only log of significant decisions. New ADRs are added with the next number. Past ADRs are not edited; if a decision is reversed, write a new ADR that supersedes it.

## Index

| #                                              | Title                                                                 | Status   |
| ---------------------------------------------- | --------------------------------------------------------------------- | -------- |
| [0001](0001-language-rust.md)                  | Language: Rust                                                        | Accepted |
| [0002](0002-runtime-tokio.md)                  | Async runtime: tokio                                                  | Accepted |
| [0003](0003-histogram-hdr.md)                  | Latency histogram: hdrhistogram                                       | Accepted |
| [0004](0004-compete-with-reaatech.md)          | Strategy: head-on competition with reaatech/mcp-load-test             | Accepted |
| [0005](0005-serve-mcp-mode.md)                 | Serve mode: expose load tester as MCP server over stdio               | Accepted |
| [0006](0006-zero-copy-protocol-types.md)       | Zero-copy protocol types on the request hot path                      | Accepted |
| [0007](0007-transport-security-posture.md)     | Transport security posture: redirects, frame caps, path validation    | Accepted |
| [0008](0008-release-profile-panic-abort.md)    | Release profile: `panic = "abort"`                                    | Accepted |
| [0009](0009-regression-threshold-defaults.md)  | Regression threshold defaults: 10% p99 / 0.5pp error rate             | Accepted |
| [0010](0010-strict-schema-validation.md)       | Opt-in strict MCP schema validation                                   | Accepted |
| [0011](0011-supply-chain-advisory-policy.md)   | Supply-chain policy: CDLA-Permissive-2.0 + triaged advisory ignores   | Accepted |
| [0012](0012-ssrf-host-allowlist.md)            | SSRF host-allowlist + always-on private-IP-literal block              | Accepted |
| [0013](0013-spawn-options-api.md)              | SpawnOptions API for stdio stderr capture                             | Accepted |
| [0014](0014-error-hints-explain-doctor.md)     | Error hints, `--explain`, and `doctor` (AI-friendliness §21)          | Accepted |
| [0015](0015-defer-crates-io-distribution.md)   | Defer crates.io; git-install + GitHub Release binaries (amends 0004)  | Accepted; v0.0.1 plan not executed |
| [0016](0016-dns-rebinding-resolver-pinning.md) | DNS-rebinding defense: resolver pinning (closes 0012's open question) | Accepted |
| [0017](0017-session-pool-in-scenario.md)       | Session pool inside the scenario — real `concurrent` (M8)             | Accepted |
| [0018](0018-multi-version-protocol.md)         | Multi-version MCP protocol strategy (2025-03-26 → 2026-07-28)         | Accepted |
| [0019](0019-stateless-connection-layer.md)     | Experimental stateless connection layer for MCP 2026-07-28 RC/draft | Accepted; final release truth superseded by 0023 |
| [0020](0020-publish-to-crates-io.md)           | Proposed crates.io publication (not executed)                         | Proposed; not in force |
| [0021](0021-trace-record-replay.md)            | Trace record + replay: JSONL `mcp-trace/1`, redaction, replay diff    | Accepted |
| [0022](0022-six-crate-workspace.md)            | Six-crate layered workspace (core/protocol/engine/output/facade/cli)  | Accepted |
| [0023](0023-mcp-2026-final-reconciliation.md)  | Reconcile the scoped MCP 2026-07-28 implementation to the final tag   | Accepted; scoped experimental implementation |

## When to write a new ADR

- Choosing between alternative dependencies/frameworks
- Establishing a project-wide convention
- Reversing a previous decision
- Anything you'd otherwise re-litigate in PR review

## Template

```markdown
# NNNN. Short title

Date: YYYY-MM-DD
Status: Proposed | Accepted | Superseded by [NNNN](NNNN-...)

## Context

What's the situation? What's the question we're answering?

## Decision

The choice we made.

## Alternatives considered

What else we looked at and why we didn't pick them.

## Consequences

What this commits us to. What it makes easy / hard. Open questions.
```
