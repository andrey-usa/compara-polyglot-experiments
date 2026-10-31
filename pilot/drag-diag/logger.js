// Drag event log for Actual's mobile budget: every pointer/mouse/drag event
// the page sees, with the row and list under it, the drag data types and
// effects, and whether the page's handlers accepted it (defaultPrevented,
// read after React's root listener ran).
(() => {
  const t0 = performance.now();
  const log = (window.__dl = []);
  const seen = new WeakMap();
  const label = (el) => {
    if (!el || !el.closest) return null;
    const row = el.closest('[data-testid=category-row]');
    const name = row && row.querySelector('[data-testid=category-name]');
    if (name) return 'row:' + name.textContent.trim();
    const grp = el.closest('[data-testid=category-group-row]');
    const gname = grp && grp.querySelector('[data-testid=category-group-name]');
    if (gname) return 'group:' + gname.textContent.trim();
    if (el.closest('.react-aria-DropIndicator,[data-drop-target]')) return 'drop-indicator';
    return el.tagName + (el.getAttribute('data-testid') ? '#' + el.getAttribute('data-testid') : '');
  };
  const grid = (el) => {
    const g = el && el.closest && el.closest('[role=grid]');
    return g ? (g.getAttribute('aria-label') || '').replace(' expense group categories', '') : null;
  };
  const types = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'dragstart', 'drag', 'dragenter', 'dragover', 'dragleave', 'drop', 'dragend'];
  for (const type of types) {
    window.addEventListener(type, (e) => {
      if (type === 'drag') return;
      const dt = e.dataTransfer;
      const entry = {
        t: Math.round(performance.now() - t0), type, x: Math.round(e.clientX), y: Math.round(e.clientY),
        on: label(e.target), grid: grid(e.target),
        types: dt ? [...dt.types].join(',') : undefined,
        effect: dt ? dt.effectAllowed + '/' + dt.dropEffect : undefined,
        draggable: e.target && e.target.closest ? !!e.target.closest('[draggable=true]') : undefined,
        prevented: false, bubbled: false,
      };
      seen.set(e, entry);
      log.push(entry);
    }, true);
    window.addEventListener(type, (e) => {
      const entry = seen.get(e);
      if (entry) { entry.prevented = e.defaultPrevented; entry.bubbled = true; }
    }, false);
  }
  window.__rows = (g) => {
    const el = [...document.querySelectorAll('[role=grid]')].find((x) => (x.getAttribute('aria-label') || '') === g + ' expense group categories');
    return el ? [...el.querySelectorAll('[data-testid=category-name]')].map((n) => n.textContent.trim()).join('|') : null;
  };
  window.__dump = () => {
    // Runs of dragover on the same row and list are folded into one line.
    const out = [];
    for (const e of window.__dl) {
      const last = out[out.length - 1];
      if (last && e.type === 'dragover' && last.type === 'dragover' && last.on === e.on && last.grid === e.grid && last.prevented === e.prevented) {
        last.n = (last.n || 1) + 1; last.lastXY = e.x + ',' + e.y; continue;
      }
      out.push({ ...e });
    }
    return JSON.stringify({ events: out, bills: window.__rows('Bills'), usual: window.__rows('Usual Expenses') });
  };
  return 'logger on';
})()
