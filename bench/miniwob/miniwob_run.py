#!/usr/bin/env python3
"""MiniWoB++ model-driven tool comparison harness (T-0022).

Same model and prompt loop, two browser tools (navigera, agent-browser).
Serves the MiniWoB++ HTML locally, starts seeded episodes, gives the model
the on-page instruction plus the tool's own observation, runs the ONE shell
command the model answers with, repeats up to --max-steps, and reads the
reward harness-side (the model never sees the page's JavaScript globals).

Model: SiliconFlow chat completions (OpenAI-compatible), temperature 0,
max_tokens 1024. Spend guard: before every request, computed usage cost so
far + retained reservations for unknown-outcome requests + that request's
documented maximum (164K context x $0.27/1M + 1024 x $0.42/1M = $0.04471)
must fit --max-spend; no client retries.

Recheck (acceptance): python3 miniwob/run.py --tool navigera --tasks click-button --seeds 1
"""
import argparse, json, os, re, subprocess, sys, threading, time, urllib.request
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
from functools import partial

HTML_ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "miniwob-plusplus", "miniwob", "html")
EPISODE_MAX_TIME_MS = 600000  # 10 min per episode, both tools, recorded protocol choice
# Lead direction (T-0022 note 20261024T172934Z): same model as the T-0029 cells for
# cross-benchmark comparability - deepseek-ai/DeepSeek-V4.1-Flash on SiliconFlow.
# (The T-0022 card originally said deepseek-chat; the later lead note supersedes it.)
PRICE_IN, PRICE_CACHED, PRICE_OUT = 0.15, 0.003, 0.60  # $ / 1M tokens, SiliconFlow deepseek-ai/DeepSeek-V4.1-Flash
CTX_MAX, MAX_TOKENS = 1_049_000, 1024
REQ_MAX_USD = CTX_MAX * PRICE_IN / 1e6 + MAX_TOKENS * PRICE_OUT / 1e6  # 0.15796 documented max per request

NAVIGERA_MANUAL = os.environ.get("NAVIGERA_SKILL_MD", os.path.join(os.path.dirname(os.path.abspath(__file__)), "navigera-skill.md"))

START_JS = """() => { Math.seedrandom(%s); core.EPISODE_MAX_TIME = %d; core.startEpisodeReal(); return 'started'; }"""
QUERY_JS = "() => document.getElementById('query').innerText"
POLL_JS = "() => JSON.stringify({done: WOB_DONE_GLOBAL, reward: WOB_REWARD_GLOBAL, raw: WOB_RAW_REWARD_GLOBAL})"

class Tool:
    def __init__(self, name, nav_bin, ab_bin, session="mw"):
        self.name, self.session = name, session
        self.nav_bin, self.ab_bin = nav_bin, ab_bin
    def _run(self, argv, timeout=120):
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return (p.stdout + p.stderr).strip()
    def goto(self, url):
        if self.name == "navigera":
            self._run([self.nav_bin, "-s", self.session, "start"], timeout=180)
            return self._run([self.nav_bin, "-s", self.session, "goto", url])
        return self._run([self.ab_bin, "--session", self.session, "open", url], timeout=180)
    def eval(self, js, timeout=60):
        if self.name == "navigera":
            return self._run([self.nav_bin, "-s", self.session, "--raw", "eval", js], timeout)
        # agent-browser eval evaluates an expression; wrap arrow functions as an IIFE
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
    """One guarded request. Returns (text, usage) or raises BudgetStop/RuntimeError."""
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
    ledger["retained"] -= REQ_MAX_USD  # settled: release the reservation, add the computed cost
    u = data.get("usage") or {}
    cost = (u.get("prompt_tokens", 0) - u.get("prompt_cache_hit_tokens", 0)) * PRICE_IN / 1e6 \
         + u.get("prompt_cache_hit_tokens", 0) * PRICE_CACHED / 1e6 \
         + u.get("completion_tokens", 0) * PRICE_OUT / 1e6
    ledger["cost"] += cost
    ledger["tokens"] += u.get("total_tokens", 0)
    ledger["requests"] += 1
    return data["choices"][0]["message"]["content"], u, cost, data["choices"][0].get("finish_reason")
class BudgetStop(Exception):
    pass

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
    """navigera --raw returns JSON directly; agent-browser may return a quoted+escaped JSON string."""
    v = out.strip()
    for _ in range(3):
        v = json.loads(v)
        if not isinstance(v, str):
            return v
    return v

