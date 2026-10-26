---
title: CLI reference primavera
description: CLI reference primavera generated from --help.
---

# CLI reference primavera


Generated from `primavera next fa3fe34 against navigera session-history e7e26cd`.

Binary reports: `primavera 0.0.1`.

## Top level

```text
unknown command "--help"

primavera: plain-text web tests on real Chrome (through navigera)

usage:
  primavera run [<file.pv|file.feature|dir>...] [options]
                                                run scenarios (default: the current directory)
  primavera lint [<file|dir>...]                check files without a browser
  primavera list [<file|dir>...]                the scenarios a run would take
  primavera trace <dir> [<line>]                a recorded trace (--trace): every step, or one in full
  primavera fix [<file>:<line>] [--use <n>] [--all [--min <p>]]
                                                put a suggestion of the last run (nearest on the page) into the failing step
  primavera navigera <command…>                 navigera's commands, built in (`primavera navigera -s work goto <url>`)
  primavera from-session <name> [--scenario <s>] [--out <f.pv>]
                                                what a navigera session did, as a scenario (refs as locators)
  primavera record <url> [--out <f.pv>] [--scenario <s>] [--headless] [--for <s>]
                                                a browser to use by hand; what you do becomes a scenario
  primavera record --extend <f.pv> [--scenario <s>] [--base-url <url>] [--secrets <file>]
                                                run a scenario to its end, then record more steps into it
  primavera mcp [--headed] [--chromium <path>]  an MCP server: navigera's browser tools and primavera_from_session/run/failure/fix
  primavera skill                               the agent skill (the format and commands, for an AI agent)
  primavera --version

run options:
  --base-url <url>      where relative URLs (`goto /cart`) point (else `base` in the file, PRIMAVERA_BASE_URL)
  --var <name=value>    a variable for ${name} (repeatable)
  --secrets <file>      NAME=value lines for ${secret:NAME} (the environment comes first); masked in output
  --grep <text>         only scenarios whose name contains it
  --tag <tag>           only scenarios with this @tag (repeatable: any of them)
  --workers <n|auto>    scenarios at once (default 1; auto: half the CPUs, at most 8); also -j <n>
  --isolation <how>     context (default): one browser, a browser context per worker; browser: a browser each
  --retries <n>         run a failing scenario again, up to n times; one that then passes is flaky
  --fail-on-flaky       a flaky scenario fails the run
  --shard <i/n>         run the i-th of n slices (for CI matrix jobs)
  --repeat <n>          run every scenario n times (flake hunting: each run is reported, then a pass count)
  --shuffle [<seed>]    run in a random order (the seed is printed; give it again to repeat the order)
  --reporter <name>     console (default), jsonl (one JSON object per scenario on stdout), github
                        (workflow annotations; on by itself under GitHub Actions); repeatable
  --config <file>       settings, a [server] and [env.<name>] profiles (default: ./primavera.toml)
  --env <name>          the config's [env.<name>] profile on top of the rest
  --update-screenshots  a screenshot that differs from its baseline (or has none) becomes it
  --compare-report <k>  what `expect download … compare` keeps: html, json, rows (comma-separated) or none (default html)
  --trace <when>        on-failure or all: each step's page, console and requests in trace.jsonl
                        beside the screenshot (and ax.txt at a failure); read it with `primavera trace`
  --junit <file>        also write JUnit XML
  --cucumber <file>     also write Cucumber JSON for the .feature files (step by step)
  --allure <dir>        also write Allure results (a JSON file per scenario run, screenshots attached)
  --html <file>         also write the run as one HTML page (steps, times, failures with their screenshot)
  --json                print what the scenarios saved, as one JSON object, at the end
  --timeout-ms <ms>     how long an expect retries (default 5000)
  --wait-ms <ms>        how long a step waits for its element (navigera's default 5000)
  --headed              show the browser
  --chromium <path>     the browser to run
  --engine lightpanda   run on Lightpanda (no layout: a fast smoke tier; screenshots and pixel checks fail)
  --evidence <dir>      failure screenshots and downloads (default .primavera/runs/<time>)
  --pause-on-failure    keep the browser at a failure, open to `navigera --attach`, until Enter (one worker)

exit codes: 0 all passed, 1 a scenario failed, 2 a file or an argument is wrong, 3 the browser or the app server didn't start
```
