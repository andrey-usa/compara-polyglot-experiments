#!/usr/bin/env python3
"""Same-runner A/B of compara builds on one keyed comparison of local files.

    bench_ab.py --build base=PATH --build head=PATH ... --a A --b B \
        [--threads 1,4] [--repeats 7] [--label TEXT]

Runs every build once per round, rounds interleaved (so a noisy moment hits
every build alike), at each thread count; prints a markdown table of the
median and best wall time per build and the ratio of each median to the
first build's. Counts must agree across builds or the run fails.
"""
import argparse
import os
import statistics
import subprocess
import sys
import time

KEY = ["-k", "account_id,txn_id", "-i", "updated_at"]


def run(binary, a, b, threads):
    t0 = time.perf_counter()
    p = subprocess.run([binary, "compare", a, b, *KEY, "--threads", str(threads)],
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    wall = time.perf_counter() - t0
    if p.returncode not in (0, 1):
        sys.exit(f"{binary} failed ({p.returncode}): {p.stderr[-1500:]}")
    counts = p.stdout.strip().splitlines()[-1].split("|")[:-1]  # drop the engine/time cell
    return wall, "|".join(c.strip() for c in counts)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="append", required=True, help="name=path")
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--threads", default="1,4")
    ap.add_argument("--repeats", type=int, default=7)
    ap.add_argument("--label", default="")
    ap.add_argument("--key", default="account_id,txn_id")
    args = ap.parse_args()
    KEY[1] = args.key
    builds = [tuple(b.split("=", 1)) for b in args.build]
    print(f"\n#### {args.label} (median of {args.repeats}, interleaved; best in brackets)\n")
    head = "| threads | " + " | ".join(n for n, _ in builds) + " | " + " | ".join(f"{n} / {builds[0][0]}" for n, _ in builds[1:]) + " |"
    print(head)
    print("|" + "---:|" * (2 * len(builds)))
    for t in [int(x) for x in args.threads.split(",")]:
        times = {n: [] for n, _ in builds}
        ref = None
        for path in [p for _, p in builds]:
            run(path, args.a, args.b, t)  # warm the page cache and the binary
        for _ in range(args.repeats):
            for n, path in builds:
                wall, counts = run(path, args.a, args.b, t)
                if ref is None:
                    ref = counts
                elif counts != ref:
                    sys.exit(f"{n} disagrees: {counts} vs {ref}")
                times[n].append(wall)
        med = {n: statistics.median(v) for n, v in times.items()}
        cells = [f"{med[n]:.3f} s [{min(times[n]):.3f}]" for n, _ in builds]
        ratios = [f"{med[n] / med[builds[0][0]]:.3f}x" for n, _ in builds[1:]]
        print(f"| {t} | " + " | ".join(cells) + " | " + " | ".join(ratios) + " |", flush=True)


if __name__ == "__main__":
    main()
