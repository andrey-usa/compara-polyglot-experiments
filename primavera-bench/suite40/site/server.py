#!/usr/bin/env python3
"""Acme Supply — a deterministic local web shop for realistic browser tests.

A fictional store (no real brand) that packs the patterns that break browser
agents into one small site, served from the standard library only:

  * SPA routing (history.pushState) and content rendered after a delayed fetch
  * search that submits on Enter only (no button), <select> filters
  * infinite scroll (IntersectionObserver), toasts that vanish
  * a cookie banner overlaying the page, a CSS :hover account menu
  * a coupon widget inside an open shadow root
  * a card form inside an iframe, confirm() dialogs, client-side validation
  * login with cookies + 302/303 redirects, a paginated orders table
  * docs that open in a new tab (target=_blank) with a collapsed <details> FAQ
  * a support form (in an iframe) with radios, a select and a file upload
  * /slow: a page whose image never finishes (load event hangs)

State (carts, orders, tickets, logins) lives in memory and is readable at
GET /__state, so tests and the agent eval can verify what an agent actually
did, not only what it claims. Every server instance starts empty.

Usage: server.py [--port 0]   (prints "LISTENING http://127.0.0.1:<port>")
"""
import argparse
import html
import json
import random
import secrets
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MATERIALS = ["Brass", "Steel", "Titanium", "Copper", "Nickel", "Carbon"]
ITEMS = ["Widget", "Sprocket", "Gear", "Bolt", "Flange", "Bracket", "Hinge", "Valve", "Spring", "Clamp"]
CATEGORIES = {"Widget": "Mechanical", "Sprocket": "Drive", "Gear": "Drive", "Bolt": "Fasteners",
              "Flange": "Plumbing", "Bracket": "Mounting", "Hinge": "Mounting", "Valve": "Plumbing",
              "Spring": "Mechanical", "Clamp": "Fasteners"}
COUNTRIES = ["Argentina", "Australia", "Austria", "Belgium", "Brazil", "Canada", "Chile", "Denmark",
             "Finland", "France", "Germany", "India", "Ireland", "Italy", "Japan", "Mexico",
             "Netherlands", "New Zealand", "Norway", "Poland", "Portugal", "Singapore", "Spain",
             "Sweden", "Switzerland", "United Kingdom", "United States"]
SHIPPING = {"standard": 5.00, "express": 19.00}
COUPONS = {"SAVE10": 0.10}
PAGE_SIZE = 12
DEMO_USER = ("demo@acme.test", "hunter2")
RETURN_DAYS = 37
RESTOCKING_FEE = "8%"


def build_products():
    rng = random.Random(7)
    products = []
    pid = 100
    for item in ITEMS:
        for mat in MATERIALS:
            pid += 1
            products.append({
                "id": pid,
                "name": f"{mat} {item}",
                "category": CATEGORIES[item],
                "price": round(rng.uniform(3, 180), 2),
                "stock": rng.randint(0, 250),
                "rating": round(rng.uniform(2.5, 5.0), 1),
            })
    rng.shuffle(products)
    return products


PRODUCTS = build_products()
BY_ID = {p["id"]: p for p in PRODUCTS}


def seeded_orders():
    rng = random.Random(11)
    statuses = ["Delivered", "Shipped", "Processing", "Cancelled", "Returned"]
    orders = []
    for n in range(1060, 1035, -1):
        status = rng.choice(statuses)
        if n == 1042:
            status = "Awaiting pickup"
        orders.append({"id": f"A-{n}", "status": status, "total": round(rng.uniform(20, 400), 2),
                       "date": f"2026-0{rng.randint(1, 9)}-{rng.randint(10, 28)}"})
    return orders


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.carts = {}          # sid -> {product_id: qty}
        self.coupons = {}        # sid -> code
        self.users = {}          # sid -> email
        self.orders = []         # placed orders
        self.tickets = []
        self.events = []
        self.next_order = 2001
        self.next_ticket = 501

    def log(self, kind, **data):
        self.events.append({"t": round(time.time(), 3), "kind": kind, **data})

    def snapshot(self):
        with self.lock:
            return {
                "orders": self.orders,
                "tickets": self.tickets,
                "carts": {sid: {str(k): v for k, v in c.items()} for sid, c in self.carts.items()},
                "logins": sorted(set(self.users.values())),
                "events": self.events[-200:],
            }


