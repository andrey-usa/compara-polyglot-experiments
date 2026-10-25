# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- A `goto` to a server that takes the connection and never answers fails
  at its timeout and cancels that navigation: the tab answers at once again
  (it had answered nothing while the dead load was pending, so the next
  `url` or `ax`, and a test runner's failure screenshot, each waited their
  whole timeout too: one failed step cost 105 s).
- `get box` of an element inside an iframe is in top-level viewport pixels
  (each frame's offset added), as `mouse` takes them and Playwright's
  `boundingBox()` reports; it was relative to the element's own frame.
- `--natural` with chrome-headless-shell as the browser (`--chromium`,
  `$CHROME_BIN`) is refused with a message instead of starting a browser
  that still tells pages it is headless.

## [0.6.0] - 2026-10-24

The browser handed over to a person and back, downloads, frames of any
origin and custom dropdowns; clicks that wait for what moves or covers
their element and say when they changed nothing; and `reset` and `fork`,
so a test runner gets a clean, separate session in about 40 ms.

### Added

- Element ops reach into same-origin iframes, framesets and `srcdoc`
  frames, and `--selector` into open shadow roots, when the page itself
  has no match (role/name and text locators already reached shadow roots).
  `wait --selector/--text/--gone`, `get count` and `text` look there too.
- `--wait-ms <ms>` (on `start`, `serve`, `mcp` or one command;
  `NAVIGERA_WAIT_MS`): how long element ops wait for their element when they
  name no timeout. A test suite against a slow app sets it once.
- Downloads: files a page downloads are saved under `--download-dir`
  (`NAVIGERA_DOWNLOAD_DIR`; default a directory of the session's own) with
  the name the page gave them. `downloads` lists them; `wait --download
  "*.csv"` waits for one and returns its path. `downloads --dir <path>`
  gives one session a directory of its own (kept across `reset`; forks
  and other sessions keep theirs), so a test runner can give each test
  one and file names don't carry over (`report (2).csv`) between tests.
- `click --button right|middle` (a native context-menu click), `click
  --force` (DOM events on the element, covered or not), `select --add`
  (add to a multiple select's choice), `tab-select <target id>`,
  `network --since <n>`.
- `select` on custom dropdowns (ARIA comboboxes such as react-select): it
  opens them the way a user does (ArrowDown, else a click), finds the
  option in the listbox that opened (portals included), types to filter
  when it isn't listed, and clicks it.
- `fill --keys`: clear the field and type the value key by key, for forms
  whose app keeps only what was typed.
- `scroll --to top|bottom` with a target scrolls inside it.
- Selectors, text and locators reach into cross-origin iframes (a payment
  provider's card form, a sign-in frame) when the page itself has no match:
  navigera looks there from an isolated world of its own, which the page
  can't see. A single `wait --text`/`--selector` does too. With `--attach`,
  frames in another process stay ref-only.
- Library: `Driver::fork` (and `BrowserSession::fork`), another driver on
  the same browser in a browser context of its own, movable to another
  thread: several sessions at once without a browser each (a test runner's
  workers). Each takes only the tabs its own tabs open, and routes,
  credentials, the dialog answer and the downloads list are each
  session's own.
- `reset`: a clean slate in the same browser, a new browser context (no
  cookies, storage or cache from before) with one blank tab, in about 40 ms
  instead of a new browser's few hundred. For test runners that isolate
  scenarios, and agents that want to start over signed out. `set`, routes,
  init scripts and the dialog answer stay; `reset --all` drops them too.
- `--frame <css>` on element targets and `wait`: look only inside that
  iframe or frame (`"#outer >> #inner"` for nested ones), of this origin
  or another, when two frames hold the same element (the first frame the
  CSS matches; when it matches none, the error says so). Without it,
  frames are searched as before.
- `mouse move|down|up|click|wheel`: raw mouse input at a point, for what
  has no element to aim at (a canvas, a map, a game): viewport CSS pixels,
  or from a target's top-left corner (`mouse click 30 40 --selector
  canvas`). The pointer stays where it went and held buttons stay held, so
  `mouse down`, moves (`--steps` for a smooth path), `mouse up` drags or
  draws; `--count 2` double-clicks; `--button right|middle`. A target
  out of view is scrolled into view first, as for a click; `down`, `up`
  and `wheel` with a target go to its center; a click outside the
  viewport is refused instead of landing on nothing. MCP:
  `browser_mouse`. (An agent working through WebGames couldn't play its
  canvas games without it.)
- `--attach ws://host:port/devtools/page/<id>`: work in that one tab of a
  running browser as it is, not in a window of its own (a tab a paused
  test run left open for a look). `BrowserSession::page_endpoint` gives a
  tab's.
- A click that changes nothing says so (`no_effect` in its result): no DOM
  change, focus, form value, URL, scroll, clipboard write, request, dialog,
  tab or download within 200 ms. Requests that are the page reporting the
  click (beacons, analytics services, first-party `/event/…`-style paths
  whose answer changes nothing) don't count, and the note names them;
  nor does what the browser or page loads on its own meanwhile (the tab's
  icon, lazy images, fonts).
  Typically a button the page hasn't wired up yet. A click that does
  something costs nothing extra; one whose effect comes a moment later now
  returns once it has.

- `start --profile <name> --from chrome`: the agent's own browser, seeded
  with a copy of one of your Chrome profiles' sign-ins -- cookies (HttpOnly
  too), Local Storage, IndexedDB, saved logins -- so it starts signed in
  without touching your running browser, on any Chrome version (attaching
  needs 144+). `--from` takes `chrome`, `beta`, `dev`, `canary`,
  `chromium`, `edge` or a user data directory; `--from-profile` picks a
  profile by directory, name or email. The copy replaces only a profile
  navigera made, and is refused while that profile's browser runs.
- `show` / `hide` (MCP: `browser_handoff`): the session's browser on the
  person's screen with the same tabs and sign-ins, for a step only they can
  do -- a login, a 2FA prompt, a CAPTCHA -- and back off it. A `--profile`
  browser restarts on its own profile; one navigera launched hands its
  cookies and storage to its replacement.
- `navigera profiles`: the browsers and profiles a session can be handed
  (name, signed-in email, whether `--attach` reaches the browser now), and
  navigera's own `--profile` browsers.
- `--natural` (`NAVIGERA_NATURAL=1`): a headless browser that shows pages no
  sign of being headless -- the user agent the same Chrome sends with a
  window (page, workers, request headers) and a real screen size -- checked
  in CI against a page that runs the checks anti-bot scripts make.

### Changed

- A click that may navigate waits for this tab's navigation only when one
  can come: not for a `download` link, a link or form that opens another
  tab or window, or a link or submit inside a frame (which navigates the
  frame), and the wait ends as soon as the click opened a dialog, a tab or
  a download instead. Each of those clicks had cost 150-200 ms (a
  download link 170 ms, now about 20).

- A click waits for its element to stop moving: CSS animations and
  transitions on it or an ancestor (a modal sliding in) finish first, since
  pages often wire a dialog's buttons only once it has opened. Pages with
  nothing animating pay nothing for this.
- A script sent to a page that a navigation then replaced no longer waits
  out the whole timeout when the old page never answers (seen on macOS):
  a second after the new page commits, the op looks again there.
- `--text` prefers an exact match inside a cross-origin frame (the "Pay"
  button of a payment frame) over page text that merely contains it (a
  "Payment details" heading), and never picks an iframe by its title.
- `--text` matches what an element shows as well as what names it: a
  button showing "Add to cart" whose `aria-label` is "Add Carbon Gear to
  cart" is the match, not the product card around it (a click on the card
  did nothing; `no_effect` caught it in the benchmark).
- A click on an element that stays covered by another fails and names the
  covering element, instead of sending DOM events to an element no user
  could click. The element is first tried at other scroll positions, which
  is what uncovers it from a sticky header; `fill` does the same before
  typing into a field under a panel.
- `select` matches option labels with whitespace collapsed (non-breaking
  spaces count as spaces).
- `type` with a target fails when the target doesn't take focus, instead of
  typing into whatever has it.
- A disabled control is waited for (a Submit that unlocks once the form is
  valid) and otherwise refused: `click` and `fill` no longer report
  success on a disabled button, or a disabled or read-only field.
- When a selector's first match is out of a user's reach (a decoy copy of
  an id parked off the page) and another match isn't, the other one is
  used.
- A click on an element a user can't see where it is (zero size, clipped,
  the hidden input of a styled radio card) fails at once and names what
  stands for it, instead of waiting and then sending DOM events.
- An element op whose page is replaced by a navigation while it looks for
  its element (a step that saves with a request and then reloads) looks
  again on the new page instead of failing with "Inspected target
  navigated or closed".
- "not found" errors name the wait asked for (5000 ms, not 4750), and page
  errors drop the redundant `Error:` prefix.

- A `--profile` browser starts with `navigator.webdriver` false, as
  navigera's own launches always have, and `quit --close-browser` returns
  once the profile is free, so a `start` right after it finds no browser on
  its way out.

- With a decision model on, a plain `ax` of a tree over 16 KB
  (`NAVIGERA_AX_PREVIEW_BYTES`) shows its first 40 lines and how many more
  there are, with a note that `--find` and
  `check` act on the whole page without reading it and that `ax --limit
  <n>` reads it all. The note alone came after the whole tree, once the
  agent had read it: in the 0.5.0 eval the agent reached for `--find` or
  `check` unasked in 3 of 12 runs. `ax` with a limit, a depth, `--all` or a
  scope is unchanged, and so is `ax` without a decider.

### Fixed

- A page that stops answering (a script holding its thread, a renderer
  hung under load) cost one full command timeout per command after it:
  a click's press timed out, then its clean-up and checks each waited the
  whole 35 s again (105 s for one click on a loaded macOS runner). Now,
  once a command on a tab has gone unanswered, the next ones wait 3 s
  until the tab answers again: the click fails about 35 s in and says it
  timed out, and the next op fails in seconds rather than another 35.
- A session's only tab closing (the user closed it in an `--attach`ed
  browser, or the page closed itself) left every op failing with Chrome's
  "Session with given id not found", while `status` and `tab-list` still
  showed the tab. Now ops say the tab is closed and that `tab-new`
  opens another; `status` and the tab in `tab-list` carry `closed: true`;
  and `tab-new` (or `reset`) replaces the closed tab instead of listing
  it beside the new one. An op still waiting on a tab when it closes (an
  `eval` awaiting a promise, a `goto` waiting for a slow page) ends then
  and says so, instead of after its 35 s timeout; with other tabs open,
  the session goes on in one of them.
