#!/usr/bin/env python3
"""T-0069 primavera M3 exit bench: an agent writes and repairs web tests,
primavera (next 76d22b9) vs Playwright Test, same model, same shell-tool loop.

Loop (T-0022/T-0046 lineage): fresh context per trial, ONE shell command per
model turn, up to --max-turns. The agent works in a per-trial dir against a
local Acme Supply site (site/server.py, from primavera bench/suite40 +
make_site.py changes). After the agent declares TASK-DONE (or turns run out)
the harness scores harness-side: the trial's solution must PASS on the clean
site and FAIL on the same site with the task's feature broken (--broken).
Repair tasks (R1-R4): agent phase runs the CHANGED site (--change rN); the
seed test is the reference test that passed pre-change; scoring needs
pass-on-changed AND fail-on-changed+broken.

Model: SiliconFlow chat completions (OpenAI-compatible), temperature 0,
max_tokens 1024. Spend guard + reserve/settle/unknown WAL identical to
T-0046 (cumulative across ledger files in the out dir, fail-closed).
Model-executed shell never sees *_API_KEY. Prompt gate pins each arm's
manual bytes (primavera `skill` output / playwright-skill.md) before the
first paid request.

Self-test (no model, zero spend):
  python3 harness-t0069_run.py --arm primavera --self-test --tasks R1,W1,W3
"""
import argparse, glob, hashlib, json, os, re, shutil, signal, subprocess, sys, time, urllib.error, urllib.request

PRICE_IN, PRICE_CACHED, PRICE_OUT = 0.15, 0.003, 0.60  # $/1M, SiliconFlow deepseek-ai/DeepSeek-V4.1-Flash
CTX_MAX, MAX_TOKENS = 1_049_000, 1024
REQ_MAX_USD = CTX_MAX * PRICE_IN / 1e6 + MAX_TOKENS * PRICE_OUT / 1e6  # 0.15796
OUT_REC_CAP = 8192
REQUEST_DEADLINE_S = 180
CMD_TIMEOUT_S = 180
TRIAL_WALL_CAP_S = 1800
DONE_CMD = "echo TASK-DONE"

# Prompt-gate pins (verified before the FIRST paid request; fail-closed):
# primavera arm manual = `primavera skill` of the next 3aa36db build (7438 B; lead 2026-10-26: CI builds next head 3aa36db - 76d22b9 is not public).
EXPECTED_PV_SKILL_SHA256 = "322c5be977257903aa6bb3f3b1c7220894e09242427d4be800d2d41f26672c7a"
# playwright arm manual = playwright-skill.md (drafted from Playwright docs; 6154 B).
EXPECTED_PW_SKILL_SHA256 = "6e4564b7a74852086fdcb3c08e159dc62ba83c8638b289b2f212ab349a4c43fc"

FIXTURES = {
    "coupon": "a valid coupon code is SAVE10 (10% off)",
    "creds": "a registered account exists: demo@acme.test / hunter2",
    "card": "checkout accepts card number 4242424242424242 with any future expiry and any CVC",
    "upload": "the support form accepts a file upload; create any small file to attach",
}

# id -> (kind, intent, broken feature, site --change, fixtures, solution-of)
TASKS = {
    "W1": ("write", "Search narrows the product list: on the shop page, searching for a product "
           "name narrows the grid to matching products.", "search", None, [], None),
    "W2": ("write", "Sorting by price shows the cheapest first: choosing price-ascending order on "
           "the shop page lists products cheapest first.", "sort", None, [], None),
    "W3": ("write", "Adding a product from the grid counts it in the cart: a product's add control "
           "on the shop grid increments the cart count.", "add-cart", None, [], None),
    "W4": ("write", "The cart shows line items and the subtotal: with products added, the cart page "
           "lists each line item and a consistent subtotal.", "add-cart", None, [], None),
    "W5": ("write", "A valid coupon lowers the total: with a product in the cart, applying a valid "
           "coupon on the cart page shows a discount and lowers the total.", "coupon", None, ["coupon"], None),
    "W6": ("write", "Signing in shows the order history: valid credentials on the sign-in page "
           "reach an account page that lists past orders.", "signin", None, ["creds"], None),
    "W7": ("write", "Filing a support request with an attachment: the support form accepts a "
           "request including a file upload and confirms with a ticket id.", "tickets", None, ["upload"], None),
    "W8": ("write", "Placing an order end to end: with a product in the cart, completing checkout "
           "places the order and shows an order id.", "checkout", None, ["card"], None),
    "R1": ("repair", "its original intent: adding a product from the grid counts it in the cart",
           "add-cart", "r1", [], "r1.pv"),
    "R2": ("repair", "its original intent: opening a product from the grid shows the product detail",
           "detail", "r2", [], "r2.pv"),
    "R3": ("repair", "its original intent: placing an order end to end",
           "checkout", "r3", ["card"], "r3.pv"),
    "R4": ("repair", "its original intent: searching narrows the product list",
           "search", "r4", [], "r4.pv"),
}

