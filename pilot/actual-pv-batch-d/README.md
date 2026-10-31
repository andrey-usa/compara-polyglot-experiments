# actual-pv-batch-d

Actual Budget's Playwright e2e mobile specs converted to primavera `.pv`
files: transactions.mobile (7), schedules.mobile (5), payees.mobile (5),
rules.mobile (4), accounts.mobile (3), bank-sync.mobile (3),
budget-automations.mobile (1), reports.mobile (1), settings.mobile (1)
— 30 tests in total.

The app is actualbudget/actual @ dff6b4b21bed7d20a0977cd71c990f40b6e26e8e,
built and served exactly like the Playwright baseline (`yarn build:browser
--skip-translations`, `serve-build.mjs`). primavera and navigera are built
from source at pinned commits (see the workflow): primavera next
2ee2589, navigera next 575564e — the pilot pins, kept deliberately even
though primavera `next` has since moved. Per-test results land in
`results/junit.xml`; failure evidence in `results/evidence/`.

Files:

- `pv/transactions.mobile.pv` - 7 scenarios, one per upstream test, in
  spec order. Scenario names are the exact upstream names.
- `pv/schedules.mobile.pv` - 5 scenarios.
- `pv/payees.mobile.pv` - 5 scenarios.
- `pv/rules.mobile.pv` - 4 scenarios.
- `pv/accounts.mobile.pv` - 3 scenarios.
- `pv/bank-sync.mobile.pv` - 3 scenarios.
- `pv/budget-automations.mobile.pv` - 1 scenario.
- `pv/reports.mobile.pv` - 1 scenario.
- `pv/settings.mobile.pv` - 1 scenario.

All files are self-contained: the shared setup (demo file, navbar
navigation) lives in flows at the top of each file.

Mobile emulation mapping (upstream Playwright config has no device
descriptor): config-wide userAgent `playwright` -> `set ua "playwright"`;
each spec's `setViewportSize(350, 600)` -> `set viewport 350 600 --mobile`.
The app's narrow layout keys off width < 512 only; `--mobile`
additionally enables touch emulation. Exception: reports.mobile starts
at the Playwright default 1280x720 (its "Add new widget" step is
desktop-only) and narrows to 350x600 --mobile only for its own narrow
step (W20).

Screenshot steps are in-environment repeatability checks only (the
baseline runs with VRT unset, so upstream's `toMatchThemeScreenshots` is
a no-op there): baselines are captured in the run's own environment
first (`--update-screenshots` pass) and then verified by the main pass;
they are not committed, because fonts and Chromium builds differ between
machines.

Workarounds are labeled in each file header (pilot W1-W3, batch C
W4/W8/W12 where reused, batch D W14-W24). calculator.mobile is not
ported: it needs a fake clock and raw touch, so it stays on Playwright
and is listed as a gap in the batch report.