def run_episode(tool, task, seed, model, api_key, ledger, max_spend, max_steps, base_url, scripted):
    rec = {"task": task, "seed": seed, "tool": tool.name, "steps": 0, "reward": 0.0, "raw_reward": 0.0,
           "done": False, "tokens": 0, "cost": 0.0, "wall_s": 0.0, "error": None}
    t0 = time.time()
    try:
        tool.goto(f"{base_url}/miniwob/{task}.html")
        time.sleep(1.0)
        out = tool.eval(START_JS % (json.dumps(f"{task}-{seed}"), EPISODE_MAX_TIME_MS))
        if "started" not in out:
            raise RuntimeError(f"episode start failed: {out[:200]}")
        instruction = tool.eval(QUERY_JS).strip().strip('"')
        rec["instruction"] = instruction
        if scripted:
            js = SCRIPTED_JS.get(task)
            if js:
                out = tool.eval(js)
                rec["steps"] = 1
                rec["scripted_result"] = out[:200]
            time.sleep(0.5)
        else:
            messages = [{"role": "system", "content": system_prompt(tool)}]
            reprompts = 0  # pattern-A recovery: re-prompt on a commandless text-only turn
            for step in range(1, max_steps + 1):
                obs = tool.observe()
                messages.append({"role": "user", "content":
                    f"Task instruction: {instruction}\n\nCurrent page observation ({tool.name}):\n{obs}\n\n"
                    f"Reply with exactly one shell command in a fenced block (step {step}/{max_steps})."})
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
                r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=90, env=tool.env())
                rec.setdefault("commands", []).append({"step": step, "cmd": cmd, "rc": r.returncode, "out": (r.stdout + r.stderr)[:400], "finish_reason": finish_reason})
                messages.append({"role": "assistant", "content": text})
                messages.append({"role": "user", "content": f"Command output (truncated):\n{(r.stdout + r.stderr)[:3000]}"})
                rec["steps"] = step
                poll = parse_js_json(tool.eval(POLL_JS))
                if poll["done"]:
                    break
        poll = parse_js_json(tool.eval(POLL_JS))
        rec.update(done=poll["done"], reward=poll["reward"], raw_reward=poll["raw"])
    except BudgetStop as e:
        rec["error"] = f"budget-stop: {e}"
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
    rec["wall_s"] = round(time.time() - t0, 1)
    return rec

# DOM-direct validation policy for the harness self-check (--scripted-policy, no model).
# Each entry solves the task through the tool's own eval channel. click-button is the
# acceptance task; the rest validate serve/eval/episode/reward plumbing on more shapes.
SCRIPTED_JS = {
  "click-button": """() => { const q=document.getElementById('query').innerText; const m=q.match(/"([^"]+)"/); const b=[...document.querySelectorAll('#area button')].find(x=>x.innerText===m[1]); if(b){b.click(); return 'clicked:'+m[1];} return 'not-found'; }""",
  "click-button-sequence": """() => { const q=document.getElementById('query').innerText; const want=[...q.matchAll(/"([^"]+)"/g)].map(x=>x[1]); const btns=[...document.querySelectorAll('#area button')]; let n=0; for(const w of want){ const b=btns.find(x=>x.innerText===w); if(b){b.click(); n++;} } return 'clicked:'+n; }""",
  "click-checkboxes": """() => { const q=document.getElementById('query').innerText; const want=[...q.matchAll(/'([^']+)'/g)].map(x=>x[1].toLowerCase()); let n=0; [...document.querySelectorAll('#area label')].forEach(l=>{ const t=l.innerText.toLowerCase(); const c=l.querySelector('input[type=checkbox]'); if(c && want.some(w=>t.includes(w)) && !c.checked){ c.click(); n++; } }); const s=document.querySelector('#area button, #subbtn'); if(s) s.click(); return 'checked:'+n; }""",
  "enter-text": """() => { const q=document.getElementById('query').innerText; const m=q.match(/"([^"]+)"/); const t=document.querySelector('#area input[type=text]'); if(t&&m){ t.focus(); t.value=m[1]; t.dispatchEvent(new Event('input',{bubbles:true})); t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',keyCode:13,which:13,bubbles:true})); t.dispatchEvent(new KeyboardEvent('keyup',{key:'Enter',keyCode:13,which:13,bubbles:true})); const s=document.querySelector('#area button'); if(s) s.click(); return 'entered:'+m[1]; } return 'no-field'; }""",
  "click-tab": """() => { const q=document.getElementById('query').innerText; const m=q.match(/"([^"]+)"/) || q.match(/'(\d+)'/); const tabs=[...document.querySelectorAll('#area a, #area li, #area button, #area span')].filter(e=>e.innerText && e.innerText.trim()=== (m?m[1]:'')); if(tabs[0]){tabs[0].click(); return 'clicked';} return 'not-found'; }""",
}