- `--attach <port>` and `--attach http://host:port` sent Chrome's
  `/json/version` request without the port in `Host`, got back a WebSocket
  URL without one, and failed to connect.
- `goto` and `reload` try once more when Chrome answers "Not attached to
  an active page" (the tab's target was swapped mid-navigation), instead of
  failing the step.
- `goto` no longer retries a navigation that ended in `ERR_ABORTED`
  because it was an answer: a URL that downloads a file was fetched four
  times (four copies on disk); now the file is saved once and `goto`
  succeeds with `download: <name>`, the page staying where it was. A 204 or
  205 answer fails at once and says so, instead of after three retries.
- `goto` and `reload` no longer wait out their whole timeout when the
  load event is lost on its way (seen as rare 35 s stalls on slow macOS
  machines): every 2 s they also look at the page itself, and return once
  its document is the new one and has loaded.

## [0.5.0] - 2026-10-22

Lightpanda 1.0 as a second engine, the inspection and state commands agents
reach for in Playwright, and a decision model the agent is told how to use.

### Added

- Lightpanda 1.0 (`--engine lightpanda`, or `NAVIGERA_ENGINE=lightpanda` for
  every command of a shell): the same commands and `ax` tree as on Chrome,
  with the engine's gaps covered (see Fixed). CI runs the end-to-end suite
  on Lightpanda 1.0.1 and the nightly.
