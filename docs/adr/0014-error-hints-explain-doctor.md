# 0014. Error hints, `--explain`, and `doctor` (AI-friendliness §21)

Date: 2026-05-17
Status: Accepted

## Context

DESIGN.md §21 ("AI-friendliness") is a first-release pillar: `mcp-loadtest` is a tool
that AI agents both operate and are operated by, so its failure modes,
self-description, and setup diagnostics must be machine-actionable, not just
human-readable. Three §21 items were still unimplemented going into the
pre-publish window:

- **§21.3 actionable errors** — every `Err` reaching the user should suggest a
  concrete next step (`BrokenPipe(Os { code: 32, .. })` → "server closed the
  pipe — capture it with `--capture-stderr`"). LLMs (and humans) act on the
  former and bounce off the latter.
- **§21.4 `--explain`** — every subcommand answers `--explain` with a static
  algorithm/knobs description so an agent plans the right invocation instead
  of trial-and-error.
- **§21.6 `doctor`** — a ✅/❌ checklist of common setup problems with a
  one-line fix per ❌, chainable into an agent fix-it loop.

Constraints (project conventions): the **library** error enums are `thiserror` types
locked across `0.x` and `#[non_exhaustive]`; production files stay < 300
lines; this CLI crate `forbid`s `unsafe`; async paths use `tokio` I/O.

## Decision

**Hints live in the CLI crate, at the `anyhow` boundary — not in the
library.** A `hints::ErrorHint` trait (`fn hint(&self) -> Option<&'static
str>`) is implemented for `RunError` / `SessionError` / `TransportError` /
`ConfigError` / `ReportError`. Each `match` maps variants to a stable
`&'static str` mirroring §21.3 wording; wrapper variants (`RunError::Session`,
`SessionError::Transport`) **delegate inward** so the most-specific advice
wins. F1's SSRF rejection arrives as `TransportError::Other(msg)` with a
stable `"blocked host"` substring (and an "ADR 0012" cite); the hint
substring-matches it rather than adding an enum variant (the enum is locked
and `Other` is the agreed carrier — see ADR 0012). `print_with_hint(&anyhow::
Error)` prints the error + its `.chain()` to stderr and appends exactly one
`Hint:` line — the first match while walking the chain outermost-first.
Keeping this out of the library preserves the clean typed-error contract for
embedding consumers (§21.1) while still giving CLI users the UX prose.

**`--explain` is a pre-clap `std::env::args()` scan**, not a clap flag the
dispatcher reads. `run --config` and `cross --server` have _required_ args;
clap rejects the parse for a missing required arg before any handler (or a
subcommand-local `--explain`) could run. So `main` calls
`explain::maybe_handle_explain()` as its **first statement**, before
`Cli::parse()`: it finds the first non-flag arg (the subcommand), prints that
subcommand's static text to stdout, and returns `true` (caller exits 0). A
`#[arg(long, global = true)] explain` is still registered on the clap struct
purely so `--help` advertises it and the normal path tolerates a stray
`--explain`. The `deadlock-probe` text is copied **verbatim** from DESIGN
§21.4 (snapshot-stable per §21.9); other subcommands get a consistent "What it
does / Algorithm / Tunable knobs" block; an unknown/absent subcommand prints a
general overview. Always exit 0.

**`doctor` runs four best-effort checks and exits non-zero on any ❌ via a
_bare_ `anyhow::anyhow!` message.** `run_doctor(server: Option<String>,
runs_dir: PathBuf)` collects four `CheckResult { name, ok, detail, fix }` —
all run (no early return) so one ❌ never masks another:

1. **python** — `<MCP_LOADTEST_PYTHON|python> --version` via `tokio::process`.
2. **server initialize smoke** (only with `--server`) — `split_server_command`
   → `Session::spawn_with(.., SpawnOptions::capture_stderr(tmp))` (reuses F2)
   under a 12s `timeout`; on failure the last 20 lines of captured stderr are
   echoed (§21.3). Temp capture file is unique per-pid and cleaned up.
3. **stale runs/** — `tokio::fs::read_dir`, **bounded** to one level of run
   dirs + one level of files each; ❌ if > 50 dirs or > 500 MiB. A missing
   dir is "no runs yet" (pass).
4. **windows toolchain** — `#[cfg(windows)]` only (else n/a pass); compares
   this binary's `cfg!(target_env)` against `rustc -vV`'s `host:` line; ❌
   _only_ on a confident `gnu`-vs-`msvc` mismatch, any uncertainty → "could
   not determine" pass (a diagnostic must not fail on its own inability).

The summary error is deliberately a bare `anyhow!("doctor: N check(s)
failed")` with **no typed source**, so `print_with_hint`'s downcast finds
nothing and adds no spurious `Hint:` over the checklist — the per-❌ `fix:`
lines are the actionable advice.

`cmd_doctor` is split into `cmd_doctor/{mod,python,server,runs,toolchain}.rs`
to stay under the 300-line cap and keep each check independently
unit-testable.

## Alternatives considered

- **Hints as `Display`/`Error` prose in the library enums.** Rejected: pins
  UX strings into a locked `0.x` API surface and pollutes the clean typed
  errors that embedding consumers (§21.1) rely on. The `anyhow` boundary in
  the CLI is the right seam.
- **A library `ErrorHint` trait the CLI re-uses.** Rejected for the same
  reason — it would still live under the library stability guarantee.
- **Subcommand-local clap `--explain` (e.g. read in the dispatch arm).**
  Rejected: unreachable for `run`/`cross` because clap enforces their required
  args _before_ dispatch. The pre-clap scan is the only thing that makes
  `run --explain` (no `--config`) work.
- **A clap `ArgGroup`/conflicts trick to make required args optional under
  `--explain`.** Rejected: convoluted, fragile across clap upgrades, and still
  forces every subcommand to special-case it; the env scan is ~10 lines and
  subcommand-agnostic.
- **`doctor` returns a typed error / first ❌'s typed cause.** Rejected: that
  would make `print_with_hint` bolt an unrelated `Hint:` onto the summary. A
  bare message keeps the checklist the single source of remediation.
- **`doctor` early-returns on the first ❌.** Rejected: an agent fixing setup
  wants the _whole_ failing picture in one pass, not one error at a time.
- **Recursive `runs/` sizing.** Rejected: a pathological tree could stall a
  diagnostic; one level is enough (run dirs are flat) and bounded by design.

## Consequences

- New CLI-crate public surface (CHANGELOG **Added**):
  `cmd_doctor::run_doctor` / `cmd_doctor::CheckResult`,
  `explain::maybe_handle_explain`, `hints::ErrorHint` (+ impls for the five
  error types) and `hints::print_with_hint`. These are CLI-crate items; the
  **library** API is unchanged by Feature 3.
- New CLI surface in the binary: the `doctor` subcommand, the global
  `--explain` flag, and `run --capture-stderr` / `--tee-stderr` (the latter
  two forward into F2's `Run::with_stderr_capture`).
- Every error reaching the user now prints through `print_with_hint` (source
  chain + at most one `Hint:`), and the process exits explicitly non-zero
  rather than via the default `Debug`-printing `Termination` impl.
- `--explain` output for `deadlock-probe` is byte-locked to DESIGN §21.4;
  changing either without the other is a contract drift (guarded by a unit
  test asserting the §21.4 landmarks).
- Open question (deferred): the non-`deadlock-probe` `--explain` texts are
  hand-written and not yet snapshot-tested (§21.9); a future change could add
  `insta` snapshots for all subcommand explanations. Not blocking for the first release.
- Testing note: the CLI crate has no `assert_cmd` and `forbid`s `unsafe`, so
  the "`run --explain` exits 0 without `--config`" contract is proven by a
  manual binary smoke (recorded in the Feature 3 report) rather than an
  in-crate integration test; the pure `explanation_for` logic and the env-scan
  no-op path are unit/integration tested.
