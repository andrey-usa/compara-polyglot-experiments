#!/usr/bin/env python3
"""Flake gauntlet: one local app built to break UI tests.

Stdlib only. Two origins on 127.0.0.1 (different ports) so cross-origin
iframe pages are testable offline.

  python3 server.py --port 0 --seed 7 --jitter 150
    # prints: LISTENING http://127.0.0.1:<port>  XO http://127.0.0.1:<xoport>

Deterministic for a given seed: every timing (late-render delay, XHR
latency, toast duration, remount period, handler bind delay) is drawn from
one seeded rng at startup. --jitter <ms> adds up to that much extra random
latency to the slow/dynamic endpoints, from the same rng stream.

Every page POSTs what it saw to /event/<page> (the page's own ground
truth). GET /verdict/<page> says whether the actions the page saw were a
legitimate user path or a cheat (e.g. a click that reached a covered
element). GET /reset clears all events. GET / lists the pages.
"""
import argparse
import html as htmlmod
import json
import random
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

EVENTS = {}          # page -> [event dicts]
LOCK = threading.Lock()
CFG = {}             # filled at startup: ports, seed, delays
START_TS = time.time()


def log_event(page, ev):
    ev = dict(ev)
    ev.setdefault("ts", time.time())
    with LOCK:
        EVENTS.setdefault(page, []).append(ev)


def verdict(page):
    """Return (legitimate: bool, reason: str) from the recorded events."""
    with LOCK:
        evs = list(EVENTS.get(page, []))
    fn = VERDICTS.get(page)
    if not fn:
        return False, "no verdict defined for page"
    return fn(evs)


# ---------------------------------------------------------------- pages

LOG_JS = """
<script>
var TELEMETRY_OFF = false;
var __evbuf = [];
function log(type, detail){
  var body = Object.assign({type:type, t:performance.now()/1000}, detail||{});
  if (TELEMETRY_OFF) { __evbuf.push(body); return; }
  fetch('/event/'+PAGE,{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify(body)}).catch(function(){});
}
function __flushEvents(){
  if (!__evbuf.length) return;
  var batch = __evbuf.splice(0, __evbuf.length);
  fetch('/events/'+PAGE,{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify(batch)}).catch(function(){});
}
</script>
"""

def shell(page, title, user_path_comment, body):
    # Intended user path, in a comment at the top, per the gauntlet brief.
    t = htmlmod.escape(title)
    return ("<!doctype html><html><head><meta charset=\"utf-8\">"
            f"<title>{t}</title>"
            "<style>body{font-family:sans-serif;max-width:720px;margin:2em auto}"
            "button{font-size:1em;padding:6px 14px}</style></head><body>"
            f"<!-- INTENDED USER PATH: {user_path_comment} -->"
            f"<h1>{t}</h1>"
            f"<script>var PAGE={json.dumps(page)};</script>"
            + (LOG_JS.replace("var TELEMETRY_OFF = false;",
                              "var TELEMETRY_OFF = true;")
               if CFG.get("telemetry") == 0 else LOG_JS) +
            f"{body}"
            "</body></html>")


PAGES = {}

def page(name):
    def deco(fn):
        PAGES[name] = fn
        return fn
    return deco


def draw(name):
    """The random source for one page's timings: the run's one stream, or
    with --fixed-delays a stream of the page's own, started over on every
    load, so every load waits the same (a benchmark compares tools, not
    draws)."""
    if CFG.get("fixed"):
        return random.Random("%d:%s" % (CFG["seed"], name))
    return CFG["rng"]


@page("late-render")
def late_render():
    delay = 1.0 + draw("late-render").random() * 7.0  # 1..8 s, seeded
    CFG["late_render_delay"] = delay
    return shell("late-render", "Late render",
        "Wait until the app says Ready (data arrives after a seeded 1-8 s "
        "delay), then click Continue. A tool that clicks or asserts before "
        "the render is racing the app.",
        f"""<div id="app"><p>Loading&hellip;</p></div>
<script>
setTimeout(function(){{
  document.getElementById('app').innerHTML =
    '<p id="ready">Ready</p><button id="go">Continue</button>';
  log('rendered', {{delay:{delay:.3f}}});
  document.getElementById('go').addEventListener('click', function(){{
    log('commit', {{}}); document.getElementById('app').innerHTML='<p>Done</p>';
  }});
}}, {delay*1000:.0f});
</script>""")


