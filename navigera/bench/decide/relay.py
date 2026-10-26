#!/usr/bin/env python3
"""A logging relay in front of TypeSafe's API, for measuring navigera's
decisions with the real model.

navigera speaks to it as its decider (`NAVIGERA_DECIDER_URL`); it adds the
key from its own environment and forwards each request, so the key never
reaches the agent's shell or the session. Every request is logged as one
JSON line: question types, request size, the model that answered, its token
usage, the status and the round-trip time.

    TYPESAFE_API_KEY=... python3 relay.py --port 0 --log jev.jsonl   (prints "LISTENING <url>")
    NAVIGERA_DECIDER_URL=<url> navigera -s eval start
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = "https://api.typesafe.ai/v1/systemone"


class Handler(BaseHTTPRequestHandler):
    upstream = UPSTREAM
    key = ""
    log_path = None

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0) or 0))
        req = urllib.request.Request(self.upstream, data=body, method="POST", headers={
            "Content-Type": "application/json", "Authorization": f"Bearer {self.key}"})
        started = time.time()
        try:
            with urllib.request.urlopen(req, timeout=90) as r:
                status, data = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, data = e.code, e.read()
        except OSError as e:
            status, data = 502, json.dumps({"error": f"relay: {e}"}).encode()
        ms = round((time.time() - started) * 1000)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        if self.log_path:
            try:
                sent = json.loads(body or b"{}")
            except ValueError:
                sent = {}
            try:
                reply = json.loads(data or b"{}")
            except ValueError:
                reply = {}
            questions = sent.get("questions") or {}
            entry = {"t": round(started, 3), "ms": ms, "status": status, "chars": len(body),
                     "questions": len(questions),
                     "types": sorted({q.get("type", "?") for q in questions.values() if isinstance(q, dict)}),
                     "model": reply.get("model"), "usage": reply.get("usage"),
                     "error": None if status == 200 else data[:300].decode("utf-8", "replace")}
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps(entry) + "\n")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8792)
    ap.add_argument("--log")
    ap.add_argument("--upstream", default=os.environ.get("TYPESAFE_UPSTREAM", UPSTREAM))
    args = ap.parse_args()
    Handler.key = os.environ.get("TYPESAFE_API_KEY", "").strip()
    if not Handler.key:
        print("relay.py: TYPESAFE_API_KEY is not set", file=sys.stderr)
        return 2
    Handler.upstream, Handler.log_path = args.upstream, args.log
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"LISTENING http://127.0.0.1:{server.server_port}/v1/systemone", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
