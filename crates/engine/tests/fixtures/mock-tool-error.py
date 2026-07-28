#!/usr/bin/env python3
"""mock-tool-error: tools/call succeeds at JSON-RPC but sets isError=true."""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from _common import (  # noqa: E402
    cli_protocol_version,
    read_frame,
    respond_error,
    respond_initialize,
    respond_ok,
    respond_tools_list,
)

TOOLS = [{"name": "fail", "inputSchema": {"type": "object"}}]


def main():
    protocol_version = cli_protocol_version()
    while True:
        msg = read_frame()
        if msg is None:
            return
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            respond_initialize(msg_id, protocol_version=protocol_version)
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            respond_tools_list(msg_id, TOOLS)
        elif method == "tools/call":
            respond_ok(
                msg_id,
                {
                    "content": [{"type": "text", "text": "logical failure"}],
                    "isError": True,
                },
            )
        elif msg_id is not None:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