- `drag <from> <to>`: HTML5 drag and drop for a `draggable` source (the drag
  events with one DataTransfer, since headless Chrome will not start a native
  drag from synthetic input, and on Lightpanda too), a held mouse moved
  across for anything else; the result says whether the drop was accepted.
- `state save|load <file>`: every cookie and the open tabs' localStorage to
  a file and back, in Playwright's `storageState` format, so a signed-in
  session can be reused, by navigera or Playwright.
- `status`: the session's engine, browser executable, CDP endpoint and tab
  state.
- `console`, `errors`, `network` (one request in full with `network <n>`,
  HAR with `--har`): what the page logged and fetched; an uncaught exception
  during a command is reported in its `notes`.
- `cookies` (list/set/delete/clear), `storage` (local/session), `pdf`.
- `set viewport|device|media|geo|offline|headers|ua|locale|timezone|credentials`:
  emulation for every tab, replayed on new tabs.
- `route` / `unroute`: fail or answer requests by URL glob.
- `init-script`: code that runs before the page's own in every document.
- Locators on every element command: `--role/--name`, `--label`,
  `--placeholder`, `--testid`, `--nth`.
- `get`, `is`, `dblclick`, `focus`, `check`/`uncheck <target>`,
  `keydown`/`keyup`, `insert-text`, `wait --idle`.