STATE = State()

CSS = """
*{box-sizing:border-box} body{font-family:system-ui,sans-serif;margin:0;color:#1d1d1f;background:#fafafa}
header{display:flex;gap:18px;align-items:center;padding:10px 24px;background:#16324f;color:#fff}
header a{color:#fff;text-decoration:none} header .brand{font-weight:700;font-size:18px;margin-right:auto}
.menu{position:relative} .menu>.drop{display:none;position:absolute;right:0;top:100%;background:#fff;
 border:1px solid #ccc;min-width:150px;z-index:5;padding:6px 0}
.menu:hover>.drop{display:block} .menu .drop a{display:block;color:#16324f;padding:6px 12px}
main{max-width:980px;margin:0 auto;padding:20px}
.grid{display:grid;grid-template-columns:repeat(3,1fr);gap:14px}
.card{background:#fff;border:1px solid #ddd;border-radius:8px;padding:12px;min-height:230px}
.card h3{margin:0 0 6px;font-size:16px} .price{font-weight:600}
button{cursor:pointer;padding:6px 12px;border-radius:6px;border:1px solid #16324f;background:#16324f;color:#fff}
button.secondary{background:#fff;color:#16324f}
#cookie{position:fixed;left:0;right:0;bottom:0;background:#222;color:#fff;padding:18px 24px;z-index:20;
 display:flex;gap:14px;align-items:center;height:120px}
.toast{position:fixed;top:16px;right:16px;background:#2e7d32;color:#fff;padding:10px 14px;border-radius:6px}
.error{color:#b00020} .muted{color:#666;font-size:13px} table{border-collapse:collapse;width:100%}
td,th{border-bottom:1px solid #ddd;padding:6px;text-align:left} .badge{background:#e53935;border-radius:10px;padding:0 7px}
label{display:block;margin:8px 0 2px} input,select,textarea{padding:6px;font-size:14px}
"""

HEADER = """
<header>
  <a class="brand" href="/">Acme Supply</a>
  <a href="/shop">Shop</a>
  <a href="/docs" target="_blank" rel="noopener">Docs</a>
  <a href="/support">Support</a>
  <a href="/cart" id="cart-link">Cart <span class="badge" id="cart-count">{count}</span></a>
  <div class="menu"><a href="#" onclick="return false" aria-haspopup="true">Account ▾</a>
    <div class="drop"><a href="/account">Orders</a><a href="/login">Sign in</a><a href="/logout">Sign out</a></div>
  </div>
</header>
"""

COOKIE_BANNER = """
<div id="cookie" role="dialog" aria-label="Cookie consent">
  <span>We use cookies to run this shop. Accept to continue.</span>
  <button id="cookie-accept">Accept all</button>
</div>
<script>
  if (document.cookie.includes('consent=1')) document.getElementById('cookie').remove();
  document.getElementById('cookie-accept')?.addEventListener('click', () => {
    document.cookie = 'consent=1; path=/'; document.getElementById('cookie').remove();
  });
</script>
"""


def page(title, body, count=0, banner=True):
    return f"""<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>{html.escape(title)} — Acme Supply</title>
<style>{CSS}</style></head><body>{HEADER.format(count=count)}<main>{body}</main>
{COOKIE_BANNER if banner else ''}</body></html>"""