def system_prompt(tool):
    if tool.name == "navigera":
        manual = open(NAVIGERA_MANUAL).read() if os.path.exists(NAVIGERA_MANUAL) else "(navigera skill manual unavailable)"
    else:
        manual = subprocess.run([tool.ab_bin, "--help"], capture_output=True, text=True).stdout
    if tool.name == "navigera":
        invoke = "%s -s %s" % (tool.nav_bin, tool.session)
    else:
        invoke = "%s --session %s" % (tool.ab_bin, tool.session)
    return ("You drive a web browser with %s, one shell command per turn, to complete a small web task.\n"
            "Rules: reply with EXACTLY ONE shell command in a single fenced block; no prose outside the block.\n"
            "Invoke the tool ONLY as `%s <args...>` - the binary is NOT on PATH; use this exact prefix every time.\n"
            "The browser session is already running; do not start/open a new one. Do not read or reference the page's JavaScript globals.\n"
            "Prefer the tool's documented accessibility-refs actions. Finish the task in as few steps as possible.\n\n"
            "Tool manual:\n%s") % (tool.name, invoke, manual[:9000])

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", choices=["navigera", "agent-browser", "both"], required=True)
    ap.add_argument("--tasks", default="click-button")
    ap.add_argument("--tasks-file", help="newline-separated task list; overrides --tasks")
    ap.add_argument("--offset", type=int, default=0, help="start index into the task list")
    ap.add_argument("--count", type=int, default=0, help="max tasks from offset (0 = all)")
    ap.add_argument("--miniwob-dir", help="path to a miniwob-plusplus clone (default: sibling clone)")
    ap.add_argument("--seeds", type=int, default=1)
    ap.add_argument("--max-steps", type=int, default=10)
    ap.add_argument("--max-spend", type=float, default=5.00)
    ap.add_argument("--model", default="deepseek-ai/DeepSeek-V4.1-Flash")
    ap.add_argument("--port", type=int, default=8877)
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "out"))
    ap.add_argument("--nav-bin", default="navigera")
    ap.add_argument("--ab-bin", default="agent-browser")
    ap.add_argument("--scripted-policy", action="store_true", help="no model calls; harness self-check")
    ap.add_argument("--start-tool", default="navigera")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    html_root = os.path.join(args.miniwob_dir, "miniwob", "html") if args.miniwob_dir else HTML_ROOT
    handler = partial(SimpleHTTPRequestHandler, directory=html_root)
    httpd = ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    base_url = f"http://127.0.0.1:{args.port}"

    if args.tasks_file:
        tasks = [l.strip() for l in open(args.tasks_file) if l.strip()]
    else:
        tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
        if tasks == ["all"]:
            skip = set(l.strip() for l in open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "canvas-tasks.txt")))
            tasks = sorted(f[:-5] for f in os.listdir(os.path.join(html_root, "miniwob")) if f.endswith(".html") and f not in skip)
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
    jpath = os.path.join(args.out, "miniwob-episodes.jsonl")
    n = 0
    for ti, task in enumerate(tasks):
        order = tools if ti % 2 == 0 else list(reversed(tools))
        if order[0] != first and len(tools) > 1:
            order = list(reversed(order))
        for seed in range(args.seeds):
            for tname in order:
                tool = Tool(tname, args.nav_bin, args.ab_bin, session=f"mw-{tname}")
                rec = run_episode(tool, task, seed, args.model, api_key, ledger, args.max_spend,
                                  args.max_steps, base_url, args.scripted_policy)
                with open(jpath, "a") as f:
                    f.write(json.dumps(rec) + "\n")
                n += 1
                print(f"[{n}] {rec['tool']} {task} seed={seed} reward={rec['reward']} steps={rec['steps']} "
                      f"cost={rec['cost']:.4f} wall={rec['wall_s']}s err={rec['error']}", flush=True)
    print(f"ledger: computed=${ledger['cost']:.4f} retained=${ledger['retained']:.4f} "
          f"requests={ledger['requests']} tokens={ledger['tokens']}")

if __name__ == "__main__":
    main()
