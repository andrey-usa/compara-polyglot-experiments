#!/usr/bin/env python3
"""Agent eval with an OpenAI-compatible driver: two models, one harness.

The same eval as run.py — same site (bench/site/server.py), same tasks
(bench/agent-eval/tasks.json), same skilled workspace and skill, same
prompt, same checks — with the Gemini CLI swapped for a direct
chat-completions loop against OpenRouter. The model gets one tool,
run_shell_command (the shell tool the Gemini harness gives the agent),
and drives the browser tool named by --tool through it: plain navigera
(Chrome), navigera-lightpanda, navigera-shell, the +jev variants (whose
decider relay, --decider-url/--decider-log, bench/decide/relay.py, is
run.py's own wiring, held outside the agent's shell), or agent-browser
(on Chrome, or on its Lightpanda engine).

  openrouter_run.py --tool navigera-lightpanda+jev \
                    --models deepseek/deepseek-v4.1-flash,inception/mercury-3 \
                    --tasks lookup,support --reps 1 --out out/openrouter

Each run's record carries the model, task, rep, pass/fail (run.py's
check, unchanged), model turns, prompt/completion/cached tokens, the
cost OpenRouter reports in each response's usage, and wall time. The
models alternate who runs first per task and rep, so going first
cannot favour one of them. A run stops at 40 model turns (the Gemini
settings' maxSessionTurns), at --run-timeout, or after the same
command fails three times in a row.

Key: OPENROUTER_API_KEY in the environment.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import types
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import run as evalrun  # noqa: E402  the eval's site, setup, prompt and checks

API = "https://openrouter.ai/api/v1/chat/completions"
DEFAULT_MODELS = "deepseek/deepseek-v4.1-flash,inception/mercury-3"

SHELL_TOOL = [{
    "type": "function",
    "function": {
        "name": "run_shell_command",
        "description": "Run a shell command in the workspace and return its output.",
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string", "description": "The command to run"}},
            "required": ["command"],
        },
    },
}]

MAX_TURNS = 40        # GEMINI_SETTINGS' model.maxSessionTurns in run.py
SHELL_TIMEOUT = 240   # GEMINI_SETTINGS' tools.shell.inactivityTimeout
REPEAT_LIMIT = 3      # the same command failing this many times in a row ends the run
OUTPUT_TAIL = 12000   # chars of a command's output handed back to the model


def chat(key: str, model: str, messages: list) -> tuple[dict, int]:
    """One model call; returns (response, http attempts). 429/5xx and
    network errors retry with backoff: a rate limit is not an answer."""
    body = {"model": model, "messages": messages, "tools": SHELL_TOOL,
            "tool_choice": "auto", "usage": {"include": True}}
    data = json.dumps(body).encode()
    last = None
    for attempt in range(4):
        req = urllib.request.Request(API, data=data, method="POST")
        req.add_header("Content-Type", "application/json")
        req.add_header("Authorization", f"Bearer {key}")
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                return json.loads(r.read().decode()), attempt + 1
        except urllib.error.HTTPError as e:
            try:
                detail = e.read().decode()[:300]
            except Exception:
                detail = ""
            last = f"HTTP {e.code}: {detail}"
            if e.code not in (429, 500, 502, 503, 504):
                break
        except Exception as e:  # URLError, timeout, bad JSON
            last = f"{type(e).__name__}: {e}"
        time.sleep(5 * (attempt + 1))
    raise RuntimeError(last or "request failed")


def run_shell(command: str, work: Path, env: dict) -> tuple[int | None, str]:
    try:
        p = subprocess.run(command, shell=True,
                           executable=None if os.name == "nt" else "/bin/bash",
                           cwd=work, env=env, capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=SHELL_TIMEOUT)
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except subprocess.TimeoutExpired as e:
        out = e.stdout if isinstance(e.stdout, str) else ""
        return None, out + f"\n[command timed out after {SHELL_TIMEOUT}s]"


def tool_text(rc: int | None, out: str) -> str:
    if len(out) > OUTPUT_TAIL:
        out = "[output truncated to the last chars]\n" + out[-OUTPUT_TAIL:]
    return (f"Exit code: {rc}\n" if rc is not None else "") + out


def run_agent(key: str, model: str, prompt: str, work: Path, env: dict, args) -> dict:
    messages = [{"role": "user", "content": prompt}]
    started = time.time()
    deadline = started + args.run_timeout
    turns = attempts = 0
    tokens_in = tokens_out = tokens_cached = 0
    cost, cost_seen = 0.0, False
    text_parts, commands, errors = [], [], []
    timed_out, stuck = False, None
    decider_seen = False
    last_cmd, repeat = None, 0
    while turns < MAX_TURNS:
        if time.time() > deadline:
            timed_out = True
            break
        try:
            resp, n = chat(key, model, messages)
        except RuntimeError as e:
            errors.append(str(e))
            break
        attempts += n
        turns += 1
        usage = resp.get("usage") or {}
        tokens_in += usage.get("prompt_tokens") or 0
        tokens_out += usage.get("completion_tokens") or 0
        tokens_cached += (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
        if usage.get("cost") is not None:
            cost += usage["cost"]
            cost_seen = True
        choices = resp.get("choices") or []
        if not choices:
            errors.append("response had no choices")
            break
        msg = choices[0].get("message") or {}
        messages.append(msg)
        if msg.get("content"):
            text_parts.append(msg["content"])
        calls = msg.get("tool_calls") or []
        if not calls:
            break  # a plain reply ends the run
        for call in calls:
            fn = call.get("function") or {}
            cmd = ""
            try:
                cmd = json.loads(fn.get("arguments") or "{}").get("command", "")
            except json.JSONDecodeError:
                pass
            if fn.get("name") != "run_shell_command" or not cmd:
                messages.append({"role": "tool", "tool_call_id": call.get("id"),
                                 "content": 'Error: call run_shell_command with a JSON '
                                            'object {"command": "..."}.'})
                continue
            commands.append(cmd)
            rc, out = run_shell(cmd, work, env)
            result_text = tool_text(rc, out)
            # Whether the agent saw `start` report a decider (navigera+jev
            # runs), as run.py records it for the Gemini driver.
            if '"decider"' in result_text:
                decider_seen = True
            messages.append({"role": "tool", "tool_call_id": call.get("id"),
                             "content": result_text})
            if rc:
                repeat = repeat + 1 if cmd == last_cmd else 1
                last_cmd = cmd
                if repeat >= REPEAT_LIMIT:
                    stuck = f"stuck: same command failed {REPEAT_LIMIT} times in a row: {cmd[:160]}"
            else:
                last_cmd, repeat = None, 0
        if stuck:
            break
    return {
        "exit": 0 if not errors else 1, "timed_out": timed_out,
        "wall_s": round(time.time() - started, 1),
        "text": "".join(text_parts), "shell_commands": commands,
        "requests": turns, "api_attempts": attempts,
        "tokens_in": tokens_in, "tokens_out": tokens_out,
        "tokens_cached": tokens_cached, "tokens_total": tokens_in + tokens_out,
        "cost_usd": round(cost, 6) if cost_seen else None,
        "errors": errors[-3:], "stuck": stuck, "decider_seen": decider_seen,
    }


def render(tool: str, models: list, runs: list) -> str:
    lines = [f"### OpenRouter head-to-head — skilled ({tool})", "",
             "| model | task | rep | result | turns | shell cmds | in tok | cached | out tok | cost | wall |",
             "|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in runs:
        result = "✓" if r["ok"] else ("⚠ infra" if r.get("infra") else "✗")
        cost = f"${r['cost_usd']:.4f}" if r.get("cost_usd") is not None else "—"
        lines.append(f"| `{r['model']}` | {r['task']} | {r['rep']} | {result} | {r.get('requests') or '—'} | "
                     f"{r.get('shell_count') or '—'} | {r.get('tokens_in') or '—'} | {r.get('tokens_cached') or '—'} | "
                     f"{r.get('tokens_out') or '—'} | {cost} | {r.get('wall_s') or '—'}s |")
    lines += ["", "| model | passed | turns | tokens | cost | wall |", "|---|---|---|---|---|---|"]
    for m in models:
        rs = [r for r in runs if r["model"] == m]
        if not rs:
            continue
        cost = sum(r["cost_usd"] for r in rs if r.get("cost_usd") is not None)
        lines.append(f"| `{m}` | {sum(1 for r in rs if r['ok'])}/{len(rs)} | "
                     f"{sum(r.get('requests') or 0 for r in rs)} | {sum(r.get('tokens_total') or 0 for r in rs)} | "
                     f"${cost:.4f} | {sum(r.get('wall_s') or 0 for r in rs):.0f}s |")
    if any("decider_calls" in r for r in runs):
        lines += ["", "| model | runs that saw the decider | runs that used find/check | find/check commands | "
                      "decider calls | decider input tokens | decider errors |", "|---|---|---|---|---|---|---|"]
        for m in models:
            rs = [r for r in runs if r["model"] == m]
            if not rs:
                continue
            lines.append(f"| `{m}` | {sum(1 for r in rs if r.get('decider_seen'))}/{len(rs)} | "
                         f"{sum(1 for r in rs if r.get('decision_commands'))}/{len(rs)} | "
                         f"{sum(r.get('decision_commands') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_calls') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_tokens') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_errors') or 0 for r in rs)} |")
    fails = [r for r in runs if not r["ok"]]
    if fails:
        lines += ["", "Failures:"]
        for r in fails:
            lines.append(f"- `{r['model']}` {r['task']} rep{r['rep']}: "
                         f"{'; '.join(r.get('why', []))[:240]}"
                         + (" (timed out)" if r.get("timed_out") else ""))
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", default="navigera",
                    help="run.py tool variant: navigera, navigera-lightpanda, navigera-shell, "
                         "a +jev variant (which needs --decider-url/--decider-log), "
                         "agent-browser or agent-browser-lightpanda")
    ap.add_argument("--decider-url", help="decision-model endpoint for +jev tools (bench/decide/relay.py)")
    ap.add_argument("--decider-log", help="the relay's request log, to count each run's decider calls")
    ap.add_argument("--models", default=DEFAULT_MODELS)
    ap.add_argument("--tasks", default="lookup,support")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--run-timeout", type=int, default=900)
    ap.add_argument("--max-spend", type=float, default=1.0,
                    help="stop starting runs once the summed reported cost passes this many USD")
    ap.add_argument("--nv-bin-dir", default=str(evalrun.REPO / "target" / "release"),
                    help="directory holding the navigera binary under test")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        print("OPENROUTER_API_KEY is not set", file=sys.stderr)
        return 1
    if args.tool not in evalrun.TOOLS:
        print(f"unknown tool {args.tool!r} (run.py knows: {', '.join(evalrun.TOOLS)})", file=sys.stderr)
        return 2
    models = [m.strip() for m in args.models.split(",") if m.strip()]
    all_tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    if args.tasks == "all":
        tasks = [t for t in all_tasks if not t.get("live")]
    else:
        wanted = {t.strip() for t in args.tasks.split(",") if t.strip()}
        tasks = [t for t in all_tasks if t["id"] in wanted]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    setup_args = types.SimpleNamespace(model="", decider_url=args.decider_url, nv_bin_dir=args.nv_bin_dir)
    runs, spent, stop = [], 0.0, False
    for rep in range(args.reps):
        for ti, task in enumerate(tasks):
            order = models if (ti + rep) % 2 == 0 else list(reversed(models))
            for oi, model in enumerate(order):
                if stop:
                    break
                if spent >= args.max_spend:
                    print(f"spend cap ${args.max_spend:.2f} reached (${spent:.4f} reported); "
                          "no further runs started", flush=True)
                    stop = True
                    break
                root = Path(tempfile.mkdtemp(prefix=f"eval-or-{task['id']}-"))
                rec = {"model": model, "tool": args.tool, "task": task["id"], "rep": rep, "order": oi + 1,
                       "mode": "skilled", "agent": "openrouter", "os": evalrun.OS_NAME}
                site = None
                try:
                    site = evalrun.Site()
                    setup_args.model = model
                    work, env = evalrun.prepare(args.tool, "skilled", root, setup_args)
                    spec = evalrun.TOOLS[args.tool]
                    prompt = evalrun.SKILLED_PROMPT.format(
                        workdir=work, cmd=spec["cmd"], skill=spec["skill"], name=args.tool,
                        repo=spec["repo"], base=site.base, task=task["prompt"],
                        hint=spec.get("hint", ""))
                    before = evalrun.log_lines(args.decider_log) if spec.get("decider") else 0
                    res = run_agent(key, model, prompt, work, env, args)
                    if spec.get("decider"):
                        calls = evalrun.read_log(args.decider_log)[before:]
                        res["decider_calls"] = len(calls)
                        res["decider_tokens"] = sum((c.get("usage") or {}).get("input_tokens") or 0
                                                    for c in calls)
                        res["decider_errors"] = sum(1 for c in calls if c.get("status") != 200)
                    answer = evalrun.final_answer(res["text"])
                    ok, why = evalrun.check(task, answer, site, args.tool)
                    if res.get("stuck"):
                        why.append(res["stuck"])
                    # An API failure before the first answer is infra, like
                    # run.py's quota/outage bucket; later errors fail the run.
                    infra = bool(res.get("errors")) and res.get("requests", 0) == 0
                    if res.get("errors") and not infra:
                        why += res["errors"]
                    rec.update(res)
                    rec.update({
                        "ok": ok, "why": why, "infra": infra, "answer": answer[-300:],
                        "shell_count": len(res["shell_commands"]),
                        "tool_commands": sum(1 for c in res["shell_commands"] if spec["cmd"] in c),
                        "decision_commands": sum(1 for c in res["shell_commands"]
                                                 if evalrun.DECISION.search(c)),
                        "off_tool_commands": [c[:200] for c in res["shell_commands"]
                                              if evalrun.OFF_TOOL.search(c)],
                    })
                    if res.get("cost_usd"):
                        spent += res["cost_usd"]
                except Exception as e:  # harness/setup problem: record, keep going
                    rec.update({"ok": False, "why": [f"harness: {e}"]})
                finally:
                    if site:
                        site.close()
                    evalrun.cleanup(root)
                rec.pop("text", None)
                runs.append(rec)
                status = "PASS" if rec["ok"] else "FAIL " + "; ".join(rec.get("why", []))[:200]
                print(f"[skilled] {args.tool} {model:32} {task['id']:9} rep{rep}: {status} "
                      f"(turns={rec.get('requests')}, shell={rec.get('shell_count')}, "
                      f"cost={rec.get('cost_usd')}, wall={rec.get('wall_s')}s)", flush=True)
                (out / "runs.jsonl").open("a").write(json.dumps(rec) + "\n")
    table = render(args.tool, models, runs)
    (out / "table.md").write_text(table)
    (out / "summary.json").write_text(json.dumps(
        {"agent": "openrouter", "tool": args.tool, "models": models, "runs": runs}, indent=1))
    summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_file:
        with open(summary_file, "a", encoding="utf-8") as f:
            f.write(table)
    print(table)
    return 0


if __name__ == "__main__":
    sys.exit(main())
