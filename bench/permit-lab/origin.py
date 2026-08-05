#!/usr/bin/env python3
"""Latency-injecting source origin for the permit lab.

Serves files from a directory over HTTP after sleeping LATENCY_MS, so a
run can model the origin round trip that a `gs://` or `https://` source
pays *inside* oximg's CPU permit. Counts requests so a run can verify
the coalescer did not merge the load it thought it was generating.

stdlib only: this has to run on any box with python3 and no installs.
"""

import os
import socketserver
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.environ.get("ROOT", "fixtures")
LATENCY_MS = float(os.environ.get("LATENCY_MS", "0"))
# Cost charged once per NEW connection, on its first data request:
# models what connection establishment costs against a real origin
# (TCP + TLS 1.3 over the link RTT is ~2 round trips + crypto — the
# cost a *reused* connection never pays). Loopback TCP is ~0.1ms, so
# without this the lab cannot see connection churn at all.
SETUP_MS = float(os.environ.get("SETUP_MS", "0"))
PORT = int(os.environ.get("PORT", "8099"))

_served = 0
_conns = 0
_lock = threading.Lock()


class Server(ThreadingHTTPServer):
    """`http.server` without the reverse-DNS stall.

    `HTTPServer.server_bind()` calls `socket.getfqdn()`, a reverse
    lookup that hangs for tens of seconds on a host whose resolver has
    no reverse zone — observed here on a Tailscale-managed box, where
    the harness appeared to start, never bound, and every oximg request
    failed with a connect timeout instead. Nothing here uses
    `server_name`.
    """

    daemon_threads = True

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # Set on the first *data* request of a connection (one Handler
    # instance per connection under ThreadingHTTPServer). Counting
    # lazily — instead of in setup() — keeps the harness's own counter
    # scrapes out of the connection count, and charges SETUP_MS where a
    # TLS handshake would land: before the first byte of the first
    # request, inside whatever TTFB the client measures.
    _counted = False

    def _count_connection(self):
        global _conns
        if self._counted:
            return
        self._counted = True
        with _lock:
            _conns += 1
        if SETUP_MS:
            time.sleep(SETUP_MS / 1000.0)

    def _counter(self, value):
        body = str(value).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802
        global _served
        if self.path == "/__count":
            with _lock:
                value = _served
            self._counter(value)
            return
        if self.path == "/__conns":
            # Data connections only (see _count_connection): the ratio
            # against /__count is connections per request, i.e. how
            # often a client paid connection setup instead of reusing.
            with _lock:
                value = _conns
            self._counter(value)
            return
        self._count_connection()
        name = os.path.basename(self.path.split("?")[0])
        path = os.path.join(ROOT, name)
        if not os.path.isfile(path):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        with open(path, "rb") as f:
            data = f.read()
        # The whole point of the harness: a controllable origin RTT.
        if LATENCY_MS:
            time.sleep(LATENCY_MS / 1000.0)
        with _lock:
            _served += 1
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    srv = Server(("0.0.0.0", PORT), Handler)
    print(
        f"origin on :{PORT} root={ROOT} latency={LATENCY_MS}ms setup={SETUP_MS}ms",
        file=sys.stderr,
        flush=True,
    )
    srv.serve_forever()
