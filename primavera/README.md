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

Status: **M0 spike** (working name).

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
| `no errors` | no page exceptions or console errors |
| `not …` | any of them, negated |

## Running

```text
primavera run [<file.pv|dir>...] [--base-url <url>] [--var k=v] [--grep <text>] [--tag <tag>]
              [--junit <file>] [--timeout-ms <ms>] [--wait-ms <ms>] [--headed] [--chromium <path>]
primavera lint [<file.pv|dir>...]
primavera list [<file.pv|dir>...]
```

- One browser per run; every scenario starts in a fresh browser context
  (navigera `reset`: no cookies, storage or tabs from the scenario before),
  in about 40 ms instead of a new browser's few hundred.
- A failure prints the step, what was expected and seen, where the page
  was, a screenshot (under `.primavera/runs/`), and a note when an earlier
  click changed nothing on the page (navigera's `no_effect`), the usual
  reason the next check fails.
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
