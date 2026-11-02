#!/usr/bin/env python3
"""T-0225: LLM-in-the-loop repeat of the scripted tool bench.

Same tasks as the scripted bench (search / form / table on the local
site, plus a WebMCP task), driven by one model through one shell tool
(the proven agent-eval loop shape: the model gets run_shell_command
and drives the browser tool named by --arms through it). Each session
records success (the task's expected value observed in a tool result),
wall time, model turns, prompt/completion/cached tokens, and USD cost.

Provider: SiliconFlow OpenAI-compatible API, model
deepseek-ai/DeepSeek-V4.1-Flash (the DeepSeek model the board's
earlier cells used), key from SILICONFLOW_API_KEY in the environment
(CI `main` environment secret; never in a sandbox). Prices verified
against SiliconFlow's listing for this model: $0.15/M input,
$0.003/M cached input, $0.60/M output. The budget guard is per
RUN (one Budget caps total spend across all sessions) and reserves
each request's estimated cost from its actual messages before
sending it; a failed request releases its reservation (no response,
no usage, no charge). No client retries.

Arms:
  nav-default       navigera CLI, default JSON output
  nav-compact       navigera CLI, NAVIGERA_OUTPUT=compact
  nav-batch         navigera CLI; the model may send a whole flow as
                    one `batch` call (steps on stdin, one per line)
  agent-browser     agent-browser CLI
  playwright-cli    playwright-cli CLI (-s session)
  playwright-mcp    playwright-mcp MCP server; the model issues
                    `mcp <tool> <json-args>` lines, routed to the
                    persistent server process
  chrome-devtools-mcp  chrome-devtools-mcp MCP server, same `mcp` lines
  nav-webmcp        navigera CLI, WebMCP task only (webmcp-list /
                    webmcp-call on webmcp.html, natively, no --attach)

Usage (CI): python3 llm-bench/llm_bench.py --reps 3 --out results
Env: SILICONFLOW_API_KEY (required), NAV_BIN, TOOLS_DIR, CHROME_PATH.
"""
import argparse
import glob
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "https://api.siliconflow.com/v1/chat/completions"
MODEL_DEFAULT = "deepseek-ai/DeepSeek-V4.1-Flash"
PRICE_IN, PRICE_IN_CACHED, PRICE_OUT = 0.15, 0.003, 0.60  # USD / 1M tokens
MAX_TOKENS = 1024
# Reservation is per CALL, estimated from the actual messages about to
# be sent (~4 chars/token plus a fixed overhead for the tool
# definition and message framing), NOT the model context ceiling: the
# old constant reserved the full 1M-token context (about $0.158) for
# every call, which made one kept reservation look like a session
# budget. Sessions run sequentially, so at most one reservation is
# ever outstanding.
TOOL_DEF_OVERHEAD_TOKENS = 250
MIN_RESERVE_USD = 0.005
MAX_TURNS = 40
CMD_TIMEOUT = 240
SESSION_TIMEOUT = 1500
OUTPUT_TAIL = 12000
REPEAT_LIMIT = 3

BASE = "http://127.0.0.1:8901"

TASKS = {
    "search": {
        "goal": "On the home page, search for 'headphones' and report the "
                "results line.",
        "expect": "Results for: headphones",
    },
    "form": {
        "goal": "On the order form, place an order for name Alex, city Tokyo, "
                "with the agreement checked, and report the confirmation.",
        "expect": "Order placed for Alex in Tokyo",
    },
    "table": {
        "goal": "On the prices table, report how many rows it has and the "
                "last row's last cell.",
        "expect": "20 rows, last=200",
    },
    "webmcp": {
        "goal": "On the WebMCP demo page, list the page's WebMCP tools and "
                "call the search tool with query 'headphones'.",
        "expect": "WebMCP result for: headphones",
    },
}

