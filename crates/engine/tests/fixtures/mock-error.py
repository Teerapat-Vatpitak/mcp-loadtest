#!/usr/bin/env python3
"""mock-error: every tools/call returns a JSON-RPC error, cycling codes."""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from _common import (  # noqa: E402  (path manipulation above is intentional)
    read_frame,
    respond_error,
    respond_initialize,
    respond_tools_list,
)

TOOLS = [
    {
        "name": "echo",
        "description": "Always errors; cycles -32601 / -32602 / -32603.",
        "inputSchema": {"type": "object"},
    }
]

# JSON-RPC error codes cycled per tools/call: method-not-found, invalid-params,
# internal-error. The client's classify_error maps these to ServerError.
ERROR_CODES = [-32601, -32602, -32603]

# Module-global counter — selects which code to return this call.
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
            code = ERROR_CODES[_calls_made % len(ERROR_CODES)]
            _calls_made += 1
            respond_error(msg_id, code, f"mock-error: deliberate failure {code}")
        elif not is_notification:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
