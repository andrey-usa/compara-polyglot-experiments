//! Tab-level operations, implemented directly on CDP.
//!
//! The key performance decision: `Runtime.evaluate` with
//! `returnByValue: true` returns object results in ONE round trip — no
//! evaluate + callFunctionOn + releaseObject dance. Expressions are wrapped
//! so bare arrows (`() => …`), IIFEs, and plain expressions all evaluate to
//! their value. Element actions by CSS selector or visible text are one
//! `Runtime.evaluate` too (find, auto-wait and act in the page); actions by
//! `ax` ref resolve the node first (`DOM.resolveNode` + `callFunctionOn`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use super::ax;
use super::client::CdpClient;
use super::events::Shared;

const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long, after a navigation commits, ops wait for its DOMContentLoaded
/// before working on the partly parsed document.
const POST_COMMIT_WAIT: Duration = Duration::from_secs(5);

/// Default auto-wait for an element when the op carries no `timeout_ms`:
/// long enough for SPA rendering, short enough that an agent with a wrong
/// selector hears about it quickly.
pub const DEFAULT_ELEMENT_WAIT: Duration = Duration::from_millis(5000);

/// What an element op acts on.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// First element matching a CSS selector (auto-waits).
    Selector(String),
    /// A `backendNodeId` from `ax` (`[ref=N]`).
    Ref(u64),
    /// The best element whose visible text / label / value matches (auto-waits).
    Text(String),
    /// By role and accessible name, label, placeholder, test id, and/or
    /// CSS, optionally the nth match (auto-waits).
    Locator(Locator),
}

/// `--role/--name/--label/--placeholder/--testid/--nth` (and `--selector`
/// with `--nth`). Text matches are case-insensitive, exact before contains.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Locator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub testid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
    /// 0-based; negative counts from the end (-1: the last).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nth: Option<i64>,
}

impl Locator {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (k, v) in [
            ("role", &self.role),
            ("name", &self.name),
            ("label", &self.label),
            ("placeholder", &self.placeholder),
            ("testid", &self.testid),
            ("selector", &self.css),
        ] {
            if let Some(v) = v {
                parts.push(format!("{k}={v:?}"));
            }
        }
        if let Some(n) = self.nth {
            parts.push(format!("nth={n}"));
        }
        parts.join(" ")
    }
}

/// How `click` presses: which button, and whether to skip the checks a
/// user's click passes (`--force`: DOM events on the element, covered or
/// not).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClickOptions {
    pub button: String,
    pub force: bool,
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Selector(s) => format!("selector {s:?}"),
            Target::Ref(r) => format!("ref {r}"),
            Target::Text(t) => format!("text {t:?}"),
            Target::Locator(l) => l.describe(),
        }
    }
}

/// Where element queries look, as a JS expression: the top document first,
/// then its open shadow roots, then the documents of same-origin frames
/// (iframes, framesets, `srcdoc`) and their shadow roots. A plain CSS query
/// stops at a shadow root or a frame; a user's eyes don't. Cross-origin
/// frames are out of reach here (their elements are in `ax` with refs).
pub const DEEP: &str = r#"(() => {
    const docs = (d, out) => {
        out.push(d);
        for (const f of d.querySelectorAll('iframe, frame')) {
            let cd = null;
            try { cd = f.contentDocument; } catch (e) {}
            if (cd && cd.documentElement) docs(cd, out);
        }
        return out;
    };
    const shadows = (root, out) => {
        const tw = (root.ownerDocument || root).createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
        for (let n = tw.nextNode(); n; n = tw.nextNode()) {
            if (n.shadowRoot) { out.push(n.shadowRoot); shadows(n.shadowRoot, out); }
        }
        return out;
    };
    const roots = () => { const out = []; for (const d of docs(document, [])) { out.push(d); shadows(d, out); } return out; };
    return {
        roots,
        docs: () => docs(document, []),
        // The top document's own answer first: a plain query costs nothing
        // extra when the element is where it usually is.
        first(sel) {
            const el = document.querySelector(sel);
            if (el) return el;
            for (const r of roots().slice(1)) { const e = r.querySelector(sel); if (e) return e; }
            return null;
        },
        all(sel) { const out = []; for (const r of roots()) out.push(...r.querySelectorAll(sel)); return out; },
        style: (el) => (el.ownerDocument.defaultView || window).getComputedStyle(el),
        text() {
            let t = '';
            for (const d of docs(document, [])) {
                if (d.body) t += '\n' + d.body.innerText;
                for (const s of shadows(d, [])) t += '\n' + s.textContent;
            }
            return t;
        },
    };
})()"#;

/// Scroll the element into view and, if it is visible and actually receives
/// a pointer event at its centre, return that point for a *trusted* CDP
/// mouse event (`isTrusted: true`, default actions, focus, user activation
/// — what real sites expect).
///
/// First it lets the element settle: CSS animations and transitions running
/// on it or an ancestor (a modal sliding in) are allowed to finish, because
/// a user clicks what has stopped moving, and pages often wire a dialog's
/// buttons only once it has finished opening. Pages with nothing animating
/// pay nothing for this.
///
/// An element under something else (a toast, an overlay, a sticky header)
/// is scrolled to other positions, then waited for. If it stays covered,
/// `strict` returns `{covered}` and nothing is clicked: a user can't reach
/// it either. Without `strict` (`--force`, or an engine with no layout) the
/// element gets DOM-dispatched events, reported as `synthetic`. An element
/// with no box at all always gets those.
const CLICK_PREP: &str = r#"async function (el, force, settleMs, strict, button) {
    const deadline = Date.now() + (force ? 0 : settleMs);
    // The element's ancestors through shadow hosts: what can move it.
    const chain = new Set();
    for (let n = el; n; n = n.parentElement || (n.getRootNode && n.getRootNode().host) || null) chain.add(n);
    const moving = () => {
        const d = el.ownerDocument;
        if (!d.getAnimations) return [];
        return d.getAnimations().filter((a) => {
            if (a.playState !== 'running' || !a.effect || !chain.has(a.effect.target)) return false;
            const t = a.effect.getComputedTiming ? a.effect.getComputedTiming() : {};
            return Number.isFinite(t.endTime);
        });
    };
    const settle = async () => {
        const anims = moving();
        if (!anims.length) return;
        await Promise.race([
            Promise.all(anims.map((a) => a.finished.catch(() => null))),
            new Promise((res) => setTimeout(res, Math.max(0, deadline - Date.now()))),
        ]);
        await new Promise((res) => requestAnimationFrame(() => res()));
    };
    // Mouse events are dispatched in top-level viewport coordinates: add the
    // offsets of every iframe on the way up (null if one is cross-origin).
    const topLevel = (doc, x, y) => {
        let w = doc.defaultView;
        while (w && w !== w.top) {
            const fe = w.frameElement;
            if (!fe) return null;
            const fr = fe.getBoundingClientRect();
            x += fr.left + fe.clientLeft; y += fr.top + fe.clientTop;
            w = w.parent;
        }
        return { x, y };
    };
    // Will a click here likely load a new document (link, form submit)?
    const mayNavigate = (e) => {
        const a = e.closest('a[href]');
        if (a) { const h = a.getAttribute('href') || ''; return !(h.startsWith('#') || h.startsWith('javascript:')); }
        const c = e.closest('button, input');
        return !!(c && c.form && (c.type === 'submit' || c.type === 'image'));
    };
    // A styled checkbox/radio is often a hidden or covered <input> inside a
    // visible <label>: clicking the label is what a user does and toggles it.
    const labels = (el.labels ? [...el.labels] : []);
    const onLabel = (hit) => labels.some((l) => l === hit || l.contains(hit));
    // Where to put the element in the viewport: the centre first, then the
    // edges, which is what uncovers it from a sticky header or footer.
    const ALIGN = [
        { block: 'center', inline: 'center' }, { block: 'start', inline: 'nearest' },
        { block: 'end', inline: 'nearest' }, { block: 'nearest', inline: 'nearest' },
    ];
    // A disabled control can't be clicked by a user; wait for it to be
    // enabled (a form that unlocks Submit once valid), else say so.
    const disabled = () => !!(el.disabled || el.closest('fieldset[disabled]'));
    while (!force && disabled() && Date.now() < deadline) await new Promise((res) => setTimeout(res, 50));
    if (!force && strict && disabled()) return { disabled: true };
    let hit = null, tries = 0;
    while (!force) {
        // Checked on every pass: an element that just appeared may only now
        // be starting to slide in.
        await settle();
        const align = ALIGN[tries++ % ALIGN.length];
        el.scrollIntoView(Object.assign({ behavior: 'instant' }, align));
        let box = el, r = el.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) {
            const l = labels.find((l) => { const b = l.getBoundingClientRect(); return b.width > 0 && b.height > 0; });
            if (l) { box = l; l.scrollIntoView(Object.assign({ behavior: 'instant' }, align)); r = l.getBoundingClientRect(); }
        }
        const x = r.left + r.width / 2, y = r.top + r.height / 2;
        const root = box.getRootNode(), doc = box.ownerDocument;
        hit = (r.width > 0 && r.height > 0) ? (root.elementFromPoint ? root.elementFromPoint(x, y) : doc.elementFromPoint(x, y)) : null;
        if (hit && (hit === el || el.contains(hit) || (hit.shadowRoot && hit.shadowRoot.contains(el)) || onLabel(hit))) {
            const p = topLevel(doc, x, y);
            if (p) {
                // Count the press reaching this document: Chrome can ack a
                // dispatched mouse event it never delivered (see click()).
                const w = doc.defaultView;
                w.top.__nvSeen = 0;
                for (const t of ['pointerdown', 'mousedown', 'mouseup', 'click'])
                    w.addEventListener(t, () => { w.top.__nvSeen++; }, { capture: true, once: true });
                return Object.assign(p, { nav: mayNavigate(el), check: true });
            }
            // Inside a cross-origin frame: our coordinates are frame-local;
            // the caller asks CDP for the box instead.
            return { xframe: true, nav: mayNavigate(el) };
        }
        // Covered (a toast, a closing overlay) or not laid out yet: try the
        // other alignments at once, then give it a moment, like a user
        // would. An element with no box at all rarely gets one, and one
        // whose own ancestor answers the hit test is hidden on purpose (a
        // styled control's real input): don't wait long for either.
        if (hit && tries < ALIGN.length) continue;
        const hiddenHere = !hit || hit.contains(el);
        if (Date.now() >= deadline || (hiddenHere && Date.now() >= deadline - settleMs + 300)) break;
        await new Promise((res) => setTimeout(res, 100));
    }
    const describe = (n) => {
        if (!n) return null;
        let s = n.tagName.toLowerCase() + (n.id ? '#' + n.id : '') + (n.classList && n.classList.length ? '.' + n.classList[0] : '');
        const t = (n.innerText || n.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim();
        return t ? s + ' ' + JSON.stringify(t.slice(0, 40)) : s;
    };
    // What a user would click for a hidden control: its label, or the
    // nearest ancestor that looks clickable and has a box.
    const visibleFor = () => {
        const sized = (n) => { const b = n.getBoundingClientRect(); return b.width >= 4 && b.height >= 4; };
        const lab = labels.find(sized);
        if (lab) return lab;
        const CLICKY = 'a[href],button,label,summary,[role=button],[role=radio],[role=checkbox],[role=option],[role=tab],[role=switch],[role=menuitem],[onclick],[tabindex]';
        for (let n = el.parentElement || (el.getRootNode() && el.getRootNode().host); n && n !== el.ownerDocument.body; n = n.parentElement || (n.getRootNode() && n.getRootNode().host)) {
            if (!sized(n)) continue;
            if (n.matches(CLICKY) || (el.ownerDocument.defaultView || window).getComputedStyle(n).cursor === 'pointer') return n;
        }
        return null;
    };
    if (!force && strict && (!hit || hit.contains(el))) {
        // Hidden where it is: zero size, clipped, or under its own wrapper.
        // A user can't click it; they click what stands for it.
        return { hidden: true, at: describe(hit), instead: describe(visibleFor()) };
    }
    if (hit && !force && strict) {
        // Under something else the whole time: a user would hit that
        // instead. Nothing was clicked.
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        return { covered: true, covered_by: describe(hit) };
    }
    const view = el.ownerDocument.defaultView;
    const b = button === 'right' ? 2 : button === 'middle' ? 1 : 0;
    const types = b === 2 ? ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'contextmenu']
        : b === 1 ? ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'auxclick']
        : ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click'];
    for (const type of types) {
        el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, composed: true, button: b, view }));
    }
    return { synthetic: true, covered: !!hit && !force, covered_by: hit && hit !== el ? describe(hit) : null, nav: mayNavigate(el) };
}"#;

