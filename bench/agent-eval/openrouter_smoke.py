#!/usr/bin/env python3
"""Smoke check: can the OpenRouter models drive the eval's tool surface?

Sends one forced run_shell_command tool call to each candidate model and
verifies the reply is a well-formed tool call with JSON arguments.
Prints one JSON line per model; exits 1 if any model fails.
"""
import json, os, sys, urllib.request, urllib.error

API = "https://openrouter.ai/api/v1/chat/completions"
MODELS = ["inception/mercury-3", "deepseek/deepseek-v4.1-flash"]

TOOLS = [{
    "type": "function",
    "function": {
        "name": "run_shell_command",
        "description": "Run a shell command and return its stdout.",
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string", "description": "The command to run"}},
            "required": ["command"],
        },
    },
}]


def call(model, key):
    body = {
        "model": model,
        "messages": [{"role": "user", "content":
            "You are a browser automation agent. Use the run_shell_command tool to run this command: "
            "navigera open https://example.com . Respond with exactly one tool call and no other text."}],
        "tools": TOOLS,
        "tool_choice": {"type": "function", "function": {"name": "run_shell_command"}},
        "max_tokens": 300,
        "usage": {"include": True},
    }
    req = urllib.request.Request(API, data=json.dumps(body).encode(), method="POST")
    req.add_header("Content-Type", "application/json")
    req.add_header("Authorization", f"Bearer {key}")
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}


def main():
    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        print("OPENROUTER_API_KEY is not set")
        return 1
    failed = False
    for model in MODELS:
        status, r = call(model, key)
        out = {"model": model, "http": status}
        try:
            ch = r["choices"][0]
            fn = (ch["message"].get("tool_calls") or [{}])[0].get("function", {})
            cmd = None
            try:
                cmd = json.loads(fn.get("arguments", "")).get("command")
            except Exception:
                pass
            out.update(finish_reason=ch.get("finish_reason"), tool=fn.get("name"),
                       command=cmd, provider=r.get("provider"), usage=r.get("usage"))
            ok = fn.get("name") == "run_shell_command" and isinstance(cmd, str) and "navigera" in cmd
        except Exception:
            out["error"] = json.dumps(r)[:300]
            ok = False
        out["tool_call_ok"] = ok
        failed |= not ok
        print(json.dumps(out))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
