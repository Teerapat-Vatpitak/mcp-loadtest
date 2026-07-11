#!/usr/bin/env python3
"""mock-slow-init: sleeps 5s on initialize, then behaves normally."""

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
        "description": "Echo arguments back; initialize is delayed 5s.",
        "inputSchema": {"type": "object"},
    }
]

# Slow handshake only. 5s < the 10s default startup budget, so a normal
# session still initializes; tools/list and tools/call respond immediately.
INIT_DELAY_SECS = 5.0


def main():
    while True:
        msg = read_frame()
        if msg is None:
            return  # stdin closed
        method = msg.get("method")
        msg_id = msg.get("id")
        is_notification = msg_id is None

        if method == "initialize":
            time.sleep(INIT_DELAY_SECS)
            respond_initialize(msg_id)
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            respond_tools_list(msg_id, TOOLS)
        elif method == "tools/call":
            args = msg.get("params", {}).get("arguments", {})
            respond_ok(
                msg_id,
                {
                    "content": [{"type": "text", "text": json.dumps(args)}],
                },
            )
        elif not is_notification:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
