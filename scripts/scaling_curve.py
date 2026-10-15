"""How do the ports scale from one core to four, and what is the serial fraction?

Both C and C++ measured ~1.7x from 1 to 4 threads while using ~3 cores of CPU.
That is a bigger prize than anything else found in the C++ gap work, and it is
not language-specific. This sweeps every thread count and fits Amdahl.
"""
import subprocess
import time
import resource
import statistics
import sys
import collections

from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent  # repository root

PORTS = {"C":    str(ROOT / "c/csvdiff"),
         "C++":  str(ROOT / "cpp/build/csvdiff"),
         "Rust": str(ROOT / "rust/target/release/compara"),
         "Zig":  str(ROOT / "zig/zig-out/bin/csvdiff")}
# `--summary`, not `-o /dev/null`: see `bench_formats_ports.py`.
EXTRA = {"Rust": ["--summary"]}
if len(sys.argv) < 3:
    sys.exit(f"usage: {sys.argv[0]} <a.csv> <b.csv> [rounds]")
A, B = sys.argv[1], sys.argv[2]
BASE = ["compare", A, B, "-k", "account_id,txn_id", "-i", "updated_at"]
THREADS = [1, 2, 3, 4]
ROUNDS = int(sys.argv[3]) if len(sys.argv) > 3 else 5

def run(exe, port, t):
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.perf_counter()
    subprocess.run([exe, *BASE, "--threads", str(t), *EXTRA.get(port, [])],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t1 = time.perf_counter()
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    return t1 - t0, (r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime)

res = collections.defaultdict(list)
for r in range(ROUNDS):
    for t in THREADS:
        items = list(PORTS.items())
        items = items[r % len(items):] + items[:r % len(items)]   # rotate
        for name, exe in items:
            res[(name, t)].append(run(exe, name, t))

print(f"{'port':<5} " + " ".join(f"{t}thr".rjust(14) for t in THREADS) + "   speedup  serial%")
for name in PORTS:
    cells, w1 = [], None
    for t in THREADS:
        w = statistics.median(v[0] for v in res[(name, t)])
        c = statistics.median(v[1] for v in res[(name, t)])
        if t == 1:
            w1 = w
        cells.append(f"{w:6.2f}s/{c/w:4.2f}x")
    w4 = statistics.median(v[0] for v in res[(name, 4)])
    sp = w1 / w4
    n = 4
    # Amdahl: speedup = 1 / (s + (1-s)/n)  ->  s = (n/sp - 1) / (n - 1)
    s = (n / sp - 1) / (n - 1)
    print(f"{name:<5} " + " ".join(c.rjust(14) for c in cells) + f"   {sp:5.2f}x   {s*100:5.1f}%")
print("\ncell = wall / cores-busy;  serial% is the Amdahl fraction implied by the 1->4 speedup")
