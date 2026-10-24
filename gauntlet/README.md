# Flake gauntlet

One local app built to break UI tests. Each page is a timing or DOM trap
that a naive scripted test trips on: content that renders late, a button
that remounts every 1.5 s, a toast that covers the target, handlers that
bind after the modal shows, a slow XHR, a lazy list, a client-side
redirect chain, nested and cross-origin iframes, open shadow roots,
duplicate ids, a non-breaking space in a label, a multi-select, and a
real file download.

Stdlib Python only. Two origins on 127.0.0.1 (different ports) so the
cross-origin iframe page works offline.

```sh
python3 server.py --port 18201 --seed 7 --jitter 0 --telemetry 1
# LISTENING http://127.0.0.1:18201  XO http://127.0.0.1:18202
```

- `--seed`: every timing (late-render delay, XHR latency, toast duration,
  remount period, handler bind delay) comes from one seeded rng, so runs
  are reproducible for a given seed.
- `--jitter <ms>`: adds up to that much extra random latency to the
  slow/dynamic endpoints, from the same rng stream.
- `--xo-port`: the port of the second (cross-origin) origin.

## Pages

| Page | What it breaks |
|---|---|
| `late-render` | Content renders after a seeded 1–8 s delay |
| `remount` | Button destroyed/recreated (React key change) every 1.5 s |
| `transition-transform` | Target slides 300 px over 1.2 s on load |
| `toast-cover` | Toast covers the button for 2 s |
| `modal-late-handlers` | Modal handlers bind 1.5 s after show |
| `slow-xhr` | Seeded 2–4 s XHR; stale reads fail |
| `lazy-list` | 20-item chunks on scroll; target is item 45 |
| `/redir/1` | Client-side redirect chain 1→2→3→final with a chain token |
| `nested-iframes` | Same-origin nested iframes; commit must come from depth 2 |
| `xo-iframe` | Cross-origin iframe (second port) with a payment form |
| `shadow-open-named` | Open shadow root, button has an accessible name |
| `shadow-open-unnamed` | Open shadow root, icon-only button, no accessible name |
| `duplicate-ids` | Two buttons share `id="dup"`; the first is a hidden decoy |
| `nbsp-label` | Option label contains a non-breaking space |
| `select-multiple` | `<select multiple>`; all three options in one commit |
| `download` | `report.csv` download; the server counts a real request |

Each page carries its intended user path in an HTML comment at the top.

## Verdicts

Every page POSTs what it saw to `POST /event/<page>` (the page's own
ground truth). `GET /verdict/<page>` returns JSON
`{legitimate, reason, events}`: whether the actions the page saw were a
legitimate user path or a cheat (for example, a click that reached a
covered element). `GET /reset` clears all events. `GET /` lists the pages.

## Run protocol

1. Start the server with a fixed seed; record the seed.
2. `GET /reset`.
3. Run the scenario for the page.
4. `GET /verdict/<page>` → `{legitimate, reason, events}`.
5. `GET /reset` before the next page.

The `.pv` scenarios in this directory (`g01`–`g16`) check the same pages;
the run is `primavera run gauntlet/ --repeat 20 --shuffle --reporter github`
with `GAUNTLET_URL` pointing at the server (the XO origin is served from
the same server on its second port).
