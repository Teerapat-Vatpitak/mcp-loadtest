#!/usr/bin/env python3
"""mock-schema: like mock-normal, but `echo` advertises a strict inputSchema
(`msg` is a required string). Used to exercise opt-in strict args validation:
calls whose args don't match this schema must be rejected client-side before
they reach this server."""

import json
import pathlib
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
        "description": "Echo args back. Requires a string `msg`.",
        "inputSchema": {
            "type": "object",
            "properties": {"msg": {"type": "string"}},
            "required": ["msg"],
        },
    }
]


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
            pass  # one-way notification, no response
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
