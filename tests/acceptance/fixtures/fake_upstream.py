#!/usr/bin/env python3
"""Fake upstream service for the acceptance suite (scenario A8).

Serves on 127.0.0.1:17499 by default.

  GET  /probe    healthy mode: 200, application/json, body {"ok": true}
                 renamed mode: 200, text/plain, body "okay: true"
                 The gateway's http connector turns the healthy answer into
                 {"status": 200, "body": "{\"ok\": true}", "json": {"ok": true}},
                 which satisfies the probe contract in gateway.json. The renamed
                 answer has the field renamed and is not parseable JSON, so the
                 connector's "json" field is absent and the canary contract
                 check reports it as missing, whether the connector parses by
                 content type or by content.
  POST /control  body {"mode": "healthy" | "renamed"} switches the mode.
  GET  /control  reports the current mode.
  GET  /health   {"ok": true, "mode": ...}

Standard library only. Logs one line per request to stdout.
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODES = ("healthy", "renamed")
STATE = {"mode": "healthy"}
LOCK = threading.Lock()


def current_mode() -> str:
    with LOCK:
        return STATE["mode"]


class Handler(BaseHTTPRequestHandler):
    server_version = "FakeUpstream/0.1"

    def _reply(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, status: int, obj: object) -> None:
        self._reply(status, json.dumps(obj).encode("utf-8"), "application/json")

    def do_GET(self) -> None:  # noqa: N802 (http.server naming)
        path = self.path.split("?", 1)[0]
        if path == "/probe":
            if current_mode() == "healthy":
                self._json(200, {"ok": True})
            else:
                self._reply(200, b"okay: true\n", "text/plain; charset=utf-8")
        elif path == "/control":
            self._json(200, {"mode": current_mode()})
        elif path == "/health":
            self._json(200, {"ok": True, "mode": current_mode()})
        else:
            self._json(404, {"error": {"code": "not_found", "message": f"no route for GET {path}"}})

    def do_POST(self) -> None:  # noqa: N802
        path = self.path.split("?", 1)[0]
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if path != "/control":
            self._json(404, {"error": {"code": "not_found", "message": f"no route for POST {path}"}})
            return
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            self._json(400, {"error": {"code": "bad_json", "message": "body must be JSON"}})
            return
        mode = body.get("mode") if isinstance(body, dict) else None
        if mode not in MODES:
            self._json(422, {"error": {"code": "bad_mode", "message": f"mode must be one of {list(MODES)}"}})
            return
        with LOCK:
            STATE["mode"] = mode
        self._json(200, {"mode": mode})

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stdout.write("fake_upstream %s %s\n" % (self.address_string(), fmt % args))
        sys.stdout.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description="Fake upstream for the Kernos acceptance suite")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=17499)
    ap.add_argument("--mode", choices=MODES, default="healthy")
    args = ap.parse_args()
    STATE["mode"] = args.mode
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True
    print(f"fake_upstream listening on http://{args.host}:{args.port} mode={args.mode}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
