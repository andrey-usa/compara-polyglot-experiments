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

Status: **M1 in progress** (working name). The plan, the language and the
milestones are in agent-guidance `plans/prova.md`.

## The format

- Top-level lines: `base <url>`, `set NAME = value`, `use other.pv`,
  `flow name(a, b)`, `auth name [ttl 30m]`, `before all|each`, `after
  each|all`, `scenario "name" @tag…`. Steps are indented under them.
- A step is a navigera command line, written exactly as you would type it
  after `navigera -s work …`: `goto`, `click`, `fill`, `select`, `press`,
  `check`, `hover`, `scroll`, `upload`, `wait`, `eval`, `set`, `route`,
  `state load`, and the rest. Targets use navigera's locators (`--role`
  with `--name`, `--label`, `--text`, `--placeholder`, `--testid`,
  `--selector`, `--nth`). Refs from `ax` are refused: they change between
  runs.
- A relative path a step reads (`upload`, `state load`, `eval --file`,
  `init-script --file`, `route --body-file`) is found next to the `.pv`
  file, as `use` and `for each` files are, so a suite runs the same from
  any directory. Files a step writes (`screenshot`, `state save`) land in
  the current directory.
- Words split like a shell's, with no expansion. `${NAME}` and
  `${NAME:-default}` come from `save`, a flow's arguments, the data row,
  `before all`, `--var`, `set`, then the environment; `$$` (or `\$` in
  double quotes) is a literal `$`. Also `${run.id}` (one per run: unique
  emails), `${scenario.name}`, and `${download.path}`/`${download.name}`
  after `expect download`.
- `${secret:NAME}`: from the environment, else `--secrets <file>`
  (`NAME=value` lines). Its value is masked as `***` in everything the run
  prints and saves (messages, the page's URL and title, saved values,
  JUnit and JSON lines).
- A default with spaces is one word: quote it whole,
  `"${GREETING:-hello there}"` (words split before `${…}` is filled in).
- `save NAME = <command>` keeps a command's result (`get text …`).
- `flow name(a, b)` at the top level holds steps; `run name x y` runs them
  with `${a}` and `${b}` set. `use other.pv` takes another file's flows
  and `set`s. A failure inside a flow names the flow's own line.
- `optional <step>`: a step that may not apply (a cookie banner that only
  sometimes shows); its failure is noted, not fatal.
- `if <expect clause>` with steps indented under it runs them when the
  clause holds now (or within its `--timeout-ms`): `if --selector
  "#banner" visible`, then `click --text Accept` indented below.
- `auth shopper [ttl 30m]` holds steps that sign in; `as shopper` in a
  scenario starts it signed in. The block runs once per run, in a browser
  context of its own, the first time a scenario needs it (the other
  workers wait for it), and its cookies and storage are saved under
  `.primavera/auth/` (keep that out of git); with a `ttl` the saved state
  is reused by later runs until it is that old.
- `before all` runs once for the file, before its first scenario; what it
  saves every scenario of the file sees. If it fails, none of them run.
  `after all` runs once after the last one (a failure there is reported as
  `(after all)`).
- `handoff until <expect clause>`: the steps only a person can do (2FA, a
  CAPTCHA). When the clause already holds, nothing happens; else the
  browser comes on screen, the run waits for the clause (5 minutes, or its
  `--timeout-ms`) or Enter, and the browser goes back off screen. Plain
  `handoff` waits for Enter. With no screen it fails at once and says so.
- Data rows: an indented `examples` line with a table under it, or
  `scenario "…" for each logins.csv`, makes one scenario per row, named
  from the row (`scenario "Ship to ${country}"`); the row's columns are
  variables.
- `expect` retries until it holds or its timeout (5 s, `--timeout-ms` on the
  line or the run) runs out:

| expect | |
|---|---|
| `url`/`title` `is`, `contains`, `starts-with`, `ends-with`, `matches` `<value>` | the active tab |
| `text "<t>"`, `no text "<t>"` [`--within <target>`] | visible text, frames included, or inside an element |
| `<target> visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`, `editable`, `empty` | an element's state |
| `<target> text`, `value`, `attr <name>`, `style <property>` + a comparison | an element's text, value, attribute or computed style |
| `--selector <css> count` `is`, `>`, `>=`, `<`, `<=` `<n>` | how many match |
| `${var}` + a comparison | a saved value |
| `js "<expression>"` (or `eval`) [+ a comparison] | a script in the page: truthy, or its value compared |
| `request <METHOD> <url glob> [status <n>]`, `no request …` | a request the page made in this scenario (`*` in the URL matches anything) |
| `request … json <$.path>` + a comparison | a value in the last matching response's JSON (`$.items[0].sku`) |
| `request … count` + a comparison | how many matching requests |
| `cookie <name>` [+ a comparison], `no cookie <name>`, `cookie <name> absent` | a cookie of the scenario's browser context |
| `storage local\|session <key>` [+ a comparison \| `absent`] | Local or Session Storage |
| `download <file glob>`, `no download …` | a finished download (then `${download.path}`, `${download.name}`: the latest match; a second file of the same name arrives as `name (2).ext`) |
| `console` + a comparison, `no console "<t>"` | a console message (any level) |
| `no errors` | no page exceptions or console errors |
| `not …` | any of them, negated |

  `--soft` on any `expect` notes a failure and goes on; the scenario fails
  at its end with all of them.

## Running

```text
primavera run [<file.pv|dir>...] [--base-url <url>] [--var k=v] [--grep <text>] [--tag <tag>]
              [--workers <n|auto>] [--isolation context|browser] [--retries <n>] [--fail-on-flaky] [--shard <i/n>]
              [--repeat <n>] [--shuffle [<seed>]]
              [--reporter console|jsonl|github] [--junit <file>] [--json]
              [--timeout-ms <ms>] [--wait-ms <ms>] [--headed] [--chromium <path>]
              [--secrets <file>] [--pause-on-failure]
primavera lint [<file.pv|dir>...]
primavera list [<file.pv|dir>...]
```

- Every scenario starts in a fresh browser context (navigera `reset
  --all`: no cookies, storage, tabs, routes, init scripts, `set`
  emulation or dialog answer from the scenario before), in about 40 ms
  instead of a new browser's few hundred. `--workers n` runs n at once,
  each in a browser context of one shared browser (about 40% less memory
  at 4 workers than a browser each; `--isolation browser` gives each its
  own); the longest (from `.primavera/timings.json`) start first.
- `--retries n` runs a failing scenario again; one that passes then is
  reported FLAKY with how its first try failed. `--fail-on-flaky` fails
  the run on one. `--shard i/n` takes every n-th scenario, for CI matrix
  jobs.
- `--repeat 20 --shuffle` hunts flakes and order dependence: every
  scenario runs 20 times (`name #k`), in a random order whose seed is
  printed (`--shuffle <seed>` runs that order again), then a pass count per
  scenario, FLAKY where it isn't all of them.
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
- `--pause-on-failure` stops at a failure with the browser as the failure
  left it and prints how to look at that very tab (`navigera -s pv start
  --attach ws://…/devtools/page/…`, then `navigera -s pv ax`); Enter goes
  on with the run. It runs one worker. With no terminal to press Enter in,
  it waits `PRIMAVERA_PAUSE_S` seconds (default 600, for an agent looking
  from elsewhere), and under CI (`CI` set) it doesn't pause unless that is
  set.
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
