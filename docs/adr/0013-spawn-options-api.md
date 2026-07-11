# 0013. SpawnOptions API for stdio stderr capture

Date: 2026-05-17
Status: Accepted

## Context

When `mcp-loadtest` spawns a stdio MCP server it has, since M1, inherited the
child's stderr (`Stdio::inherit()`) so a panicking server is visible to whoever
ran the load test. That is the right default for a terminal, but it has a real
footgun: when `mcp-loadtest` itself runs as a child of an MCP-aware agent
(Claude Code, Cursor, …), the server-under-test's stderr blends into the
agent's own output stream — noisy, interleaved, and impossible to attribute to
a specific run after the fact. The original `stdio.rs` docstring tracked this
as "deferred to a later release: a `--capture-stderr` flag".

We are folding it into the first release instead. The reason is timing, not scope creep:
threading stderr disposition through the spawn path touches the public
`StdioTransport::spawn` / `Session::spawn` signatures (it needs an options
argument and an `async` body to open the capture file / start a pump task).
Doing it pre-publish absorbs the breaking part into the initial release with no
released version to break; doing it after the first release would force either
a wart (a second `spawn_with` that the 2-arg form can't share) shipped under a
stability guarantee, or a breaking bump.

Requirements: keep the documented 2-arg `Session::spawn(cmd, args)` working
unchanged (integration tests and the README example depend on it); add a way to
(a) capture the server's stderr to a per-run file and (b) additionally mirror
it live to the parent's stderr ("tee"); keep the capture machinery off the hot
path entirely when unused; obey the project rules (no unbounded `tokio::spawn`,
no blocking I/O in async paths, no `panic!`/`expect` in lib code, production
files < 300 lines).

## Decision

Introduce an extensible options struct rather than overloaded constructors:

- `StderrMode { Inherit (default), CaptureToFile(PathBuf), TeeToFile(PathBuf) }`
  and `SpawnOptions { stderr: StderrMode }` (`#[non_exhaustive]`, `Default`,
  ctors `inherit()` / `capture_stderr(p)` / `tee_stderr(p)`). Re-exported as
  `mcp_loadtest::{SpawnOptions, StderrMode}`. `#[non_exhaustive]` so future
  spawn knobs (env scrubbing, stdout capture, …) are additive.
- New `StdioTransport::spawn_with(cmd, args, &SpawnOptions)` and
  `Session::spawn_with(cmd, args, SpawnOptions)`. The existing
  `spawn(cmd, args)` on both **delegates** to `spawn_with(.., default())`, so
  the 2-arg public API is byte-for-byte source-compatible.
- `StdioTransport::spawn` becomes `async` (it must `await` the capture-file
  open and pump spawn). This is a breaking change **only for that function**;
  its only callers are `Session::spawn`/`spawn_with`, both already `async`, so
  no observable change to the documented `Session::spawn` API.
- Capture/tee is a dedicated background task (`stderr_pump`) that reads the
  child's piped stderr line-by-line into the file (and, for tee, to
  `tokio::io::stderr()`). It is **cancellation-aware and JoinHandle-tracked**,
  exactly mirroring the existing `ws`/`sse` reader-task lifecycle: the
  `JoinHandle` + a `CancellationToken` are stored on `StdioTransport`;
  `shutdown` cancels then awaits it (bounded 2s) so the file is **flushed
  before every exit path** (EOF / cancel / read-error each flush before
  breaking, or the last buffered line is lost); `Drop` cancels + `abort`s as
  the backstop when a caller skips `shutdown`. All pump I/O is best-effort
  (write errors on a torn-down pipe are swallowed so they cannot poison
  teardown).
- `Inherit` (the default, and every existing caller) starts **no pump** and
  uses `Stdio::inherit()` — zero added overhead on the unchanged path.
- `Run` gains `StderrCapture { Off (default), Capture, Tee }`, a
  `#[must_use] with_stderr_capture(self, _)` builder (the 3-arg `Run::new`
  signature is unchanged), and resolves the capture path to
  `runs/<id>/server-stderr.log` (the run dir already exists by then). The CLI
  `run --capture-stderr` / `--tee-stderr` flags map onto it; `tee` wins if both
  are passed.
- The pump lives in its own file `stderr_pump.rs`, declared a **private child
  module of `stdio`** via `#[path = "stderr_pump.rs"] mod stderr_pump;` inside
  `stdio.rs`. This keeps `stdio.rs` under the 300-line production cap without
  requiring a new `pub mod` line in `transport/mod.rs` (owned by a different
  workstream); the pump stays an implementation detail, not public surface.
- To consume `self` in `shutdown` while `StdioTransport` now implements `Drop`
  (a `Drop` type cannot be destructured field-by-field), `child` and `stdin`
  are stored as `Option` and `take`-n in `shutdown`; the hot `request`/`notify`
  path borrows them through a helper that maps the unreachable `None` to
  `TransportError::Closed` (no `expect`, honouring the no-panic-in-lib rule).

## Alternatives considered

- **Overloaded constructors / a second public `spawn2`.** Rejected:
  `#[non_exhaustive] SpawnOptions` is the idiomatic forward-compatible knob;
  more spawn options are coming and we don't want a constructor per
  combination, nor a permanently-awkward name under a stability guarantee.
- **Keep deferring to a later release.** Rejected on timing: the `async`-ification of
  `StdioTransport::spawn` is breaking; pre-publish it is free, post-publish it
  costs a breaking bump or a permanent wart.
- **Synchronous pump on a blocking thread (`std::thread` + blocking reads).**
  Rejected: violates the "no blocking I/O in async paths / no untracked
  threads" rule and complicates cancellation; a `tokio` task with `select!` on
  the cancel token is the established pattern here (`ws`/`sse`).
- **`tokio_util::task::AbortOnDropHandle`.** Available in the pinned
  tokio-util, but it abandons the final flush on drop and diverges from the
  `ws`/`sse` `Option<JoinHandle>` + explicit-cancel pattern; consistency and a
  guaranteed graceful flush on `shutdown` won.
- **A new `TransportError` variant for a missing pipe.** Rejected: the enum is
  `#[non_exhaustive]` and locked; the existing `Other(String)` / `Closed`
  variants already express "no stderr handle" and "stdin gone".

## Consequences

- `mcp_loadtest::{SpawnOptions, StderrMode, StderrCapture}` and
  `Session::spawn_with` / `StdioTransport::spawn_with` /
  `Run::with_stderr_capture` are new public surface (CHANGELOG **Added**).
- `StdioTransport::spawn` is now `async` (CHANGELOG **Changed**, breaking,
  absorbed pre-publish). `Session::spawn(cmd, args)` is **unchanged** — the
  documented entry point and existing integration tests stay green.
- `--capture-stderr` / `--tee-stderr` are **silent no-ops for http/sse/ws**:
  those transports have no child process. Accepted and documented; out of scope
  to invent a synthetic capture for network transports.
- Capture/tee adds one tracked, cancellation-aware task per stdio run. The
  inherit default path is unchanged and pays nothing.
- Open question (deferred): no rotation / size cap on `server-stderr.log` — a
  pathological server that streams unbounded stderr can grow the file without
  limit. The stdout side is already OOM-guarded (`MAX_LINE_BYTES`); a stderr
  size cap is a reasonable follow-up but is not a load-tester-OOM vector
  (it streams to disk, not memory).
