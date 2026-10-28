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

Self-test (no model, zero spend; T-0091: every task seeded in both arms):
  python3 harness-t0069_run.py --arm primavera --self-test
"""
import argparse, glob, hashlib, json, os, re, shutil, signal, subprocess, sys, time, urllib.error, urllib.request

PRICE_IN, PRICE_CACHED, PRICE_OUT = 0.15, 0.003, 0.60  # $/1M, SiliconFlow deepseek-ai/DeepSeek-V4.1-Flash
CTX_MAX = 1_049_000
# T-0096: output-token cap from the environment so a cell can compare caps
# without a code change. 1024 is the round-2 value. Recorded per episode and
# in the run summary.
MAX_TOKENS = int(os.environ.get("BENCH_MAX_TOKENS", "1024"))
REQ_MAX_USD = CTX_MAX * PRICE_IN / 1e6 + MAX_TOKENS * PRICE_OUT / 1e6  # 0.15796
OUT_REC_CAP = 8192
REQUEST_DEADLINE_S = 180
CMD_TIMEOUT_S = 180
TRIAL_WALL_CAP_S = 1800
DONE_CMD = "echo TASK-DONE"

# Prompt-gate pins (verified before the FIRST paid request; fail-closed):
# primavera arm manual = `primavera skill` of the pinned build (T-0069: next 3aa36db,
# 7438 B; T-0091 round 2 + T-0096 cell: primavera next 7026c80, navigera compiled in
# via path dep at cc300d30 (side branch off f37b95c; NOT 0.6.2/69ea463, predates
# 3830c5e and 1e77d29; CI checkouts pin it in t0069-eval.yml and t0096-cell.yml), 7432 B).
# Round 5: primavera next with the rewritten agent skill (5341 B, shorter than
# the Playwright manual), navigera next compiled in; both pinned in t0096-cell.yml.
EXPECTED_PV_SKILL_SHA256 = "dd42592fa25b7e52e461fc274fca5b8b2f1768fdcefb59af32171d1972d3d306"
# The round-2 to round-4 manual, for the record: a4b79287e8ab23a2140f21107afe1c4df9eada6b13db4ee34c8b009bd1209585
# (primavera-skill-7026c80.txt).
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


# T-0091: seeded reference solution for EVERY task in both arms; the preflight
# gate requires all of them green (pass clean, fail broken) before a paid run.
PV_SELFTEST = {"R1": "r1.pv", "R2": "r2.pv", "R3": "r3.pv", "R4": "r4.pv",
               "W1": "ref-w1.pv", "W2": "ref-w2.pv", "W3": "ref-r1.pv",
               "W4": "ref-w4.pv", "W5": "ref-w5.pv", "W6": "ref-w6.pv",
               "W7": "ref-w7.pv", "W8": "ref-w8.pv"}
PW_SELFTEST = {"R1": "pw-r1.spec.ts", "R2": "pw-r2.spec.ts", "R3": "pw-r3.spec.ts",
               "R4": "pw-r4.spec.ts", "W1": "pw-w1.spec.ts", "W2": "pw-w2.spec.ts",
               "W3": "pw-w3.spec.ts", "W4": "pw-w4.spec.ts", "W5": "pw-w5.spec.ts",
               "W6": "pw-w6.spec.ts", "W7": "pw-w7.spec.ts", "W8": "pw-w8.spec.ts"}

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
    parts = usage_parts(u)
    # cost_usd stays the uncached bound (every prompt token at PRICE_IN): it
    # is what the spend cap and the WAL count, so a wrong cache price can only
    # overstate spend. cost_cached_usd prices the cache hits the provider
    # reported; it is the figure arms are compared on.
    cost = parts["prompt"] * PRICE_IN / 1e6 + parts["completion"] * PRICE_OUT / 1e6
    cost_cached = (parts["prompt"] - parts["cached"]) * PRICE_IN / 1e6 + parts["cached"] * PRICE_CACHED / 1e6 \
        + parts["completion"] * PRICE_OUT / 1e6
    wal_append(ledger, {"event": "settle", "req": seq, "t": time.time(), "usage": u, "cost_usd": cost,
                        "cost_cached_usd": cost_cached, **{f"{k}_tokens": v for k, v in parts.items()}})
    ledger["cost"] += cost
    ledger["cost_cached"] = ledger.get("cost_cached", 0.0) + cost_cached
    for k, v in parts.items():
        ledger[f"{k}_tokens"] = ledger.get(f"{k}_tokens", 0) + v
    ledger["tokens"] += u.get("total_tokens", 0)
    ledger["requests"] += 1
    u = dict(u, _parts=parts, _cost_cached=cost_cached)
    return data["choices"][0]["message"]["content"], u, cost, data["choices"][0].get("finish_reason")

def usage_parts(u):
    """Prompt, cache-hit, completion and reasoning tokens of one reply. The
    OpenAI-style field (SiliconFlow) is prompt_tokens_details.cached_tokens;
    DeepSeek's own API says prompt_cache_hit_tokens. Reasoning tokens are part
    of completion_tokens, billed at PRICE_OUT."""
    pd = u.get("prompt_tokens_details") or {}
    cd = u.get("completion_tokens_details") or {}
    prompt = int(u.get("prompt_tokens") or 0)
    cached = int(pd.get("cached_tokens") or u.get("prompt_cache_hit_tokens") or 0)
    return {"prompt": prompt, "cached": min(cached, prompt), "completion": int(u.get("completion_tokens") or 0),
            "reasoning": int(cd.get("reasoning_tokens") or 0)}

class EpisodeWallCap(Exception):
    pass

CMD_RE = re.compile(r"```(?:bash|sh)?\s*\n(.+?)\n```", re.S)
DSML_RE = re.compile(r"<｜+DSML｜+")

def extract_command(text):
    m = CMD_RE.search(text)
    if m:
        # The WHOLE fenced block is the command (multi-line: heredocs, grouped
        # writes). The first-line-only rule amputated heredocs and forced
        # agents into one-line printf rewrites.
        block = m.group(1).strip("\n")
        first = block.splitlines()[0].strip() if block.splitlines() else ""
        # Never execute a fence marker or an empty block. A truncated fenced
        # block previously yielded "```bash" as the command (rc 2).
        return "" if (not first or first.startswith("```")) else block
    stripped = text.strip()
    if stripped.startswith("```"):
        # Truncated fenced block (length cut): no complete command, and do
        # NOT execute its first inner line either - a bare heredoc opener
        # would create an empty solution file.
        return ""
    if DSML_RE.search(stripped):
        # Tool-call markup is not a shell command; never execute its lines.
        return ""
    for line in stripped.splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            return line
    return ""

def classify_reply(text):
    """kind 'command': a runnable extraction exists (the whole fenced block, or
    one unfenced line). Any shell command goes (this is a shell-tool bench;
    there is no single-tool invoke prefix). DSML markup alone is never
    executed: it classifies as dsml_nocommand. Otherwise commandless variants
    -> re-prompt (cap 2 consecutive)."""
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

def chown_workdir(workdir, args):
    """Hand the trial workdir to the bench user. Must run AFTER every
    harness-side write into the workdir (config + seeded solution): the
    harness runs as the invoking user, the bench user owns the dir from
    here on, and every later writer (model commands, scoring runs) already
    executes as the bench user."""
    if getattr(args, "as_user", None):
        subprocess.run(["sudo", "-n", "chown", "-R", f"{args.as_user}:{args.as_user}", workdir],
                       check=True, capture_output=True)

def setup_workdir(arm, workdir, args):
    os.makedirs(workdir, exist_ok=True)
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


def reprompt_empty_length_text():
    """T-0096: empty content with finish_reason=length means the output budget
    was exhausted before any visible content (reasoning consumed it). The
    command-contract re-prompt cannot help an empty reply; this one asks for one
    short command and chunked file writes."""
    return ("Your previous reply was empty: the output token budget was exhausted "
            "before any reply text. Reply with ONE short shell command now, in a "
            "single fenced block. Write files in small appended chunks "
            "(printf or cat >> file), never in one long command.")


def reprompt_dsml_text():
    """DSML markup is never executed; say so once, plainly."""
    return ("Your reply was tool-call markup, not a shell command: no tool-call "
            "markup; one fenced shell block. Reply with EXACTLY ONE shell "
            "command in a single fenced block and nothing else.")


def reprompt_text(arm, attempt):
    """T-0091 stall recovery: attempt 1 is the generic contract reminder;
    attempt 2 repeats the tool's command contract and one minimal worked example."""
    base = ("Your reply contained no shell command. Reply with EXACTLY ONE shell "
            "command in a single fenced block and nothing else.")
    if attempt < 2:
        return base
    if arm == "primavera":
        contract = ("The primavera binary is on PATH. Run a test with: "
                    "primavera run solution.pv --base-url $BASE_URL")
        example = ('cat > solution.pv <<\'EOF\'\n'
                   'base $BASE_URL\n\n'
                   'scenario "shop loads"\n'
                   '  goto /shop\n'
                   '  expect text "Acme Supply"\n'
                   'EOF')
    else:
        contract = ("Playwright Test is installed. Run a test with: "
                    "npx playwright test solution.spec.ts (the config reads $BASE_URL). "
                    "Write specs as solution.spec.ts.")
        example = ('cat > solution.spec.ts <<\'EOF\'\n'
                   "import { test, expect } from \'@playwright/test\';\n"
                   "test(\'shop loads\', async ({ page }) => {\n"
                   "  await page.goto(\'/shop\');\n"
                   "  await expect(page.getByText(\'Acme Supply\')).toBeVisible();\n"
                   '});\n'
                   'EOF')
    return (base + "\n" + contract + "\nOne minimal worked example of a single-turn "
            "command (write your real test the same way, then run it):\n```sh\n"
            + example + "\n```")


