# WebArena-Verified pilot

A neutral-ground comparison of the browser tools: the tasks, sites and
scoring are ServiceNow's [WebArena-Verified](https://github.com/ServiceNow/webarena-verified)
(official 258-task Hard subset, Apache-2.0), driven by the same
OpenAI-compatible shell loop as `bench/agent-eval/openrouter_run.py`.

- `webarena_run.py` — the runner. Per task it recreates the task's sites
  from the suite's pristine Docker images (the suite has no data-reset
  operation — its env-ctrl `init` only applies configuration — so a
  fresh container is the reset), runs the agent, and scores with the
  suite's own `evaluate_task` on the agent's response JSON plus the
  captured network trace. Records per run: tool, model, task id, site,
  evaluator verdict and score, turns, tokens, OpenRouter cost, wall,
  site-setup seconds.
- `harproxy.py` — a stdlib recording reverse proxy. It listens on each
  site's canonical port, forwards to the container on a shadow port,
  injects the site's auto-login header (the optimized images' header
  authentication, so the agent starts logged in as the suite's harness
  arranges) and records every exchange for the HAR.
- `pilot_tasks.json` — the frozen 48-task pilot selection and how it was
  drawn. Wikipedia (~100 GB data) and Map (~60 GB) cannot run on
  GitHub-hosted runners, so the selection stratifies the 233 Hard tasks
  confined to shopping / shopping_admin / reddit / gitlab.

Run it through `.github/workflows/webarena-eval.yml` (matrix: tool ×
model; `count`/`offset` inputs slice the pilot for resumable runs), or
locally on a Docker host with `OPENROUTER_API_KEY` set.
