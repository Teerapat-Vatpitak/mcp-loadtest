"""Shared framing helpers for mock MCP servers.

Each mock imports from this file. Stdlib only — no third-party deps so we
don't couple to any real MCP SDK version.
"""

import json
import sys


def write_frame(obj):
    """Write a JSON object as one line on stdout, then flush."""
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def read_frame():
    """Read one JSON line from stdin. Returns parsed dict, or None on EOF."""
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


def respond_initialize(request_id, server_name="mock", server_version="0.0.0",
                       protocol_version="2025-03-26"):
    """Send a minimal `initialize` result."""
    write_frame({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": server_name, "version": server_version},
        },
    })


def cli_protocol_version(default="2025-03-26"):
    """Read an optional `--protocol-version <v>` from argv (version knob)."""
    if "--protocol-version" in sys.argv:
        return sys.argv[sys.argv.index("--protocol-version") + 1]
    return default


def respond_tools_list(request_id, tools):
    """Send a `tools/list` result with the given tool definitions."""
    write_frame({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {"tools": tools},
    })


def respond_ok(request_id, result):
    """Send a generic successful response with the given result payload."""
    write_frame({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result,
    })


def respond_error(request_id, code, message, data=None):
    """Send a JSON-RPC error response."""
    err = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    write_frame({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": err,
    })
