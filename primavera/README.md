# primavera

Plain-text web tests that run on real Chrome. One native binary drives the
browser in-process through [navigera](https://github.com/andrey-usa/navigera):
no Node, no transpile step, no driver process, and no model at run time.

```text
# tests/shop.pv
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

```console
$ primavera run tests --junit report.xml
PASS  tests/shop.pv:7  Adding a product counts it in the cart  (557 ms)
1 passed, 0 failed in 0.9 s
```

Status: **M1 in progress** (working name).

## The format

- Top-level lines: `base <url>`, `set NAME = value`, `before each`,
  `after each`, `scenario "name" @tag…`. Steps are indented under them.
- A step is a navigera command line, written exactly as you would type it
  after `navigera -s work …`: `goto`, `click`, `fill`, `select`, `press`,
  `check`, `hover`, `scroll`, `upload`, `wait`, `eval`, `set`, `route`,
  `state load`, and the rest. Targets use navigera's locators (`--role`
  with `--name`, `--label`, `--text`, `--placeholder`, `--testid`,
  `--selector`, `--nth`). Refs from `ax` are refused: they change between
  runs.
- Words split like a shell's, with no expansion. `${NAME}` and
  `${NAME:-default}` come from `save`, `--var`, `set`, then the
  environment; `$$` (or `\$` in double quotes) is a literal `$`.
- `save NAME = <command>` keeps a command's result (`get text …`).
- `flow name(a, b)` at the top level holds steps; `run name x y` runs them
  with `${a}` and `${b}` set. `use other.pv` takes another file's flows
  and `set`s. A failure inside a flow names the flow's own line.
- `optional <step>`: a step that may not apply (a cookie banner that only
  sometimes shows); its failure is noted, not fatal.
- Data rows: an indented `examples` line with a table under it, or
  `scenario "…" for each logins.csv`, makes one scenario per row, named
  from the row (`scenario "Ship to ${country}"`); the row's columns are
  variables.
- `expect` retries until it holds or its timeout (5 s, `--timeout-ms` on the
  line or the run) runs out:

| expect | |
|---|---|
| `url`/`title` `is`, `contains`, `starts-with`, `ends-with`, `matches` `<value>` | the active tab |
| `text "<t>"`, `no text "<t>"` | visible text, frames included |
| `<target> visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`, `editable` | an element's state |
| `<target> text`, `value`, `attr <name>` + a comparison | an element's text, value or attribute |
| `--selector <css> count` `is`, `>`, `>=`, `<`, `<=` `<n>` | how many match |
| `${var}` + a comparison | a saved value |
| `js "<expression>"` [+ a comparison] | a script in the page: truthy, or its value compared |
| `request <METHOD> <url glob> [status <n>]`, `no request …` | a request the page made in this scenario (`*` in the URL matches anything) |
| `cookie <name>` [+ a comparison], `no cookie <name>` | a cookie of the scenario's browser context |
| `storage local\|session <key>` [+ a comparison] | Local or Session Storage |
| `no errors` | no page exceptions or console errors |
| `not …` | any of them, negated |

## Running

```text
primavera run [<file.pv|dir>...] [--base-url <url>] [--var k=v] [--grep <text>] [--tag <tag>]
              [--workers <n|auto>] [--retries <n>] [--fail-on-flaky] [--shard <i/n>]
              [--reporter console|jsonl|github] [--junit <file>] [--json]
              [--timeout-ms <ms>] [--wait-ms <ms>] [--headed] [--chromium <path>]
primavera lint [<file.pv|dir>...]
primavera list [<file.pv|dir>...]
```

- Every scenario starts in a fresh browser context (navigera `reset`: no
  cookies, storage or tabs from the scenario before), in about 40 ms
  instead of a new browser's few hundred. `--workers n` runs n at once, a
  browser each; the longest (from `.primavera/timings.json`) start first.
- `--retries n` runs a failing scenario again; one that passes then is
  reported FLAKY with how its first try failed. `--fail-on-flaky` fails
  the run on one. `--shard i/n` takes every n-th scenario, for CI matrix
  jobs.
- `--reporter jsonl` prints one JSON object per scenario as it finishes
  (the console moves to stderr); `github` writes workflow annotations on
  the failing lines (on by itself under GitHub Actions). `--json` prints
  what the scenarios saved, for a pipeline's next step.
- A failure prints the step, what was expected and seen, where the page
  was, a screenshot (under `.primavera/runs/`), and a note when an earlier
  click changed nothing on the page (navigera's `no_effect`), the usual
  reason the next check fails. When the element isn't there, it lists the
  closest ones on the page as locators to paste in:

  ```text
  FAIL  shop.pv:21  Accept the cookies  (611 ms)
        line 23: click --role button --name "Accept everything"
        click role="button" name="Accept everything": … no element …
        nearest on the page:
          1. button "Accept all"   --role button --name "Accept all"   0.63
  ```
- Exit codes: 0 passed, 1 a scenario failed, 2 a file or argument is wrong,
  3 the browser didn't start.

## Building

primavera links navigera as a library, checked out beside it:

```text
work/
  navigera/     github.com/andrey-usa/navigera (or navigera-private)
  primavera/    this repository
```

```console
$ cd primavera && cargo build --release
$ ./target/release/primavera run examples --base-url http://127.0.0.1:<port>
```

`examples/bench-site` runs against navigera's bench site
(`python3 ../navigera/bench/site/server.py --port 0`).

## License

MIT