/// The second half of a DOM-event double click (the first click's events
/// were sent by CLICK_PREP).
const DBLCLICK_REST: &str = r#"function (el) {
    const view = el.ownerDocument.defaultView;
    for (const type of ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click']) {
        el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, composed: true, detail: 2, view }));
    }
    el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, cancelable: true, composed: true, detail: 2, view }));
    return true;
}"#;

/// What `get` reads from an element.
pub const GET_EL: &str = r#"function (el, what, name) {
    switch (what) {
        case 'text': return (el.innerText ?? el.textContent ?? '').trim();
        case 'value': return el.value ?? null;
        case 'attr': return el.getAttribute(name);
        case 'html': return el.innerHTML;
        case 'outer': return el.outerHTML;
        case 'box': { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; }
        case 'style': return (el.ownerDocument.defaultView || window).getComputedStyle(el).getPropertyValue(name);
        case 'checked': return !!el.checked;
        default: throw new Error('get: unknown property ' + what);
    }
}"#;

/// The states `is` answers.
pub const IS_EL: &str = r#"function (el, state) {
    const visible = () => el.getClientRects().length > 0 && (el.ownerDocument.defaultView || window).getComputedStyle(el).visibility !== 'hidden';
    const disabled = () => !!(el.disabled || el.closest('fieldset[disabled]') || el.getAttribute('aria-disabled') === 'true');
    switch (state) {
        case 'visible': return visible();
        case 'hidden': return !visible();
        case 'enabled': return !disabled();
        case 'disabled': return disabled();
        case 'checked': return !!(el.checked || el.getAttribute('aria-checked') === 'true');
        case 'editable': return !disabled() && !el.readOnly && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
        case 'focused': { const a = el.getRootNode().activeElement || el.ownerDocument.activeElement; return el === a || el.contains(a); }
        default: throw new Error('is: unknown state ' + state + ' (visible, hidden, enabled, disabled, checked, editable, focused)');
    }
}"#;

/// Hover: like CLICK_PREP but the fallback only fires enter/over events.
const HOVER_PREP: &str = r#"async function (el, settleMs) {
    const deadline = Date.now() + settleMs;
    for (;;) {
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        const r = el.getBoundingClientRect();
        const x = r.left + r.width / 2, y = r.top + r.height / 2;
        const doc = el.ownerDocument, root = el.getRootNode();
        const hit = (r.width > 0 && r.height > 0) ? (root.elementFromPoint ? root.elementFromPoint(x, y) : doc.elementFromPoint(x, y)) : null;
        if (hit && (hit === el || el.contains(hit) || (hit.shadowRoot && hit.shadowRoot.contains(el)))) {
            let ox = 0, oy = 0, w = doc.defaultView;
            while (w && w !== w.top) {
                const fe = w.frameElement;
                if (!fe) { ox = NaN; break; }
                const fr = fe.getBoundingClientRect();
                ox += fr.left + fe.clientLeft; oy += fr.top + fe.clientTop;
                w = w.parent;
            }
            if (!Number.isNaN(ox)) return { x: x + ox, y: y + oy };
            return { xframe: true };
        }
        if (Date.now() >= deadline) break;
        await new Promise((res) => setTimeout(res, 100));
    }
    for (const type of ['pointerover', 'pointerenter', 'mouseover', 'mouseenter']) {
        el.dispatchEvent(new MouseEvent(type, { bubbles: true, composed: true, view: window }));
    }
    return { synthetic: true };
}"#;

/// Fill `el` with `value`: native value setter (so React & co. see it) plus
/// `input`/`change` events. Contenteditable hosts get their text replaced.
/// A field whose centre is under something else (a sticky bar, an
/// overlapping panel) is scrolled until it isn't, as a user would before
/// typing; some pages check.
const FILL_EL: &str = r#"async function (el, value, settleMs) {
    // A disabled or read-only field takes no typing; wait for it to unlock.
    const locked = () => !!(el.disabled || el.readOnly || el.closest('fieldset[disabled]'));
    const end = Date.now() + (settleMs || 0);
    while (locked() && Date.now() < end) await new Promise((res) => setTimeout(res, 50));
    if (locked()) throw new Error('fill: the field is ' + (el.readOnly ? 'read-only' : 'disabled') + '; a user can\'t type into it');
    const covered = () => {
        const r = el.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return false;
        const hit = (el.getRootNode().elementFromPoint ? el.getRootNode() : el.ownerDocument).elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return !!hit && hit !== el && !el.contains(hit);
    };
    if (covered()) {
        for (const block of ['center', 'start', 'end', 'nearest']) {
            el.scrollIntoView({ block, inline: 'nearest', behavior: 'instant' });
            if (!covered()) break;
        }
    }
    el.focus();
    if (el.isContentEditable) {
        el.textContent = value;
        el.dispatchEvent(new InputEvent('input', { bubbles: true, composed: true }));
        return true;
    }
    if (el.tagName === 'SELECT') throw new Error('fill: this is a <select>; use `select --value`');
    if (!('value' in el)) throw new Error('fill: element is not an input, textarea or contenteditable (' + el.tagName.toLowerCase() + ')');
    // The setter of the element's own window: an element in a frame is
    // not an instance of this window's classes.
    const view = el.ownerDocument.defaultView || window;
    const proto = el.tagName === 'TEXTAREA' ? view.HTMLTextAreaElement.prototype
        : el.tagName === 'INPUT' ? view.HTMLInputElement.prototype : null;
    const desc = proto ? Object.getOwnPropertyDescriptor(proto, 'value') : null;
    if (desc && desc.set) { desc.set.call(el, value); } else { el.value = value; }
    el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
}"#;

/// Choose `<select>` options by value or visible label (exact first, then
/// with whitespace collapsed -- non-breaking spaces count as spaces -- then
/// case-insensitive), firing input/change like a user would. The chosen
/// options replace the selection, or join it with `add` (a multiple
/// select, a user holding Ctrl).
const SELECT_EL: &str = r#"function (el, wanted, add) {
    if (el.tagName !== 'SELECT') {
        throw new Error('select: element is ' + el.tagName.toLowerCase() + ', not a <select>; for a custom dropdown, click it and then click the option by ref');
    }
    const opts = Array.from(el.options);
    const norm = (s) => s.replace(/\s+/g, ' ').trim();
    const pick = (w) => opts.find(o => o.value === w) || opts.find(o => o.label.trim() === w)
        || opts.find(o => norm(o.label) === norm(w)) || opts.find(o => norm(o.value) === norm(w))
        || opts.find(o => norm(o.label).toLowerCase() === norm(w).toLowerCase());
    const chosen = wanted.map(w => { const o = pick(w); if (!o) throw new Error('select: no option ' + JSON.stringify(w) + ' (have: ' + opts.map(o => JSON.stringify(o.label.trim())).slice(0, 30).join(', ') + ')'); return o; });
    if (!el.multiple && chosen.length > 1) throw new Error('select: single-choice <select>, got ' + chosen.length + ' values');
    if (add && !el.multiple) throw new Error('select --add: this <select> takes one option');
    for (const o of opts) o.selected = chosen.includes(o) || (add && o.selected);
    el.focus();
    el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return opts.filter(o => o.selected).map(o => norm(o.label));
}"#;

/// A custom dropdown's parts: whether `el` is (or holds, or sits in) an
/// ARIA combobox, which element takes keys (it gets focus), whether it is
/// open, and the listbox it controls.
pub const COMBO_FOCUS: &str = r#"function (el) {
    const isCombo = (n) => !!n && n.nodeType === 1 && (n.getAttribute('role') === 'combobox'
        || n.getAttribute('aria-haspopup') === 'listbox' || n.hasAttribute('aria-expanded'));
    let box = isCombo(el) ? el : (el.closest('[role=combobox],[aria-haspopup=listbox]') || el.querySelector('[role=combobox],[aria-haspopup=listbox]'));
    if (!box) return { combobox: false, tag: el.tagName.toLowerCase() };
    const field = box.matches('input,textarea,[contenteditable=""],[contenteditable=true]') ? box
        : box.querySelector('input:not([type=hidden]),textarea,[contenteditable=""],[contenteditable=true]');
    const keys = !!field || box.tabIndex >= 0;
    (field || box).focus();
    const expandedOf = (n) => n && n.getAttribute && n.getAttribute('aria-expanded');
    const expanded = [box, field].some((n) => expandedOf(n) === 'true');
    const ids = [box, field].map((n) => n && (n.getAttribute('aria-controls') || n.getAttribute('aria-owns'))).filter(Boolean);
    return { combobox: true, keys, expanded, listbox: ids[0] ? ids[0].split(/\s+/)[0] : null };
}"#;

/// Find `want` among the options of the open listbox (the one the
/// combobox names, else any visible one): exact text, then the start, then
/// anywhere, ignoring case and whitespace. A match is marked with a
/// one-off attribute so it can be clicked by a trusted click.
pub const COMBO_PICK: &str = r#"function (want, listboxId, typed, deep) {
    const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
    const w = norm(want);
    const shown = (n) => { const r = n.getBoundingClientRect(); return r.width > 0 && r.height > 0 && deep.style(n).visibility !== 'hidden'; };
    let boxes = [];
    if (listboxId) for (const r of deep.roots()) { const b = r.getElementById ? r.getElementById(listboxId) : null; if (b) boxes.push(b); }
    if (!boxes.some(shown)) boxes = deep.all('[role=listbox]');
    boxes = boxes.filter(shown);
    const opts = [];
    for (const b of boxes) opts.push(...b.querySelectorAll('[role=option]'));
    const live = opts.filter(shown);
    const label = (o) => norm(o.getAttribute('aria-label') || o.innerText || o.textContent);
    const hit = live.find((o) => label(o) === w) || live.find((o) => label(o).startsWith(w)) || live.find((o) => label(o).includes(w))
        || live.find((o) => norm(o.getAttribute('data-value') || o.getAttribute('value')) === w);
    const options = live.slice(0, 20).map((o) => (o.innerText || o.textContent || '').trim());
    if (!hit) return { open: boxes.length > 0, picked: false, options };
    const token = Math.random().toString(36).slice(2, 10);
    hit.setAttribute('data-nv-pick', token);
    hit.scrollIntoView({ block: 'nearest', behavior: 'instant' });
    return { open: true, picked: true, token, label: (hit.innerText || hit.textContent || '').trim(), options };
}"#;

/// Focus a field and select everything in it, so typing replaces it.
pub const FOCUS_SELECT_ALL: &str = r#"function (el) {
    el.scrollIntoView({ block: 'center', behavior: 'instant' });
    el.focus();
    if (typeof el.select === 'function') el.select();
    else if (el.isContentEditable) {
        const r = el.ownerDocument.createRange();
        r.selectNodeContents(el);
        const sel = (el.ownerDocument.defaultView || window).getSelection();
        sel.removeAllRanges();
        sel.addRange(r);
    }
    const a = el.getRootNode().activeElement || el.ownerDocument.activeElement;
    return a === el || el.contains(a);
}"#;

/// Focus an element (for `press`/`type` with a target).
pub const FOCUS_EL: &str = r#"function (el) {
    el.scrollIntoView({ block: 'center', behavior: 'instant' });
    el.focus();
    const a = el.getRootNode().activeElement || el.ownerDocument.activeElement;
    return a === el || el.contains(a);
}"#;

/// Scroll an element into view (or scroll inside it by `dy`).
/// With `to` (`top`/`bottom`) the element's own content scrolls there: a
/// list that loads more as it reaches its end.
const SCROLL_EL: &str = r#"function (el, dy, to) {
    if (to === 'bottom') { el.scrollTo({ top: el.scrollHeight, behavior: 'instant' }); }
    else if (to === 'top') { el.scrollTo({ top: 0, behavior: 'instant' }); }
    else if (dy) { el.scrollBy({ top: dy, behavior: 'instant' }); }
    else { el.scrollIntoView({ block: 'center', behavior: 'instant' }); }
    const out = { scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight };
    if (to || dy) { out.scrollTop = Math.round(el.scrollTop); out.elementScrollHeight = el.scrollHeight; }
    return out;
}"#;

