# `navigera` reference

navigera drives a real Chrome over the DevTools Protocol from one native
binary. There are three ways to run it:

| mode | how | for |
|---|---|---|
| **session** | `navigera -s <name> start`, then one command per process, then `quit` | agents' shell tools, people at a terminal |
| **serve** | `navigera serve`; JSON commands on stdin, one JSON response per line | programs holding a pipe open |
| **one-shot** | `navigera <command> …` | a single action (launches and closes a browser) |

The agent-oriented guide is [the skill](../skills/navigera/SKILL.md),
also printed by `navigera skill`. This page is the full reference.

## CLI shape

```text
navigera [GLOBAL FLAGS] <command> [ARGS] [GLOBAL FLAGS]
```

| global flag | meaning |
|---|---|
| `-s, --session <name\|path>` | named session. A bare name lives in a directory only you can enter: `$XDG_RUNTIME_DIR/navigera/<name>.sock`, else `$TMPDIR/navigera-<uid>/<name>.sock` (mode 0700; navigera refuses one that is a symlink, someone else's or open to others), `%TEMP%\navigera\` on Windows; a value with a path separator is used as the path. `$NAVIGERA_SESSION` sets a default for client calls |
| `--raw` | print only the result (strings unquoted, e.g. the `ax` tree). Errors go to stderr as `error: …`, notes (dialogs, new tabs, an exception the page threw, an engine difference) as `note: …` |
| `--pretty` | pretty-print the JSON |
| `--timeout-ms <ms>` | before the command: navigation/command timeout (default 35000). After an element command: that op's element wait (default 5000) |
| `--wait-ms <ms>` | element ops' wait when they name no timeout (default 5000; `$NAVIGERA_WAIT_MS`). On `start`/`serve`/`mcp` it holds for the whole session |
| `--download-dir <dir>` | where downloaded files go (`$NAVIGERA_DOWNLOAD_DIR`; default a directory of the session's own under the system temp directory) |
| `--chromium <path>` | browser binary (also `$NAVIGERA_CHROMIUM`, `$CHROME_BIN`) |
| `--engine chrome\|lightpanda` (`$NAVIGERA_ENGINE`), `--headed` | engine; visible window |
| `--idle-timeout-s <s>` | a session server shuts down after this idle time (default 1800, 0 = never) |
| `-V, --version`, `-h, --help`, `help <command>` | info |

Every command prints one JSON object: `{"id":…, "ok":true, "result":…,
"elapsed_ms":…}` or `{"ok":false, "error":"…"}`. Two optional fields report
side effects: `dialogs` (dialogs the page opened and how they were answered)
and `new_tabs` (tabs the page opened, now in the tab list). The exit code is
0 on success, 1 on a failed op and 2 on bad arguments.

**Targets.** Element commands take exactly one target:

- a ref from `ax` as the first argument (`click 12`; `@12`, `e12` and
  `ref=12` also work) or `--ref 12`;
- `--selector <css>`: the first match a user could reach (when the first
  one is parked off the page and another isn't, the other);
- `--text "<visible text>"`: the best match by accessible label, text or
  value. An exact match beats a partial one, actionable elements beat plain
  ones, and a `<label>` stands for its control.

- `--role <role> [--name <accessible name>]`, `--label <form label>`,
  `--placeholder <text>`, `--testid <data-testid>`: semantic locators
  (case-insensitive, exact before contains, visible first, open shadow
  roots included); `--nth <i>` picks among their matches or `--selector`'s
  (0-based, `-1` = the last).
- `--frame <css>` with any of them: look only inside that iframe or frame
  (`"#outer >> #inner"` for one inside another), of this origin or another,
  when two frames hold the same element (two address forms, two payment
  frames). `wait --selector/--text/--gone --frame …` too. When the CSS
  matches several frames, the first in document order is the one; when it
  matches none, the error says so.

Selector, text and locator targets auto-wait (default 5 s, `--wait-ms`) for
the element to appear. They search the top document first, then its open
shadow roots and the documents of same-origin frames (iframes, framesets,
`srcdoc`), so a form inside an iframe or a button inside a web component is
reached the same way. When the element isn't there, frames of another
origin (a card form from a payment provider, a sign-in frame) are searched
too, from an isolated world navigera makes in each, which the page can't
see. A single `wait --text` or `wait --selector` looks there as well. In a
browser navigera launched, cross-site frames share the page's process, so
all of them are reachable; with `--attach` to your own browser, frames in
another process are reachable by ref only.

## Commands

| command | args | result |
|---|---|---|
| `start` | (with `-s`) | `{session, socket, log, pid}`; detached server + browser; `already_running` if it exists |
| `goto` | `<url>` or `--url`; `--wait load\|domcontentloaded\|commit`; `--timeout-ms` | state `{tab, tabs, url, title}`. `example.com` → `https://`, `localhost:3000` → `http://` |
| `ax` (`snapshot`) | `[<ref> \| --selector <css> \| --text t]` scope; `--limit <lines>` (2000); `--refs all`; `--format json` (old flat list); `--all` | text tree (below) |
| `click` | target; `--button right\|middle`; `--force` | state; fails naming the element that covers the target; `synthetic_click` note if a DOM click was used (`--force`, an element with no box); `no_effect` note if the page did nothing in response |
| `fill` | target, `<value>` or `--value`; `--keys` selects what the field holds and types the value key by key (apps that keep only typed input, observable-model forms) | state |
| `type` | `<text>` or `--value`; optional `--selector/--ref/--text` to focus first | state |
| `press` | `<key>` or `--key`; optional target to focus first | state |
| `select` | target, option label(s)/value(s) (positional or repeated `--value`; labels match with whitespace collapsed); `--add` keeps what a multiple select already has. On a custom dropdown (an ARIA combobox: react-select and the like) it does what a user does: focuses it, opens it (ArrowDown, else a click), finds the option by its text in the listbox that opened (a portal at the end of `<body>` included), types to filter when it isn't listed, and clicks it | state + `selected` |
| `hover` | target | state; `synthetic_hover` note if the element could not be hovered |
| `mouse` | `move <x> <y> [--steps n]`, `down`, `up`, `click <x> <y> [--count n]`, `wheel <dx> <dy>`; `--button left\|right\|middle`; an optional target makes x/y relative to its top-left corner, scrolling it into view when that point is out of it (`down`, `up` and `wheel` with a target go to its center; a target in a cross-origin frame is refused). A `click` at viewport coordinates outside the viewport is refused; a `move` may leave it (a drag out of the window) | state, plus `mouse: {x, y, held}` (viewport CSS pixels, the buttons still held) |
| `scroll` | `--by <px>` (default 800) or `--to top\|bottom`; or a target to scroll into view (`--by` and `--to` scroll inside it: a list that loads more at its end) | `{scrollY, scrollHeight, viewport}`, plus the element's `scrollTop` |
| `upload` | target, file path(s) (positional or `--file`) | `{files}` |
| `wait` | any of `--selector <css>` (visible), `--text <t>`, `--url <part>`, `--gone <css>`, `--js <expr>`, `--idle` (no fetch/XHR for 500 ms; also `--load networkidle`), `--download [<glob>]` (a finished download whose file name matches), `--ms <n>`; `--timeout-ms` | state + `waited_ms` (+ `download`) |
| `downloads` | `--clear`, `--json`, `--dir <path>` | `#n state bytes path (url)` per file the pages downloaded, saved under `--download-dir` with the name the page gave it (`report (2).csv` if taken). With `--attach` the browser keeps its own download folder and `path` is empty. `--dir` saves this session's downloads in `<path>` from now on (created if missing; kept across `reset`), apart from other sessions' |
| `eval` (`evaluate`) | `<js>`, `--expression <js>`, `--file <path.js>`, or `-` (read the script from stdin) | the JSON value (an arrow function is called; promises are awaited) |
| `text` | `--selector <css>` | `textContent` or `null` |
| `find` | `<description>` | `{ref, element, in, p, confidence, others, sure}`: the element a decision model chose ([below](#decisions-find-and-check)) |
| `check` | `<yes/no question>` (no target given) | `{question, yes, answer}`: the probability the answer is yes |
| `check` / `uncheck` | target (a ref or a targeting flag) | state + `checked`, `changed`: clicks a checkbox/radio/switch only when its state differs, fails if the click didn't change it |
| `get` | `text\|value\|html\|outer\|box\|checked` target; `attr <name>` / `style <prop>` target; `count <css>`; `title`; `url` | the value; `box` is `{x, y, width, height}` in top-level viewport pixels (inside a cross-origin frame, that frame's) |
| `is` | `visible\|hidden\|enabled\|disabled\|checked\|editable\|focused` target | `true`/`false` (a missing element is not visible, after 500 ms) |
| `dblclick` / `focus` | target | state / `{focused}` |
| `drag` | `<from> <to>` (each a ref or CSS selector), or `--from-text <t> --to-text <t>` | state and `drag`: `{html5: true, dropped}` for a `draggable` source (the drag events with one DataTransfer, which headless Chrome will not start from synthetic input; `dropped` is false when the target does not accept it), else the two points a held mouse moved between (sliders, pointer-driven sortable lists) |
| `keydown` / `keyup` | `<key>` | holds / releases a key; held modifiers apply to later `press`es |
| `insert-text` | `<text>`, optional target to focus first | state: text inserted at the caret in one go (paste, IME), no key events |
| `console` | `--level error\|warn\|info\|log\|debug`, `--limit <n>` (200), `--clear`, `--json` | `[level] text  (url:line)` per line: console calls, uncaught exceptions (`[exception]`), failed loads and CSP reports |
| `errors` | `--limit`, `--clear`, `--json` | `console --level error` |
| `network` | `--filter <text>`, `--type fetch\|xhr\|document\|…`, `--failed`, `--since <n>` (only requests after #n), `--limit <n>` (100), `--clear`; `--har <file>` | `#n METHOD STATUS type url size time` per line (same-origin URLs as paths); `--har` writes HAR 1.2 |
| `network <n>` | `--body-file <file>` | one request in full: headers, post data, response body (8 KB shown; the file gets all of it) |
| `cookies` | `[--all] [--url <u>] [--json]`; `set <name> <value> [--url <u> \| --domain <d>] [--path] [--expires <unix s>] [--http-only] [--secure] [--same-site Lax\|Strict\|None]`; `delete <name>`; `clear` | one cookie per line (`--all`: every cookie in the browser) |
| `storage` | `[local\|session] [get [<key>] \| set <key> <value> \| remove <key> \| clear]` | the value, or the whole store as an object |
| `state` | `save\|load <file.json>` | every cookie (HttpOnly too) and the open tabs' localStorage to a file, or back: `{saved\|loaded, cookies, origins}`. The file is Playwright's `storageState` format, so either tool reads the other's. `load` sets the cookies at once and gives each origin's keys to documents of that origin that lack them |
| `show` / `hide` | — | the session's browser on screen, or off it, with the same tabs (the active one stays active) and sign-ins: a `--profile` browser restarts on its profile, which keeps everything; one navigera launched is replaced by one carrying its cookies and the open tabs' storage. For a step only a person can do (a login, 2FA, a CAPTCHA). `show` needs a display. Not for `--attach`, which is already on screen | state + `visible` |
| `profiles` | — | no session needed: `{browsers: [{browser, user_data_dir, attach_ready, hand_off, profiles: [{dir, name, email, last_used}]}], navigera_profiles: [{name, dir, running}]}`: what `--attach` and `--from` can be pointed at |
| `pdf` | `[<path>]` or `--path`; `--landscape`; `--background` | `{path, bytes}` |
| `set` | `viewport <w> <h> [--scale <dpr>] [--mobile]`, `device <name>` (iPhone 15, iPhone 15 Pro Max, iPhone SE, Pixel 8, Galaxy S24, iPad Mini, iPad Pro 11, Laptop, Desktop), `media dark\|light\|print\|reduced-motion\|forced-colors`, `geo <lat> <lon> [--accuracy <m>]`, `offline on\|off`, `headers 'Name: value'…` (or a JSON object), `ua <string>`, `locale <tag>`, `timezone <zone>`, `credentials <user> <password>`; `off` resets one | what was set. Applies to every tab and to tabs opened later |
| `route` | `<url glob>` (`*`, `?` over the whole URL), then `--abort` (default) or `--status <n> [--body <text> \| --body-file <f>] [--content-type <t>] [--header 'Name: value']…`; no argument lists the rules with their hit counts | `{route, action}`. Only matching URLs are intercepted |
| `unroute` | `[<url glob>]` | `{removed}` (all rules without a glob) |
| `init-script` | `<js>`, `--file <path.js>`, `--clear`; no argument lists them | runs before the page's own scripts in every document of every tab |
| `status` | — | `{engine, executable, endpoint, attached, version}` + state (`closed: true` once the session's only tab was closed) |
| `locate` | target (a ref, usually) | `{locators: [{kind, flags}]}`: the flags that find the element again, most stable first (`testid`, `role` + name, `label`, `placeholder`, `text`, `css`), each kept only when the op's own finder picks this element on the page as it is; inside a same-origin frame each has `--frame`; `xframe: true` and none inside a cross-origin one |
| `history` | `--clear` empties it | `{history: [{command, at, url, locators?}]}`: the actions the session ran (navigation, clicks, typing, selects, uploads, drags, scrolls, tabs, dialogs; not reads, waits or `eval`), oldest first, the last 1000; one on a ref has the `locate` of its element from just before it ran, so `primavera from-session` can write it as a test |
| `title` / `url` | — | string / state |
| `back` / `forward` / `reload` | — | state |
| `screenshot` | `[<path>]` or `--path`; `--full-page` | `{path, bytes}` |
| `reset` | | `{tabs, active, url}`: a new browser context (no cookies, storage or cache from before) with one blank tab, in the same browser (about 40 ms); the old tabs close; `set`, routes, credentials, init scripts, the dialog answer and the `downloads --dir` stay (`reset --all` drops them, as in a new session). Chrome only |
| `tab-new` | `[<url>]` | `{tabs:[{index,target,active}], active, url}`; a tab that was closed under the session (the user closed it, or the page did) shows `closed: true` until `tab-new` replaces it, and ops that need it say so |
| `tab-list` | — | same |
| `tab-select` | `<index>` or a `target` id (or its first characters) from `tab-list`, which stays right when tabs open and close | state |
| `tab-close` | `[<index>]` (default: active) | state |
| `dialog` | `--accept [--prompt-text t]` or `--dismiss` | `{dialogs: "accept"\|"dismiss"}` |
| `quit` (`close`) | — | `{bye: true}`; a session server and its browser exit |
| `skill` | — | prints the agent guide (SKILL.md) |
| `install-skill` | `[--dir <skills dir>] [--claude \| --continue] [--global]` | writes `<dir>/navigera/SKILL.md` (default `./.agents/skills`, read by Cline and Kilo; `--claude`: `.claude/skills`; `--continue`: `.continue/skills`; `--global`: under your home directory) |

### The `ax` tree

```text
page "Cart — Acme Supply" http://127.0.0.1:8765/cart (tab 0 of 2)
- main:
  - heading "Your cart" [level=1]
  - table:
    - row:
      - cell "Brass Sprocket"
      - cell "Qty 2":
        - spinbutton "Qty" [ref=146] value="2"
      - cell "Remove Brass Sprocket": button [ref=193]
  - textbox "Coupon code" [ref=148]
  - button "Apply coupon" [ref=212]
```

- The tree comes from one `Accessibility.getFullAXTree` call per frame.
  Same-origin and in-process iframes are spliced in under their `Iframe`
  node, and open shadow roots are included.
- Wrapper nodes (`generic`, label boxes) are collapsed, and text that repeats
  an ancestor's name or a neighbouring control's label is dropped.
- A named node with one same-named actionable child is printed on one line,
  for example `heading "X": link [ref=7]`.
- `[ref=N]` (a backend DOM node id) appears on actionable nodes: interactive
  roles and focusable elements. Use `--refs all` to get one on every node.
- States are shown in brackets: `checked`, `disabled`, `expanded` or
  `collapsed`, `selected`, `required`, `invalid`, `level=N`.
- Inputs show `value="…"`, links show `url=` (shortened to the path on the
  same origin), and `<select>` lists its `options:` inline.
- Collapsed content (closed `<details>`, menus that open on hover) is not in
  the tree until it is opened.

## Behaviour

- **Snapshots wait for fetched data.** Before `ax` and `screenshot`,
  navigera waits until the tab has had no `fetch`/XHR in flight for 500 ms
  (Playwright's `networkidle` window), at most 3 s; requests open longer
  than 5 s (long polls, streams) don't count. A page that has been quiet —
  usual when an agent has been thinking — is read at once. So
  `goto … && … ax` shows a single-page app's list, not its "Loading…"
  placeholder.
- **Clicks** are trusted mouse events at the element's centre after
  scrolling it into view, once the element has stopped moving: CSS
  animations and transitions running on it or an ancestor (a dialog sliding
  in) finish first, as a user waits for them, and pages often wire a
  dialog's buttons only once it has opened. With nothing animating this
  costs nothing. navigera checks that the press reached the page:
  Chrome occasionally acknowledges mouse events it never delivers (seen
  right after a tab switch), and then the click is sent once more, or as
  DOM events if it still doesn't arrive (`synthetic_click` says so). Same-origin iframe offsets are added in the page;
  inside a cross-origin iframe (refs only) the box comes from
  `DOM.getContentQuads`.
  - A styled checkbox or radio (the `<input>` hidden or covered by its own
    label's box) is clicked through its label, as a user would.
  - If something else covers the element's centre (a toast, a sticky
    header, an overlay), the element is tried at the edges of the viewport,
    then the click waits for the cover to go (up to the element wait). If it
    stays covered the click fails and names the covering element: a user
    would have clicked that instead. `click --force` sends DOM events to the
    element regardless (`synthetic_click`). `fill` likewise scrolls a field
    out from under a panel before setting it.
  - A disabled control is waited for (up to the element wait) and then
    refused; `fill` does the same for a disabled or read-only field.
  - An element a user can't see where it is (zero size, clipped, or under
    its own wrapper, like the real input of a styled radio card) fails at
    once, naming what a click there would hit and, where there is one, what
    stands for it (its label, or the clickable card around it). `--force`
    sends DOM events to it.
  - If the click starts a navigation, the command returns once the new
    document is parsed.
  - A click that changes nothing says so. From just before the press,
    navigera watches the page's DOM (the document, the element's frame and
    every open shadow root), events a DOM observer can't see (input,
    change, submit, toggle, invalid, copy, a scroll, a navigation
    starting), clipboard writes, focus moving elsewhere, form values and
    the URL a script set, and what only the browser sees (navigations,
    dialogs, new tabs, downloads, requests). The first sign ends the
    watch, usually within the round trip that already checked delivery.
    Requests that are the page reporting on itself don't count: beacons
    and well-known analytics services are ignored, and a first-party
    request that reads like reporting (`/event/…`, `/collect`, `/log`) is
    waited for (up to 1.5 s) and judged by what the page does with the
    answer. With nothing after 200 ms (or after that answer), the result
    carries `no_effect`, naming any reporting requests: often a control
    the page hasn't wired up yet, so check for what the click should do
    before clicking again. Clicks on fields, `<select>`s and right or
    middle clicks aren't judged, nor pages with same-origin frames it
    doesn't watch. `NAVIGERA_TRACE_EFFECT=1` on `start` adds an
    `effect_trace` to click results (what was seen).
- **Page errors are reported.** An uncaught exception while a command ran
  adds one line to its `notes` (`the page threw TypeError: …; \`errors\`
  lists them`), so a click whose handler crashed doesn't look like a click
  that did nothing. `console`, `errors` and `network` keep the last 1000
  entries per tab.
- **A page replaced mid-op:** when a navigation replaces the page while an
  op is still looking for its element (a step that saves with a request and
  only then reloads, a client-side redirect), the op looks again on the new
  page within its wait.
- **Navigation settling:** every command that reads or acts on the page
  first waits for any navigation an earlier action started, such as a JS
  redirect, so it never reads the previous page. The wait is bounded by the
  op's own `timeout_ms` and never fails the op. Once the new document has
  committed, it waits at most 5 s for `DOMContentLoaded`: a page that keeps
  streaming is then used as it is, and the response says `"loading": true`.
- **Paths** (`upload`, `screenshot`) given to a session client are resolved
  in the caller's working directory, not the session server's.
- **New tabs:** pages opened with `target=_blank` or `window.open` are
  adopted into the tab list and become active. Tabs the page closes are
  dropped.
- **Dialogs** are answered the moment they open: accept by default, or
  dismiss after `dialog --dismiss`. A `beforeunload` dialog is always
  accepted. Each dialog is reported in the response.
- **No `Runtime.enable`:** it is the best-known automation fingerprint, and
  nothing here needs it.
- **Logs:** launch details go to stderr only with `NAVIGERA_VERBOSE=1`.

## Decisions: `find` and `check`

Two small decisions an agent otherwise spends a page snapshot and an LLM turn
on can go to a System One model instead: TypeSafe's
[Jev](https://docs.typesafe.ai), which answers typed questions (a choice
among options, a yes/no probability) in a few hundred milliseconds and bills
input tokens only. Opt-in: nothing is sent anywhere unless one of these is set
where the session (or `navigera mcp`) starts:

| variable | |
|---|---|
| `TYPESAFE_API_KEY` | TypeSafe's API, `https://api.typesafe.ai/v1/systemone` |
| `NAVIGERA_DECIDER_URL` | an endpoint with the same API (a LiteLLM proxy's `/typesafe/v1/systemone`, a mock); `http://` directly, `https://` through `curl` |
| `NAVIGERA_DECIDER_MODEL` | model id (default `jev-latest`) |
| `NAVIGERA_FIND_MIN` | probability `--find` needs before it acts (default 0.5) |

`start` reports `"decider": "<model>"` when one is configured, so an agent can
tell without looking at the variables.

- `find "add the brass valve to the cart"` returns the chosen element's ref,
  its line, the probability and the three runners-up. Every element with a
  ref is an option, described by its line and the containers around it (the
  card's heading, a dialog's name), so twelve "Add to cart" buttons are told
  apart. Large pages are split into choices of 200 and the winners asked
  again.
- Element ops take `--find "<description>"` in place of a ref (`click --find
  "accept the cookies"`, `fill --find "email field" a@b.c`): the op runs on
  the chosen element and its result carries `found`. Below `NAVIGERA_FIND_MIN`
  the op fails and lists the candidates, so a guess is never acted on.
- `check "Did the order go through?"` returns the probability of yes. The
  page goes as its text tree without refs; near 0.5 means unsure.
- A plain `ax` of a tree over 16 KB (`NAVIGERA_AX_PREVIEW_BYTES`; `0` turns
  it off) shows its first 40 lines, ends with `... N more lines not shown`,
  and notes that `--find` and `check` act on the whole page; a tree over 60
  lines but under that size is shown whole, with the note; `ax --limit <n>`, `--max-depth`, `--all` or a scope
  (`--selector`, a ref) returns what was asked for, with the note. Without a
  decider, `ax` is unchanged.

What leaves the machine: the page's tree, with what was typed into text
fields replaced by `(filled)` (password fields are masked by the browser
already). MCP clients see `browser_find`, `browser_check` and a `find` target
only when a decider is configured. `docs/multi-model.md` is the design and the
measurements.

## Serve protocol (programs)

`ax` without `format` returns the flat JSON list (`[{ref, role, name,
value?, url?}]`, `url` on links; main frame only) that serve clients have always received; send
`"format":"text"` for the indented tree the CLI prints.

```json
{"id":1,"op":"goto","url":"https://example.com"}
{"id":2,"op":"ax","format":"text"}
{"id":3,"op":"click","ref":12}
{"id":4,"op":"fill","selector":"#q","value":"widget","timeout_ms":3000}
{"id":5,"op":"select","text":"Country","value":["Canada"]}
{"id":6,"op":"wait","text":"Saved"}
{"id":7,"op":"eval","expression":"() => document.title"}
{"id":8,"op":"quit"}
```

Each op's fields are the long flag names in snake case: `selector`, `ref`,
`text`, `value`, `timeout_ms`, `wait`, `format`, `limit`, `by`, `to`,
`files`, `ms`, `js`, `accept`, `prompt_text` and so on. Malformed lines and
unknown ops return `ok:false` and leave the session running. EOF or `quit`
closes the browser. The session socket speaks exactly this protocol, one
connection at a time.

A line holding a JSON **array** of commands is a batch: each runs in order
and gets its own response line (a failure doesn't stop the rest; `quit`
does). Use it where the shell can't send newlines: PowerShell's `echo`
passes `\n` through literally, so `echo '{…}\n{…}' | navigera serve` is one
bad line, while `echo '[{…},{…}]' | navigera serve` works. A leading UTF-8
BOM (PowerShell 5 adds one) is ignored.

**Driving serve from a program.** Read one response line per command line
(a batch: one per element), match responses by `id`, and give every read a
timeout. Read or discard stderr (`stderr=DEVNULL` in Python): a full stderr
pipe stalls the server. Decode stdout as UTF-8 (`encoding="utf-8"` in
Python; Windows' default code page can't decode page text). A minimal Python
client:

```python
import json, subprocess
nv = subprocess.Popen(["navigera", "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                      stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
def call(op, _id=[0], **fields):
    _id[0] += 1
    nv.stdin.write(json.dumps({"id": _id[0], "op": op, **fields}) + "\n"); nv.stdin.flush()
    reply = json.loads(nv.stdout.readline())
    assert reply["id"] == _id[0], reply
    return reply
call("goto", url="https://example.com")
print(call("ax", format="text")["result"])
call("quit"); nv.wait()
```

An agent working from a shell needs none of this: use a session
(`navigera -s work start`, then one command per call). Per call, a direct
session command costs about 3 ms more than a line on a `serve` pipe (Linux,
warm session: `title` 3.3 vs 0.5 ms, `ax` 14.7 vs 11.1 ms), far less than one
model turn. A wrapper pays off only for long runs with no decision between
steps, written by a program that already exists; a script that launches its
own `serve` pays a browser cold start and loses cookies on every run.

## MCP server (IDE agents)

`navigera mcp` serves the [Model Context Protocol](https://modelcontextprotocol.io)
on stdio, for agents that take their tools over MCP: Cline, Roo Code, Kilo
Code, Continue, VS Code, Claude Desktop, Cursor. Each tool is one serve op, so
it behaves exactly like the CLI. The browser starts on the first tool call.
`--headed`, `--profile <name>` (a persistent, signed-in browser) and
`--attach` work as they do for sessions.

| tool | does |
|---|---|
| `browser_navigate` | `goto` |
| `browser_snapshot` | `ax`: the tree with `[ref=N]` (optional `ref`/`selector` scope, `limit`) |
| `browser_click`, `browser_fill`, `browser_type`, `browser_press_key`, `browser_select_option`, `browser_hover`, `browser_scroll`, `browser_file_upload` | the element ops, by `ref`, `selector` or `text` |
| `browser_mouse` | `mouse`: raw input at a point (a canvas, a map, a game), or from an element's corner |
| `browser_wait_for`, `browser_evaluate`, `browser_screenshot` (returns the image), `browser_tabs`, `browser_history`, `browser_handle_dialog` | as the CLI |
| `browser_console_messages`, `browser_network_requests` | `console`, `network` |
| `browser_resize` | `set viewport` |
| `browser_handoff` | `show` / `hide`: the browser on the person's screen with the same tabs and sign-ins (a login, 2FA, a CAPTCHA), and back |
| `browser_run` | several ops in one call (`[{"op":"fill","ref":21,"value":"Ada"}, {"op":"click","ref":30}]`), stopping at the first failure, ending with a snapshot |
| `browser_close` | close the browser; the next call starts a fresh one |

Every action takes `snapshot: true` to return the updated tree in the same
call. The whole tool list (23 tools) is about 14 KB (~3,600 tokens), against
~20 KB for Playwright MCP 0.0.83 and ~26 KB for Chrome DevTools MCP 1.10.1.

Client configuration (all run the same command):

```jsonc
// Cline (cline_mcp_settings.json), Roo Code (.roo/mcp.json), Claude Desktop, Cursor
{ "mcpServers": { "navigera": { "command": "navigera", "args": ["mcp"] } } }

// Kilo Code (kilo.json)
{ "mcp": { "navigera": { "type": "local", "command": ["navigera", "mcp"] } } }

// VS Code (.vscode/mcp.json)
{ "servers": { "navigera": { "type": "stdio", "command": "navigera", "args": ["mcp"] } } }
```

```yaml
# Continue (config.yaml)
mcpServers:
  - name: navigera
    command: navigera
    args: [mcp]
```

From a shell: `cline mcp install navigera -- navigera mcp`, `claude mcp add navigera -- navigera mcp`.
Without a local install, use `"command": "npx", "args": ["-y", "navigera", "mcp"]`
(the npm package carries the binary). `server.json` describes it for the
MCP Registry, as `io.github.andrey-usa/navigera`.

## Browsers and engines

- **Chrome-family (default).** navigera looks for a browser in this
  order:
  1. `--chromium`;
  2. `$NAVIGERA_CHROMIUM`, `$CHROME_BIN`;
  3. system Chrome, Chromium or Edge;
  4. Playwright and Puppeteer browser caches. For headless runs the cache
     search prefers chrome-headless-shell.

  Brave and Edge work via `--chromium`. chrome-headless-shell starts
  faster than full Chrome: it has no browser UI to bring up.
- **CDP transport.** `--remote-debugging-pipe` by default on Linux/macOS:
  no TCP port, and the browser exits when navigera dies. `--transport ws`
  (or `$NAVIGERA_CDP_TRANSPORT=ws`) opens a DevTools WebSocket port instead, for
  attaching other tools. Windows always uses `ws`; Chrome runs in a
  kill-on-close job object there, so it still exits with navigera.
- **Windows sessions.** The session file holds `127.0.0.1:<port> <token>`;
  the server listens on that loopback port and drops any client whose first
  line isn't the token (a web page posting to localhost can't drive it).
  `start` detaches the server from the console, the caller's job and its
  stdio handles, so the shell that ran `start` returns at once. It returns
  once the browser is up (no need to sleep after it); the first Chrome
  launch on a fresh Windows machine takes 4–6 s, later ones under 1 s.
- **A session that stopped.** A client call to a session that isn't running
  quotes the end of its server's log (`<socket>.log`, kept after the server
  exits), which says why it stopped: `idle for 1800s`, a browser crash, or
  `quit`.
- **`--engine lightpanda`** (experimental; tested on Lightpanda 1.0.1 and the
  nightly) starts `lightpanda serve` with iframes and stylesheets loaded and
  telemetry off (`LIGHTPANDA_DISABLE_TELEMETRY`). The binary comes from
  `$LIGHTPANDA_BIN` or `PATH`, or from `--chromium`; no X server is needed.
  `goto` waits for load as on Chrome. Differences from Chrome:
  - one tab: a page opened with `target=_blank` or `window.open` is loaded in
    the current tab, and the response says so in `notes`; `tab-new` fails.
  - dialogs are answered from a response armed before they open: one per
    dialog, re-armed after each, so a script that opens two dialogs in a row
    gets the policy for the first and a dismissal for the second (`dialogs`
    in the response reports what the page got).
  - no layout: clicks go straight to DOM events where the hit test can't
    work (inside shadow roots), without the settle wait and without the
    "covered" warning.
  - shadow roots: the engine's accessibility tree stops at one and gives the
    nodes inside no ids, so `ax` lists what an open shadow root holds after
    the tree, under `- shadow root of <tag>`, without refs; target those
    elements with `--text`.
  - password fields: the engine reports `*****` for every one, filled or
    not; `ax` shows a dot per character, as on Chrome, and no value for an
    empty field.
  - infinite scroll: with no layout, an `IntersectionObserver` sees every
    target as on screen, so a page that loads more as you scroll loads all
    of it (60 products where Chrome shows 12), and `ax` is that much longer.
  - `status` reports the engine of a running session; `start` on a session
    that runs the other engine fails instead of reusing it.

**Your own browser: `--attach` and `--profile`** (with `start`, or `serve`):

| | what it connects to | closes the browser? |
|---|---|---|
| `--attach` / `--attach chrome` | your running Chrome (also `beta`, `dev`, `canary`, `chromium`, `edge`): Chrome 144+ serves remote debugging for your default profile once you turn it on at `chrome://inspect/#remote-debugging`, and asks you to Allow each connection. navigera reads the endpoint from `DevToolsActivePort` in that browser's user data directory | never |
| `--attach <dir>` | a Chrome started with `--remote-debugging-port` and `--user-data-dir=<dir>` | never |
| `--attach ws://…` / `http://host:port` / `<port>` | that DevTools endpoint | never |
| `--attach ws://host:port/devtools/page/<id>` | that one tab, worked in as it is (no window of its own): a tab another program left open for a look, like a paused test run | never |
| `--profile <name> --from <browser>` | the same, its profile first replaced with a copy of one of your browser's profiles (`chrome`, `beta`, `dev`, `canary`, `chromium`, `edge`, or a user data directory; `--from-profile` picks one by directory, name or email, default the one used last): cookies (HttpOnly too), Local Storage, IndexedDB, saved logins and autofill, bookmarks, preferences, and `Local State` with the cookie key. Not caches, history, tabs, sync or extensions. The key stays where Chrome keeps it (Keychain, DPAPI, the desktop keyring), so the copy opens for the same user on the same machine. Refused while that profile's browser runs, and for a directory that is not a browser profile. On Windows close Chrome for the copy; it locks its cookie database | only with `quit --close-browser` |
| `--profile <name>` | a visible Chrome on its own persistent profile (`%LOCALAPPDATA%\navigera\profiles\<name>`, `~/Library/Application Support/navigera/profiles/<name>`, `~/.local/share/navigera/profiles/<name>`; a path works too), started when it isn't running and reused when it is. Sign in once; cookies, settings and extensions stay. `--headless` hides it | only with `quit --close-browser` |

The agent gets a window of its own; your tabs are never touched, and only
popups from navigera's own tabs are adopted. `quit` disconnects and leaves
every window open. Because Chrome asks to Allow every new connection, these
modes need a session (one connection for all commands): a one-shot command
with `--attach` is refused. Chrome 136–143 can't serve its default profile
at all (the `--remote-debugging-port` switch is ignored for it, and the
inspect toggle arrives in 144): use `--profile` there, which works on any
version.

**Natural mode: `--natural`** (or `NAVIGERA_NATURAL=1`). What headless
Chrome shows a page that a windowed one doesn't, removed: the user agent
says `HeadlessChrome` in `navigator.userAgent`, in workers and in every
request header; it becomes the one the same Chrome sends with a window (its
major version, the reduced form Chrome sends). The screen is 800x600 behind
a 1440x900 window; it becomes 1920x1080. The full Chrome is used, not
chrome-headless-shell, which is a different program; `--natural` with a
chrome-headless-shell given (`--chromium`, `$CHROME_BIN`) is refused, since
its client hints, plugins and `window.chrome` say headless whatever the
flags. In every mode,
`navigator.webdriver` is false, there is no `--enable-automation`, and a
`--profile` browser starts with no automation flag. The CI check is
`tests/handoff.rs` against `tests/fixtures/natural_site.py`, a page that runs
the checks anti-bot scripts make: webdriver, user agent and client hints
(page, worker, header), `Runtime.enable` side effects, plugins, languages,
`window.chrome`, window and screen sizes, notification permissions. What
natural mode does not do: solve CAPTCHAs, or fake a fingerprint (WebGL on a
machine without a GPU still reports a software renderer).

**Diagnostics:**

- `NAVIGERA_TIMINGS=1` prints launch phases at shutdown.
- `NAVIGERA_CDP_TRACE=<file>` records every CDP command sent; CI checks it with
  `tools/cdp_check.py`.
- `NAVIGERA_VERBOSE=1` turns on launch logs.

## Playwright CLI / agent-browser → navigera

| playwright-cli | agent-browser | navigera |
|---|---|---|
| `open <url>` / `goto <url>` | `open <url>` | `start` + `goto <url>` |
| `snapshot` | `snapshot` | `ax` (or `snapshot`) |
| `click e15` | `click @e2` | `click 15` (also `@e15`, `e15`) |
| `fill e3 "x"` | `fill @e3 "x"` | `fill 3 "x"` |
| `type`, `press Enter`, `select`, `hover` | same | same |
| `eval "document.title"` | `eval "document.title"` | `eval "document.title"` |
| `tab-new`, `tab-list`, `tab-select`, `tab-close` | `tab new`, `tab`, … | `tab-new`, `tab-list`, `tab-select`, `tab-close` |
| `close` | `close` | `quit` (or `close`) |
| `-s=name` | `--session name` | `-s name` / `-s=name` |
| `--raw` | `--json` (default text) | `--raw` (default JSON) |
