---
title: Docs
description: "Documentation for navigera, compara and primavera — three fast tools for browser automation, table comparison and plain-text web tests."
---

# Docs

Three tools, one idea: fast, local, scriptable, with output a person or
an agent can read. Pick by the job, not by the brand — they are designed
to work together (primavera drives the browser through navigera; compara
checks the data either of them produces).

| Product | Use it when | Start here |
|---|---|---|
| **navigera** | An agent (or a person at a shell) has to *drive a browser*: read a page, click, fill, wait, screenshot, hand a session over. | [navigera overview](/navigera/) |
| **compara** | Two tables have to *agree*: a nightly export against yesterday's, a CSV against a warehouse Parquet, a migration's source against its target. | [compara overview](/compara/) |
| **primavera** | A web flow has to *keep working*: plain-text scenarios that run on real Chrome, in CI, with a one-page HTML report. | [primavera overview](/primavera/) |

## When to use which

- **Testing a web app end to end?** Write it in primavera. It is the
  test runner; navigera is the engine underneath it, and you only reach
  for navigera directly when a scenario needs a step primavera does not
  have.
- **Driving a browser for anything else?** Use navigera directly — an
  agent's shell tool, an MCP client, or a script that needs one page
  read and one value extracted.
- **Checking data, not pages?** Use compara. It never opens a browser:
  two tables in, counts and a report out, with exit codes made for
  pipelines (0 identical, 1 differences, 2 error, 3 duplicate keys).

## How these docs are organised

Every product has the same shape: **Overview** (what it is, from its
README), **Install** (every channel the README lists), **Quick start**
(one copy-paste path, run in CI on every push), **Guides**
(task-oriented), **Reference** (commands and flags, generated from
`--help` where the CLI has it), **Changelog**, and the product's agent
**skill** where one exists. Links that point at a file in a product
repo open that file on GitHub; pages included here link within the site.

Nothing here is published anywhere yet — this site is built as a CI
artifact for inspection, and publishing is the owner's call.
