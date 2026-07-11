#!/usr/bin/env python3
"""mock-malformed: every 10th tools/call emits a truncated (but \\n-ended) line."""

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
        "description": "Echo args; every 10th call returns broken JSON.",
        "inputSchema": {"type": "object"},
    }
]

# Module-global counter — every 10th tools/call (1-indexed) emits a complete
# *line* (newline-terminated) that is invalid JSON. The trailing "\n" is
# mandatory: the stdio transport reads a full line then JSON-parses it, so a
# terminated-but-invalid line yields a parse error (-> CallOutcome::Malformed),
# whereas an unterminated line would hang (-> wrong outcome: Timeout).
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
            _calls_made += 1
            if _calls_made % 10 == 0:
                # Raw write — NOT _common.write_frame (it always emits valid
                # JSON). Truncated object, still \n-terminated.
                sys.stdout.write(
                    '{"jsonrpc":"2.0","id":' + str(msg_id) + ',"result":{\n'
                )
                sys.stdout.flush()
            else:
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