class BudgetStop(Exception):
    pass

class AuthFatal(Exception):
    pass

class RequestDeadline(Exception):
    pass

def _req_alarm(signum, frame):
    raise RequestDeadline(f"absolute request deadline ({REQUEST_DEADLINE_S}s) exceeded")

def wal_append(ledger, rec):
    path = ledger.get("wal_path")
    if not path:
        return
    rec.setdefault("run", ledger.get("run_id"))
    with open(path, "a") as f:
        f.write(json.dumps(rec) + "\n")
        f.flush()
        os.fsync(f.fileno())

def seed_from_wal(ledger, paths=None):
    """Cumulative guard across ledger files in the out dir (T-0046 policy, verbatim):
    settle (first wins) > unknown > unresolved reserve; mid-file corruption fails
    closed; a torn final record counts as one unknown-billable request."""
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
    if deadline_s is not None and deadline_s <= 0:
        raise EpisodeWallCap("no trial wall budget left for a new request")
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
            raise AuthFatal(f"provider auth rejected the key (HTTP {e.code}) on request {seq} - halting the run")
        raise
    except Exception:
        wal_append(ledger, {"event": "unknown", "req": seq, "t": time.time(), "max_usd": REQ_MAX_USD})
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
    ledger["retained"] -= REQ_MAX_USD
    u = data.get("usage") or {}
    cost = (u.get("prompt_tokens", 0) - u.get("prompt_cache_hit_tokens", 0)) * PRICE_IN / 1e6 \
         + u.get("prompt_cache_hit_tokens", 0) * PRICE_CACHED / 1e6 \
         + u.get("completion_tokens", 0) * PRICE_OUT / 1e6
    wal_append(ledger, {"event": "settle", "req": seq, "t": time.time(), "usage": u, "cost_usd": cost})
    ledger["cost"] += cost
    ledger["tokens"] += u.get("total_tokens", 0)
    ledger["requests"] += 1
    return data["choices"][0]["message"]["content"], u, cost, data["choices"][0].get("finish_reason")

class EpisodeWallCap(Exception):
    pass

CMD_RE = re.compile(r"```(?:bash|sh)?\s*\n(.+?)\n```", re.S)
DSML_RE = re.compile(r"<｜+DSML｜+")

def extract_command(text):
    m = CMD_RE.search(text)
    if m:
        return m.group(1).strip().splitlines()[0].strip()
    for line in text.strip().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            return line
    return ""

def classify_reply(text):
    """kind 'command': a runnable first-line extraction exists. Any shell command
    goes (T-0069 is a shell-tool bench; there is no single-tool invoke prefix).
    Otherwise commandless variants -> re-prompt (cap 2 consecutive)."""
    dsml = bool(DSML_RE.search(text))
    cand = extract_command(text)
    if cand:
        return ("command", cand, dsml)
    if dsml:
        return ("dsml_nocommand", None, dsml)
    return ("commandless", None, dsml)