def late_render_verdict(evs):
    rendered = next((e for e in evs if e["type"] == "rendered"), None)
    commit = next((e for e in evs if e["type"] == "commit"), None)
    if not commit:
        return False, "no commit seen"
    if not rendered:
        return False, "commit without a render — clicked/asserted before the app rendered (race)"
    if commit["ts"] < rendered["ts"]:
        return False, "commit predates the render event"
    return True, f"commit {commit['ts']-rendered['ts']:.2f}s after render"


@page("remount")
def remount():
    period = 1.5
    return shell("remount", "Node re-mounted on every render",
        "Click the button. The button is destroyed and recreated (like a "
        "React key change) every 1.5 s, each mount slightly offset. A tool "
        "that snapshots the element and clicks late hits a stale "
        "node/position and misses.",
        """<div id="slot"></div><p id="pos"></p>
<script>
var gen = 0;
function mount(){
  gen++;
  var dx = Math.round((Math.random()*40-20)), dy = Math.round((Math.random()*30-15));
  var b = document.createElement('button');
  b.id = 'target'; b.textContent = 'Click me (gen '+gen+')';
  b.style.transform = 'translate('+dx+'px,'+dy+'px)';
  var g = gen;
  b.addEventListener('click', function(){
    log('commit', {clicked_gen:g, alive_gen:gen});
    document.getElementById('pos').textContent = 'committed gen '+g;
  });
  var slot = document.getElementById('slot');
  slot.innerHTML=''; slot.appendChild(b);
}
mount();
setInterval(mount, 1500);
</script>""")


def remount_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen (stale snapshot probably missed)"
    c = commits[-1]
    if c.get("clicked_gen") != c.get("alive_gen"):
        return False, (f"torn click: hit gen {c.get('clicked_gen')} while gen "
                       f"{c.get('alive_gen')} was mounted")
    return True, f"hit the live mount (gen {c['clicked_gen']})"


@page("transition-transform")
def transition_transform():
    return shell("transition-transform", "CSS transition + transform on the target",
        "Click the button. It slides 300 px over 1.2 s right after load. A "
        "tool that clicks the pre-transition coordinates misses.",
        """<button id="go" style="transition:transform 1.2s ease;transform:translateX(0)">Click me</button>
<p id="out"></p>
<script>
requestAnimationFrame(function(){
  document.getElementById('go').style.transform = 'translateX(300px)';
});
document.getElementById('go').addEventListener('click', function(ev){
  var r = ev.target.getBoundingClientRect();
  log('commit', {x:ev.clientX, y:ev.clientY,
    inside_live_box:(ev.clientX>=r.left&&ev.clientX<=r.right&&ev.clientY>=r.top&&ev.clientY<=r.bottom)});
  document.getElementById('out').textContent='clicked';
});
</script>""")


def transition_transform_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen (clicked the pre-transition position)"
    c = commits[-1]
    if not c.get("inside_live_box"):
        return False, "click coordinates outside the button's live box"
    return True, "hit the button at its live position"


