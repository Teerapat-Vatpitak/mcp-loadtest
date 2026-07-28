#!/usr/bin/env python3
"""mock-normal: echoes args and rejects malformed JSON without exiting."""

import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from _common import (  # noqa: E402  (path manipulation above is intentional)
    cli_protocol_version,
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


def main():
    protocol_version = cli_protocol_version()
    while True:
        try:
            msg = read_frame()
        except json.JSONDecodeError:
            # A normal JSON-RPC server must reject malformed JSON instead of
            # crashing. Keeping the process alive also makes the raw-fuzzer
            # reaction observable as an explicit -32700 response rather than
            # an OS-scheduling race between child exit/EOF and its probe
            # deadline.
            respond_error(None, -32700, "parse error")
            continue
        if msg is None:
            return  # stdin closed
        method = msg.get("method")
        msg_id = msg.get("id")
        is_notification = msg_id is None

        # JSON-RPC notifications never receive a response. In particular, a
        # raw fuzzer frame containing `tools/list` but no id must not produce
        # a success envelope with `"id": null`.
        if is_notification:
            continue
        if method == "initialize":
            respond_initialize(msg_id, protocol_version=protocol_version)
        elif method == "tools/list":
            respond_tools_list(msg_id, TOOLS)
        elif method == "tools/call":
            args = msg.get("params", {}).get("arguments", {})
            respond_ok(msg_id, {
                "content": [{"type": "text", "text": json.dumps(args)}],
            })
        else:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
