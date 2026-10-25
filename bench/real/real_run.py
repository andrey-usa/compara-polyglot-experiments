#!/usr/bin/env python3
"""REAL (agisdk) model-driven tool comparison harness.

Same model and prompt loop as the MiniWoB++ harness (bench/miniwob/
on experiment-23): one shell-command tool, temperature 0, max_tokens
1024, the tool manual truncated to its first 9,000 characters in the
system prompt, pattern-A re-prompt (up to 2) on a commandless turn, and
the same computed-cost budget guard. What changes is the task source
and the judge, which follow the separable route from the agisdk source
(base.py setup/teardown, evaluate.py script judging):

  1. setup: the tool's own browser GETs <site>/config?run_id=<run-id>
     &task_id=v2.<task>&latency=0, then <site>/finish, then loads the
     site URL. No account, no key, no leaderboard submission.
  2. the model drives the task with the tool's own CLI (navigera or
     agent-browser), one shell command per turn, up to --max-steps.
     When the model believes the task is done it replies with exactly
     the command `echo DONE`; the harness stops the episode there.
  3. judging: the harness GETs <site>/finish with the tool's browser,
     reads the <pre> JSON, saves it, and runs the task's v2 eval script
     from eval_scripts/ against it (30 s timeout, same as evaluate.py).
     Pass = "SUCCESS" in the script's stdout (case-insensitive).

Model: SiliconFlow chat completions (OpenAI-compatible),
deepseek-ai/DeepSeek-V4.1-Flash. Spend guard: before every request,
computed usage cost so far + retained reservations for unknown-outcome
requests + that request's documented maximum (1,049,000 context tokens
x $0.15/1M + 1024 x $0.60/1M = $0.15796) must fit --max-spend; no client
retries. Computed cost = usage x listed prices, not provider-billed.

Scripted self-check (--scripted-policy, no model calls): setup, no
agent actions, capture /finish, run the eval script. A no-op episode
must judge FAILURE; that proves the setup -> capture -> judge path in
CI before any model spend. The UI-driving path itself was proven by a
hand-driven navigera run on dashdish-1 (order placed through the real
UI, eval SUCCESS on the captured /finish JSON).

Recheck (acceptance): python3 bench/real/real_run.py --tool navigera
--tasks-file bench/real/tasks.json --count 1 --scripted-policy
"""
import argparse, json, os, re, subprocess, sys, time, urllib.request

PRICE_IN, PRICE_CACHED, PRICE_OUT = 0.15, 0.003, 0.60  # $ / 1M tokens, SiliconFlow DeepSeek-V4.1-Flash
CTX_MAX, MAX_TOKENS = 1_049_000, 1024
REQ_MAX_USD = CTX_MAX * PRICE_IN / 1e6 + MAX_TOKENS * PRICE_OUT / 1e6  # 0.15796 documented max per request

NAVIGERA_MANUAL = os.environ.get("NAVIGERA_SKILL_MD", os.path.join(os.path.dirname(os.path.abspath(__file__)), "navigera-skill.md"))
FINISH_JS = "() => { const pre = document.querySelector('pre'); return pre ? pre.innerText : document.body.innerText; }"

class BudgetStop(Exception):
    pass