@page("toast-cover")
def toast_cover():
    dur = 2.0
    return shell("toast-cover", "Toast covers the button for 2 s",
        "Click the button only after the toast dismisses (2 s). The toast "
        "has its own click handler. A tool whose click bypasses hit-testing "
        "(clicks covered elements) commits while covered — a cheat.",
        f"""<!-- NOTES: elementFromPoint at the button's center returns
#go, not #toast, while the toast is visible: the toast (fixed, x 40-280, y 40-130
incl. padding) never geometrically covers #go (in flow below it). A click on #go
while the toast is visible passes hit-testing; it races the app's ready state. -->
<div id="toast" style="position:fixed;left:40px;top:40px;width:220px;height:70px;
background:#333;color:#fff;padding:10px;z-index:10">Saving&hellip;</div>
<button id="go" style="margin-top:120px">Submit</button><p id="out"></p>
<script>
var toastGoneAt = null;
setTimeout(function(){{
  document.getElementById('toast').style.display='none';
  toastGoneAt = performance.now()/1000; log('toast-gone', {{}});
}}, {dur*1000:.0f});
document.getElementById('toast').addEventListener('click', function(){{
  log('toast-clicked', {{}});  /* a real hit-test would land here */
}});
document.getElementById('go').addEventListener('click', function(){{
  log('commit', {{toast_visible: toastGoneAt===null}});
  document.getElementById('out').textContent='submitted';
}});
</script>""")


def toast_cover_verdict(evs):
    toast_clicks = [e for e in evs if e["type"] == "toast-clicked"]
    commits = [e for e in evs if e["type"] == "commit"]
    gone = next((e for e in evs if e["type"] == "toast-gone"), None)
    if toast_clicks and not commits:
        return True, "click correctly landed on the covering toast (hit-test respected)"
    if not commits:
        return False, "no commit seen"
    c = commits[-1]
    if c.get("toast_visible"):
        return False, ("committed while the toast was visible "
                       "(raced the app's ready state)")
    if not gone:
        return False, "commit without the toast ever dismissing"
    return True, "committed after the toast dismissed"


@page("modal-late-handlers")
def modal_late_handlers():
    bind_delay = 1.5
    return shell("modal-late-handlers", "Modal whose handlers bind late",
        "Open is automatic: the modal shows on load. Click Close only after "
        "its handler binds (1.5 s after show). Clicking earlier silently "
        "does nothing — the JS handler-binding race (navigera N8 family).",
        f"""<div id="modal" style="border:2px solid #000;padding:20px">
<p>Modal dialog</p><button id="close">Close</button></div><p id="out"></p>
<script>
log('modal-shown', {{}});
var boundAt = null;
setTimeout(function(){{
  document.getElementById('close').addEventListener('click', function(){{
    log('commit', {{}}); document.getElementById('out').textContent='closed';
  }});
  boundAt = performance.now()/1000; log('handlers-bound', {{}});
}}, {bind_delay*1000:.0f});
document.getElementById('close').addEventListener('click', function(){{
  if (boundAt===null) log('early-click-noop', {{}});
}}, true);
</script>""")


def modal_late_handlers_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    bound = next((e for e in evs if e["type"] == "handlers-bound"), None)
    early = [e for e in evs if e["type"] == "early-click-noop"]
    if not commits:
        if early:
            return False, ("clicked before handlers bound — the click was a "
                           "silent no-op (handler-binding race)")
        return False, "no commit seen"
    c = commits[-1]
    if bound and c["ts"] < bound["ts"]:
        return False, "commit predates handler binding — impossible via real clicks"
    return True, ("committed after handlers bound" +
                  (f" (plus {len(early)} early no-op click(s))" if early else ""))


@page("slow-xhr")
def slow_xhr():
    rng = draw("slow-xhr")
    base = 2.0 + rng.random() * 2.0                    # 2..4 s, seeded
    jit = rng.random() * CFG["jitter_ms"] / 1000.0
    CFG["slow_xhr_delay"] = base + jit
    return shell("slow-xhr", "Slow and jittery XHR",
        "Click Load, then use the result. The XHR takes a seeded 2-4 s plus "
        "jitter. Reading #out before it resolves reads stale content.",
        """<button id="load">Load</button><p id="out">—</p>
<button id="use" disabled>Use result</button><p id="done"></p>
<script>
document.getElementById('load').addEventListener('click', function(){
  log('xhr-start', {});
  fetch('/slow.json').then(function(r){return r.json();}).then(function(j){
    document.getElementById('out').textContent = j.value;
    document.getElementById('use').disabled = false;
    log('xhr-done', {value:j.value});
  });
});
document.getElementById('use').addEventListener('click', function(){
  var v = document.getElementById('out').textContent;
  log('commit', {observed:v});
  document.getElementById('done').textContent = 'used: '+v;
});
</script>""")


