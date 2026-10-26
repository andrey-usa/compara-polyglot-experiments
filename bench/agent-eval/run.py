#!/usr/bin/env python3
"""Agent eval: how a real, generic coding agent copes with each browser tool.

A real agent (Gemini CLI, headless, `--approval-mode=yolo`) gets a task on
the local Acme Supply site (bench/site/server.py) and one browser tool:

  skilled  the tool is installed and its own Agent Skill is in the workspace
           (`.agents/skills/<tool>/SKILL.md`, the skill each vendor ships)
  onboard  only the tool's name and repository URL: the agent must install
           it from its own docs, then do a short task

Everything else is identical across tools: same model, prompt template,
settings, Chrome binary, site and checks. Success is judged from the site's
recorded state (orders, tickets, logins) plus the final answer, never from
the agent's own claim. The run also records model turns, tokens, tool
calls, how many shell commands used the tool, and off-tool workarounds
(curl against the site, ad-hoc Playwright/Puppeteer scripts).

  run.py --tools navigera,agent-browser,playwright-cli --tasks all \
         --mode skilled --model gemini-3.5-flash-lite --out out/

`navigera+jev` is navigera with a decision model (`find`, `--find`,
`check`): the session gets `--decider-url` (bench/decide/relay.py in front
of TypeSafe's API, holding the key), and each run records the decider's
calls and input tokens from the relay's `--decider-log`.

Measuring the decider takes three arms on the same tasks, model and reps,
each on its own model key so a spent quota cannot fall on one arm only:
`navigera` (no decider), `navigera+jev` (the decider is on and the agent
learns it only as a user would: `start`'s note, a note on a long `ax`, the
skill) and `navigera+jev-told` (the prompt asks for it too). The first pair
measures adoption, the second what a used decider saves. Tasks where
deciding pays are the ones where reading the page costs more than naming
the element: `preferences` (a 170-line form, four named changes and a
confirmation to check) and the live docs tasks; on the small pages of the
others one `ax` is cheaper, and an agent that skips the decider there is
right to. Compare success first, then agent tokens plus decider tokens,
turns and wall time, per task.

`--agent scripted` replays known-good navigera command plans instead of
calling a model: it validates the site, the checks and this harness without
an API key (CI runs it on every eval, and locally).

`--agent openai` drives the same tasks from any OpenAI-compatible chat
endpoint (env `OPENAI_BASE_URL`, default SiliconFlow; the key is
`OPENAI_API_KEY` or `SILICONFLOW_API_KEY`): the model gets one shell tool
and the same prompt, turns and timeout as the Gemini CLI runs, so a
DeepSeek run is measured against the Gemini runs, not beside them.
"""
import argparse
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

WINDOWS = os.name == "nt"
OS_NAME = {"nt": "windows"}.get(os.name, "darwin" if sys.platform == "darwin" else "linux")

if WINDOWS and not sys.flags.utf8_mode and __name__ == "__main__":
    # Model output, tables (✓ ✗ ⚠) and JSON are UTF-8; Windows' default
    # code page would fail on them in pipes and files.
    sys.exit(subprocess.run([sys.executable, "-X", "utf8", *sys.argv]).returncode)

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SITE = REPO / "bench" / "site" / "server.py"

TOOLS = {
    "navigera": {
        "cmd": "navigera", "skill": "navigera",
        "repo": "https://github.com/andrey-usa/navigera",
    },
    # navigera on Lightpanda ($NAVIGERA_ENGINE, so the agent's commands are
    # the same as on Chrome) and on chrome-headless-shell (the job points
    # $CHROME_BIN at it); each also with the decider.
    "navigera-lightpanda": {
        "cmd": "navigera", "skill": "navigera",
        "repo": "https://github.com/andrey-usa/navigera",
        "engine": "lightpanda",
    },
    "navigera-shell": {
        "cmd": "navigera", "skill": "navigera",
        "repo": "https://github.com/andrey-usa/navigera",
    },
    "navigera+jev": {
        "cmd": "navigera", "skill": "navigera", "decider": True,
        "repo": "https://github.com/andrey-usa/navigera",
    },
    "navigera-lightpanda+jev": {
        "cmd": "navigera", "skill": "navigera", "decider": True,
        "repo": "https://github.com/andrey-usa/navigera",
        "engine": "lightpanda",
    },
    "navigera-shell+jev": {
        "cmd": "navigera", "skill": "navigera", "decider": True,
        "repo": "https://github.com/andrey-usa/navigera",
    },
    # The same, with the prompt asking for the decider: what Jev saves when
    # the agent uses it, apart from whether the skill alone gets it used.
    "navigera+jev-told": {
        "cmd": "navigera", "skill": "navigera", "decider": True,
        "repo": "https://github.com/andrey-usa/navigera",
        "hint": " A decision model is configured for this browser session: prefer `--find \"<description>\"` on"
                " element commands and `check \"<question>\"` over reading the page with `ax`; read the page only"
                " for text you must report, or when --find says it is unsure.",
    },
    "agent-browser": {
        "cmd": "agent-browser", "skill": "agent-browser",
        "repo": "https://github.com/vercel-labs/agent-browser",
    },
    # agent-browser's own Lightpanda engine: does it hit the same engine
    # differences (dialogs, popups, iframes) navigera had to handle?
    "agent-browser-lightpanda": {
        "cmd": "agent-browser", "skill": "agent-browser",
        "repo": "https://github.com/vercel-labs/agent-browser",
        "env": {"AGENT_BROWSER_ENGINE": "lightpanda", "AGENT_BROWSER_EXECUTABLE_PATH": "$LIGHTPANDA_BIN"},
    },
    "playwright-cli": {
        "cmd": "playwright-cli", "skill": "playwright-cli",
        "repo": "https://github.com/microsoft/playwright-cli",
    },
}

