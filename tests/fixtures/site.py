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

# `/form` is a form; with `?auto=1` it uses itself, as a person would, for
# `primavera record` with nobody at the browser (`?auto=2`: after 1.5 s, for
# `record --extend`, which runs the scenario first).
FORM = """
<h1>Order</h1>
<label>Email <input id=email></label>
<label><input type=checkbox id=gift> Gift wrap</label>
<label>Size <select id=size><option>Small</option><option>Large</option></select></label>
<x-tip id=tip></x-tip>
<a href=/done id=go>Place order</a>
<script>
// A web component's icon button: no name, inside a shadow root.
customElements.define('x-tip', class extends HTMLElement { connectedCallback() {
  if (!this.shadowRoot) this.attachShadow({mode: 'open'}).innerHTML = '<button><svg width=12 height=12></svg></button>';
  this.shadowRoot.querySelector('button').onclick = () => { this.dataset.open = '1'; };
} });
const auto = new URLSearchParams(location.search).get('auto');
if (auto) setTimeout(() => {
  email.click(); email.value = 'ada@example.com'; email.dispatchEvent(new Event('change', {bubbles: true}));
  gift.click();
  tip.shadowRoot.querySelector('button').click();
  size.value = 'Large'; size.dispatchEvent(new Event('change', {bubbles: true}));
  setTimeout(() => go.click(), 100);
}, auto === '2' ? 1500 : 300);
</script>
"""

# `/bar`: a consent bar over the Buy button; its Accept takes it off the
# page (`?auto=1`: Accept, then Buy, for `primavera record`).
BAR = """
<h1>Shop</h1>
<button onclick="document.body.append('Bought')" style="margin-top:80vh">Buy now</button>
<div id=bar style="position:fixed;left:0;right:0;bottom:0;height:40vh;background:#eee">Cookies
  <button onclick="bar.remove()">Accept</button></div>
<script>
if (new URLSearchParams(location.search).get('auto')) setTimeout(() => {
  bar.querySelector('button').click();
  setTimeout(() => document.querySelector('button').click(), 100);
}, 300);
</script>
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
        if u.path == "/form":
            return self.page(FORM)
        if u.path == "/bar":
            return self.page(BAR)
        if u.path == "/done":
            return self.page("<h1>Thanks</h1>")
        if u.path == "/framed":
            # A button inside a frame whose name a test misspells.
            return self.page('<iframe srcdoc="<button>Subscibe</button>"></iframe>')
        if u.path == "/a11y":
            # An image without alt and a button without a name, outside #ok.
            return self.page('<main><h1>Gallery</h1><section id=ok><p>Fine here.</p></section>'
                             '<img src="/report.csv" width=10 height=10><button><svg width=8 height=8></svg></button></main>')
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
        if u.path == "/api/hop":
            self.send_response(302)
            self.send_header("Location", "/api/me")
            self.send_header("Set-Cookie", "sid=hop; Path=/")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
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
    port = int(sys.argv[sys.argv.index("--port") + 1]) if "--port" in sys.argv else 0
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"LISTENING http://127.0.0.1:{srv.server_address[1]}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    sys.exit(0)
