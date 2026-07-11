#!/usr/bin/env python3
"""mock-slow: tools/call sleeps 2 seconds before responding."""

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
        "description": "Echo arguments back, but slowly.",
        "inputSchema": {"type": "object"},
    }
]

CALL_DELAY_SECS = 2.0


def main():
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
            time.sleep(CALL_DELAY_SECS)
            args = msg.get("params", {}).get("arguments", {})
            respond_ok(msg_id, {
                "content": [{"type": "text", "text": json.dumps(args)}],
            })
        elif not is_notification:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
