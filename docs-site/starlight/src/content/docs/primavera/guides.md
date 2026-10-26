---
title: Guides primavera
description: "Task-oriented guides for primavera — write a scenario, run a subset, read the report, run in CI."
---

# Guides

## Write a scenario

A scenario is plain text: a `base` URL, optional `before each` steps,
then `scenario "name"` with steps. Steps are navigera commands in
plain words — `goto`, `click`, `expect` — with selectors by text,
`--selector`, or the other locators navigera accepts. Tags (`@smoke`)
and `--grep`/`--tag` select subsets; `--var k=v` fills variables.

```text
base http://127.0.0.1:8000

before each
  goto /shop
  click --selector "#cookie-accept"

scenario "Adding a product counts it in the cart" @smoke
  click --text "Add to cart"
  expect --selector "#cart-count" text is 1
  goto /cart
  expect text "Your cart"
```

## Run a subset, retry, repeat

```sh
primavera run tests --tag smoke            # only @smoke scenarios
primavera run tests --grep "cart"          # scenarios whose name matches
primavera run tests --workers auto --retries 1
primavera run tests --repeat 20 --shuffle  # hunt flakes and order dependence
```

`--retries n` re-runs a failing scenario; one that passes then is
reported FLAKY with how its first try failed, and `--fail-on-flaky`
fails the run on one. Every scenario starts in a fresh browser context
in about 40 ms instead of a new browser's few hundred.

## Read the report

```sh
primavera run tests --html report.html --junit report.xml
```

`--html` writes the run as one page that needs nothing else to open:
failed scenarios first and open, each with its steps and times, the
failure (message, where the page was, what came closest, the note) and
its screenshot in the page. `--junit` writes the same run for CI.
`--trace on-failure` records each step of a failing scenario, and
`primavera trace <dir>` prints it.

## Run in CI

Use the action (see [Install](/primavera/install/)): it runs on the
runner's Chrome, annotates the failing lines, and uploads the evidence
directory. `--engine lightpanda` is a smoke tier, not a replacement:
hover-revealed elements, new tabs, screenshots and `ax` pixels need
Chrome.

## Generate a test from a session

`primavera from-session` builds a test from a navigera session's
history, and `primavera record <url>` records one interactively. Both
write a `.pv` file you then edit by hand — the generated steps are a
starting point, not a finished test. See [Reference](/primavera/cli-reference/)
for every flag.
