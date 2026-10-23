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
/wizard         a step that saves with fetch, then reloads into the next
/card-radios    radio cards whose real inputs are visually hidden
/combobox       a react-select-style dropdown: ArrowDown opens a listbox
                rendered at the end of <body>
/typed-only     an input whose app keeps only what was typed
/lazy-box       a scrollable box that loads more when scrolled to its end
/honesty        a decoy copy of an id, disabled and read-only controls
/xo-parent      a card form in a cross-site iframe (localhost in a 127.0.0.1
                page: its own process); ?same-site: cross-origin, same process

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


PAGES["/wizard"] = ("wizard", "", """
<script>
const step = new URLSearchParams(location.search).get('step') || '1';
document.write(step === '1'
  ? '<label>Name <input id=name></label> <button id=next type=button>Next</button>'
  : step === '2'
  ? '<label>Email <input id=email></label> <button id=finish type=button>Finish</button>'
  : '<p id=done>Saved ' + sessionStorage.getItem('name') + ' / ' + sessionStorage.getItem('email') + '</p>');
const save = (k, v, next) => fetch('/slow-echo?ms=150').then(() => { sessionStorage.setItem(k, v); location.href = '/wizard?step=' + next; });
if (step === '1') document.getElementById('next').onclick = () => save('name', document.getElementById('name').value, 2);
if (step === '2') document.getElementById('finish').onclick = () => save('email', document.getElementById('email').value, 3);
</script>""")

PAGES["/card-radios"] = ("card radios", ".sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}.card{border:1px solid #888;padding:10px;margin:4px;cursor:pointer;width:200px}", """
<div class=card data-for=s role=radio aria-checked=false>Small</div>
<div class=card data-for=m role=radio aria-checked=false>Medium</div>
<input type=radio name=size value=s id=in-s class=sr><input type=radio name=size value=m id=in-m class=sr>
<p id=picked>none</p>
<script>
document.querySelectorAll('.card').forEach((c) => c.addEventListener('click', () => {
  document.querySelectorAll('.card').forEach((x) => x.setAttribute('aria-checked', 'false'));
  c.setAttribute('aria-checked', 'true');
  document.getElementById('in-' + c.dataset.for).checked = true;
  document.getElementById('picked').textContent = c.dataset.for;
}));
</script>""")

PAGES["/combobox"] = ("combobox", ".opt{padding:4px 8px}.opt.hl{background:#bde}", """
<div id=cb role=combobox aria-expanded=false aria-haspopup=listbox style="border:1px solid #888;padding:6px;width:220px">
  <span id=cb-value>Select…</span> <input id=cb-input aria-label=Fruit autocomplete=off style="border:0;width:8em">
</div>
<p id=committed>none</p>
<script>
const OPTIONS = ['Apple', 'Apricot', 'Banana', 'Blueberry', 'Cherry'];
const cb = document.getElementById('cb'), input = document.getElementById('cb-input');
let open = false, list = OPTIONS;
function render() {
  document.querySelectorAll('.portal').forEach((e) => e.remove());
  if (!open) return;
  const r = cb.getBoundingClientRect(), d = document.createElement('div');
  d.className = 'portal'; d.setAttribute('role', 'listbox');
  d.style.cssText = `position:absolute;top:${r.bottom + scrollY}px;left:${r.left}px;border:1px solid #888;background:#fff`;
  list.slice(0, 3).forEach((o) => {
    const e = document.createElement('div'); e.className = 'opt'; e.setAttribute('role', 'option'); e.textContent = o;
    e.addEventListener('mousedown', (ev) => { ev.preventDefault(); choose(o); });
    d.appendChild(e);
  });
  document.body.appendChild(d);
}
function choose(o) { document.getElementById('cb-value').textContent = o; document.getElementById('committed').textContent = o; input.value = ''; list = OPTIONS; open = false; cb.setAttribute('aria-expanded', 'false'); render(); }
input.addEventListener('keydown', (ev) => { if (ev.key === 'ArrowDown') { ev.preventDefault(); open = true; cb.setAttribute('aria-expanded', 'true'); render(); } });
input.addEventListener('input', () => { list = OPTIONS.filter((o) => o.toLowerCase().includes(input.value.toLowerCase())); render(); });
// Only three options show at a time: Cherry needs typing to appear.
</script>""")

