#!/usr/bin/env python3
"""What an agent's LLM reads to choose one element: the `ax` tree, against
the answer of `navigera find`, and what the decider is sent instead.

    python3 bench/decide/measure.py --navigera target/release/navigera [--real]

Without --real it starts the bench site and measures its pages; with --real
it measures a handful of public pages (needs network). The decider is
`mock_jev.py` unless NAVIGERA_DECIDER_URL or TYPESAFE_API_KEY is set, in which
case the real model chooses and the "chose" column means something.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SITE_TASKS = [
    ("/shop", "add the brass valve to the cart"),
    ("/shop/p/103", "the quantity field"),
    ("/cart", "go back to shopping"),
    ("/login", "the password field"),
    ("/docs", "open the returns FAQ"),
    ("/support", "the file attachment input"),
]
REAL_TASKS = [
    ("https://en.wikipedia.org/wiki/Rust_(programming_language)", "the link to the Cargo package manager article"),
    ("https://github.com/rust-lang/rust", "the Issues tab"),
    ("https://news.ycombinator.com/", "the login link"),
    ("https://docs.python.org/3/library/json.html", "the search box"),
    ("https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/input", "the site search"),
    ("https://www.rust-lang.org/learn", "the link to the Rust book"),
]


def start(script, *extra):
    p = subprocess.Popen([sys.executable, os.path.join(HERE, script), "--port", "0", *extra],
                         stdout=subprocess.PIPE, text=True)
    return p, p.stdout.readline().strip().removeprefix("LISTENING ")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--navigera", required=True)
    ap.add_argument("--real", action="store_true")
    args = ap.parse_args()
    procs, env = [], dict(os.environ)
    log = tempfile.NamedTemporaryFile(suffix=".jsonl", delete=False).name
    if not (env.get("NAVIGERA_DECIDER_URL") or env.get("TYPESAFE_API_KEY")):
        p, url = start("mock_jev.py", "--log", log)
        procs.append(p)
        env["NAVIGERA_DECIDER_URL"] = url
    if args.real:
        base, tasks = "", REAL_TASKS
    else:
        p, base = start("../site/server.py")
        procs.append(p)
        tasks = SITE_TASKS

    def nv(*a):
        out = subprocess.run([args.navigera, "-s", "decide-measure", *a], capture_output=True, text=True, env=env)
        return json.loads(out.stdout)

    rows = []
    try:
        nv("start")
        for path, what in tasks:
            nv("goto", base + path)
            nv("wait", "--ms", "800")
            tree = nv("ax")["result"]
            before = sum(1 for _ in open(log))
            found = nv("find", what)
            sent = sum(len(json.dumps(json.loads(l)["body"])) for l in list(open(log))[before:])
            answer = found.get("result", found)
            rows.append((path.removeprefix("https://")[:48], what, len(tree),
                         len(json.dumps(answer, separators=(",", ":"))), sent,
                         answer.get("element", answer.get("error", "")) if isinstance(answer, dict) else answer))
    finally:
        nv("quit")
        for p in procs:
            p.kill()
    print("| page | looking for | tree | find answer | sent to the decider | chose |")
    print("|---|---|--:|--:|--:|---|")
    for r in rows:
        print(f"| {r[0]} | {r[1]} | {r[2]:,} | {r[3]:,} | {r[4]:,} | {str(r[5])[:60]} |")
    print(f"| **total** | | {sum(r[2] for r in rows):,} | {sum(r[3] for r in rows):,} | {sum(r[4] for r in rows):,} | |")
    print("(bytes; 'sent' counts only requests the mock logged, so it reads 0 with a real decider)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
