#!/usr/bin/env python3
"""Every port's phase timings, on this machine, at more than one thread count.

The ladder says how long each port takes. It cannot say *where*, and a gap that
only shows up on one runner -- the 10M Parquet spread on CI's EPYC 7763, which
this repository could not reproduce on its own container -- needs the where
from that runner. This runs each port with `CSVDIFF_PHASES=1` (`COMPARA_PHASES=1` for Rust), which all four
honour, and prints the median of each phase it reports.

The phases are the ports' own, named as each port names them. They are not
mapped onto one scheme: the four engines do not split the work the same way,
and a table that pretended they did would be comparing different things under
one label. What does line up across ports is the wall column and the rule that
matters most here -- a phase that does not shrink from one thread to four is
the one to look at.

    python3 scripts/phases_ports.py --rows 10m --format parquet
    python3 scripts/phases_ports.py --rows 2m --format ndjson --threads 1,2,4 --rounds 5
"""
import argparse
import os
import pathlib
import re
import statistics
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
PORTS = [
    ("C", ROOT / "c/csvdiff", []),
    ("C++", ROOT / "cpp/build/csvdiff", []),
    # Rust writes a report unless told not to; the others write nothing.
    ("Rust", ROOT / "rust/target/release/compara", ["--summary"]),
    ("Zig", ROOT / "zig/zig-out/bin/csvdiff", []),
]
GEN = ROOT / "rust/target/release/compara-gen-data"
EXT = {"csv": "csv", "tsv": "tsv", "ndjson": "ndjson", "parquet": "parquet"}
KEY = ["-k", "account_id,txn_id", "-i", "updated_at"]
PHASE = re.compile(r"^\s+(.+?)\s+([\d.]+)s$")


def host() -> str:
    model = "unknown CPU"
    flags = ""
    try:
        text = pathlib.Path("/proc/cpuinfo").read_text()
        m = re.search(r"^model name\s*:\s*(.+)$", text, re.M)
        model = m.group(1).strip() if m else model
        f = re.search(r"^flags\s*:\s*(.+)$", text, re.M)
        have = set(f.group(1).split()) if f else set()
        flags = " ".join(v for v in ("avx2", "avx512f", "avx512bw") if v in have)
    except OSError:
        pass
    return f"{model} | {os.cpu_count()}c | {flags or 'no avx2'}"


def run(exe: pathlib.Path, extra: list[str], a: pathlib.Path, b: pathlib.Path,
        threads: int) -> tuple[float, float, dict[str, float]]:
    env = dict(os.environ, CSVDIFF_PHASES="1", COMPARA_PHASES="1")
    before = os.times()
    t0 = time.perf_counter()
    r = subprocess.run([str(exe), "compare", str(a), str(b), *KEY, *extra,
                        "--threads", str(threads)],
                       capture_output=True, text=True, env=env)
    wall = time.perf_counter() - t0
    after = os.times()
    cpu = (after.children_user - before.children_user) + \
          (after.children_system - before.children_system)
    # 0 is no differences and 1 is differences found; anything else is a failure
    # and a timing of a failure is not a timing.
    if r.returncode not in (0, 1):
        sys.exit(f"{exe} exited {r.returncode}:\n{r.stderr[-2000:]}")
    phases: dict[str, float] = {}
    for line in r.stderr.splitlines():
        m = PHASE.match(line)
        if m:
            # A label printed twice -- both sides of an index, say -- keeps the
            # larger, since the two run at once and the longer one is the wait.
            name = m.group(1).strip()
            phases[name] = max(phases.get(name, 0.0), float(m.group(2)))
    return wall, cpu, phases


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--rows", default="2m")
    ap.add_argument("--format", default="csv", choices=sorted(EXT))
    ap.add_argument("--threads", default="1,4",
                    help="comma-separated thread counts (default 1,4)")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--data-dir", default="/tmp/phases-data")
    ap.add_argument("--ports", default="C,C++,Rust,Zig")
    args = ap.parse_args()

    threads = [int(t) for t in args.threads.split(",") if t.strip()]
    wanted = {p.strip() for p in args.ports.split(",")}
    ports = [p for p in PORTS if p[0] in wanted]
    for name, exe, _ in ports:
        if not exe.exists():
            sys.exit(f"{name}: {exe} is not built")

    data = pathlib.Path(args.data_dir)
    data.mkdir(parents=True, exist_ok=True)
    a = data / f"p_a.{EXT[args.format]}"
    b = data / f"p_b.{EXT[args.format]}"
    if not (a.exists() and b.exists()):
        subprocess.run([str(GEN), "-n", args.rows, "-o", str(data), "--prefix", "p",
                        "-f", args.format], check=True, stdout=subprocess.DEVNULL)
    # Once, untimed: the first reader of a fresh file pays for the page cache.
    for f in (a, b):
        with open(f, "rb") as fh:
            while fh.read(1 << 24):
                pass

    print(f"host: {host()}")
    print(f"{args.rows} rows, {args.format}, median of {args.rounds} rounds, "
          f"ports interleaved within each round\n")

    # Interleaved: every port once per round, so drift lands on all of them.
    samples: dict[tuple[str, int], list[tuple[float, float, dict[str, float]]]] = {}
    for _ in range(args.rounds):
        for t in threads:
            for name, exe, extra in ports:
                samples.setdefault((name, t), []).append(run(exe, extra, a, b, t))

    for name, _, _ in ports:
        print(f"## {name}")
        labels: list[str] = []
        for t in threads:
            for _, _, ph in samples[(name, t)]:
                for k in ph:
                    if k not in labels:
                        labels.append(k)
        head = f"{'':<32}" + "".join(f"{str(t) + ' thr':>11}" for t in threads)
        if len(threads) > 1:
            head += f"{'scales':>9}"
        print(head)

        def med(t: int, pick) -> float:
            return statistics.median(pick(s) for s in samples[(name, t)])

        rows = [("wall", lambda s: s[0]), ("cpu", lambda s: s[1])]
        rows += [(k, (lambda k: lambda s: s[2].get(k, 0.0))(k)) for k in labels]
        for label, pick in rows:
            vals = [med(t, pick) for t in threads]
            line = f"{label:<32}" + "".join(f"{v:>10.3f}s" for v in vals)
            if len(threads) > 1 and vals[-1] > 0 and label != "cpu":
                line += f"{vals[0] / vals[-1]:>8.2f}x"
            print(line)
        print()


if __name__ == "__main__":
    main()
