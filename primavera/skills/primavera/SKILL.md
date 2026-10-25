---
name: primavera
description: Write and run plain-text web tests (.pv files) on real Chrome with primavera — each step is a navigera command line, each check an `expect` that retries. Use to turn what you did in a navigera session into a regression test, to check a web app after a change, or to automate a browser job, without writing Playwright code.
---

# primavera

One native binary runs `.pv` scenario files on Chrome through navigera,
in-process. Every step is a line you could run as `navigera -s work <step>`,
so explore with navigera first, then paste the steps that worked.

## A file

```text
base http://127.0.0.1:8000
set email = demo@acme.test

before each
  goto /shop
  optional click --selector "#cookie-accept" --timeout-ms 500

scenario "Adding a product counts it in the cart" @smoke
  click --text "Add to cart"
  expect --selector "#cart-count" text is 1
  goto /cart
  expect text "Your cart"
```

- Top level: `base`, `set NAME = value`, `use other.pv`, `flow name(a, b)`,
  `auth name [ttl 30m]`, `before all|each`, `after each|all`,
  `scenario "name" @tags`. Steps are indented.
- Steps: any navigera command (`goto`, `click`, `fill`, `select`, `press`,
  `check`, `hover`, `scroll`, `upload`, `wait`, `eval`, `dialog`, `set`,
  `route`, `state load`, `tab-select`, …). Use locators, never refs:
  `--role button --name "Pay"`, `--label Email`, `--text "Sign in"`,
  `--testid total`, `--placeholder`, `--selector <css>`, `--nth`.
  Frames and open shadow roots are searched for you.
- Relative files a step reads (`upload --file`, `state load`, `eval
  --file`, `route --body-file`) are found next to the `.pv` file.
- `save NAME = <command>` keeps a result (`save id = get text --testid order`).
- `run name x y` runs a `flow`; `optional <step>` may fail without failing;
  `if <expect clause>` runs the steps indented under it when it holds now.
- `as shopper` starts signed in: the `auth shopper` block's steps run once
  per run (reused for its `ttl` across runs), their cookies and storage
  loaded here. `before all` saves values every scenario of the file sees.
- `handoff until <expect clause>`: a person does what only a person can
  (2FA); skipped when the clause already holds.
- Data rows: `examples` plus a `| a | b |` table under the scenario, or
  `scenario "…" for each rows.csv`; columns become `${a}`.
- Words split like a shell's, no expansion. `${NAME}`, `${NAME:-default}`
  (from save, a flow's args, the row, `before all`, `--var`, `set`, the
  environment), `${run.id}`, `${scenario.name}`, `${secret:NAME}` (env or
  `--secrets file`; masked in output). A literal `$`: `\$` in double
  quotes, or single quotes.

## expect (retried until it holds, 5 s by default)

```text
expect url ends-with /cart                 # is contains starts-with ends-with matches
expect title contains Checkout
expect text "Thank you"                    # visible text, frames included
expect text Saved --within --testid toast
expect no text "Error"
expect --role button --name Pay enabled    # visible hidden enabled disabled checked unchecked focused editable empty
expect --testid total text is "$36.00"     # text, value, attr <name>, style <prop> + a comparison
expect --selector .card count >= 3
expect ${order} matches "^ORD-[0-9]+$"
expect js "document.forms.length" >= 1     # or truthy without a comparison
expect request POST /api/orders status 201
expect request POST /api/orders json $.items[0].sku is sku-1
expect request GET /api/items* count >= 2
expect cookie session                      # present; or compared; `no cookie x`, `cookie x absent`
expect storage local cart contains sku-1
expect download report-*.csv               # then ${download.path}
expect console contains ready
expect no errors
expect not --selector .toast visible       # any of them negated
expect text "Saved" --timeout-ms 10000
expect text "Saved" --soft                 # noted; the scenario fails at its end
```

## Run

```bash
primavera lint tests/                       # milliseconds, no browser
primavera run tests/ --base-url http://127.0.0.1:8000
primavera run tests/ --workers 4 --retries 1 --junit report.xml
primavera run tests/ --html report.html   # one page: failures first, steps, times, screenshots
primavera run tests/ --reporter jsonl       # one JSON object per scenario (console to stderr)
primavera run tests/ --repeat 20 --shuffle  # flake hunt: pass counts per scenario
primavera run job.pv --var month=2026-09 --json   # print what was saved
primavera run tests/ --secrets .env.test   # NAME=value lines for ${secret:NAME}
primavera run tests/x.pv --pause-on-failure  # keeps the failing tab; prints how to attach
```

Exit codes: 0 passed, 1 failed, 2 a file or argument is wrong, 3 the browser
didn't start.

## When it fails

The output names the line, what was expected and what was seen, the page,
a screenshot under `.primavera/runs/`, the closest elements on the page as
locators (paste one in), and whether the click before changed nothing on
the page (the page may not have been ready: add an `expect` for what shows
it is ready, before the click). Open the page with navigera to look:
`navigera -s look start && navigera -s look goto <url> && navigera -s look ax`.