/// The element itself (for ops that need its remote object, like upload).
/// Is the element (or an ancestor) an HTML5 drag source?
const DRAG_IS_HTML5: &str = "function (el) { return !!el.closest('[draggable=true]'); }";

/// dragstart on the drag source, with a DataTransfer kept for the drop.
const DRAG_START: &str = r#"function (el) {
  const fire = (t, type, dt) => {
    const r = t.getBoundingClientRect();
    const o = { bubbles: true, cancelable: true, composed: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 };
    const real = typeof DataTransfer !== 'undefined' && dt instanceof DataTransfer;
    let ev = null;
    try { ev = new DragEvent(type, Object.assign({ dataTransfer: real ? dt : null }, o)); } catch (e) {}
    if (!ev || ev.dataTransfer !== dt) {
      ev = ev || new Event(type, o);
      Object.defineProperty(ev, 'dataTransfer', { value: dt });
    }
    return t.dispatchEvent(ev);
  };
  const src = el.closest('[draggable=true]') || el;
  let dt;
  try { dt = new DataTransfer(); } catch (e) {
    const data = {};
    dt = { dropEffect: 'move', effectAllowed: 'all', files: [], items: [], types: [],
           setData(k, v) { data[k] = String(v); if (!this.types.includes(k)) this.types.push(k); },
           getData(k) { return data[k] ?? ''; }, clearData(k) { if (k) delete data[k]; else for (const x in data) delete data[x]; },
           setDragImage() {} };
  }
  window.__nvDrag = { src, dt };
  fire(src, 'dragstart', dt);
  return true;
}"#;

/// dragenter, dragover and (when the page allows it) drop on the target,
/// then dragend on the source. True when the drop happened.
const DRAG_DROP: &str = r#"function (el) {
  const fire = (t, type, dt) => {
    const r = t.getBoundingClientRect();
    const o = { bubbles: true, cancelable: true, composed: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 };
    const real = typeof DataTransfer !== 'undefined' && dt instanceof DataTransfer;
    let ev = null;
    try { ev = new DragEvent(type, Object.assign({ dataTransfer: real ? dt : null }, o)); } catch (e) {}
    if (!ev || ev.dataTransfer !== dt) {
      ev = ev || new Event(type, o);
      Object.defineProperty(ev, 'dataTransfer', { value: dt });
    }
    return t.dispatchEvent(ev);
  };
  const d = window.__nvDrag;
  if (!d) throw new Error('drag: the drag did not start');
  delete window.__nvDrag;
  fire(el, 'dragenter', d.dt);
  const allowed = !fire(el, 'dragover', d.dt);
  if (allowed) fire(el, 'drop', d.dt);
  fire(d.src, 'dragend', d.dt);
  return allowed;
}"#;

const IDENTITY: &str = "function (el) { return el; }";

/// Auto-wait: resolve the first match for `sel`, polling every 50 ms for up
/// to `ms` (SPAs render after load; an agent shouldn't have to poll). The
/// top document first, then open shadow roots and same-origin frames.
const WAIT_FOR: &str = r#"async function (sel, ms, what, deepFn) {
    const end = Date.now() + ms;
    let deep = null;
    // Somewhere a user could see it: a box, not hidden, not parked at a
    // negative position no scrolling reaches (a decoy copy of an id).
    const reachable = (e) => {
        const r = e.getBoundingClientRect(), w = e.ownerDocument.defaultView || window;
        return r.width > 0 && r.height > 0 && r.right + w.scrollX > 0 && r.bottom + w.scrollY > 0
            && w.getComputedStyle(e).visibility !== 'hidden';
    };
    for (;;) {
        // The plain query first: the deep search is built (and compiled)
        // only when the top document has no match. When the first match is
        // out of a user's reach and another isn't, the other is the one.
        let el = document.querySelector(sel);
        if (el && !reachable(el)) {
            for (const c of document.querySelectorAll(sel)) if (reachable(c)) { el = c; break; }
        }
        el = el || (deep = deep || deepFn()).first(sel);
        if (el) return el;
        if (Date.now() >= end) throw new Error(what + ': no element matches selector ' + JSON.stringify(sel) + ' within ' + ms + ' ms (run `ax` to see the page and act by --ref)');
        await new Promise((r) => setTimeout(r, 50));
    }
}"#;

/// Auto-wait by role / accessible name / label / placeholder / test id /
/// CSS, then pick the nth match. Visible elements come first; text matches
/// are case-insensitive, exact before contains. Walks open shadow roots.
const WAIT_LOCATOR: &str = r#"async function (spec, ms, what, deep) {
    const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
    const all = () => { const out = []; for (const r of deep.roots()) out.push(...r.querySelectorAll('*')); return out; };
    const implicitRole = (el) => {
        const t = el.tagName, type = (el.getAttribute('type') || '').toLowerCase();
        switch (t) {
            case 'A': case 'AREA': return el.hasAttribute('href') ? 'link' : null;
            case 'BUTTON': case 'SUMMARY': return 'button';
            case 'INPUT':
                if (['button', 'submit', 'reset', 'image'].includes(type)) return 'button';
                if (type === 'checkbox') return 'checkbox';
                if (type === 'radio') return 'radio';
                if (type === 'range') return 'slider';
                if (type === 'number') return 'spinbutton';
                if (type === 'search') return 'searchbox';
                if (type === 'hidden' || type === 'file') return null;
                return el.hasAttribute('list') ? 'combobox' : 'textbox';
            case 'TEXTAREA': return 'textbox';
            case 'SELECT': return (el.multiple || el.size > 1) ? 'listbox' : 'combobox';
            case 'OPTION': return 'option';
            case 'H1': case 'H2': case 'H3': case 'H4': case 'H5': case 'H6': return 'heading';
            case 'IMG': return el.getAttribute('alt') === '' ? 'presentation' : 'img';
            case 'UL': case 'OL': return 'list';
            case 'LI': return 'listitem';
            case 'NAV': return 'navigation';
            case 'MAIN': return 'main';
            case 'ASIDE': return 'complementary';
            case 'FORM': return 'form';
            case 'TABLE': return 'table';
            case 'TR': return 'row';
            case 'TD': return 'cell';
            case 'TH': return 'columnheader';
            case 'DIALOG': return 'dialog';
            case 'FIELDSET': case 'DETAILS': return 'group';
            case 'PROGRESS': return 'progressbar';
            case 'ARTICLE': return 'article';
            case 'SECTION': return (el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby')) ? 'region' : null;
            case 'HEADER': return el.closest('article,aside,main,nav,section') ? null : 'banner';
            case 'FOOTER': return el.closest('article,aside,main,nav,section') ? null : 'contentinfo';
        }
        return null;
    };
    const role = (el) => (el.getAttribute('role') || '').split(' ')[0] || implicitRole(el);
    const byIds = (el, attr) => (el.getAttribute(attr) || '').split(/\s+/).filter(Boolean)
        .map((id) => (el.getRootNode().getElementById ? el.getRootNode().getElementById(id) : document.getElementById(id)))
        .filter(Boolean).map((n) => n.textContent).join(' ');
    const labelText = (el) => {
        const own = el.getAttribute('aria-label') || byIds(el, 'aria-labelledby');
        if (own) return own;
        if (el.labels && el.labels.length) return [...el.labels].map((l) => l.textContent).join(' ');
        return '';
    };
    const FROM_CONTENT = new Set(['button', 'link', 'heading', 'cell', 'columnheader', 'rowheader', 'option', 'tab',
        'menuitem', 'menuitemcheckbox', 'menuitemradio', 'treeitem', 'listitem', 'checkbox', 'radio', 'switch', 'row', 'tooltip']);
    const accName = (el) => {
        const l = labelText(el);
        if (l) return l;
        if (el.tagName === 'IMG' || el.tagName === 'AREA' || (el.tagName === 'INPUT' && el.type === 'image')) return el.getAttribute('alt') || el.title || '';
        if (el.tagName === 'INPUT' && ['button', 'submit', 'reset'].includes(el.type)) return el.value || el.type;
        if (FROM_CONTENT.has(role(el))) return el.innerText || el.textContent || el.title || '';
        return el.title || el.getAttribute('placeholder') || '';
    };
    const matches = (got, want) => { const g = norm(got), w = norm(want); return g === w ? 2 : (w && g.includes(w) ? 1 : 0); };
    const visible = (el) => el.getClientRects().length > 0 && deep.style(el).visibility !== 'hidden';
    const end = Date.now() + ms;
    for (;;) {
        let found = [];
        for (const el of (spec.css ? deep.all(spec.css) : all())) {
            let score = 0, ok = true;
            const need = (s) => { if (!s) ok = false; else score += s; };
            if (spec.testid) need(['data-testid', 'data-test-id', 'data-test', 'data-qa'].some((a) => el.getAttribute(a) === spec.testid) ? 2 : 0);
            if (ok && spec.role) need(role(el) === spec.role.toLowerCase() ? 2 : 0);
            if (ok && spec.name) need(matches(accName(el), spec.name));
            if (ok && spec.label) need(matches(labelText(el), spec.label));
            if (ok && spec.placeholder) need(matches(el.getAttribute('placeholder') || el.getAttribute('aria-placeholder'), spec.placeholder));
            if (ok) found.push({ el, score: score + (visible(el) ? 10 : 0) });
        }
        // Best score first; document order among equals.
        found = found.sort((a, b) => b.score - a.score).map((f) => f.el);
        if (spec.nth !== undefined && spec.nth !== null) {
            const top = found.length ? found : [];
            const i = spec.nth < 0 ? top.length + spec.nth : spec.nth;
            if (i >= 0 && i < top.length) return top[i];
        } else if (found.length) return found[0];
        if (Date.now() >= end) {
            const desc = Object.entries(spec).map(([k, v]) => k + '=' + JSON.stringify(v)).join(' ');
            throw new Error(what + ': no element with ' + desc + ' within ' + ms + ' ms (' + found.length + ' matched; run `ax` to see the page)');
        }
        await new Promise((r) => setTimeout(r, 50));
    }
}"#;

/// Auto-wait by visible text: the best element whose accessible label,
/// text or value matches `text` — exact (case-insensitive) beats contains,
/// actionable elements beat plain ones, visible beats hidden. Walks open
/// shadow roots.
const WAIT_TEXT: &str = r#"async function (text, ms, what, deep) {
    const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
    const want = norm(text);
    // The longest word is the cheap pre-filter for text nodes.
    const key = want.split(' ').reduce((a, b) => (b.length > a.length ? b : a), '');
    const SKIP = new Set(['SCRIPT', 'STYLE', 'TEMPLATE', 'NOSCRIPT', 'HEAD', 'TITLE', 'META', 'LINK']);
    const ACTION = 'a,button,input,select,textarea,summary,label,option,[role],[onclick],[tabindex],[contenteditable=""],[contenteditable="true"]';
    const maxLen = want.length * 4 + 40;
    const label = (el) => norm(el.getAttribute('aria-label') || (el.labels && el.labels[0] && el.labels[0].innerText)
        || el.innerText || el.value || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt'));
    // Candidates without touching layout: parents of text nodes holding the
    // phrase (and a few small ancestors, for text split across inline
    // tags), plus elements whose label comes from attributes or a value.
    // The whole phrase is tried first; only if no text node holds it is the
    // longest word used. Walks open shadow roots; never <head>, scripts or
    // styles. Big containers (lists, tables) are never climbed into.
    const lenCache = new WeakMap();
    const textLen = (el) => {
        let n = lenCache.get(el);
        if (n === undefined) { n = (el.textContent || '').length; lenCache.set(el, n); }
        return n;
    };
    const fromText = (root, needle, out) => {
        const tw = (root.ownerDocument || root).createTreeWalker(root, NodeFilter.SHOW_TEXT);
        for (let n = tw.nextNode(); n; n = tw.nextNode()) {
            if (!n.data.replace(/\s+/g, ' ').toLowerCase().includes(needle)) continue;
            let p = n.parentElement;
            for (let up = 0; p && up < 4; up++, p = p.parentElement) {
                if (SKIP.has(p.tagName) || p.childElementCount > 12 || textLen(p) > maxLen * 2) break;
                out.add(p);
            }
        }
        for (const el of root.querySelectorAll('*')) if (el.shadowRoot) fromText(el.shadowRoot, needle, out);
        return out;
    };
    const fromAttrs = (root, out) => {
        for (const el of root.querySelectorAll('[aria-label],[placeholder],[title],[alt],input,textarea,select')) out.add(el);
        for (const el of root.querySelectorAll('*')) if (el.shadowRoot) fromAttrs(el.shadowRoot, out);
        return out;
    };
    const candidates = (root) => {
        let out = fromText(root, want, new Set());
        if (out.size === 0 && key !== want) out = fromText(root, key, out);
        return fromAttrs(root, out);
    };
    const visible = (el) => {
        const r = el.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return false;
        return el.checkVisibility ? el.checkVisibility({ visibilityProperty: true }) : true;
    };
    const end = Date.now() + ms;
    for (;;) {
        let best = null, bestScore = -1;
        // Document by document, the top one first: a match there wins over
        // one inside a frame.
        for (const d of deep.docs()) {
            for (const el of candidates(d.body || d.documentElement)) {
                if (SKIP.has(el.tagName) || el.closest('head')) continue;
                const l = label(el);
                if (!l || l.length > maxLen) continue;
                const exact = l === want, has = l.includes(want);
                if (!exact && !has) continue;
                // Visible beats everything: an exact match on a hidden element
                // must not win over the button the agent can see.
                const score = (visible(el) ? 8 : 0) + (exact ? 4 : 0) + (el.matches(ACTION) ? 2 : 0) - l.length / 1000;
                if (score > bestScore) { best = el; bestScore = score; }
            }
            if (best) break;
        }
        // A <label> stands for its control (fill/select/upload need the input).
        if (best && best.tagName === 'LABEL' && best.control) return best.control;
        if (best) return best;
        if (Date.now() >= end) throw new Error(what + ': no element with text ' + JSON.stringify(text) + ' within ' + ms + ' ms (run `ax` to see the page and act by --ref)');
        await new Promise((r) => setTimeout(r, 100));
    }
}"#;