- MCP: `browser_console_messages`, `browser_network_requests`,
  `browser_resize`.
- `notes` in responses: what the engine did differently from what was asked
  (e.g. a Lightpanda popup loaded in the current tab).
- Agent eval: any OpenAI-compatible model as the agent (`--agent openai`,
  e.g. DeepSeek-V4.1-Flash through SiliconFlow) on the same tasks, prompt,
  turn limit and timeout as Gemini CLI; the decision benchmark takes such a
  model as its baseline; an OpenRouter head-to-head of tool variants
  (`openrouter-eval.yml`); a WebArena-Verified pilot (`bench/webarena`,
  `webarena-eval.yml`).

### Changed

- With a decision model on, the agent is told how to use it where it
  decides what to do next: `start` adds a note (act with `--find`, verify
  with `check`, read `ax` only for text to report), a tree over 60 lines
  ends with a one-line note that acting needs no reading, and the skill's
  main loop shows the decider form next to the ax form. Before, `start` said
  only `"decider": "<model>"` and the usage sat in a later skill section: in
  eval run 39437548538 the agent saw the decider in 8 of 10 runs and used
  it in none; in run 39561380436 it used it in 3 of 15 unprompted.
- Agent eval: a `preferences` task (a 170-line settings form, four named
  changes, a confirmation to check; the rest must stay as it was), the task
  the decider is measured on, and the three-arm protocol for measuring it
  (`navigera`, `navigera+jev`, `navigera+jev-told`, one model key each).
- `start` on a running session with a different engine fails instead of
  silently reusing it.

### Fixed

- `reload` and `goto` try again after a moment when Chrome answers "Not
  attached to an active page" (the tab between two documents; nothing was
  done yet): seen once in a parallel test run. `tab-select` reads 4 or
  more hex characters as a target id even when they are all digits.
- A fork's `tab-new` opened its tab in the default browser context, with
  the owner's cookies and storage; it opens in the fork's own now. A
  download started by a subframe is listed for the session whose tab holds
  it (it went to the default list); `reset --all` no longer reports a
  dialog answered before it; a closed fork's routes, answers and downloads
  are let go.
- Sessions on one browser (`Driver::fork`) no longer share routes, HTTP
  credentials, the dialog answer or the download list: each is the
  session's own, keyed by its browser context. One worker's `route` mocked
  the others' requests, and `unroute` removed theirs.
- `--attach <port>` and `--attach http://host:port`: the DevTools URL asked
  for carries the port again (Chrome answered with a WebSocket URL without
  one, and the connection was refused).
- Lightpanda 1.0.x: `goto` waits for the load event (it returned at commit
  and gave up after 500 ms); confirm()/prompt() get the dialog policy (they
  returned false/null while reported accepted); `target=_blank` pages are
  followed in the one tab; iframes and stylesheets load; table cells keep
  their text in `ax`; shadow-DOM clicks no longer wait 3 s; no `xvfb-run`
  wrapper; telemetry off through `LIGHTPANDA_DISABLE_TELEMETRY`.
- Dialogs, popups and JS navigations reported just before a command's
  response are no longer missed by the op that caused them.
- Lightpanda: a navigation a click handler starts (`location.href = ...`)
  is waited for. The engine reports it only after the click's reply, so the
  click returned the old page with `loading: true`; it now returns the page
  the click led to.
- Lightpanda: `ax` shows what open shadow roots hold. The engine's tree
  stops at a shadow root and gives the nodes inside no ids, so a coupon
  field in a web component was invisible to an agent reading the snapshot;
  it is now listed under the root, without refs, with `--text` as the way to
  target it.
