# 15. Defer crates.io; distribute the first release (v0.0.1) via git-install + GitHub Release binaries

Date: 2026-05-18
Status: Accepted

Amends [0004](0004-compete-with-reaatech.md): the head-on-competition strategy
stands; only the first release's distribution **channel and timing** change. Supersedes
the "published to crates.io" clause of DESIGN.md §10 "Definition of done".

## Context

v0.0.1 is code-complete and ready to tag (CI green, names free, packaging
verified). The original Definition of Done assumed a crates.io publish.

crates.io is **append-only and irreversible**:

- A published version can never be deleted — only `yank`ed (hidden from new
  resolves; existing `Cargo.lock`s still pull it).
- The crate name is claimed permanently on first publish.
- The first publish must be correct because it cannot be retracted.

For a first public release we are not ready to accept that permanent commitment —
the public API surface may still move in 0.x, and the name/version is forever once
taken. We still want ADR 0004's public-feedback loop reopened _now_, just without
the irreversible lock-in.

## Decision

Defer crates.io. Ship v0.0.1 through two reversible channels:

1. **`cargo install --git`** —
   `cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest mcp-loadtest-cli`.
   Needs only a public repo; nothing to maintain server-side.
2. **Prebuilt binaries on the GitHub Release** — built by a hand-rolled
   `.github/workflows/release.yml` matrix (Linux/macOS x64+arm64/Windows) that
   also creates the Release; for users without a Rust toolchain.

The repo goes **public** (reversible — it can be re-privatised; a Release/tag can
be edited or removed). crates.io stays a recorded future option.

## Alternatives considered

| Option                         | Why rejected (for v0.0.1)                                                                                                        |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| **Publish to crates.io now**   | Append-only/irreversible; premature while the 0.x API and the permanent name commitment are not something we want to lock today. |
| **Private / from-source only** | Kills the public-feedback loop ADR 0004 explicitly wants reopened; contradicts the head-on-competition goal.                     |
| **git-install only**           | No path for users without a Rust toolchain.                                                                                      |
| **binaries only**              | No `cargo install`; library consumers have no dependency path at all.                                                            |

Both channels together cover the dev (Rust) and toolchain-free audiences while
staying fully reversible.

## Consequences

**Positive**

- Fully reversible: no permanent name/version/API commitment; a bad release can be
  edited or deleted, unlike a crates.io version.
- ADR 0004's public-feedback loop reopens immediately, sooner than a
  publish-perfect crates.io gate would allow.
- Library is still consumable by other Rust code via a git dependency.

**Negative**

- No `cargo add mcp-loadtest` for downstream crates — a git dependency is
  second-class (no semver resolution from a registry, no docs.rs).
- Not discoverable through crates.io search — weaker than reaatech's npm presence.
- `--git` installs build from source (slower than a registry binary).
- The hand-rolled `release.yml` workflow must be maintained (no cargo-dist
  niceties: no generated installer scripts or `cargo binstall` metadata).

**Mitigations**

- crates.io remains open via a future ADR once the 0.x API stabilises or a real
  external library consumer asks for it.
- docs.rs gap covered by README + `docs/` + local `cargo doc`.

## Open questions

- **Revisit trigger for crates.io.** Proposed: after the 0.x API stabilises _or_
  the first external library consumer requests a registry dependency.
- **Name-squatting risk.** Deferring leaves `mcp-loadtest` / `mcp-loadtest-cli`
  unclaimed (verified free 2026-05-18). Reserving them needs a placeholder
  publish — itself an append-only act, i.e. the exact commitment this ADR avoids.
  Default: **do not** reserve; accept the squat risk until the real publish.
