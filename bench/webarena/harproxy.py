#!/usr/bin/env python3
"""A recording reverse proxy for one WebArena-Verified site.

The browser tools talk to the site's canonical address (the host/port the
suite's images are configured for, e.g. http://localhost:7770). This proxy
listens on that canonical port, forwards every request to the site
container published on a shadow port, and appends each completed
request/response pair as one JSON line to an entries file — the runner
assembles those lines into the network.har the suite's evaluators read.

It also injects the site's auto-login header (the optimized images accept
HTTP header authentication; see the suite's environment docs) into every
request that does not already carry it, so the agent starts logged in the
same way the suite's own harness arranges.

Stdlib only. One process per site per task:

  harproxy.py --listen 7770 --upstream 17770 \
              --entries out/task-44/trace-shopping.jsonl \
              --header "X-M2-Customer-Auto-Login: emma.lopez@gmail.com:Password.123"
"""
import argparse
import http.client
import json
import re
import select
import socket
import sys
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl

HOP_BY_HOP = {"connection", "proxy-connection", "keep-alive", "te", "trailer",
              "transfer-encoding", "upgrade", "proxy-authorization", "proxy-authenticate"}
TEXTUAL_PREFIXES = ("text/",)
TEXTUAL_TYPES = {"application/json", "application/x-www-form-urlencoded", "application/xml",
                 "application/javascript", "application/xhtml+xml", "multipart/form-data"}
MAX_TEXT = 4_000_000  # chars of a body kept as HAR text


def is_textual(mime: str) -> bool:
    mime = mime.split(";")[0].strip().lower()
    return mime.startswith(TEXTUAL_PREFIXES) or mime in TEXTUAL_TYPES or mime.endswith("+json") \
        or mime.endswith("+xml")


class Recorder:
    def __init__(self, path: str):
        self.f = open(path, "a", encoding="utf-8")
        self.lock = threading.Lock()

    def add(self, entry: dict):
        with self.lock:
            self.f.write(json.dumps(entry) + "\n")
            self.f.flush()


def header_list(msg) -> list:
    return [{"name": k, "value": v} for k, v in msg.items()]


