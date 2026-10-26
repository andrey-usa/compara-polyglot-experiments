#!/usr/bin/env python3
"""Fixture for natural mode and profile hand-off.

/ is a page that looks for the signs of an automated browser, as anti-bot
scripts do, and reports what it found as JSON in window.__signals (the title
becomes "done"). /headers echoes the request headers. /login signs in: a
persistent cookie and a localStorage key. /whoami says who the cookie and the
storage say you are."""
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAGE = r"""<!doctype html><html><head><meta charset=utf-8><title>checking</title></head><body>
<pre id=out></pre>
<script>
(async () => {
  const s = {};
  s.webdriver = navigator.webdriver === true;
  s.ua_headless = /Headless/i.test(navigator.userAgent);
  const brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
  s.brands_headless = brands.some(b => /Headless/i.test(b.brand));
  s.brands_missing = !navigator.userAgentData;
  // DevTools serialises console arguments only while the Runtime domain is
  // on: a getter on an Error's stack runs then and only then.
  let touched = false;
  const e = new Error('x');
  Object.defineProperty(e, 'stack', { configurable: true, get() { touched = true; return ''; } });
  console.debug(e);
  await new Promise(r => setTimeout(r, 300));
  s.runtime_enabled = touched;
  s.no_plugins = navigator.plugins.length === 0;
  s.no_languages = !navigator.languages || navigator.languages.length === 0;
  s.no_chrome_object = typeof window.chrome === 'undefined';
  s.zero_outer = window.outerWidth === 0 || window.outerHeight === 0;
  s.default_headless_size = window.innerWidth === 800 && window.innerHeight === 600;
  s.screen_equals_window = screen.width === window.outerWidth && screen.height === window.outerHeight;
  try {
    const p = await navigator.permissions.query({ name: 'notifications' });
    s.permissions_mismatch = Notification.permission === 'denied' && p.state === 'prompt';
  } catch (_) { s.permissions_mismatch = false; }
  const w = await new Promise(res => {
    const blob = new Blob(['postMessage(navigator.userAgent)'], { type: 'text/javascript' });
    const wk = new Worker(URL.createObjectURL(blob));
    wk.onmessage = m => res(m.data);
    setTimeout(() => res(''), 2000);
  });
  s.worker_ua_headless = /Headless/i.test(w);
  const h = await (await fetch('/headers')).json();
  s.header_ua_headless = /Headless/i.test(h['user-agent'] || '');
  s.header_brands_headless = /Headless/i.test(h['sec-ch-ua'] || '');
  try {
    const gl = document.createElement('canvas').getContext('webgl');
    const dbg = gl && gl.getExtension('WEBGL_debug_renderer_info');
    s.info_webgl_renderer = dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : '';
  } catch (_) { s.info_webgl_renderer = ''; }
  s.info_ua = navigator.userAgent;
  s.info_size = [window.innerWidth, window.innerHeight, window.outerWidth, window.outerHeight, screen.width, screen.height];
  window.__signals = s;
  document.getElementById('out').textContent = JSON.stringify(s, null, 1);
  document.title = 'done';
})();
</script></body></html>"""

LOGIN = """<!doctype html><title>signed in</title><script>
localStorage.setItem('user', 'ada'); document.title = 'signed in';
</script>"""

WHOAMI = """<!doctype html><title>whoami</title><script>
document.title = JSON.stringify({cookie: document.cookie, user: localStorage.getItem('user')});
</script>"""

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        extra = []
        if self.path.startswith('/login'):
            body, ctype = LOGIN.encode(), 'text/html; charset=utf-8'
            extra.append(('Set-Cookie', 'sid=ada-session; Max-Age=86400; Path=/; HttpOnly'))
            extra.append(('Set-Cookie', 'seen=1; Max-Age=86400; Path=/'))
        elif self.path.startswith('/whoami'):
            body, ctype = WHOAMI.encode(), 'text/html; charset=utf-8'
        elif self.path.startswith('/headers'):
            body = json.dumps({k.lower(): v for k, v in self.headers.items()}).encode()
            ctype = 'application/json'
        else:
            body = PAGE.encode()
            ctype = 'text/html; charset=utf-8'
        if not isinstance(body, bytes):
            body = body.encode()
        self.send_response(200)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Accept-CH', 'Sec-CH-UA-Full-Version-List, Sec-CH-UA-Platform')
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

srv = ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1]) if len(sys.argv) > 1 else 0), H)
print(f"LISTENING http://127.0.0.1:{srv.server_address[1]}", flush=True)
srv.serve_forever()