SHOP_JS = r"""
const grid = document.getElementById('grid');
const status = document.getElementById('status');
const q = document.getElementById('q'), cat = document.getElementById('cat'), sort = document.getElementById('sort');
let pageNo = 0, done = false, loading = false;
const sentinel = document.getElementById('sentinel');
const io = new IntersectionObserver((entries) => {
  if (entries.some(e => e.isIntersecting) && !done && !loading && !document.getElementById('list').hidden) load(false);
});
function params() { return new URLSearchParams(location.search); }
function syncInputs() { const p = params(); q.value = p.get('q') || ''; cat.value = p.get('cat') || ''; sort.value = p.get('sort') || 'featured'; }
let generation = 0;
async function load(reset) {
  if (loading && !reset) return;
  const gen = reset ? ++generation : generation;
  loading = true;
  if (reset) { pageNo = 0; done = false; grid.innerHTML = ''; status.textContent = 'Loading products…'; }
  const p = params(); p.set('page', pageNo);
  const res = await fetch('/api/products?' + p.toString());
  const data = await res.json();
  await new Promise(r => setTimeout(r, 350));   // slow API: content arrives after load
  if (gen !== generation) return;                // a newer search replaced this one
  for (const item of data.items) {
    const card = document.createElement('article'); card.className = 'card';
    card.innerHTML = `<h3><a href="/shop/p/${item.id}" data-id="${item.id}">${item.name}</a></h3>
      <div class="muted">${item.category} · ★ ${item.rating}</div>
      <div class="price">$${item.price.toFixed(2)}</div>
      <button data-add="${item.id}" aria-label="Add ${item.name} to cart">Add to cart</button>`;
    grid.appendChild(card);
  }
  pageNo += 1; done = !data.more; loading = false;
  status.textContent = data.total ? `Showing ${grid.children.length} of ${data.total} products` : 'No products match.';
  // Re-arm the observer: if the sentinel is still on screen, it fires again.
  io.unobserve(sentinel); io.observe(sentinel);
}
function route() {
  const m = location.pathname.match(/^\/shop\/p\/(\d+)$/);
  document.getElementById('list').hidden = !!m;
  document.getElementById('detail').hidden = !m;
  if (m) return showDetail(m[1]);
  syncInputs(); load(true);
}
async function showDetail(id) {
  const box = document.getElementById('detail');
  box.innerHTML = '<p>Loading…</p>';
  const item = await (await fetch('/api/products/' + id)).json();
  await new Promise(r => setTimeout(r, 250));
  box.innerHTML = `<p><a href="/shop" id="back-to-shop">← All products</a></p><h1>${item.name}</h1>
    <p class="price">Price: $${item.price.toFixed(2)}</p><p>In stock: <b id="stock">${item.stock}</b> units</p>
    <p class="muted">Category: ${item.category}</p>
    <label for="qty">Quantity</label><input id="qty" type="number" min="1" value="1">
    <button data-add="${item.id}" id="detail-add">Add to cart</button>`;
}
function nav(url) { history.pushState({}, '', url); route(); }
document.addEventListener('click', async (e) => {
  const a = e.target.closest('a[href^="/shop"]');
  if (a && !a.target) { e.preventDefault(); return nav(a.getAttribute('href')); }
  const add = e.target.closest('[data-add]');
  if (add) {
    const qty = add.id === 'detail-add' ? parseInt(document.getElementById('qty').value || '1', 10) : 1;
    const res = await fetch('/api/cart', { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id: +add.dataset.add, qty }) });
    const data = await res.json();
    document.getElementById('cart-count').textContent = data.count;
    const t = document.createElement('div'); t.className = 'toast'; t.setAttribute('role', 'status');
    t.textContent = 'Added to cart'; document.body.appendChild(t); setTimeout(() => t.remove(), 1800);
  }
});
q.addEventListener('keydown', (e) => {
  if (e.key !== 'Enter') return;
  const p = params(); p.set('q', q.value); nav('/shop?' + p.toString());
});
for (const s of [cat, sort]) s.addEventListener('change', () => {
  const p = params(); p.set(s.id, s.value); nav('/shop?' + p.toString());
});
io.observe(sentinel);
window.addEventListener('popstate', route);
route();
"""


def shop_page(count):
    cats = "".join(f'<option value="{c}">{c}</option>' for c in sorted(set(CATEGORIES.values())))
    body = f"""
<div id="list">
  <h1>Shop</h1>
  <div style="display:flex;gap:12px;align-items:end;margin-bottom:14px">
    <div><label for="q">Search products</label><input id="q" type="search" placeholder="Search… (press Enter)"></div>
    <div><label for="cat">Category</label><select id="cat"><option value="">All categories</option>{cats}</select></div>
    <div><label for="sort">Sort by</label><select id="sort"><option value="featured">Featured</option>
      <option value="price-asc">Price: low to high</option><option value="price-desc">Price: high to low</option>
      <option value="name">Name</option></select></div>
  </div>
  <p id="status" role="status">Loading products…</p>
  <div class="grid" id="grid"></div>
  <div id="sentinel" style="height:40px"></div>
</div>
<section id="detail" hidden></section>
<script>{SHOP_JS}</script>"""
    return page("Shop", body, count)


