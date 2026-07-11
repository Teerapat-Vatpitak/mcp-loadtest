#!/usr/bin/env python3
"""mock-sse-server: MCP SSE transport (event-stream subscribe + POST send).

- GET /sse opens a long-lived event stream. First event: `endpoint` whose
  data is the absolute URL the client should POST request bodies to.
- POST /post accepts a JSON-RPC request body, dispatches it, and writes the
  response into the SSE stream as an `event: message` frame. The POST itself
  replies 202 Accepted (response delivery is via SSE).

Simplification (M4): single connected client only. A `queue.Queue` carries
outbound `message` events from the POST handler to the SSE writer thread.
For a second concurrent /sse the latest one wins (older one's queue is
abandoned). Race conditions on multi-client are tolerable for load-test
fixtures — the Rust transport opens one connection.

Stdlib only. Bind 127.0.0.1:<port>; print `LISTENING: 127.0.0.1:<port>`.
"""

import argparse
import json
import queue
import signal
import socketserver
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Single-client outbound queue. POST handlers `put()` JSON-RPC response dicts;
# the active /sse handler `get()`s them and writes SSE frames. Replaced when a
# new /sse connection opens — previous queue is abandoned.
_OUTBOUND_LOCK = threading.Lock()
_OUTBOUND: "queue.Queue[dict]" = queue.Queue()


def _swap_outbound() -> "queue.Queue[dict]":
    """Install a fresh outbound queue for a new SSE subscriber, return it."""
    global _OUTBOUND
    new_q: "queue.Queue[dict]" = queue.Queue()
    with _OUTBOUND_LOCK:
        _OUTBOUND = new_q
    return new_q


def _current_outbound() -> "queue.Queue[dict]":
    with _OUTBOUND_LOCK:
        return _OUTBOUND


def dispatch(msg):
    """Return a JSON-RPC response dict for `msg`, or None for notifications."""
    method = msg.get("method")
    msg_id = msg.get("id")
    if msg_id is None:
        return None
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-sse", "version": "0.0.0"},
            },
        }
    if method == "tools/list":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo arguments back as JSON text.",
                        "inputSchema": {"type": "object"},
                    }
                ]
            },
        }
    if method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {"content": [{"type": "text", "text": json.dumps(args)}]},
        }
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "error": {"code": -32601, "message": f"method not found: {method}"},
    }


# Sentinel pushed into the queue to tell the SSE writer to exit cleanly.
_STOP = object()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        if self.path != "/sse":
            self.send_response(404)
            self.end_headers()
            return

        my_queue = _swap_outbound()

        host = (
            self.headers.get("Host")
            or f"{self.server.server_address[0]}:{self.server.server_address[1]}"
        )  # type: ignore[attr-defined]
        endpoint_url = f"http://{host}/post"

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("X-Accel-Buffering", "no")
        self.end_headers()

        try:
            self.wfile.write(
                f"event: endpoint\ndata: {endpoint_url}\n\n".encode("utf-8")
            )
            self.wfile.flush()
            while True:
                try:
                    item = my_queue.get(timeout=15.0)
                except queue.Empty:
                    # SSE keep-alive comment — keeps proxies + tcp happy.
                    self.wfile.write(b": keep-alive\n\n")
                    self.wfile.flush()
                    continue
                if item is _STOP:
                    return
                data = json.dumps(item)
                self.wfile.write(f"event: message\ndata: {data}\n\n".encode("utf-8"))
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            # Client disconnected — that's fine, just exit the handler.
            return

    def do_POST(self):  # noqa: N802
        if self.path not in ("/post", "/"):
            self.send_response(404)
            self.end_headers()
            return

        length = int(self.headers.get("Content-Length", "0") or "0")
        raw = self.rfile.read(length) if length else b""
        try:
            msg = json.loads(raw.decode("utf-8")) if raw else {}
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.send_response(400)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"invalid json\n")
            return

        response = dispatch(msg)
        if response is not None:
            _current_outbound().put(response)

        self.send_response(202)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, fmt, *args):
        pass


class _Server(ThreadingHTTPServer):
    """`http.server.HTTPServer.server_bind()` calls `socket.getfqdn(host)`, a
    reverse-DNS lookup that can block ~30s on macOS CI runners (and is not
    needed for a loopback test mock). Bind via the plain TCPServer path and
    set `server_name`/`server_port` directly so `LISTENING:` is emitted
    immediately on every platform.
    """

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    server = _Server((args.host, args.port), Handler)
    host, port = server.server_address[0], server.server_address[1]
    sys.stdout.write(f"LISTENING: {host}:{port}\n")
    sys.stdout.flush()

    def shutdown(_signum=None, _frame=None):
        # Wake any blocked SSE writer so it returns and the server can close.
        _current_outbound().put(_STOP)
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGINT, shutdown)
    signal.signal(signal.SIGTERM, shutdown)

    try:
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