/// Key name -> (key, code, windowsVirtualKeyCode, text) for `press`.
fn key_info(name: &str) -> Option<(String, String, u32, Option<String>)> {
    let named: &[(&str, &str, u32, Option<&str>)] = &[
        ("Enter", "Enter", 13, Some("\r")),
        ("Tab", "Tab", 9, None),
        ("Escape", "Escape", 27, None),
        ("Backspace", "Backspace", 8, None),
        ("Delete", "Delete", 46, None),
        ("ArrowUp", "ArrowUp", 38, None),
        ("ArrowDown", "ArrowDown", 40, None),
        ("ArrowLeft", "ArrowLeft", 37, None),
        ("ArrowRight", "ArrowRight", 39, None),
        ("Home", "Home", 36, None),
        ("End", "End", 35, None),
        ("PageUp", "PageUp", 33, None),
        ("PageDown", "PageDown", 34, None),
        ("Space", "Space", 32, Some(" ")),
        ("Control", "ControlLeft", 17, None),
        ("Shift", "ShiftLeft", 16, None),
        ("Alt", "AltLeft", 18, None),
        ("Meta", "MetaLeft", 91, None),
    ];
    let aliases = [
        ("Esc", "Escape"),
        ("Return", "Enter"),
        ("Up", "ArrowUp"),
        ("Down", "ArrowDown"),
        ("Left", "ArrowLeft"),
        ("Right", "ArrowRight"),
        ("Ctrl", "Control"),
        ("Cmd", "Meta"),
        (" ", "Space"),
    ];
    let name = aliases.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)).map(|(_, k)| *k).unwrap_or(name);
    if let Some((key, code, vk, text)) = named.iter().find(|(k, ..)| k.eq_ignore_ascii_case(name)) {
        let key = if *key == "Space" { " " } else { key };
        return Some((key.to_string(), code.to_string(), *vk, text.map(str::to_string)));
    }
    if let Some(n) = name.strip_prefix(['F', 'f']).and_then(|n| n.parse::<u32>().ok()) {
        if (1..=12).contains(&n) {
            return Some((format!("F{n}"), format!("F{n}"), 111 + n, None));
        }
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return None };
    let upper = c.to_ascii_uppercase();
    let code = if c.is_ascii_alphabetic() {
        format!("Key{upper}")
    } else if c.is_ascii_digit() {
        format!("Digit{c}")
    } else {
        String::new()
    };
    let vk = if c.is_ascii_alphanumeric() { upper as u32 } else { 0 };
    Some((c.to_string(), code, vk, Some(c.to_string())))
}

/// The character Shift produces on a US keyboard.
fn shifted_char(c: char) -> char {
    const PLAIN: &str = "1234567890-=[]\\;',./`";
    const SHIFTED: &str = "!@#$%^&*()_+{}|:\"<>?~";
    if c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }
    PLAIN.chars().position(|p| p == c).and_then(|i| SHIFTED.chars().nth(i)).unwrap_or(c)
}

/// Does Enter on the focused element (through open shadow roots and
/// same-origin frames) possibly navigate? Form controls, links, buttons.
/// What open shadow roots hold, for an engine whose accessibility tree leaves
/// them out (Lightpanda: neither its AX tree nor `DOM.getDocument` with
/// `pierce` reaches inside one, and `DOM.describeNode` refuses a node there,
/// so no ref can point at it either). One entry per actionable element or
/// live region: `{host, tag, role, name, value?, checked?, level?}`, `host`
/// numbering the shadow roots in document order. `scope` limits it to roots
/// under the element a selector names.
const SHADOW_AX: &str = r#"(scope) => {
  const top = scope ? document.querySelector(scope) : document;
  if (!top) return [];
  const out = [];
  const squash = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const bare = (node) => {
    const copy = node.cloneNode(true);
    for (const c of copy.querySelectorAll('input, select, textarea, button')) c.remove();
    return squash(copy.textContent);
  };
  const nameOf = (el) => {
    const aria = el.getAttribute('aria-label');
    if (aria && squash(aria)) return squash(aria);
    const by = el.getAttribute('aria-labelledby');
    if (by) {
      const root = el.getRootNode();
      const t = by.split(/\s+/).map((id) => root.getElementById && root.getElementById(id)).filter(Boolean).map((n) => n.textContent).join(' ');
      if (squash(t)) return squash(t);
    }
    if (/^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName)) {
      const label = (el.labels && el.labels[0]) || el.closest('label');
      if (label && bare(label)) return bare(label);
      for (const a of ['placeholder', 'title']) if (el.getAttribute(a)) return squash(el.getAttribute(a));
      if (el.tagName === 'INPUT' && /^(submit|button|reset)$/i.test(el.type)) return squash(el.value);
      return '';
    }
    return squash(el.getAttribute('title') || el.textContent).slice(0, 100);
  };
  const roleOf = (el) => {
    const explicit = el.getAttribute('role');
    if (explicit) return explicit;
    const t = el.tagName;
    if (t === 'A' && el.hasAttribute('href')) return 'link';
    if (t === 'BUTTON' || t === 'SUMMARY') return 'button';
    if (t === 'SELECT') return el.multiple ? 'listbox' : 'combobox';
    if (t === 'TEXTAREA') return 'textbox';
    if (t === 'OUTPUT') return 'status';
    if (/^H[1-6]$/.test(t)) return 'heading';
    if (t === 'INPUT') {
      const ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'hidden') return null;
      return { checkbox: 'checkbox', radio: 'radio', number: 'spinbutton', range: 'slider', search: 'searchbox',
               submit: 'button', button: 'button', reset: 'button', image: 'button', file: 'button' }[ty] || 'textbox';
    }
    if (el.isContentEditable && el.hasAttribute('contenteditable')) return 'textbox';
    return null;
  };
  const hidden = (el) => {
    if (el.hidden || el.closest('[hidden], [aria-hidden="true"]')) return true;
    try { return getComputedStyle(el).display === 'none'; } catch (e) { return false; }
  };
  let hosts = 0;
  const visit = (root, host, tag) => {
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) visit(el.shadowRoot, ++hosts, el.localName);
      if (host === 0 || out.length >= 200) continue;
      try {
        const role = roleOf(el);
        if (!role || hidden(el)) continue;
        const name = nameOf(el);
        const live = /^(status|alert|log)$/.test(role);
        if (live && !name) continue;
        const entry = { host, tag, role, name };
        if (/^(textbox|searchbox|spinbutton|combobox|slider)$/.test(role) && el.value) entry.value = String(el.value);
        if (role === 'checkbox' || role === 'radio') entry.checked = !!el.checked;
        if (role === 'heading') entry.level = +el.tagName.slice(1) || 2;
        out.push(entry);
      } catch (e) {}
    }
  };
  visit(top, 0, '');
  return out;
}"#;

const ENTER_MAY_NAVIGATE: &str = r#"(() => {
    let e = document.activeElement;
    for (;;) {
        if (e && e.shadowRoot && e.shadowRoot.activeElement) { e = e.shadowRoot.activeElement; continue; }
        if (e && (e.tagName === 'IFRAME' || e.tagName === 'FRAME')) {
            let d = null; try { d = e.contentDocument; } catch (_) {}
            if (!d) return true;
            e = d.activeElement; continue;
        }
        break;
    }
    if (!e || e === document.body) return false;
    return !!(e.form || e.closest('a[href], button, [role=button], [role=link]'));
})()"#;

fn modifier_bit(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "alt" | "option" => Some(1),
        "control" | "ctrl" => Some(2),
        "meta" | "cmd" | "command" => Some(4),
        "shift" => Some(8),
        _ => None,
    }
}

/// JS-side wait budget: a bit under the CDP command timeout, so the page's
/// own "not found" error wins the race against the transport timeout.
/// A JS expression awaiting the element `target` names (not for refs,
/// which resolve through CDP).
fn finder_expr(target: &Target, wait_ms: u128, what: &str) -> String {
    match target {
        Target::Selector(sel) => format!("({WAIT_FOR})({}, {wait_ms}, {}, () => {DEEP})", json!(sel), json!(what)),
        Target::Text(text) => format!("({WAIT_TEXT})({}, {wait_ms}, {}, {DEEP})", json!(text), json!(what)),
        Target::Locator(loc) => format!("({WAIT_LOCATOR})({}, {wait_ms}, {}, {DEEP})", json!(loc), json!(what)),
        Target::Ref(node) => format!("Promise.reject(new Error('{what}: ref {node} must be resolved through CDP'))"),
    }
}

/// The page an evaluate ran in was replaced by a navigation before it
/// answered.
fn navigated_away(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    ["Inspected target navigated or closed", "Execution context was destroyed", "Cannot find context with specified id"]
        .iter()
        .any(|s| m.contains(s))
}

/// The element wait the page runs: the wait asked for, unless the CDP
/// command timeout is so close that the page's own "not found" (which
/// names the element) could lose the race to the transport's timeout.
fn wait_budget_ms(wait: Duration, timeout: Duration) -> u128 {
    if timeout >= wait + Duration::from_millis(500) {
        wait.as_millis().max(1)
    } else {
        timeout.as_millis().saturating_sub(250).max(1)
    }
}

/// Readable message from a CDP `exceptionDetails` (first lines of the
/// exception description, not the whole JSON blob).
fn exception_message(details: &Value) -> String {
    let description = details
        .get("exception")
        .and_then(|e| e.get("description").or_else(|| e.get("value")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| details.get("text").and_then(Value::as_str).unwrap_or("exception").to_string());
    // "Error: " is the class of every page error: the op's own prefix
    // already says what failed.
    let description = description.strip_prefix("Error: ").unwrap_or(&description);
    description.lines().filter(|l| !l.trim_start().starts_with("at ")).take(4).collect::<Vec<_>>().join("\n")
}

/// `node[key].value` as a string ("" when absent).
fn ax_str<'a>(node: &'a Value, key: &str) -> &'a str {
    node.get(key).and_then(|v| v.get("value")).and_then(Value::as_str).unwrap_or("")
}

/// `ax` options passed down from the protocol.
#[derive(Clone, Debug, Default)]
pub struct AxOptions {
    pub max_depth: Option<u32>,
    /// Legacy flat JSON (`[{ref, role, name, value?}]`) instead of text.
    pub json: bool,
    /// With `json`: every non-ignored node in the raw shape.
    pub all: bool,
    pub all_refs: bool,
    pub limit: usize,
    pub scope: Option<Target>,
}

#[derive(Clone)]
pub struct Page {
    handle: tokio::runtime::Handle,
    client: CdpClient,
    session_id: String,
    target_id: String,
    shared: Arc<Shared>,
}

impl Page {
    pub fn new(
        handle: tokio::runtime::Handle,
        client: CdpClient,
        session_id: String,
        target_id: String,
        shared: Arc<Shared>,
    ) -> Self {
        Self { handle, client, session_id, target_id, shared }
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.handle.block_on(fut)
    }

