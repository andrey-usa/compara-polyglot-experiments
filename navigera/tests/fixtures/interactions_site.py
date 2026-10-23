#!/usr/bin/env python3
"""Fixture for the interactions that real practice sites showed navigera
getting wrong, one page each, built the way those sites behave:

/hidden-layers  a second click on a button now covered by another (the
                page notes a click a user couldn't have made)
/iframe-form    a form inside a same-origin iframe
/frameset       nested framesets with text in each frame
/srcdoc         a button inside nested srcdoc iframes
/multi-select   a <select multiple>
/nbsp-select    options whose labels have non-breaking spaces
/slow?ms=N      a button that appears after N ms
/download       a file sent as an attachment, and one made from a Blob
/shadow-css     icon-only buttons inside (nested) open shadow roots
/entry-modal    a modal that slides in and wires its Close button only
                once it has finished
/overlapped     an input whose centre is under a panel until the box
                scrolls; typing into a covered field clears it
/right-click    a button that reports right and double clicks

Prints "LISTENING http://127.0.0.1:<port>" and serves until killed."""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HEAD = "<!doctype html><html><head><meta charset=utf-8><title>{title}</title><style>{style}</style></head><body>"

PAGES = {}

PAGES["/hidden-layers"] = ("hidden layers", "", """
<div id=spot style="position:relative;height:60px">
  <button id=greenButton style="position:absolute;left:0;top:0;width:120px;height:40px;background:#3a3">Button</button>
</div>
<div id=log></div>
<script>
const green = document.getElementById('greenButton');
green.addEventListener('click', () => {
  if (document.getElementById('blueButton')) {
    const p = document.createElement('p');
    p.className = 'bg-warning';
    p.textContent = 'green was clicked while covered';
    document.getElementById('log').appendChild(p);
    return;
  }
  const b = document.createElement('button');
  b.id = 'blueButton';
  b.textContent = 'Button';
  b.style.cssText = 'position:absolute;left:0;top:0;width:120px;height:40px;background:#33a;z-index:2';
  green.parentElement.appendChild(b);
});
</script>""")

PAGES["/iframe-form"] = ("iframe form", "iframe{width:420px;height:160px;border:1px solid #888}", """
<h1>Newsletter</h1>
<iframe id=email-subscribe src="/iframe-inner"></iframe>""")

PAGES["/iframe-inner"] = ("inner", "", """
<label>Email <input id=email type=email></label>
<button id=btn-subscribe type=button>Subscribe</button>
<p id=msg></p>
<script>
document.getElementById('btn-subscribe').addEventListener('click', () => {
  document.getElementById('msg').textContent = 'Thanks ' + document.getElementById('email').value;
});
</script>""")

FRAMESET = """<!doctype html><html><head><title>frames</title></head>
<frameset rows="50%,50%">
  <frame name=frame-top src="/frame-top">
  <frame name=frame-bottom src="/frame-text?t=BOTTOM">
</frameset></html>"""

FRAME_TOP = """<!doctype html><html><head><title>top</title></head>
<frameset cols="33%,33%,33%">
  <frame name=frame-left src="/frame-text?t=LEFT">
  <frame name=frame-middle src="/frame-text?t=MIDDLE">
  <frame name=frame-right src="/frame-text?t=RIGHT">
</frameset></html>"""

PAGES["/srcdoc"] = ("srcdoc", "iframe{width:420px;height:150px}", """
<p id=out>nothing yet</p>
<div id=host></div>
<script>
const inner = '<button id=deep onclick="parent.parent.document.getElementById(\\'out\\').textContent = \\'deep clicked\\'">Deep button</button>';
const outer = document.createElement('iframe');
outer.srcdoc = '<p>outer frame</p><iframe id=innerframe></iframe>';
outer.addEventListener('load', () => { outer.contentDocument.getElementById('innerframe').srcdoc = inner; });
document.getElementById('host').appendChild(outer);
</script>""")

PAGES["/multi-select"] = ("multi select", "", """
<label for=cars>Cars</label>
<select id=cars multiple size=4>
  <option value=volvo>Volvo</option><option value=saab>Saab</option>
  <option value=opel>Opel</option><option value=audi>Audi</option>
</select>
<p id=picked></p>
<script>
const cars = document.getElementById('cars');
cars.addEventListener('change', () => {
  document.getElementById('picked').textContent = [...cars.selectedOptions].map(o => o.value).join(',');
});
</script>""")

PAGES["/nbsp-select"] = ("nbsp select", "", """
<select id=city>
  <option value=pick>Pick one</option>
  <option value=ny>New&nbsp;York</option>
  <option value=sf>San&nbsp;Francisco</option>
</select>""")

PAGES["/download"] = ("download", "", """
<a id=dl href="/file.csv">Download CSV</a>
<button id=blob type=button>Save notes</button>
<script>
document.getElementById('blob').addEventListener('click', () => {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob(['notes from a blob\\n'], { type: 'text/plain' }));
  a.download = 'notes.txt';
  document.body.appendChild(a);
  a.click();
  a.remove();
});
</script>""")

