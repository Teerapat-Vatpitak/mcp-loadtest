#!/usr/bin/env python3
"""Initialize normally, reject tools/list, but accept tools/call.

This adversarial shape proves strict validation cannot silently continue with
an empty schema registry when discovery fails.
"""

import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from _common import (  # noqa: E402
    read_frame,
    respond_error,
    respond_initialize,
    respond_ok,
)


def main():
    while True:
        msg = read_frame()
        if msg is None:
            return
        method = msg.get("method")
        msg_id = msg.get("id")

        if method == "initialize":
            respond_initialize(msg_id, server_name="mock-tools-list-error")
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            respond_error(msg_id, -32603, "intentional tools/list failure")
        elif method == "tools/call":
            args = msg.get("params", {}).get("arguments", {})
            respond_ok(
                msg_id,
                {
                    "content": [
                        {"type": "text", "text": json.dumps(args, sort_keys=True)}
                    ]
                },
            )
        elif msg_id is not None:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
