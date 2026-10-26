---
title: Guides
description: "Task-oriented guides for navigera — read a page, act on it, wait, capture, hand a session over."
---

# Guides

## Read a page

```sh
navigera -s work start --headless
navigera -s work goto https://example.com
navigera -s work ax --raw        # indented tree, [ref=N] on actionable nodes
navigera -s work eval "document.title"
```

`ax` is the default read: one tree, refs you can act on. `eval` runs
JavaScript and returns one value; use it only for what the tree does not
show.

## Act on an element

```sh
navigera -s work click 12
navigera -s work fill 31 "hello" && navigera -s work press Enter
navigera -s work wait --text "Saved"
```

Targets can be a ref, `--selector <css>`, `--text "<visible text>"`,
`--role/--name`, `--label`, `--placeholder` or `--testid`. Chain sure
steps in one shell call; `wait` for the thing the next step depends on.

## Capture and inspect

```sh
navigera -s work screenshot shot.png
navigera -s work pdf page.pdf
navigera -s work console          # what the page logged
navigera -s work network          # what the page fetched
```

## Sessions and handoff

A named session (`-s work`) survives between shell calls: the browser
stays warm behind a local socket. `show` puts a headless session on the
person's screen for the step only they can do (a login, a 2FA prompt, a
CAPTCHA) and `hide` takes it back. `start --attach` drives your running
Chrome itself; `start --profile agent --from chrome` seeds a browser of
its own from a copy of your profile's sign-ins.

## Agents (MCP)

`navigera mcp` is the Model Context Protocol server for Cline, Roo Code,
Kilo Code, Continue, VS Code, Claude Desktop and Cursor, with the same
tree, refs and waits as the CLI. Add it as
`{"command": "navigera", "args": ["mcp"]}`. The full command list is in
[Reference](/navigera/reference/) and the agent guide in
[Agent skill](/navigera/skill/).

## Decisions by a fast model (optional)

With a decider configured (`TYPESAFE_API_KEY`, or any compatible
endpoint via `NAVIGERA_DECIDER_URL`), two small decisions can go to it
instead of reading a whole tree:

```sh
navigera -s work click --find "accept the cookies"
navigera -s work check "Is an error message shown?"
```

Below a 0.5 probability, `--find` refuses and lists the candidates
instead of acting. See [Multi-model](/navigera/multi-model/) for the
measured results.
