#!/usr/bin/env python3
"""E7 CSP-egress observer (scripts/e7-plugins-spike.sh).

A tiny OFF-ORIGIN HTTP server: the fixture plugin's `plugin.js` fetch()es
`http://127.0.0.1:<port>/beacon?...`. Every arriving request is appended as a
JSON line to <logfile> — a line in that file IS the proof that plugin egress
left the browser. The CSP leg asserts the file stays EMPTY while the plugin
still loads.

Usage: e7_beacon.py <port> <logfile>
Stdlib only. Runs until killed (the gate records and kills this PID only).
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
LOGFILE = sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def _record(self):
        entry = {
            "ts": time.time(),
            "method": self.command,
            "path": self.path,
            "origin": self.headers.get("Origin"),
            "referer": self.headers.get("Referer"),
            "ua": (self.headers.get("User-Agent") or "")[:80],
        }
        with open(LOGFILE, "a", encoding="utf-8") as f:
            f.write(json.dumps(entry) + "\n")
        body = b'{"beacon":"observed"}'
        self.send_response(200)
        # CORS-open so the observation isn't masked by a failed preflight:
        # we want the request to ARRIVE whenever the browser lets it leave.
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_GET(self):
        self._record()

    def do_POST(self):
        self._record()

    def do_OPTIONS(self):
        self._record()

    def log_message(self, *args):  # quiet
        pass


if __name__ == "__main__":
    open(LOGFILE, "a").close()
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