SKILLED_PROMPT = """You are in {workdir}. The command-line browser tool `{cmd}` is installed, and its usage guide is available to you as the skill "{skill}". Read that skill first, then use `{cmd}` (through shell commands) for every browser interaction.{hint} Do not drive a browser through other libraries or scripts, and do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""

ONBOARD_PROMPT = """You are in {workdir}. Use the browser automation tool {name} ({repo}) to complete the task below. It is not installed yet: install it by following its own documentation, then use it (through shell commands) for every browser interaction. A Chrome binary is available at $CHROME_BIN. Do not drive a browser through other libraries or scripts, and do not fetch the website with curl, wget or similar.

The website is running at {base}.

Task: {task}

When you are done, end your reply with one line exactly in this form:
FINAL ANSWER: <answer>"""

# Gemini CLI strips the shell environment to a short allowlist when it runs
# in GitHub Actions (GITHUB_SHA set). Pass through what a developer machine
# would have, or the agents never see the Chrome path or the Rust toolchain.
PASS_ENV = ["CHROME_BIN", "RUSTUP_HOME", "CARGO_HOME", "NPM_CONFIG_PREFIX",
            "AGENT_BROWSER_EXECUTABLE_PATH", "AGENT_BROWSER_ARGS",
            "PLAYWRIGHT_MCP_EXECUTABLE_PATH", "PLAYWRIGHT_MCP_SANDBOX",
            "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD", "DO_NOT_TRACK", "DISABLE_TELEMETRY", "CI",
            "NAVIGERA_DECIDER_URL", "LIGHTPANDA_BIN", "NAVIGERA_ENGINE", "LIGHTPANDA_DISABLE_TELEMETRY",
            "AGENT_BROWSER_ENGINE",
            # Windows: what a desktop session has (npm, Node and PowerShell
            # look things up there; Gemini CLI keeps only TEMP/USERPROFILE/…).
            "APPDATA", "LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432",
            "ProgramData", "HOMEDRIVE", "HOMEPATH", "USERNAME", "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE", "OS"]

# Models Gemini CLI calls for its own helper work (loop detection, web fetch:
# the `gemini-3-flash-base` alias). Seen in healthy runs; any other model in
# a run's stats means its fallback chain replaced the requested model.
HELPER_MODELS = {"gemini-3-flash-preview"}

GEMINI_SETTINGS = {
    "security": {"auth": {"selectedType": "gemini-api-key"}, "folderTrust": {"enabled": False},
                 "environmentVariableRedaction": {"allowed": PASS_ENV}},
    "model": {"maxSessionTurns": 40},
    "general": {"checkpointing": {"enabled": False}, "enableAutoUpdate": False,
                "enableAutoUpdateNotification": False, "maxAttempts": 10},
    "privacy": {"usageStatisticsEnabled": False},
    "telemetry": {"enabled": False},
    "tools": {"shell": {"inactivityTimeout": 240, "enableInteractiveShell": False},
              "exclude": ["google_web_search"]},
    "experimental": {"enableAgents": False},
    "skills": {"enabled": True},
    "context": {"fileName": ["AGENTS.md", "GEMINI.md"]},
}


# --- site -------------------------------------------------------------------

class LiveSite:
    """A public website: no local server, no recorded state to check."""

    def __init__(self, url: str):
        self.base = url.rstrip("/")

    def get(self, path: str):
        raise RuntimeError("live sites have no /__state; check the answer only")

    def close(self):
        pass


class Site:
    def __init__(self):
        self.proc = subprocess.Popen([sys.executable, str(SITE), "--port", "0"],
                                     stdout=subprocess.PIPE, text=True)
        line = self.proc.stdout.readline().strip()
        if not line.startswith("LISTENING "):
            raise RuntimeError(f"site did not start: {line!r}")
        self.base = line.split(" ", 1)[1]

    def get(self, path: str):
        with urllib.request.urlopen(self.base + path, timeout=10) as r:
            return json.load(r)

    def close(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# --- workspace --------------------------------------------------------------

def exe(name: str, env: dict | None = None) -> str:
    """Full path of a command on the run's PATH. Windows needs it: process
    creation searches the parent's PATH, not the child env's, and npm tools
    are `.cmd` shims that only a PATHEXT-aware lookup finds."""
    return shutil.which(name, path=(env or os.environ).get("PATH")) or name


def npm_root() -> str:
    try:
        return subprocess.run([exe("npm"), "root", "-g"], capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception:
        return ""


def prepare(tool: str, mode: str, root: Path, args) -> tuple[Path, dict]:
    work, home = root / "work", root / "home"
    work.mkdir(parents=True)
    (home / ".gemini").mkdir(parents=True)
    settings = json.loads(json.dumps(GEMINI_SETTINGS))
    settings["model"]["name"] = args.model
    if mode == "skilled":
        settings["tools"]["exclude"].append("web_fetch")
    (home / ".gemini" / "settings.json").write_text(json.dumps(settings, indent=1))

    chrome = os.environ["CHROME_BIN"]
    env = {
        **os.environ,
        "HOME": str(home), "GEMINI_CLI_HOME": str(home), "GEMINI_CLI_TRUST_WORKSPACE": "true",
        # Toolchains stay where the runner installed them.
        "RUSTUP_HOME": os.environ.get("RUSTUP_HOME") or str(Path.home() / ".rustup"),
        "CHROME_BIN": chrome,
        "AGENT_BROWSER_EXECUTABLE_PATH": chrome, "AGENT_BROWSER_ARGS": "--no-sandbox",
        "PLAYWRIGHT_MCP_EXECUTABLE_PATH": chrome, "PLAYWRIGHT_MCP_SANDBOX": "false",
        "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1",
        "DO_NOT_TRACK": "1", "DISABLE_TELEMETRY": "1", "LIGHTPANDA_DISABLE_TELEMETRY": "1", "CI": "true",
    }
    if os.environ.get("LIGHTPANDA_BIN"):
        env["LIGHTPANDA_BIN"] = os.environ["LIGHTPANDA_BIN"]
    env.pop("NAVIGERA_ENGINE", None)
    if TOOLS[tool].get("engine"):
        env["NAVIGERA_ENGINE"] = TOOLS[tool]["engine"]
    for key, value in TOOLS[tool].get("env", {}).items():
        env[key] = os.path.expandvars(value)
    # Only the relay holds the TypeSafe key; only navigera+jev gets a decider.
    env.pop("TYPESAFE_API_KEY", None)
    env.pop("NAVIGERA_DECIDER_URL", None)
    if TOOLS[tool].get("decider"):
        if not args.decider_url:
            raise RuntimeError(f"{tool} needs --decider-url")
        env["NAVIGERA_DECIDER_URL"] = args.decider_url
    if WINDOWS:
        # A temp dir per run: tool daemons' sockets and browser profiles
        # land under the run's root, so cleanup can find them by path.
        (root / "tmp").mkdir()
        env["TEMP"] = env["TMP"] = str(root / "tmp")
    if mode == "onboard":
        # Whatever the agent installs lands in this run's own prefix.
        prefix = home / ".npm-global"
        env["NPM_CONFIG_PREFIX"] = str(prefix)
        env["CARGO_HOME"] = str(home / ".cargo")
        env["PATH"] = os.pathsep.join([str(prefix if WINDOWS else prefix / "bin"), str(home / ".cargo" / "bin"),
                                       str(home / ".local" / "bin"), env.get("PATH", "")])
        if args.nv_bin_dir:
            env["PATH"] = os.pathsep.join(p for p in env["PATH"].split(os.pathsep)
                                          if Path(p).resolve() != Path(args.nv_bin_dir).resolve())
        return work, env

    skills = work / ".agents" / "skills"
    skills.mkdir(parents=True)
    if TOOLS[tool]["cmd"] == "navigera":
        if args.nv_bin_dir:
            env["PATH"] = os.pathsep.join([args.nv_bin_dir, env.get("PATH", "")])
        subprocess.run([exe("navigera", env), "install-skill", "--dir", str(skills)], env=env, check=True,
                       capture_output=True)
    elif TOOLS[tool]["cmd"] == "agent-browser":
        src = Path(npm_root()) / "agent-browser" / "skills" / "agent-browser"
        shutil.copytree(src, skills / "agent-browser")
    elif tool == "playwright-cli":
        subprocess.run([exe("playwright-cli", env), "install", "--skills=agents"], cwd=work, env=env,
                       capture_output=True, timeout=300)
        if not (skills / "playwright-cli" / "SKILL.md").exists():
            raise RuntimeError("playwright-cli install --skills=agents wrote no skill")
        (work / ".playwright").mkdir(exist_ok=True)
        (work / ".playwright" / "cli.config.json").write_text(json.dumps({"browser": {
            "browserName": "chromium",
            "launchOptions": {"executablePath": chrome, "headless": True, "chromiumSandbox": False}}}))
    return work, env


# --- agents -----------------------------------------------------------------

def gemini_argv(gemini: str) -> list[str]:
    """How to start Gemini CLI. `--gemini` may name its JS entry point: on
    Windows the npm `.cmd` shim runs through cmd.exe, which cuts a
    multi-line `-p` prompt at the first newline."""
    if gemini.endswith((".js", ".mjs")):
        return [exe("node"), gemini]
    return [exe(gemini)]


def kill_tree(proc: subprocess.Popen):
    if WINDOWS:
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(proc.pid)], capture_output=True)
    else:
        os.killpg(proc.pid, signal.SIGKILL)


def run_gemini(prompt: str, work: Path, env: dict, args) -> dict:
    cmd = [*gemini_argv(args.gemini), "-p", prompt, "-m", args.model, "--approval-mode=yolo",
           "--skip-trust", "-o", "stream-json"]
    started = time.time()
    group = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {"start_new_session": True}
    proc = subprocess.Popen(cmd, cwd=work, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding="utf-8", errors="replace", **group)
    try:
        out, err = proc.communicate(timeout=args.run_timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        kill_tree(proc)
        out, err = proc.communicate()
        timed_out = True
    wall = time.time() - started
    events = []
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    text = "".join(e.get("content", "") for e in events
                   if e.get("type") == "message" and e.get("role") == "assistant")
    shells = [e.get("parameters", {}).get("command", "") for e in events
              if e.get("type") == "tool_use" and e.get("tool_name") == "run_shell_command"]
    tool_names = [e.get("tool_name") for e in events if e.get("type") == "tool_use"]
    # What each shell command printed (tail), so a failed install or a
    # confusing error explains itself in the summary.
    by_id = {e.get("tool_id"): e.get("parameters", {}).get("command", "") for e in events
             if e.get("type") == "tool_use" and e.get("tool_name") == "run_shell_command"}
    shell_steps = []
    answered = set()
    for e in events:
        if e.get("type") == "tool_result" and e.get("tool_id") in by_id:
            answered.add(e["tool_id"])
            out_text = e.get("output") or (e.get("error") or {}).get("message") or ""
            shell_steps.append({"cmd": by_id[e["tool_id"]][:200], "status": e.get("status"),
                                "tail": out_text[-300:]})
    # Commands that never returned: what a timed-out run was stuck in.
    in_flight = [cmd[:300] for tid, cmd in by_id.items() if tid not in answered]
    result = next((e for e in reversed(events) if e.get("type") == "result"), {})
    stats = result.get("stats") or {}
    models = stats.get("models") or {}
    # stream-json stats carry tokens but no request count: count model turns
    # instead (one turn = one model call; a new one starts after each batch
    # of tool results).
    requests, after_tools = (1 if events else 0), False
    for e in events:
        kind = e.get("type")
        if kind == "tool_result":
            after_tools = True
        elif after_tools and (kind == "tool_use" or (kind == "message" and e.get("role") == "assistant")):
            requests += 1
            after_tools = False
    errors = [e.get("message", "") for e in events if e.get("type") == "error"]
    # Whether the agent saw `start` report a decider (navigera+jev runs).
    decider_seen = any('"decider"' in str(e.get("output") or "") for e in events if e.get("type") == "tool_result")
    return {
        "exit": proc.returncode, "timed_out": timed_out, "wall_s": round(wall, 1),
        "text": text, "shell_commands": shells, "shell_steps": shell_steps, "tools_used": tool_names,
        "in_flight": in_flight,
        "requests": requests or None, "models": list(models) if isinstance(models, dict) else [],
        "tokens_in": stats.get("input_tokens"), "tokens_out": stats.get("output_tokens"),
        "tokens_total": stats.get("total_tokens"), "tokens_cached": stats.get("cached"),
        "tool_calls": stats.get("tool_calls"),
        "errors": errors[-3:], "stderr_tail": stderr_tail(err) if proc.returncode else "",
        "decider_seen": decider_seen,
    }


# --- OpenAI-compatible agents ------------------------------------------------
# The same job Gemini CLI does, without the CLI: the model is offered one
# shell tool in the run's workspace and its calls are executed with the
# run's env, for at most as many turns and as long overall as a Gemini run.

SHELL_TOOL = {
    "type": "function",
    "function": {
        "name": "run_shell_command",
        "description": "Run a shell command in the workspace and return its combined stdout and stderr.",
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string", "description": "The command to run, as one shell line."}},
            "required": ["command"],
        },
    },
}

OPENAI_DEFAULT_BASE = "https://api.siliconflow.com/v1"
SHELL_TIMEOUT = 240  # Gemini CLI's shell inactivity timeout, for parity
TOOL_OUTPUT_LIMIT = 32000


def openai_endpoint(env: dict, args) -> tuple[str, str]:
    base = (args.base_url or env.get("OPENAI_BASE_URL") or os.environ.get("OPENAI_BASE_URL")
            or OPENAI_DEFAULT_BASE).rstrip("/")
    key = env.get("OPENAI_API_KEY") or env.get("SILICONFLOW_API_KEY") \
        or os.environ.get("OPENAI_API_KEY") or os.environ.get("SILICONFLOW_API_KEY") or ""
    return base, key


def chat_completion(messages: list, env: dict, args, deadline: float) -> tuple[dict | None, str]:
    """One POST /chat/completions, retried on 429/5xx. Returns (reply, error)."""
    base, key = openai_endpoint(env, args)
    if not key:
        return None, "no API key: set OPENAI_API_KEY or SILICONFLOW_API_KEY"
    body = json.dumps({"model": args.model, "messages": messages, "tools": [SHELL_TOOL]}).encode()
    wait, last = 5.0, ""
    for _ in range(3):
        req = urllib.request.Request(base + "/chat/completions", data=body, method="POST",
                                     headers={"Content-Type": "application/json",
                                              "Authorization": f"Bearer {key}"})
        try:
            with urllib.request.urlopen(req, timeout=max(10, min(600, deadline - time.time()))) as r:
                return json.load(r), ""
        except urllib.error.HTTPError as e:
            last = f"status: {e.code}: {e.read()[:200].decode('utf-8', 'replace')}"
            if e.code not in (429, 500, 502, 503):
                break
        except OSError as e:
            last = f"fetch failed: {e}"
        if time.time() + wait > deadline:
            break
        time.sleep(wait)
        wait *= 2
    return None, last


def run_openai(prompt: str, work: Path, env: dict, args) -> dict:
    """The run_gemini record shape, from a plain chat-completions loop."""
    started = time.time()
    deadline = started + args.run_timeout
    messages = [{"role": "user", "content": prompt}]
    shells, shell_steps, texts, errors = [], [], [], []
    tokens_in = tokens_out = tokens_total = tokens_cached = 0
    requests = tool_calls = 0
    decider_seen = timed_out = False
    exit_code = 0
    group = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {"start_new_session": True}
    for _turn in range(GEMINI_SETTINGS["model"]["maxSessionTurns"]):
        if time.time() > deadline:
            timed_out = True
            break
        reply, err = chat_completion(messages, env, args, deadline)
        if reply is None:
            errors.append(err)
            exit_code = 1
            break
        requests += 1
        usage = reply.get("usage") or {}
        tokens_in += usage.get("prompt_tokens") or 0
        tokens_out += usage.get("completion_tokens") or 0
        tokens_total += usage.get("total_tokens") or 0
        tokens_cached += (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
        choice = (reply.get("choices") or [{}])[0]
        msg = choice.get("message") or {}
        if msg.get("content"):
            texts.append(msg["content"])
        calls = msg.get("tool_calls") or []
        messages.append({"role": "assistant", "content": msg.get("content") or "",
                         **({"tool_calls": calls} if calls else {})})
        if not calls:
            break
        for call in calls:
            tool_calls += 1
            try:
                command = json.loads((call.get("function") or {}).get("arguments") or "{}").get("command", "")
            except json.JSONDecodeError:
                command = ""
            shells.append(command)
            try:
                p = subprocess.run(["bash", "-lc", command], cwd=work, env=env, capture_output=True,
                                   text=True, encoding="utf-8", errors="replace",
                                   timeout=min(SHELL_TIMEOUT, max(10, deadline - time.time())), **group)
                output, status = (p.stdout or "") + (p.stderr or ""), "success" if p.returncode == 0 else "error"
            except subprocess.TimeoutExpired as e:
                output, status = ((e.stdout or b"").decode("utf-8", "replace") if isinstance(e.stdout, bytes)
                                  else (e.stdout or "")) + "\n[command timed out]", "error"
            if '"decider"' in output:
                decider_seen = True
            shell_steps.append({"cmd": command[:200], "status": status, "tail": output[-300:]})
            if len(output) > TOOL_OUTPUT_LIMIT:
                output = output[:2000] + "\n[output truncated]\n" + output[-TOOL_OUTPUT_LIMIT + 2000:]
            messages.append({"role": "tool", "tool_call_id": call.get("id") or "",
                             "content": output or "(no output)"})
    else:
        errors.append("turn limit reached")
    return {
        "exit": exit_code, "timed_out": timed_out, "wall_s": round(time.time() - started, 1),
        "text": "".join(texts), "shell_commands": shells, "shell_steps": shell_steps,
        "tools_used": ["run_shell_command"] * tool_calls,
        "in_flight": [],
        "requests": requests or None, "models": [args.model] if requests else [],
        "tokens_in": tokens_in or None, "tokens_out": tokens_out or None,
        "tokens_total": tokens_total or None, "tokens_cached": tokens_cached or None,
        "tool_calls": tool_calls,
        "errors": errors[-3:], "stderr_tail": errors[-1] if errors and exit_code else "",
        "decider_seen": decider_seen,
    }


def stderr_tail(err: str, limit: int = 1500) -> str:
    """The end of Gemini CLI's stderr without JS stack frames: a 1500-char
    tail of a raw stack trace is all `at …` lines and no error message."""
    lines = [l for l in err.splitlines() if not re.match(r"\s+at ", l)]
    return "\n".join(lines)[-limit:]


def scripted_plan(tool: str, task: str, base: str) -> list[list[str]]:
    """Known-good navigera plans (refs found by --text / selectors)."""
    s = ["-s", "eval"]
    if TOOLS[tool].get("engine"):
        s += ["--engine", TOOLS[tool]["engine"]]
    plans = {
        "lookup": [["start"], ["goto", f"{base}/shop/p/103"], ["wait", "--selector", "#stock"],
                   ["--raw", "ax", "--selector", "#detail"]],
        "purchase": [["start"], ["goto", f"{base}/shop?q=brass+sprocket"], ["click", "--text", "Accept all"],
                     ["click", "--text", "Add Brass Sprocket to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '1'"],
                     ["click", "--text", "Add Brass Sprocket to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '2'"],
                     ["goto", f"{base}/shop?q=titanium+widget"], ["click", "--text", "Add Titanium Widget to cart"],
                     ["wait", "--js", "document.querySelector('#cart-count').textContent === '3'"],
                     ["goto", f"{base}/cart"], ["fill", "--text", "Coupon code", "SAVE10"],
                     ["click", "--text", "Apply coupon"], ["wait", "--text", "Discount (SAVE10)"],
                     ["click", "--text", "Proceed to checkout"], ["fill", "--selector", "#name", "Ada Lovelace"],
                     ["select", "--selector", "#country", "Canada"],
                     ["click", "--selector", "input[value=express]"], ["click", "--text", "Continue to payment"],
                     ["eval", "() => { const d = document.querySelector('#payframe').contentDocument;"
                              " for (const [id, v] of [['card','4242 4242 4242 4242'],['exp','12/30'],['cvc','123']])"
                              " { const el = d.getElementById(id); el.value = v; el.dispatchEvent(new Event('input')); } return true; }"],
                     ["click", "--text", "Place order"], ["wait", "--url", "/orders/"],
                     ["--raw", "eval", "document.querySelector('#order-id').textContent"]],
        "account": [["start"], ["goto", f"{base}/account"], ["fill", "--selector", "#email", "demo@acme.test"],
                    ["fill", "--selector", "#password", "hunter2"], ["press", "Enter", "--selector", "#password"],
                    ["click", "--text", "Next page"], ["--raw", "ax", "--selector", "table"]],
        "docs": [["start"], ["goto", f"{base}/shop"], ["click", "--text", "Docs"],
                 ["click", "--text", "Returns FAQ"], ["--raw", "ax", "--selector", "#returns-faq"]],
        "support": [["start"], ["goto", f"{base}/support"],
                    ["eval", "() => { const d = document.querySelector('iframe').contentDocument;"
                             " d.getElementById('category').value = 'Billing';"
                             " d.querySelector('input[value=high]').checked = true;"
                             " d.getElementById('message').value = 'Charged twice for order A-1042';"
                             " d.getElementById('send').click(); return true; }"],
                    ["wait", "--ms", "500"],
                    ["--raw", "eval", "document.querySelector('iframe').contentDocument.body.innerText"]],
        "preferences": [["start"], ["goto", f"{base}/preferences"], ["click", "--text", "Accept all"],
                        ["click", "--selector", "input[name=price_drop]"],
                        ["click", "--selector", "input[name=newsletter]"],
                        ["select", "--selector", "select[name=currency]", "EUR"],
                        ["select", "--selector", "select[name=history_page_size]", "50"],
                        ["click", "--text", "Save preferences"], ["wait", "--text", "Preferences saved."],
                        ["--raw", "text", "--selector", "#saved"]],
    }
    return [s + p for p in plans[task]] + [s + ["quit"]]


def run_scripted(tool: str, task: str, base: str, work: Path, env: dict) -> dict:
    started = time.time()
    shells, outputs = [], []
    plan = scripted_plan(tool, task, base)
    for argv in plan:
        p = subprocess.run([exe("navigera", env), *argv], cwd=work, env=env, capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=120)
        shells.append("navigera " + " ".join(argv))
        outputs.append(p.stdout)
        if p.returncode != 0:
            # The session must not outlive a failed plan: the next run would
            # find it and drive the wrong browser.
            subprocess.run([exe("navigera", env), *plan[-1]], cwd=work, env=env, capture_output=True, timeout=60)
            return {"exit": p.returncode, "timed_out": False, "wall_s": round(time.time() - started, 1),
                    "text": "", "shell_commands": shells, "tools_used": [], "requests": 0,
                    "errors": [p.stdout[-500:] + p.stderr[-500:]], "stderr_tail": p.stderr[-800:]}
    last = outputs[-2] if len(outputs) > 1 else ""
    return {"exit": 0, "timed_out": False, "wall_s": round(time.time() - started, 1),
            "text": "FINAL ANSWER: " + " ".join(last.split())[-600:], "shell_commands": shells,
            "tools_used": ["run_shell_command"] * len(shells), "requests": 0, "errors": []}


TOOL_PATTERNS = ("agent-browser", "playwright-cli", "cli-daemon", "navigera")

# Windows has no pkill -f: match command lines through CIM, skip this
# harness and its ancestors (their command lines name the tools too).
WIN_CLEANUP = r"""
$keep = @{}; $p = $PID
while ($p) { $keep[[int]$p] = 1; $p = (Get-CimInstance Win32_Process -Filter "ProcessId=$p").ParentProcessId }
foreach ($x in $env:KEEP_PIDS -split ',') { if ($x) { $keep[[int]$x] = 1 } }
Get-CimInstance Win32_Process | Where-Object {
  -not $keep[[int]$_.ProcessId] -and $_.CommandLine -and (
    $_.CommandLine -like "*$env:RUN_ROOT*" -or
    $_.Name -in @('navigera.exe', 'agent-browser.exe') -or
    ($_.Name -eq 'node.exe' -and ($_.CommandLine -match 'playwright-cli|cli-daemon|agent-browser')))
} | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
"""


def cleanup(root: Path):
    """Each tool's own daemon/browser must not leak into the next run."""
    if WINDOWS:
        env = {**os.environ, "RUN_ROOT": root.name, "KEEP_PIDS": str(os.getpid())}
        subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", WIN_CLEANUP],
                       env=env, capture_output=True, timeout=120)
        return
    for pat in TOOL_PATTERNS:
        subprocess.run(["pkill", "-f", f"{pat}.*{root.name}"], capture_output=True)


