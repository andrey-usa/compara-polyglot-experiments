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

Status: pre-release. Everything below works in this build.

## The format

- Top-level lines: `base <url>`, `set NAME = value`, `use other.pv`,
  `flow name(a, b)`, `step "<phrase>"` (a Gherkin step's steps, see
  below), `auth name [ttl 30m]`, `before all|each`, `after each|all`,
  `scenario "name" @tag…`. Steps are indented under them.
- A step is a navigera command without the `navigera -s <session>`
  prefix (`goto /cart`, `click --text Add`): `goto`, `click`, `fill`, `select`, `press`,
  `check`, `hover`, `scroll`, `upload`, `wait`, `eval`, `set`, `route`,
  `state load`, and the rest. Targets use navigera's locators (`--role`
  with `--name`, `--label`, `--text`, `--placeholder`, `--testid`,
  `--selector`, `--near <text>` for the one in the card or row that shows
  the text, `--nth`). Refs from `ax` (`click 12`) are an error: they
  change between runs. `check --label Terms` ticks a box; `check
  "<question>"` with no locator asks navigera a yes/no question about
  the page (a model call).
- A relative path a step reads (`upload`, `state load`, `eval --file`,
  `init-script --file`, `route --body-file`) is found next to the `.pv`
  file, as `use` and `for each` files are, so a suite runs the same from
  any directory. Files a step writes (`screenshot`, `state save`) land in
  the current directory.
- Words split like a shell's, with no expansion. `${NAME}` and
  `${NAME:-default}` come from `save`, a flow's arguments, the data row,
  `before all`, `--var`, `set`, then the environment; `$$` (or `\$`) is a
  literal `$`, and quotes don't stop `${…}` (`'${x}'` is filled in too). Also `${run.id}` (one per run: unique
  emails), `${scenario.name}`, and `${download.path}`/`${download.name}`
  after `expect download`.
- `${secret:NAME}`: from the environment, else `--secrets <file>`
  (`NAME=value` lines; the environment wins). Its value is masked as `***` in everything the run
  prints and saves (messages, the page's URL and title, saved values,
  JUnit and JSON lines).
- A default with spaces is one word: quote it whole,
  `"${GREETING:-hello there}"` (words split before `${…}` is filled in).
- A `set` value can hold `${…}` too (a default, a `--var`, another `set`,
  a secret, the environment): it is filled in once, when a scenario starts.
- `save NAME = <command>` keeps a command's result (`get text …`).
- `exec [--timeout-ms <ms>] <program> <args…>` runs a program (no shell:
  `sh -c "…"` or `pwsh -c "…"` for one) in the `.pv` file's directory,
  with `PRIMAVERA_BASE_URL` set, for what a browser can't reach: seed a
  database (`psql`, `sqlplus`), call a script. Its exit code and output
  become `${exec.exit}`, `${exec.stdout}` and `${exec.stderr}`, and
  `expect exit 0`, `expect stdout contains SHIPPED`, `expect stderr …`
  check them (the first 16 MB of each). A nonzero exit fails the step,
  with the last line of its stderr, unless a step indented under it checks
  `exit`. It is stopped after 60 s (`--timeout-ms`), and that fails the
  step whatever is under it.

  ```text
  exec ./scripts/order-status.sh ${order}
    expect exit 0
    expect stdout contains SHIPPED
  ```
