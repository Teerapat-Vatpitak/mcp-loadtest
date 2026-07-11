# Writing a custom scenario

The built-in scenarios cover most workloads, but eventually you'll need
something specific — say, a "warm cache then hammer it" probe, or a tool that
needs a multi-step setup before each call. `mcp-loadtest` ships exactly one
extension point: implement `trait Scenario` and register it.

This walkthrough builds a `CacheWarmup` scenario from scratch using
`DeadlockProbe` as a reference. Every line maps to real code in
[`crates/mcp-loadtest/src/scenario/deadlock_probe.rs`](../../crates/mcp-loadtest/src/scenario/deadlock_probe.rs).

## The trait

`Scenario` is defined in
[`crates/mcp-loadtest/src/scenario/mod.rs`](../../crates/mcp-loadtest/src/scenario/mod.rs):

```rust
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Drive the scenario until completion or until `ctx.cancel_token` fires.
    /// Records per-call metrics via `ctx.metrics`. Must observe cancellation.
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome;

    /// JSON Schema fragment for the TOML config block. Used by
    /// `mcp-loadtest example-config`.
    fn config_schema(&self) -> serde_json::Value;

    /// Short, stable identifier for logs / reports / CLI.
    fn name(&self) -> &'static str;
}
```

Three methods, one of which is async, none of which leak internals.
`ScenarioOutcome` is the structured tally the orchestrator turns into a
report:

```rust
pub struct ScenarioOutcome {
    pub total_calls: u64,
    pub successful_calls: u64,
    pub hang_count: u32,
    pub deadlock_count: u32,
    pub error_count: u64,
    pub notes: Vec<String>,
}
```

## Step 1. Write the scenario

Create `crates/mcp-loadtest/src/scenario/cache_warmup.rs`. The scenario fires
one warmup call (e.g. populating a cache the server lazy-loads), then issues
`concurrent` more calls and measures only those. A regression here usually
means the warmup didn't actually warm anything.

```rust
//! `cache_warmup` scenario — measure steady-state latency after a single
//! warmup call. Useful for servers with a lazy-init cache where the first
//! call is misleadingly slow.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::Session;
use crate::hang_detector::{HangOutcome, hang_detect};
use crate::metrics::CallOutcome;
use crate::scenario::{RunContext, Scenario, ScenarioOutcome};

pub struct CacheWarmup {
    /// How many measured calls to issue after the warmup.
    pub iterations: u32,
    /// Per-call hang threshold.
    pub hang_threshold: Duration,
    /// Grace period after threshold before classifying as deadlock.
    pub grace_period: Duration,
    /// Tool to invoke (used for warmup + measurement).
    pub tool: String,
    /// Arguments passed on every call.
    pub args: Value,
}

#[async_trait]
impl Scenario for CacheWarmup {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();

        // 1. Warmup call — not recorded in the histogram.
        let warmup = session.call_tool(&self.tool, self.args.clone());
        match hang_detect(warmup, self.hang_threshold, self.grace_period).await {
            HangOutcome::Ok { .. } => {
                outcome.notes.push("warmup ok".into());
            }
            HangOutcome::Deadlock { hung_for } => {
                // A deadlock on the warmup is a real bug — surface it.
                outcome.deadlock_count += 1;
                outcome.notes.push(format!(
                    "deadlock during warmup: hung_for={}ms",
                    hung_for.as_millis()
                ));
                return outcome;
            }
            HangOutcome::Slow { duration, .. } => {
                outcome.notes.push(format!(
                    "warmup slow: {}ms (continuing)",
                    duration.as_millis()
                ));
            }
            HangOutcome::Err(e) => {
                outcome.error_count += 1;
                outcome.notes.push(format!("warmup error: {e}"));
                return outcome;
            }
        }

        // 2. Measurement loop. Observe cancellation; record into ctx.metrics.
        for iter in 0..self.iterations {
            if ctx.is_cancelled() {
                outcome.notes.push(format!("cancelled at iter={iter}"));
                break;
            }

            let call_fut = session.call_tool(&self.tool, self.args.clone());
            let result = hang_detect(call_fut, self.hang_threshold, self.grace_period).await;
            outcome.total_calls += 1;

            match result {
                HangOutcome::Ok { duration, .. } => {
                    outcome.successful_calls += 1;
                    ctx.metrics.record(duration, CallOutcome::Success);
                }
                HangOutcome::Slow { duration, .. } => {
                    outcome.hang_count += 1;
                    ctx.metrics.record(duration, CallOutcome::Hang);
                }
                HangOutcome::Deadlock { hung_for } => {
                    outcome.deadlock_count += 1;
                    ctx.metrics.record(hung_for, CallOutcome::Deadlock);
                    outcome
                        .notes
                        .push(format!("deadlock at iter={iter} hung_for={}ms", hung_for.as_millis()));
                    // Session is wedged; further calls will hang too.
                    break;
                }
                HangOutcome::Err(e) => {
                    outcome.error_count += 1;
                    ctx.metrics.record(Duration::ZERO, CallOutcome::ServerError);
                    outcome.notes.push(format!("error at iter={iter}: {e}"));
                }
            }
        }

        outcome
    }

    fn config_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "iterations":     { "type": "integer", "minimum": 1, "default": 50 },
                "hang_threshold": { "type": "string",  "default": "5s" },
                "grace_period":   { "type": "string",  "default": "10s" },
                "tool":           { "type": "string" },
                "args":           { "type": "object" }
            },
            "required": ["tool"]
        })
    }

    fn name(&self) -> &'static str {
        "cache_warmup"
    }
}
```