# --- checks -----------------------------------------------------------------

def final_answer(text: str) -> str:
    m = re.findall(r"FINAL ANSWER:\s*(.+)", text)
    return m[-1].strip() if m else text.strip()[-400:]


def expected_browser(tool: str) -> str | None:
    """The browser a tool must stay on, as the site sees it (User-Agent)."""
    spec = TOOLS.get(tool, {})
    if spec.get("engine") == "lightpanda" or spec.get("env", {}).get("AGENT_BROWSER_ENGINE") == "lightpanda":
        return "Lightpanda"
    return None


def check(task: dict, answer: str, site, tool: str = "") -> tuple[bool, list[str]]:
    c, why = task["check"], []
    state = {} if task.get("live") else site.get("/__state")
    want = expected_browser(tool)
    if want and state.get("browsers"):
        others = {b: n for b, n in state["browsers"].items() if b != want}
        if others:
            why.append(f"left the {want} engine: pages loaded by {others}")
    if "answer_product" in c:
        item = site.get("/api/products?q=" + urllib.request.quote(c["answer_product"].lower()))["items"][0]
        if f"{item['price']:.2f}" not in answer:
            why.append(f"price {item['price']:.2f} not in answer")
        if not re.search(rf"(?<!\d){item['stock']}(?!\d)", answer):
            why.append(f"stock {item['stock']} not in answer")
    for needle in c.get("answer_contains", []):
        if needle.lower() not in answer.lower():
            why.append(f"{needle!r} not in answer")
    if "login" in c and c["login"] not in state["logins"]:
        why.append("never logged in")
    if "order" in c:
        want = c["order"]
        orders = state["orders"]
        if len(orders) != 1:
            why.append(f"{len(orders)} orders placed (want 1)")
        if orders:
            o = orders[-1]
            got = {i["name"]: i["qty"] for i in o["items"]}
            if got != want["items"]:
                why.append(f"items {got}")
            for k in ("coupon", "country", "speed", "name"):
                if str(o.get(k)) != want[k]:
                    why.append(f"{k}={o.get(k)!r}")
            if o["id"] not in answer:
                why.append(f"order id {o['id']} not in answer")
    if "preferences" in c:
        saved = list(state.get("preferences", {}).values())
        if not saved:
            why.append("preferences never saved")
        else:
            got, defaults = saved[-1], site.get("/__defaults")
            for k, v in c["preferences"].items():
                if got.get(k) != v:
                    why.append(f"{k}={got.get(k)!r} (want {v!r})")
            changed = sorted(k for k, v in got.items() if k not in c["preferences"] and v != defaults.get(k))
            if changed:
                why.append(f"also changed {', '.join(changed)}")
    if "ticket" in c:
        want = c["ticket"]
        tickets = state["tickets"]
        if not tickets:
            why.append("no ticket created")
        else:
            t = tickets[-1]
            for k in ("category", "priority"):
                if t.get(k) != want[k]:
                    why.append(f"{k}={t.get(k)!r}")
            if want["message_contains"] not in (t.get("message") or ""):
                why.append("message differs")
            if t["id"] not in answer:
                why.append(f"ticket id {t['id']} not in answer")
    return not why, why