PAGES["/typed-only"] = ("typed only", "", """
<label>Amount <input id=amt></label> <button id=go type=button>Commit</button>
<p id=model>model=</p>
<script>
let model = '';
const amt = document.getElementById('amt');
amt.addEventListener('keydown', (e) => { if (!e.isTrusted) return;
  if (e.key.length === 1) model += e.key; else if (e.key === 'Backspace') model = amt.selectionStart === 0 && amt.selectionEnd === amt.value.length ? '' : model.slice(0, -1); });
amt.addEventListener('blur', () => { amt.value = model; });
document.getElementById('go').onclick = () => { document.getElementById('model').textContent = 'model=' + model; };
</script>""")

PAGES["/lazy-box"] = ("lazy box", "", """
<div id=list style="height:200px;overflow:auto;border:1px solid #888"></div>
<script>
let n = 0, busy = false;
const list = document.getElementById('list');
function more() { for (let i = 0; i < 20; i++) { n++; const d = document.createElement('div'); d.style.padding = '8px'; d.textContent = 'Item ' + n; list.appendChild(d); } }
more();
list.addEventListener('scroll', () => {
  if (!busy && list.scrollTop + list.clientHeight >= list.scrollHeight - 10 && n < 60) { busy = true; setTimeout(() => { more(); busy = false; }, 100); }
});
</script>""")


PAGES["/honesty"] = ("honesty", "", """
<button id=dup style="position:absolute;left:-9999px" onclick="out('decoy')">Decoy</button>
<button id=dup onclick="out('real')">Real action</button>
<button id=save disabled onclick="out('saved')">Save</button>
<button id=unlock-later disabled onclick="out('unlocked')">Unlocks soon</button>
<input id=ro readonly value=fixed> <input id=off disabled>
<article style="width:300px;padding:40px;border:1px solid #888"><h3>Carbon Gear</h3><div>$34.82</div>
<button aria-label="Add Carbon Gear to cart" onclick="out('added')">Add to cart</button></article>
<p id=out>none</p>
<script>
function out(t) { document.getElementById('out').textContent = t; }
setTimeout(() => { document.getElementById('unlock-later').disabled = false; }, 600);
</script>""")


PAGES["/effects"] = ("effects", "#strip{width:200px;overflow-x:auto;white-space:nowrap}#strip span{display:inline-block;width:150px}", """
<button id=late>Close</button>
<button id=noop>Does nothing</button>
<label><input type=checkbox id=agree> Agree</label>
<input id=q value=typed> <button id=clear>Clear</button>
<button id=send>Send</button>
<button id=later>Later</button>
<div id=strip><span>a</span><span>b</span><span>c</span></div> <button id=next>Next</button>
<a id=self href="#">Top</a> <button id=copy>Copy</button> <button id=clip>Copy link</button>
<button id=save>Save</button>
<x-panel id=panel></x-panel> <x-badge id=badge></x-badge>
<p id=out>none</p>
<script>
const out = (t) => { document.getElementById('out').textContent = t; };
// Close is wired only once the page has "loaded its scripts": here when
// the test says so (`wire()`), on a real page a moment after load.
function wire() { document.getElementById('late').onclick = () => out('closed'); }
// Clear sets the field from script: no DOM change, no event.
document.getElementById('clear').onclick = () => { document.getElementById('q').value = ''; };
// Send only starts a request.
document.getElementById('send').onclick = () => { fetch('/slow-echo?ms=400'); };
// Copy puts text on the clipboard and shows nothing.
document.addEventListener('copy', (e) => { e.clipboardData.setData('text/plain', 'ref 42'); e.preventDefault(); });
document.getElementById('copy').onclick = () => { document.execCommand('copy'); };
document.getElementById('clip').onclick = () => { navigator.clipboard.writeText(location.href).catch(() => {}); };
// Save shows its result only once the server answers (400 ms).
document.getElementById('save').onclick = () => fetch('/slow-echo?ms=400').then(() => out('saved'));
// `?telemetry`: every click is reported, as analytics do.
if (location.search.includes('telemetry')) document.addEventListener('click', (e) => { fetch('/event/click?on=' + e.target.id); }, true);
document.getElementById('later').onclick = () => setTimeout(() => out('later'), 80);
document.getElementById('next').onclick = () => { document.getElementById('strip').scrollLeft += 150; };
// A button in one shadow root changes text in another one.
customElements.define('x-badge', class extends HTMLElement {
  constructor() { super(); this.attachShadow({ mode: 'open' }).innerHTML = '<b>0</b>'; }
  set count(n) { this.shadowRoot.querySelector('b').textContent = n; }
});
customElements.define('x-panel', class extends HTMLElement {
  constructor() { super(); this.attachShadow({ mode: 'open' }).innerHTML = '<button id=bump>Bump</button>'; let n = 0;
    this.shadowRoot.querySelector('button').onclick = () => { document.getElementById('badge').count = ++n; }; }
});
</script>""")