- Lightpanda: a settings form reads in `ax` as it does on Chrome. A
  checkbox showed its default value `"on"`, which reads like a state (in the
  DeepSeek eval, run 39530836547, both Lightpanda runs of the preferences
  task left a box unticked and failed); a `<select>` inside a `<label>` was
  named with every option ("Currency USD EUR GBP …") next to a copy of the
  label; an unnamed table cell took two lines per checkbox. The preferences
  page is 177 lines on Lightpanda now, 243 before, 169 on Chrome.
- Lightpanda: a checkbox or radio shows in `ax` the state a click left it
  in. The engine's tree keeps the `checked` attribute the page loaded with,
  so a box an agent had ticked read as unticked; in the DeepSeek eval (run
  39691526622) an agent clicked one back that way and ran out of turns.
- Lightpanda: an empty password field no longer reads as filled in `ax`.
  The engine gives every password field the value `*****`; the snapshot now
  asks the field and shows a dot per character, or nothing, as on Chrome.

## [0.4.1] - 2026-10-14

### Added

- `find <description>`, `check <question>` and `--find <description>` on
  element commands: element choice and yes/no questions about the page,
  answered by a System One decision model (TypeSafe Jev) when
  `TYPESAFE_API_KEY` or `NAVIGERA_DECIDER_URL` is set. MCP gains
  `browser_find`, `browser_check` and a `find` target in that case only.
  Typed values are redacted from what is sent. `start` reports `"decider"`
  with the model id, so an agent can tell without reading the key.
- `server.json` for the MCP Registry (`io.github.andrey-usa/navigera`; the npm
  package carries the matching `mcpName`), published by the release workflow
  when `PUBLISH_MCP_REGISTRY` is set.
- `install-skill --continue` (`.continue/skills`); `--global` finds the home
  directory on Windows too.
- `navigera mcp`: a Model Context Protocol server on stdio for IDE agents
  (Cline, Roo Code, Kilo Code, Continue, VS Code, Claude Desktop, Cursor).
  18 tools named after Playwright MCP's, `snapshot: true` on every action,
  `browser_run` for several steps in one call; the browser starts on the first
  tool call.

### Security

- Chrome's renderer sandbox is on by default. `--no-sandbox` is added only as
  root, with `NAVIGERA_NO_SANDBOX=1`, or when Chrome reports that the machine
  has no usable sandbox (with a warning on stderr). That last case is
  remembered per browser binary until it changes or the machine reboots, so
  only the first launch pays for the failed attempt (about 350 ms with Chrome
  for Testing on a GitHub runner).
- Named sessions live in a private per-user directory
  (`$XDG_RUNTIME_DIR/navigera` or `$TMPDIR/navigera-<uid>`, mode 0700) instead
  of `/tmp/navigera-<name>.sock`, so another user can't squat a session's
  socket; the log is 0600 and doesn't follow symlinks.
- `install.sh` and `install.ps1` verify the release's SHA-256 checksum.
- The skill tells agents to treat page text as data, not instructions.

## [0.4.0] - 2026-10-09

First public release.

### Added

- Named sessions: a warm browser behind a local socket, one shell command per
  step (`navigera -s <name> start`, then `goto`, `ax`, `click`, …, `quit`).
- `ax`: the page as a compact accessibility tree with `[ref=N]` handles,
  including iframes (cross-origin too) and open shadow DOM; links carry their
  `url`.
- Element commands by ref, CSS selector or visible text, with auto-wait;
  trusted mouse and keyboard input; tabs, dialogs, popups, uploads,
  screenshots.
- `serve`: a JSON-lines protocol on stdin/stdout; a line holding a JSON array
  runs as a batch.
- `eval --file <path>` and `eval -` (script from stdin).
- `--attach` (your running Chrome 144+) and `--profile` (a persistent
  navigera browser).
- An agent skill compiled into the binary (`navigera skill`,
  `navigera install-skill`).
- Prebuilt binaries for Linux (x86_64, aarch64; static), macOS (arm64,
  x86_64) and Windows (x64); install scripts for sh and PowerShell; packages
  for crates.io, npm, PyPI, Homebrew and Scoop.

[0.6.0]: https://github.com/andrey-usa/navigera/releases/tag/v0.6.0
[0.5.0]: https://github.com/andrey-usa/navigera/releases/tag/v0.5.0
[0.4.1]: https://github.com/andrey-usa/navigera/releases/tag/v0.4.1
[0.4.0]: https://github.com/andrey-usa/navigera/releases/tag/v0.4.0