# A find/check op (after any options), or --find on an element op.
DECISION = re.compile(r"navigera(\s+(-s|--session)\s+\S+|\s+--?[\w-]+)*\s+(find|check)\s|\bnavigera\b.*\s--find\b")


def read_log(path: str | None) -> list:
    if not path or not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def log_lines(path: str | None) -> int:
    return len(read_log(path))


OFF_TOOL = re.compile(r"\b(curl|wget|httpie)\b.*127\.0\.0\.1|require\(['\"](playwright|puppeteer)|"
                      r"from playwright|import (playwright|puppeteer)|chromedp|selenium", re.I)


def smoke(args) -> int:
    """A minimal model call through the same settings as the eval runs, so a
    bad key, a model id or a helper-model quota shows up before any task."""
    root = Path(tempfile.mkdtemp(prefix="eval-smoke-"))
    work, env = prepare("navigera", "skilled", root, args)
    res = run_openai("Reply with the single word OK.", work, env, args) if args.agent == "openai" \
        else run_gemini("Reply with the single word OK.", work, env, args)
    print(json.dumps({"exit": res["exit"], "models": res["models"], "text": res["text"][:80],
                      "tokens_total": res["tokens_total"]}))
    if res["exit"] != 0 or "OK" not in res["text"].upper():
        print(res.get("stderr_tail") or "", file=sys.stderr)
        return 1
    extra = [m for m in res["models"] if m != args.model and m not in HELPER_MODELS]
    if extra:
        print(f"{args.model} fell back to {extra}: its quota is likely spent for today", file=sys.stderr)
        return 1
    return 0


