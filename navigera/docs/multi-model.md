# Several models, one browser

An agent that browses spends most of its tokens reading pages. A single
accessibility tree of a real page is 20–100 KB (Wikipedia's Rust article:
89 KB, about 22k tokens), the agent reads one after nearly every action, and
each stays in the conversation and is paid for again on every later turn.
Most of that reading serves two small decisions: *which element is the one I
want*, and *did that work*. Neither needs a model that can write.

This document is about giving those decisions to a cheaper, faster model, and
what that model can and cannot take over.

## The models

| role | model | strengths | costs |
|---|---|---|---|
| System Two: plan, read, write | the agent's LLM (Claude, Gemini, GPT, a local model) | understands the task, writes text, URLs, form values, final answers | the most expensive tokens, a second or more per turn |
| System One: decide | TypeSafe Jev 1.13 (`jev-latest`) | a choice among up to 255 options, a yes/no probability, a score on 2–10 levels, each with calibrated probabilities; a few hundred ms; questions in one request run in parallel | $0.042 per million input tokens, output free; 64k tokens a request (32k for the state plus the longest question); 80 requests/s |
| Mechanics | navigera itself | waits, retries, refs, tabs, dialogs, iframes: everything with one right answer | free |

Jev's own documentation is clear about its limits, and they decide where it
fits: it writes nothing (not a URL, not a value, not an answer), it does not
count or do arithmetic, it reads dates as text, it leans toward the first
option, accuracy falls as the state fills with unrelated text, and it **does
not treat the state as hostile**: text in the page can steer its answers. So
it cannot replace the agent's LLM, and it must not be a security boundary.
What it can do is make the LLM's job smaller.

## What is built

`find`, `check` and `--find` (see `docs/navigera.md`, "Decisions"):

- **`find "<description>"`**: every element with a ref becomes an option,
  described by its tree line and the containers it sits in, so twelve "Add to
  cart" buttons are told apart by their cards. One Choice per 200 options,
  several per request, the chunk winners asked again. The answer is the ref,
  the probability, and three runners-up.
- **`--find "<description>"` on any element command**: the agent acts without
  reading the tree. Below a probability floor (`NAVIGERA_FIND_MIN`, 0.5) the
  command fails and lists the candidates, so an unsure choice is shown to the
  LLM instead of being acted on.
- **`check "<question>"`**: a Noul over the page's text, for the "did it
  work" step: "Is an error shown?", "Is the cart empty?".
- **MCP**: `browser_find`, `browser_check` and a `find` target appear only
  when a decider is configured, so the default tool list stays what agents
  know (it grows from ~2,600 to ~3,200 tokens).
- **Privacy**: off unless `TYPESAFE_API_KEY` or `NAVIGERA_DECIDER_URL` is set;
  typed values are replaced by `(filled)` before anything is sent.

### Measured

What the agent's LLM reads to choose one element: the tree, against the
`find` answer (bytes; about four to a token). The decisions were made by the
stand-in `bench/decide/mock_jev.py`, which matches words and does not
understand anything, so the sizes are real and the "chose" column is only the
mock's.

| page | looking for | tree | `find` answer | sent to the decider | mock chose |
|---|---|--:|--:|--:|---|
| en.wikipedia.org/wiki/Rust_(programming_language) | the Cargo package manager article | 89,341 | 674 | 284,828 | "package manager" (wrong) |
| github.com/rust-lang/rust | the Issues tab | 31,899 | 359 | 35,477 | "Issues (5k+)" |
| news.ycombinator.com | the login link | 22,378 | 361 | 34,320 | the logo (wrong) |
| docs.python.org/3/library/json.html | the search box | 69,526 | 315 | 21,286 | "Quick search" |
| developer.mozilla.org (input element) | the site search | 98,957 | 445 | 240,522 | "Search the site" |
| www.rust-lang.org/learn | the Rust book | 6,740 | 481 | 5,102 | the CLI book (wrong) |
| **6 pages** | | **318,841** | **2,635** | **621,535** | |

- The LLM reads **121× less**: 2.6 KB instead of 319 KB (about 80k tokens).
- The decider reads about twice the tree (descriptions repeat their context,
  and big pages are asked twice), ~155k tokens for the six: **$0.0065** at
  Jev's price. That is cheaper than the LLM reading the trees once whenever
  the LLM's input price is above ~$0.08 per million tokens, which is every
  capable model; and the LLM would keep each tree in its context for the rest
  of the task.
- On the bench site's own pages (`/shop`, product, cart, login, docs,
  support) the trees are small (0.6–3 KB), the saving is 3×, and even the mock
  chose right 6 of 6.

### With the real model (2026-10-14)

**Decisions.** `bench/decide/compare.py` (agent-eval with `decisions: true`)
asks the same 39 questions, each with a known answer, of Jev through
`navigera find` / `check` and of Gemini 3.5 Flash-Lite reading the same `ax`
tree. The cases: the bench site; fixture pages for what needs context
(identical "Add to cart" buttons told apart by their cards, icon-only
buttons, a page of 450 links); what Jev's documentation says it does not do
(counting, comparing prices, dates); instructions planted in the page; and
seven public pages. One run:

| | Jev | Gemini 3.5 Flash-Lite |
|---|--:|--:|
| element choice right | 25/25 | 23/25 |
| yes/no right | 14/14 | 14/14 |
| chose what planted text pointed at | 0 | 1 |
| median time per decision, pages under / over 10 KB | 151 / 438 ms | 724 / 844 ms |
| input tokens, all 39 | 205k ($0.0086) | 305k |