    /// Page events only. `Runtime.enable` is deliberately not sent: nothing
    /// here needs execution-context or console events, it costs a round
    /// trip per tab plus a stream of events, and it is the best-known
    /// automation fingerprint anti-bot scripts probe for.
    pub async fn enable(&self) -> Result<()> {
        let session = Some(self.session_id.as_str());
        let send = |method: &'static str| self.client.send(method, json!({}), session, CMD_TIMEOUT);
        // One round trip for all of them. Page is required; the rest is best
        // effort: Runtime and Log feed `console`/`errors` (console calls,
        // uncaught exceptions, failed loads, CSP reports), Network feeds
        // `network` and `network_quiet` (an engine without it just never
        // waits for data). `NAVIGERA_NO_NETWORK=1` (diagnostic) leaves
        // Network off.
        let network = async {
            if std::env::var_os("NAVIGERA_NO_NETWORK").is_none() {
                let _ = send("Network.enable").await;
            }
        };
        let (page, _, _, _) = tokio::join!(send("Page.enable"), send("Runtime.enable"), send("Log.enable"), network);
        page?;
        Ok(())
    }

    /// Wait (at most `max`) until the page has produced two animation
    /// frames: by then a tab brought to the front has painted and takes
    /// input. Returns false on timeout (a page that can't paint).
    pub fn wait_frame(&self, max: Duration) -> bool {
        let ms = max.as_millis();
        let js = format!(
            "new Promise(r => {{ setTimeout(() => r(false), {ms}); \
             requestAnimationFrame(() => requestAnimationFrame(() => r(true))); }})"
        );
        self.send(
            "Runtime.evaluate",
            json!({ "expression": js, "awaitPromise": true, "returnByValue": true }),
            max + Duration::from_secs(1),
        )
        .ok()
        .and_then(|r| r.pointer("/result/value").and_then(Value::as_bool))
        .unwrap_or(false)
    }

