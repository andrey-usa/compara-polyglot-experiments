#!/usr/bin/env python3
"""What each port's `--json` document contains, and what it costs to produce.

This replaces `json_sample_cost.py`, which timed `--json` against no `--json`
and left the reader to infer why the numbers differed. They differ because the
ports do not emit the same document, and timing alone cannot say that -- an
earlier entry read "C 3%, C++ 36%, Rust 0%, Zig 1%" as those ports either
skipping the samples or building them cheaply, when in fact three of the four
have no row-sample feature at all.

So this reports the shape first and the cost second. The shape is the reason.

    python3 scripts/report_cost.py A.csv B.csv --key id [--threads 4]
"""
from __future__ import annotations

import argparse
import collections
import json
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# `-o /dev/null` for Rust for the same reason the benchmark harnesses pass it:
# the Rust port renders an HTML report the others do not, and charging that to
# it would be the very asymmetry this script exists to expose.
PORTS = [
    ("C", ROOT / "c/csvdiff", []),
    ("C++", ROOT / "cpp/build/csvdiff", []),
    ("Rust", ROOT / "rust/target/release/compara", ["-o", "/dev/null"]),
    ("Zig", ROOT / "zig/zig-out/bin/csvdiff", []),
]

# Sections that name individual rows, as opposed to counts and column stats.
SAMPLE_KEYS = ("changed", "added", "removed", "dup_a", "dup_b")


def rows_in(section) -> int:
    if isinstance(section, list):
        return len(section)
    if isinstance(section, dict):
        for k in ("rows", "items"):
            if isinstance(section.get(k), list):
                return len(section[k])
    return 0


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--key", required=True)
    ap.add_argument("--ignore", default=None)
    ap.add_argument("--threads", default=None)
    ap.add_argument("--repeats", type=int, default=5)
    args = ap.parse_args(argv)

    common = ["-k", args.key]
    if args.ignore:
        common += ["-i", args.ignore]
    if args.threads:
        common += ["--threads", args.threads]

    built = [(n, p, f) for n, p, f in PORTS if p.exists()]
    if not built:
        print("no port is built", file=sys.stderr)
        return 1

    shape: dict[str, tuple[list[str], int, int]] = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name, path, flags in built:
            out = Path(tmp) / f"{name}.json"
            subprocess.run([str(path), "compare", args.a, args.b, *common, *flags,
                            "--json", str(out)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                doc = json.loads(out.read_text())
            except (OSError, json.JSONDecodeError):
                shape[name] = ([], 0, 0)
                continue
            sampled = sum(rows_in(doc.get(k)) for k in SAMPLE_KEYS)
            shape[name] = (sorted(doc), sampled, out.stat().st_size)

    print("## What each port's --json actually contains\n")
    print("| Port | top-level keys | rows named | bytes |")
    print("| --- | --- | ---: | ---: |")
    for name, _, _ in built:
        keys, sampled, size = shape[name]
        print(f"| {name} | {', '.join(keys) or '(none)'} | {sampled:,} | {size:,} |")

    # Only now the cost, so it is read as a consequence of the shape above.
    wall: dict[tuple[str, bool], list[float]] = collections.defaultdict(list)
    for rnd in range(args.repeats):
        order = built[rnd % len(built):] + built[:rnd % len(built)]
        for js in (False, True):
            for name, path, flags in order:
                argv_run = [str(path), "compare", args.a, args.b, *common, *flags]
                if js:
                    argv_run += ["--json", "/dev/null"]
                started = time.perf_counter()
                subprocess.run(argv_run, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
                wall[(name, js)].append(time.perf_counter() - started)

    print(f"\n## What producing it costs ({args.repeats} interleaved rounds, medians)\n")
    print("| Port | counts only | with --json | the report costs |")
    print("| --- | ---: | ---: | ---: |")
    for name, _, _ in built:
        off = statistics.median(wall[(name, False)])
        on = statistics.median(wall[(name, True)])
        print(f"| {name} | {off:.2f}s | {on:.2f}s | {100 * (on - off) / off:+.0f}% |")

    print("\nA port that names no rows has nothing to charge for. Compare the two "
          "tables before reading the second one as an efficiency result.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
