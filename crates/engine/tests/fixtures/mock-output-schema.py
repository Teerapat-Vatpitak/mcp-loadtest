#!/usr/bin/env python3
"""mock-output-schema: `report` advertises an outputSchema; `--mode
ok|bad|missing` selects whether tools/call returns conformant, violating,
or absent structuredContent (exercises non-gating result-side validation)."""

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

MODE = sys.argv[sys.argv.index("--mode") + 1] if "--mode" in sys.argv else "ok"

TOOLS = [
    {
        "name": "report",
        "description": "Returns structuredContent shaped per --mode.",
        "inputSchema": {"type": "object"},
        "outputSchema": {
            "type": "object",
            "properties": {
                "answer": {"type": "string"},
                "count": {"type": "integer"},
            },
            "required": ["answer"],
        },
    }
]

# `bad` violates twice: required `answer` is missing, `count` has the wrong
# type. `missing` omits structuredContent entirely (a spec violation when
# outputSchema is advertised). Unknown modes fail fast at startup.
STRUCTURED = {
    "ok": {"answer": "forty-two", "count": 42},
    "bad": {"count": "not-an-integer"},
    "missing": None,
}[MODE]


def main():
    while True:
        msg = read_frame()
        if msg is None:
            return  # stdin closed
        method = msg.get("method")
        msg_id = msg.get("id")

        if method == "initialize":
            respond_initialize(msg_id)
        elif method == "notifications/initialized":
            pass  # one-way notification, no response
        elif method == "tools/list":
            respond_tools_list(msg_id, TOOLS)
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": json.dumps(STRUCTURED)}]}
            if STRUCTURED is not None:
                result["structuredContent"] = STRUCTURED
            respond_ok(msg_id, result)
        elif msg_id is not None:
            respond_error(msg_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