def label(run: dict) -> str:
    """Row name: the tool, plus the OS when it isn't Linux."""
    return run["tool"] + (f" ({run['os']})" if run.get("os", "linux") != "linux" else "")


def render(mode: str, agent: str, model: str, runs: list, tools: list, tasks: list) -> tuple[str, list]:
    """Markdown table (per tool, then per task) and the per-tool summary.
    `tools` are row labels (see `label`)."""
    def med(xs):
        xs = [x for x in xs if isinstance(x, (int, float))]
        return statistics.median(xs) if xs else None

    lines = [f"### Agent eval — {mode} ({agent}, model {model if agent != 'scripted' else '—'})", "",
             "| tool | passed | median model turns | median shell cmds | median tokens | median wall | off-tool cmds |",
             "|---|---|---|---|---|---|---|"]
    summary = []
    for tool in tools:
        rs = [r for r in runs if label(r) == tool and not r.get("infra")]
        infra_n = sum(1 for r in runs if label(r) == tool and r.get("infra"))
        passed = sum(1 for r in rs if r["ok"])
        row = {"tool": tool, "passed": passed, "runs": len(rs), "infra": infra_n,
               "median_requests": med([r.get("requests") for r in rs]),
               "median_shell": med([r.get("shell_count") for r in rs]),
               "median_tokens": med([r.get("tokens_total") for r in rs]),
               "median_wall_s": med([r.get("wall_s") for r in rs]),
               "off_tool": sum(len(r.get("off_tool_commands") or []) for r in rs)}
        summary.append(row)
        fmt = lambda v, f="{:.0f}": "—" if v is None else f.format(v)
        lines.append(f"| `{tool}` | {passed}/{len(rs)}" + (f" (+{infra_n} ⚠ infra)" if infra_n else "") + f" | {fmt(row['median_requests'])} | {fmt(row['median_shell'])} | "
                     f"{fmt(row['median_tokens'])} | {fmt(row['median_wall_s'], '{:.0f}s')} | {row['off_tool']} |")
    lines += ["", "| task | " + " | ".join(f"`{t}`" for t in tools) + " |",
              "|---|" + "---|" * len(tools)]
    for task in tasks:
        cells = []
        for tool in tools:
            rs = [r for r in runs if label(r) == tool and r["task"] == task]
            cells.append(" ".join(
                ("⚠" if r.get("infra") else "✓" if r["ok"] else "✗")
                + (f" {r['tokens_total'] // 1000}K" if r.get("tokens_total") else "")
                for r in rs) or "—")
        lines.append(f"| {task} | " + " | ".join(cells) + " |")
    deciding = [t for t in tools if any("decider_calls" in r for r in runs if label(r) == t)]
    if deciding:
        lines += ["", "| tool | runs that saw the decider | runs that used find/check | find/check commands | "
                  "decider calls | decider input tokens | decider errors |", "|---|---|---|---|---|---|---|"]
        for tool in deciding:
            rs = [r for r in runs if label(r) == tool and not r.get("infra")]
            lines.append(f"| `{tool}` | {sum(1 for r in rs if r.get('decider_seen'))}/{len(rs)} | "
                         f"{sum(1 for r in rs if r.get('decision_commands'))}/{len(rs)} | "
                         f"{sum(r.get('decision_commands') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_calls') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_tokens') or 0 for r in rs)} | "
                         f"{sum(r.get('decider_errors') or 0 for r in rs)} |")
    fails = [r for r in runs if not r["ok"]]
    if fails:
        lines += ["", "Failures:"]
        for r in fails:
            lines.append(f"- `{label(r)}` {r['task']}: {'; '.join(r.get('why', []))[:240]}"
                         + (" (timed out)" if r.get("timed_out") else "")
                         + (f" — {r['errors'][-1][:160]}" if r.get("errors") else ""))
            # What the agent did last explains most failures, and this table
            # is the one notice a long run's JSON chunks cannot crowd out.
            tail = [" ".join(c.split()).replace("`", "'")[:110] for c in (r.get("shell_commands") or [])[-8:]]
            if tail:
                lines.append("  last commands: " + " · ".join(f"`{c}`" for c in tail))
    return "\n".join(lines) + "\n", summary


