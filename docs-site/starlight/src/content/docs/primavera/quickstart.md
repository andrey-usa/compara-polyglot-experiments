---
title: Quick start primavera
description: "One copy-paste path from a built binary to a first passing scenario. Run in CI on every push."
---

# Quick start

One path, start to finish. It builds primavera (with navigera beside
it, as [Install](/primavera/install/) describes), writes one scenario
against a public page, and runs it. This exact sequence is run in CI on
every push to the site branch, on the runner's Chrome. Note the
navigera branch: primavera's CI builds against `session-history`, and
navigera `master` does not currently compile against primavera `next`
(see [Install](/primavera/install/) for the details) — so the quick
start pins `session-history` deliberately.

```sh
mkdir work && cd work
git clone --branch session-history https://github.com/andrey-usa/navigera.git
git clone --branch next https://github.com/andrey-usa/primavera.git
cd primavera && cargo build --release && cd ..
mkdir -p tests
cat > tests/smoke.pv << 'EOF'
base https://example.com

scenario "The example page loads"
  goto /
  expect title is "Example Domain"
EOF
./primavera/target/release/primavera run tests
```

What you should see: one scenario passing, in under a few seconds, with
a line like `PASS tests/smoke.pv:3 The example page loads`. Add
`--html report.html` to get the run as one page that needs nothing else
to open. A failing scenario prints the step, what was expected and
seen, where the page was, and a screenshot under `.primavera/runs/`.

Why the title, not the text: example.com's visible text changed in
2026 — its "Example Domain" heading is gone (the page is now
paragraphs in several languages, and it asks not to be relied on for
testing). The title is still "Example Domain", so the title check is
the stable expectation; an earlier draft of this page used
`expect text "Example Domain"` and failed on the live page for exactly
that reason.

Next: the [agent skill](/primavera/skill/) is the guide to hand an
agent that writes tests, and [Reference](/primavera/cli-reference/) lists
every command and flag.