class Tool:
    def __init__(self, name, nav_bin, ab_bin, session="real"):
        self.name, self.session = name, session
        self.nav_bin, self.ab_bin = nav_bin, ab_bin
    def _run(self, argv, timeout=180):
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return (p.stdout + p.stderr).strip()
    def start(self):
        if self.name == "navigera":
            return self._run([self.nav_bin, "-s", self.session, "start"], timeout=180)
        return ""  # agent-browser starts its session on first open
    def goto(self, url):
        if self.name == "navigera":
            return self._run([self.nav_bin, "-s", self.session, "goto", url])
        return self._run([self.ab_bin, "--session", self.session, "open", url], timeout=180)
    def eval(self, js, timeout=120):
        if self.name == "navigera":
            return self._run([self.nav_bin, "-s", self.session, "--raw", "eval", js], timeout)
        expr = "(" + js + ")()" if js.lstrip().startswith("() =>") or js.lstrip().startswith("async () =>") else js
        return self._run([self.ab_bin, "--session", self.session, "eval", expr], timeout)
    def observe(self, limit=12000):
        if self.name == "navigera":
            out = self._run([self.nav_bin, "-s", self.session, "--raw", "ax", "--limit", "200"])
        else:
            out = self._run([self.ab_bin, "--session", self.session, "snapshot", "-i"])
        return out[:limit]
    def env(self):
        env = dict(os.environ)
        if self.name == "agent-browser":
            env["AGENT_BROWSER_SESSION"] = self.session
        return env

