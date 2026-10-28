---
name: primavera
description: Write and run plain-text web tests (.pv files) on real Chrome with primavera — each step is a navigera command line, each check an `expect` that retries. Use to turn what you did in a browser into a regression test, to check a web app after a change, or to automate a browser job, without writing test code.
---

# primavera

`.pv` files of steps and checks, run on Chrome by one binary. The shortest
way to a passing test: write the scenario from what the task says, `lint`
it, run it, and fix what the failure names. Look at the page only when the
failure doesn't say enough.

## A file

```text
base http://127.0.0.1:8000

before each
  goto /shop
  optional click --selector "#cookie-accept" --timeout-ms 500

scenario "Adding a product counts it in the cart"
  click --role button --name "Add to cart" --nth 0
  expect --selector "#cart-count" text is 1
  goto /cart
  expect text "Your cart"
```

- Top level: `base <url>`, `set NAME = value`, `before each`, `after each`,
  `scenario "name"`. Steps are indented under them.
- A step is a navigera command: `goto /cart`, `click <target>`, `fill
  <target> "text"`, `select <target> "option"`, `press Enter`, `check
  <target>`, `hover`, `scroll`, `upload <target> file.txt`, `wait --text
  "Saved"`, `eval "<js>"`, `dialog --accept`, `tab-select 1`. `primavera
  navigera <command> --help` gives a command's flags and examples.
- Targets are locators, never refs: `--role button --name "Pay"`, `--label
  Email`, `--text "Sign in"`, `--testid total`, `--placeholder Search`,
  `--selector "#id"`. `--nth 1` (0-based) picks among the matches of any but
  `--text`. Frames and shadow roots are searched for you.
- Words split like a shell's: quote anything with spaces; inside double
  quotes write `\"` for a quote; single quotes keep backslashes as written.
  `${NAME}` is a variable; any other `$` is just a `$`.
- `save NAME = <command>` keeps a result (`save total = get text --testid
  total`), used later as `${total}`.
- `print <command>` shows a result in the run's output without checking or
  waiting: `print get text --testid total`, `print eval "…"`, `print ax
  --selector main` (the page as a tree). Use it to look while you write.
- `optional <step>` may fail (a banner that only sometimes shows); `if
  <expect clause>` runs the steps indented under it when the clause holds.

## expect (retried until it holds, 5 s)

```text
expect url ends-with /cart
expect title contains Checkout
expect text "Thank you"
expect no text "Error"
expect --role button --name Pay enabled
expect --testid total text is "$36.00"
expect --selector ".row" count >= 3
expect ${total} matches '^\$[0-9.]+$'
expect js "document.querySelectorAll('tr').length" >= 2
expect js --file check.js
expect request POST /api/orders status 201
expect not --selector .toast visible
```

- Comparisons: `is`, `contains`, `starts-with`, `ends-with`, `matches` (a
  regex, best in single quotes), `>`, `>=`, `<`, `<=`. An element's `text`,
  `value`, `attr <name>` or `count`, or a state: `visible hidden enabled
  disabled checked unchecked focused editable empty`. `not` negates any.
- `text "…"` is visible text anywhere on the page.
- `js --file check.js` runs a script beside the `.pv` (a sum of rows, …):
  an expression, or a body that `return`s. It holds on a true value, or
  compares like the rest.
- Comments are whole lines starting with `#`.

## Run, read, fix

Write the file and run it in one command:

```bash
cat > cart.pv <<'EOF'
…
EOF
primavera lint cart.pv && primavera run cart.pv --base-url "$BASE_URL"
```

`lint` takes milliseconds and catches a misspelled step or flag before a
browser starts. A failure names the line, what was expected and what was
seen, the page and its title, and, when an element wasn't found, the
closest ones on the page as locators:

```text
FAIL  cart.pv:3  Adding a product counts it in the cart  (5.2 s)
      line 5: click --role button --name "Add to basket"
      click role="button" name="Add to basket": no element … within 5000 ms
      at http://127.0.0.1:8000/shop, title "Shop"
      nearest on the page:
        1. button "Add to cart"   --role button --name "Add to cart"   0.70
      fix: primavera fix cart.pv:5 --use 1
```

Paste the locator in, or run the `fix` line. When a click changed nothing,
the failure notes it: the page may not have been ready, so add an `expect`
for what shows it is ready before the click.

## Looking at the page

One browser session keeps its state (cart, sign-in) between commands:

```bash
primavera navigera -s look start
primavera navigera -s look --raw goto "$BASE_URL/shop"
primavera navigera -s look --raw ax           # the page as a tree
primavera navigera -s look --raw eval "document.querySelector('#total').textContent"
```

`ax` lines become locators: `button "Pay" [ref=12]` is `--role button
--name "Pay"`. After clicking through by ref (`--raw click 12`), `primavera
from-session look --out shop.pv` writes what the session did as a scenario,
refs turned into locators; add the expects the test is for.

Exit codes of a run: 0 passed, 1 a scenario failed, 2 a file or argument is
wrong, 3 the browser or the app server didn't start. More (flows, sign-in
once with `auth`, data rows, workers, retries, reports): `primavera run
--help` and the README.
