#!/usr/bin/env python3
"""Agent-style scenario: how an AI agent's tool calls drive a browser.

CLI tools run one process per step against a warm background session (the
way a shell tool works); MCP servers get one tools/call per step over a
persistent stdio connection (the way an MCP client works). Both run the
ladder's canonical session (listing page: title, count, 500-card extract;
fill + click filter; visible count; screenshot; detail page in a new tab:
title, rows) plus one accessibility snapshot, the agent's usual way to look
at a page.

Tools:
  nv      navigera --session <name> <op>          (Rust, this repo)
  ab      agent-browser --session <name> --json <op>  (Vercel Labs, Rust daemon)
  pw      playwright-cli -s=<name> <op>               (Microsoft, Playwright daemon)
  pwmcp   @playwright/mcp over stdio                   (Microsoft, MCP)
  cdmcp   chrome-devtools-mcp over stdio               (Google, Puppeteer, MCP)

Prints one JSON line: {tool, wall_s, steps:[{op, ms}], snapshot_bytes,
counts, extract_out}. Env: NAVIGERA, CHROME_BIN, LADDER_BASE,
AGENT_BROWSER, PLAYWRIGHT_CLI, PLAYWRIGHT_MCP, CHROME_DEVTOOLS_MCP (binaries).
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time

# Same expressions as nv_serve.py, written as IIFEs so a tool that evaluates
# the expression as-is (rather than calling a returned function) gets values.
TITLE = "(() => document.title)()"
CARDS = "(() => document.querySelectorAll('.card').length)()"
EXTRACT = ("(() => Array.from(document.querySelectorAll('.card')).map(c => ({"
           "t: c.querySelector('.t').textContent, "
           "p: c.querySelector('.p').textContent, "
           "v: c.dataset.vendor})))()")
VISIBLE = "(() => document.querySelectorAll('.card:not(.hidden)').length)()"
ROWS = "(() => document.querySelectorAll('#rows tr').length)()"


def run(argv: list[str], env: dict, cwd: str | None = None) -> tuple[float, str]:
    t0 = time.perf_counter()
    p = subprocess.run(argv, env=env, cwd=cwd, capture_output=True, text=True, timeout=120)
    ms = (time.perf_counter() - t0) * 1000.0
    if p.returncode != 0:
        raise RuntimeError(f"{argv[:5]}... exited {p.returncode}\nstdout: {p.stdout[-1500:]}\n"
                           f"stderr: {p.stderr[-1500:]}")
    return ms, p.stdout


def result_of(tool: str, out: str):
    if tool == "pw":
        return json.loads(out) if out.strip() else None
    doc = json.loads(out.strip().splitlines()[-1])
    if tool == "nv":
        if not doc.get("ok"):
            raise RuntimeError(f"navigera error: {doc.get('error')}")
        return doc.get("result")
    if not doc.get("success", False):
        raise RuntimeError(f"agent-browser error: {doc.get('error')}")
    data = doc.get("data") or {}
    return data.get("result", data)


class Mcp:
    """Minimal MCP stdio client (JSON-RPC, one message per line)."""

    def __init__(self, argv: list[str], env: dict, cwd: str):
        self.log = open(os.path.join(cwd, "mcp-stderr.log"), "w")
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=self.log, env=env, cwd=cwd, text=True, bufsize=1)
        self.next_id = 0
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "ladder", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.schemas = {t["name"]: t.get("inputSchema", {}).get("properties", {})
                        for t in self.request("tools/list", {}).get("tools", [])}

    def send(self, msg: dict):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def request(self, method: str, params: dict):
        self.next_id += 1
        rid = self.next_id
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"MCP server exited during {method}")
            msg = json.loads(line)
            if msg.get("id") == rid:
                if "error" in msg:
                    raise RuntimeError(f"MCP {method}: {msg['error']}")
                return msg["result"]

    def tool(self, name: str, args: dict) -> tuple[float, str]:
        t0 = time.perf_counter()
        res = self.request("tools/call", {"name": name, "arguments": args})
        ms = (time.perf_counter() - t0) * 1000.0
        text = "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
        if res.get("isError"):
            raise RuntimeError(f"MCP tool {name} failed: {text[:800]}")
        return ms, text

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()
            self.p.wait()
        self.log.close()


def mcp_json(text: str):
    """The JSON value an MCP evaluate tool printed (fenced or after a heading)."""
    m = re.search(r"```(?:json)?\n(.*?)\n```", text, re.S)
    if m:
        return json.loads(m.group(1))
    m = re.search(r"### Result\n(.*?)(?:\n### |\Z)", text, re.S)
    if m:
        return json.loads(m.group(1).strip())
    raise RuntimeError(f"no JSON result in MCP output: {text[:400]}")


def ref_for(snapshot: str, pattern: str) -> str:
    for line in snapshot.splitlines():
        if re.search(pattern, line):
            m = re.search(r"\[ref=([^\]]+)\]", line)
            if m:
                return m.group(1)
    raise RuntimeError(f"no ref for {pattern!r} in snapshot")


def uid_for(snapshot: str, pattern: str) -> str:
    for line in snapshot.splitlines():
        if re.search(pattern, line):
            m = re.search(r"uid=(\S+)", line)
            if m:
                return m.group(1)
    raise RuntimeError(f"no uid for {pattern!r} in snapshot")


def run_cli(tool: str, args, base: str, env: dict):
    s = args.session
    cwd = None
    if tool == "nv":
        exe = os.environ["NAVIGERA"]

        def cmd(*a):
            return [exe, "--session", s, *a]

        def ev(js):
            return cmd("eval", js)

        plan = [
            ("start", cmd("start", "--chromium", args.chrome or env["CHROME_BIN"])),
            ("goto", cmd("goto", f"{base}/page_a.html")),
            ("eval title", ev(TITLE)),
            ("eval count", ev(CARDS)),
            ("eval extract", ev(EXTRACT)),
            ("snapshot", cmd("ax")),
            ("fill", cmd("fill", "--selector", "#q", "--value", "widget")),
            ("click", cmd("click", "--selector", "#search")),
            ("eval visible", ev(VISIBLE)),
            ("screenshot", cmd("screenshot", "--path", args.shot_out)),
            ("tab new", cmd("tab-new", f"{base}/page_b.html")),
            ("eval title b", ev(TITLE)),
            ("eval rows", ev(ROWS)),
            ("close", cmd("quit")),
        ]
    elif tool == "ab":
        exe = os.environ.get("AGENT_BROWSER", "agent-browser")
        env.setdefault("AGENT_BROWSER_EXECUTABLE_PATH", args.chrome or env.get("CHROME_BIN", ""))
        # Same sandbox setting every other contender launches Chrome with.
        env.setdefault("AGENT_BROWSER_ARGS", "--no-sandbox")

        def cmd(*a):
            return [exe, "--session", s, "--json", *a]

        def ev(js):
            return cmd("eval", js)

        plan = [
            ("goto", cmd("open", f"{base}/page_a.html")),  # also starts daemon + browser
            ("eval title", ev(TITLE)),
            ("eval count", ev(CARDS)),
            ("eval extract", ev(EXTRACT)),
            ("snapshot", cmd("snapshot")),
            ("fill", cmd("fill", "#q", "widget")),
            ("click", cmd("click", "#search")),
            ("eval visible", ev(VISIBLE)),
            ("screenshot", cmd("screenshot", args.shot_out)),
            ("tab new", cmd("tab", "new", f"{base}/page_b.html")),
            ("eval title b", ev(TITLE)),
            ("eval rows", ev(ROWS)),
            ("close", cmd("close")),
        ]
    else:  # pw: playwright-cli
        exe = os.environ.get("PLAYWRIGHT_CLI", "playwright-cli")
        cwd = tempfile.mkdtemp(prefix="pwcli-")
        os.makedirs(os.path.join(cwd, ".playwright"))
        with open(os.path.join(cwd, ".playwright", "cli.config.json"), "w") as f:
            json.dump({"browser": {"browserName": "chromium", "launchOptions": {
                "executablePath": args.chrome or env["CHROME_BIN"], "headless": True,
                "chromiumSandbox": False}}, "outputDir": cwd}, f)
        shot = os.path.basename(args.shot_out)

        def cmd(*a):
            return [exe, f"-s={s}", *a]

        def ev(js):
            return cmd("--raw", "eval", js)

        plan = [
            ("goto", cmd("open", f"{base}/page_a.html")),  # starts daemon + browser
            ("eval title", ev(TITLE)),
            ("eval count", ev(CARDS)),
            ("eval extract", ev(EXTRACT)),
            ("snapshot", cmd("snapshot")),
            ("fill", cmd("fill", "#q", "widget")),
            ("click", cmd("click", "#search")),
            ("eval visible", ev(VISIBLE)),
            ("screenshot", cmd("screenshot", f"--filename={shot}")),
            ("tab new", cmd("tab-new", f"{base}/page_b.html")),
            ("eval title b", ev(TITLE)),
            ("eval rows", ev(ROWS)),
            ("close", cmd("close")),
        ]

    steps, values, snapshot_bytes = [], {}, 0
    t0 = time.perf_counter()
    for name, argv in plan:
        ms, out = run(argv, env, cwd)
        steps.append({"op": name, "ms": round(ms, 2)})
        if name.startswith("eval"):
            values[name] = result_of(tool, out)
        elif name == "snapshot":
            snapshot_bytes = len(out.encode())
            if tool != "pw":
                result_of(tool, out)  # raises on a failed snapshot
        elif name not in ("start", "close") and tool != "pw":
            result_of(tool, out)
    return time.perf_counter() - t0, steps, values, snapshot_bytes


def run_mcp(tool: str, args, base: str, env: dict):
    chrome = args.chrome or env["CHROME_BIN"]
    cwd = tempfile.mkdtemp(prefix=f"{tool}-")
    steps, values = [], {}
    t0 = time.perf_counter()
    if tool == "pwmcp":
        exe = os.environ.get("PLAYWRIGHT_MCP", "playwright-mcp")
        argv = [exe, "--headless", "--browser", "chromium", "--executable-path", chrome,
                "--isolated", "--no-sandbox", "--output-dir", cwd]
    else:
        exe = os.environ.get("CHROME_DEVTOOLS_MCP", "chrome-devtools-mcp")
        argv = [exe, "--headless", "--executable-path", chrome, "--isolated",
                "--chrome-arg=--no-sandbox", "--no-usage-statistics"]
    mcp = Mcp(argv, env, cwd)
    steps.append({"op": "start", "ms": round((time.perf_counter() - t0) * 1000.0, 2)})

    def step(name, tool_name, tool_args):
        ms, text = mcp.tool(tool_name, tool_args)
        steps.append({"op": name, "ms": round(ms, 2)})
        return text

    def ev(name, expr):
        fn = f"() => {expr}"
        values[name] = mcp_json(step(name, "browser_evaluate" if tool == "pwmcp" else "evaluate_script",
                                     {"function": fn}))

    snapshot = ""
    try:
        if tool == "pwmcp":
            step("goto", "browser_navigate", {"url": f"{base}/page_a.html"})
            ev("eval title", TITLE)
            ev("eval count", CARDS)
            ev("eval extract", EXTRACT)
            snapshot = step("snapshot", "browser_snapshot", {})
            if "target" in mcp.schemas.get("browser_type", {}):
                # Current releases take a ref or a selector as `target`.
                step("fill", "browser_type", {"target": "#q", "text": "widget"})
                step("click", "browser_click", {"target": "#search"})
            else:
                # Older releases: refs from the snapshot only.
                box = ref_for(snapshot, r"- (textbox|searchbox)")
                step("fill", "browser_type", {"element": "search box", "ref": box, "text": "widget"})
                step("click", "browser_click", {"element": "Search button", "ref": ref_for(snapshot, r'button "Search')})
            ev("eval visible", VISIBLE)
            step("screenshot", "browser_take_screenshot", {"filename": os.path.basename(args.shot_out)})
            if "url" in mcp.schemas.get("browser_tabs", {}):
                step("tab new", "browser_tabs", {"action": "new", "url": f"{base}/page_b.html"})
            else:  # older releases: new tab, then navigate it (one logical step)
                ms1, _ = mcp.tool("browser_tabs", {"action": "new"})
                ms2, _ = mcp.tool("browser_navigate", {"url": f"{base}/page_b.html"})
                steps.append({"op": "tab new", "ms": round(ms1 + ms2, 2)})
            ev("eval title b", TITLE)
            ev("eval rows", ROWS)
            step("close", "browser_close", {})
        else:
            # 1.10 defaults to --page-id-routing: page tools need a pageId.
            page = {"id": None}

            def selected(text):
                m = re.search(r"^(\d+): .*\[selected\]", text, re.M)
                if m:
                    page["id"] = int(m.group(1))

            def cd(name, tool_name, tool_args):
                if "pageId" in mcp.schemas.get(tool_name, {}) and page["id"] is not None:
                    tool_args = {**tool_args, "pageId": page["id"]}
                text = step(name, tool_name, tool_args)
                selected(text)
                return text

            def cdev(name, expr):
                values[name] = mcp_json(cd(name, "evaluate_script", {"function": f"() => {expr}"}))

            if "pageId" in mcp.schemas.get("navigate_page", {}):
                selected(step("list pages", "list_pages", {}))
            cd("goto", "navigate_page", {"type": "url", "url": f"{base}/page_a.html"})
            cdev("eval title", TITLE)
            cdev("eval count", CARDS)
            cdev("eval extract", EXTRACT)
            snapshot = cd("snapshot", "take_snapshot", {})
            cd("fill", "fill", {"uid": uid_for(snapshot, r"textbox|searchbox"), "value": "widget"})
            cd("click", "click", {"uid": uid_for(snapshot, r'button "Search')})
            cdev("eval visible", VISIBLE)
            cd("screenshot", "take_screenshot", {"filePath": args.shot_out})
            cd("tab new", "new_page", {"url": f"{base}/page_b.html"})
            cdev("eval title b", TITLE)
            cdev("eval rows", ROWS)
    finally:
        c0 = time.perf_counter()
        mcp.close()
        steps.append({"op": "close" if tool != "pwmcp" else "exit", "ms": round((time.perf_counter() - c0) * 1000.0, 2)})
    return time.perf_counter() - t0, steps, values, len(snapshot.encode())


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", choices=["nv", "ab", "pw", "pwmcp", "cdmcp"], required=True)
    ap.add_argument("--extract-out", required=True)
    ap.add_argument("--shot-out", required=True)
    ap.add_argument("--chrome", help="browser binary (default $CHROME_BIN)")
    ap.add_argument("--session", default=f"ladder-{os.getpid()}")
    args = ap.parse_args()

    base = os.environ["LADDER_BASE"]
    env = dict(os.environ)
    if args.tool in ("pwmcp", "cdmcp"):
        wall, steps, values, snapshot_bytes = run_mcp(args.tool, args, base, env)
    else:
        wall, steps, values, snapshot_bytes = run_cli(args.tool, args, base, env)
    with open(args.extract_out, "w", encoding="utf-8") as f:
        json.dump(values.get("eval extract"), f)
    counts = {
        "title_a": values.get("eval title"),
        "cards_a": values.get("eval count"),
        "visible": values.get("eval visible"),
        "title_b": values.get("eval title b"),
        "rows_b": values.get("eval rows"),
    }
    print(json.dumps({"tool": args.tool, "wall_s": wall, "steps": steps,
                      "snapshot_bytes": snapshot_bytes, "counts": counts,
                      "extract_out": args.extract_out}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