_HEREDOC_RE = re.compile(r"<<-?\s*[\'\"]?(\w+)[\'\"]?\n(.*?)\n\1", re.S)
_WRITE_TARGET_RE = re.compile(
    r"(?:>|>>|tee\s+(?:-a\s+)?)[\s\'\"]*(?:\./)?({sol})(?:[\'\"]|\s|$)")

def salvage_partial_solution(arm, workdir, commands):
    """T-0091: when the agent never produced a runnable solution file (turn budget
    exhausted, or no-command termination), extract the most recent solution
    content the model tried to write (heredoc body whose command targets the
    solution file) into a SEPARATE diagnostic file. The scored solution file is
    never created or modified here: the primary score always reflects the file
    the agent actually left. Returns (source_turn, salvaged_path), or None."""
    sol = solution_name(arm)
    path = os.path.join(workdir, sol)
    if os.path.exists(path) and os.path.getsize(path) > 0:
        return None
    target = re.compile(r"(?:>|>>|tee\s+(?:-a\s+)?)[\s\'\"]*(?:\./)?"
                        + re.escape(sol) + r"(?:[\'\"]|\s|$)")
    for entry in reversed(commands):
        cmd = entry.get("cmd")
        if not cmd:
            continue
        m = _HEREDOC_RE.search(cmd)
        if m and target.search(cmd):
            body = m.group(2)
            if body.strip():
                spath = os.path.join(workdir, "salvaged-partial-" + sol)
                with open(spath, "w") as f:
                    f.write(body + "\n")
                return (entry.get("turn"), spath)
    return None