PAGES["/shadow-css"] = ("shadow css", "", """
<guid-generator></guid-generator>
<outer-box></outer-box>
<p id=nested-out>untouched</p>
<script>
customElements.define('guid-generator', class extends HTMLElement {
  connectedCallback() {
    const root = this.attachShadow({ mode: 'open' });
    root.innerHTML = '<input id=editField><button id=buttonGenerate><svg width=16 height=16><circle cx=8 cy=8 r=6/></svg></button>';
    root.getElementById('buttonGenerate').addEventListener('click', () => {
      root.getElementById('editField').value = 'guid-' + Math.random().toString(16).slice(2, 10);
    });
  }
});
customElements.define('inner-box', class extends HTMLElement {
  connectedCallback() {
    const root = this.attachShadow({ mode: 'open' });
    root.innerHTML = '<button id=nested><svg width=16 height=16><rect width=12 height=12/></svg></button>';
    root.getElementById('nested').addEventListener('click', () => { document.getElementById('nested-out').textContent = 'nested clicked'; });
  }
});
customElements.define('outer-box', class extends HTMLElement {
  connectedCallback() { this.attachShadow({ mode: 'open' }).innerHTML = '<div><inner-box></inner-box></div>'; }
});
</script>""")

PAGES["/entry-modal"] = ("entry modal", """
.modal{position:fixed;left:20%;top:15%;width:50%;padding:20px;background:#fff;border:1px solid #333;
  opacity:0;transform:translateY(-60px);transition:opacity .7s ease-out, transform .7s ease-out;display:none}
.modal.show{opacity:1;transform:none}""", """
<p>Some page</p>
<div class=modal id=modal role=dialog aria-label="This is a modal window">
  <h3>This is a modal window</h3>
  <p>It's commonly used to encourage a user to take an action.</p>
  <button id=close type=button>Close</button>
</div>
<script>
const modal = document.getElementById('modal');
setTimeout(() => {
  modal.style.display = 'block';
  void modal.offsetWidth;
  modal.classList.add('show');
  // The page's script wires the button once the modal has finished opening.
  modal.addEventListener('transitionend', () => {
    document.getElementById('close').onclick = () => { modal.classList.remove('show'); modal.style.display = 'none'; };
  }, { once: true });
}, 100);
</script>""")

PAGES["/overlapped"] = ("overlapped", """
#wrap{position:relative;width:300px;height:120px}
#scroller{height:120px;overflow-y:scroll;border:1px solid #999}
#panel{position:absolute;left:0;right:16px;top:45px;bottom:0;background:#eee;border-top:1px solid #999}
input{display:block;margin:10px}""", """
<div id=wrap>
  <div id=scroller>
    <input id=id placeholder=Id>
    <input id=name placeholder=Name>
    <div style="height:300px"></div>
  </div>
  <div id=panel>Subscribe for news</div>
</div>
<p id=status>ok</p>
<script>
const name = document.getElementById('name');
name.addEventListener('input', () => {
  const r = name.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  if (hit !== name) { name.value = ''; document.getElementById('status').textContent = 'cleared: covered while typing'; }
});
</script>""")

PAGES["/right-click"] = ("right click", "", """
<button id=rightClickBtn type=button>Right Click Me</button>
<button id=doubleClickBtn type=button>Double Click Me</button>
<p id=msg></p>
<script>
document.getElementById('rightClickBtn').addEventListener('contextmenu', (e) => {
  e.preventDefault();
  document.getElementById('msg').textContent = 'You have done a right click';
});
document.getElementById('rightClickBtn').addEventListener('click', () => {
  document.getElementById('msg').textContent = 'That was a left click';
});
document.getElementById('doubleClickBtn').addEventListener('dblclick', () => {
  document.getElementById('msg').textContent = 'You have done a double click';
});
</script>""")


def slow_page(ms):
    return ("slow", "", f"""
<p>Loading…</p>
<script>
setTimeout(() => {{
  const b = document.createElement('button');
  b.id = 'late';
  b.textContent = 'Late button';
  b.onclick = () => {{ document.querySelector('p').textContent = 'late clicked'; }};
  document.body.appendChild(b);
}}, {ms});
</script>""")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, body, ctype="text/html; charset=utf-8", extra=()):
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
        if u.path == "/frameset":
            return self.send(FRAMESET)
        if u.path == "/frame-top":
            return self.send(FRAME_TOP)
        if u.path == "/frame-text":
            t = q.get("t", ["?"])[0]
            return self.send(f"<!doctype html><html><body><div id=content>{t}</div></body></html>")
        if u.path == "/file.csv":
            return self.send("id,name\n1,Ada\n2,Grace\n", "text/csv",
                             [("Content-Disposition", 'attachment; filename="report.csv"')])
        if u.path == "/slow":
            page = slow_page(int(q.get("ms", ["2000"])[0]))
        elif u.path in PAGES:
            page = PAGES[u.path]
        else:
            self.send_response(404)
            self.end_headers()
            return
        title, style, body = page
        self.send(HEAD.format(title=title, style=style) + body + "</body></html>")


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(f"LISTENING http://127.0.0.1:{srv.server_address[1]}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    sys.exit(0)