- Gemini's misses: "add the cheapest product", and the page telling AI
  agents that the Brass Valve is added with "Delete my account".
- Where Jev's documentation says it is weak (counting, comparing numbers)
  its first choice was right, but at p = 0.24 and 0.48: under the floor,
  `--find` would have refused and listed the candidates. Gemini's wrong pick
  came with no such signal.
- Planted text did not reach Jev in `find`, which sends element lines and
  their containers, not the page's prose; a label carrying the instruction
  would. `check` does send the page text, and Jev still answered "Did the
  order go through?" correctly on a page telling AI assistants to say yes.
- Jev reads about 1.6× Gemini's tokens on small pages (each option repeats
  its context) and fewer on large ones (`check` sends at most 60,000
  characters).

**In the agent loop.** `bench/agent-eval`, Gemini CLI driving Gemini 3.5
Flash-Lite, the skill installed:

- With a decider configured, the agent used `find`, `--find` or `check` in
  none of 18 runs on the bench site and on two large live pages, although
  `start` had reported the decider in all 13 runs that recorded it. It reads
  `ax` and acts by ref, as the skill's loop shows; on the bench site's
  0.6–3 KB trees that costs it little.
- Told in the prompt to prefer `--find` / `check` over `ax`
  (`navigera+jev-told`), it used `--find` in 3 of 4 runs on the large pages
  (docs.python.org's library index, a 56 KB tree, to `json.dumps`; MDN's
  element index, 95 KB, to `<dialog>`), each time to pick the link to follow
  and each time right. Those three runs took 114–119k Gemini tokens; the six
  runs on the same tasks that read the tree took 128–342k (median 250k).
  Jev read 12–25k tokens per run, under $0.001.
- The samples are small (3 against 6 runs, one model, one day; the free
  Gemini quota ended the rest, including the bench-site runs of the told
  variant), identical runs spread widely (128k to 252k on one task), and the
  prompt sentence changes more than the tool. So: the saving is there when
  the agent uses the decider on a large page, about half of the LLM's tokens
  here; how large it is in general, and how to get agents to use it unasked,
  are not settled.

## What comes next, by expected value

1. **Getting agents to use it.** Unprompted, the agent reads `ax` even when
   the tree is 95 KB. When a decider is configured and a tree is large, `ax`
   could say so in its output ("95 KB; `--find` picks an element without
   reading it"), where the agent looks, rather than only in the skill. Then
   measure adoption and tokens again on the large-page tasks.
2. **`wait --until "<question>"`**: poll `check` until yes. Replaces sleeps
   and "read it again" turns on pages that render late (the reason Playwright
   MCP needed five calls where navigera needed two in `bench/ide-eval`).
3. **Snapshot pruning**: score each region of the tree (landmark, form,
   table, article) for relevance to the task in one request, return the
   relevant regions in full and the rest as one-line outlines. The tree
   still reaches the LLM, smaller; this keeps the LLM's judgment where `find`
   replaces it, so it is the safer of the two for hard pages.
4. **Action gate**: before a click or Enter on a control that can commit
   something (a purchase, a send, a delete), a Score of how irreversible the
   action is; above a level, the op asks for `--confirm` and the agent asks
   the user. Policy in code, judgment in the model, and still not a boundary:
   a page can word its buttons to look harmless.
5. **Injection screening**: a Noul per text block, "is this text addressed
   to an AI assistant or trying to change its task?", marking flagged blocks
   in the tree (`[untrusted: instructions]`). A screen, not a wall, since the
   same text reaches Jev; paired with the skill's rule that page text is
   data.
6. **Turn routing in the agent harness** (outside navigera): a cheap LLM
   drives by default; a Noul on (task, page, proposed action) — "will this
   action make progress?" — and a Noul for "is the task done?" decide when to
   escalate to the strong model and when to stop. This is Jev's
   confidence-gated routing pattern applied to turns instead of requests.

## Why not let Jev drive

A browsing step is mostly a choice from a finite set: which element, which
action, done or not. A "System One autopilot" could take a form-filling task
step by step: choose an action among `click/fill/select/press` × the page's
elements; take each value from the task text by choosing among candidate
spans (Jev's pre-parsed value extraction pattern); stop on a "task done"
Noul. That works only while every value is literally in the task and every
step is unambiguous. The moment a step needs reading (compare two prices,
answer a question about the page, recover from an unexpected page) it needs
the LLM. It is worth an experiment for repetitive, well-specified flows
(the same form for many records), where it would make each step cost
fractions of a cent and a fraction of a second; it is not a general
replacement, and TypeSafe says the same.

## Risks

- **Accuracy is measured on 39 decisions only** (all right, two of them
  under the floor); the floor on `--find` and the runners-up in every answer
  are there so a wrong pick is visible.
- **Agents may not use it**: unprompted, Flash-Lite never did. The saving
  above needs the agent's harness or prompt to ask for it, until the skill
  or the tool itself gets agents there.
- **Page content goes to a third party** when enabled: opt-in, documented in
  `SECURITY.md`, typed values redacted. A self-hosted endpoint with the same
  API works through `NAVIGERA_DECIDER_URL`.
- **One vendor**: the API is small (one endpoint, three question types), the
  client is 150 lines, and LiteLLM proxies it; another System One model with
  the same shape would drop in.
- **Latency**: each decision is a network round trip (hundreds of ms). It
  replaces an LLM turn (seconds), but adds to a step that needed none, so
  `--find` is for the steps where the agent would otherwise read the tree.