def run_trial(arm, task_id, trial, args, ledger, site):
    kind, intent, broken, change, fx, seed = TASKS[task_id]
    rec = {"task": task_id, "kind": kind, "arm": arm, "trial": trial, "turns": 0,
           "max_tokens": MAX_TOKENS,  # T-0096: the cap this episode ran under
           "done_declared": False, "pass_clean": None, "pass_broken": None, "score": 0,
           "tokens": 0, "cost": 0.0, "cost_cached": 0.0, "prompt_tokens": 0, "cached_tokens": 0,
           "completion_tokens": 0, "reasoning_tokens": 0, "wall_s": 0.0, "error": None}
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
            st = (PV_SELFTEST if arm == "primavera" else PW_SELFTEST).get(task_id)
            if not st:
                rec["error"] = "self-test: no seeded solution for this task"
                return rec
            shutil.copy(os.path.join(args.selftest_dir, st), os.path.join(workdir, solution_name(arm)))
            rec["done_declared"] = True
            chown_workdir(workdir, args)
        else:
            chown_workdir(workdir, args)
            messages = [{"role": "system", "content": args.system_prompt_text},
                        {"role": "user", "content": task_prompt(task_id, arm)}]
            reprompts = 0
            for turn in range(1, args.max_turns + 1):
                rem()
                text, usage, cost, finish_reason = chat(args.api_key, args.model, messages, ledger,
                                                        args.max_spend, deadline_s=rem())
                rec["tokens"] += usage.get("total_tokens", 0)
                rec["cost"] += cost
                rec["cost_cached"] += usage.get("_cost_cached", 0.0)
                for k, v in (usage.get("_parts") or {}).items():
                    rec[f"{k}_tokens"] += v
                rec.setdefault("finish_reasons", []).append(finish_reason)
                kind_r, cmd, dsml = classify_reply(text)
                if dsml:
                    rec["dsml_turns"] = rec.get("dsml_turns", 0) + 1
                if kind_r != "command":
                    empty_length = (text == "" and finish_reason == "length")
                    rec.setdefault("commands", []).append({"turn": turn, "cmd": None,
                        "model_text": text[:400], "reply_kind": kind_r,
                        "finish_reason": finish_reason})
                    if reprompts < 2:
                        reprompts += 1
                        if empty_length:
                            rt = reprompt_empty_length_text()
                            rk = "empty-length"
                        elif kind_r == "dsml_nocommand" and reprompts == 1:
                            rt = reprompt_dsml_text()
                            rk = "dsml"
                            rec["dsml_nocommand"] = rec.get("dsml_nocommand", 0) + 1
                        else:
                            rt = reprompt_text(arm, reprompts)
                            rk = f"contract-{reprompts}"
                        rec["commands"][-1]["reprompt_sent"] = rk  # T-0096: which re-prompt was sent
                        messages.append({"role": "assistant", "content": text})
                        messages.append({"role": "user", "content": rt})
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
                # The whole block runs as one script, so heredocs and grouped
                # writes survive; the file stays in the workdir for audit.
                script = os.path.join(workdir, f".bench-turn-{turn}.sh")
                with open(script, "w") as fh:
                    fh.write(cmd + "\n")
                os.chmod(script, 0o755)
                if pre:
                    r = subprocess.run(pre + ["bash", os.path.basename(script)],
                                       capture_output=True, text=True,
                                       timeout=min(CMD_TIMEOUT_S, rem()), cwd=workdir)
                else:
                    r = subprocess.run(["bash", script], capture_output=True, text=True,
                                       timeout=min(CMD_TIMEOUT_S, rem()), cwd=workdir, env=env)
                rec.setdefault("commands", []).append({"turn": turn, "cmd": cmd,
                    "lines": cmd.count("\n") + 1, "rc": r.returncode,
                    "out": (r.stdout + r.stderr)[:OUT_REC_CAP], "finish_reason": finish_reason})
                messages.append({"role": "assistant", "content": text})
                messages.append({"role": "user", "content":
                    f"Command output (truncated):\n{(r.stdout + r.stderr)[:3000]}"})
        # Scoring (harness-side; no model): pass on the trial's site config, then
        # must FAIL with the task's feature broken on top of the same config.
        # T-0091: the PRIMARY score is unchanged - it evaluates exactly the
        # solution file the agent left (solution_origin records agent-file vs
        # missing). A salvaged partial, when one exists, is scored SEPARATELY as
        # a labeled diagnostic (rec["salvage"]); it never feeds the primary
        # score. Raw commands, outputs and the untouched workdir stay in the
        # episode/workdir so both interpretations remain auditable.
        sol_path = os.path.join(workdir, solution_name(arm))
        if not args.self_test:
            rec["solution_origin"] = ("agent-file" if os.path.exists(sol_path)
                                      and os.path.getsize(sol_path) > 0 else "missing")
        ok1, tail1 = run_solution(arm, workdir, base_url, args.primavera_bin, args)
        rec["pass_clean"] = ok1
        rec["clean_tail"] = tail1[-400:]
        site.start(change=change, broken=broken)
        ok2, tail2 = run_solution(arm, workdir, site.url(), args.primavera_bin, args)
        rec["pass_broken"] = ok2
        rec["broken_tail"] = tail2[-400:]
        rec["score"] = 1 if (ok1 and not ok2) else 0
        if not args.self_test and rec.get("solution_origin") == "missing":
            salv = salvage_partial_solution(arm, workdir, rec.get("commands", []))
            if salv:
                turn_s, spath = salv
                sdir = workdir + "-salvage"
                setup_workdir(arm, sdir, args)
                shutil.copy(spath, os.path.join(sdir, solution_name(arm)))
                chown_workdir(sdir, args)
                site.start(change=change)
                s_ok1, s_tail1 = run_solution(arm, sdir, site.url(), args.primavera_bin, args)
                site.start(change=change, broken=broken)
                s_ok2, s_tail2 = run_solution(arm, sdir, site.url(), args.primavera_bin, args)
                rec["salvage"] = {"diagnostic": True, "source_turn": turn_s,
                                  "pass_clean": s_ok1, "pass_broken": s_ok2,
                                  "score": 1 if (s_ok1 and not s_ok2) else 0,
                                  "clean_tail": s_tail1[-400:], "broken_tail": s_tail2[-400:]}
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
    # T-0096: worst-case reserve for this exact configuration, BEFORE any request.
    n_trials_planned = len(tasks) * args.trials
    worst = n_trials_planned * args.max_turns * REQ_MAX_USD
    print(f"RESERVE-CEILING tasks={len(tasks)}({','.join(tasks)}) trials={args.trials} "
          f"max_turns={args.max_turns} max_tokens={MAX_TOKENS} req_max_usd={REQ_MAX_USD:.5f} "
          f"worst_case=${worst:.2f} computed_not_billed", flush=True)
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
                  f"turns={rec['turns']} cost={rec['cost']:.4f} cost_cached={rec['cost_cached']:.4f} "
                  f"out={rec['completion_tokens']} reasoning={rec['reasoning_tokens']} "
                  f"wall={rec['wall_s']}s err={rec['error']}", flush=True)
    if drained:
        dpath = os.path.join(args.out, f"t0069-drained-{RUN_ID}.json")
        with open(dpath, "w") as f:
            json.dump({"run": RUN_ID, "drained": drained,
                       "resume_with": "--skip " + ",".join(drained)}, f, indent=1)
        print(f"DRAIN: wall budget reached; {len(drained)} trials deferred "
              f"(resume this slice in a later dispatch; every trial runs exactly once). "
              f"deferred={','.join(drained)} file={dpath}", flush=True)
    print(f"ledger: max_tokens={MAX_TOKENS} computed=${ledger['cost']:.4f} (uncached bound) "
          f"cached=${ledger.get('cost_cached', 0.0):.4f} retained=${ledger['retained']:.4f} "
          f"requests={ledger['requests']} tokens={ledger['tokens']} prompt={ledger.get('prompt_tokens', 0)} "
          f"cache_hits={ledger.get('cached_tokens', 0)} out={ledger.get('completion_tokens', 0)} "
          f"reasoning={ledger.get('reasoning_tokens', 0)}")

if __name__ == "__main__":
    try:
        main()
    except AuthFatal as e:
        print(f"AUTH-FATAL: {e} - run halted; completed trials and all request accounting are durable", flush=True)
        sys.exit(2)