def slow_xhr_verdict(evs):
    done = next((e for e in evs if e["type"] == "xhr-done"), None)
    commits = [e for e in evs if e["type"] == "commit"]
    starts = [e for e in evs if e["type"] == "xhr-start"]
    if len(starts) > 1:
        return False, f"fired {len(starts)} XHRs — impatient double-fire"
    if not commits:
        return False, "no commit seen"
    c = commits[-1]
    if not done:
        return False, "committed without the XHR resolving — read stale content"
    if c["ts"] < done["ts"]:
        return False, "commit predates XHR resolution"
    if c.get("observed") in (None, "", "—"):
        return False, "committed but observed empty/stale content"
    return True, f"used the resolved value {c.get('observed')!r}"


@page("lazy-list")
def lazy_list():
    return shell("lazy-list", "Lazy list loading on scroll",
        "Scroll to the bottom repeatedly to load more items; item 45 has "
        "the target button. Interacting before scrolling to it is "
        "impossible — the node does not exist yet.",
        """<div id="list" style="height:300px;overflow:auto;border:1px solid #888"></div>
<script>
var n = 0, loading = false;
function addItems(k){
  var list = document.getElementById('list');
  for (var i=0;i<k;i++){
    n++;
    var d = document.createElement('div');
    d.style.padding='8px'; d.style.borderBottom='1px solid #ddd';
    d.id = 'item-'+n;
    d.innerHTML = 'Item '+n + (n===45 ? ' <button id="target45">Pick 45</button>' : '');
    list.appendChild(d);
    if (n===45) document.getElementById('target45').addEventListener('click', function(){
      log('commit', {item:45});
    });
  }
}
addItems(20);
document.getElementById('list').addEventListener('scroll', function(ev){
  var el = ev.target;
  log('scroll', {scrollTop:Math.round(el.scrollTop)});
  if (!loading && el.scrollTop + el.clientHeight >= el.scrollHeight - 20 && n < 60){
    loading = true;
    setTimeout(function(){ addItems(20); loading=false; log('loaded', {n:n}); }, 400);
  }
});
</script>""")


def lazy_list_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    scrolls = [e for e in evs if e["type"] == "scroll"]
    if not commits:
        return False, "no commit seen"
    if not scrolls:
        return False, "committed item 45 without scrolling — the node did not exist (cheat)"
    if commits[-1]["ts"] < scrolls[0]["ts"]:
        return False, "commit predates any scroll"
    return True, f"scrolled ({len(scrolls)} scroll events) then picked item 45"


@page("redirect-chain")
def redirect_chain():
    # Served at /redir/1 ; hops carry a token so direct gotos are detectable.
    return None  # handled by special-case routing below


def redir_page(hop, nxt, token):
    label = {1: "one", 2: "two", 3: "three"}[hop]
    return ("<!doctype html><html><head><meta charset=\"utf-8\">"
            f"<title>Redirect {label}</title></head><body>"
            f"<!-- INTENDED USER PATH: follow the chain hop by hop (each hop "
            f"logs itself with the chain token); do not jump to /redir/final "
            f"directly. --><h1>Hop {hop}</h1><p>redirecting&hellip;</p>"
            "<script>"
            f"fetch('/event/redirect-chain',{{method:'POST',headers:{{'Content-Type':'application/json'}},"
            f"body:JSON.stringify({{type:'hop',hop:{hop},token:{json.dumps(token)}}})}})"
            f".finally(function(){{location.replace({json.dumps(nxt + '?t=' + token)});}});"
            "</script></body></html>")


