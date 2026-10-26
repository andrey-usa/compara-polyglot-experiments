#!/usr/bin/env python3
"""T-0069 preflight gate: assert the seeded-solution self-test produced the
expected scores before any paid request runs. Zero model spend.

Usage: preflight-check.py --arm primavera|playwright --out <outdir>
Exit 0 = toolchain + restricted-user execution verified; nonzero blocks paid jobs.
"""
import argparse, glob, json, os, sys

ap = argparse.ArgumentParser()
ap.add_argument("--arm", required=True, choices=["primavera", "playwright"])
ap.add_argument("--out", required=True)
a = ap.parse_args()

expected = {"R1": 1, "R2": 1, "R3": 1, "R4": 1}
if a.arm == "primavera":
    expected["W3"] = 1  # seeded cross-task write-task solution (pv arm only)

recs = []
for p in glob.glob(os.path.join(a.out, "**", "t0069-episodes-*.jsonl"), recursive=True):
    with open(p) as f:
        for line in f:
            line = line.strip()
            if line:
                recs.append(json.loads(line))

seen, bad = {}, []
for r in recs:
    t = r.get("task")
    if t in expected:
        seen.setdefault(t, 0)
        seen[t] += 1
        if r.get("score") != expected[t] or r.get("error"):
            bad.append(r)

missing = sorted(set(expected) - set(seen))
if missing or bad:
    print(f"PREFLIGHT-FAIL arm={a.arm} missing_tasks={missing} "
          f"bad_trials={[(r['task'], r['trial'], r.get('score'), r.get('error')) for r in bad]}")
    sys.exit(1)
counts = {t: seen[t] for t in sorted(seen)}
print(f"PREFLIGHT-OK arm={a.arm} trials={counts} (restricted-user execution verified)")
