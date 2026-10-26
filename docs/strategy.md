# Becoming the browser agents use

Internal notes, not user documentation: where navigera stands among the
browser tools open-source IDE agents use, what was measured, and what would
make it the default. Keep this file out of the public repository.

## Where agents get a browser today

| agent | how it browses | what that means for navigera |
|---|---|---|
| Cline (VS Code, CLI 3.0.70) | a built-in `browser_action` on Puppeteer that works from **screenshots** and needs a vision model; any MCP server | vision-only is slow, costly and fails on text-only models. navigera reaches it by MCP or by its shell tool plus the skill |
| Roo Code | removed its browser tool (inactive since May 2026); MCP only | the gap is open: whoever is in its MCP lists is its browser |
| Kilo Code (CLI 7.8.8, OpenCode-based) | MCP (`kilo.json`), shell, skills in `.agents/skills` | both paths work today |
| Continue (`cn` 1.5.47) | MCP (`config.yaml`), skills in `.continue/skills` / `.claude/skills` | both paths work; `install-skill --continue` |
| VS Code agent mode, Cursor, Claude Desktop | MCP | MCP only |
| VSCodium | the same extensions from Open VSX; no Copilot | identical to VS Code through the extensions above |

The common denominator is MCP, and the competitor in every MCP list is
**Playwright MCP**, with Chrome DevTools MCP second.

## What we measured

`bench/ide-eval` runs Cline, Kilo and Continue on the same task with three
browsers, driven by a scripted OpenAI-compatible model (`mock_llm.py`), so
the agent, the task and the model's choices are fixed and only the browser
changes. CI (run 38106699446) gates on it:

| agent | navigera (shell + skill) | navigera MCP | Playwright MCP 0.0.83 |
|---|---|---|---|
| Cline | 2 calls, 12.5k tokens | 3 calls, 25.8k | 5 calls, 54.0k |
| Kilo | 2 calls, 33.7k | 3 calls, 58.3k | 5 calls, 109.3k |
| Continue | 2 calls | 3 calls | 5 calls |

- **Turns:** Playwright MCP needs extra calls because its snapshot right
  after `navigate` still shows a single-page app's "Loading…" placeholder;
  navigera's snapshot waits briefly for data the page is still fetching.
- **Tool list:** ~10 KB (2,600 tokens) against 20 KB for Playwright MCP and
  26 KB for Chrome DevTools MCP. Every request carries it.
- **Without vision:** everything above works with text-only models, which
  Cline's built-in browser cannot use.
- **Not yet measured with real models:** the `gemini` leg of `ide-eval`
  needs `GEMINI_API_KEY` in the repository's `main` environment.

## What makes a browser the default

1. **Be where the agent looks.** Agents install what their MCP lists show.
   - The official MCP Registry: `server.json` is in the repo and the release
     workflow publishes it (`PUBLISH_MCP_REGISTRY`), with the npm package's
     `mcpName` matching.
   - Cline's MCP Marketplace: a submission issue in `cline/mcp-marketplace`
     with a 400×400 logo and the README; Cline installs from it in one
     click.
   - Smithery, Glama, PulseMCP, mcp.so: they index the Registry and GitHub;
     check each lists navigera after the first Registry publish.
   - Continue Hub: an MCP block (`hub.continue.dev`).
   - One-line install everywhere: `npx -y navigera mcp` needs no install.
2. **Win the comparison people will run.** The first thing a user does is
   try a known task in Playwright MCP and in navigera. Publish the
   `ide-eval` numbers with real models (Gemini Flash-Lite as the cheap model,
   one frontier model), per agent, refreshed by CI weekly, on the README.
3. **Be the cheapest correct path for small models.** Most IDE-agent users
   run cheaper models for long tasks. The tree with refs, the late-data wait
   and `browser_run` (several steps in one call) already favor them; `find`
   and `check` (a System One model taking element choice and verification,
   `docs/multi-model.md`) are the step no competitor has: the agent's LLM
   reads 121× less to choose an element on real pages.
4. **Never be the reason a task failed.** The categories where agents fail
   in browsers are known, and each needs a test in the bench site before a
   competitor's issue tracker shows it: late rendering, overlays and cookie
   banners, iframes and shadow DOM, new tabs, dialogs, uploads, downloads,
   infinite scroll, logins. navigera covers all but downloads.
5. **Be safe to leave running.** Sandbox on, private sessions, verified
   installs, file access limited in MCP, page text treated as data
   (`SECURITY.md`). Security reviewers at companies decide which MCP server
   is allowed; being the one with a written threat model is an advantage.

## Gaps against the competition

| gap | who has it | proposal |
|---|---|---|
| downloads (save a file the page offers) | Playwright MCP | `download` op and an MCP tool: wait for the download, return its path |
| vision fallback for canvas-heavy pages | Playwright MCP (`--caps vision`), Cline built-in | `screenshot --marks`: the screenshot with each ref's number drawn on its element, for models that can see |
| network inspection (API calls, failed requests) | Chrome DevTools MCP | `network` op: recent requests with status, type and size; lets agents read an SPA's JSON instead of its DOM |
| console messages | both | `console` op: recent console errors and warnings |
| PDF of a page | Playwright MCP | low priority |
| performance traces | Chrome DevTools MCP | not our focus |
| remote/cloud browsers (Browserbase, Steel, Browserless) | Playwright MCP via CDP endpoint | `--attach ws://…` speaks CDP already, but providers hand out `wss://` URLs and the WebSocket client has no TLS: add rustls behind a feature (or a local relay), then test against one provider |

## Next steps

| | who |
|---|---|
| merge the open PRs (sandbox fallback, `find`/`check`, distribution) | you |
| add `GEMINI_API_KEY` (and `TYPESAFE_API_KEY` for the `find` eval) to the `main` environment of navigera-private | you |
| set `PUBLISH_MCP_REGISTRY=true` before the next release; submit to Cline's marketplace (needs a logo) | you |
| run `ide-eval` with Gemini, then the `find`/`check` eval; put the numbers in the README | me, once the keys are in |
| `download`, `console` and `network` ops; `screenshot --marks` | me |
| `wait --until "<question>"` and snapshot pruning (`docs/multi-model.md`) | me, after the `find` eval says the model chooses well |
