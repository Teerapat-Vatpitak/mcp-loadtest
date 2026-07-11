#!/usr/bin/env python3
"""mock-broken: hangs forever on the first tools/call (Vibe-Trading bug pattern)."""

import json
import pathlib
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from _common import (  # noqa: E402  (path manipulation above is intentional)
    read_frame,
    respond_error,
    respond_initialize,
    respond_ok,
    respond_tools_list,
)

TOOLS = [
    {
        "name": "echo",
        "description": "Echo arguments back as a JSON-stringified text content.",
        "inputSchema": {"type": "object"},
    }
]

# Module-global counter — first tools/call hangs, subsequent ones respond normally.
_calls_made = 0


def main():
    global _calls_made
    while True:
        msg = read_frame()
        if msg is None:
            return  # stdin closed
        method = msg.get("method")
        msg_id = msg.get("id")
        is_notification = msg_id is None

        if method == "initialize":
            respond_initialize(msg_id)
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            respond_tools_list(msg_id, TOOLS)
        elif method == "tools/call":
            if _calls_made == 0:
                _calls_made += 1
                # The bug: lazy-init blocking import inside async worker thread.
                # In the canonical Vibe-Trading reproduction this was effectively
                # forever — we use a sleep that outlives any reasonable test.
                time.sleep(99999)
            else:
                _calls_made += 1
                args = msg.get("params", {}).get("arguments", {})
                respond_ok(msg_id, {
                    "content": [{"type": "text", "text": json.dumps(args)}],
                })
        elif not is_notification:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
