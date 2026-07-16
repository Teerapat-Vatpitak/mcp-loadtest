#!/usr/bin/env python3
"""Interleaves a JSON-RPC notification before every response.

Mirrors real MCP servers (e.g. the reference "everything" server) that emit
`notifications/tools/list_changed` and progress notifications on the stream at
any time — including before the `initialize` result. A single-flight client
that assumes the next line is always its response desyncs on the first
notification; a correct client skips notification frames (no `id`).
"""
import json
from _common import read_frame, write_frame, respond_initialize, respond_tools_list, respond_ok


def notify():
    """Emit a notification frame: has `method`, no `id`."""
    write_frame({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})


def main():
    while True:
        msg = read_frame()
        if msg is None:
            break
        method = msg.get("method")
        if method == "initialize":
            notify()  # fired before the result — the key desync trigger
            respond_initialize(msg["id"])
        elif method == "notifications/initialized":
            continue  # client notification; no reply
        elif method == "tools/list":
            notify()
            respond_tools_list(msg["id"], tools=[
                {"name": "echo", "inputSchema": {"type": "object"}},
            ])
        elif method == "tools/call":
            notify()
            args = msg["params"]["arguments"]
            respond_ok(msg["id"], {"content": [{"type": "text", "text": json.dumps(args)}]})


if __name__ == "__main__":
    main()