    /// Wait until no fetch/XHR is in flight and none started or ended for
    /// `quiet`, at most `max`. A single-page app usually renders what it
    /// fetched right after the response: reading the page earlier shows
    /// its "Loading…" placeholder. Returns at once on a page that has been
    /// quiet (typically: the agent was thinking for seconds).
    pub fn network_quiet(&self, quiet: Duration, max: Duration) -> bool {
        let deadline = std::time::Instant::now() + max;
        loop {
            let (pending, last) = self.shared.net_state(&self.session_id);
            let idle_for = last.map(|t| t.elapsed()).unwrap_or(Duration::MAX);
            if pending == 0 && idle_for >= quiet {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.block_on(self.client.send(method, params, Some(&self.session_id), timeout))
    }

    /// Send one CDP command to this tab's session.
    pub fn send_cdp(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.send(method, params, timeout)
    }

    /// Evaluate a JS expression; the result comes back by value in one round
    /// trip. Bare arrow functions are invoked; IIFEs and plain expressions
    /// evaluate to their value.
    pub fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value> {
        // Wrap once: if the expression evaluates to a function, call it.
        // Handles `() => …`, `(() => …)()`, and plain `document.title`.
        let wrapped = format!(
            "(() => {{ const __r = ({}\n); return (typeof __r === 'function') ? __r() : __r; }})()",
            expression
        );
        self.evaluate_raw(&wrapped, timeout)
    }

    fn evaluate_raw(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let res = self.send(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            timeout,
        )?;
        if let Some(details) = res.get("exceptionDetails") {
            bail!("{}", exception_message(details));
        }
        // returnByValue => result.result.value holds the JSON value.
        Ok(res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }

    /// Navigate the main frame. `wait` is `load` (default), `domcontentloaded`
    /// or `commit` (return once the navigation is committed).
    ///
    /// Subscribes to CDP events *before* navigating so a fast local page that
    /// loads before the response returns isn't missed.
    pub fn navigate(&self, url: &str, wait: &str, timeout: Duration) -> Result<()> {
        self.block_on(async {
            let mut events = self.client.subscribe();
            let res = self
                .client
                .send(
                    "Page.navigate",
                    json!({ "url": url }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .with_context(|| format!("navigate to {url}"))?;
            if let Some(error) = res.get("errorText").and_then(Value::as_str) {
                bail!("navigate to {url}: {error}");
            }
            if wait == "commit" {
                if res.get("loaderId").is_some() {
                    self.shared.mark_loading(&self.session_id);
                }
                return Ok(());
            }
            // A same-document navigation (hash change) has no loaderId and
            // fires no load event.
            if res.get("loaderId").is_none() {
                return Ok(());
            }
            let loader = res.get("loaderId").and_then(Value::as_str).unwrap_or("").to_string();
            let want = if wait == "domcontentloaded" {
                "Page.domContentEventFired"
            } else {
                "Page.loadEventFired"
            };
            // Count load events only after OUR navigation committed (its
            // frameNavigated carries the loaderId Page.navigate returned), so
            // a late load event from the previous document can't end the wait.
            let mut committed = false;
            self.wait_event(&mut events, timeout, |method, params| {
                if method == "Page.frameNavigated" {
                    let frame = params.get("frame").unwrap_or(&Value::Null);
                    let main = frame.get("parentId").is_none();
                    let ours = loader.is_empty()
                        || frame.get("loaderId").and_then(Value::as_str) == Some(loader.as_str());
                    if main && ours {
                        committed = true;
                    }
                }
                committed && method == want
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "timed out after {} ms waiting for {} of {url} (a slow subresource? try `goto --wait domcontentloaded`)",
                    timeout.as_millis(),
                    if wait == "domcontentloaded" { "DOMContentLoaded" } else { "load" }
                )
            })?;
            Ok(())
        })
    }

    /// Wait for an event of THIS tab's session satisfying `pred(method, params)`.
    async fn wait_event(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<Value>,
        timeout: Duration,
        mut pred: impl FnMut(&str, &Value) -> bool,
    ) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if event.get("sessionId").and_then(Value::as_str) != Some(self.session_id.as_str()) {
                            continue;
                        }
                        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
                        if pred(method, event.get("params").unwrap_or(&Value::Null)) {
                            return Ok(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => bail!("CDP event stream ended: {e}"),
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {} ms", timeout.as_millis()))?
    }

    /// Navigate and return as soon as the navigation commits (no load wait).
    pub fn navigate_commit(&self, url: &str, timeout: Duration) -> Result<()> {
        self.navigate(url, "commit", timeout)
    }

    /// Back/forward through session history (`delta` = -1 / +1).
    pub fn history(&self, delta: i64, timeout: Duration) -> Result<()> {
        let history = self.send("Page.getNavigationHistory", json!({}), timeout)?;
        let index = history.get("currentIndex").and_then(Value::as_i64).unwrap_or(0) + delta;
        let entries = history.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(entry) = usize::try_from(index).ok().and_then(|i| entries.get(i)) else {
            bail!("no {} history entry", if delta < 0 { "previous" } else { "next" });
        };
        let id = entry.get("id").cloned().unwrap_or(Value::Null);
        self.block_on(async {
            let mut events = self.client.subscribe();
            self.client
                .send("Page.navigateToHistoryEntry", json!({ "entryId": id }), Some(&self.session_id), timeout)
                .await?;
            // Committed (cross-document, back/forward cache) or a
            // same-document history step: either way the URL has changed.
            self.wait_event(&mut events, timeout, |method, params| match method {
                "Page.frameNavigated" => params.get("frame").is_some_and(|f| f.get("parentId").is_none()),
                "Page.navigatedWithinDocument" => true,
                _ => false,
            })
            .await
        })?;
        self.settle(timeout)
    }

    pub fn reload(&self, timeout: Duration) -> Result<()> {
        self.block_on(async {
            let mut events = self.client.subscribe();
            self.client.send("Page.reload", json!({}), Some(&self.session_id), timeout).await?;
            let mut committed = false;
            self.wait_event(&mut events, timeout, |method, params| {
                if method == "Page.frameNavigated" && params.get("frame").is_some_and(|f| f.get("parentId").is_none()) {
                    committed = true;
                }
                committed && method == "Page.loadEventFired"
            })
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for reload"))
        })
    }

    /// If an earlier action started a navigation in this tab, wait (up to
    /// `timeout`) until its new document is parsed. No-op otherwise.
    pub fn settle(&self, timeout: Duration) -> Result<()> {
        let end = Instant::now() + timeout;
        loop {
            let state = self.shared.nav_state(&self.session_id);
            // A request that never starts loading (cancelled, 204, download)
            // stops counting after 2 s.
            let pending = state.requested_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
            if !pending && (!state.loading || state.dom_ready) {
                return Ok(());
            }
            // Committed but still parsing after POST_COMMIT_WAIT: the page
            // streams or hangs on a resource. Its DOM is the new page, so
            // reading it is safe; waiting longer only stalls the agent
            // (the op result then says `loading: true`).
            if !pending && state.committed_at.is_some_and(|t| t.elapsed() >= POST_COMMIT_WAIT) {
                return Ok(());
            }
            if Instant::now() >= end {
                bail!("timed out after {} ms waiting for a navigation to finish", timeout.as_millis());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Navigation generation (bumped by every request / load start).
    pub fn nav_generation(&self) -> u64 {
        self.shared.nav_state(&self.session_id).generation
    }

    /// After an action that may navigate (a link, a submit, Enter in a
    /// form): give the navigation up to `grace` to be requested — form
    /// submissions are scheduled as a task, so the request can trail the
    /// input event's CDP reply — then wait for the new document.
    pub fn navigation_started(&self, since_generation: u64, grace: Duration) -> bool {
        let end = Instant::now() + grace;
        loop {
            if self.nav_generation() > since_generation {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// True while a navigation is requested or the document is still being
    /// parsed (before DOMContentLoaded).
    /// One round trip that runs nothing, on Lightpanda only. That engine
    /// reports a navigation a script scheduled -- `location.href = ...` in a
    /// click handler -- when it handles the next CDP message, so the events
    /// trail the click's own reply, and a settle that looked right then found
    /// nothing to wait for: the op returned the old page with `loading: true`.
    /// This is that next message. Chrome reports them before the reply.
    pub fn flush_events(&self, timeout: Duration) {
        if self.shared.is_single_target() {
            let _ = self.send("Runtime.evaluate", json!({ "expression": "0", "returnByValue": true }), timeout);
        }
    }

    pub fn is_loading(&self) -> bool {
        let state = self.shared.nav_state(&self.session_id);
        state.requested_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) || (state.loading && !state.dom_ready)
    }

    pub fn title(&self, timeout: Duration) -> Result<String> {
        let value = self.evaluate_raw("document.title", timeout)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    pub fn url(&self, timeout: Duration) -> Result<String> {
        let value = self.evaluate_raw("location.href", timeout)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    /// Run `function (el, ...args)` on the element `target` names, waiting
    /// up to `wait` for selector/text targets to appear. One round trip for
    /// selector/text; resolve + call + release for refs.
    ///
    /// A navigation can replace the page while the op is still looking for
    /// its element (a reload a click started after a request, a client-side
    /// redirect). The page's script dies with it; the element is then looked
    /// for again on the new page, within what is left of the wait. Nothing
    /// acted yet when that happens: clicks only measure here, and the ops
    /// that act in the page (fill, select) are the same op again.
    pub(crate) fn on_element(
        &self,
        target: &Target,
        what: &str,
        function: &str,
        args: &[Value],
        wait: Duration,
        timeout: Duration,
    ) -> Result<Value> {
        let started = Instant::now();
        let mut tries = 0;
        loop {
            let left =
                if tries == 0 { wait } else { wait.saturating_sub(started.elapsed()).max(Duration::from_millis(250)) };
            match self.on_element_once(target, what, function, args, left, timeout) {
                Err(e) if tries < 3 && !matches!(target, Target::Ref(_)) && navigated_away(&e) => {
                    tries += 1;
                    self.flush_events(Duration::from_millis(100));
                    let _ = self.settle(left + POST_COMMIT_WAIT);
                }
                other => return other,
            }
        }
    }

    fn on_element_once(
        &self,
        target: &Target,
        what: &str,
        function: &str,
        args: &[Value],
        wait: Duration,
        timeout: Duration,
    ) -> Result<Value> {
        let arg_list: Vec<String> = args.iter().map(Value::to_string).collect();
        let extra = if arg_list.is_empty() { String::new() } else { format!(", {}", arg_list.join(", ")) };
        let wait_ms = wait_budget_ms(wait, timeout);
        let finder = match target {
            Target::Ref(node) => {
                let fn_on_this = format!(
                    "function (...args) {{ const el = this.nodeType === 1 ? this : this.parentElement; \
                     if (!el) throw new Error({}); return ({function})(el, ...args); }}",
                    json!(format!("{what}: ref is not inside an element"))
                );
                return self.call_on_ref(*node, &fn_on_this, args, true, timeout).map(|(v, _)| v);
            }
            other => finder_expr(other, wait_ms, what),
        };
        let expr = format!("(async () => {{ const el = await {finder}; return ({function})(el{extra}); }})()");
        self.evaluate_raw(&expr, timeout)
    }

    /// Resolve an `ax` ref (`backendNodeId`) and call `function` on it.
    /// Returns the value (or remote object id when `by_value` is false).
    fn call_on_ref(
        &self,
        backend_node_id: u64,
        function: &str,
        args: &[Value],
        by_value: bool,
        timeout: Duration,
    ) -> Result<(Value, Option<String>)> {
        self.block_on(async {
            let resolved = self
                .client
                .send(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": backend_node_id, "objectGroup": "nv-ref" }),
                    Some(&self.session_id),
                    timeout,
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "ref {backend_node_id} not found (stale after navigation or re-render? take a fresh `ax`)"
                    )
                })?;
            let object_id = resolved
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("ref {backend_node_id} has no JS object"))?
                .to_string();
            let arguments: Vec<Value> = args.iter().map(|v| json!({ "value": v })).collect();
            let res = self
                .client
                .send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": function,
                        "arguments": arguments,
                        "returnByValue": by_value,
                        "awaitPromise": true,
                    }),
                    Some(&self.session_id),
                    timeout,
                )
                .await;
            if by_value {
                // Free the resolved handle whatever happened.
                let _ = self
                    .client
                    .send(
                        "Runtime.releaseObjectGroup",
                        json!({ "objectGroup": "nv-ref" }),
                        Some(&self.session_id),
                        timeout,
                    )
                    .await;
            }
            let res = res.with_context(|| format!("call on ref {backend_node_id}"))?;
            if let Some(details) = res.get("exceptionDetails") {
                bail!("{}", exception_message(details));
            }
            let object = res.get("result").and_then(|r| r.get("objectId")).and_then(Value::as_str).map(str::to_string);
            Ok((res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null), object))
        })
    }

    /// Click: a trusted CDP mouse click at the element's centre once it has
    /// stopped moving, or DOM-dispatched events when it has no hittable box
    /// or `force` says so (see [`CLICK_PREP`]). An element that stays under
    /// another one is an error naming that one, not a click a user couldn't
    /// make. Returns `{synthetic: true}` when DOM events were used.
    pub fn click(&self, target: &Target, opts: &ClickOptions, wait: Duration, timeout: Duration) -> Result<Value> {
        let settle = json!(self.hit_settle_ms(wait));
        let strict = json!(!self.shared.is_single_target());
        let button = json!(opts.button);
        let dom = |page: &Self| {
            page.on_element(
                target,
                "click",
                CLICK_PREP,
                &[json!(true), settle.clone(), strict.clone(), button.clone()],
                wait,
                timeout,
            )
        };
        if opts.force {
            return dom(self);
        }
        let mut point = self.on_element(
            target,
            "click",
            CLICK_PREP,
            &[json!(false), settle.clone(), strict.clone(), button.clone()],
            wait,
            timeout,
        )?;
        if point.get("disabled").and_then(Value::as_bool) == Some(true) {
            bail!("the element is disabled (waited {settle} ms for it to be enabled); a user can't click it");
        }
        if point.get("hidden").and_then(Value::as_bool) == Some(true) {
            let instead = point.get("instead").and_then(Value::as_str);
            bail!(
                "the element isn't visible where it is ({}): a user can't click it{}. \
                 `--force` sends DOM events to it anyway",
                match point.get("at").and_then(Value::as_str) {
                    Some(at) => format!("a click there lands on {at}"),
                    None => "no box on screen".to_string(),
                },
                instead.map(|i| format!("; click what stands for it: {i}")).unwrap_or_default()
            );
        }
        if point.get("covered").and_then(Value::as_bool) == Some(true)
            && point.get("synthetic").and_then(Value::as_bool) != Some(true)
        {
            bail!(
                "the element is covered by {} at its centre (waited {} ms; a user would click that instead). \
                 Close or wait for it; `--force` sends DOM events to the element anyway",
                point.get("covered_by").and_then(Value::as_str).unwrap_or("another element"),
                settle
            );
        }
        if point.get("xframe").and_then(Value::as_bool) == Some(true) {
            // Inside a cross-origin iframe the page can't see its frame's
            // offset; CDP reports the box in top-level coordinates.
            match target {
                Target::Ref(node) => match self.content_quad_center(*node, timeout) {
                    Some((x, y)) => {
                        point["x"] = json!(x);
                        point["y"] = json!(y);
                    }
                    None => return dom(self),
                },
                _ => return dom(self),
            }
        }
        if !self.mouse_click(&point, &opts.button, timeout)? {
            // Engine without the Input domain: DOM events.
            return dom(self);
        }
        let check = point.get("check").and_then(Value::as_bool) == Some(true)
            && std::env::var_os("NAVIGERA_NO_CLICK_CHECK").is_none(); // diagnostic knob
        if check && !self.press_arrived(timeout) {
            eprintln!(
                "[click] the browser did not deliver the mouse events at ({}, {}); sending them again",
                point["x"], point["y"]
            );
            // Chrome acked the mouse events without delivering them: no
            // pointerdown/mousedown/mouseup/click reached the page. Seen in
            // CI right after a tab switch, when the tab has no hit-test data
            // yet and the browser finds no target for the event. Nothing
            // happened on the page, so sending it again can't double-click.
            self.wait_frame(Duration::from_millis(500));
            self.mouse_click(&point, &opts.button, timeout)?;
            if !self.press_arrived(timeout) {
                eprintln!("[click] still not delivered; dispatching DOM events");
                let mut res = dom(self)?;
                res["dropped"] = json!(true);
                return Ok(res);
            }
            point["redelivered"] = json!(true);
        }
        Ok(point)
    }

    /// How long a click waits for its element to stop moving and become
    /// hittable (covered by a closing overlay, not laid out yet): the
    /// element wait. Lightpanda has no layout that could change: its boxes
    /// are synthetic, and inside shadow roots its hit test never matches,
    /// so waiting there only delays the same DOM-event click.
    fn hit_settle_ms(&self, wait: Duration) -> u64 {
        if self.shared.is_single_target() {
            0
        } else {
            wait.as_millis() as u64
        }
    }

    /// Did the click's press reach the page (`__nvSeen`, set by CLICK_PREP)?
    /// An error (the click navigated away, the context is gone) counts as
    /// arrived: only a definite "nothing came" triggers a resend.
    /// A slow page gets 300 ms to process the events before "nothing came"
    /// is believed (a dropped event never arrives; a queued one does).
    fn press_arrived(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        loop {
            let seen = self
                .send("Runtime.evaluate", json!({ "expression": "window.__nvSeen", "returnByValue": true }), timeout)
                .ok()
                .map(|r| r.pointer("/result/value").and_then(Value::as_u64));
            match seen {
                Some(Some(0)) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Some(Some(0)) => return false,
                _ => return true,
            }
        }
    }

    /// Centre of a node's first content quad, in top-level viewport
    /// coordinates (works inside cross-origin iframes).
    fn content_quad_center(&self, backend_node_id: u64, timeout: Duration) -> Option<(f64, f64)> {
        let res = self.send("DOM.getContentQuads", json!({ "backendNodeId": backend_node_id }), timeout).ok()?;
        let quad: Vec<f64> =
            res.get("quads")?.as_array()?.first()?.as_array()?.iter().filter_map(Value::as_f64).collect();
        if quad.len() != 8 {
            return None;
        }
        let x = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
        let y = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
        Some((x, y))
    }

    /// Dispatch a real mouse click at `point.{x,y}`. Ok(false) when the
    /// engine has no Input domain (the very first event fails), so the
    /// caller can fall back to DOM events without ever clicking twice; a
    /// failure after the press is an error. No-op (Ok(true)) when the page
    /// already used DOM events.
    fn mouse_click(&self, point: &Value, button: &str, timeout: Duration) -> Result<bool> {
        let (Some(x), Some(y)) = (point.get("x").and_then(Value::as_f64), point.get("y").and_then(Value::as_f64))
        else {
            return Ok(true);
        };
        let (button, held) = match button {
            "right" => ("right", 2),
            "middle" => ("middle", 4),
            _ => ("left", 1),
        };
        for (i, (kind, button, buttons)) in
            [("mouseMoved", "none", 0), ("mousePressed", button, held), ("mouseReleased", button, 0)]
                .into_iter()
                .enumerate()
        {
            let sent = self.send(
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind, "x": x, "y": y,
                    "button": button, "buttons": buttons, "clickCount": 1,
                }),
                timeout,
            );
            match sent {
                Ok(_) => {}
                Err(_) if i == 0 => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Double click: two trusted presses at the element's centre (the
    /// browser fires click, click, dblclick), or DOM events where no trusted
    /// click is possible.
    pub fn dblclick(&self, target: &Target, wait: Duration, timeout: Duration) -> Result<Value> {
        let settle = json!(self.hit_settle_ms(wait));
        let strict = json!(!self.shared.is_single_target());
        let mut point = self.on_element(
            target,
            "dblclick",
            CLICK_PREP,
            &[json!(false), settle.clone(), strict.clone(), json!("left")],
            wait,
            timeout,
        )?;
        if point.get("hidden").and_then(Value::as_bool) == Some(true) {
            bail!(
                "the element isn't visible where it is; a user can't double-click it{}",
                point.get("instead").and_then(Value::as_str).map(|i| format!(" (try {i})")).unwrap_or_default()
            );
        }
        if point.get("covered").and_then(Value::as_bool) == Some(true)
            && point.get("synthetic").and_then(Value::as_bool) != Some(true)
        {
            bail!(
                "the element is covered by {} at its centre; close or wait for it",
                point.get("covered_by").and_then(Value::as_str).unwrap_or("another element")
            );
        }
        if point.get("xframe").and_then(Value::as_bool) == Some(true) {
            match target {
                Target::Ref(node) => match self.content_quad_center(*node, timeout) {
                    Some((x, y)) => point = json!({ "x": x, "y": y }),
                    None => bail!("dblclick: element is inside a cross-origin iframe without a box"),
                },
                _ => bail!("dblclick: element is inside a cross-origin iframe; target it by ref from `ax`"),
            }
        }
        if point.get("synthetic").and_then(Value::as_bool) == Some(true) {
            // CLICK_PREP already sent the first click's events.
            self.on_element(target, "dblclick", DBLCLICK_REST, &[], wait, timeout)?;
            return Ok(point);
        }
        let (Some(x), Some(y)) = (point.get("x").and_then(Value::as_f64), point.get("y").and_then(Value::as_f64))
        else {
            return Ok(point);
        };
        let steps = [
            ("mouseMoved", "none", 0, 0),
            ("mousePressed", "left", 1, 1),
            ("mouseReleased", "left", 0, 1),
            ("mousePressed", "left", 1, 2),
            ("mouseReleased", "left", 0, 2),
        ];
        for (i, (kind, button, buttons, count)) in steps.into_iter().enumerate() {
            let sent = self.send(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count }),
                timeout,
            );
            match sent {
                Ok(_) => {}
                Err(_) if i == 0 => {
                    self.on_element(
                        target,
                        "dblclick",
                        CLICK_PREP,
                        &[json!(true), settle.clone(), strict.clone(), json!("left")],
                        wait,
                        timeout,
                    )?;
                    self.on_element(target, "dblclick", DBLCLICK_REST, &[], wait, timeout)?;
                    return Ok(json!({ "synthetic": true }));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(point)
    }

    /// Drag `from` onto `to`.
    ///
    /// A `draggable` element is HTML5 drag and drop, which headless Chrome
    /// does not start from synthetic mouse input: it gets the events a
    /// browser fires, dragstart through dragend, with one DataTransfer shared
    /// from start to drop (a stand-in where the engine has no DataTransfer).
    /// Anything else gets a trusted press over `from`, a move in steps and a
    /// release over `to`, which sliders and pointer-driven sortable lists
    /// follow. Returns `{html5, dropped?}` or the two points.
    pub fn drag(&self, from: &Target, to: &Target, wait: Duration, timeout: Duration) -> Result<Value> {
        let html5 = self.on_element(from, "drag", DRAG_IS_HTML5, &[], wait, timeout)?;
        if html5 == Value::Bool(true) {
            self.on_element(from, "drag", DRAG_START, &[], wait, timeout)?;
            let dropped = self.on_element(to, "drag", DRAG_DROP, &[], wait, timeout)?;
            return Ok(json!({ "html5": true, "dropped": dropped }));
        }
        let settle = json!(self.hit_settle_ms(wait));
        let a = self.on_element(from, "drag", HOVER_PREP, std::slice::from_ref(&settle), wait, timeout)?;
        let b = self.on_element(to, "drag", HOVER_PREP, &[settle], wait, timeout)?;
        let point = |p: &Value| Some((p.get("x")?.as_f64()?, p.get("y")?.as_f64()?));
        let (Some((ax, ay)), Some((bx, by))) = (point(&a), point(&b)) else {
            bail!("drag: no on-screen point for both elements (inside a cross-origin iframe?)");
        };
        let mut events = vec![("mouseMoved", ax, ay, "none", 0), ("mousePressed", ax, ay, "left", 1)];
        for i in 1..=8 {
            let t = i as f64 / 8.0;
            events.push(("mouseMoved", ax + (bx - ax) * t, ay + (by - ay) * t, "left", 1));
        }
        events.push(("mouseReleased", bx, by, "left", 0));
        for (kind, x, y, button, buttons) in events {
            self.send(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": 1 }),
                timeout,
            )?;
        }
        Ok(json!({ "from": { "x": ax, "y": ay }, "to": { "x": bx, "y": by } }))
    }

    /// Press or release one key (`down`), without the other half.
    pub fn key_event(&self, name: &str, down: bool, timeout: Duration) -> Result<()> {
        let (key, code, vk, text) = key_info(name).ok_or_else(|| {
            anyhow::anyhow!("unknown key {name:?} (use Shift, Control, ArrowDown, a single character, …)")
        })?;
        let mut event = json!({
            "type": if !down { "keyUp" } else if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key, "code": code, "windowsVirtualKeyCode": vk,
        });
        if let (true, Some(t)) = (down, &text) {
            event["text"] = json!(t);
        }
        self.send("Input.dispatchKeyEvent", event, timeout).map(|_| ())
    }

    /// Insert text at the caret in one go (`Input.insertText`): like a paste
    /// or an IME commit, no per-key events.
    pub fn insert_text(&self, target: Option<&Target>, text: &str, wait: Duration, timeout: Duration) -> Result<()> {
        if let Some(target) = target {
            self.on_element(target, "insert-text", FOCUS_EL, &[], wait, timeout)?;
        }
        self.send("Input.insertText", json!({ "text": text }), timeout).map(|_| ())
    }

    /// Move the mouse over the element (opens hover menus and tooltips).
    /// Returns `{synthetic: true}` when only DOM events could be sent.
    pub fn hover(&self, target: &Target, wait: Duration, timeout: Duration) -> Result<Value> {
        let settle = json!(self.hit_settle_ms(wait));
        let mut point = self.on_element(target, "hover", HOVER_PREP, &[settle], wait, timeout)?;
        if point.get("xframe").and_then(Value::as_bool) == Some(true) {
            let center = match target {
                Target::Ref(node) => self.content_quad_center(*node, timeout),
                _ => None,
            };
            match center {
                Some((x, y)) => point = json!({ "x": x, "y": y }),
                None => bail!("hover: element is inside a cross-origin iframe; target it by ref from `ax`"),
            }
        }
        if let (Some(x), Some(y)) = (point.get("x").and_then(Value::as_f64), point.get("y").and_then(Value::as_f64)) {
            self.send(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0 }),
                timeout,
            )?;
            // :hover styles apply on the next frame; let two pass so an `ax`
            // right after sees the opened menu.
            self.evaluate_raw(
                "new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))",
                timeout,
            )?;
        }
        Ok(point)
    }

    /// Set an input/textarea/contenteditable's value (native setter + events).
    pub fn fill(&self, target: &Target, value: &str, wait: Duration, timeout: Duration) -> Result<()> {
        let settle = json!(self.hit_settle_ms(wait));
        self.on_element(target, "fill", FILL_EL, &[json!(value), settle], wait, timeout)?;
        Ok(())
    }

    /// Pick `<select>` options by value or label; returns the chosen labels.
    pub fn select(
        &self,
        target: &Target,
        values: &[String],
        add: bool,
        wait: Duration,
        timeout: Duration,
    ) -> Result<Value> {
        self.on_element(target, "select", SELECT_EL, &[json!(values), json!(add)], wait, timeout)
    }

    /// Scroll: an element into view (or inside it by `dy`), else the window.
    pub fn scroll(
        &self,
        target: Option<&Target>,
        dy: Option<f64>,
        to: Option<&str>,
        wait: Duration,
        timeout: Duration,
    ) -> Result<Value> {
        if let Some(target) = target {
            if let Some(to) = to.filter(|t| !matches!(*t, "top" | "bottom")) {
                bail!("scroll --to takes top|bottom, got {to:?}");
            }
            return self.on_element(target, "scroll", SCROLL_EL, &[json!(dy.unwrap_or(0.0)), json!(to)], wait, timeout);
        }
        let action = match to {
            Some("top") => "window.scrollTo({ top: 0, behavior: 'instant' })".to_string(),
            Some("bottom") => {
                "window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'instant' })".to_string()
            }
            Some(other) => bail!("scroll --to takes top|bottom, got {other:?}"),
            None => format!("window.scrollBy({{ top: {}, behavior: 'instant' }})", dy.unwrap_or(800.0)),
        };
        self.evaluate_raw(
            &format!("(() => {{ {action}; return {{ scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight, viewport: window.innerHeight }}; }})()"),
            timeout,
        )
    }

    /// Focus `target` (if any), then press a key or chord like `Enter`,
    /// `Control+a`, `Shift+Tab`.
    /// Press a key or chord. Returns true when the key may start a
    /// navigation (Enter on a form field, link or button), so the caller
    /// knows to wait for one; other keys cost no navigation grace.
    pub fn press(&self, target: Option<&Target>, chord: &str, wait: Duration, timeout: Duration) -> Result<bool> {
        if let Some(target) = target {
            self.on_element(target, "press", FOCUS_EL, &[], wait, timeout)?;
        }
        let enter = matches!(chord.to_ascii_lowercase().as_str(), "enter" | "return");
        // Enter navigates only from a form control, a link or a button.
        // (Cross-origin frames hide their focus: assume it might.)
        let may_navigate =
            enter && self.evaluate_raw(ENTER_MAY_NAVIGATE, timeout).ok().and_then(|v| v.as_bool()).unwrap_or(true);
        let parts: Vec<&str> = chord.split('+').filter(|p| !p.is_empty()).collect();
        let parts = if chord.ends_with("++") || chord == "+" {
            let mut p = parts;
            p.push("+");
            p
        } else {
            parts
        };
        let Some((last, mods)) = parts.split_last() else {
            bail!("press needs a key, e.g. Enter, Tab, Control+a");
        };
        let mut modifiers = 0;
        for m in mods {
            modifiers |= modifier_bit(m)
                .ok_or_else(|| anyhow::anyhow!("press: unknown modifier {m:?} (use Control, Shift, Alt, Meta)"))?;
        }
        let (key, code, vk, text) = key_info(last).ok_or_else(|| {
            anyhow::anyhow!("press: unknown key {last:?} (use Enter, Tab, Escape, ArrowDown, a single character, …)")
        })?;
        // Shift on a single character types its shifted form (US layout):
        // Shift+a is "A", Shift+1 is "!".
        let (key, text) = match (&text, modifiers & 8 != 0) {
            (Some(t), true) if t.chars().count() == 1 && t != "\r" && t != " " => {
                let shifted = shifted_char(t.chars().next().unwrap_or_default()).to_string();
                (shifted.clone(), Some(shifted))
            }
            _ => (key, text),
        };
        // With Control/Alt/Meta held the key produces no text (a shortcut).
        let text = if modifiers & (1 | 2 | 4) != 0 { None } else { text };
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": modifiers,
        });
        if let Some(t) = &text {
            down["text"] = json!(t);
            down["unmodifiedText"] = json!(t);
        }
        self.send("Input.dispatchKeyEvent", down, timeout)?;
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": modifiers }),
            timeout,
        )?;
        Ok(may_navigate)
    }

    /// Type text key by key into `target` (or the focused element): real
    /// keyDown/keyUp per character, so key listeners and autocomplete fire.
    pub fn type_text(&self, target: Option<&Target>, text: &str, wait: Duration, timeout: Duration) -> Result<()> {
        if let Some(target) = target {
            // Keys go to whatever has focus: typing after a focus that didn't
            // take would land in another field.
            if self.on_element(target, "type", FOCUS_EL, &[], wait, timeout)? != Value::Bool(true) {
                bail!("the element did not take focus (disabled, hidden, or not focusable); nothing was typed");
            }
        }
        for c in text.chars() {
            if c == '\n' {
                self.press(None, "Enter", wait, timeout)?;
                continue;
            }
            let s = c.to_string();
            let (key, code, vk, _) = key_info(&s).unwrap_or((s.clone(), String::new(), 0, None));
            self.send(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk, "text": s, "unmodifiedText": s }),
                timeout,
            )?;
            self.send(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk }),
                timeout,
            )?;
        }
        Ok(())
    }

    /// Set the files of an `<input type=file>`.
    pub fn upload(&self, target: &Target, files: &[String], wait: Duration, timeout: Duration) -> Result<()> {
        let files: Vec<String> = files
            .iter()
            .map(|f| {
                std::fs::canonicalize(f)
                    .map(|p| p.display().to_string())
                    .map_err(|e| anyhow::anyhow!("upload: {f}: {e}"))
            })
            .collect::<Result<_>>()?;
        let mut params = json!({ "files": files });
        match target {
            Target::Ref(node) => params["backendNodeId"] = json!(node),
            _ => {
                // Find the element (auto-wait), keep it as a remote object.
                let finder = finder_expr(target, wait_budget_ms(wait, timeout), "upload");
                let res = self.send(
                    "Runtime.evaluate",
                    json!({ "expression": format!("(async () => {{ const el = await {finder}; return ({IDENTITY})(el); }})()"),
                            "awaitPromise": true, "objectGroup": "nv-upload" }),
                    timeout,
                )?;
                if let Some(details) = res.get("exceptionDetails") {
                    bail!("{}", exception_message(details));
                }
                let object = res
                    .get("result")
                    .and_then(|r| r.get("objectId"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("upload: element has no remote object"))?;
                params["objectId"] = json!(object);
            }
        }
        let result = self.send("DOM.setFileInputFiles", params, timeout);
        let _ = self.send("Runtime.releaseObjectGroup", json!({ "objectGroup": "nv-upload" }), timeout);
        result.map_err(|e| anyhow::anyhow!("upload: {e:#} (is it an <input type=file>?)"))?;
        Ok(())
    }

    /// First matching element's textContent, if present.
    pub fn text_content(&self, selector: &str, timeout: Duration) -> Result<Option<String>> {
        let sel = serde_json::to_string(selector)?;
        let value = self.evaluate_raw(&format!("({DEEP}.first({sel}) || {{}}).textContent ?? null"), timeout)?;
        Ok(value.as_str().map(str::to_string))
    }

    /// One poll of a `wait` condition; true when satisfied.
    pub fn check(&self, js_predicate: &str, timeout: Duration) -> Result<bool> {
        Ok(self.evaluate_raw(js_predicate, timeout)?.as_bool().unwrap_or(false))
    }

    /// PNG screenshot bytes; `full_page` captures beyond the viewport.
    pub fn screenshot_png(&self, timeout: Duration, full_page: bool) -> Result<Vec<u8>> {
        let mut params = json!({ "format": "png" });
        if full_page {
            params["captureBeyondViewport"] = Value::Bool(true);
        }
        let res = self.send("Page.captureScreenshot", params, timeout)?;
        let data =
            res.get("data").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("captureScreenshot: no data"))?;
        base64::engine::general_purpose::STANDARD.decode(data).context("screenshot base64 decode")
    }

    /// Child frames' AX trees, each keyed by its owner `<iframe>` node.
    fn frame_trees(&self, timeout: Duration) -> Vec<ax::Frame> {
        let Ok(tree) = self.send("Page.getFrameTree", json!({}), timeout) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        fn collect(node: &Value, out: &mut Vec<String>) {
            for child in node.get("childFrames").and_then(Value::as_array).into_iter().flatten() {
                if let Some(id) = child.get("frame").and_then(|f| f.get("id")).and_then(Value::as_str) {
                    out.push(id.to_string());
                }
                collect(child, out);
            }
        }
        collect(tree.get("frameTree").unwrap_or(&Value::Null), &mut ids);
        let mut frames = Vec::new();
        for id in ids.into_iter().take(20) {
            let Ok(owner) = self.send("DOM.getFrameOwner", json!({ "frameId": id }), timeout) else {
                continue;
            };
            let Some(owner_backend_id) = owner.get("backendNodeId").and_then(Value::as_u64) else {
                continue;
            };
            // Out-of-process frames live in another target; skipped here.
            let Ok(res) = self.send("Accessibility.getFullAXTree", json!({ "frameId": id }), timeout) else {
                continue;
            };
            let nodes = res.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default();
            frames.push(ax::Frame { owner_backend_id, nodes });
        }
        frames
    }

    /// Backend node id of the element `target` names (for `ax --selector`).
    fn backend_id(&self, target: &Target, wait: Duration, timeout: Duration) -> Result<u64> {
        if let Target::Ref(node) = target {
            return Ok(*node);
        }
        let finder = finder_expr(target, wait_budget_ms(wait, timeout), "ax");
        let res = self.send(
            "Runtime.evaluate",
            json!({ "expression": format!("(async () => await {finder})()"), "awaitPromise": true, "objectGroup": "nv-ax" }),
            timeout,
        )?;
        if let Some(details) = res.get("exceptionDetails") {
            bail!("{}", exception_message(details));
        }
        let object = res
            .get("result")
            .and_then(|r| r.get("objectId"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("ax: scope element has no remote object"))?
            .to_string();
        let described = self.send("DOM.describeNode", json!({ "objectId": object }), timeout);
        let _ = self.send("Runtime.releaseObjectGroup", json!({ "objectGroup": "nv-ax" }), timeout);
        described?
            .get("node")
            .and_then(|n| n.get("backendNodeId"))
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("ax: no backend node id for scope"))
    }

    /// Lightpanda gives every password field the value `*****`, typed into or
    /// not, so an empty one read as filled. Each such field is asked what it
    /// holds instead, and shows Chrome's form: a dot per character, or no
    /// value when it is empty. A field that cannot be asked keeps the stars.
    fn unmask_passwords(&self, nodes: &mut [Value], timeout: Duration) {
        for node in nodes.iter_mut() {
            let masked = node.get("value").and_then(|v| v.get("value")).and_then(Value::as_str) == Some("*****");
            let Some(id) = node.get("backendDOMNodeId").and_then(Value::as_u64) else {
                continue;
            };
            if !masked {
                continue;
            }
            let Ok((len, _)) = self.call_on_ref(
                id,
                "function () { return this.type === 'password' ? String(this.value).length : -1; }",
                &[],
                true,
                timeout,
            ) else {
                continue;
            };
            match len.as_i64() {
                Some(0) => {
                    if let Some(o) = node.as_object_mut() {
                        o.remove("value");
                    }
                }
                Some(n) if n > 0 => node["value"] = json!({ "type": "string", "value": "•".repeat(n as usize) }),
                _ => {}
            }
        }
    }

    /// Lightpanda takes a native checkbox's or radio's state in its
    /// accessibility tree from the `checked` attribute, the state the page
    /// loaded with, rather than from the element: after a click the tree
    /// still showed the old state. In the DeepSeek eval (run 39691526622) an
    /// agent on the preferences form read a box it had ticked as unticked,
    /// clicked it again, and ran out of turns with both boxes as they began.
    /// The states are read from the elements, in one call for all of them,
    /// and written into the nodes.
    fn live_checked(&self, nodes: &mut [Value], timeout: Duration) {
        let boxes: Vec<(usize, u64)> = nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                let role = n.get("role").and_then(|r| r.get("value")).and_then(Value::as_str)?;
                if role != "checkbox" && role != "radio" {
                    return None;
                }
                Some((i, n.get("backendDOMNodeId").and_then(Value::as_u64)?))
            })
            .collect();
        if boxes.is_empty() {
            return;
        }
        let session = Some(self.session_id.as_str());
        let states: Option<Vec<Value>> = self.block_on(async {
            let resolved = futures_util::future::join_all(boxes.iter().map(|&(_, id)| {
                self.client.send(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": id, "objectGroup": "nv-live" }),
                    session,
                    timeout,
                )
            }))
            .await;
            let ids: Vec<Option<String>> = resolved
                .iter()
                .map(|r| r.as_ref().ok()?.get("object")?.get("objectId")?.as_str().map(str::to_string))
                .collect();
            let this = ids.iter().flatten().next()?.clone();
            let arguments: Vec<Value> = ids
                .iter()
                .map(|id| match id {
                    Some(id) => json!({ "objectId": id }),
                    None => json!({ "value": null }),
                })
                .collect();
            let res = self
                .client
                .send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": this,
                        "functionDeclaration": "function (...els) { return els.map(e => e && e.tagName === 'INPUT' \
                            && (e.type === 'checkbox' || e.type === 'radio') ? e.checked : null); }",
                        "arguments": arguments,
                        "returnByValue": true,
                    }),
                    session,
                    timeout,
                )
                .await;
            let _ = self
                .client
                .send("Runtime.releaseObjectGroup", json!({ "objectGroup": "nv-live" }), session, timeout)
                .await;
            res.ok()?.get("result")?.get("value")?.as_array().cloned()
        });
        let Some(states) = states else { return };
        for (&(i, _), state) in boxes.iter().zip(states.iter()) {
            let Some(checked) = state.as_bool() else { continue };
            let value = json!({ "type": "tristate", "value": if checked { "true" } else { "false" } });
            let node = &mut nodes[i];
            if !node.get("properties").is_some_and(Value::is_array) {
                node["properties"] = json!([]);
            }
            let props = node["properties"].as_array_mut().expect("an array");
            match props.iter_mut().find(|p| p.get("name").and_then(Value::as_str) == Some("checked")) {
                Some(p) => p["value"] = value,
                None => props.push(json!({ "name": "checked", "value": value })),
            }
        }
    }

    /// Accessibility snapshot for agents (see [`ax`]): an indented text
    /// tree with `[ref=N]` on actionable nodes, iframes included. `json`
    /// keeps the older flat `[{ref, role, name, value?}]` list.
    pub fn ax_tree(&self, timeout: Duration, opts: &AxOptions) -> Result<Value> {
        let mut params = json!({});
        if let Some(d) = opts.max_depth {
            params["depth"] = json!(d);
        }
        let res = self.send("Accessibility.getFullAXTree", params, timeout)?;
        let mut nodes = res.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default();
        if self.shared.is_single_target() {
            self.unmask_passwords(&mut nodes, timeout);
            self.live_checked(&mut nodes, timeout);
        }
        if opts.json {
            return Ok(flat_ax(&nodes, opts.all));
        }
        let frames = self.frame_trees(timeout);
        let root = nodes.iter().find(|n| n.get("parentId").is_none());
        let title = root.map(|n| ax_str(n, "name").to_string()).unwrap_or_default();
        let url = root
            .and_then(|n| n.get("properties").and_then(Value::as_array))
            .and_then(|props| props.iter().find(|p| p.get("name").and_then(Value::as_str) == Some("url")))
            .and_then(|p| p.get("value").and_then(|v| v.get("value")).and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let origin = url
            .find("://")
            .and_then(|i| url[i + 3..].find('/').map(|j| url[..i + 3 + j].to_string()))
            .unwrap_or_default();
        let root_backend_id = match &opts.scope {
            Some(target) => Some(self.backend_id(target, DEFAULT_ELEMENT_WAIT, timeout)?),
            None => None,
        };
        let rendered = ax::render(
            &format!("page {} {url}", serde_json::to_string(&title).unwrap_or_default()),
            &nodes,
            &frames,
            &ax::Options { all_refs: opts.all_refs, limit: opts.limit, root_backend_id, origin },
        );
        let mut text = rendered.text;
        // Lightpanda's tree stops at a shadow root; what is inside is read
        // from the page instead, without refs (see SHADOW_AX).
        let scope = match &opts.scope {
            None => Some(Value::Null),
            Some(Target::Selector(sel)) => Some(json!(sel)),
            Some(_) => None,
        };
        if let (true, Some(scope)) = (self.shared.is_single_target(), scope) {
            if let Ok(Value::Array(found)) = self.evaluate_raw(&format!("({SHADOW_AX})({scope})"), timeout) {
                let shadow = ax::render_shadow(&found);
                if !shadow.is_empty() {
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(shadow.trim_end());
                }
            }
        }
        Ok(Value::String(text))
    }

    /// Close this tab's target.
    pub fn close_target(&self, timeout: Duration) -> Result<()> {
        self.shared.unregister(&self.session_id);
        self.block_on(async {
            self.client.send("Target.closeTarget", json!({ "targetId": self.target_id }), None, timeout).await?;
            Ok(())
        })
    }
}

