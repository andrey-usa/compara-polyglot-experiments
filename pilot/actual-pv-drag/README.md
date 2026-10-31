# actual-pv-drag

The two "moves a category to another group with drag and drop" tests from
Actual Budget's Playwright e2e spec `budget.mobile.test.ts`, rerun against
navigera with the real-drag fix (batch C D5, left open by batch C).

Batch C drove these with raw mouse move/down/move/up, which cannot initiate
an HTML5 drag in the headless shell. Here the mouse sequence is replaced by
navigera's native `drag` (real browser drag via `Input.setInterceptDrags` +
`Input.dispatchDragEvent`); the result JSON (`path`, `dropped`) is saved per
try as `dr` and printed by the workflow as `::notice` lines.

Pins: navigera `next` c660a00, Actual dff6b4b, primavera `next` 7ea6551.
The app is built and served exactly like the Playwright baseline
(`yarn build:browser --skip-translations`, `serve-build.mjs`).

Files:

- `pv/drag-mobile.pv` - the two drag scenarios (Envelope, Tracking), each
  run 3 times (`--repeat 3`). Setup flows and the final assertion are copied
  unchanged from batch C (`experiment-53 @ 8aaeee7`,
  `pilot/actual-pv-batch-c/pv/budget-mobile.pv`).

Known difference from upstream: navigera `drag` aims at the target element's
center, while upstream drops on the Internet row's top edge (x+20, y+5).