def redirect_chain_verdict(evs):
    hops = sorted({e.get("hop") for e in evs if e["type"] == "hop"})
    finals = [e for e in evs if e["type"] == "final"]
    tokens = {e.get("token") for e in evs if e.get("token")}
    if len(tokens) > 1:
        return False, "mixed chain tokens"
    if not finals:
        return False, "never reached the final page"
    if hops != [1, 2, 3]:
        return False, (f"reached final with hops {hops} — jumped the chain "
                       "(direct goto)")
    return True, "followed hops 1→2→3→final with one token"


@page("nested-iframes")
def nested_iframes():
    return shell("nested-iframes", "Nested same-origin iframes",
        "Drill into outer then inner iframe and submit the innermost form. "
        "The commit must come from depth 2.",
        """<iframe id="outer" src="/nest/outer" width="500" height="300"></iframe>""")


@page("nest-outer")
def nest_outer():
    return ("<!doctype html><html><body>"
            "<!-- INTENDED USER PATH: outer frame; the real form is one level deeper. -->"
            "<p>Outer frame</p>"
            '<iframe id="inner" src="/nest/inner" width="400" height="200"></iframe>'
            "</body></html>")


@page("nest-inner")
def nest_inner():
    return ("<!doctype html><html><body>"
            "<!-- INTENDED USER PATH: fill the name and submit; the commit "
            "must be logged at depth 2. -->"
            "<script>var PAGE='nested-iframes';</script>" + LOG_JS +
            "<form id=\"f\"><input id=\"name\" aria-label=\"name\">"
            "<button>Submit</button></form>"
            "<script>document.getElementById('f').addEventListener('submit',function(e){"
            "e.preventDefault();"
            "log('commit',{depth:2,value:document.getElementById('name').value});});"
            "</script></body></html>")


def nested_iframes_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen"
    c = commits[-1]
    if c.get("depth") != 2:
        return False, f"commit came from depth {c.get('depth')}, not the innermost frame"
    return True, f"committed from depth 2: {c.get('value')!r}"


@page("xo-iframe")
def xo_iframe():
    xoport = CFG["xo_port"]
    return shell("xo-iframe", "Cross-origin iframe (second port)",
        "Fill and submit the payment form inside the cross-origin iframe "
        "(other port = other origin). The iframe page logs its own commit "
        "to its origin's /event/xo-iframe.",
        f"""<iframe id="pay" title="Secure payment input frame"
 src="http://127.0.0.1:{xoport}/xo-form" width="400" height="200"></iframe>""")


@page("xo-form")
def xo_form():
    # Served by the SECOND server (other origin). Posts to its own /event/xo-iframe.
    return ("<!doctype html><html><head><meta charset=\"utf-8\">"
            "<title>Payment</title></head><body>"
            "<!-- INTENDED USER PATH: fill cardholder + card number inside the "
            "cross-origin frame and submit; commit is logged by the frame "
            "itself. -->"
            "<script>var PAGE='xo-iframe';</script>" + LOG_JS +
            "<form id=\"f\"><input id=\"cardholder\" aria-label=\"Cardholder\">"
            "<input id=\"card\" aria-label=\"Card number\">"
            "<button>Pay</button></form><p id=\"out\"></p>"
            "<script>document.getElementById('f').addEventListener('submit',function(e){"
            "e.preventDefault();"
            "var v={cardholder:document.getElementById('cardholder').value,"
            "card:document.getElementById('card').value};"
            "log('commit',v);document.getElementById('out').textContent='paid';});"
            "</script></body></html>")


def xo_iframe_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen from the cross-origin frame"
    c = commits[-1]
    if not c.get("cardholder") or not c.get("card"):
        return False, "commit missing field values"
    return True, f"cross-origin form committed: {c.get('cardholder')!r}"


@page("shadow-open-named")
def shadow_open_named():
    return shell("shadow-open-named", "Open shadow DOM, accessible name present",
        "Reach into the open shadow root and click the button named "
        "'Pay now'.",
        """<div id="host"></div><p id="out"></p>
<script>
var root = document.getElementById('host').attachShadow({mode:'open'});
root.innerHTML = '<button id="pay">Pay now</button>';
root.getElementById('pay').addEventListener('click', function(){
  log('commit', {named:true}); document.getElementById('out').textContent='paid';
});
</script>""")


