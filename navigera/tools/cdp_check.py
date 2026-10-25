#!/usr/bin/env python3
"""Validate navigera's CDP usage against a browser's own protocol schema.

Two sources of truth are compared with the protocol the browser under test
serves at /json/protocol (so the check is exact for that Chrome version):

  1. the runtime trace (NAVIGERA_CDP_TRACE=<file>, written while the e2e tests
     run): every command actually sent, with its parameter names and short
     string values;
  2. a static scan of src/ for "Domain.name" strings (commands and events
     the code refers to, including ones the tests did not exercise).

Errors (exit 1): unknown command/event, deprecated command or parameter,
unknown parameter, missing required parameter, string value outside the
parameter's enum. Notes: experimental commands in use, commands referenced
in the source but never exercised by the tests.

Usage:
  cdp_check.py --protocol protocol.json --trace trace.jsonl [--src src] [--markdown out.md]
  cdp_check.py --protocol http://127.0.0.1:9222/json/protocol ...
"""
import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path


def load_protocol(src: str) -> dict:
    if src.startswith("http://") or src.startswith("https://"):
        with urllib.request.urlopen(src, timeout=10) as r:
            return json.load(r)
    return json.loads(Path(src).read_text())


def index(protocol: dict):
    commands, events, types = {}, {}, {}
    for d in protocol.get("domains", []):
        dom = d["domain"]
        for t in d.get("types", []):
            types[f"{dom}.{t['id']}"] = t
        for c in d.get("commands", []):
            commands[f"{dom}.{c['name']}"] = {**c, "_domain": d}
        for e in d.get("events", []):
            events[f"{dom}.{e['name']}"] = {**e, "_domain": d}
    return commands, events, types


def enum_of(param: dict, domain: str, types: dict):
    if "enum" in param:
        return param["enum"]
    ref = param.get("$ref")
    if ref:
        t = types.get(ref if "." in ref else f"{domain}.{ref}")
        if t and "enum" in t:
            return t["enum"]
    return None


def check_trace(lines, commands, types):
    errors, notes, seen = [], set(), {}
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            rec = json.loads(raw)
            method, params = rec["method"], rec.get("params") or {}
        except (json.JSONDecodeError, KeyError, TypeError):
            notes.add("skipped an unreadable trace line")
            continue
        seen.setdefault(method, 0)
        seen[method] += 1
        cmd = commands.get(method)
        if cmd is None:
            errors.append(f"unknown command `{method}`")
            continue
        domain = cmd["_domain"]["domain"]
        if cmd.get("deprecated"):
            errors.append(f"deprecated command `{method}`")
        if cmd.get("experimental") or cmd["_domain"].get("experimental"):
            notes.add(f"experimental: `{method}`")
        declared = {p["name"]: p for p in cmd.get("parameters", [])}
        for name, value in params.items():
            p = declared.get(name)
            if p is None:
                errors.append(f"`{method}`: unknown parameter `{name}`")
                continue
            if p.get("deprecated"):
                errors.append(f"`{method}`: deprecated parameter `{name}`")
            allowed = enum_of(p, domain, types)
            if allowed and isinstance(value, str) and not value.startswith("<") and value not in allowed:
                errors.append(f"`{method}`: `{name}`={value!r} not in {allowed}")
        for name, p in declared.items():
            if not p.get("optional") and name not in params:
                errors.append(f"`{method}`: missing required parameter `{name}`")
    return sorted(set(errors)), sorted(notes), seen


DOMAIN_REF = re.compile(r'"([A-Z][A-Za-z]+)\.([a-z][A-Za-z]+)"|\b([A-Z][A-Za-z]+)\.([a-z][A-Za-z]+)\b')


def scan_source(src: Path, domains: set):
    found = {}
    for path in sorted(src.rglob("*.rs")):
        for no, line in enumerate(path.read_text().splitlines(), 1):
            for m in DOMAIN_REF.finditer(line):
                dom, name = (m.group(1), m.group(2)) if m.group(1) else (m.group(3), m.group(4))
                if dom in domains:
                    found.setdefault(f"{dom}.{name}", f"{path}:{no}")
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--protocol", required=True)
    ap.add_argument("--trace", nargs="*", default=[])
    ap.add_argument("--src", default="src")
    ap.add_argument("--markdown")
    ap.add_argument("--label", default="")
    args = ap.parse_args()

    protocol = load_protocol(args.protocol)
    commands, events, types = index(protocol)
    domains = {d["domain"] for d in protocol.get("domains", [])}
    version = protocol.get("version", {})

    lines = []
    for t in args.trace:
        p = Path(t)
        if p.exists():
            lines.extend(p.read_text().splitlines())
    errors, notes, seen = check_trace(lines, commands, types)

    referenced = scan_source(Path(args.src), domains)
    for name, where in sorted(referenced.items()):
        if name not in commands and name not in events:
            errors.append(f"`{name}` ({where}) is neither a command nor an event in this protocol")
        elif name in commands and commands[name].get("deprecated"):
            errors.append(f"`{name}` ({where}) is deprecated")
        elif name in events and events[name].get("deprecated"):
            errors.append(f"event `{name}` ({where}) is deprecated")
    unexercised = sorted(n for n in referenced if n in commands and n not in seen)

    title = f"CDP check {args.label}".strip()
    out = [f"### {title}", "",
           f"Protocol {version.get('major', '?')}.{version.get('minor', '?')} from the browser under test; "
           f"{len(seen)} distinct commands traced ({sum(seen.values())} calls), "
           f"{len(referenced)} protocol names referenced in `{args.src}`.", ""]
    if errors:
        out += ["**Errors**", ""] + [f"- {e}" for e in sorted(set(errors))] + [""]
    else:
        out += ["No errors: every command, parameter and enum value is valid for this browser.", ""]
    if notes:
        out += ["Experimental commands in use: " + ", ".join(n.split(": ", 1)[1] for n in notes), ""]
    if unexercised:
        out += ["Referenced in source but not exercised by the tests: " + ", ".join(f"`{n}`" for n in unexercised), ""]
    text = "\n".join(out)
    print(text)
    if args.markdown:
        Path(args.markdown).write_text(text)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