ARM_TASKS = {
    "nav-webmcp": ["webmcp"],
}
ALL_ARMS = ["nav-default", "nav-compact", "nav-batch", "agent-browser",
            "playwright-cli", "playwright-mcp", "chrome-devtools-mcp",
            "nav-webmcp"]


def chrome_path():
    env = os.environ.get("CHROME_PATH")
    if env:
        return env
    hits = sorted(glob.glob(os.path.expanduser(
        "~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome")))
    return hits[-1] if hits else "chrome"


class BudgetStop(RuntimeError):
    pass


def estimate_prompt_tokens(messages):
    """Rough prompt size for the reservation: ~4 chars per token,
    plus framing per message and the tool definition overhead."""
    n = TOOL_DEF_OVERHEAD_TOKENS
    for m in messages:
        c = m.get("content", "")
        if isinstance(c, str):
            n += len(c) // 4 + 8
        for call in (m.get("tool_calls") or []):
            n += len(json.dumps(call)) // 4 + 8
    return n


def reserve_for(messages):
    est = estimate_prompt_tokens(messages)
    return max(MIN_RESERVE_USD,
               (est * PRICE_IN + MAX_TOKENS * PRICE_OUT) / 1e6)


class Budget:
    """Guard is per RUN: one Budget caps total model spend across
    all sessions (sessions run sequentially, so at most one call's
    reservation is ever outstanding). A reservation is released when
    its request fails: the provider bills only served completions
    (usage in the response), so a raised request has no charge to
    hold against the budget. The old code kept a failed call's full
    worst-case reservation forever, so 31 fast-failing sessions
    accumulated 31 x $0.158 = $4.90 of phantom reservations and the
    32nd session tripped the $5 guard before its first call."""

    def __init__(self, total):
        self.total = total
        self.spent = 0.0
        self.reserved = 0.0

    def check(self, amount):
        if self.spent + self.reserved + amount > self.total:
            raise BudgetStop(
                f"budget exceeded: spent {self.spent:.4f} + reserved "
                f"{self.reserved:.4f} + reserve {amount:.4f} "
                f"> {self.total}")

    def reserve(self, amount):
        self.check(amount)
        self.reserved += amount

    def settle(self, amount, cost):
        self.reserved = max(0.0, self.reserved - amount)
        self.spent += cost

    def release(self, amount):
        self.reserved = max(0.0, self.reserved - amount)