A few patterns worth noting — all of them project conventions every built-in
scenario follows:

- **Always observe `ctx.cancel_token`** in the hot loop. Without this, `Ctrl-C`
  or a timeout in `Run` can't shut you down cleanly.
- **Wrap every call in `hang_detect`.** It's the per-call watchdog that turns
  "hang" into a structured outcome instead of a stalled task. Even if your
  scenario isn't about deadlocks, you get them for free.
- **Record into `ctx.metrics`, not your own collector.** The recorder is
  lock-free per-worker; the orchestrator merges shards at the end.
- **Bail on the first deadlock.** The session is wedged after a hang — the
  hung request still owns stdin/stdout. More calls will just hang too.

## Step 2. Register the module

In `crates/mcp-loadtest/src/scenario/mod.rs`, add the module next to the
existing ones:

```rust
pub mod cold_start;
pub mod deadlock_probe;
pub mod pattern;
pub mod race_check;
pub mod ramp;
pub mod soak;
pub mod sustained;
pub mod cache_warmup;   // ← new
```

## Step 3. Wire it into the CLI

`crates/mcp-loadtest-cli/src/cmd_run/builder.rs` has a
`build_scenario(kind, params)` dispatch (param helpers like `required_str` /
`parse_dur_field` live in the sibling `cmd_run/params.rs`). Add a branch:

```rust
"cache_warmup" => {
    let iterations = params
        .get("iterations")
        .and_then(Value::as_u64)
        .unwrap_or(50) as u32;
    let hang_threshold =
        parse_dur_field(params.get("hang_threshold"), Duration::from_secs(5))?;
    let grace_period =
        parse_dur_field(params.get("grace_period"), Duration::from_secs(10))?;
    let tool = required_str(params, "tool")?;
    let args = params.get("args").cloned().unwrap_or(json!({}));
    Ok(Box::new(CacheWarmup {
        iterations,
        hang_threshold,
        grace_period,
        tool,
        args,
    }))
}
```

Update `list-scenarios` and `DESIGN.md §8` while you're there — both are
discovery surfaces.

## Step 4. Drive it from TOML

```toml
[server]
command = "python"
args = ["-m", "my_mcp"]

[scenario]
type = "cache_warmup"
iterations = 100
hang_threshold = "2s"
grace_period = "5s"
tool = "get_market_data"
args = { ticker = "AAPL" }

[thresholds]
p99_latency = "200ms"   # measured *after* warmup, so this should be tight
hang_timeout = "5s"

[output]
report_dir = "./runs"
formats = ["terminal", "markdown", "json"]
```

```bash
$ mcp-loadtest run --config cache_warmup.toml
Run 01KR9KAR8X6XSWQ4HZ7M0V5K3B
Status: PASS
Scenario: cache_warmup
Latency  p50=4.1ms  p95=8.7ms  p99=14.2ms  p999=29.0ms
```

If your p99 is suspiciously close to your cold-call latency, the warmup isn't
warming anything — that's the signal `cache_warmup` is built to surface.

## Step 5. Drive it from Rust

The library API is identical to what the CLI does:

```rust
use std::time::Duration;
use mcp_loadtest::Session;
use mcp_loadtest::scenario::Scenario;
// use mcp_loadtest::scenario::cache_warmup::CacheWarmup;  // your new scenario
use serde_json::json;

#[tokio::test]
async fn cache_warmup_meets_steady_state_p99() {
    let mut session = Session::spawn("python", ["-m", "my_mcp"]).await.unwrap();
    let scenario = CacheWarmup {
        iterations: 100,
        hang_threshold: Duration::from_secs(2),
        grace_period: Duration::from_secs(5),
        tool: "get_market_data".into(),
        args: json!({ "ticker": "AAPL" }),
    };
    // Build a RunContext (see RunContext::new helpers in mod.rs).
    let ctx = /* ... */;
    let outcome = scenario.drive(&mut session, &ctx).await;
    assert_eq!(outcome.deadlock_count, 0);
    assert!(outcome.successful_calls >= 95, "{outcome:?}");
}
```

## Required tests

Every scenario in this repo ships with three tests (project convention):

1. **Happy path** against `mock-normal.py`.
2. **Pathological path** against `mock-broken.py` or whichever mock exercises
   the failure mode the scenario detects.
3. **Snapshot test** for the report markdown using `insta::assert_snapshot!`.

See [`crates/engine/tests/deadlock.rs`](../../crates/engine/tests/deadlock.rs)
for the canonical example.

## Where to look next

- [`crates/engine/src/scenario/deadlock_probe.rs`](../../crates/engine/src/scenario/deadlock_probe.rs)
  — the reference implementation this walkthrough mirrors line-for-line.
- [DESIGN.md §15](../../DESIGN.md#15-algorithm-specs) — algorithm specs for
  `hang_detect`, `deadlock_probe`, and the leak detector.