COUPON_JS = r"""
customElements.define('acme-coupon', class extends HTMLElement {
  connectedCallback() {
    const root = this.attachShadow({ mode: 'open' });
    root.innerHTML = `<style>input{padding:6px}</style><label>Coupon code <input id="code" placeholder="Enter code"></label>
      <button id="apply">Apply coupon</button> <span id="msg" role="status"></span>`;
    root.getElementById('apply').addEventListener('click', async () => {
      const res = await fetch('/api/coupon', { method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ code: root.getElementById('code').value }) });
      const data = await res.json();
      root.getElementById('msg').textContent = data.ok ? `Coupon ${data.code} applied: −${data.percent}%` : 'Invalid coupon';
      if (data.ok) setTimeout(() => location.reload(), 400);
    });
  }
});
"""


def cart_totals(sid):
    cart = STATE.carts.get(sid, {})
    lines = [(BY_ID[i], q) for i, q in cart.items() if q > 0]
    subtotal = round(sum(p["price"] * q for p, q in lines), 2)
    code = STATE.coupons.get(sid)
    discount = round(subtotal * COUPONS.get(code, 0), 2)
    return lines, subtotal, code, discount


def cart_page(sid):
    lines, subtotal, code, discount = cart_totals(sid)
    rows = "".join(
        f"""<tr><td>{html.escape(p['name'])}</td><td>${p['price']:.2f}</td>
        <td><label class="muted" for="qty-{p['id']}">Qty</label><input id="qty-{p['id']}" type="number" min="0" value="{q}" data-qty="{p['id']}" style="width:60px"></td>
        <td>${p['price'] * q:.2f}</td><td><button class="secondary" data-remove="{p['id']}" aria-label="Remove {html.escape(p['name'])}">Remove</button></td></tr>"""
        for p, q in lines)
    if not lines:
        body = '<h1>Your cart</h1><p>Your cart is empty. <a href="/shop">Continue shopping</a></p>'
    else:
        body = f"""<h1>Your cart</h1>
<table><thead><tr><th>Product</th><th>Price</th><th>Quantity</th><th>Line total</th><th></th></tr></thead><tbody>{rows}</tbody></table>
<p>Subtotal: <b id="subtotal">${subtotal:.2f}</b></p>
{f'<p>Discount ({html.escape(code)}): −${discount:.2f}</p>' if code else ''}
<acme-coupon></acme-coupon>
<p><button id="checkout">Proceed to checkout</button></p>
<script>{COUPON_JS}
document.addEventListener('click', async (e) => {{
  const r = e.target.closest('[data-remove]');
  if (r) {{
    if (!confirm('Remove this item from your cart?')) return;
    await fetch('/api/cart', {{ method: 'POST', headers: {{ 'content-type': 'application/json' }}, body: JSON.stringify({{ id: +r.dataset.remove, set: 0 }}) }});
    location.reload();
  }}
  if (e.target.id === 'checkout') location.href = '/checkout';
}});
document.addEventListener('change', async (e) => {{
  const i = e.target.closest('[data-qty]');
  if (!i) return;
  await fetch('/api/cart', {{ method: 'POST', headers: {{ 'content-type': 'application/json' }}, body: JSON.stringify({{ id: +i.dataset.qty, set: parseInt(i.value || '0', 10) }}) }});
  location.reload();
}});
</script>"""
    count = sum(q for _, q in lines)
    return page("Cart", body, count)


