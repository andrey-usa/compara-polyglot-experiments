#!/usr/bin/env python3
"""Two builds of compara against DuckDB and Polars, on one runner, in one sitting.

For each pair and thread count every tool runs back to back -- compara's base
build, its head build, DuckDB and Polars through scripts/contenders.py -- once
for the counts and once writing the rows, so a cell compares builds and tools,
never machines. The counts are gated: all four must agree, or the run fails.

The medians go to the step summary, to results/ladder.tsv, and to one
annotation per thread count (annotations are what the API serves back without
a log download).

usage: python3 scripts/ladder_ab.py --builds base=PATH,head=PATH \
           --pairs NAME=A,B[|KEY[|IGNORE]][;...] --threads 1,2,4 --out results

KEY defaults to id,k2 (the size-ladder shape) and IGNORE to nothing.
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

KEY = "id,k2"
HERE = Path(__file__).resolve().parent
CONTENDERS = HERE / "contenders.py"


def run(cmd, env=None):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p, time.perf_counter() - t0


def compara_counts(out):
    m = re.search(r"matched ([\d,]+) \(changed ([\d,]+)\) \| added ([\d,]+) \| removed ([\d,]+)", out)
    if not m:
        raise SystemExit(f"unparsed compara output: {out[:300]}")
    _, changed, added, removed = (int(x.replace(",", "")) for x in m.groups())
    return (added, removed, changed)


def rounds_for(path):
    size = os.path.getsize(path)
    return (3, True) if size > 20_000_000 else (5, False)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--builds", required=True)
    ap.add_argument("--pairs", required=True)
    ap.add_argument("--threads", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--contenders", default="duckdb,polars")
    args = ap.parse_args()

    builds = dict(b.split("=", 1) for b in args.builds.split(","))
    pairs = []
    for spec in args.pairs.split(";"):
        name, rest = spec.split("=", 1)
        files, key, ignore = (rest.split("|") + [KEY, ""])[:3]
        a, b = files.split(",")
        pairs.append((name, a, b, key or KEY, ignore))
    threads = [int(t) for t in args.threads.split(",")]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    p, _ = run([sys.executable, "-c",
                "import polars, duckdb; print(f'polars {polars.__version__}, duckdb {duckdb.__version__}')"])
    contenders = p.stdout.strip()
    tools = list(builds) + [c for c in args.contenders.split(",") if c]
    med = {}
    for t in threads:
        for name, a, b, key, ignore in pairs:
            nrounds, warmup = rounds_for(a)
            gate = {}
            for tool in tools:
                for mode in ("counts", "rows"):
                    work = out / name / str(t) / tool / mode
                    work.mkdir(parents=True, exist_ok=True)
                    if tool in builds:
                        cmd = [builds[tool], "compare", a, b, "-k", key, "--threads", str(t)]
                        if ignore:
                            cmd += ["-i", ignore]
                        cmd += ["--summary"] if mode == "counts" else ["--export-dir", str(work)]
                    else:
                        cmd = [sys.executable, str(CONTENDERS), tool, a, b, "--key", key,
                               "--ignore", ignore, "--json", str(work / "counts.json"),
                               "--threads", str(t)]
                        if mode == "rows":
                            cmd += ["--rows-out", str(work)]
                    env = dict(os.environ, POLARS_MAX_THREADS=str(t))
                    if warmup:
                        run(cmd, env)
                    secs = []
                    for _ in range(nrounds):
                        p, dt = run(cmd, env)
                        if p.returncode not in (0, 1):
                            raise SystemExit(f"{tool} {mode} failed on {name}: {p.stderr[-800:]}")
                        secs.append(dt)
                    med[(t, name, tool, mode)] = statistics.median(secs)
                    if mode == "counts":
                        if tool in builds:
                            gate[tool] = compara_counts(p.stdout)
                        else:
                            c = json.loads((work / "counts.json").read_text())
                            gate[tool] = (c["added"], c["removed"], c["changed"])
                    print(f"T={t} {name:22s} {tool:8s} {mode:6s} {med[(t, name, tool, mode)]:7.3f}s "
                          f"{[round(s, 3) for s in secs]}", flush=True)
            if len(set(gate.values())) != 1:
                raise SystemExit(f"COUNT MISMATCH {name} T={t}: {gate}")

    with (out / "ladder.tsv").open("w") as f:
        f.write("threads\tpair\ttool\tmode\tmedian_s\n")
        for (t, name, tool, mode), m in sorted(med.items()):
            f.write(f"{t}\t{name}\t{tool}\t{mode}\t{m:.3f}\n")

    head = " | ".join(tools)
    lines = [f"# compara {', '.join(builds)} vs {contenders}", "",
             "Medians, seconds; each cell is counts / rows written.", ""]
    for t in threads:
        lines += [f"## {t} thread(s)", "", f"| pair | {head} |", "|---" * (len(tools) + 1) + "|"]
        note = [f"T={t} counts/rows s: {head}"]
        for name, *_ in pairs:
            cells = [f"{med[(t, name, tool, 'counts')]:.3f} / {med[(t, name, tool, 'rows')]:.3f}"
                     for tool in tools]
            lines.append(f"| {name} | " + " | ".join(cells) + " |")
            note.append(f"{name}: " + " | ".join(cells))
        lines.append("")
        print(f"::notice title=ladder T={t}::" + "%0A".join(note), flush=True)
    table = "\n".join(lines) + "\n"
    print(table)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(table)


if __name__ == "__main__":
    main()