def chat(key, model, messages, budget):
    amount = reserve_for(messages)
    budget.reserve(amount)
    body = json.dumps({"model": model, "messages": messages,
                       "temperature": 0, "max_tokens": MAX_TOKENS,
                       "tools": [{"type": "function", "function": {
                           "name": "run_shell_command",
                           "description": "Run a shell command and return "
                                          "its output.",
                           "parameters": {"type": "object",
                                          "properties": {"command": {
                                              "type": "string"}},
                                          "required": ["command"]}}}]})
    req = urllib.request.Request(API, data=body.encode(),
                                 headers={"Authorization": f"Bearer {key}",
                                          "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            resp = json.loads(r.read())
    except Exception as e:
        # No response, no usage: nothing was served, so nothing is
        # charged. Release the reservation instead of holding it
        # against every later session (see Budget).
        budget.release(amount)
        raise RuntimeError(f"model request failed, reservation "
                           f"released: {e}")
    usage = resp.get("usage", {})
    prompt = usage.get("prompt_tokens", 0)
    completion = usage.get("completion_tokens", 0)
    cached = usage.get("prompt_tokens_details", {}).get("cached_tokens", 0)
    cost = ((prompt - cached) * PRICE_IN + cached * PRICE_IN_CACHED
            + completion * PRICE_OUT) / 1e6
    budget.settle(amount, cost)
    return resp["choices"][0]["message"], {
        "prompt": prompt, "completion": completion, "cached": cached,
        "cost": cost}


class McpServer:
    """A persistent MCP server process spoken to over stdio JSON-RPC."""

    def __init__(self, cmd, env_extra):
        env = dict(os.environ)
        env.update(env_extra)
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True,
                                  bufsize=1, env=env)
        self.rid = 0
        self._send({"jsonrpc": "2.0", "id": self._next(),
                    "method": "initialize",
                    "params": {"protocolVersion": "2024-11-05",
                               "capabilities": {},
                               "clientInfo": {"name": "llm-bench",
                                              "version": "1.0"}}})
        self._recv()
        self._send({"jsonrpc": "2.0",
                    "method": "notifications/initialized"})
        rid = self._next()
        self._send({"jsonrpc": "2.0", "id": rid, "method": "tools/list",
                    "params": {}})
        tools = {}
        while True:
            d = self._recv()
            if not d or d.get("id") == rid:
                tools = d
                break
        self.tool_names = [t.get("name") for t in
                           tools.get("result", {}).get("tools", [])]

    def _next(self):
        self.rid += 1
        return self.rid

    def _send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def _recv(self):
        line = self.p.stdout.readline()
        return json.loads(line) if line else {}

    def call(self, name, args):
        rid = self._next()
        self._send({"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                    "params": {"name": name, "arguments": args}})
        while True:
            d = self._recv()
            if not d or d.get("id") == rid:
                return d

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def arm_vocab(arm, tools_dir, nav_bin):
    """(setup_cmds, prompt_block, is_mcp) for one arm."""
    if arm in ("nav-default", "nav-compact", "nav-batch", "nav-webmcp"):
        prefix = f"{nav_bin} -s SESS"
        extra = ""
        if arm == "nav-compact":
            extra = ("Output is compact: set NAVIGERA_OUTPUT=compact "
                     "in the environment of every command, e.g. "
                     "`NAVIGERA_OUTPUT=compact " + prefix + " ax`. ")
        if arm == "nav-batch":
            extra = ("You may send a whole flow as ONE call: "
                     "`" + prefix + " --raw batch -` with the steps on "
                     "stdin, one navigera command per line (heredoc), "
                     "e.g. goto, fill, click, text. Batch counts as "
                     "one model turn for all its steps. ")
        if arm == "nav-webmcp":
            extra = ("WebMCP only: `webmcp-list` shows the page's "
                     "tools; `webmcp-call <name> --params '{json}' "
                     "--confirm` calls one (confirm only what the "
                     "task asked for). ")
        return ([f"{prefix} start --headless"],
                f"Browser tool: navigera CLI. Start with `{prefix} "
                f"start --headless` (already run for you; the session "
                f"is SESS). Then commands like `{prefix} goto <url>`, "
                f"`{prefix} ax` (read the tree), `{prefix} fill "
                f"--selector <css> <value>`, `{prefix} click --text "
                f"<text>`, `{prefix} text --selector <css>`, "
                f"`{prefix} eval <js>`, `{prefix} quit`. {extra}"
                f"Session name in every command is SESS.",
                False)
    if arm == "agent-browser":
        ab = os.path.join(tools_dir, "node_modules", ".bin", "agent-browser")
        return ([f"{ab} open {BASE}/"],
                f"Browser tool: agent-browser CLI (no session flag; "
                f"one shared browser). Commands like `{ab} open "
                f"<url>`, `{ab} snapshot` (read the tree with refs), "
                f"`{ab} click <selector-or-@ref>`, `{ab} fill "
                f"<selector> <text>`, `{ab} select <selector> "
                f"<value>`, `{ab} check <selector>`, `{ab} "
                f"screenshot`. The browser is already open on the "
                f"home page.",
                False)
    if arm == "playwright-cli":
        pw = os.path.join(tools_dir, "node_modules", ".bin", "playwright-cli")
        return ([f"{pw} -s SESS open {BASE}/"],
                f"Browser tool: playwright-cli CLI. Start with "
                f"`{pw} -s SESS open <url>` (already run for you). "
                f"Then `{pw} -s SESS goto <url>`, `{pw} -s SESS "
                f"snapshot` (read the tree with refs), `{pw} -s "
                f"SESS click <target>`, `{pw} -s SESS fill <target> "
                f"<text>`, `{pw} -s SESS select <target> <value>`, "
                f"`{pw} -s SESS check <target>`, `{pw} -s SESS eval "
                f"<js>`, `{pw} -s SESS close`. Session name in "
                f"every command is SESS.",
                False)
    if arm == "playwright-mcp":
        cfg = {"browser": {"browserName": "chromium",
                           "launchOptions": {"executablePath": chrome_path(),
                                             "chromiumSandbox": False,
                                             "args": ["--no-sandbox"]}}}
        cfg_path = os.path.join(os.getcwd(), "mcp-pw.config.json")
        with open(cfg_path, "w") as f:
            json.dump(cfg, f)
        cmd = ["node",
               os.path.join(tools_dir, "node_modules", "@playwright",
                            "mcp", "cli.js"),
               "--config", cfg_path]
        return (cmd,
                "Browser tool: playwright-mcp MCP server (already "
                "running). Issue `mcp <tool> <json-args>` lines, e.g. "
                "`mcp browser_navigate {{\"url\": \"...\"}}`, `mcp "
                "browser_snapshot {{}}`, `mcp browser_click "
                "{{\"element\": \"...\", \"ref\": \"...\"}}`. Only `mcp` "
                "lines are accepted; anything else is an error. "
                "Available tools are listed below.",
                True)
    if arm == "chrome-devtools-mcp":
        cmd = ["node",
               os.path.join(tools_dir, "node_modules",
                            "chrome-devtools-mcp", "build", "src", "bin",
                            "chrome-devtools-mcp.js"),
               "--headless", "--no-usage-statistics", "--isolated",
               "--executablePath", chrome_path(),
               "--chrome-arg=--no-sandbox",
               "--chrome-arg=--disable-setuid-sandbox"]
        return (cmd,
                "Browser tool: chrome-devtools-mcp MCP server "
                "(already running). Issue `mcp <tool> <json-args>` "
                "lines, e.g. `mcp navigate_page {{\"url\": \"...\"}}`, "
                "`mcp take_snapshot {{}}`. Only `mcp` lines are "
                "accepted; anything else is an error. Available "
                "tools are listed below.",
                True)
    raise ValueError(f"unknown arm {arm}")


def run_session(key, model, arm, task, rep, tools_dir, nav_bin, budget,
                out_dir):
    sess = f"{arm}-{task}-{rep}".replace("_", "-")
    task_def = TASKS[task]
    setup, vocab, is_mcp = arm_vocab(arm, tools_dir, nav_bin)
    vocab = vocab.replace("SESS", sess)
    mcp = None
    if is_mcp:
        mcp = McpServer(setup, {"PLAYWRIGHT_MCP_EXECUTABLE_PATH":
                                chrome_path()} if arm == "playwright-mcp"
                        else {})
        vocab += f" Tools: {', '.join(mcp.tool_names)}."
    else:
        for cmd in setup:
            cmd = cmd.replace("SESS", sess)
            env = dict(os.environ)
            if arm == "nav-compact":
                env["NAVIGERA_OUTPUT"] = "compact"
            subprocess.run(cmd, shell=True, capture_output=True,
                           text=True, timeout=CMD_TIMEOUT, env=env)
    prompt = (f"Task: {task_def['goal']} Site base: {BASE}. "
              f"Home page: {BASE}/ . Form page: {BASE}/form.html . "
              f"Table page: {BASE}/table.html . WebMCP page: "
              f"{BASE}/webmcp.html . {vocab} Work ONLY through "
              f"run_shell_command. Keep each command to one browser "
              f"action (except a navigera batch call, which may hold "
              f"a whole flow). When the task's expected result is "
              f"visible in a tool result, reply DONE and stop. "
              f"For the table task, reply DONE followed by your "
              f"counted answer (e.g. DONE: 20 rows, last=200).")
    messages = [{"role": "user", "content": prompt}]
    turns = 0
    prompt_tokens = completion_tokens = cached_tokens = 0
    cost = 0.0
    success = False
    last_cmds = []
    t0 = time.monotonic()
    try:
        while turns < MAX_TURNS and time.monotonic() - t0 < SESSION_TIMEOUT:
            msg, usage = chat(key, model, messages, budget)
            prompt_tokens += usage["prompt"]
            completion_tokens += usage["completion"]
            cached_tokens += usage["cached"]
            cost += usage["cost"]
            turns += 1
            messages.append(msg)
            calls = (msg.get("tool_calls") or [])
            if not calls:
                text = (msg.get("content") or "").strip()
                if text.startswith("DONE"):
                    if task == "table" and task_def["expect"] in text:
                        success = True
                    break
                messages.append({"role": "user", "content":
                                 "Continue with run_shell_command, "
                                 "or reply DONE when the expected "
                                 "result is visible."})
                continue
            for call in calls:
                args = json.loads(call["function"].get("arguments")
                                  or "{}")
                command = args.get("command", "")
                out = exec_command(command, arm, sess, tools_dir,
                                   nav_bin, mcp)
                if task_def["expect"] in out:
                    success = True
                messages.append({"role": "tool",
                                 "tool_call_id": call["id"],
                                 "content": out[-OUTPUT_TAIL:]})
                last_cmds.append(command)
                if len(last_cmds) >= REPEAT_LIMIT and \
                        len(set(last_cmds[-REPEAT_LIMIT:])) == 1:
                    messages.append({"role": "user", "content":
                                     "That command failed the same "
                                     "way three times; try a "
                                     "different approach or reply "
                                     "DONE if the result is visible."})
                if success:
                    break
            if success:
                break
    except BudgetStop as e:
        print(f"  budget stop: {e}", flush=True)
    except Exception as e:
        print(f"  session error: {e}", flush=True)
    finally:
        cleanup(arm, sess, tools_dir, nav_bin, mcp)
    wall_ms = round((time.monotonic() - t0) * 1000)
    rec = {"arm": arm, "task": task, "rep": rep, "success": success,
           "turns": turns, "prompt_tokens": prompt_tokens,
           "completion_tokens": completion_tokens,
           "cached_tokens": cached_tokens, "cost_usd": round(cost, 6),
           "wall_ms": wall_ms}
    with open(os.path.join(out_dir, f"{arm}-{task}-{rep}.json"),
              "w") as f:
        json.dump(rec, f, indent=2)
    line = (f"RESULT arm={arm} task={task} rep={rep} "
            f"success={int(success)} turns={turns} "
            f"prompt_tokens={prompt_tokens} "
            f"completion_tokens={completion_tokens} "
            f"cached_tokens={cached_tokens} cost_usd={cost:.6f} "
            f"wall_ms={wall_ms}")
    print(line, flush=True)
    print(f"::notice::{line}", flush=True)
    return rec


def exec_command(command, arm, sess, tools_dir, nav_bin, mcp):
    command = command.replace("SESS", sess)
    if mcp is not None:
        stripped = command.strip()
        if not stripped.startswith("mcp "):
            return ("error: this arm accepts only `mcp <tool> "
                    "<json-args>` lines")
        rest = stripped[4:].strip()
        name, _, argstr = rest.partition(" ")
        try:
            args = json.loads(argstr) if argstr else {}
        except Exception as e:
            return f"error: bad json args: {e}"
        try:
            return json.dumps(mcp.call(name, args))[:OUTPUT_TAIL]
        except Exception as e:
            return f"error: mcp call failed: {e}"
    env = dict(os.environ)
    if arm == "nav-compact":
        env["NAVIGERA_OUTPUT"] = "compact"
    try:
        r = subprocess.run(command, shell=True, capture_output=True,
                           text=True, timeout=CMD_TIMEOUT, env=env)
        return (r.stdout + r.stderr)[-OUTPUT_TAIL:] or "(no output)"
    except subprocess.TimeoutExpired:
        return "error: command timed out"


def cleanup(arm, sess, tools_dir, nav_bin, mcp):
    if mcp is not None:
        mcp.close()
        return
    try:
        if arm.startswith("nav-"):
            subprocess.run(f"{nav_bin} -s {sess} quit", shell=True,
                           capture_output=True, text=True, timeout=60)
        elif arm == "playwright-cli":
            pw = os.path.join(tools_dir, "node_modules", ".bin",
                              "playwright-cli")
            subprocess.run(f"{pw} -s {sess} close", shell=True,
                           capture_output=True, text=True, timeout=60)
        elif arm == "agent-browser":
            ab = os.path.join(tools_dir, "node_modules", ".bin",
                              "agent-browser")
            subprocess.run(f"{ab} close", shell=True,
                           capture_output=True, text=True, timeout=60)
    except Exception:
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arms", default=",".join(ALL_ARMS))
    ap.add_argument("--tasks", default=None,
                    help="comma-separated task subset (probe use); "
                         "default is the arm's task list")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--model", default=MODEL_DEFAULT)
    ap.add_argument("--out", default="results")
    ap.add_argument("--budget", type=float, default=5.00)
    ap.add_argument("--site-dir", default="llm-bench/site")
    args = ap.parse_args()
    key = os.environ.get("SILICONFLOW_API_KEY")
    if not key:
        print("SILICONFLOW_API_KEY is not set", file=sys.stderr)
        return 2
    tools_dir = os.environ.get("TOOLS_DIR", "tools-npm")
    nav_bin = os.environ.get("NAV_BIN", "./nav/target/release/navigera")
    os.makedirs(args.out, exist_ok=True)
    server = subprocess.Popen([sys.executable, "-m", "http.server",
                               "8901", "-d", args.site_dir],
                              stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    budget = Budget(args.budget)
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    only_tasks = ([t.strip() for t in args.tasks.split(",") if t.strip()]
                  if args.tasks else None)

    def tasks_for(arm):
        base = ARM_TASKS.get(arm, ["search", "form", "table"])
        return [t for t in base if t in only_tasks] if only_tasks \
            else base

    records = []
    try:
        for rep in range(1, args.reps + 1):
            order = arms[rep % len(arms):] + arms[:rep % len(arms)]
            for arm in order:
                for task in tasks_for(arm):
                    print(f"--- {arm} / {task} / rep {rep} ---",
                          flush=True)
                    records.append(run_session(
                        key, args.model, arm, task, rep, tools_dir,
                        nav_bin, budget, args.out))
    finally:
        server.terminate()
    with open(os.path.join(args.out, "summary.json"), "w") as f:
        json.dump({"model": args.model, "records": records,
                   "total_cost_usd": round(budget.spent, 6)}, f, indent=2)
    print("\n## LLM bench summary", flush=True)
    print("| arm | task | success | median turns | median wall ms | "
          "median tokens | median $ |", flush=True)
    for arm in arms:
        for task in tasks_for(arm):
            rs = [r for r in records if r["arm"] == arm
                  and r["task"] == task]
            if not rs:
                continue
            med = lambda k: sorted(r[k] for r in rs)[len(rs) // 2]
            toks = med("prompt_tokens") + med("completion_tokens")
            print(f"| {arm} | {task} | {sum(r['success'] for r in rs)}/"
                  f"{len(rs)} | {med('turns')} | {med('wall_ms')} | "
                  f"{toks} | {med('cost_usd'):.6f} |", flush=True)
    print(f"Total model spend: ${budget.spent:.4f} of "
          f"${args.budget:.2f} budget", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