CHECKOUT_JS = r"""
const step1 = document.getElementById('step1'), step2 = document.getElementById('step2');
document.getElementById('continue').addEventListener('click', () => {
  const name = document.getElementById('name').value.trim();
  const country = document.getElementById('country').value;
  const speed = document.querySelector('input[name=speed]:checked');
  const err = [];
  if (!name) err.push('Full name is required.');
  if (!country) err.push('Choose a country.');
  if (!speed) err.push('Choose a shipping speed.');
  document.getElementById('err1').textContent = err.join(' ');
  if (err.length) return;
  step1.hidden = true; step2.hidden = false;
  const ship = { standard: 5, express: 19 }[speed.value];
  document.getElementById('summary').textContent = `Shipping to ${country} (${speed.value}) — total $${(TOTAL + ship).toFixed(2)}`;
});
document.getElementById('place').addEventListener('click', async () => {
  const frame = document.getElementById('payframe').contentDocument;
  const card = frame.getElementById('card').value.replace(/\s+/g, '');
  const exp = frame.getElementById('exp').value.trim(), cvc = frame.getElementById('cvc').value.trim();
  const errEl = document.getElementById('err2');
  if (!/^\d{16}$/.test(card) || !/^\d{2}\/\d{2}$/.test(exp) || !/^\d{3,4}$/.test(cvc)) {
    errEl.textContent = 'Card details are incomplete or invalid (card: 16 digits, expiry MM/YY, CVC 3-4 digits).'; return;
  }
  const speed = document.querySelector('input[name=speed]:checked').value;
  const ship = { standard: 5, express: 19 }[speed];
  if (!confirm(`Place order for $${(TOTAL + ship).toFixed(2)}?`)) return;
  const res = await fetch('/api/orders', { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name: document.getElementById('name').value.trim(), country: document.getElementById('country').value,
      speed, card_last4: card.slice(-4) }) });
  const data = await res.json();
  if (!data.ok) { errEl.textContent = data.error; return; }
  location.href = '/orders/' + data.order.id;
});
"""


def checkout_page(sid):
    lines, subtotal, code, discount = cart_totals(sid)
    if not lines:
        return page("Checkout", '<h1>Checkout</h1><p>Your cart is empty. <a href="/shop">Shop</a></p>')
    opts = "".join(f"<option>{c}</option>" for c in COUNTRIES)
    total = round(subtotal - discount, 2)
    body = f"""<h1>Checkout</h1>
<p>Order total before shipping: <b>${total:.2f}</b></p>
<section id="step1"><h2>1. Shipping</h2>
  <label for="name">Full name</label><input id="name" autocomplete="name">
  <label for="country">Country</label><select id="country"><option value="">Select a country…</option>{opts}</select>
  <fieldset style="margin-top:10px"><legend>Shipping speed</legend>
    <label><input type="radio" name="speed" value="standard"> Standard (5–8 days) — $5.00</label>
    <label><input type="radio" name="speed" value="express"> Express (1–2 days) — $19.00</label>
  </fieldset>
  <p class="error" id="err1" role="alert"></p>
  <button id="continue">Continue to payment</button>
</section>
<section id="step2" hidden><h2>2. Payment</h2><p id="summary"></p>
  <iframe id="payframe" title="Secure card payment" src="/pay-frame" style="width:420px;height:190px;border:1px solid #ccc"></iframe>
  <p class="error" id="err2" role="alert"></p>
  <button id="place">Place order</button>
</section>
<script>const TOTAL = {total};{CHECKOUT_JS}</script>"""
    return page("Checkout", body, sum(q for _, q in lines))


PAY_FRAME = """<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><title>Card details</title>
<style>body{font-family:system-ui;margin:10px} input{padding:6px;margin:2px 0 8px}</style></head><body>
<label for="card">Card number</label><br><input id="card" inputmode="numeric" placeholder="1234 1234 1234 1234" size="24"><br>
<label for="exp">Expiry (MM/YY)</label> <input id="exp" placeholder="MM/YY" size="6">
<label for="cvc">CVC</label> <input id="cvc" placeholder="123" size="4">
</body></html>"""


