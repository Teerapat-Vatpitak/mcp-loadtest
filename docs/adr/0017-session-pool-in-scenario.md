# 0017. Session pool inside the scenario (real `concurrent`, M8)

Date: 2026-06-12
Status: Accepted

**Implementation annotation (2026-07-28):** the internal pool is now shared
by single-tool and multi-pattern sustained workloads. `deadlock_probe` and
`race_check` use it to complete independent sessions before a synchronized
one-call-per-worker start gate. For those correctness checks, a direct-library
context without `SessionFactory` rejects N>1 rather than silently falling back
to sequential execution.

## Context

Since M2 every scenario's `concurrent` knob has been **declared, not real**:
`Scenario::drive(&self, &mut Session, &RunContext)` hands the scenario one
borrowed session, `Session::call_tool` takes `&mut self`, so all calls
serialize on the borrow. Each scenario disclosed this honestly in its outcome
notes ("runs sequentially on one session; concurrent=N is recorded but not
multiplexed"), and DESIGN §8's N-worker intent stayed backlog (M8+).

M8's prerequisite landed earlier: `RunContext` now carries an optional
[`SessionFactory`](../../crates/engine/src/run/factory.rs) — a
cloneable handle that spawns a fresh, handshake-complete `Session` for any of
the four transports. `cold_start` already consumes it. The question: how do
we turn `concurrent` into real N-client load **without changing the locked
`Scenario` trait**?

## Decision

Pool **inside the scenario**, over factory-spawned sessions, with a graceful
sequential fallback:

1. A shared `pub(crate)` helper, `scenario::pool::drive_pooled(ctx, n,
per_worker)`, spawns `n` fresh sessions through `ctx.session_factory`
   concurrently. Once a bounded constructor starts, cancellation is observed
   after it completes so a half-constructed stdio child is never abandoned to
   Drop-only termination. The helper then runs one tokio
   task per successfully-spawned session executing the caller-supplied worker
   loop, joins **every** handle via `JoinSet`, and merges the per-worker
   `ScenarioOutcome`s (counters summed, `hung_for_ms` appended, notes
   prefixed `worker N:`). A summary note — `pool: N workers (M requested)` —
   always discloses the real pool size.
2. Spawn-failure policy: partial failures are counted (`error_count` +1 each,
   note per failure) and the pool proceeds with the survivors; if **all**
   spawns fail the outcome carries `error_count == requested` plus an
   explanatory note — never a panic.
3. `Sustained::drive` takes the pooled path when `ctx.session_factory` is
   `Some` **and** `concurrent > 1`; each worker runs the _same_ loop body as
   the sequential path (identical `classify_error` / `is_terminal_error`
   semantics). The borrowed `&mut Session` stays idle there — it cannot move
   into worker tasks, and a borrowed "worker 0" special case would force a
   heterogeneous loop for no measurement gain. Without a factory (bare
   `RunContext::new`, e.g. direct library use) or with `concurrent <= 1`, the
   sequential loop runs unchanged, with its honest disclosure note.
4. Scope of this slice: `sustained` (single tool+args form) only.
   Multi-pattern sustained (`PatternScenario`), `ramp`, and `spike` adopt the
   same helper in the next commit.

## Alternatives considered

- **Change the `Scenario` trait to take a pool** (e.g. `drive(&self, &mut
SessionPool, &RunContext)`). Rejected: the trait is locked and public —
  this breaks every external `Scenario` impl, and the fallback story is
  worse (a pool type must then fake a one-session pool for bare contexts,
  where today the single borrowed session is the natural degraded mode).
- **Orchestrator-transparent pooling** — `Run::execute` spawns N sessions,
  drives N copies of the same scenario, merges the reports. Rejected: it
  double-counts scenario-internal pacing (each ramp copy would re-run every
  ramp step at full step size; each spike copy its own baseline/peak phases)
  and hides the concurrency from exactly the component that must pace it.
- **In-flight pipelining on one session** (N concurrent JSON-RPC ids over one
  connection). Rejected _for now_: `Session` is deliberately synchronous
  (M1 scope — one in-flight id, `&mut self`), and every transport would need
  response correlation. Also measures a different thing (one multiplexed
  client) than DESIGN §8 intends (N independent clients). Future work.

## Consequences

- **Makes easy:** RPS / latency under `sustained --concurrent N` are now a
  genuine N-client rate (observed: 4 workers completed 12 calls against a
  2s-per-call server in 6s, vs. 3 sequentially); ramp/spike can reuse the
  helper as-is; the honest-notes contract is preserved on every path.
- **Fail-closed limitation — process sampling:** the process sampler watches
  only the **original** server process. For stdio, each pooled session spawns
  its **own** child, so the report clears the irrelevant initial-child sample
  and configured RSS/leak gates emit an unavailable-evidence violation
  whenever factory sessions were attempted. Future work: aggregate sampling
  across pool children (factory would need to surface child PIDs). For network
  transports (http/sse/ws), process metrics are unavailable unless a future
  remote sampler is added, and configured process gates already fail closed.
- **Cost commitment:** the pooled path pays N spawn+handshakes at scenario
  start, _inside_ the configured duration (the deadline is anchored before
  the spawn phase, mirroring when the sequential clock starts). For stdio
  that is N child processes alive for the run.
- **Honest shortfall:** when spawns partially fail the run continues smaller
  and says so — thresholds on error rate will see one error per failed spawn.
