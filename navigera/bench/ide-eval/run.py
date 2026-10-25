#!/usr/bin/env python3
"""IDE-agent eval: the browser tool inside Cline, Kilo Code and Continue.

The open-source IDE agents no longer ship a browser of their own outside the
editor UI (Cline's CLI, Kilo's CLI and Continue's `cn` have none, Roo Code
removed its tool). They browse through MCP servers or through shell commands
plus a skill. This harness runs each agent's own CLI, headless, on the Acme
Supply tasks with each way of getting a browser:

  navigera        the CLI on PATH and its skill where the agent looks for skills
  navigera-mcp    `navigera mcp` configured as the agent's MCP server
  playwright-mcp  Microsoft's Playwright MCP, the incumbent

Success is judged from the site's recorded state and the final answer (the
checks of bench/agent-eval), never from the agent's own claim.

  run.py --agents cline,kilo,continue --browsers navigera,navigera-mcp,playwright-mcp \\
         --tasks lookup --model mock --out out/

`--model mock` starts mock_llm.py, a scripted OpenAI-compatible model that
plays `lookup` through whichever tools the agent offers: no API key, and a
deterministic check that each agent really reaches the browser and hands the
results back. `--model gemini:<id>` uses GEMINI_API_KEY with each agent's own
Gemini provider.
"""
import argparse
import importlib.util
import json
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
_spec = importlib.util.spec_from_file_location("agent_eval", REPO / "bench" / "agent-eval" / "run.py")
ae = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ae)

AGENTS = ("cline", "kilo", "continue")
BROWSERS = ("navigera", "navigera-mcp", "playwright-mcp")

SKILL_PROMPT = """You are in {work}. The command-line browser tool `navigera` is installed, and its usage guide is available to you as the skill "navigera". Read that skill first, then use `navigera` (through shell commands) for every browser interaction. Do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""

MCP_PROMPT = """You are in {work}. Use the browser tools of the `{server}` MCP server for every browser interaction. Do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Mock:
    """mock_llm.py on a free port, with its request log."""

    def __init__(self, root: Path):
        self.port = free_port()
        self.log = root / "mock.jsonl"
        self.proc = subprocess.Popen([sys.executable, "-I", str(HERE / "mock_llm.py"), "--port", str(self.port),
                                      "--log", str(self.log)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=0.2).close()
                return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("mock model did not start")

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1"

    def requests(self) -> list[dict]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if line.strip()]

    def close(self):
        self.proc.kill()
        self.proc.wait()


def mcp_server(browser: str, args) -> dict | None:
    """(command, args) of the MCP server for this browser, or None for the CLI."""
    if browser == "navigera-mcp":
        return {"name": "navigera", "command": shutil.which("navigera") or "navigera", "args": ["mcp"]}
    if browser == "playwright-mcp":
        cli = [args.playwright_mcp] if args.playwright_mcp.endswith((".js", ".mjs")) else []
        command = shutil.which("node") if cli else shutil.which(args.playwright_mcp) or args.playwright_mcp
        flags = ["--headless", "--isolated", "--no-sandbox", "--executable-path", os.environ["CHROME_BIN"]]
        return {"name": "playwright", "command": command, "args": cli + flags}
    return None


def write_cline(home: Path, work: Path, server: dict | None, model: tuple, env: dict, args) -> list[str]:
    settings = home / ".cline" / "data" / "settings"
    settings.mkdir(parents=True, exist_ok=True)
    servers = {server["name"]: {"command": server["command"], "args": server["args"]}} if server else {}
    (settings / "cline_mcp_settings.json").write_text(json.dumps({"mcpServers": servers}))
    kind, model_id, base = model
    auth = ([args.cline, "auth", "--provider", "openai-compatible", "--apikey", "test", "--modelid", model_id,
             "--baseurl", base] if kind == "mock" else
            [args.cline, "auth", "--provider", "gemini", "--apikey", env["GEMINI_API_KEY"], "--modelid", model_id])
    subprocess.run(auth, cwd=work, env=env, capture_output=True, timeout=120, check=True)
    # Act mode with auto-approve (the default), not --yolo: the yolo tool
    # preset turns skills off.
    return [args.cline, "--json", "-c", str(work)]


def write_kilo(home: Path, work: Path, server: dict | None, model: tuple, env: dict, args) -> list[str]:
    kind, model_id, base = model
    config = {"autoupdate": False, "share": "disabled"}
    if kind == "mock":
        config["provider"] = {"mock": {"npm": "@ai-sdk/openai-compatible", "name": "Mock",
                                       "options": {"baseURL": base, "apiKey": "test"},
                                       "models": {model_id: {"name": model_id, "tool_call": True}}}}
        name = f"mock/{model_id}"
    else:
        env["GOOGLE_GENERATIVE_AI_API_KEY"] = env["GEMINI_API_KEY"]
        name = f"google/{model_id}"
    config["model"] = config["small_model"] = name
    if server:
        config["mcp"] = {server["name"]: {"type": "local", "command": [server["command"], *server["args"]],
                                          "enabled": True}}
    (work / "kilo.json").write_text(json.dumps(config))
    return [args.kilo, "run", "--format", "json", "--auto", "-m", name, "--dir", str(work)]


