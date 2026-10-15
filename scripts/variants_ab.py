#!/usr/bin/env python3
"""Several builds of one engine, interleaved on one runner.

`bench_ab.sh` answers "did this change pay" on the machine you are sitting at.
On CI the same question kept costing a second round of runs: a branch run and a
main run are two runners, and two runners are as often two different CPUs --
three 10M runs out of four did not pair when clang 23 was measured. This runs
every build named on the command line on *one* runner, once per round at each
thread count, so the CPU is shared by construction and drift lands on all of
them. Each build is reported against the first build of its group (the text
before the first space in its label).

Name the base twice to see the floor: two copies of one binary, interleaved,
are as far apart as noise alone puts them, and a head that is not further from
the base than that has not been told apart from it.

    python3 scripts/variants_ab.py --format ndjson --rows 10m \\
        "cpp base=/tmp/base/cpp/build/csvdiff" "cpp head=cpp/build/csvdiff" \\
        "cpp base-again=/tmp/base-again"

`.github/workflows/ab.yml` builds two refs and calls this.
"""
import argparse
import hashlib
import os
import pathlib
import re
import statistics
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from phases_ports import GEN, KEY, host  # noqa: E402

# A port that prints its own elapsed time -- the C++ summary line ends in one --
# would otherwise hash differently on every run.
TIMING = re.compile(rb"\b\d+(?:\.\d+)?m?s\b")


def run(exe: str, extra: list[str], a: pathlib.Path, b: pathlib.Path,
        threads: int) -> tuple[float, float, str]:
    before = os.times()
    t0 = time.perf_counter()
    r = subprocess.run([exe, "compare", str(a), str(b), *KEY, *extra,
                        "--threads", str(threads)], capture_output=True)
    wall = time.perf_counter() - t0
    after = os.times()
    cpu = (after.children_user - before.children_user) + \
          (after.children_system - before.children_system)
    # 0 is no differences and 1 is differences found; anything else is a failure.
    if r.returncode not in (0, 1):
        sys.exit(f"{exe} exited {r.returncode}:\n{r.stderr[-2000:].decode(errors='replace')}")
    return wall, cpu, hashlib.sha256(TIMING.sub(b"", r.stdout)).hexdigest()[:12]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--rows", default="10m")
    ap.add_argument("--format", default="csv")
    ap.add_argument("--threads", default="1,4")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--data-dir", default="/tmp/variants-data")
    ap.add_argument("--extra", action="append", default=[],
                    help="an argument passed to every build, e.g. --extra=--summary")
    ap.add_argument("builds", nargs="+", help="label=path")
    args = ap.parse_args()
    builds = [tuple(s.split("=", 1)) for s in args.builds]
    threads = [int(t) for t in args.threads.split(",") if t.strip()]

    data = pathlib.Path(args.data_dir)
    data.mkdir(parents=True, exist_ok=True)
    a, b = data / f"p_a.{args.format}", data / f"p_b.{args.format}"
    if not (a.exists() and b.exists()):
        subprocess.run([str(GEN), "-n", args.rows, "-o", str(data), "--prefix", "p",
                        "-f", args.format], check=True, stdout=subprocess.DEVNULL)
    # Once, untimed: the first reader of a fresh file pays for the page cache.
    for f in (a, b):
        with open(f, "rb") as fh:
            while fh.read(1 << 24):
                pass

    print(f"host: {host()}")
    print(f"{args.rows} rows, {args.format}, median of {args.rounds} rounds, interleaved\n")
    samples: dict[tuple[str, int], list[tuple[float, float]]] = {}
    digests: dict[str, set[str]] = {}
    for _ in range(args.rounds):
        for t in threads:
            for label, exe in builds:
                w, c, d = run(exe, args.extra, a, b, t)
                samples.setdefault((label, t), []).append((w, c))
                digests.setdefault(label.split(" ")[0], set()).add(d)

    print(f"{'build':<30}" + "".join(f"{f'{t} thr wall':>13}{'vs base':>9}{'cpu':>9}"
                                     for t in threads))
    base: dict[str, str] = {}
    for label, _ in builds:
        g = label.split(" ")[0]
        base.setdefault(g, label)
        row = f"{label:<30}"
        for t in threads:
            w = statistics.median(s[0] for s in samples[(label, t)])
            w0 = statistics.median(s[0] for s in samples[(base[g], t)])
            c = statistics.median(s[1] for s in samples[(label, t)])
            row += f"{w:>12.3f}s{w / w0:>8.3f}x{c:>8.2f}s"
        print(row)
    print()
    for g, ds in digests.items():
        print(f"output {g}: {'identical' if len(ds) == 1 else 'DIFFERS ' + ' '.join(sorted(ds))}")


if __name__ == "__main__":
    main()