/// AX roles an agent can act on; always kept by the flat JSON view.
const AX_INTERACTIVE: &[&str] = ax::INTERACTIVE;

/// AX roles that carry no information of their own in the flat view.
const AX_STRUCTURAL: &[&str] = &["generic", "none", "presentation", "InlineTextBox", "LineBreak"];

/// The older flat JSON snapshot (`ax --format json`).
fn flat_ax(nodes: &[Value], all: bool) -> Value {
    let live = nodes.iter().filter(|n| !n.get("ignored").and_then(Value::as_bool).unwrap_or(false));
    if all {
        return Value::Array(
            live.map(|n| {
                json!({
                    "id": n.get("nodeId").and_then(Value::as_str).unwrap_or(""),
                    "role": ax_str(n, "role"),
                    "name": ax_str(n, "name"),
                    "backendNodeId": n.get("backendDOMNodeId").and_then(Value::as_u64).unwrap_or(0),
                })
            })
            .collect(),
        );
    }
    let names: std::collections::HashMap<&str, &str> =
        nodes.iter().filter_map(|n| Some((n.get("nodeId")?.as_str()?, ax_str(n, "name")))).collect();
    Value::Array(
        live.filter_map(|n| {
            let role = ax_str(n, "role");
            let name = ax_str(n, "name");
            let node_ref = n.get("backendDOMNodeId").and_then(Value::as_u64)?;
            if !AX_INTERACTIVE.contains(&role) {
                if name.is_empty() || AX_STRUCTURAL.contains(&role) {
                    return None;
                }
                let parent_name =
                    n.get("parentId").and_then(Value::as_str).and_then(|p| names.get(p).copied()).unwrap_or("");
                if role == "StaticText" && name == parent_name {
                    return None;
                }
            }
            let mut out = json!({ "ref": node_ref, "role": role, "name": name });
            let value = ax_str(n, "value");
            if !value.is_empty() {
                out["value"] = json!(value);
            }
            if role == "link" {
                if let Some(Value::String(url)) = ax::prop(n, "url") {
                    if !url.is_empty() && !url.starts_with("javascript:") {
                        out["url"] = json!(url);
                    }
                }
            }
            Some(out)
        })
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_chords_resolve() {
        assert_eq!(key_info("Enter").unwrap().3.as_deref(), Some("\r"));
        assert_eq!(key_info("esc").unwrap().0, "Escape");
        assert_eq!(key_info("a").unwrap(), ("a".into(), "KeyA".into(), 65, Some("a".into())));
        assert_eq!(key_info("F5").unwrap().2, 116);
        assert!(key_info("Bogus").is_none());
        assert_eq!(modifier_bit("Ctrl"), Some(2));
    }

    #[test]
    fn exception_messages_drop_stack_frames() {
        let details = json!({"text": "Uncaught", "exception": {"description": "Error: click: no element\n    at foo (x.js:1:1)\n    at bar"}});
        assert_eq!(exception_message(&details), "click: no element");
        // Other classes say something: they stay.
        let details = json!({"exception": {"description": "TypeError: x is undefined\n    at y"}});
        assert_eq!(exception_message(&details), "TypeError: x is undefined");
    }
}
