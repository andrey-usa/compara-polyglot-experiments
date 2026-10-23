#!/usr/bin/env python3
"""Same-runner A/B of navigera builds: a cold one-shot command, and a session
(start, goto, ax, fill, click, click by text, eval, quit) against the bench site, rounds interleaved so a
noisy moment hits every build alike. Prints a markdown table of medians.

    ab.py --build base=PATH --build head=PATH [--build "natural=PATH --natural"] \
        --site URL [--rounds 15]
"""
import argparse
import os
import shlex
import statistics
import subprocess
import sys
import tempfile
import time


def timed(argv):
    t0 = time.perf_counter()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL)
    dt = time.perf_counter() - t0
    if p.returncode != 0:
        sys.exit(f"{' '.join(argv)} failed ({p.returncode}): {p.stdout[-800:]} {p.stderr[-800:]}")
    return dt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="append", required=True, help="name=path [flags]")
    ap.add_argument("--site", required=True)
    ap.add_argument("--rounds", type=int, default=15)
    args = ap.parse_args()
    builds = []
    for b in args.build:
        name, rest = b.split("=", 1)
        parts = shlex.split(rest)
        builds.append((name, parts[0], parts[1:]))
    steps = ["cold one-shot", "start", "goto", "ax", "fill", "click", "click text", "eval", "quit", "session total"]
    times = {(n, s): [] for n, _, _ in builds for s in steps}
    tmp = tempfile.mkdtemp()
    page = args.site.rstrip("/") + "/shop"
    for r in range(args.rounds + 1):
        for name, exe, flags in builds:
            got = {}
            got["cold one-shot"] = timed([exe, *flags, "--raw", "eval", "1+1"])
            sock = os.path.join(tmp, f"{name}-{r}.sock")
            s = [exe, *flags, "--session", sock]
            got["start"] = timed(s + ["start"])
            got["goto"] = timed(s + ["goto", page])
            got["ax"] = timed(s + ["ax"])
            got["fill"] = timed(s + ["fill", "--selector", "#q", "valve"])
            got["click"] = timed(s + ["click", "--selector", "#cookie-accept"])
            got["click text"] = timed(s + ["click", "--text", "Add to cart"])
            got["eval"] = timed(s + ["eval", "document.title"])
            got["quit"] = timed(s + ["quit"])
            got["session total"] = sum(got[k] for k in ["start", "goto", "ax", "fill", "click", "click text", "eval", "quit"])
            if r > 0:  # round 0 warms caches
                for k, v in got.items():
                    times[(name, k)].append(v)
    names = [n for n, _, _ in builds]
    print(f"\n#### navigera A/B (median of {args.rounds} interleaved rounds, ms)\n")
    print("| step | " + " | ".join(names) + " | " + " | ".join(f"{n} / {names[0]}" for n in names[1:]) + " |")
    print("|---|" + "---:|" * (2 * len(names) - 1))
    for st in steps:
        med = {n: statistics.median(times[(n, st)]) * 1000 for n in names}
        print(f"| {st} | " + " | ".join(f"{med[n]:.0f}" for n in names) + " | "
              + " | ".join(f"{med[n] / med[names[0]]:.2f}x" for n in names[1:]) + " |")


if __name__ == "__main__":
    main()