def write_continue(home: Path, work: Path, server: dict | None, model: tuple, env: dict, args) -> list[str]:
    kind, model_id, base = model
    if kind == "mock":
        entry = {"name": model_id, "provider": "openai", "model": model_id, "apiBase": base, "apiKey": "test"}
    else:
        entry = {"name": model_id, "provider": "gemini", "model": model_id, "apiKey": env["GEMINI_API_KEY"]}
    entry.update({"roles": ["chat"], "capabilities": ["tool_use"]})
    config = {"name": "eval", "version": "0.0.1", "schema": "v1", "models": [entry]}
    if server:
        config["mcpServers"] = [{"name": server["name"], "command": server["command"], "args": server["args"]}]
    path = home / "continue.yaml"
    path.write_text(json.dumps(config))  # JSON is valid YAML
    return [args.cn, "-p", "--format", "json", "--auto", "--config", str(path)]


SETUP = {"cline": write_cline, "kilo": write_kilo, "continue": write_continue}
# Where each agent looks for project skills.
SKILL_DIR = {"cline": ".agents/skills", "kilo": ".agents/skills", "continue": ".claude/skills"}


def parse(agent: str, stdout: str) -> dict:
    """Final text, tool calls (name, input) and token totals from an agent's output."""
    text, calls, tokens_in, tokens_out, iterations = "", [], None, None, None
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        if agent == "cline":
            inner = event.get("event") or {}
            if event.get("type") == "agent_event" and inner.get("type") == "content_start" and inner.get("toolName"):
                calls.append((inner["toolName"], inner.get("input")))
            elif event.get("type") == "run_result":
                text = event.get("text") or ""
                usage = event.get("aggregateUsage") or event.get("usage") or {}
                tokens_in, tokens_out = usage.get("inputTokens"), usage.get("outputTokens")
                iterations = event.get("iterations")
        elif agent == "kilo":
            part = event.get("part") or {}
            if event.get("type") == "tool_use":
                calls.append((part.get("tool"), (part.get("state") or {}).get("input")))
            elif event.get("type") == "text":
                text = part.get("text") or text
            elif event.get("type") == "step_finish":
                t = part.get("tokens") or {}
                tokens_in = (tokens_in or 0) + (t.get("input") or 0)
                tokens_out = (tokens_out or 0) + (t.get("output") or 0)
                iterations = (iterations or 0) + 1
        elif agent == "continue":
            text = event.get("response") or text
    return {"text": text, "calls": calls, "tokens_in": tokens_in, "tokens_out": tokens_out, "iterations": iterations}


def is_browser_call(name: str, call_input) -> bool:
    if name and "browser_" in name:
        return True
    return "navigera" in json.dumps(call_input or "")


