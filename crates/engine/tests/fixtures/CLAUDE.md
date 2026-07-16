# Mock MCP servers

Each mock is a single Python file < 50 lines. Uses stdlib only — **no `fastmcp` or other MCP SDK** (avoid version coupling with the thing under test).

## Template

See `_common.py` for `read_frame()` / `write_frame()` / `respond_*()` helpers.

```python
#!/usr/bin/env python3
"""mock-XXX: <one-line description of behavior>"""
import sys, json, time
from _common import read_frame, write_frame, respond_initialize, respond_tools_list, respond_ok

def main():
    while True:
        msg = read_frame()
        if msg is None: break  # stdin closed
        method = msg.get("method")
        if method == "initialize":
            respond_initialize(msg["id"])
        elif method == "tools/list":
            respond_tools_list(msg["id"], tools=[
                {"name": "echo", "inputSchema": {"type": "object"}},
            ])
        elif method == "tools/call":
            # YOUR BEHAVIOR HERE
            args = msg["params"]["arguments"]
            respond_ok(msg["id"], {"content":[{"type":"text","text": json.dumps(args)}]})

if __name__ == "__main__":
    main()
```

## Conventions

- Filename: `mock-<descriptor>.py` (lowercase, hyphenated).
- First-line docstring: one sentence, present-tense ("Hangs on first tools/call.").
- < 50 lines (split helpers into `_common.py`).
- Stdlib only. No `pip install` dependencies.
- Exit cleanly on stdin EOF — no zombie processes.

## When to add a new mock

A new mock is justified when:

- A scenario needs to test a behavior **not covered** by existing mocks.
- The behavior **can't be expressed** by a CLI flag on an existing mock.

If unsure, prefer extending `mock-normal.py` with a `--mode <foo>` arg.

## Inventory

| Mock                    | Behavior                                                                                                            | Used by                                                                                                                                        |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `mock-normal.py`        | Echoes args, responds in 1ms. Reference impl. `--protocol-version <v>` overrides the `initialize` reply's version   | most happy-path tests; `protocol_version` negotiation tests                                                                                    |
| `mock-slow.py`          | `tools/call` sleeps 2s                                                                                              | latency histogram correctness                                                                                                                  |
| `mock-broken.py`        | Hangs on first `tools/call` (Vibe-Trading bug pattern)                                                              | `deadlock_probe` scenario                                                                                                                      |
| `mock-crash.py`         | Panics 1% of calls (`exit(1)`)                                                                                      | error rate accuracy                                                                                                                            |
| `mock-leak.py`          | Leaks 10 KB per call into module-global list                                                                        | `new_fixtures::mock_leak_rss_slope_positive`                                                                                                   |
| `mock-error.py`         | Returns JSON-RPC errors per spec, cycling codes                                                                     | `new_fixtures::mock_error_classifies_as_server_error`                                                                                          |
| `mock-slow-init.py`     | Sleeps 5s on `initialize`                                                                                           | `new_fixtures::mock_slow_init_pinned_contract` (pins the fixture's own contract; real cold_start coverage lives in `tests/scenarios_basic.rs`) |
| `mock-malformed.py`     | Returns invalid JSON every 10th response                                                                            | `new_fixtures::mock_malformed_classifies_as_malformed`                                                                                         |
| `mock-notify.py`        | Interleaves a `notifications/tools/list_changed` frame (has `method`, no `id`) before every response, including before the `initialize` result — mirrors real servers like the MCP reference "everything" server | `notify_interleave::session_tolerates_interleaved_notifications`                                                                               |
| `mock-output-schema.py` | `report` advertises `outputSchema`; `--mode ok\|bad\|missing` → conformant / violating / absent `structuredContent` | `strict_validation` result-side tests                                                                                                          |
| `mock-stateless-http.py` | MCP 2026-07-28 stateless server over Streamable HTTP: rejects `initialize`, answers `server/discover`, **requires** the RC `_meta` block on tools/*; `--lazy-deadlock` hangs every `tools/call` | `stateless` (ADR 0019) |
| `mock-schema.py`         | Like `mock-normal`, but `echo` advertises a strict `inputSchema` (required string `msg`) — non-matching calls must be rejected client-side before reaching the server                              | `strict_validation` args-side tests; `run_strict` CLI test                                                                                    |
| `mock-http-server.py`    | Streamable HTTP MCP server (simple JSON variant): POST returns the JSON-RPC response as `application/json`; notifications return 204. Prints `LISTENING: 127.0.0.1:<port>` on stdout              | `host_guard` allow-path tests (spawned as a real HTTP listener); DESIGN §16.5                                                                  |
| `mock-sse-server.py`     | MCP SSE transport: `GET /sse` opens the event stream (first `endpoint` event carries the POST URL), `POST /post` replies 202 and delivers the response via SSE. Single-client only                | no Rust test yet — transport parity per DESIGN §16.6 (protocol SSE tests use an in-test mock); manual runs                                     |
| `_common.py`             | Not a mock — shared helpers imported by the stdio mocks: `read_frame`/`write_frame` (line-delimited JSON), `respond_initialize`/`respond_tools_list`/`respond_ok`/`respond_error`, `cli_protocol_version` | all stdio mocks above                                                                                                                    |

Update this table when adding/modifying a mock.