def merge(paths: list[str], out: Path) -> int:
    """Combine per-job summary-<mode>.json files (one per parallel job)."""
    by_mode: dict = {}
    for p in paths:
        doc = json.loads(Path(p).read_text())
        m = by_mode.setdefault(doc["mode"], {"agent": doc["agent"], "model": doc["model"], "runs": []})
        m["runs"] += doc["runs"]
    out.mkdir(parents=True, exist_ok=True)
    order = list(TOOLS)
    for mode, m in by_mode.items():
        runs = m["runs"]
        tools = sorted({label(r) for r in runs},
                       key=lambda t: (order.index(t.split(" ")[0]) if t.split(" ")[0] in order else 99, t))
        tasks = list(dict.fromkeys(r["task"] for r in runs))
        table, summary = render(mode, m["agent"], m["model"], runs, tools, tasks)
        (out / f"table-{mode}.md").write_text(table)
        (out / f"summary-{mode}.json").write_text(json.dumps(
            {"mode": mode, "agent": m["agent"], "model": m["model"], "tools": summary, "runs": runs}, indent=1))
        print(table)
    return 0


# --- main -------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools", default="navigera,navigera-lightpanda,agent-browser,playwright-cli")
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--mode", choices=["skilled", "onboard"], default="skilled")
    ap.add_argument("--agent", choices=["gemini", "openai", "scripted"], default="gemini")
    ap.add_argument("--model", default="gemini-3.5-flash-lite")
    ap.add_argument("--gemini", default="gemini")
    ap.add_argument("--base-url", default="",
                    help="--agent openai: chat-completions base URL (default: $OPENAI_BASE_URL or SiliconFlow)")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--run-timeout", type=int, default=900)
    ap.add_argument("--nv-bin-dir", default=str(REPO / "target" / "release"),
                    help="directory holding the navigera binary under test")
    ap.add_argument("--out", required=True)
    ap.add_argument("--merge", nargs="*", help="combine these summary-<mode>.json files into --out and exit")
    ap.add_argument("--require-pass", action="store_true", help="exit 1 unless every run passed (CI checks)")
    ap.add_argument("--decider-url", help="decision-model endpoint for navigera+jev (bench/decide/relay.py)")
    ap.add_argument("--decider-log", help="the relay's request log, to count each run's decider calls")
    ap.add_argument("--smoke", action="store_true",
                    help="one 'reply OK' call with exactly the runs' settings; prints the models used")
    args = ap.parse_args()
    if args.merge:
        return merge(args.merge, Path(args.out))
    if args.smoke:
        return smoke(args)

    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    if args.tasks != "all":
        wanted = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in wanted]
    else:
        # Live-internet tasks are informational: only run when named.
        tasks = [t for t in tasks if not t.get("live")]
    if args.mode == "onboard":
        tasks = [t for t in tasks if t["id"] in ("lookup", "docs")][:1] or tasks[:1]
    tools = [t for t in args.tools.split(",") if t]
    if args.agent == "scripted":
        tools = ["navigera"]
        if os.environ.get("LIGHTPANDA_BIN"):
            tools.append("navigera-lightpanda")
        # An explicit --tools narrows the scripted set (e.g. lightpanda-only
        # on a box with no Chrome).
        tools = [t for t in tools if t in args.tools.split(",")]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    runs = []
    for rep in range(args.reps):
        for task in tasks:
            for tool in tools:
                root = Path(tempfile.mkdtemp(prefix=f"eval-{tool}-{task['id']}-"))
                rec = {"tool": tool, "task": task["id"], "rep": rep, "mode": args.mode, "agent": args.agent,
                       "os": OS_NAME}
                site = None
                try:
                    site = LiveSite(task["url"]) if task.get("live") else Site()
                    work, env = prepare(tool, args.mode, root, args)
                    spec = TOOLS[tool]
                    template = SKILLED_PROMPT if args.mode == "skilled" else ONBOARD_PROMPT
                    prompt = template.format(workdir=work, cmd=spec["cmd"], skill=spec["skill"], name=tool,
                                             repo=spec["repo"], base=site.base, task=task["prompt"],
                                             hint=spec.get("hint", ""))
                    before = log_lines(args.decider_log)
                    res = run_gemini(prompt, work, env, args) if args.agent == "gemini" \
                        else run_openai(prompt, work, env, args) if args.agent == "openai" \
                        else run_scripted(tool, task["id"], site.base, work, env)
                    if spec.get("decider"):
                        calls = read_log(args.decider_log)[before:]
                        res["decider_calls"] = len(calls)
                        res["decider_tokens"] = sum((c.get("usage") or {}).get("input_tokens") or 0 for c in calls)
                        res["decider_errors"] = sum(1 for c in calls if c.get("status") != 200)
                    answer = final_answer(res["text"])
                    ok, why = check(task, answer, site, tool)
                    shells = res["shell_commands"]
                    rec.update(res)
                    # Model quota or outage, not the tool: reported apart and
                    # left out of pass rates.
                    # "failed sending request" / "fetch failed" / ECONN*: the
                    # model API call itself never got through.
                    infra = re.search(r"Quota exceeded|RESOURCE_EXHAUSTED|status: (429|503)|\b503\b.*UNAVAILABLE"
                                      r"|failed sending request|fetch failed|ECONNRESET|ETIMEDOUT|EAI_AGAIN"
                                      r"|[Ii]nsufficient balance",
                                      (res.get("stderr_tail") or "") + " ".join(res.get("errors") or []))
                    # On a quota error Gemini CLI silently retries on the next
                    # model of its fallback chain: such a run no longer
                    # measures the requested model, pass or fail.
                    switched = [m for m in res.get("models") or [] if m != args.model and m not in HELPER_MODELS]
                    infra_why = (["model quota/outage (infra)"] if infra and not ok else []) + \
                                ([f"model switched to {', '.join(switched)} (infra)"] if switched else [])
                    stuck = ([f"stuck in: {res['in_flight'][-1][:160]}"]
                             if res.get("timed_out") and res.get("in_flight") else [])
                    rec.update({
                        "infra": bool(infra_why),
                        "ok": ok and not switched, "why": why + infra_why + stuck,
                        "answer": answer[-300:],
                        "tool_commands": sum(1 for c in shells if spec["cmd"] in c),
                        "off_tool_commands": [c[:200] for c in shells if OFF_TOOL.search(c)],
                        "shell_count": len(shells),
                        "decision_commands": sum(1 for c in shells if DECISION.search(c)),
                    })
                except Exception as e:  # harness/setup problem: record, keep going
                    rec.update({"ok": False, "why": [f"harness: {e}"]})
                finally:
                    if site:
                        site.close()
                    cleanup(root)
                rec.pop("text", None)
                runs.append(rec)
                status = "PASS" if rec["ok"] else "FAIL " + "; ".join(rec.get("why", []))[:200]
                print(f"[{args.mode}] {tool:15} {task['id']:9} rep{rep}: {status} "
                      f"(requests={rec.get('requests')}, shell={rec.get('shell_count')}, "
                      f"wall={rec.get('wall_s')}s)", flush=True)
                (out / "runs.jsonl").open("a").write(json.dumps(rec) + "\n")

    labels = [label({"tool": t, "os": OS_NAME}) for t in tools]
    table, summary = render(args.mode, args.agent, args.model, runs, labels, [t["id"] for t in tasks])
    (out / f"table-{args.mode}.md").write_text(table)
    (out / f"summary-{args.mode}.json").write_text(json.dumps({"mode": args.mode, "agent": args.agent,
                                                               "model": args.model, "tools": summary,
                                                               "runs": runs}, indent=1))
    if args.require_pass:
        if any(not r["ok"] and not r.get("infra") for r in runs):
            return 1
        if any(r.get("infra") for r in runs):
            print("::warning title=agent check::model quota/outage; the tool was not judged")
    print(table)
    return 0


if __name__ == "__main__":
    sys.exit(main())
