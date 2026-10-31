#!/bin/bash
# navigera's `drag` of Gift onto Internet on Actual's mobile budget, as the
# batch C scenario does it, with the page's drag event log.
set -u
NAV=${NAV:-navigera}; S=diag-$1; shift
n() { "$NAV" -s "$S" "$@"; }
LOGGER=$(cat "$(dirname "$0")/logger.js")
NOANIM="(()=>{const css='*,*::before,*::after{transition-duration:0s !important;transition-delay:0s !important;animation-duration:0s !important;animation-delay:0s !important}';const f=()=>{const s=document.createElement('style');s.textContent=css;document.head.appendChild(s)};document.head?f():document.addEventListener('DOMContentLoaded',f,{once:true})})()"
n start "$@" >/dev/null
n set ua playwright >/dev/null
n set viewport 350 600 --mobile >/dev/null
n init-script "$NOANIM" >/dev/null
n goto http://127.0.0.1:3001/ >/dev/null
n click --role button --name "Try the demo" >/dev/null
n wait --selector "[data-testid=budget-table]" --timeout-ms 60000 >/dev/null
n set viewport 350 1000 >/dev/null
n eval "[...document.querySelectorAll('[data-navbar-state] a')].find(a=>a.textContent.trim()==='Budget').click()" >/dev/null
n wait --selector "[data-testid=budget-table]" --timeout-ms 60000 >/dev/null
n eval "(()=>{const rows=[...document.querySelectorAll(\"[data-testid='category-row']\")]; const tag=(name,t)=>{const r=rows.find(x=>{const m=x.querySelector(\"[data-testid='category-name']\"); return m&&m.textContent.trim()===name}); r.setAttribute('data-pv-target',t)}; tag('Gift','gift-row'); tag('Internet','internet-row'); return 'ok'})()" >/dev/null
n eval "$LOGGER" >/dev/null
echo "before: $(n --raw eval "window.__rows('Bills')")"
echo "drag: $(n drag "[data-pv-target='gift-row']" "[data-pv-target='internet-row']" ${DRAG_ARGS:-})"
sleep 2
echo "dump: $(n --raw eval "window.__dump()")"
n quit >/dev/null
