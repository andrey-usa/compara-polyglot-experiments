"""What the index insert actually costs on the critical path.

The two indexes are built on concurrent threads ("The two indexes share nothing,
so they are built at the same time"), so the A and B insert phases OVERLAP. The
scaling entry summed them, which overstates the insert's share of wall roughly
two to one. This takes the max of the two sides instead, which is what the
critical path sees, and checks the arithmetic by comparing the sum of all phases
against the measured wall.
"""
import subprocess
import re
import time
import os
import statistics
import sys

from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent  # repository root

PORTS = {"C":    (str(ROOT / "c/csvdiff"), []),
         "C++":  (str(ROOT / "cpp/build/csvdiff"), []),
         # `--summary`, not `-o /dev/null`: see `bench_formats_ports.py`.
         "Rust": (str(ROOT / "rust/target/release/compara"), ["--summary"]),
         "Zig":  (str(ROOT / "zig/zig-out/bin/csvdiff"), [])}
if len(sys.argv) < 3:
    sys.exit(f"usage: {sys.argv[0]} <a.csv> <b.csv> [rounds]")
A, B = sys.argv[1], sys.argv[2]
K = ["-k", "account_id,txn_id", "-i", "updated_at"]
ROUNDS = int(sys.argv[3]) if len(sys.argv) > 3 else 5
env = dict(os.environ, CSVDIFF_PHASES="1", COMPARA_PHASES="1")
# Each port names its insert phase slightly differently.
INSERT = re.compile(r'(index insert \(serial\)|insert in order)')

def one(exe, extra, thr):
    t0 = time.perf_counter()
    r = subprocess.run([exe, "compare", A, B, *K, "--threads", str(thr), *extra],
                       capture_output=True, text=True, env=env)
    wall = time.perf_counter() - t0
    total, inserts = 0.0, []
    for line in r.stderr.splitlines():
        m = re.match(r'\s+(.+?)\s+([\d.]+)s$', line)
        if not m:
            continue
        total += float(m.group(2))
        if INSERT.search(m.group(1)):
            inserts.append(float(m.group(2)))
    return wall, total, inserts

print(f"{'port':<5} {'wall':>7} {'phase sum':>10} {'insert sum':>11} {'insert max':>11}"
      f" {'sum/wall':>9} {'max/wall':>9}")
for name, (exe, extra) in PORTS.items():
    rows = [one(exe, extra, 4) for _ in range(ROUNDS)]
    w  = statistics.median(r[0] for r in rows)
    t  = statistics.median(r[1] for r in rows)
    s  = statistics.median(sum(r[2]) for r in rows)
    mx = statistics.median(max(r[2]) if r[2] else 0 for r in rows)
    print(f"{name:<5} {w:>6.3f}s {t:>9.3f}s {s:>10.3f}s {mx:>10.3f}s"
          f" {s/w*100:>8.0f}% {mx/w*100:>8.0f}%")
print("\nsum/wall is what the scaling entry published; max/wall is the critical path.")