- `http <METHOD> <url> [--json <text> | --body <text> | --json-file <f> |
  --body-file <f>] [--header "Name: value"]… [--no-follow] [--insecure]
  [--timeout-ms <ms>]` makes a
  request outside the page (an API to set up or check what the UI did; a
  relative URL takes the base URL). The scenario's cookies go with it and
  the cookies it sets come back to the browser, so an API sign-in signs
  the page in. Its answer is `${http.status}` and `${http.body}`; `expect
  status 201`, `expect json $.role is shopper`, `expect header Location
  ends-with /u/1`, `expect body contains …` check it, and `save id = json
  $.id` (or `header <name>`) keeps a value. A 4xx or 5xx fails the step,
  with the start of the body, unless a step indented under it checks
  `status`; a timeout fails it whatever is under it. Redirects are followed
  (up to 20, a cookie set on the way going to the next hop and to the
  browser); `--no-follow` gives the 3xx itself. `--insecure` accepts a
  certificate nothing vouches for (a local server's own). It goes through `curl` (on every supported OS), its options on
  curl's stdin, so a secret in a header or body stays out of the process
  list.

  ```text
  http POST /api/users --json '{"email": "${email}", "role": "shopper"}'
    expect status 201
    expect json $.role is shopper
  save user = json $.id
  ```
- `flow name(a, b)` at the top level holds steps; `run name x y` runs them
  with `${a}` and `${b}` set. `use other.pv` takes another file's flows
  and `set`s. A failure inside a flow names the flow's own line.
- `optional <step>`: a step that may not apply (a cookie banner that only
  sometimes shows); its failure is noted, not fatal.
- `if <expect clause>` with steps indented under it runs them when the
  clause holds now (or within its `--timeout-ms`): `if --selector
  "#banner" visible`, then `click --text Accept` indented below.
- `auth shopper [ttl 30m]` (`s`, `m`, `h`, `d`, or plain seconds) holds steps that sign in; `as shopper` in a
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
  line or the run) runs out; `exit`, `stdout`, `stderr`, `status`, `body`,
  `json` and `header` read the last `exec` or `http` answer once. `is`
  compares numbers as numbers (`1` is `1.0`). A glob (`request`,
  `download`) has `*` for any run of characters and matches the whole URL
  or file name:

| expect | |
|---|---|
| `url`/`title` `is` (or `==`), `contains`, `starts-with`, `ends-with`, `matches` `<value>` | the active tab |
| `text "<t>"`, `no text "<t>"` [`--within <target>`] | visible text, frames included, or inside an element |
| `<target> visible`, `hidden`, `enabled`, `disabled`, `checked`, `unchecked`, `focused`, `editable`, `empty` | an element's state |
| `<target> text`, `value`, `attr <name>`, `style <property>` + a comparison | an element's text, value, attribute or computed style |
| `--selector <css> count` `is`, `>`, `>=`, `<`, `<=` `<n>` | how many match (exactly one `--selector`) |
| `${var}` + a comparison | a saved value |
| `js "<expression>"` (or `eval`) [+ a comparison] | a script in the page: truthy, or its value compared |
| `request <METHOD> <url glob> [status <n>]`, `no request <METHOD> <url glob>` | a request the page made in this scenario (`*` in the URL matches anything) |
| `request … json <$.path>` + a comparison | a value in the last matching response's JSON: `$`, `.key`, `[0]`, `["key"]` (`$.items[0].sku`) |
| `request … count` + a comparison | how many matching requests |
| `cookie <name>` [+ a comparison], `no cookie <name>`, `cookie <name> absent` | a cookie of the scenario's browser context |
| `storage local\|session <key>` [+ a comparison \| `absent`] | Local or Session Storage |
| `download <file glob>`, `no download …` | a finished download (then `${download.path}`, `${download.name}`: the latest match). Each scenario's downloads go to a directory of its own beside its evidence (`.primavera/runs/<time>/<scenario>/downloads`, kept only when something came), so a name never carries over from another scenario; a second file of the same name in one scenario arrives as `name (2).ext` |
| `download <file glob> compare <expected file>` [`--key <columns>`] [compara flags…] | the download is the same table as the expected file beside the `.pv` ([compara](https://github.com/andrey-usa/compara) on the PATH, or `PRIMAVERA_COMPARA`): CSV, NDJSON or Parquet, either side; compara's flags (`-i`, `--trim`, `--tolerance`) pass through. A difference fails the step with compara's counts and the reports it kept beside the scenario's downloads: `--report html,json,rows` (the HTML report, the JSON summary of counts and column statistics, CSVs of the changed, added and removed rows) or `none`; the run's `--compare-report` (`compare_report` in `primavera.toml`, default `html`) when the step doesn't say, and compara's own `-o`, `--json` or `--export-dir` win |
| `console` + a comparison, `no console "<t>"` | a console message (any level) |
| `no errors` | no page exceptions or console errors |
| `accessible` [`--selector <css>`] [`--tags wcag2a,wcag2aa,…`] [`--ignore <rule>,…`] | [axe-core](https://github.com/dequelabs/axe-core) (built in) finds no violation in the page, or under the selector: WCAG 2.0 and 2.1 A and AA by default (`--tags best-practice` and the rest of axe's tags too); a failure names each rule, its impact, the first elements and what to fix |
| `lcp`, `fcp`, `ttfb`, `load` `<`, `<=`, `>`, `>=` `<time>` (`2.5s`, `800ms`); `cls` + a comparison and a score (`0.1`) | a performance budget for the current page, as the browser measured it: largest and first contentful paint, time to first byte, the load event (ms from the navigation's start); the layout shift score (its largest session window, shifts after input left out). Not measured yet (the load event still to come) is retried |
| `exit` [`is`] `<n>`, `stdout`/`stderr` + a comparison | what the last `exec` gave |
| `status` [`is`] `<n>`, `body` + a comparison, `json <$.path>` [+ a comparison], `header <name>` [+ a comparison] | the last `http` answer |
| `ax [<target>] matches` + the tree indented under it | the page's (or the target's) accessibility tree, as `navigera ax` prints it, holds those lines: partially (`button` matches `button "Place order"`) and in order, a line indented under another inside what that one matched, refs ignored, `/regex/` (spaces allowed) for a name or a value |
| `screenshot <name> [<target>]` [`--full-page`] [`--threshold 0.2`] [`--max-diff-pixels <n>` \| `--max-diff-ratio <r>`] [`--mask <css>`]… | the viewport (or the element, or the whole page) as its baseline shows it, pixel by pixel; see below |
| `not …` | any of them, negated |

  `--soft` on any `expect` notes a failure and goes on; the scenario fails
  at its end with all of them. A `${var}` can stand for any compared value
  (a count, a regex), checked once it is filled in; `--timeout-ms` takes a
  number as written.

  ```text
  expect ax --role form --name Checkout matches
    - textbox "Email" value=/.+@example\.com/
    - combobox "Country" value="Canada"
    - button "Place order"
  ```

  A screenshot's baseline is beside the `.pv` file, in
  `<file>.pv.screenshots/<name>-<os>.png` (fonts differ between systems,
  so each OS keeps its own); keep it with the tests. With none yet the run
  writes it and the step fails, so a person looks at it first;
  `--update-screenshots` writes or replaces baselines and passes. While it
  is taken, animations and transitions stop, the caret hides and each
  `--mask` element is painted over in magenta (a mask that matches nothing
  fails the step). An element is scrolled into view first, inside its
  scroll containers and frames, and cropped where it is on the page, in an
  iframe too. Two pixels differ when their
  colour distance is over `--threshold` (0 to 1); a mismatch writes this
  run's image and a diff (the differing pixels red) to the evidence
  directory and names both.

  Copy the lines from `navigera ax` and keep the ones that matter. A miss
  names the first line that has no place and the nearest line of its role
  (`no line matches \`- combobox "Country" value="Mexico"\`; nearest:
  \`- combobox "Country" value="Canada"\``).

## Gherkin

`.feature` files run as they are, beside `.pv` files (no generated code):
`Feature`, `Background` (before each scenario), `Scenario`/`Example`,
`Scenario Outline` (or `Scenario Template`) with `Examples` (or
`Scenarios`) tables, several allowed, each with its own tags (`<column>`
filled in; one scenario per row, named `Name (col=value, …)` when the name
has no `<…>`), `Rule` (a `Background` under it runs before that rule's
scenarios only), tags (a feature's go on all its scenarios), and
`Given`/`When`/`Then`/`And`/`But`/`*` steps, all alike. English keywords
only.

A step is one of the run's own `step` blocks, from any `.pv` file it loads:

```text
step "I sign in with {password}"
  fill --label Password ${password}
  click --text "Sign in"
```

`{name}` takes a `"…"` or `'…'` value or one word and is `${name}` in the
steps under it; a line two `step` blocks match is an error. Else it is a
phrase of the built-in book: `I open "/cart"` (`go to`, `navigate to`,
`visit`, `am on`), `I click "Apply"` (`click on`; by visible text), `I fill
"Email" with "a@b.c"` (`fill in`, `enter`; by label), `I type "x" into
"Search"`, `I select "Canada" from "Country"` (or `in`), `I check "Terms"`,
`I uncheck …`, `I press "Enter"`, `I wait for "Saved"`, `I see "Saved"` (`I
should see`), `I don't see "Error"` (`should not`, `shouldn't`, `do not`),
`the URL is|contains|starts with|ends with|matches "…"` (`url`, `page URL`),
`the title is|contains … "…"` (`page title`). An icon button or a field
with no label needs a `step` block (`click --role …`, `--placeholder`). A
step nothing matches is a file error that says how to write it. Data tables and doc strings under a
step aren't supported yet.

`--cucumber <file>` writes Cucumber JSON for the `.feature` files, step by
step (passed, failed, skipped, with times), for test-management importers;
`--junit`, `--html`, `--allure` and the console report them like any
scenario.

## Running

```text
primavera run [<file.pv|file.feature|dir>...] [--base-url <url>] [--var k=v] [--grep <text>] [--tag <tag>]
              [--workers <n|auto>|-j <n>] [--isolation context|browser] [--retries <n>] [--fail-on-flaky] [--shard <i/n>]
              [--repeat <n>] [--shuffle [<seed>]]
              [--reporter console|jsonl|github] [--junit <file>] [--cucumber <file>] [--allure <dir>] [--html <file>] [--json]
              [--timeout-ms <ms>] [--wait-ms <ms>] [--headed] [--chromium <path>] [--engine chrome|lightpanda]
              [--secrets <file>] [--evidence <dir>] [--pause-on-failure] [--trace on-failure|all] [--update-screenshots]
              [--compare-report html,json,rows|none] [--config <file>] [--env <name>]
primavera lint [<file.pv|file.feature|dir>...]
primavera list [<file.pv|file.feature|dir>...]
primavera trace <dir> [<line>]
primavera fix [<file>:<line>] [--use <n>] [--all [--min <p>]]
primavera navigera <command…>
primavera from-session <name> [--scenario <name>] [--out <file.pv>]
primavera record <url> [--out <file.pv>] [--scenario <name>] [--headless] [--for <seconds>]
primavera record --extend <file.pv> [--scenario <name>] [--base-url <url>] [--secrets <file>]
primavera mcp [--headed] [--chromium <path>]
primavera skill          # the agent skill (what an agent reads to write tests)
primavera --version
```

`--base-url` falls back to `base` in the file, then `PRIMAVERA_BASE_URL`.
`--workers auto` is half the CPUs, at most 8.

- A click that opens a tab or a popup (`target=_blank`, `window.open`)
  moves the scenario to it, as navigera does: the next steps read and act
  there. `tab-select 0` goes back to the first tab; `tab-close` closes the
  one you're on.
- Every scenario starts in a fresh browser context (navigera `reset
  --all`: no cookies, storage, tabs, routes, init scripts, `set`
  emulation or dialog answer from the scenario before), in about 40 ms
  instead of a new browser's few hundred. `--workers n` runs n at once,
  each in a browser context of one shared browser (about 40% less memory
  at 4 workers than a browser each; `--isolation browser` gives each its
  own); the longest start first (by `.primavera/timings.json`, which each
  run updates; the first run goes in file order).
- `--engine lightpanda` (or `engine = "lightpanda"` in a profile) runs on
  [Lightpanda](https://github.com/lightpanda-io/browser) (`$LIGHTPANDA_BIN`
  or `lightpanda` on the PATH), a new Lightpanda for each scenario (it has
  one context per process): less memory, no layout. A smoke tier, not a
  replacement: hover-revealed elements, new tabs, screenshots and `ax`
  pixels need Chrome. On the 40-scenario suite's 25 site scenarios it
  passes 22, and is not faster: 22 s against Chrome's 11 s at one worker
  (half of it the three failures' waits).
- `--html report.html` writes the run as one page that needs nothing
  else to open: failed scenarios first and open, each with its steps and
  their times, the failure (message, where the page was, what came
  closest, the note) and its screenshot in the page (up to 2 MB; a
  bigger one is linked); flaky ones show how their first try failed.
  Secrets are masked as in the console. A screenshot two scenarios share
  (a failed `before all`) is in the page once; a bigger one, and any once
  the page holds 10 MB of them (failures first), is a link relative to the
  report, so upload the report and `.primavera/runs/`
  together. With `--repeat`, a pass count per scenario comes first. A run
  that finds no scenario (exit 2) writes no report.
- `--allure <dir>` writes Allure 2 results for `allure generate <dir>` or an
  Allure server: one `<uuid>-result.json` per scenario run with its steps,
  times, status (a flaky one passed, marked flaky), labels (the file, the
  `.feature`'s name, tags, the worker) and the failure's screenshot
  attached. The directory is kept, so several runs or shards can feed one
  report; the history id is the same from run to run.
- `--trace on-failure` (or `all`) records each step of a scenario: where
  the page was, the console messages and the requests it brought, in
  `trace.jsonl` beside the scenario's screenshot, with the page's ax tree
  at a failure in `ax.txt`. `primavera trace <dir>` prints it (a run's
  directory lists its traces); `primavera trace <dir> <line>` shows one
  step in full, with the tree when it is the one that failed. Secrets are
  masked in it as in the console.
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
  the failing lines, beside the console (on by itself under GitHub
  Actions). `--json` prints
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

  When that list is unsure (the best under 0.6, or the first two within
  0.1) and a decision model is set up for navigera (`TYPESAFE_API_KEY`, or
  `NAVIGERA_DECIDER_URL`), primavera also asks it which element the step
  meant (navigera's `find`, on the failing page) and puts its pick first,
  marked `(decider)`; one call per such failure, none otherwise.
- `primavera fix <file>:<line> [--use <n>]` puts the last run's n-th
  "nearest on the page" into that step (only its locator flags change, and
  only if the line still reads as it did); `primavera fix --all [--min
  0.6]` takes each failure's first suggestion that scored that much, and
  plain `primavera fix` lists them. The failure block prints the command.
  A `.feature` step is left alone: change its `step` block instead.
- `primavera navigera …` is navigera itself, built in: explore a page the
  way an agent does (`primavera navigera -s work start`, `goto`, `ax`,
  `click 12`, `fill 15 ada@example.com`) with no second program to
  install. `primavera from-session work` then writes what that session did
  as a scenario: each action once, and the waits between (reads and `eval`
  left out), every ref replaced by the most stable locator that found its
  element again just before the action (test id, role and name, label, placeholder, text, CSS;
  navigera's `locate`, each checked on the page; inside a web component's
  shadow root, CSS through its hosts: `--selector "#tip >>> button"`), `base` from the first
  `goto`, an `expect url` where an action moved the page, an `expect text`
  for a heading that changed or a status/alert message that appeared (as a
  comment when it has numbers, which may change), and a value typed
  into a password field (`type=password`, as navigera's `locate` says) as
  `${secret:NAME}`, named after the field (`${secret:PASSWORD}`,
  `${secret:CONFIRM_PASSWORD}`). What it can't keep (a mouse at
  coordinates, a ref no locator found again) is a `# left out:`
  line. Read it through, keep the expects that matter, add the ones the
  test is for. A session started by a plain `navigera` works too when that
  navigera has `history`.
- `primavera record <url> --out t.pv` opens a browser to use by hand: each
  click, field change, check, select, upload and Enter becomes a step as
  you do it, with the most stable locator that finds the element (the same
  `locate` as `from-session`, run in the page at that moment, so a click
  that navigates away is still known), an `expect url` where the page
  moved, a password field's value as `${secret:NAME}` (never written
  down). The file is rewritten after
  every step: close the browser or press Ctrl-C to stop. A click into a
  field isn't a step (its value is); typing in a field twice keeps the
  last value.
- `primavera record --extend t.pv [--scenario <name>]` grows a scenario
  instead: it runs that scenario (default: the file's last) to its end,
  `after each` left out, then hands you the browser there; what you do
  goes in after its last step, the same way, and the rest of the file stays
  as it was. A scenario that fails before its end is not extended: fix it
  first.
- `primavera mcp` is the same loop for an agent over MCP (stdio): navigera's
  browser tools (`browser_navigate`, `browser_snapshot`, `browser_click`,
  … one browser for all of them) beside `primavera_from_session` (what
  those tools did, as a scenario; `out` writes it), `primavera_run`,
  `primavera_list`, `primavera_lint`, `primavera_failure` (the last run's
  failures with the nearest elements) and `primavera_fix`. Register it as
  `{"command": "primavera", "args": ["mcp"]}`.
- `--pause-on-failure` stops at a failure with the browser as the failure
  left it and prints how to look at that very tab (`navigera -s pv start
  --attach ws://…/devtools/page/…`, then `navigera -s pv ax`); Enter goes
  on with the run. It runs one worker. With no terminal to press Enter in,
  it waits `PRIMAVERA_PAUSE_S` seconds (default 600, for an agent looking
  from elsewhere), and under CI (`CI` set) it doesn't pause unless that is
  set.
- Exit codes: 0 passed, 1 a scenario failed, 2 a file or argument is wrong,
  3 the browser or the app server didn't start.

## Config: `primavera.toml`

A `primavera.toml` in the current directory (or `--config <file>`) holds
what a run takes by default; flags on the command line win over it.
Paths in it are from its own directory.

```toml
base_url = "http://127.0.0.1:3000"
paths = ["tests"]            # what to run when the command line names nothing
workers = 4                  # or "auto"
retries = 1
reporters = ["github"]       # added to --reporter
junit = "reports/junit.xml"  # also html, cucumber, allure, secrets, evidence, chromium
trace = "on-failure"         # or "all", "off"
compare_report = ["html", "json"]  # what `expect download … compare` keeps
[vars]                       # as --var; --var on the command line wins
region = "us"

[server]                     # the app under test
command = "npm run dev"      # sh -c (cmd /C on Windows)
cwd = "app"                  # where it runs (default: the config's directory)
url = "http://127.0.0.1:3000/health"
timeout_s = 60
# reuse = true               # one already answering at url is used (default, except under CI: CI set and not empty, false or 0)
# env = { PORT = "3000" }

[env.staging]                # --env staging: on top of the rest
base_url = "https://staging.example.com"
[env.staging.vars]
region = "eu"
```

The `[server]` starts before the browser and is ready when its `url`
answers (any status under 500); its output goes to `.primavera/server.log`,
and it is stopped with everything it started when the run ends. One that
exits or doesn't answer in time ends the run with exit 3 and the end of its
log. Other keys: `timeout_ms`, `wait_ms`, `isolation`, `fail_on_flaky`,
`headed`; an unknown key is an error.

## GitHub Actions

```yaml
- uses: andrey-usa/primavera@next
  with:
    paths: tests
    base-url: http://127.0.0.1:3000
    args: --workers auto --retries 1 --junit report.xml
```

It runs on the runner's Chrome (Linux, macOS, Windows). `version` picks a
release (`latest` by default, or a tag); with no release binary for the
runner it builds primavera from the action's own commit (`source` asks for
that), with navigera at `navigera-ref`. Failures are annotations on the
lines that failed; the `evidence` output is the directory holding their
screenshots, for `actions/upload-artifact`.

## Container

`Dockerfile` builds an image with primavera and Debian's headless Chromium,
amd64 or arm64, from the directory holding both checkouts (see Building):

```console
$ docker build -f primavera/Dockerfile -t primavera .
$ docker run --rm --network host -v "$PWD/tests:/tests:ro" primavera run /tests --base-url http://127.0.0.1:3000
```

It runs as an unprivileged user (any uid, so OpenShift's random ones too),
with a read-only root filesystem if asked (`HOME=/tmp` and a `/tmp` to
write to), and without Chrome's own sandbox: a container without user
namespaces can't give Chrome one, and the container is the boundary.
`examples/kubernetes/job.yaml` is a Job: the tests from a ConfigMap, JUnit
and evidence in `/work/out`, a memory-backed `/dev/shm`.

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

MIT. `vendor/axe-core` is axe-core, unmodified, under the Mozilla Public
License 2.0 (its `LICENSE` beside it).
