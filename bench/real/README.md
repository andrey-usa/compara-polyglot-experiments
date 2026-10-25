# REAL (agisdk) model-driven tool comparison

Same model and prompt loop as the MiniWoB++ harness (bench/miniwob/):
one shell-command tool, DeepSeek-V4.1-Flash via SiliconFlow, temperature
0, max_tokens 1024, each tool manual truncated to its first 9,000
characters in the system prompt, pattern-A re-prompt (up to 2) on a
commandless turn, per-cell computed-cost guard ($2.49; two cells fit
the $5 combined budget). The task source and judge are the separable
route from the agisdk source: /config setup with a distinct run id per
cell, the tool's own browser, /finish JSON captured by the harness, and
the task's v2 eval script as the judge (pass = "SUCCESS" in stdout).

## Pins

- agisdk: agi-inc/agisdk @ 97a245dcbccd3e1cf4d5ee00d5b1707942258518
  (task JSONs + eval scripts under v2/; only the 31 subset tasks are
  copied here, verbatim, into tasks.json + eval_scripts/).
- Subset (fixed before any paid cell, first-3-numeric per site,
  marrisuite its only task; 31 tasks, 11 sites): dashdish-1/3/4,
  gocalendar-4/6/8, gomail-1/2/4, marrisuite-8, networkin-1/5/6,
  omnizon-3/9/13, opendining-1/2/4, staynb-1/4/5, topwork-2/3/4,
  udriver-2/3/4, zilloft-2/3/5.
- navigera: built in CI from the experiment-17 candidate tree @
  e9c1b57 in full (the 0.6 candidate lineage); the system prompt reads
  the skill the built binary installs.
- agent-browser: npm agent-browser@0.39.1 (same as the MiniWoB++ run).
- Model: deepseek-ai/DeepSeek-V4.1-Flash via SiliconFlow
  (https://api.siliconflow.com/v1), key from the `main` environment
  as SILICONFLOW_API_KEY. No other model, no key in any file or log.
- Episodes: 1 seed per task (site runs are unseeded; seeds only
  repeat), max 25 steps, `echo DONE` ends an episode early.

## Caveats (same as the MiniWoB++ run, plus one)

- The manual[:9000] truncation is matched by mechanism, not coverage:
  navigera loses ~1/3 of its skill, agent-browser ~2/3 of its --help.
  This ranks tool+truncated-instructions as configured, not bare tools.
- Single run per cell; agent runs are stochastic.
- The two cells run as separate matrix jobs (one job per tool), not
  interleaved in one process; the harness itself alternates tools per
  task when run with --tool both.

## Recheck

Scripted self-check (zero model spend, proves setup -> capture ->
judge; a no-op episode must judge FAILURE):

```text
$ python3 bench/real/real_run.py --tool navigera --tasks-file bench/real/tasks.json --count 1 --scripted-policy --out out/real
[...] navigera dashdish-1 seed=0 judge=False steps=0 ...
```

Aggregate a cell's episodes:

```text
$ python3 bench/real/aggregate-real.py out/real/real-episodes.jsonl
```

## Workflows

- real-smoke.yml (push): scripted self-check on dashdish-1, both tools,
  no model key needed.
- real-eval.yml (push): the full 31-task matrix, one job per tool.
