#!/usr/bin/env python3
"""Generate CLI reference pages from --help output (T-0083).

Usage: python3 gen-cli-reference.py --bin <path> --out <page.md> --title <t>
Runs <bin> --help and --version, then --help for every subcommand it can
discover from the top-level help (clap "Commands:" lists and navigera's
`help <command>` form). Output is a Starlight page of verbatim help text;
nothing is hand-edited, so the page always matches the binary it was
generated from. A header line records the binary's --version and the
source ref the generator was run against.
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path


def run(args, timeout=60):
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    except Exception as e:
        return None
    out = (r.stdout or "") + (r.stderr or "")
    return out.strip() if r.returncode == 0 and out.strip() else None


def discover_subcommands(top_help, binary):
    """Candidate subcommand names from top-level help text."""
    cands = []
    in_commands = False
    for line in top_help.splitlines():
        if line.strip().startswith("Commands:"):
            in_commands = True
            continue
        if in_commands:
            if not line.startswith(" ") and line.strip():
                break
            m = re.match(r"^\s+([a-z][a-z0-9_-]+)\b", line)
            if m and m.group(1) not in ("help",):
                cands.append(m.group(1))
    # navigera-style: lines like "  goto <url>" in a command list
    if not cands:
        for line in top_help.splitlines():
            m = re.match(r"^\s{2,}([a-z][a-z0-9_-]+)(?:\s|$)", line)
            if m and m.group(1) not in ("help", "options", "usage"):
                cands.append(m.group(1))
    seen, out = set(), []
    for c in cands:
        if c not in seen:
            seen.add(c)
            out.append(c)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--title", default="CLI reference")
    ap.add_argument("--source", default="")
    args = ap.parse_args()
    binary = args.bin
    top = run([binary, "--help"]) or run([binary, "help"]) or ""
    version = run([binary, "--version"]) or ""
    if not top:
        print(f"no --help output from {binary}", file=sys.stderr)
        return 1
    parts = [f"---\ntitle: {args.title}\ndescription: {args.title} generated from --help.\n---\n",
             f"# {args.title}\n"]
    if args.source:
        parts.append(f"\nGenerated from `{args.source}`.")
    if version:
        parts.append(f"\nBinary reports: `{version.splitlines()[0]}`.")
    parts.append("\n## Top level\n\n```text\n" + top + "\n```\n")
    for cmd in discover_subcommands(top, binary):
        text = run([binary, cmd, "--help"]) or run([binary, "help", cmd])
        if text and text != top:
            parts.append(f"\n## `{cmd}`\n\n```text\n{text}\n```\n")
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text("\n".join(parts), encoding="utf-8")
    print(f"wrote {args.out} ({len(discover_subcommands(top, binary))} subcommands tried)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
