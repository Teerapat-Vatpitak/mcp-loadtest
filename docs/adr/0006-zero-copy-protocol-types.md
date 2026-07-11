# 6. Zero-copy protocol types on the request hot path

Date: 2026-05-11
Status: Accepted

## Context

The pre-publish hot-path performance audit (commit `c8dee52`) found that every `Session::call_tool` was allocating **six times per call**. The chain of waste:

- `OutgoingRequest` held `method: String` and `params: Value`. Each call ran `method.to_owned()` and `serde_json::to_value(params)` to materialize an intermediate `Value` tree, then re-serialized the whole tree to a `Vec<u8>` for the framer.
- `OutgoingNotification` had the same shape — owned `method`, owned `params`.
- `CallToolParams` cloned the tool name + argument map for every invocation.
- Scenarios called `args.clone()` on every iteration before passing it down.

At > 1K iter/s these allocations dominate the driver's own latency budget and contaminate the latency measurements we hand back to the user. The whole point of writing this in Rust (ADR 0001) is to keep the driver's overhead well under the system under test — these allocations were undermining that goal.

## Decision

Refactor the three protocol types to borrow:

- `OutgoingRequest<'a, P: ?Sized + Serialize>` with `method: &'a str` and `params: &'a P`.
- `OutgoingNotification<'a, P: ?Sized + Serialize>` with the same shape.
- `CallToolParams<'a>` with borrowed name + borrowed arguments.

`Session::call_tool` now takes `(&str, &Value)` instead of `(&str, Value)`. Scenarios pass `&self.args` directly — no clone. The framer serializes straight from the borrowed view to a `Vec<u8>`, skipping the intermediate `Value` materialization entirely.

## Alternatives considered

| Approach | Why rejected |
|---|---|
| **Keep owned types, add a `&` wrapper** | Doesn't fix the root cause — `Value` is still materialized. |
| **`Cow<'a, str>` everywhere** | Sum-type overhead and a worse API; we control all callers, so a hard borrow is fine. |
| **Cache the serialized request bytes per tool** | Premature; `params` varies per call, so the cache key is the whole serialized form. Would help only the constant-args case. |

## Consequences

**Positive:**
- Six allocations per `call_tool` → zero on the steady-state hot path.
- Removes a category of clones from scenario code; reviewers will catch a regression because the borrow checker enforces it.
- Documented because a well-meaning "refactor back to owned for ergonomics" would silently regress every scenario.

**Negative:**
- Breaking API change — but applied pre-publish, so the cost is zero. Future contributors must keep these types borrowed.
- Lifetime parameter `'a` leaks into a couple of public types. Acceptable for a perf-tier API.

**Open:**
- Whether to expose a `to_owned()` companion for users who want to stash a request and replay it. Defer until requested.
