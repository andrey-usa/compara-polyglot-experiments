#!/usr/bin/env python3
"""A stand-in for TypeSafe's System One API (POST /v1/systemone), for tests.

It answers by word overlap, not by understanding: a Choice goes to the
option sharing the most words with what the instructions ask for, a Noul is
yes when the phrase the question quotes appears in the state. Enough to test
the plumbing of `navigera find` / `check` without a key; never a measure of
how well the real model chooses.

    python3 mock_jev.py [--port 8791] [--log requests.jsonl]   (prints "LISTENING <url>")
    NAVIGERA_DECIDER_URL=http://127.0.0.1:8791/v1/systemone navigera ...
"""
import argparse
import json
import math
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STOP = {"the", "a", "an", "to", "of", "for", "on", "in", "is", "it", "use", "which", "element", "page", "one",
        "this", "that", "and", "or", "with", "button", "link"}


def words(text: str) -> set:
    return {w for w in re.findall(r"[a-z0-9$.]+", text.lower()) if w not in STOP}


def text_of(value) -> str:
    return value if isinstance(value, str) else json.dumps(value)


def choice(q: dict) -> dict:
    instructions = text_of(q.get("instructions", ""))
    goal = instructions.split("for:", 1)[-1]
    want = words(goal)
    scores = {k: len(want & words(text_of(v))) for k, v in q.get("criteria", {}).items()}
    exp = {k: math.exp(3 * s) for k, s in scores.items()}
    total = sum(exp.values()) or 1.0
    probs = {k: v / total for k, v in exp.items()}
    best = max(probs, key=probs.get)
    return {"type": "choice", "choice": best, "probabilities": probs, "confidence": probs[best]}


def noul(q: dict, state: str) -> dict:
    question = text_of(q.get("instructions", ""))
    quoted = re.findall(r'"([^"]+)"', question)
    if quoted:
        yes = all(p.lower() in state.lower() for p in quoted)
    else:
        want = words(question)
        yes = bool(want) and len(want & words(state)) / len(want) > 0.6
    return {"type": "noul", "noul": 0.95 if yes else 0.05}


class Handler(BaseHTTPRequestHandler):
    log_path = None

    def log_message(self, *_):
        pass

    def reply(self, status: int, payload: dict):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0)) or 0) or b"{}")
        if not self.path.rstrip("/").endswith("/systemone"):
            return self.reply(404, {"error": "not found"})
        state = text_of(body.get("state", ""))
        answers = {}
        for name, q in (body.get("questions") or {}).items():
            kind = q.get("type")
            if kind == "choice":
                answers[name] = choice(q)
            elif kind == "noul":
                answers[name] = noul(q, state)
            elif kind == "score":
                levels = q.get("criteria", [])
                answers[name] = {"type": "score", "score": 0.0, "confidence": 0.5,
                                 "probabilities": {str(i): 1.0 / len(levels) for i in range(len(levels))},
                                 "legend": {str(i): text_of(l) for i, l in enumerate(levels)}}
            else:
                return self.reply(422, {"error": f"unknown question type {kind!r}"})
        if self.log_path:
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps({"auth": self.headers.get("Authorization"), "body": body, "answers": answers}) + "\n")
        tokens = len(json.dumps(body)) // 4
        self.reply(200, {"model": "mock-jev", "answers": answers, "usage": {"input_tokens": tokens, "output_tokens": 0}})


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8791)
    ap.add_argument("--log")
    args = ap.parse_args()
    Handler.log_path = args.log
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    # Like bench/site/server.py: the first stdout line says where it listens.
    print(f"LISTENING http://127.0.0.1:{server.server_port}/v1/systemone", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
