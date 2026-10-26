// Site-level accessibility fixes for Starlight's own chrome, so the
// built site passes axe with 0 critical and 0 serious:
//  - Scrollable <pre> code blocks and <table>s get tabindex="0": a
//    scrollable region must be keyboard-focusable (axe
//    scrollable-region-focusable). Starlight/expressive-code leaves
//    some of them without one.
//  - The search button's <kbd> shortcut hint is removed: it is
//    decorative (the shortcut still works, and is exposed via
//    aria-keyshortcuts), and its visible text otherwise breaks the
//    button's accessible name on every page (axe
//    label-content-name-mismatch: visible "Search Ctrl K" against
//    accessible name "Search"). Removing it, rather than hiding it,
//    keeps the fix platform-independent: the hint text differs per
//    platform (Ctrl vs Cmd), so no static accessible name can match it.
(function () {
  document.querySelectorAll('pre, table').forEach(function (el) {
    if (el.scrollWidth > el.clientWidth && !el.hasAttribute('tabindex')) {
      el.setAttribute('tabindex', '0');
    }
  });
  document.querySelectorAll('button[data-open-modal] kbd').forEach(function (el) {
    el.remove();
  });
})();