def body_text(raw: bytes, mime: str) -> str:
    if not raw or not is_textual(mime):
        return ""
    return raw[:MAX_TEXT].decode("utf-8", errors="replace")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "harproxy"
    sys_version = ""

    # -- plumbing -----------------------------------------------------------
    def log_message(self, fmt, *args):  # keep stderr for real errors only
        pass

    def _read_body(self) -> bytes:
        te = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in te:
            out = b""
            while True:
                size = int(self.rfile.readline().strip().split(b";")[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return out
                out += self.rfile.read(size)
                self.rfile.readline()
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    # -- request handling ---------------------------------------------------
    def _handle(self):
        started = time.time()
        stamp = datetime.now(timezone.utc)
        body = self._read_body()
        if (self.headers.get("Upgrade") or "").lower() in ("websocket", "h2c"):
            self._tunnel(body, started, stamp)
            return
        cfg = self.server.cfg
        host = self.headers.get("Host", f"localhost:{cfg['listen']}")
        url = f"http://{host}{self.path}"
        headers = {}
        present = set()
        for k, v in self.headers.items():
            present.add(k.lower())
            if k.lower() not in HOP_BY_HOP and k.lower() != "host":
                headers[k] = v
        headers["Host"] = host
        for name, value in cfg["inject"]:
            if name.lower() not in present:
                headers[name] = value
        status, status_text, resp_headers, resp_body = 502, "Bad Gateway", [], b""
        try:
            conn = http.client.HTTPConnection("127.0.0.1", cfg["upstream"], timeout=180)
            conn.request(self.command, self.path, body=body or None, headers=headers)
            resp = conn.getresponse()
            status, status_text = resp.status, resp.reason or ""
            resp_headers = resp.getheaders()
            resp_body = resp.read() or b""
            conn.close()
        except Exception as e:  # upstream down/reset: answer like a dead site would look
            resp_body = f"proxy upstream error: {e}".encode()
        out_headers = [(k, v) for k, v in resp_headers
                       if k.lower() not in HOP_BY_HOP and k.lower() != "content-length"]
        try:
            self.send_response(status, status_text)
            for k, v in out_headers:
                self.send_header(k, v)
            self.send_header("Content-Length", str(0 if self.command == "HEAD" else len(resp_body)))
            self.end_headers()
            if self.command != "HEAD" and resp_body:
                self.wfile.write(resp_body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self._record(started, stamp, url, body, status, out_headers, resp_body)

    def _tunnel(self, body: bytes, started: float, stamp: datetime):
        """WebSocket/upgrade requests: replay the handshake upstream, then
        shuttle bytes both ways. Recorded with the handshake's status."""
        cfg = self.server.cfg
        host = self.headers.get("Host", f"localhost:{cfg['listen']}")
        url = f"http://{host}{self.path}"
        status = 101
        try:
            up = socket.create_connection(("127.0.0.1", cfg["upstream"]), timeout=30)
        except Exception:
            try:
                self.send_response(502)
                self.send_header("Content-Length", "0")
                self.end_headers()
            except Exception:
                pass
            self._record(started, stamp, url, body, 502, [], b"")
            return
        lines = [f"{self.command} {self.path} {self.request_version}"]
        for k, v in self.headers.items():
            lines.append(f"{k}: {v}")
        raw = ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8", "replace") + body
        try:
            up.sendall(raw)
            self.connection.settimeout(0.5)
            up.settimeout(0.5)
            first = True
            while True:
                r, _, _ = select.select([self.connection, up], [], [], 60)
                if not r:
                    break
                for s in r:
                    data = s.recv(65536)
                    if not data:
                        raise ConnectionError("closed")
                    if first and s is up:
                        m = re.match(rb"HTTP/[\d.]+ (\d+)", data)
                        if m:
                            status = int(m.group(1))
                        first = False
                    (up if s is self.connection else self.connection).sendall(data)
        except Exception:
            pass
        finally:
            try:
                up.close()
            except Exception:
                pass
        self._record(started, stamp, url, body, status, [], b"")

    def _record(self, started, stamp, url, req_body, status, resp_headers, resp_body):
        cfg = self.server.cfg
        req_mime = self.headers.get("Content-Type", "")
        resp_mime = ""
        redirect = ""
        for k, v in resp_headers:
            if k.lower() == "content-type":
                resp_mime = v
            if k.lower() == "location":
                redirect = v
        entry = {
            "startedDateTime": stamp.strftime("%Y-%m-%dT%H:%M:%S.") + f"{stamp.microsecond // 1000:03d}Z",
            "time": int((time.time() - started) * 1000),
            "request": {
                "method": self.command, "url": url, "httpVersion": "HTTP/1.1",
                "cookies": [], "headers": header_list(self.headers),
                "queryString": [{"name": k, "value": v} for k, v in
                                parse_qsl(url.partition("?")[2], keep_blank_values=True)],
                "headersSize": -1,
                "bodySize": len(req_body),
            },
            "response": {
                "status": status, "statusText": "", "httpVersion": "HTTP/1.1",
                "cookies": [], "headers": [{"name": k, "value": v} for k, v in resp_headers],
                "content": {"size": len(resp_body), "mimeType": resp_mime.split(";")[0],
                            "text": body_text(resp_body, resp_mime)},
                "redirectURL": redirect, "headersSize": -1, "bodySize": len(resp_body),
            },
            "cache": {}, "timings": {"send": 0, "wait": int((time.time() - started) * 1000), "receive": 0},
        }
        if req_body:
            entry["request"]["postData"] = {"mimeType": req_mime,
                                            "text": body_text(req_body, req_mime)}
        cfg["recorder"].add(entry)

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_PATCH = _handle
    do_DELETE = _handle
    do_HEAD = _handle
    do_OPTIONS = _handle


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, required=True)
    ap.add_argument("--upstream", type=int, required=True)
    ap.add_argument("--entries", required=True)
    ap.add_argument("--header", action="append", default=[],
                    help="'Name: value' to inject when the request lacks it (repeatable)")
    args = ap.parse_args()
    inject = []
    for h in args.header:
        name, _, value = h.partition(":")
        inject.append((name.strip(), value.strip()))
    server = ThreadingHTTPServer(("127.0.0.1", args.listen), Handler)
    server.daemon_threads = True
    server.cfg = {"listen": args.listen, "upstream": args.upstream, "inject": inject,
                  "recorder": Recorder(args.entries)}
    print(f"harproxy :{args.listen} -> :{args.upstream}, entries {args.entries}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
