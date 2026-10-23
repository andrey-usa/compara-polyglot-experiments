"""A small site for primavera's own tests: `/flaky` fails its first visit
and passes after, `/late` shows its text after `?ms=`, and `/` links them."""
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

VISITS = {"flaky": 0}


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

    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
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
