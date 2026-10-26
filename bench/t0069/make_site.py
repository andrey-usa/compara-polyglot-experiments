#!/usr/bin/env python3
"""T-0069: build the repair-task site copy from primavera's pristine suite40 server.py.

Adds --change r1|r2|r3|r4 (site changes the way real sites change) and
--broken add-cart|detail|checkout|search (feature switch: a test that still
passes with the feature broken scores as fail). Primavera itself is untouched.
"""
import os, sys

SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vendor", "suite40-site", "server.py")
DST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "site", "server.py")

s = open(SRC).read()

EXTRA = '''
OVERLAY_BANNER = """
<div id="cookie-overlay" style="position:fixed;inset:0;background:rgba(15,23,42,.55);z-index:9999;display:flex;align-items:center;justify-content:center">
  <div id="cookie" role="dialog" aria-label="Cookie consent" style="background:#fff;padding:24px;border-radius:8px;max-width:420px">
    <span>We use cookies to run this shop. Accept to continue.</span>
    <button id="cookie-accept">Accept all</button>
  </div>
</div>
<script>
  document.getElementById('cookie-accept')?.addEventListener('click', () => {
    document.getElementById('cookie-overlay').remove();
  });
</script>
"""

SEARCH_COMPONENT_JS = r"""
if (!customElements.get('acme-search')) customElements.define('acme-search', class extends HTMLElement {
  connectedCallback() {
    const root = this.attachShadow({ mode: 'open' });
    root.innerHTML = `<span class="lbl">Search products</span><input id="q" type="search" placeholder="Search… (press Enter)">`;
  }
});
"""

CHANGES = set()
BROKEN = [None]


def apply_changes():
    """T-0069 repair tasks: site changes the way real sites change."""
    global SHOP_JS, COOKIE_BANNER
    if "r2" in CHANGES:
        # The product link is gone: the card itself is the opener, a button whose
        # accessible name repeats the product name shown in its h3.
        SHOP_JS = SHOP_JS.replace(
            '<h3><a href="/shop/p/${item.id}" data-id="${item.id}">${item.name}</a></h3>',
            '<h3>${item.name}</h3><button class="card-open" data-nav="/shop/p/${item.id}">Open</button>')
        SHOP_JS = SHOP_JS.replace(
            "  const a = e.target.closest('a[href^=\\"/shop\\"]');",
            "  const opener = e.target.closest('.card-open');"
            "  if (opener) { e.preventDefault(); return nav(opener.dataset.nav); }"
            "  const a = e.target.closest('a[href^=\\"/shop\\"]');")
    if "r1" in CHANGES:
        # The buy button is renamed.
        SHOP_JS = SHOP_JS.replace("Add to cart</button>", "Add to basket</button>")
        SHOP_JS = SHOP_JS.replace('aria-label="Add ${item.name} to cart"',
                                  'aria-label="Add ${item.name} to basket"')
    if "r3" in CHANGES:
        # The cookie banner becomes a modal overlay covering the page.
        COOKIE_BANNER = OVERLAY_BANNER
    if "r4" in CHANGES:
        # The search field moves into a web component (open shadow root).
        SHOP_JS = SEARCH_COMPONENT_JS + SHOP_JS.replace(
            "const q = document.getElementById('q'), cat = document.getElementById('cat'), sort = document.getElementById('sort');",
            "const qHost = document.querySelector('acme-search');\\n"
            "const q = qHost ? qHost.shadowRoot.getElementById('q') : document.getElementById('q'), cat = document.getElementById('cat'), sort = document.getElementById('sort');")


'''

R = []

# 1. globals + CLI
R.append((
"""def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    args = ap.parse_args()
    httpd""",
EXTRA + """def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--change", action="append", choices=["r1", "r2", "r3", "r4"], default=[])
    ap.add_argument("--broken", choices=["add-cart", "detail", "checkout", "search", "sort", "coupon", "signin", "tickets"])
    args = ap.parse_args()
    CHANGES.update(args.change)
    BROKEN[0] = args.broken
    apply_changes()
    httpd"""))

# 2. r4: search box markup swap in shop_page
R.append((
"""def shop_page(count):
    cats = "".""",
"""def shop_page(count):
    search_box = ("<acme-search></acme-search>" if "r4" in CHANGES else
                  '<div><label for="q">Search products</label><input id="q" type="search" placeholder="Search… (press Enter)"></div>')
    cats = ""."""))
R.append((
"""    <div><label for="q">Search products</label><input id="q" type="search" placeholder="Search… (press Enter)"></div>
    <div><label for="cat">Category</label>""",
"""    {search_box}
    <div><label for="cat">Category</label>"""))

# 3. broken=search: ignore q filter
R.append((
"""            if q := qs.get("q", [""])[0].strip().lower():""",
"""            if q := ("" if BROKEN[0] == "search" else qs.get("q", [""])[0].strip().lower()):"""))

# 4. broken=detail: product API 500s
R.append((
"""        if path.startswith("/api/products/"):
            p = BY_ID.get(int(path.rsplit("/", 1)[1]))""",
"""        if path.startswith("/api/products/"):
            if BROKEN[0] == "detail":
                return self.json({"error": "failed to load product"}, 500)
            p = BY_ID.get(int(path.rsplit("/", 1)[1]))"""))

# 5. broken=add-cart: silent no-op
R.append((
"""            if path == "/api/cart":
                cart = STATE.carts.setdefault(sid, {})""",
"""            if path == "/api/cart":
                if BROKEN[0] == "add-cart":
                    return self.json({"ok": True, "count": sum(STATE.carts.get(sid, {}).values())})
                cart = STATE.carts.setdefault(sid, {})"""))

# 6. broken=checkout: always declined
R.append((
"""            if path == "/api/orders":
                lines, subtotal, code, discount = cart_totals(sid)""",
"""            if path == "/api/orders":
                if BROKEN[0] == "checkout":
                    return self.json({"ok": False, "error": "Payment declined."})
                lines, subtotal, code, discount = cart_totals(sid)"""))


# 7. broken=sort: ignore sort param
R.append((
"""            s = qs.get("sort", ["featured"])[0]""",
"""            s = ("featured" if BROKEN[0] == "sort" else qs.get("sort", ["featured"])[0])"""))

# 8. broken=coupon: always invalid
R.append((
"""            if path == "/api/coupon":
                code = str(data.get("code", "")).strip().upper()""",
"""            if path == "/api/coupon":
                if BROKEN[0] == "coupon":
                    return self.json({"ok": False})
                code = str(data.get("code", "")).strip().upper()"""))

# 9. broken=signin: always 401
R.append((
"""            if path == "/login":
                email, password = data.get("email", "").strip().lower(), data.get("password", "")""",
"""            if path == "/login":
                email, password = data.get("email", "").strip().lower(), data.get("password", "")
                if BROKEN[0] == "signin":
                    return self.send(401, login_page("Wrong email or password.", data.get("next") or "/account"))"""))

# 10. broken=tickets: silent no-op
R.append((
'            if path == "/api/tickets":\n                tid = f"T-{STATE.next_ticket}"',
'            if path == "/api/tickets":\n                if BROKEN[0] == "tickets":\n                    return self.json({"ok": True, "ticket": {"id": "T-0"}})\n                tid = f"T-{STATE.next_ticket}"'))

for i, (old, new) in enumerate(R, 1):
    assert s.count(old) == 1, f"replacement {i}: anchor found {s.count(old)} times"
    s = s.replace(old, new)

os.makedirs(os.path.dirname(DST), exist_ok=True)
open(DST, "w").write(s)
print(f"wrote {DST} ({len(s.splitlines())} lines)")
