# Debugging deadlocks

When `mcp-loadtest deadlock-probe` prints `DEADLOCK DETECTED`, the server
under test stopped responding to a `tools/call` and didn't recover. This page
walks through how to read the report, where to look for the root cause, and
how the canonical example — Vibe-Trading PR #85 — was actually fixed.

## What you'll see

```text
$ mcp-loadtest deadlock-probe \
    --server "python agent/mcp_server.py" \
    --tool analyze_options \
    --concurrent 1 \
    --hang-threshold 2s \
    --grace-period 5s \
    --args '{"spot":450,"strike":460,"expiry_days":30}'

Run 01KR9JX7E4P638TKQM96YA0B4Z
Status: FAIL (1 deadlock)
Server: python agent/mcp_server.py
Scenario: deadlock_probe
Deadlocks: 1   Hangs: 0   Errors: 0

  deadlock detected: tool=analyze_options iter=0 hung_for=7012ms

Report: runs/01KR9JX7E4P638TKQM96YA0B4Z/report.md

Error: DEADLOCK DETECTED — 1 deadlock(s), 0 error(s), 0 threshold violation(s)
```

Three things matter here:

- **`hung_for=7012ms`** — that's exactly `hang_threshold + grace_period`
  (2s + 5s). The probe waited the full window, never got a response, and
  declared the call dead. See [DESIGN.md §15.1](../../DESIGN.md#151-hang-detector)
  for the hang-detector spec.
- **`iter=0`** — the very first call hung. This is the lazy-init fingerprint.
  Start with `--concurrent 1` for this focused diagnosis. With
  `--concurrent N`, N independent sessions finish their handshakes and
  release one call each through a shared gate, so multiple workers can report
  the same first-call failure.
- **`runs/<ulid>/`** — the quick subcommand writes `report.md` and
  machine-readable `metrics.json`. It inherits the server's stderr and does
  not record a trace. Use the config-driven `run` form below when you need
  captured stderr or wire frames.

## Read the stderr first

The quick `deadlock-probe` subcommand inherits the stdio server's stderr. For a
reproducible file, express the same probe in a config and use `run`:

```toml
# deadlock.toml
[server]
command = "python"
args = ["agent/mcp_server.py"]
transport = "stdio"

[scenario]
type = "deadlock_probe"
concurrent = 5
tool = "analyze_options"
args = { spot = 450, strike = 460, expiry_days = 30 }
hang_threshold = "2s"
grace_period = "5s"

[output]
report_dir = "./runs"
formats = ["terminal", "markdown", "json"]
```

```bash
mcp-loadtest run --config deadlock.toml \
  --capture-stderr \
  --trace ./deadlock-trace.jsonl
```

`--capture-stderr` writes the initial session to
`runs/<ulid>/server-stderr.log`; `--tee-stderr` writes the same file and
mirrors it live. Pooled and cold-start sessions each get a non-truncating
artifact at `runs/<ulid>/server-stderr/session-NNNNNN.log`, so inspect those
worker files for concurrency-only failures. These flags apply to `run`, not
to the quick `deadlock-probe` subcommand. Lazy-init bugs usually log
*something* on the way down—an `ImportError`, a thread-stack dump, or a
half-emitted line. Open the captured file:

```bash
$ cat runs/01KR9JX7E4P638TKQM96YA0B4Z/server-stderr.log
INFO:mcp.server:starting on stdio
INFO:fastmcp.tools:registered 12 tools
INFO:fastmcp.session:initialize ok
INFO:fastmcp.session:tools/list ok (12)
INFO:fastmcp.session:tools/call analyze_options
# ...then silence...
```

Stderr just stops. No exception, no `Got SIGTERM` line — the thread is
blocked but the process is alive. That's the lazy-init signature.

## Inspect the trace

`mcp-trace/1` contains a header followed by raw client→server and
server→client JSON-RPC frames. It does **not** contain synthetic `deadlock`
events. Filter the recorded calls and compare their ids:

```bash
jq -r '
  select(.method == "tools/call") |
  {dir, elapsed_ms, message: (.body | fromjson)}
' ./deadlock-trace.jsonl
```

A client-to-server request id with no matching server-to-client response is
the wire-level symptom. The report supplies the `hung_for` classification;
the trace supplies the exact request body. Client-side traces redact
secret-looking keys under `params.arguments` by default, but a server can
still echo sensitive data in its response—treat trace files as artifacts that
may contain secrets.

## Identify the bug class

Three lazy-init patterns account for almost every MCP deadlock seen in the
wild:

1. **Blocking import inside an async worker thread.** This is the
   Vibe-Trading PR #85 shape — `_get_registry()` lazy-loads tools via
   `importlib.import_module("src.tools.shell.*")` on the first call, and the
   import sits behind the asyncio worker's GIL hold. Subsequent calls pile up
   behind it. See [PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85)
   for the exact diff.

2. **Synchronous I/O inside an `async def` handler.** A blocking `requests.get`
   or `subprocess.run` in a coroutine starves the event loop. The first call
   stalls the worker; the second never gets scheduled. Symptom: all calls
   after `iter=0` hang on the same `tool`, not just one.

3. **Cross-task lock acquired before first use.** A `threading.Lock` (or
   `asyncio.Lock` taken from outside the loop) protecting a one-time setup
   path. The setup task and the call task race; the setup task wins, holds
   the lock through a blocking step, and the call task is stuck.

A successful `initialize` + `tools/list` combined with a wedged first
`tools/call` is the giveaway. If `initialize` itself hung, you'd be looking
at a different class of bug.

## How Vibe-Trading PR #85 was actually fixed

The five-line fix that landed in
[PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85) moved the
tool-registry initialization from "first call" to "import time":

```python
# BEFORE — lazy init inside the async worker, deadlocks on first tools/call.
_registry = None

def _get_registry():
    global _registry
    if _registry is None:
        # Blocking importlib inside FastMCP's worker thread.
        _registry = _build_registry_from_imports()
    return _registry

# AFTER — eager init at module import. Deterministic, no worker-thread import.
_REGISTRY = _build_registry_from_imports()

def _get_registry():
    return _REGISTRY
```

PR [#86](https://github.com/HKUDS/Vibe-Trading/pull/86) added a regression
smoke that spawns the server, calls `analyze_options`, and asserts a
response within 5 seconds. `mcp-loadtest` does the same thing at higher
pressure and tighter latency budgets — see
[`crates/engine/tests/vibe_trading_regression.rs`](../../crates/engine/tests/vibe_trading_regression.rs),
which is pinned to commit `71220c7c` (the parent of PR #85) and proves the
bug class is detected against the actual buggy code.

## A worked-example test you can copy

Drop this into `tests/deadlock_smoke.rs` in your own MCP server's test
suite. It's the same pattern the Vibe-Trading regression test uses, minus
the fixture-cloning plumbing.

```rust
use std::time::{Duration, Instant};

use mcp_loadtest::Session;
use mcp_loadtest::metrics::Recorder;
use mcp_loadtest::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest::scenario::{RunContext, Scenario};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn first_tool_call_does_not_deadlock() {
    let mut session = Session::spawn("python", ["-m", "my_mcp"])
        .await
        .expect("spawn failed");

    let probe = DeadlockProbe {
        // A directly-constructed RunContext has no SessionFactory, so this
        // embedded example uses the focused one-session path. `Run::execute`
        // attaches a factory automatically for synchronized N>1 probes.
        concurrent: 1,
        hang_threshold: Duration::from_secs(2),
        grace_period: Duration::from_secs(5),
        tool: "get_market_data".into(),
        args: json!({ "ticker": "AAPL" }),
    };

    let ctx = RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );

    let outcome = probe.drive(&mut session, &ctx).await;

    assert_eq!(
        outcome.deadlock_count, 0,
        "lazy-init deadlock detected — check the first tools/call path. \
         outcome={outcome:?}"
    );
}
```

If this test fails locally, the error message points straight at the
offending tool and the hang duration. You now have a 7-second feedback loop
on a bug class that used to surface as a flaky timeout in production.

## Checklist for fixing a real deadlock

1. **Reproduce.** Run `mcp-loadtest deadlock-probe` against your server with
   the same args your test does. Confirm the deadlock is deterministic.
2. **Capture stderr.** Re-run the config form with `--capture-stderr`, then
   inspect `runs/<ulid>/server-stderr.log` and, for pooled/cold-start runs,
   each file under `runs/<ulid>/server-stderr/`.
3. **Find the wedged request.** Re-run with `--trace <file>`, filter frames
   whose `.method == "tools/call"`, and find the request id without a response.
4. **Inspect the handler.** For the tool in the wedged call, walk the code
   path from "JSON-RPC arrives" to "response sent." Look for the three
   patterns above (lazy import, sync I/O in async, cross-task lock).
5. **Eager-init or `asyncio.to_thread`.** Most fixes are one of these:
   - Lift the lazy initialization out of the worker thread (Vibe-Trading PR #85).
   - Wrap the blocking call in `asyncio.to_thread(...)`.
   - Move the lock acquisition out of the hot path; pre-build the resource.
6. **Lock in a regression test.** Drop the test above into your suite. It
   takes ~7 seconds and gates your CI against the bug coming back.

## See also

- [`docs/examples/ci-integration.md`](ci-integration.md) — wire
  `deadlock-probe` into GitHub Actions.
- [DESIGN.md §15.2](../../DESIGN.md#152-deadlock-probe-scenario) — the
  probe algorithm spec.
- [DESIGN.md §18](../../DESIGN.md#18-error-taxonomy) — how `Hang`,
  `Deadlock`, `Timeout`, and `Crash` are classified differently.
- [`crates/engine/tests/vibe_trading_regression.rs`](../../crates/engine/tests/vibe_trading_regression.rs)
  — the worked example pinned to the actual buggy commit.
