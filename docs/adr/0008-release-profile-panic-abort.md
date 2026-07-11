# 8. Release profile: `panic = "abort"`

Date: 2026-05-11
Status: Accepted

## Context

`mcp-loadtest` ships a single-binary CLI plus a library half (`crates/mcp-loadtest`). The pre-publish profile audit (commit `dc8287c`) found that `[profile.release]` lacked `panic = "abort"`. Unwinding is the cargo default and is appropriate for libraries that have callers expecting `catch_unwind` to work — but:

- The library half uses `Result<T, RunError>` throughout. There is no `panic!()` outside tests (a project convention).
- Every panic path that *does* exist is either a bug (assertion violation) or an OS-level fault (allocator OOM). Neither is a recoverable condition.
- The CLI binary has no caller that meaningfully recovers — a panic during a load test invalidates the run, and the right answer is to exit non-zero so the operator notices.
- Unwinding bloats the binary: each `Result<T, E>` path that crosses an unwind boundary pulls in landing-pad metadata, and `core::panicking` shows up in the symbol table.

## Decision

Add `panic = "abort"` to `[profile.release]` in the workspace `Cargo.toml`. Keep `[profile.dev]`, `[profile.test]`, and `[profile.bench]` on unwinding so panics surface as test failures with stack traces during development.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep unwind in release for future panic-safety guarantees** | We don't make panic-safety promises in the library API. Reserving the right to add them later doesn't justify the binary-size cost today. |
| **Strip + LTO without `panic = "abort"`** | LTO and strip were already applied; the 600 KB win is specifically from removing unwind tables. The two are complementary, not alternatives. |
| **`panic = "abort"` everywhere including tests** | Loses panic-as-test-failure ergonomics; `cargo test` becomes harder to debug. Net loss for the dev loop. |

## Consequences

**Positive:**
- Stripped release binary shrunk **5.7 MB → 5.1 MB (-600 KB / -10.5%)**. Material at `cargo install` distribution scale.
- Faster startup (no unwind landing-pad initialization).
- Simpler crash semantics: a release-build panic is a hard abort, not a possibly-recovered-from condition.

**Negative:**
- `std::panic::catch_unwind` is unavailable in release builds. Downstream library users who wrap our crate in a panic-safe boundary lose the unwind path.
- We can't ever add a "scenario panic was caught and run continued" feature in release without reverting this.

**Open:**
- None. The trade-off favors binary size and crash clarity over a use case (`catch_unwind` around our lib) we don't support anyway.
