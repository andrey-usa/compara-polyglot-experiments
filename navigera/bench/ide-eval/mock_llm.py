#!/usr/bin/env python3
"""A scripted, OpenAI-compatible chat model for testing agent plumbing.

It plays the `lookup` task the way a careful model would, through whatever
browser the agent was given: an MCP server's `browser_navigate` and
`browser_snapshot` tools, or `navigera` commands through the agent's own
shell tool. It decides each step from the conversation it is sent (no state
of its own), so it also checks that the agent really delivered the tool
output back: the final answer is read out of those results.

    python3 mock_llm.py --port 8790 [--log requests.jsonl]

Then point an agent at http://127.0.0.1:8790/v1 with any model id and key.
Requests without tools (session titles, summaries) get a short plain answer.
"""
import argparse
import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SHELL_TOOLS = ("bash", "run_commands", "execute_command", "run_terminal_command", "shell", "run_shell_command")


def text_of(content) -> str:
    """Message content as plain text (strings, or lists of content parts)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(p.get("text", p.get("content", ""))) if isinstance(p, dict) else str(p) for p in content)
    return "" if content is None else str(content)


def tool_names(tools: list) -> dict:
    """name -> parameter schema, for OpenAI-style tool definitions."""
    out = {}
    for t in tools or []:
        fn = t.get("function", t)
        if fn.get("name"):
            out[fn["name"]] = fn.get("parameters") or fn.get("input_schema") or {}
    return out


def find(names: dict, suffix: str) -> str | None:
    """The tool whose name ends with `suffix`, preferring navigera's server."""
    hits = [n for n in names if n.endswith(suffix)]
    hits.sort(key=lambda n: (0 if "navigera" in n else 1, len(n)))
    return hits[0] if hits else None


def shell_tool(names: dict) -> tuple[str, dict] | None:
    for n in names:
        if n.split("__")[-1].lower() in SHELL_TOOLS:
            return n, names[n]
    return None


def shell_args(schema: dict, command: str) -> dict:
    """Arguments for a shell tool, whatever it calls its command field."""
    props = (schema or {}).get("properties", {})
    if "commands" in props:
        kind = props["commands"].get("type")
        return {"commands": [command] if kind == "array" else command}
    for key in ("command", "cmd", "script", "input"):
        if key in props:
            args = {key: command}
            for req in (schema or {}).get("required", []):
                if req not in args and props.get(req, {}).get("type") == "string":
                    args[req] = "look up the product"  # e.g. a required `description`
            return args
    return {"command": command}


def next_step(body: dict) -> dict:
    """Either {"tool": name, "args": {...}} or {"text": "..."}."""
    messages = body.get("messages", [])
    names = tool_names(body.get("tools"))
    if not names:
        return {"text": "Product lookup"}
    prompt = "\n".join(text_of(m.get("content")) for m in messages if m.get("role") in ("user", "system"))
    results = [text_of(m.get("content")) for m in messages if m.get("role") == "tool"]
    calls = sum(len(m.get("tool_calls") or []) for m in messages if m.get("role") == "assistant")
    base = re.search(r"https?://127\.0\.0\.1:\d+", prompt)
    base = base.group(0) if base else "http://127.0.0.1:8765"
    url = f"{base}/shop/p/103"
    seen = "\n".join(results)
    price, stock = re.search(r"\$(\d+\.\d\d)", seen), re.search(r"[Ii]n stock:?\s*(\d+)", seen)
    if price and stock:
        return {"text": f"The product page shows it.\nFINAL ANSWER: price=${price.group(1)} stock={stock.group(1)}"}
    if calls >= 6:
        return {"text": "FINAL ANSWER: not found (the tools returned no price)"}
    navigate, snapshot = find(names, "browser_navigate"), find(names, "browser_snapshot")
    wait = find(names, "browser_wait_for")
    if navigate and snapshot:
        if calls == 0:
            return {"tool": navigate, "args": {"url": url}}
        # A page still showing its placeholder: wait for the data, as a
        # careful model would, instead of reading it again at once.
        if results and "Loading" in results[-1] and wait:
            return {"tool": wait, "args": {"text": "In stock"}}
        return {"tool": snapshot, "args": {}}
    shell = shell_tool(names)
    if shell:
        name, schema = shell
        nav = "navigera -s mock"
        command = (f"{nav} start && {nav} goto {url} && {nav} wait --selector '#stock' && "
                   f"{nav} --raw ax --selector '#detail'")
        return {"tool": name, "args": shell_args(schema, command)}
    return {"text": "FINAL ANSWER: no browser tool was offered"}


class Handler(BaseHTTPRequestHandler):
    log_path = None

    def log_message(self, *_):
        pass

    def reply(self, status: int, payload: dict):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            return self.reply(200, {"object": "list", "data": [{"id": "mock", "object": "model", "owned_by": "mock"}]})
        self.reply(404, {"error": {"message": "not found"}})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0)) or 0) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.reply(404, {"error": {"message": f"unsupported path {self.path}"}})
        step = next_step(body)
        if self.log_path:
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps({"tools": sorted(tool_names(body.get("tools"))), "messages": len(body.get("messages", [])),
                                    "step": step}) + "\n")
        created, model = int(time.time()), body.get("model", "mock")
        usage = {"prompt_tokens": len(json.dumps(body)) // 4, "completion_tokens": 20,
                 "total_tokens": len(json.dumps(body)) // 4 + 20}
        if "tool" in step:
            call = {"id": f"call_{created}_{len(body.get('messages', []))}", "type": "function",
                    "function": {"name": step["tool"], "arguments": json.dumps(step["args"])}}
            message, finish = {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
        else:
            message, finish = {"role": "assistant", "content": step["text"]}, "stop"
        if not body.get("stream"):
            return self.reply(200, {"id": "mock", "object": "chat.completion", "created": created, "model": model,
                                    "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def chunk(delta, finish_reason=None, with_usage=False):
            payload = {"id": "mock", "object": "chat.completion.chunk", "created": created, "model": model,
                       "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]}
            if with_usage:
                payload["usage"] = usage
            self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())

        if "tool_calls" in message:
            call = dict(message["tool_calls"][0], index=0)
            chunk({"role": "assistant", "content": None, "tool_calls": [call]})
        else:
            chunk({"role": "assistant", "content": message["content"]})
        chunk({}, finish, with_usage=True)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8790)
    ap.add_argument("--log")
    args = ap.parse_args()
    Handler.log_path = args.log
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"mock model on http://127.0.0.1:{args.port}/v1", file=sys.stderr, flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