class Site:
    """One site/server.py process; restart with different --change/--broken."""
    def __init__(self, site_dir, port):
        self.site_dir, self.port = site_dir, port
        self.proc = None
        self.config = None
    def start(self, change=None, broken=None):
        self.stop()
        argv = [sys.executable, os.path.join(self.site_dir, "server.py"), "--port", str(self.port)]
        if change:
            argv += ["--change", change]
        if broken:
            argv += ["--broken", broken]
        env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
        self.proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
        self.config = (change, broken)
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/shop", timeout=2) as r:
                    if r.status == 200:
                        return
            except Exception:
                time.sleep(0.25)
        raise RuntimeError(f"site did not become ready on :{self.port} ({argv})")
    def url(self):
        return f"http://127.0.0.1:{self.port}"
    def stop(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except Exception:
                self.proc.kill()
            self.proc = None

def solution_name(arm):
    return "solution.pv" if arm == "primavera" else "solution.spec.ts"

# Env allowlist for model-executed code under --as-user. The full step env is
# NOT safe to pass: it carries runner-scoped credentials (ACTIONS_RUNTIME_TOKEN
# etc.) that must never reach model-written commands or scoring of model-written
# tests. PATH comes from the caller's env because it carries the arm-specific
# prepend (primavera bin dir / playwright node_modules/.bin).
RESTRICTED_ENV_KEYS = ("PATH", "PLAYWRIGHT_BROWSERS_PATH")

def as_user_prefix(args, env, workdir):
    """Wrap a command to run as the restricted bench user (CI paid runs; local
    self-tests run unwrapped). Only allowlisted variables are passed, with HOME
    forced to the trial workdir: no harness credential and no runner-scoped
    token is reachable from model-executed code."""
    if not getattr(args, "as_user", None):
        return None
    keep = {k: env[k] for k in RESTRICTED_ENV_KEYS if env.get(k)}
    keep["HOME"] = workdir
    keep["BASE_URL"] = env.get("BASE_URL", "")
    keep["TERM"] = "dumb"
    keep["LANG"] = "C.UTF-8"
    return ["sudo", "-n", "-u", args.as_user, "env", "-i"] + \
        [f"{k}={v}" for k, v in keep.items()]

def run_restricted(argv, args, env, workdir, **kw):
    pre = as_user_prefix(args, env, workdir)
    if pre:
        kw.pop("env", None)
        return subprocess.run(pre + argv, **kw)
    kw["env"] = env
    return subprocess.run(argv, **kw)

def run_solution(arm, workdir, base_url, pw_bin_path, args=None):
    """Harness-side scoring run. Returns (passed: bool, tail: str)."""
    sol = os.path.join(workdir, solution_name(arm))
    if not os.path.exists(sol) or os.path.getsize(sol) == 0:
        return (False, "solution file missing or empty")
    env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
    env["BASE_URL"] = base_url
    if arm == "primavera":
        argv = [pw_bin_path, "run", "solution.pv", "--base-url", base_url]
    else:
        argv = ["npx", "playwright", "test", "solution.spec.ts", "--config", "playwright.config.ts", "--reporter=line"]
    try:
        p = run_restricted(argv, args, env, workdir, cwd=workdir,
                           capture_output=True, text=True, timeout=240)
        return (p.returncode == 0, (p.stdout + p.stderr)[-2000:])
    except subprocess.TimeoutExpired:
        return (False, "scoring run timed out (240s)")

def setup_workdir(arm, workdir, args):
    os.makedirs(workdir, exist_ok=True)
    if getattr(args, "as_user", None):
        subprocess.run(["sudo", "-n", "chown", "-R", f"{args.as_user}:{args.as_user}", workdir],
                       check=True, capture_output=True)
    if arm == "playwright":
        with open(os.path.join(workdir, "playwright.config.ts"), "w") as f:
            f.write("import { defineConfig } from '@playwright/test';\n"
                    "export default defineConfig({\n"
                    "  testDir: '.',\n"
                    "  timeout: 30_000,\n"
                    "  use: { baseURL: process.env.BASE_URL || 'http://127.0.0.1:19300', browserName: 'chromium' },\n"
                    "});\n")
        nm = os.path.join(workdir, "node_modules")
        if not os.path.exists(nm):
            os.symlink(os.path.join(args.pw_dir, "node_modules"), nm)
        pj = os.path.join(args.pw_dir, "package.json")
        if os.path.exists(pj):
            shutil.copy(pj, os.path.join(workdir, "package.json"))

def agent_env(arm, base_url, args):
    env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
    env["BASE_URL"] = base_url
    if arm == "primavera":
        env["PATH"] = os.path.dirname(os.path.abspath(args.primavera_bin)) + os.pathsep + env.get("PATH", "")
    else:
        env["PATH"] = os.path.join(args.pw_dir, "node_modules", ".bin") + os.pathsep + env.get("PATH", "")
    return env

def system_prompt(arm, manual, args):
    tool_line = ("The primavera binary is on PATH. Run a test with: primavera run solution.pv --base-url $BASE_URL"
                 if arm == "primavera" else
                 "Playwright Test is installed. Run a test with: npx playwright test solution.spec.ts "
                 "(the config reads $BASE_URL). Write specs as solution.spec.ts.")
    return ("You are an agent that writes and repairs web tests. You have a shell: reply with EXACTLY ONE "
            "shell command per turn in a single fenced block; no prose outside the block.\n"
            "The site under test runs at $BASE_URL (environment variable). Your working directory is the "
            "trial directory; save your test file there.\n" + tool_line + "\n\nTool manual:\n" + manual)

def task_prompt(task_id, arm):
    kind, intent, broken, change, fx, seed = TASKS[task_id]
    sol = solution_name(arm)
    facts = "\n".join(f"- {FIXTURES[k]}" for k in fx)
    facts = ("Fixture facts:\n" + facts + "\n") if fx else ""
    if kind == "write":
        return (f"Target: $BASE_URL - the Acme Supply shop (product grid, search, sort, cart, coupon, "
                f"sign-in, support form, checkout).\n"
                f"Task {task_id}: {intent}\n{facts}\n"
                f"Write the test as {sol} in the current directory and run it until it passes against the "
                f"target. It must genuinely exercise the feature: the same test will afterwards be run "
                f"against a build where that feature is broken, and it must FAIL there.\n"
                f"When the test passes, reply with the single command: {DONE_CMD}")
    return (f"The test file {sol} in the current directory passed against this shop. The site has since "
            f"changed the way real sites change, and the test now fails. Repair it so it passes again, "
            f"keeping {intent}. It must still genuinely exercise the same feature: it will afterwards be "
            f"run against a build where that feature is broken, and it must FAIL there.\n{facts}\n"
            f"When the test passes, reply with the single command: {DONE_CMD}")

def run_trial(arm, task_id, trial, args, ledger, site):
    kind, intent, broken, change, fx, seed = TASKS[task_id]
    rec = {"task": task_id, "kind": kind, "arm": arm, "trial": trial, "turns": 0,
           "done_declared": False, "pass_clean": None, "pass_broken": None, "score": 0,
           "tokens": 0, "cost": 0.0, "wall_s": 0.0, "error": None}
    t0 = time.time()
    workdir = os.path.join(args.out, f"work-{ledger['run_id']}-{task_id}-t{trial}")
    def rem():
        r = TRIAL_WALL_CAP_S - (time.time() - t0)
        if r <= 0:
            raise EpisodeWallCap(f"trial wall cap ({TRIAL_WALL_CAP_S}s)")
        return r
    try:
        setup_workdir(arm, workdir, args)
        if kind == "repair":
            src = (os.path.join(args.ref_dir, "pv", seed) if arm == "primavera"
                   else os.path.join(args.ref_dir, "pw", "tests", seed.replace(".pv", ".spec.ts")))
            shutil.copy(src, os.path.join(workdir, solution_name(arm)))
        site.start(change=change)
        base_url = site.url()
        if args.self_test:
            pv_map = {"R1": "r1.pv", "R2": "r2.pv", "R3": "r3.pv", "R4": "r4.pv",
                      "W1": "ref-r4.pv", "W3": "ref-r1.pv"}
            pw_map = {"R1": "pw-r1.spec.ts", "R2": "pw-r2.spec.ts",
                      "R3": "pw-r3.spec.ts", "R4": "pw-r4.spec.ts"}
            st = (pv_map if arm == "primavera" else pw_map).get(task_id)
            if not st:
                rec["error"] = "self-test: no seeded solution for this task"
                return rec
            shutil.copy(os.path.join(args.selftest_dir, st), os.path.join(workdir, solution_name(arm)))
            rec["done_declared"] = True
        else:
            messages = [{"role": "system", "content": args.system_prompt_text},
                        {"role": "user", "content": task_prompt(task_id, arm)}]
            reprompts = 0
            for turn in range(1, args.max_turns + 1):
                rem()
                text, usage, cost, finish_reason = chat(args.api_key, args.model, messages, ledger,
                                                        args.max_spend, deadline_s=rem())
                rec["tokens"] += usage.get("total_tokens", 0)
                rec["cost"] += cost
                rec.setdefault("finish_reasons", []).append(finish_reason)
                kind_r, cmd, dsml = classify_reply(text)
                if dsml:
                    rec["dsml_turns"] = rec.get("dsml_turns", 0) + 1
                if kind_r != "command":
                    rec.setdefault("commands", []).append({"turn": turn, "cmd": None,
                        "model_text": text[:400], "reply_kind": kind_r})
                    if reprompts < 2:
                        reprompts += 1
                        messages.append({"role": "assistant", "content": text})
                        messages.append({"role": "user", "content":
                            "Your reply contained no shell command. Reply with EXACTLY ONE shell command "
                            "in a single fenced block and nothing else."})
                        continue
                    rec["error"] = f"model produced no command after 2 re-prompts (last: {kind_r})"
                    break
                reprompts = 0
                rec["turns"] = turn
                if cmd.strip() == DONE_CMD:
                    rec["done_declared"] = True
                    rec.setdefault("commands", []).append({"turn": turn, "cmd": cmd, "rc": 0})
                    break
                env = agent_env(arm, base_url, args)
                pre = as_user_prefix(args, env, workdir)
                if pre:
                    r = subprocess.run(pre + ["bash", "-c", cmd], capture_output=True, text=True,
                                       timeout=min(CMD_TIMEOUT_S, rem()), cwd=workdir)
                else:
                    r = subprocess.run(cmd, shell=True, capture_output=True, text=True,
                                       timeout=min(CMD_TIMEOUT_S, rem()), cwd=workdir, env=env)
                rec.setdefault("commands", []).append({"turn": turn, "cmd": cmd, "rc": r.returncode,
                    "out": (r.stdout + r.stderr)[:OUT_REC_CAP], "finish_reason": finish_reason})
                messages.append({"role": "assistant", "content": text})
                messages.append({"role": "user", "content":
                    f"Command output (truncated):\n{(r.stdout + r.stderr)[:3000]}"})
        # Scoring (harness-side; no model): pass on the trial's site config, then
        # must FAIL with the task's feature broken on top of the same config.
        ok1, tail1 = run_solution(arm, workdir, base_url, args.primavera_bin, args)
        rec["pass_clean"] = ok1
        rec["clean_tail"] = tail1[-400:]
        site.start(change=change, broken=broken)
        ok2, tail2 = run_solution(arm, workdir, site.url(), args.primavera_bin, args)
        rec["pass_broken"] = ok2
        rec["broken_tail"] = tail2[-400:]
        rec["score"] = 1 if (ok1 and not ok2) else 0
    except AuthFatal:
        raise
    except (EpisodeWallCap, BudgetStop) as e:
        rec["error"] = str(e)
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
    finally:
        site.stop()
    rec["wall_s"] = round(time.time() - t0, 1)
    return rec

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", choices=["primavera", "playwright"], required=True)
    ap.add_argument("--tasks", default="W1,W2,W3,W4,W5,W6,W7,W8,R1,R2,R3,R4")
    ap.add_argument("--trials", type=int, default=3)
    ap.add_argument("--max-turns", type=int, default=30)
    ap.add_argument("--max-spend", type=float, default=5.00)
    ap.add_argument("--model", default="deepseek-ai/DeepSeek-V4.1-Flash")
    ap.add_argument("--port", type=int, default=19500)
    ap.add_argument("--site-dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "site"))
    ap.add_argument("--ref-dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "ref-tests"))
    ap.add_argument("--selftest-dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "selftest-solutions"))
    ap.add_argument("--pw-dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "pw"))
    ap.add_argument("--pw-config", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "ref-tests", "pw", "playwright.config.ts"))
    ap.add_argument("--primavera-bin", default="primavera")  # PATH; CI passes the built binary explicitly
    ap.add_argument("--pw-skill", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "playwright-skill.md"))
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "out"))
    ap.add_argument("--as-user", default="", help="run model commands + scoring of model-written code as this user via sudo -n (CI paid runs)")
    ap.add_argument("--self-test", action="store_true", help="no model calls; score seeded solutions")
    ap.add_argument("--slice-mod", type=int, default=1, help="split the task list into N deterministic slices")
    ap.add_argument("--slice-index", type=int, default=0, help="0-based slice index (tasks[i] where i %% mod == index)")
    ap.add_argument("--uncapped", action="store_true", help="disable the dollar spend refusal (WAL unchanged); requires explicit operator authorization")
    ap.add_argument("--wall-budget-minutes", type=float, default=0,
                    help="stop STARTING new trials when elapsed + trial wall cap would exceed this budget; in-flight trial finishes, remaining trials are reported as DRAINED (0 = no budget)")
    ap.add_argument("--skip", default="", help="comma-separated TASK:TRIAL pairs to skip (resume after a drain; every trial still runs exactly once across dispatches)")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    args.api_key = os.environ.pop("SILICONFLOW_API_KEY", "")  # unsetenv: keep it out of /proc/self/environ
    if not args.self_test and not args.api_key:
        sys.exit("SILICONFLOW_API_KEY not set (or use --self-test)")
    RUN_ID = f"{args.arm}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{os.getpid()}"
    ledger = {"cost": 0.0, "retained": 0.0, "tokens": 0, "requests": 0,
              "uncapped": bool(args.uncapped),
              "wal_path": os.path.join(args.out, f"request-ledger-{RUN_ID}.jsonl"), "run_id": RUN_ID}
    seed_from_wal(ledger)

    # Prompt gate: pin the arm's manual bytes, embed verbatim, write the assembled
    # system prompt to the out dir BEFORE the first paid request (fail-closed).
    if args.arm == "primavera":
        manual_bytes = subprocess.run([args.primavera_bin, "skill"], capture_output=True).stdout
        expected = EXPECTED_PV_SKILL_SHA256
    else:
        if not os.path.exists(args.pw_skill):
            sys.exit(f"GATE: playwright skill missing at {args.pw_skill} - refusing paid work")
        manual_bytes = open(args.pw_skill, "rb").read()
        expected = EXPECTED_PW_SKILL_SHA256
    actual = hashlib.sha256(manual_bytes).hexdigest()
    if actual != expected:
        sys.exit(f"GATE: {args.arm} manual sha256 {actual} != pinned {expected} "
                 f"({len(manual_bytes)}B) - refusing paid work (fail-closed)")
    args.system_prompt_text = system_prompt(args.arm, manual_bytes.decode(), args)
    if manual_bytes.decode() not in args.system_prompt_text:
        sys.exit(f"GATE: assembled {args.arm} prompt does not embed the pinned manual verbatim - refusing paid work")
    spb = args.system_prompt_text.encode()
    spath = os.path.join(args.out, f"system-prompt-{args.arm}.txt")
    with open(spath, "w") as f:
        f.write(args.system_prompt_text)
    print(f"GATE-OK {args.arm}: manual sha256 {actual} pinned-match; prompt bytes={len(spb)} "
          f"sha256={hashlib.sha256(spb).hexdigest()} file={spath}", flush=True)

    tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
    if args.slice_mod > 1:
        tasks = [t for i, t in enumerate(tasks) if i % args.slice_mod == args.slice_index]
        print(f"SLICE {args.slice_index + 1}/{args.slice_mod}: tasks={','.join(tasks)}", flush=True)
    skip = {p.strip() for p in args.skip.split(",") if p.strip()}
    site = Site(args.site_dir, args.port)
    jpath = os.path.join(args.out, f"t0069-episodes-{RUN_ID}.jsonl")
    n = 0
    t_start = time.time()
    drained = []
    stop = False
    for task_id in tasks:
        for trial in range(1, args.trials + 1):
            if f"{task_id}:{trial}" in skip:
                print(f"[skip] {task_id} t{trial} already completed in an earlier dispatch", flush=True)
                continue
            if stop or (args.wall_budget_minutes > 0 and
                        (time.time() - t_start) + TRIAL_WALL_CAP_S > args.wall_budget_minutes * 60):
                stop = True
                drained.append(f"{task_id}:{trial}")
                continue
            rec = run_trial(args.arm, task_id, trial, args, ledger, site)
            rec["run"] = RUN_ID
            with open(jpath, "a") as f:
                f.write(json.dumps(rec) + "\n")
                f.flush()
                os.fsync(f.fileno())
            n += 1
            print(f"[{n}] {args.arm} {task_id} t{trial} score={rec['score']} "
                  f"pass_clean={rec['pass_clean']} pass_broken={rec['pass_broken']} "
                  f"turns={rec['turns']} cost={rec['cost']:.4f} wall={rec['wall_s']}s err={rec['error']}", flush=True)
    if drained:
        dpath = os.path.join(args.out, f"t0069-drained-{RUN_ID}.json")
        with open(dpath, "w") as f:
            json.dump({"run": RUN_ID, "drained": drained,
                       "resume_with": "--skip " + ",".join(drained)}, f, indent=1)
        print(f"DRAIN: wall budget reached; {len(drained)} trials deferred "
              f"(resume this slice in a later dispatch; every trial runs exactly once). "
              f"deferred={','.join(drained)} file={dpath}", flush=True)
    print(f"ledger: computed=${ledger['cost']:.4f} retained=${ledger['retained']:.4f} "
          f"requests={ledger['requests']} tokens={ledger['tokens']}")

if __name__ == "__main__":
    try:
        main()
    except AuthFatal as e:
        print(f"AUTH-FATAL: {e} - run halted; completed trials and all request accounting are durable", flush=True)
        sys.exit(2)
