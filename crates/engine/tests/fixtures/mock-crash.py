#!/usr/bin/env python3
"""mock-crash: random or deterministic tools/call exits without a response."""

import json
import pathlib
import random
import sys

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
        "description": "Echo arguments back; occasionally crashes the server.",
        "inputSchema": {"type": "object"},
    }
]

CRASH_PROBABILITY = 0.01


def crash_after_arg():
    if "--crash-after" not in sys.argv:
        return None
    value = int(sys.argv[sys.argv.index("--crash-after") + 1])
    if value < 1:
        raise ValueError("--crash-after must be >= 1")
    return value


def main():
    crash_after = crash_after_arg()
    call_count = 0
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
            call_count += 1
            deterministic_crash = crash_after is not None and call_count >= crash_after
            if deterministic_crash or random.random() < CRASH_PROBABILITY:
                # Crash mid-call: exit before sending any response. Client should
                # observe a server disconnect and classify this as Crash.
                sys.exit(1)
            args = msg.get("params", {}).get("arguments", {})
            respond_ok(msg_id, {
                "content": [{"type": "text", "text": json.dumps(args)}],
            })
        elif not is_notification:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
