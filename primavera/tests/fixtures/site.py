"""A small site for primavera's own tests: `/flaky` fails its first visit
and passes after, `/late` shows its text after `?ms=`, `/m1` has what the
newer expect subjects read, `/login` signs in (counted at `/logins`), and
`/note?x=` records what `before/after all` did (`/notes`)."""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

VISITS = {"flaky": 0}
LOGINS = [0]
NOTES = []
ORDERS = [0]

M1 = """
<div data-testid=toast>Saved order 7</div>
<span data-testid=badge style="color: rgb(0, 128, 0)">ok</span>
<input aria-label=Notes>
<textarea aria-label=Filled>x</textarea>
<button id=order onclick="fetch('/api/order', {method: 'POST', body: '{}'}).then((r) => r.json()).then((j) => { document.title = j.id; })">Order</button>
<a id=dl href=/report.csv>Report</a>
<script>console.log('m1 ready')</script>
"""

LOGIN = """
<h1>Sign in</h1>
<input aria-label=Password type=password id=pw>
<button onclick="fetch('/session', {method: 'POST', body: pw.value}).then((r) => { if (r.ok) location = '/account'; else document.body.append('wrong password'); })">Sign in</button>
"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def page(self, body):
        data = f"<!doctype html><html><head><meta charset=utf-8><title>t</title></head><body>{body}</body></html>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def raw(self, data, kind="text/plain", headers=()):
        data = data.encode() if isinstance(data, str) else data
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def answer(self, status, text):
        data = text.encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        u = urlparse(self.path)
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode()
        if u.path == "/api/order":
            ORDERS[0] += 1
            return self.raw('{"id": "ORD-7", "items": [{"sku": "sku-1", "qty": 2}], "total": 36}', "application/json")
        if u.path == "/api/users":
            if self.headers.get("Content-Type") != "application/json":
                return self.answer(415, '{"error": "send JSON"}')
            got = json.loads(body)
            return self.answer(201, json.dumps({"id": "u-1", "email": got["email"], "role": got["role"]}))
        if u.path == "/session":
            if body == "hunter22":
                LOGINS[0] += 1
                return self.raw("ok", headers=[("Set-Cookie", "sid=abc; Path=/")])
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
        if u.path == "/m1":
            return self.page(M1)
        if u.path == "/api/x":
            return self.raw("real")
        if u.path == "/report.csv":
            return self.raw("id,name\n1,Ada\n", "text/csv", [("Content-Disposition", 'attachment; filename="report-2026.csv"')])
        if u.path == "/login":
            return self.page(LOGIN)
        if u.path == "/api/me":
            sid = next((c.split("=", 1)[1] for c in (self.headers.get("Cookie") or "").split("; ") if c.startswith("sid=")), None)
            return self.answer(200, json.dumps({"sid": sid})) if sid else self.answer(401, '{"error": "signed out"}')
        if u.path == "/api/fail":
            return self.answer(500, "db down")
        if u.path == "/account":
            signed = "sid=abc" in (self.headers.get("Cookie") or "")
            return self.page("<h1>Account</h1><p>Signed in</p>" if signed else "<h1>Sign in first</h1>")
        if u.path == "/logins":
            return self.raw(str(LOGINS[0]))
        if u.path == "/note":
            NOTES.append(q.get("x", [""])[0])
            return self.page("<p>noted</p>")
        if u.path == "/notes":
            return self.raw(",".join(NOTES))
        if u.path == "/flaky":
            VISITS["flaky"] += 1
            return self.page("<p>ok</p>" if VISITS["flaky"] > 1 else "<p>not yet</p>")
        if u.path == "/late":
            ms = int(q.get("ms", ["300"])[0])
            return self.page(f"<p id=s>loading</p><script>setTimeout(() => s.textContent = 'ready', {ms})</script>")
        if u.path == "/api/n":
            time.sleep(0.05)
            data = b'{"n": 3}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            return self.wfile.write(data)
        return self.page("<h1>Home</h1><a href=/late>Late</a>")


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(f"LISTENING http://127.0.0.1:{srv.server_address[1]}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    sys.exit(0)