def chat(api_key, model, messages, ledger, max_spend):
    if ledger["cost"] + ledger["retained"] + REQ_MAX_USD > max_spend:
        raise BudgetStop(f"budget guard: {ledger['cost']:.4f} computed + {ledger['retained']:.4f} retained + {REQ_MAX_USD:.5f} max > {max_spend}")
    ledger["retained"] += REQ_MAX_USD
    body = json.dumps({"model": model, "temperature": 0, "max_tokens": MAX_TOKENS, "messages": messages}).encode()
    req = urllib.request.Request("https://api.siliconflow.com/v1/chat/completions", data=body,
        headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            data = json.loads(r.read())
    except Exception:
        raise  # reservation retained (unknown charge outcome)
    ledger["retained"] -= REQ_MAX_USD
    u = data.get("usage") or {}
    cost = (u.get("prompt_tokens", 0) - u.get("prompt_cache_hit_tokens", 0)) * PRICE_IN / 1e6 \
         + u.get("prompt_cache_hit_tokens", 0) * PRICE_CACHED / 1e6 \
         + u.get("completion_tokens", 0) * PRICE_OUT / 1e6
    ledger["cost"] += cost
    ledger["tokens"] += u.get("total_tokens", 0)
    ledger["requests"] += 1
    return data["choices"][0]["message"]["content"], u, cost, data["choices"][0].get("finish_reason")

CMD_RE = re.compile(r"```(?:bash|sh)?\s*\n(.+?)\n```", re.S)

def extract_command(text):
    m = CMD_RE.search(text)
    if m:
        return m.group(1).strip().splitlines()[0].strip()
    for line in text.strip().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            return line
    return ""

def parse_js_json(out):
    v = out.strip()
    for _ in range(3):
        v = json.loads(v)
        if not isinstance(v, str):
            return v
    return v

def system_prompt(tool):
    if tool.name == "navigera":
        manual = open(NAVIGERA_MANUAL).read() if os.path.exists(NAVIGERA_MANUAL) else "(navigera skill manual unavailable)"
    else:
        manual = subprocess.run([tool.ab_bin, "--help"], capture_output=True, text=True).stdout
    if tool.name == "navigera":
        invoke = "%s -s %s" % (tool.nav_bin, tool.session)
    else:
        invoke = "%s --session %s" % (tool.ab_bin, tool.session)
    return ("You drive a web browser with %s, one shell command per turn, to complete a small web task on a demo site.\n"
            "Rules: reply with EXACTLY ONE shell command in a single fenced block; no prose outside the block.\n"
            "Invoke the tool ONLY as `%s <args...>` - the binary is NOT on PATH; use this exact prefix every time.\n"
            "The browser session is already running at the task site; do not start/open a new one, do not create any account, do not sign up anywhere, and do not submit anything to any leaderboard.\n"
            "Prefer the tool's documented accessibility-refs actions. When you believe the task is fully complete, reply with exactly the command `echo DONE` and nothing else; otherwise keep working. Finish in as few steps as possible.\n\n"
            "Tool manual:\n%s") % (tool.name, invoke, manual[:9000])

def setup(tool, task, run_id):
    site = task["url"].rstrip("/")
    config_url = f"{site}/config?run_id={run_id}&task_id=v2.{task['task']}&latency=0"
    diag = []
    tool.start()
    diag.append({"step": "config", "out": tool.goto(config_url)[:200]})
    diag.append({"step": "config_body", "out": tool.eval("() => document.body.innerText.slice(0, 200)")[:200]})
    diag.append({"step": "finish_init", "out": tool.goto(f"{site}/finish")[:200]})
    diag.append({"step": "site", "out": tool.goto(task["url"])[:200]})
    time.sleep(1.0)
    return diag

def capture_finish(tool, task, out_path):
    site = task["url"].rstrip("/")
    tool.goto(f"{site}/finish")
    time.sleep(1.0)
    raw = tool.eval(FINISH_JS)
    try:
        text = parse_js_json(raw)
        if not isinstance(text, str):
            text = json.dumps(text)
    except Exception:
        text = raw
    with open(out_path, "w") as f:
        f.write(text)
    try:
        json.loads(text)
        valid = True
    except Exception:
        valid = False
    return len(text.encode()), valid

def judge(eval_dir, task, finish_path):
    script = os.path.join(eval_dir, task["eval"])
    try:
        p = subprocess.run([sys.executable, script, finish_path], capture_output=True, text=True, timeout=30)
        out = p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return False, "eval script timed out (30 s)"
    return ("SUCCESS" in out.upper()), out[-500:]

def run_episode(tool, task, model, api_key, ledger, max_spend, max_steps, scripted, run_id, out_dir, eval_dir):
    rec = {"task": task["task"], "site": task["site"], "goal": task["goal"], "tool": tool.name,
           "run_id": run_id, "steps": 0, "judge_success": None, "tokens": 0, "cost": 0.0,
           "wall_s": 0.0, "error": None}
    t0 = time.time()
    try:
        rec["setup"] = setup(tool, task, run_id)
        if not scripted:
            messages = [{"role": "system", "content": system_prompt(tool)}]
            reprompts = 0
            last_cmd, repeat = None, 0
            for step in range(1, max_steps + 1):
                obs = tool.observe()
                messages.append({"role": "user", "content":
                    f"Task goal: {task['goal']}\nSite: {task['url']}\n\nCurrent page observation ({tool.name}):\n{obs}\n\n"
                    f"Reply with exactly one shell command in a fenced block (step {step}/{max_steps}), or `echo DONE` if the task is complete."})
                text, usage, cost, finish_reason = chat(api_key, model, messages, ledger, max_spend)
                rec["tokens"] += usage.get("total_tokens", 0); rec["cost"] += cost
                rec.setdefault("finish_reasons", []).append(finish_reason)
                cmd = extract_command(text)
                if not cmd:
                    rec.setdefault("commands", []).append({"step": step, "cmd": None, "model_text": text[:400], "finish_reason": finish_reason})
                    if reprompts < 2:
                        reprompts += 1
                        rec["pattern_a_reprompts"] = rec.get("pattern_a_reprompts", 0) + 1
                        messages.append({"role": "assistant", "content": text})
                        messages.append({"role": "user", "content":
                            "Your reply contained no shell command. Reply with EXACTLY ONE shell command "
                            "in a single fenced block and nothing else."})
                        continue
                    rec["error"] = "model produced no command after 2 re-prompts"; break
                reprompts = 0
                if cmd == "echo DONE":
                    rec.setdefault("commands", []).append({"step": step, "cmd": cmd, "done": True})
                    rec["steps"] = step
                    break
                if cmd == last_cmd:
                    repeat += 1
                    if repeat >= 3:
                        rec["error"] = "same command 3 times in a row"; break
                else:
                    last_cmd, repeat = cmd, 1
                r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=120, env=tool.env())
                rec.setdefault("commands", []).append({"step": step, "cmd": cmd, "rc": r.returncode, "out": (r.stdout + r.stderr)[:400], "finish_reason": finish_reason})
                messages.append({"role": "assistant", "content": text})
                messages.append({"role": "user", "content": f"Command output (truncated):\n{(r.stdout + r.stderr)[:3000]}"})
                rec["steps"] = step
        finish_path = os.path.join(out_dir, f"finish-{tool.name}-{task['task']}.json")
        nbytes, valid = capture_finish(tool, task, finish_path)
        rec["finish_bytes"] = nbytes
        rec["finish_valid_json"] = valid
        ok, jout = judge(eval_dir, task, finish_path)
        rec["judge_success"] = ok
        rec["judge_stdout"] = jout
    except BudgetStop as e:
        rec["error"] = f"budget-stop: {e}"
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
    rec["wall_s"] = round(time.time() - t0, 1)
    return rec

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", choices=["navigera", "agent-browser", "both"], required=True)
    ap.add_argument("--tasks-file", required=True)
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--count", type=int, default=0, help="max tasks from offset (0 = all)")
    ap.add_argument("--seeds", type=int, default=1, help="kept for protocol parity; episodes are unseeded site runs, seeds only repeat")
    ap.add_argument("--max-steps", type=int, default=25)
    ap.add_argument("--max-spend", type=float, default=2.49)
    ap.add_argument("--model", default="deepseek-ai/DeepSeek-V4.1-Flash")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "out"))
    ap.add_argument("--nav-bin", default="navigera")
    ap.add_argument("--ab-bin", default="agent-browser")
    ap.add_argument("--run-id", default="", help="per-cell run id for /config (default: real-<tool>)")
    ap.add_argument("--scripted-policy", action="store_true", help="no model calls; harness self-check")
    ap.add_argument("--start-tool", default="navigera")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    here = os.path.dirname(os.path.abspath(__file__))
    eval_dir = os.path.join(here, "eval_scripts")
    tasks = json.load(open(args.tasks_file))
    if args.offset:
        tasks = tasks[args.offset:]
    if args.count:
        tasks = tasks[:args.count]

    api_key = os.environ.get("SILICONFLOW_API_KEY", "")
    if not args.scripted_policy and not api_key:
        sys.exit("SILICONFLOW_API_KEY not set (or use --scripted-policy)")
    ledger = {"cost": 0.0, "retained": 0.0, "tokens": 0, "requests": 0}

    tools = ["navigera", "agent-browser"] if args.tool == "both" else [args.tool]
    first = args.start_tool if args.start_tool in tools else tools[0]
    jpath = os.path.join(args.out, "real-episodes.jsonl")
    n = 0
    for ti, task in enumerate(tasks):
        order = tools if ti % 2 == 0 else list(reversed(tools))
        if order[0] != first and len(tools) > 1:
            order = list(reversed(order))
        for seed in range(args.seeds):
            for tname in order:
                tool = Tool(tname, args.nav_bin, args.ab_bin, session=f"real-{tname}")
                run_id = args.run_id or f"real-{tname}"
                rec = run_episode(tool, task, args.model, api_key, ledger, args.max_spend,
                                  args.max_steps, args.scripted_policy, run_id, args.out, eval_dir)
                with open(jpath, "a") as f:
                    f.write(json.dumps(rec) + "\n")
                n += 1
                print(f"[{n}] {rec['tool']} {task['task']} seed={seed} judge={rec['judge_success']} steps={rec['steps']} "
                      f"cost={rec['cost']:.4f} wall={rec['wall_s']}s err={rec['error']}", flush=True)
    print(f"ledger: computed=${ledger['cost']:.4f} retained=${ledger['retained']:.4f} "
          f"requests={ledger['requests']} tokens={ledger['tokens']}")

if __name__ == "__main__":
    main()
