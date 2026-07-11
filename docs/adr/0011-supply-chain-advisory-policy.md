# 11. Supply-chain policy: CDLA-Permissive-2.0 + triaged advisory ignores

Date: 2026-05-16
Status: Accepted

## Context

`cargo deny check` (a `release-checks.md` must-pass gate) failed during the
pre-release stabilization pass with a license rejection and three advisories.
`cargo audit` exited 0 — all three are `unmaintained`/`unsound` _notices_,
not severity-scored vulnerabilities. The `Cargo.lock` is byte-identical to
`main`, so these are pre-existing in the dependency tree; the RustSec DB is
live, so they surface now regardless of when CI last ran.

Triage:

| Item              | Crate                                                  | Path                                                                | Verdict                                                                                                                                                                                                         |
| ----------------- | ------------------------------------------------------ | ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| License rejected  | `webpki-roots` 0.26.11 + 1.0.7 (`CDLA-Permissive-2.0`) | `tokio-tungstenite` (rustls-tls-webpki-roots) → WS transport        | Legitimate permissive data license (Mozilla CA bundle). Allowlist it.                                                                                                                                           |
| RUSTSEC-2025-0052 | `async-std` discontinued                               | `async-std → async-object-pool → httpmock` (**[dev-dependencies]**) | Never in the published crate. No safe upgrade. Ignore.                                                                                                                                                          |
| RUSTSEC-2024-0436 | `paste` unmaintained                                   | transitive proc-macro                                               | Not a vulnerability; no drop-in upstream path. Ignore.                                                                                                                                                          |
| RUSTSEC-2026-0002 | `lru` `IterMut` Stacked-Borrows unsoundness            | `ratatui 0.29 → lru ^0.12`                                          | Only under the optional `tui` feature (default lib build excludes it). No semver-compatible fix (ratatui pins `lru ^0.12`; 0.12.5 is latest 0.12.x). Low practical impact (internal TUI cache). Ignore + track. |

## Decision

1. Add `CDLA-Permissive-2.0` to `deny.toml` `[licenses].allow`. It is a
   permissive, publish-safe license; `webpki-roots` is a standard rustls
   dependency used widely across the ecosystem.
2. Add a `deny.toml` `[advisories].ignore` list containing exactly the three
   triaged IDs above, each with an inline rationale. Nothing else is
   ignored; `version = 2`, `yanked = "warn"`, `[bans]`, `[sources]` are
   unchanged, so cargo-deny still hard-fails on any _new_ or
   actual-vulnerability advisory.
3. Record a revisit condition for the two that may become fixable upstream:
   revisit when httpmock drops async-std / when ratatui ships on patched
   `lru`.

## Alternatives considered

| Option                                            | Why rejected                                                                                                                                                                 |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Bump `ratatui` past 0.29 to pull patched `lru`    | Out of scope for a stabilization pass; ratatui major/minor bump risks API + MSRV (1.86) regressions across the TUI; the project deliberately pinned 0.29.                    |
| Drop the `httpmock` dev-dep to remove `async-std` | Loses HTTP-transport test coverage for a dev-only, not-shipped advisory — net negative.                                                                                      |
| Lower `deny.toml` to warn-only on advisories      | Blanket weakening — would also silence _future real_ vulns. Targeted, documented per-ID ignores keep the gate sharp.                                                         |
| Block the release until upstreams fix             | The only soundness item (`lru`) is transitive, optional-feature-gated, low-impact, and unfixable from here; the rest are unmaintained notices. Not a release-worthy blocker. |

## Consequences

**Positive:** `cargo deny check` passes deterministically; the policy and its
rationale are reviewable in one place; the gate still catches anything new.

**Negative:** Three advisories are intentionally suppressed — acceptable given
each is individually justified and carries a recorded revisit condition.

**Open:** Re-audit when `ratatui`/`httpmock` release fixes; if RUSTSEC-2026-0002
ever gains an exploit vector or `lru` lands in the default build, revisit
immediately (drop the ignore, force the upgrade).
