# actual-pv-pilot

Actual Budget's Playwright e2e specs converted to primavera `.pv` files - a
three-spec pilot: `settings` (route stubs, downloads, visual snapshots),
`multitab` (tabs, visibility, SharedWorker coordinator) and `help-menu`
(visual snapshots across themes).

The app is actualbudget/actual @ dff6b4b21bed7d20a0977cd71c990f40b6e26e8e,
built and served exactly like the Playwright baseline (`yarn build:browser
--skip-translations`, `serve-build.mjs`). primavera and navigera are built
from source at pinned commits (see the workflow). Per-test results land in
`results/junit.xml`; failure evidence in `results/evidence/`.

Files:

- `pv/settings.pv` - 4 scenarios, ports `settings.test.ts`
- `pv/multitab.pv` - 3 scenarios, ports the "Multiple tabs on one budget"
  describe of `multitab.test.ts`; two scenarios carry documented deviations
  (labeled in the file header)
- `pv/multitab-refresh.pv` - 2 scenarios, literal ports of the "Refreshing a
  tab" describe
- `pv/help-menu.pv` - 2 scenarios, literal port of `help-menu.test.ts`
  (12 themed screenshot comparisons)
- `pv/multitab-literal.pv` - the workaround-free literal port of the leader-
  takeover test; kept because it currently FAILS (the promoted follower
  renders an empty transaction table until a full reload) and serves as the
  defect repro. The workflow runs it as an expected-failure and keeps the log.

Screenshot baselines are captured in the run's own environment first
(`--update-screenshots` pass) and then verified by the main pass; they are
not committed, because fonts and Chromium builds differ between machines.