def xo_parent(port, child_host):
    return ("xo parent", "iframe{width:420px;height:170px;border:1px solid #888}", f"""
<h1>Checkout</h1>
<h2>Payment details</h2>
<p id=paid>unpaid</p>
<iframe id=card title="Secure card input" src="http://{child_host}:{port}/xo-child"></iframe>
<script>
addEventListener('message', (e) => {{ if (e.data && e.data.paid) document.getElementById('paid').textContent = 'paid ' + e.data.paid; }});
</script>""")


PAGES["/xo-child"] = ("card frame", "", """
<label>Card number <input id=cardnumber autocomplete=off></label>
<label><input type=checkbox id=save> Save card</label>
<select id=exp><option>01/27</option><option>02/28</option></select>
<button id=pay type=button>Pay</button>
<p id=status>idle</p>
<script>
document.getElementById('pay').onclick = () => {
  const n = document.getElementById('cardnumber').value;
  document.getElementById('status').textContent = 'sent ' + n;
  parent.postMessage({ paid: n.slice(-4) + (document.getElementById('save').checked ? ' saved' : '') + ' ' + document.getElementById('exp').value }, '*');
};
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
        q = parse_qs(u.query, keep_blank_values=True)
        if u.path == "/frameset":
            return self.send(FRAMESET)
        if u.path == "/frame-top":
            return self.send(FRAME_TOP)
        if u.path == "/frame-text":
            t = q.get("t", ["?"])[0]
            return self.send(f"<!doctype html><html><body><div id=content>{t}</div></body></html>")
        if u.path.startswith("/event/"):
            return self.send("ok", "text/plain")
        if u.path == "/slow-echo":
            import time
            time.sleep(int(q.get("ms", ["100"])[0]) / 1000)
            return self.send("ok", "text/plain")
        if u.path == "/file.csv":
            return self.send("id,name\n1,Ada\n2,Grace\n", "text/csv",
                             [("Content-Disposition", 'attachment; filename="report.csv"')])
        if u.path == "/slow":
            page = slow_page(int(q.get("ms", ["2000"])[0]))
        elif u.path == "/xo-parent":
            # localhost and 127.0.0.1 are different sites: the frame is
            # cross-site, so Chrome runs it in a process of its own.
            # `?same-site` keeps it cross-origin but in the same process.
            port = self.server.server_address[1]
            page = xo_parent(port, "127.0.0.1" if "same-site" in q else "localhost")
            if "same-site" in q:
                page = (page[0], page[1], page[2].replace(f"http://127.0.0.1:{port}/xo-child", f"http://127.0.0.1:{XO_PORT[0]}/xo-child"))
        elif u.path in PAGES:
            page = PAGES[u.path]
        else:
            self.send_response(404)
            self.end_headers()
            return
        title, style, body = page
        self.send(HEAD.format(title=title, style=style) + body + "</body></html>")


XO_PORT = [0]

if __name__ == "__main__":
    # A second port: another origin on the same site (127.0.0.1).
    xo = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    XO_PORT[0] = xo.server_address[1]
    import threading
    threading.Thread(target=xo.serve_forever, daemon=True).start()
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(f"LISTENING http://127.0.0.1:{srv.server_address[1]}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    sys.exit(0)