def shadow_open_named_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen (shadow root not pierced)"
    return True, "reached the named button inside the open shadow root"


@page("shadow-open-unnamed")
def shadow_open_unnamed():
    return shell("shadow-open-unnamed", "Open shadow DOM, no accessible name",
        "Reach into the open shadow root and click the icon-only button "
        "(no accessible name). Tests nameless targeting.",
        """<div id="host"></div><p id="out"></p>
<script>
var root = document.getElementById('host').attachShadow({mode:'open'});
root.innerHTML = '<button id="iconbtn" aria-hidden="true">\u25b6</button>';
root.getElementById('iconbtn').addEventListener('click', function(){
  log('commit', {named:false}); document.getElementById('out').textContent='played';
});
</script>""")


def shadow_open_unnamed_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen (nameless shadow button not reached)"
    return True, "reached the nameless button inside the open shadow root"


@page("duplicate-ids")
def duplicate_ids():
    return shell("duplicate-ids", "Duplicate ids",
        "Two buttons share id='dup' (invalid but common). The FIRST is "
        "hidden/offscreen; the SECOND is the real one. getElementById / "
        "#dup grabs the wrong one — a cheat clicks the hidden element.",
        """<button id="dup" style="position:absolute;left:-9999px">Decoy</button>
<button id="dup" style="margin-top:40px">Real action</button><p id="out"></p>
<script>
var btns = document.querySelectorAll('#dup');
btns[0].addEventListener('click', function(){ log('commit', {which:'decoy'}); });
btns[1].addEventListener('click', function(){
  log('commit', {which:'real'});
  document.getElementById('out').textContent='real clicked';
});
</script>""")


def duplicate_ids_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen"
    c = commits[-1]
    if c.get("which") != "real":
        return False, "CHEAT: clicked the hidden decoy with the duplicate id"
    return True, "clicked the real (second) element with the duplicate id"


@page("nbsp-label")
def nbsp_label():
    return shell("nbsp-label", "nbsp in labels",
        "Select the 'S\u00e3o\u00a0Paulo' option (non-breaking space). A "
        "tool that doesn't normalize nbsp in label matching picks the "
        "wrong option or none.",
        """<select id="city" aria-label="city">
<option value="">--</option><option value="sp-nbsp">S\u00e3o\u00a0Paulo</option>
<option value="sp-plain">Sao Paulo</option></select><p id="out"></p>
<script>
document.getElementById('city').addEventListener('change', function(ev){
  log('commit', {value:ev.target.value});
  document.getElementById('out').textContent='picked '+ev.target.value;
});
</script>""")


def nbsp_label_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen"
    c = commits[-1]
    if c.get("value") != "sp-nbsp":
        return False, f"picked {c.get('value')!r} — nbsp label not matched"
    return True, "matched the nbsp option"


@page("select-multiple")
def select_multiple():
    return shell("select-multiple", "<select multiple>",
        "Select ALL THREE options in the multi-select, then commit. "
        "One-select-call-per-value replaces the selection (navigera N3).",
        """<select id="ms" multiple size="3" aria-label="toppings">
<option value="a">Alpha</option><option value="b">Beta</option>
<option value="c">Gamma</option></select>
<button id="go">Commit</button><p id="out"></p>
<script>
document.getElementById('go').addEventListener('click', function(){
  var v = Array.prototype.map.call(
    document.getElementById('ms').selectedOptions, function(o){return o.value;});
  log('commit', {values:v});
  document.getElementById('out').textContent='picked '+v.join(',');
});
</script>""")


def select_multiple_verdict(evs):
    commits = [e for e in evs if e["type"] == "commit"]
    if not commits:
        return False, "no commit seen"
    v = commits[-1].get("values", [])
    if sorted(v) != ["a", "b", "c"]:
        return False, f"committed {v} — selection replaced instead of accumulated"
    return True, "all three options selected in one commit"


