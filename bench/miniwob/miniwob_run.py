#!/usr/bin/env python3
"""MiniWoB++ model-driven tool comparison harness (T-0022; T-0046 CI revision).

T-0046 changes vs T-0022 (CI-minimal: no fence/resume/stop-new/wall-cap -
single-shot job on a fresh runner; cancellation = cancel the workflow run):
full manuals (no 9000-char cut; fail-closed if missing/short), per-cell prompt
bytes+sha256 written to the out dir and job log BEFORE the first paid request,
DSML/fenced commandless replies get the re-prompt (shared cap, separate counters;
a command found inside markup is run, flagged), per-command output records up to
8192 chars (was 400), durable per-call request ledger (reserve/unknown/settle WAL,
fsync per event, cumulative guard seeded fail-closed), *_API_KEY stripped from the
model-executed shell env, run-namespaced output files and session names.

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
import argparse, json, os, re, signal, subprocess, sys, threading, time, urllib.error, urllib.request
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

OUT_REC_CAP = 8192  # T-0046: per-command output record cap (was 400); stated in the report

# T-0046 prompt gate pins (verified before the FIRST paid request; fail-closed):
# navigera manual = navigera/skills/navigera/SKILL.md @ 2033910 (13312 B; the CI build
# overlays exactly this blob and prints its sha256 at build time).
EXPECTED_NAV_MANUAL_SHA256 = "7e08896c10703f0b265974a09b7f4a3bf2bfa5125c31a9da9276eff93d343572"
# agent-browser cell manual = `agent-browser --help` of npm agent-browser@0.39.1.
EXPECTED_AB_HELP_SHA256 = "b8d7648fd19a0fd8c136addc6eff8f7511f34587db15f840c2930a691ee0c8ca"
# Documented variation: the invoke token (binary path + run-namespaced session) differs
# per run/cell; everything else in the prompt is fixed template + hash-pinned manual.

REQUEST_DEADLINE_S = 180   # absolute per-request wall deadline (SIGALRM); defense in depth -
                           # do NOT rely on urlopen's socket timeout alone (observed firing very late)
EPISODE_WALL_CAP_S = 640   # per-episode wall cap, just above the 600s protocol timer

class RequestDeadline(Exception):
    pass

class EpisodeWallCap(Exception):
    pass

class AuthFatal(Exception):
    """Provider rejected the key (401/403): halt the whole cell after the first
    auth failure - never burn 336 episodes repeating rejected requests."""
    pass

def _req_alarm(signum, frame):
    raise RequestDeadline(f"absolute request deadline ({REQUEST_DEADLINE_S}s) exceeded")

NAVIGERA_MANUAL = os.environ.get("NAVIGERA_SKILL_MD", os.path.join(os.path.dirname(os.path.abspath(__file__)), "navigera-skill.md"))

START_JS = """() => { Math.seedrandom(%s); core.EPISODE_MAX_TIME = %d; core.startEpisodeReal(); return 'started'; }"""
QUERY_JS = "() => document.getElementById('query').innerText"
POLL_JS = "() => JSON.stringify({done: WOB_DONE_GLOBAL, reward: WOB_REWARD_GLOBAL, raw: WOB_RAW_REWARD_GLOBAL})"

class Tool:
    def __init__(self, name, nav_bin, ab_bin, session="mw"):
        self.name, self.session = name, session
        self.nav_bin, self.ab_bin = nav_bin, ab_bin
        if name == "navigera":
            self.invoke = "%s -s %s" % (nav_bin, session)
        else:
            self.invoke = "%s --session %s" % (ab_bin, session)
    def _run(self, argv, timeout=120):
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return (p.stdout + p.stderr).strip()
    def goto(self, url, timeout=180):
        if self.name == "navigera":
            self._run([self.nav_bin, "-s", self.session, "start"], timeout=timeout)
            return self._run([self.nav_bin, "-s", self.session, "goto", url], timeout=min(120, timeout))
        return self._run([self.ab_bin, "--session", self.session, "open", url], timeout=timeout)
    def eval(self, js, timeout=60):
        if self.name == "navigera":
            return self._run([self.nav_bin, "-s", self.session, "--raw", "eval", js], timeout)
        # agent-browser eval evaluates an expression; wrap arrow functions as an IIFE
        expr = "(" + js + ")()" if js.lstrip().startswith("() =>") or js.lstrip().startswith("async () =>") else js
        return self._run([self.ab_bin, "--session", self.session, "eval", expr], timeout)
    def observe(self, limit=12000, timeout=120):
        if self.name == "navigera":
            out = self._run([self.nav_bin, "-s", self.session, "--raw", "ax", "--limit", "200"], timeout=timeout)
        else:
            out = self._run([self.ab_bin, "--session", self.session, "snapshot", "-i"], timeout=timeout)
        return out[:limit]
    def env(self):
        # T-0046 containment: the model-executed shell never sees provider credentials.
        env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
        if self.name == "agent-browser":
            env["AGENT_BROWSER_SESSION"] = self.session
        return env

def wal_append(ledger, rec):
    """T-0046: durable write-ahead request ledger - reserve before every network
    call, settle per response; fsync per event so artifacts carry per-call accounting
    even if the job is cancelled mid-run."""
    path = ledger.get("wal_path")
    if not path:
        return
    rec.setdefault("run", ledger.get("run_id"))
    with open(path, "a") as f:
        f.write(json.dumps(rec) + "\n")
        f.flush()
        os.fsync(f.fileno())

def seed_from_wal(ledger, paths=None):
    """T-0046: cumulative guard reconstructed across ledger files in the out dir.
    Key = (run, req). Precedence per key: settle (first wins) > unknown > unresolved
    reserve. Mid-file corruption fails closed; a torn final record counts as one
    unknown-billable request (read-only, never truncated). Fresh CI out dirs seed zero."""
    import glob
    paths = paths if paths is not None else sorted(glob.glob(os.path.join(os.path.dirname(ledger["wal_path"]), "request-ledger*.jsonl")))
    settled_cost, unknown, reserved = {}, set(), set()
    for path in sorted(paths):
        if not os.path.exists(path):
            continue
        run_tag = os.path.basename(path)
        with open(path, "rb") as f:
            raw_lines = f.readlines()
        torn_at = None
        for i, line in enumerate(raw_lines):
            try:
                r = json.loads(line)
                if not isinstance(r, dict) or "event" not in r or "req" not in r or not isinstance(r["req"], int):
                    raise ValueError("malformed row")
            except Exception:
                torn_at = i
                break
            key = (r.get("run", run_tag), r["req"])
            ev = r["event"]
            if ev == "reserve":
                if key not in settled_cost and key not in unknown:
                    reserved.add(key)
            elif ev == "unknown":
                if key not in settled_cost:
                    reserved.discard(key)
                    unknown.add(key)
            elif ev == "settle":
                if key not in settled_cost:
                    settled_cost[key] = float(r.get("cost_usd") or 0.0)
                    reserved.discard(key)
                    unknown.discard(key)
        if torn_at is not None and torn_at < len(raw_lines) - 1:
            sys.exit(f"WAL corrupt mid-file in {path} - refusing paid work (fail-closed)")
        if torn_at is not None:
            print(f"WAL: torn final line in {path} (read-only, NOT truncated) - counts as unknown-billable", flush=True)
            unknown.add((run_tag, -1))
    ledger["req_seq"] = 0
    ledger["cost"] = sum(settled_cost.values())
    ledger["retained"] = REQ_MAX_USD * (len(reserved) + len(unknown))
    print(f"GUARD-STATE cumulative_cost={ledger['cost']:.6f} retained={ledger['retained']:.6f} "
          f"exposure={ledger['cost'] + ledger['retained']:.6f} files={len(paths)}", flush=True)

def chat(api_key, model, messages, ledger, max_spend, deadline_s=None):
    """One guarded request. Returns (text, usage) or raises BudgetStop/RuntimeError.
    deadline_s caps the absolute SIGALRM timer below REQUEST_DEADLINE_S so an
    episode's remaining wall budget is enforced inside the blocking call."""
    if deadline_s is not None and deadline_s <= 0:
        raise EpisodeWallCap("no episode wall budget left for a new request")  # before any reservation
    if not ledger.get("uncapped") and ledger["cost"] + ledger["retained"] + REQ_MAX_USD > max_spend:
        raise BudgetStop(f"budget guard: {ledger['cost']:.4f} computed + {ledger['retained']:.4f} retained + {REQ_MAX_USD:.5f} max > {max_spend}")
    ledger["retained"] += REQ_MAX_USD
    ledger["req_seq"] = ledger.get("req_seq", 0) + 1
    seq = ledger["req_seq"]
    wal_append(ledger, {"event": "reserve", "req": seq, "t": time.time(), "max_usd": REQ_MAX_USD})
    body = json.dumps({"model": model, "temperature": 0, "max_tokens": MAX_TOKENS, "messages": messages}).encode()
    req = urllib.request.Request("https://api.siliconflow.com/v1/chat/completions", data=body,
        headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"})
    timer = REQUEST_DEADLINE_S if deadline_s is None else min(REQUEST_DEADLINE_S, deadline_s)
    signal.signal(signal.SIGALRM, _req_alarm)
    signal.setitimer(signal.ITIMER_REAL, timer)
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            data = json.loads(r.read())
    except urllib.error.HTTPError as e:
        wal_append(ledger, {"event": "unknown", "req": seq, "t": time.time(), "max_usd": REQ_MAX_USD, "http_status": e.code})
        if e.code in (401, 403):
            raise AuthFatal(f"provider auth rejected the key (HTTP {e.code}) on request {seq} - halting the cell")
        raise  # reservation retained (unknown charge outcome)
    except Exception:
        wal_append(ledger, {"event": "unknown", "req": seq, "t": time.time(), "max_usd": REQ_MAX_USD})
        raise  # reservation retained (unknown charge outcome)
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
    ledger["retained"] -= REQ_MAX_USD  # settled: release the reservation, add the computed cost
    u = data.get("usage") or {}
    cost = (u.get("prompt_tokens", 0) - u.get("prompt_cache_hit_tokens", 0)) * PRICE_IN / 1e6 \
         + u.get("prompt_cache_hit_tokens", 0) * PRICE_CACHED / 1e6 \
         + u.get("completion_tokens", 0) * PRICE_OUT / 1e6
    wal_append(ledger, {"event": "settle", "req": seq, "t": time.time(), "usage": u, "cost_usd": cost})
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

DSML_RE = re.compile(r"<\uff5c+DSML\uff5c+")

def classify_reply(text, invoke_prefix):
    """T-0046: one reply -> (kind, cmd, dsml).

    kind 'command': extracted command contains the exact invoke prefix -> run.
    kind 'recovered': no runnable first-line extraction, but a line contains the
        invoke prefix (inside DSML markup or a later fence line) -> run the
        substring from the prefix to end-of-line, trailing markup stripped.
    kind 'dsml_nocommand' / 'fenced_nocommand' / 'commandless': nothing runnable
        -> re-prompt (shared cap with pattern-A, counted separately).
    """
    dsml = bool(DSML_RE.search(text))
    cand = extract_command(text)
    if cand and invoke_prefix in cand:
        return ("command", cand, dsml)
    for line in text.splitlines():
        i = line.find(invoke_prefix)
        if i >= 0:
            cmd = re.split(r"</?\uff5c", line[i:], maxsplit=1)[0].strip().rstrip("`").strip()
            if cmd:
                return ("recovered", cmd, dsml)
    if dsml:
        return ("dsml_nocommand", None, dsml)
    if cand:
        return ("fenced_nocommand", None, dsml)
    return ("commandless", None, dsml)

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
    ep_deadline = t0 + EPISODE_WALL_CAP_S
    def rem():
        r = ep_deadline - time.time()
        if r <= 0:
            raise EpisodeWallCap(f"episode wall cap ({EPISODE_WALL_CAP_S}s absolute)")
        return r
    try:
        tool.goto(f"{base_url}/miniwob/{task}.html", timeout=min(180, rem()))
        time.sleep(min(1.0, rem()))
        out = tool.eval(START_JS % (json.dumps(f"{task}-{seed}"), EPISODE_MAX_TIME_MS), timeout=min(60, rem()))
        if "started" not in out:
            raise RuntimeError(f"episode start failed: {out[:200]}")
        instruction = tool.eval(QUERY_JS, timeout=min(60, rem())).strip().strip('"')
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
                rem()
                obs = tool.observe(timeout=min(120, rem()))
                messages.append({"role": "user", "content":
                    f"Task instruction: {instruction}\n\nCurrent page observation ({tool.name}):\n{obs}\n\n"
                    f"Reply with exactly one shell command in a fenced block (step {step}/{max_steps})."})
                text, usage, cost, finish_reason = chat(api_key, model, messages, ledger, max_spend, deadline_s=rem())
                rec["tokens"] += usage.get("total_tokens", 0); rec["cost"] += cost
                rec.setdefault("finish_reasons", []).append(finish_reason)
                kind, cmd, dsml = classify_reply(text, tool.invoke)
                if dsml:
                    rec["dsml_turns"] = rec.get("dsml_turns", 0) + 1
                if kind in ("commandless", "dsml_nocommand", "fenced_nocommand"):
                    rec.setdefault("commands", []).append({"step": step, "cmd": None, "model_text": text[:400], "finish_reason": finish_reason, "reply_kind": kind})
                    if reprompts < 2:
                        reprompts += 1
                        key = {"commandless": "pattern_a_reprompts",
                               "dsml_nocommand": "dsml_reprompts",
                               "fenced_nocommand": "fenced_reprompts"}[kind]
                        rec[key] = rec.get(key, 0) + 1
                        messages.append({"role": "assistant", "content": text})
                        messages.append({"role": "user", "content":
                            "Your reply contained no shell command. Reply with EXACTLY ONE shell command "
                            "in a single fenced block and nothing else."})
                        continue
                    rec["error"] = f"model produced no command after 2 re-prompts (last: {kind})"; break
                reprompts = 0
                if kind == "recovered":
                    rec["dsml_recovered_commands" if dsml else "recovered_commands"] = rec.get("dsml_recovered_commands" if dsml else "recovered_commands", 0) + 1
                r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=min(90, rem()), env=tool.env())
                rec.setdefault("commands", []).append({"step": step, "cmd": cmd, "rc": r.returncode, "out": (r.stdout + r.stderr)[:OUT_REC_CAP], "finish_reason": finish_reason})
                messages.append({"role": "assistant", "content": text})
                messages.append({"role": "user", "content": f"Command output (truncated):\n{(r.stdout + r.stderr)[:3000]}"})
                rec["steps"] = step
                poll = parse_js_json(tool.eval(POLL_JS))
                if poll["done"]:
                    break
        poll = parse_js_json(tool.eval(POLL_JS))
        rec.update(done=poll["done"], reward=poll["reward"], raw_reward=poll["raw"])
    except AuthFatal:
        raise  # auth failure halts the cell, not just the episode
    except EpisodeWallCap as e:
        rec["error"] = str(e)
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
        if not os.path.exists(NAVIGERA_MANUAL):
            sys.exit(f"navigera manual missing at {NAVIGERA_MANUAL} - refusing paid work (fail-closed)")
        manual = open(NAVIGERA_MANUAL).read()
        if len(manual) < 1000:
            sys.exit(f"navigera manual suspiciously short ({len(manual)}B) - refusing paid work (fail-closed)")
    else:
        manual = subprocess.run([tool.ab_bin, "--help"], capture_output=True, text=True).stdout
        if len(manual) < 1000:
            sys.exit(f"agent-browser --help suspiciously short ({len(manual)}B) - refusing paid work (fail-closed)")
    if tool.name == "navigera":
        invoke = "%s -s %s" % (tool.nav_bin, tool.session)
    else:
        invoke = "%s --session %s" % (tool.ab_bin, tool.session)
    return ("You drive a web browser with %s, one shell command per turn, to complete a small web task.\n"
            "Rules: reply with EXACTLY ONE shell command in a single fenced block; no prose outside the block.\n"
            "Invoke the tool ONLY as `%s <args...>` - the binary is NOT on PATH; use this exact prefix every time.\n"
            "The browser session is already running; do not start/open a new one. Do not read or reference the page's JavaScript globals.\n"
            "Prefer the tool's documented accessibility-refs actions. Finish the task in as few steps as possible.\n\n"
            "Tool manual:\n%s") % (tool.name, invoke, manual)  # T-0046: full manual, no 9000-char cut

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
    ap.add_argument("--uncapped", action="store_true", help="disable the dollar spend refusal (reserve/settle/unknown WAL logging unchanged); requires explicit operator authorization")
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
    RUN_ID = f"{args.tool}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{os.getpid()}"
    ledger = {"cost": 0.0, "retained": 0.0, "tokens": 0, "requests": 0,
              "uncapped": bool(args.uncapped),
              "wal_path": os.path.join(args.out, f"request-ledger-{RUN_ID}.jsonl"), "run_id": RUN_ID}
    # cumulative guard seeded across any ledger files in the out dir (fail-closed on
    # mid-file corruption); a fresh CI workspace seeds zero
    seed_from_wal(ledger)
    # T-0046 prompt gate (real gate, not just provenance): verify the manual SOURCE
    # bytes against the pinned hash, then verify the ASSEMBLED prompt embeds those exact
    # bytes verbatim - before the first paid request. Only the invoke token (binary
    # path + run-namespaced session) is documented variation. Any mismatch halts the
    # cell before a dollar is spent.
    import hashlib
    for tname in (["navigera", "agent-browser"] if args.tool == "both" else [args.tool]):
        if tname == "navigera":
            if not os.path.exists(NAVIGERA_MANUAL):
                sys.exit(f"GATE: navigera manual missing at {NAVIGERA_MANUAL} - refusing paid work")
            manual_bytes = open(NAVIGERA_MANUAL, "rb").read()
            expected = EXPECTED_NAV_MANUAL_SHA256
        else:
            manual_bytes = subprocess.run([args.ab_bin, "--help"], capture_output=True).stdout
            expected = EXPECTED_AB_HELP_SHA256
        actual = hashlib.sha256(manual_bytes).hexdigest()
        if actual != expected:
            sys.exit(f"GATE: {tname} manual sha256 {actual} != pinned {expected} "
                     f"({len(manual_bytes)}B) - refusing paid work (fail-closed)")
        tool0 = Tool(tname, args.nav_bin, args.ab_bin, session=f"mw-{tname}-{RUN_ID}")
        sp = system_prompt(tool0)
        if manual_bytes.decode() not in sp:
            sys.exit(f"GATE: assembled {tname} prompt does not embed the pinned manual bytes verbatim - refusing paid work")
        spb = sp.encode()
        spath = os.path.join(args.out, f"system-prompt-{tname}.txt")
        with open(spath, "w") as f:
            f.write(sp)
        print(f"GATE-OK {tname}: manual sha256 {actual} pinned-match; prompt bytes={len(spb)} "
              f"sha256={hashlib.sha256(spb).hexdigest()} file={spath}", flush=True)

    tools = ["navigera", "agent-browser"] if args.tool == "both" else [args.tool]
    first = args.start_tool if args.start_tool in tools else tools[0]
    jpath = os.path.join(args.out, f"miniwob-episodes-{RUN_ID}.jsonl")
    n = 0
    for ti, task in enumerate(tasks):
        order = tools if ti % 2 == 0 else list(reversed(tools))
        if order[0] != first and len(tools) > 1:
            order = list(reversed(order))
        for seed in range(args.seeds):
            for tname in order:
                tool = Tool(tname, args.nav_bin, args.ab_bin, session=f"mw-{tname}-{RUN_ID}")
                rec = run_episode(tool, task, seed, args.model, api_key, ledger, args.max_spend,
                                  args.max_steps, base_url, args.scripted_policy)
                rec["run"] = RUN_ID
                with open(jpath, "a") as f:
                    f.write(json.dumps(rec) + "\n")
                    f.flush()
                    os.fsync(f.fileno())
                n += 1
                print(f"[{n}] {rec['tool']} {task} seed={seed} reward={rec['reward']} steps={rec['steps']} "
                      f"cost={rec['cost']:.4f} wall={rec['wall_s']}s err={rec['error']}", flush=True)
    print(f"ledger: computed=${ledger['cost']:.4f} retained=${ledger['retained']:.4f} "
          f"requests={ledger['requests']} tokens={ledger['tokens']}")

if __name__ == "__main__":
    try:
        main()
    except AuthFatal as e:
        print(f"AUTH-FATAL: {e} - cell halted after the first auth rejection; "
              f"completed episodes and all request accounting are durable in the out dir", flush=True)
        sys.exit(2)
