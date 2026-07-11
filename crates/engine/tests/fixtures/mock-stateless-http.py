#!/usr/bin/env python3
"""mock-stateless-http: MCP 2026-07-28 stateless server over Streamable HTTP.

No initialize handshake: rejects `initialize` (-32600), answers
`server/discover`, and REQUIRES the RC `_meta` block (with
io.modelcontextprotocol/protocolVersion == 2026-07-28) on tools/* requests
so a client that forgets `_meta` fails loudly. `--lazy-deadlock` makes every
tools/call block forever (Vibe-Trading bug class, stateless edition).
Bind 127.0.0.1:<port>; print `LISTENING: 127.0.0.1:<port>`.
"""

import argparse
import json
import signal
import socketserver
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "2026-07-28"
META_VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
OPTS = {"lazy_deadlock": False}


def err(msg_id, code, message):
    return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}


def dispatch(msg):
    method, msg_id = msg.get("method"), msg.get("id")
    if msg_id is None:
        return None  # notification — 204
    if method == "initialize":
        return err(msg_id, -32600, "stateless server: initialize was removed in 2026-07-28")
    if method == "server/discover":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {
            "protocolVersion": VERSION,
            "protocolVersions": [VERSION],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-stateless-http", "version": "0.0.0"},
        }}
    if method in ("tools/list", "tools/call"):
        meta = msg.get("params", {}).get("_meta", {})
        if meta.get(META_VERSION_KEY) != VERSION:
            return err(msg_id, -32600, f"missing/wrong _meta {META_VERSION_KEY}")
        if method == "tools/list":
            return {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": [
                {"name": "echo", "description": "Echo args.", "inputSchema": {"type": "object"}},
            ]}}
        if OPTS["lazy_deadlock"]:
            threading.Event().wait()  # hang this request forever
        args = msg.get("params", {}).get("arguments", {})
        return {"jsonrpc": "2.0", "id": msg_id, "result": {
            "content": [{"type": "text", "text": json.dumps(args)}]}}
    return err(msg_id, -32601, f"method not found: {method}")


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802  (BaseHTTPRequestHandler API)
        length = int(self.headers.get("Content-Length", "0") or "0")
        raw = self.rfile.read(length) if length else b""
        try:
            msg = json.loads(raw.decode("utf-8")) if raw else {}
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.send_response(400)
            self.end_headers()
            return
        response = dispatch(msg)
        if response is None:
            self.send_response(204)
            self.end_headers()
            return
        body = json.dumps(response).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):  # silence per-request stderr logging
        pass


class _Server(ThreadingHTTPServer):
    """Skip `socket.getfqdn` (can block ~30s on macOS CI); see mock-http-server."""

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name, self.server_port = host, port


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--lazy-deadlock", action="store_true")
    args = parser.parse_args()
    OPTS["lazy_deadlock"] = args.lazy_deadlock

    server = _Server((args.host, args.port), Handler)
    host, port = server.server_address[0], server.server_address[1]
    sys.stdout.write(f"LISTENING: {host}:{port}\n")
    sys.stdout.flush()

    signal.signal(signal.SIGINT, lambda *_: threading.Thread(target=server.shutdown, daemon=True).start())
    signal.signal(signal.SIGTERM, lambda *_: threading.Thread(target=server.shutdown, daemon=True).start())
    try:
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
