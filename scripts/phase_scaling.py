"""Which phase accounts for the shortfall between 1 and 4 threads?

The insert turned out to be 15% of wall, not half, so most of the 38-54% serial
fraction is unexplained. This scales every phase separately, taking the max of
the two concurrent index sides rather than their sum, and checks the critical
path adds up to the wall before believing any of it.
"""
import subprocess
import re
import time
import os
import statistics
import sys

from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent  # repository root

EXE = str(ROOT / "cpp/build/csvdiff")
if len(sys.argv) < 3:
    sys.exit(f"usage: {sys.argv[0]} <a.csv> <b.csv> [rounds]")
A, B = sys.argv[1], sys.argv[2]
K = ["-k", "account_id,txn_id", "-i", "updated_at"]
ROUNDS = int(sys.argv[3]) if len(sys.argv) > 3 else 5
env = dict(os.environ, CSVDIFF_PHASES="1", COMPARA_PHASES="1")

def one(thr):
    t0 = time.perf_counter()
    r = subprocess.run([EXE, "compare", A, B, *K, "--threads", str(thr)],
                       capture_output=True, text=True, env=env)
    wall = time.perf_counter() - t0
    ph = {}
    for line in r.stderr.splitlines():
        m = re.match(r'\s+(.+?)\s+([\d.]+)s$', line)
        if m:
            ph[m.group(1)] = float(m.group(2))
    # The two index sides run concurrently: the critical path is the longer one.
    a = ph.get('A sweep (parallel)', 0) + ph.get('A index insert (serial)', 0)
    b = ph.get('B sweep (parallel)', 0) + ph.get('B index insert (serial)', 0)
    return {
        "wall": wall,
        "index region (max side)": max(a, b),
        "  of which sweep": max(ph.get('A sweep (parallel)', 0), ph.get('B sweep (parallel)', 0)),
        "  of which insert": max(ph.get('A index insert (serial)', 0),
                                 ph.get('B index insert (serial)', 0)),
        "join and compare": ph.get('join and compare', 0),
        "assemble": ph.get('assemble', 0),
    }

res = {t: [one(t) for _ in range(ROUNDS)] for t in (1, 4)}
keys = list(res[1][0].keys())
med = {t: {k: statistics.median(r[k] for r in res[t]) for k in keys} for t in (1, 4)}

print(f"{'phase':<26} {'1 thread':>9} {'4 threads':>10} {'scales':>8} {'% of 4thr wall':>15}")
for k in keys:
    o, n = med[1][k], med[4][k]
    sc = f"{o/n:.2f}x" if n > 0 else "-"
    share = f"{n/med[4]['wall']*100:.0f}%" if k != "wall" else ""
    print(f"{k:<26} {o:>8.3f}s {n:>9.3f}s {sc:>8} {share:>15}")

cp = sum(med[4][k] for k in ("index region (max side)", "join and compare", "assemble"))
print(f"\ncritical path sums to {cp:.3f}s against a {med[4]['wall']:.3f}s wall "
      f"({cp/med[4]['wall']*100:.0f}%) -- the rest is startup, mmap and teardown.")
print("If it exceeded the wall, something in it still overlaps.")