def run_one(agent: str, browser: str, task: dict, model_spec: str, args) -> dict:
    root = Path(tempfile.mkdtemp(prefix=f"ide-{agent}-{browser}-{task['id']}-"))
    home, work = root / "home", root / "work"
    home.mkdir()
    work.mkdir()
    rec = {"agent": agent, "browser": browser, "task": task["id"], "model": model_spec}
    site = mock = None
    proc = None
    try:
        site = ae.Site()
        env = dict(os.environ, HOME=str(home), USERPROFILE=str(home), CI="true", NO_COLOR="1",
                   DO_NOT_TRACK="1", KILO_DISABLE_AUTOUPDATE="1")
        if model_spec == "mock":
            mock = Mock(root)
            model = ("mock", "mock", mock.url)
        else:
            model = ("gemini", model_spec.split(":", 1)[1], None)
        server = mcp_server(browser, args)
        if server is None:
            subprocess.run(["navigera", "install-skill", "--dir", str(work / SKILL_DIR[agent])], env=env,
                           check=True, capture_output=True)
            prompt = SKILL_PROMPT.format(work=work, base=site.base, task=task["prompt"])
        else:
            prompt = MCP_PROMPT.format(work=work, server=server["name"], base=site.base, task=task["prompt"])
        argv = SETUP[agent](home, work, server, model, env, args) + [prompt]
        started = time.time()
        proc = subprocess.Popen(argv, cwd=work, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                stdin=subprocess.DEVNULL, text=True, encoding="utf-8", errors="replace",
                                start_new_session=os.name != "nt")
        timed_out = False
        try:
            out, err = proc.communicate(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            ae.kill_tree(proc)
            out, err = proc.communicate()
        wall = time.time() - started
        parsed = parse(agent, out)
        answer = ae.final_answer(parsed["text"])
        ok, why = ae.check(task, answer, site)
        if mock is not None and parsed["tokens_in"] is None:
            # Continue reports no usage: count what the agent sent the model.
            reqs = [r for r in mock.requests() if r["tools"]]
            parsed["iterations"] = len(reqs)
        browser_calls = [c for c in parsed["calls"] if is_browser_call(c[0], c[1])]
        rec.update({
            "ok": ok and not timed_out, "why": why + (["timed out"] if timed_out else []),
            "exit": proc.returncode, "timed_out": timed_out, "wall_s": round(wall, 1),
            "answer": answer[-300:], "iterations": parsed["iterations"],
            "tokens_in": parsed["tokens_in"], "tokens_out": parsed["tokens_out"],
            "tool_calls": [c[0] for c in parsed["calls"]], "browser_calls": len(browser_calls),
            "stderr_tail": ae.stderr_tail(err) if (proc.returncode or not ok) else "",
        })
    except Exception as e:  # setup problem: record, keep going
        rec.update({"ok": False, "why": [f"harness: {e}"]})
    finally:
        if proc and proc.poll() is None:
            ae.kill_tree(proc)
        if mock:
            mock.close()
        if site:
            site.close()
        subprocess.run(["navigera", "-s", "mock", "quit"], capture_output=True, env=dict(os.environ, HOME=str(home)))
        ae.cleanup(root)
    return rec


def render(runs: list, model: str) -> str:
    rows = sorted({(r["agent"], r["browser"]) for r in runs})
    tasks = sorted({r["task"] for r in runs})
    med = lambda xs: statistics.median(xs) if xs else None
    fmt = lambda v: "—" if v is None else (f"{v:.0f}" if isinstance(v, float) else str(v))
    lines = [f"### IDE-agent eval (model `{model}`)", "",
             "| agent | browser | passed | median model calls | median input tokens | median wall | browser calls |",
             "|---|---|---|---|---|---|---|"]
    for agent, browser in rows:
        rs = [r for r in runs if (r["agent"], r["browser"]) == (agent, browser)]
        passed = sum(1 for r in rs if r.get("ok"))
        lines.append(
            f"| {agent} | `{browser}` | {passed}/{len(rs)} "
            f"| {fmt(med([r['iterations'] for r in rs if r.get('iterations') is not None]))} "
            f"| {fmt(med([r['tokens_in'] for r in rs if r.get('tokens_in') is not None]))} "
            f"| {fmt(med([r['wall_s'] for r in rs if r.get('wall_s') is not None]))}s "
            f"| {fmt(med([r['browser_calls'] for r in rs if r.get('browser_calls') is not None]))} |")
    lines += ["", "| task | " + " | ".join(f"{a} · `{b}`" for a, b in rows) + " |",
              "|---|" + "---|" * len(rows)]
    for t in tasks:
        cells = []
        for a, b in rows:
            rs = [r for r in runs if (r["agent"], r["browser"], r["task"]) == (a, b, t)]
            cells.append(" ".join("✓" if r.get("ok") else "✗" for r in rs) or "—")
        lines.append(f"| {t} | " + " | ".join(cells) + " |")
    failures = [r for r in runs if not r.get("ok")]
    if failures:
        lines += ["", "Failures:"]
        lines += [f"- {r['agent']} · `{r['browser']}` · {r['task']}: {'; '.join(r.get('why') or [])}" for r in failures]
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--agents", default=",".join(AGENTS))
    ap.add_argument("--browsers", default=",".join(BROWSERS))
    ap.add_argument("--tasks", default="lookup")
    ap.add_argument("--model", default="mock", help="mock, or gemini:<model id> (needs GEMINI_API_KEY)")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--cline", default="cline")
    ap.add_argument("--kilo", default="kilo")
    ap.add_argument("--cn", default="cn")
    ap.add_argument("--playwright-mcp", default="playwright-mcp", help="its binary, or the path to its cli.js")
    ap.add_argument("--out", default="out")
    ap.add_argument("--require-pass", action="store_true")
    args = ap.parse_args()
    if not os.environ.get("CHROME_BIN"):
        print("set CHROME_BIN to the Chrome every browser setup should use", file=sys.stderr)
        return 2
    if args.model != "mock" and not os.environ.get("GEMINI_API_KEY"):
        print(f"--model {args.model} needs GEMINI_API_KEY", file=sys.stderr)
        return 2
    all_tasks = json.loads((REPO / "bench" / "agent-eval" / "tasks.json").read_text())["tasks"]
    wanted = args.tasks.split(",")
    tasks = [t for t in all_tasks if t["id"] in wanted]
    if args.model == "mock" and any(t["id"] != "lookup" for t in tasks):
        print("the mock model only plays `lookup`", file=sys.stderr)
        return 2
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    runs = []
    for rep in range(args.reps):
        for task in tasks:
            for agent in args.agents.split(","):
                for browser in args.browsers.split(","):
                    rec = run_one(agent, browser, task, args.model, args)
                    rec["rep"] = rep
                    runs.append(rec)
                    print(f"{agent:9} {browser:15} {task['id']:9} rep{rep}: {'PASS' if rec.get('ok') else 'FAIL'} "
                          f"{'; '.join(rec.get('why') or [])} (calls={rec.get('iterations')}, "
                          f"tokens_in={rec.get('tokens_in')}, wall={rec.get('wall_s')}s)", flush=True)
    table = render(runs, args.model)
    (out / "table.md").write_text(table)
    (out / "runs.json").write_text(json.dumps(runs, indent=1))
    print(table)
    if args.require_pass and any(not r.get("ok") for r in runs):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