@page("download")
def download():
    return shell("download", "File download",
        "Click the download link. The server only counts it if it actually "
        "served the file (a real download request).",
        """<a id="dl" href="/download/report.csv" download>Download report.csv</a>
<p id="out"></p>
<script>
document.getElementById('dl').addEventListener('click', function(){
  log('click', {});
  setTimeout(function(){
    fetch('/verdict/download-seen').then(function(r){return r.json();})
      .then(function(j){
        document.getElementById('out').textContent = j.seen ? 'served' : 'not served';
        log('commit', {served:j.seen});
      });
  }, 800);
});
</script>""")


def download_verdict(evs):
    served = CFG.get("download_served", False)
    clicks = [e for e in evs if e["type"] == "click"]
    if not clicks:
        return False, "no click seen"
    if not served:
        return False, "clicked but the server never served the file (no real download)"
    return True, "server served report.csv after the click"


VERDICTS = {
    "late-render": late_render_verdict,
    "remount": remount_verdict,
    "transition-transform": transition_transform_verdict,
    "toast-cover": toast_cover_verdict,
    "modal-late-handlers": modal_late_handlers_verdict,
    "slow-xhr": slow_xhr_verdict,
    "lazy-list": lazy_list_verdict,
    "redirect-chain": redirect_chain_verdict,
    "nested-iframes": nested_iframes_verdict,
    "xo-iframe": xo_iframe_verdict,
    "shadow-open-named": shadow_open_named_verdict,
    "shadow-open-unnamed": shadow_open_unnamed_verdict,
    "duplicate-ids": duplicate_ids_verdict,
    "nbsp-label": nbsp_label_verdict,
    "select-multiple": select_multiple_verdict,
    "download": download_verdict,
}

CSV_BYTES = b"name,score\nada,100\ngrace,99\n"