def login_page(error="", nxt="/account"):
    body = f"""<h1>Sign in</h1>
<form method="post" action="/login" novalidate>
  <input type="hidden" name="next" value="{html.escape(nxt)}">
  <label for="email">Email</label><input id="email" name="email" type="email" required>
  <label for="password">Password</label><input id="password" name="password" type="password" required>
  <p class="error" role="alert">{html.escape(error)}</p>
  <button type="submit">Sign in</button>
</form>
<p class="muted">Demo account: see the internal wiki.</p>"""
    return page("Sign in", body)


ACCOUNT_JS = r"""
const rows = ORDERS; let pageNo = 0; const per = 10;
function render() {
  const body = document.getElementById('orders');
  body.innerHTML = rows.slice(pageNo * per, pageNo * per + per).map(o =>
    `<tr><td>${o.id}</td><td>${o.date}</td><td>$${o.total.toFixed(2)}</td><td>${o.status}</td></tr>`).join('');
  document.getElementById('pageinfo').textContent = `Page ${pageNo + 1} of ${Math.ceil(rows.length / per)}`;
  document.getElementById('prev').disabled = pageNo === 0;
  document.getElementById('next').disabled = (pageNo + 1) * per >= rows.length;
}
document.getElementById('prev').onclick = () => { pageNo--; render(); };
document.getElementById('next').onclick = () => { pageNo++; render(); };
render();
"""


def account_page(email, orders):
    body = f"""<h1>Your orders</h1><p>Signed in as <b>{html.escape(email)}</b></p>
<table><thead><tr><th>Order</th><th>Date</th><th>Total</th><th>Status</th></tr></thead><tbody id="orders"></tbody></table>
<p><button class="secondary" id="prev">Previous</button> <span id="pageinfo"></span> <button class="secondary" id="next">Next page</button></p>
<script>const ORDERS = {json.dumps(orders)};{ACCOUNT_JS}</script>"""
    return page("Your orders", body)


def docs_page():
    body = f"""<h1>Acme Supply documentation</h1>
<nav aria-label="Docs sections"><a href="#shipping">Shipping</a> · <a href="#returns">Returns</a> · <a href="#warranty">Warranty</a></nav>
<h2 id="shipping">Shipping</h2><p>Standard shipping takes 5–8 business days; Express takes 1–2.</p>
<h2 id="returns">Returns</h2><p>Unused items can be sent back. Open the FAQ below for the exact return window and fees.</p>
<details id="returns-faq"><summary>Returns FAQ</summary>
  <p>Customers have {RETURN_DAYS} days from delivery to return an item.</p>
  <p>A restocking fee of {RESTOCKING_FEE} applies to opened items.</p>
</details>
<h2 id="warranty">Warranty</h2><p>All parts carry a 2-year limited warranty.</p>"""
    return page("Docs", body, banner=False)


SUPPORT_WIDGET = """<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><title>Support request</title>
<style>body{font-family:system-ui;margin:12px} label{display:block;margin:6px 0 2px}</style></head><body>
<h2>New support request</h2>
<label for="category">Category</label><select id="category"><option value="">Choose…</option>
<option>Orders</option><option>Billing</option><option>Shipping</option><option>Technical</option></select>
<fieldset><legend>Priority</legend>
<label><input type="radio" name="priority" value="low"> Low</label>
<label><input type="radio" name="priority" value="normal" checked> Normal</label>
<label><input type="radio" name="priority" value="high"> High</label></fieldset>
<label for="message">Message</label><textarea id="message" rows="4" cols="40"></textarea>
<label for="attachment">Attachment (optional)</label><input id="attachment" type="file">
<p id="err" role="alert" style="color:#b00020"></p>
<button id="send">Submit request</button>
<script>
document.getElementById('send').onclick = async () => {
  const category = document.getElementById('category').value, message = document.getElementById('message').value.trim();
  const priority = document.querySelector('input[name=priority]:checked').value;
  if (!category || !message) { document.getElementById('err').textContent = 'Category and message are required.'; return; }
  const file = document.getElementById('attachment').files[0];
  const res = await fetch('/api/tickets', { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ category, priority, message, attachment: file ? file.name : null }) });
  const data = await res.json();
  document.body.innerHTML = `<h2>Thanks!</h2><p>Your ticket <b>${data.ticket.id}</b> was created.</p>`;
};
</script></body></html>"""


def support_page():
    body = """<h1>Support</h1><p>Our team answers within one business day.</p>
<iframe title="Support request form" src="/support/widget" style="width:520px;height:460px;border:1px solid #ccc"></iframe>"""
    return page("Support", body)


SLOW_PAGE = """<!DOCTYPE html><html><head><meta charset="utf-8"><title>Slow page</title></head><body>
<h1>Ready before load</h1><p id="ready">DOM is ready.</p><img src="/hang.gif" alt="never loads"></body></html>"""


class Handler(BaseHTTPRequestHandler):
    server_version = "AcmeSupply/1.0"

    def log_message(self, fmt, *args):  # quiet
        pass

    # --- helpers -----------------------------------------------------------
    def sid(self):
        cookies = dict(c.strip().split("=", 1) for c in self.headers.get("Cookie", "").split(";") if "=" in c)
        return cookies.get("sid")

    def send(self, code, body, ctype="text/html; charset=utf-8", headers=None):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        sid = self.sid()
        if not sid:
            sid = secrets.token_hex(8)
            self._new_sid = sid
            self.send_header("Set-Cookie", f"sid={sid}; Path=/; HttpOnly; SameSite=Lax")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def redirect(self, location, code=302):
        self.send(code, "", headers={"Location": location})

    def json(self, obj, code=200):
        self.send(code, json.dumps(obj), "application/json")

    def body_json(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        if self.headers.get("Content-Type", "").startswith("application/json"):
            return json.loads(raw or b"{}")
        return {k: v[0] for k, v in urllib.parse.parse_qs(raw.decode()).items()}

    def cart_count(self):
        return sum(STATE.carts.get(self.sid(), {}).values())

    # --- routes ------------------------------------------------------------
    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        path, qs = url.path, urllib.parse.parse_qs(url.query)
        sid = self.sid()
        if path == "/":
            return self.redirect("/shop")
        if path == "/shop" or path.startswith("/shop/p/"):
            return self.send(200, shop_page(self.cart_count()))
        if path == "/api/products":
            items = PRODUCTS
            if q := qs.get("q", [""])[0].strip().lower():
                items = [p for p in items if q in p["name"].lower()]
            if c := qs.get("cat", [""])[0]:
                items = [p for p in items if p["category"] == c]
            s = qs.get("sort", ["featured"])[0]
            if s == "price-asc":
                items = sorted(items, key=lambda p: p["price"])
            elif s == "price-desc":
                items = sorted(items, key=lambda p: -p["price"])
            elif s == "name":
                items = sorted(items, key=lambda p: p["name"])
            n = int(qs.get("page", ["0"])[0])
            chunk = items[n * PAGE_SIZE:(n + 1) * PAGE_SIZE]
            return self.json({"items": chunk, "total": len(items), "more": (n + 1) * PAGE_SIZE < len(items)})
        if path.startswith("/api/products/"):
            p = BY_ID.get(int(path.rsplit("/", 1)[1]))
            return self.json(p) if p else self.json({"error": "not found"}, 404)
        if path == "/cart":
            return self.send(200, cart_page(sid))
        if path == "/checkout":
            return self.send(200, checkout_page(sid))
        if path == "/pay-frame":
            return self.send(200, PAY_FRAME)
        if path.startswith("/orders/"):
            oid = path.rsplit("/", 1)[1]
            order = next((o for o in STATE.orders if o["id"] == oid), None)
            if not order:
                return self.send(404, page("Not found", "<h1>Order not found</h1>"))
            items = "".join(f"<li>{html.escape(i['name'])} × {i['qty']}</li>" for i in order["items"])
            return self.send(200, page("Order placed", f"""<h1>Thank you! Order {oid} is confirmed.</h1>
<p>Order number: <b id="order-id">{oid}</b></p><ul>{items}</ul><p>Total charged: ${order['total']:.2f}</p>
<p>Shipping to {html.escape(order['country'])} ({order['speed']}).</p>"""))
        if path == "/login":
            return self.send(200, login_page(nxt=qs.get("next", ["/account"])[0]))
        if path == "/logout":
            STATE.users.pop(sid, None)
            return self.redirect("/login")
        if path == "/account":
            email = STATE.users.get(sid)
            if not email:
                return self.redirect("/login?next=/account")
            return self.send(200, account_page(email, seeded_orders()))
        if path == "/docs":
            return self.send(200, docs_page())
        if path == "/support":
            return self.send(200, support_page())
        if path == "/support/widget":
            return self.send(200, SUPPORT_WIDGET)
        if path == "/slow":
            return self.send(200, SLOW_PAGE)
        if path == "/hang.gif":
            time.sleep(float(qs.get("s", ["30"])[0]))
            return self.send(200, b"GIF89a", "image/gif")
        if path == "/__state":
            return self.json(STATE.snapshot())
        return self.send(404, page("Not found", "<h1>Page not found</h1><p><a href=\"/shop\">Back to the shop</a></p>"))

    def do_POST(self):
        path = urllib.parse.urlparse(self.path).path
        sid = self.sid() or ""
        data = self.body_json()
        with STATE.lock:
            if path == "/api/cart":
                cart = STATE.carts.setdefault(sid, {})
                pid = int(data["id"])
                if pid not in BY_ID:
                    return self.json({"ok": False}, 404)
                if "set" in data:
                    cart[pid] = max(0, int(data["set"]))
                else:
                    cart[pid] = cart.get(pid, 0) + max(1, int(data.get("qty", 1)))
                cart = {k: v for k, v in cart.items() if v > 0}
                STATE.carts[sid] = cart
                STATE.log("cart", sid=sid, cart={str(k): v for k, v in cart.items()})
                return self.json({"ok": True, "count": sum(cart.values())})
            if path == "/api/coupon":
                code = str(data.get("code", "")).strip().upper()
                if code in COUPONS:
                    STATE.coupons[sid] = code
                    STATE.log("coupon", sid=sid, code=code)
                    return self.json({"ok": True, "code": code, "percent": int(COUPONS[code] * 100)})
                return self.json({"ok": False})
            if path == "/api/orders":
                lines, subtotal, code, discount = cart_totals(sid)
                if not lines:
                    return self.json({"ok": False, "error": "Cart is empty."})
                speed = data.get("speed")
                if speed not in SHIPPING or data.get("country") not in COUNTRIES or not data.get("name"):
                    return self.json({"ok": False, "error": "Missing shipping details."})
                oid = f"A-{STATE.next_order}"
                STATE.next_order += 1
                order = {
                    "id": oid, "name": data["name"], "country": data["country"], "speed": speed,
                    "coupon": code, "card_last4": data.get("card_last4"),
                    "items": [{"id": p["id"], "name": p["name"], "qty": q} for p, q in lines],
                    "total": round(subtotal - discount + SHIPPING[speed], 2),
                }
                STATE.orders.append(order)
                STATE.carts[sid] = {}
                STATE.coupons.pop(sid, None)
                STATE.log("order", sid=sid, id=oid)
                return self.json({"ok": True, "order": order})
            if path == "/api/tickets":
                tid = f"T-{STATE.next_ticket}"
                STATE.next_ticket += 1
                ticket = {"id": tid, "category": data.get("category"), "priority": data.get("priority"),
                          "message": data.get("message"), "attachment": data.get("attachment")}
                STATE.tickets.append(ticket)
                STATE.log("ticket", sid=sid, id=tid)
                return self.json({"ok": True, "ticket": ticket})
            if path == "/login":
                email, password = data.get("email", "").strip().lower(), data.get("password", "")
                nxt = data.get("next") or "/account"
                if (email, password) == DEMO_USER:
                    STATE.users[sid] = email
                    STATE.log("login", sid=sid, email=email)
                    return self.redirect(nxt if nxt.startswith("/") else "/account", 303)
                STATE.log("login_failed", sid=sid, email=email)
                return self.send(401, login_page("Wrong email or password.", nxt))
        return self.json({"ok": False, "error": "not found"}, 404)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    args = ap.parse_args()
    httpd = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    httpd.daemon_threads = True
    print(f"LISTENING http://127.0.0.1:{httpd.server_address[1]}", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