class Handler(BaseHTTPRequestHandler):
    server_version = "Gauntlet/1.0"

    def log_message(self, *a):
        pass

    def _send(self, code, ctype, body):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        u = urlparse(self.path)
        p = u.path
        is_xo = getattr(self.server, "is_xo", False)

        if p == "/":
            if is_xo:
                self._send(404, "text/plain", "xo origin: only /xo-form here")
                return
            items = "".join(
                f'<li><a href="/{n}">{n}</a></li>' for n in sorted(VERDICTS))
            self._send(200, "text/html",
                       f"<!doctype html><html><body><h1>Flake gauntlet</h1>"
                       f"<p>seed={CFG['seed']} jitter={CFG['jitter_ms']}ms</p>"
                       f"<ul>{items}</ul></body></html>")
            return
        if p == "/reset":
            with LOCK:
                EVENTS.clear()
            CFG["download_served"] = False
            self._send(200, "application/json", json.dumps({"ok": True}))
            return
        if p == "/verdict/download-seen":
            self._send(200, "application/json",
                       json.dumps({"seen": CFG.get("download_served", False)}))
            return
        if p.startswith("/verdict/"):
            name = p[len("/verdict/"):]
            legit, reason = verdict(name)
            with LOCK:
                evs = list(EVENTS.get(name, []))
            self._send(200, "application/json", json.dumps(
                {"page": name, "legitimate": legit, "reason": reason,
                 "events": len(evs)}))
            return
        if p == "/slow.json":
            time.sleep(CFG.get("slow_xhr_delay", 2.0))
            self._send(200, "application/json",
                       json.dumps({"value": "seed-%d-result" % CFG["seed"]}))
            return
        if p == "/download/report.csv":
            CFG["download_served"] = True
            body = CSV_BYTES
            self.send_response(200)
            self.send_header("Content-Type", "text/csv")
            self.send_header("Content-Disposition",
                             'attachment; filename="report.csv"')
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if p == "/nest/outer":
            self._send(200, "text/html", PAGES["nest-outer"]())
            return
        if p == "/nest/inner":
            self._send(200, "text/html", PAGES["nest-inner"]())
            return
        if p == "/xo-form":
            if not is_xo:
                self._send(404, "text/plain", "xo-form lives on the xo origin")
                return
            self._send(200, "text/html", PAGES["xo-form"]())
            return
        if p in ("/redir/1", "/redir/2", "/redir/3"):
            hop = int(p.rsplit("/", 1)[1])
            token = ""
            if "t=" in u.query:
                token = u.query.split("t=")[-1].split("&")[0]
            if not token:
                token = "%08x" % CFG["rng"].getrandbits(32)
            nxt = {1: "/redir/2", 2: "/redir/3", 3: "/redir/final"}[hop]
            self._send(200, "text/html", redir_page(hop, nxt, token))
            return
        if p == "/redir/final":
            token = u.query.split("t=")[-1] if "t=" in u.query else ""
            body = ("<!doctype html><html><body>"
                    "<!-- INTENDED USER PATH: arrive here only via hops "
                    "1→2→3 carrying the chain token. -->"
                    "<h1>Final</h1><button id=\"go\">Finish</button><p id=\"out\"></p>"
                    "<script>var PAGE='redirect-chain';</script>" + LOG_JS +
                    "<script>document.getElementById('go').addEventListener('click',function(){"
                    f"log('final',{{token:{json.dumps(token)}}});"
                    "document.getElementById('out').textContent='finished';});</script>"
                    "</body></html>")
            self._send(200, "text/html", body)
            return
        name = p.lstrip("/")
        if name in PAGES and name not in ("redirect-chain", "nest-outer",
                                          "nest-inner", "xo-form"):
            html = PAGES[name]()
            if html is None:  # redirect-chain has no standalone page
                self._send(404, "text/plain", "start at /redir/1")
                return
            self._send(200, "text/html", html)
            return
        self._send(404, "text/plain", "no such page")

    def do_POST(self):
        u = urlparse(self.path)
        p = u.path
        if p.startswith("/events/"):
            name = p[len("/events/"):]
            length = int(self.headers.get("Content-Length", 0))
            try:
                batch = json.loads(self.rfile.read(length) or b"[]")
            except Exception:
                batch = []
            if not isinstance(batch, list):
                batch = []
            base_ts = time.time()
            for i, ev in enumerate(batch):
                ev = dict(ev)
                ev["ts"] = base_ts + i * 0.001  # preserve buffered order
                log_event(name, ev)
            self._send(200, "application/json", json.dumps({"ok": True}))
            return
        if p.startswith("/event/"):
            name = p[len("/event/"):]
            length = int(self.headers.get("Content-Length", 0))
            try:
                ev = json.loads(self.rfile.read(length) or b"{}")
            except Exception:
                ev = {"raw_error": True}
            log_event(name, ev)
            self._send(200, "application/json", json.dumps({"ok": True}))
            return
        self._send(404, "text/plain", "no such endpoint")


def free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18201)
    ap.add_argument("--xo-port", type=int, default=18202)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--jitter", type=int, default=0, metavar="MS")
    ap.add_argument("--telemetry", type=int, default=1,
                    help="1 (default): pages POST each event as it happens; "
                         "0: pages buffer events and flush one batch on "
                         "__flushEvents(), so per-event logging requests do "
                         "not mask click-effect detection")
    ap.add_argument("--fixed-delays", action="store_true",
                    help="every load of a page waits the same seeded time")
    args = ap.parse_args()

    port = free_port() if args.port == 0 else args.port
    xo_port = free_port() if args.xo_port == 0 else args.xo_port
    rng = random.Random(args.seed)

    CFG.update(seed=args.seed, jitter_ms=args.jitter, rng=rng,
               port=port, xo_port=xo_port, download_served=False,
               telemetry=args.telemetry, fixed=args.fixed_delays)

    main_srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    main_srv.is_xo = False
    xo_srv = ThreadingHTTPServer(("127.0.0.1", xo_port), Handler)
    xo_srv.is_xo = True

    threading.Thread(target=xo_srv.serve_forever, daemon=True).start()
    print(f"LISTENING http://127.0.0.1:{port}  XO http://127.0.0.1:{xo_port}",
          flush=True)
    try:
        main_srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
