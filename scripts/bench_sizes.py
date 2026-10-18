#!/usr/bin/env python3
"""Size ladder: compara vs Polars vs DuckDB, summary and full diffs.

One job, one host, one sitting: every cell runs all three tools back to back,
so a cell compares engines, never machines. Contenders run through the
repository's own scripts/contenders.py; compara runs as the just-built binary.

Counts are gated per cell: added/removed/changed must agree across the three
tools, or the run fails.

usage: python3 scripts/bench_sizes.py --sizes 500,5000 --formats csv,parquet \\
           --threads 4 --compara rust/target/release/compara \\
           --data data --out results
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


def run(cmd):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, text=True)
    return p, time.perf_counter() - t0


def parse_compara_summary(out):
    m = re.search(r"matched ([\d,]+) \(changed ([\d,]+)\) \| added ([\d,]+) \| removed ([\d,]+)", out)
    if not m:
        raise AssertionError(f"unparsed compara output: {out[:300]}")
    matched, changed, added, removed = (int(x.replace(",", "")) for x in m.groups())
    return {"added": added, "removed": removed, "changed": changed}


def rounds_for(n):
    if n <= 50_000:
        return 5, False
    if n <= 500_000:
        return 3, False
    return 3, True  # one warmup round, discarded, then three timed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", required=True)
    ap.add_argument("--formats", required=True)
    ap.add_argument("--threads", required=True)
    ap.add_argument("--compara", required=True)
    ap.add_argument("--data", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    sizes = [s.strip() for s in args.sizes.split(",") if s.strip()]
    fmts = [f.strip() for f in args.formats.split(",") if f.strip()]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    tsv = out / "results.tsv"

    versions = {}
    p, _ = run([args.compara, "--version"])
    versions["compara"] = (p.stdout or p.stderr).strip()
    p, _ = run([sys.executable, "-c",
                "import polars, duckdb; print(f'polars {polars.__version__} / duckdb {duckdb.__version__}')"])
    versions["contenders"] = p.stdout.strip()
    print("versions:", versions, flush=True)

    rows = []  # (size, fmt, tool, mode, round, seconds, added, removed, changed)

    for size in sizes:
        n = int(size)
        nrounds, warmup = rounds_for(n)
        for fmt in fmts:
            a = str(Path(args.data) / size / f"A.{fmt}")
            b = str(Path(args.data) / size / f"B.{fmt}")
            print(f"=== {size} rows {fmt} ===", flush=True)
            gate = {}

            def variant(tool, mode):
                workdir = out / size / fmt / tool / mode
                if tool == "compara":
                    cmd = [args.compara, "compare", a, b, "-k", KEY,
                           "--threads", args.threads]
                    if mode == "summary":
                        cmd.append("--summary")
                    else:
                        cmd += ["--export-dir", str(workdir)]
                    p, dt = run(cmd)
                    assert p.returncode in (0, 1), f"compara failed: {p.stderr[:500]}"
                    counts = parse_compara_summary(p.stdout) if mode == "summary" else {}
                else:
                    workdir.mkdir(parents=True, exist_ok=True)
                    js = workdir / "counts.json"
                    cmd = [sys.executable, str(CONTENDERS), tool, a, b,
                           "--key", KEY, "--json", str(js), "--threads", args.threads]
                    if mode == "fulldiff":
                        cmd += ["--rows-out", str(workdir)]
                    p, dt = run(cmd)
                    assert p.returncode in (0, 1), f"{tool} failed: {p.stderr[-500:]}"
                    counts = {}
                    if mode == "summary":
                        c = json.loads(js.read_text())
                        counts = {"added": c["added"], "removed": c["removed"],
                                  "changed": c["changed"]}
                return dt, counts

            for tool, mode in (("compara", "summary"), ("compara", "fulldiff"),
                               ("polars", "summary"), ("polars", "fulldiff"),
                               ("duckdb", "summary"), ("duckdb", "fulldiff")):
                if warmup:
                    variant(tool, mode)  # discarded
                secs, counts = [], {}
                for r in range(nrounds):
                    dt, counts = variant(tool, mode)
                    secs.append(dt)
                    rows.append((size, fmt, tool, mode, r, dt,
                                 counts.get("added", ""), counts.get("removed", ""),
                                 counts.get("changed", "")))
                med = statistics.median(secs)
                print(f"  {tool:8s} {mode:8s} median {med:8.3f}s "
                      f"rounds={[f'{s:.2f}' for s in secs]}", flush=True)
                if mode == "summary":
                    gate[tool] = (counts["added"], counts["removed"], counts["changed"])
            if len(set(gate.values())) != 1:
                raise SystemExit(f"COUNT MISMATCH at {size}/{fmt}: {gate}")
            print(f"  gate OK {gate}", flush=True)

    with tsv.open("w") as f:
        f.write("size\tfmt\ttool\tmode\tround\tseconds\tadded\tremoved\tchanged\n")
        for size, fmt, tool, mode, r, dt, ad, rm, ch in rows:
            f.write(f"{size}\t{fmt}\t{tool}\t{mode}\t{r}\t{dt:.3f}\t{ad}\t{rm}\t{ch}\n")

    # Median table.
    med = {}
    for size, fmt, tool, mode, _r, dt, _ad, _rm, _ch in rows:
        med.setdefault((size, fmt, tool, mode), []).append(dt)
    lines = ["# Size ladder: compara vs Polars vs DuckDB", "",
             f"compara {versions['compara']}; {versions['contenders']}; "
             f"threads={args.threads}; medians of per-cell rounds", "",
             "| rows | fmt | mode | compara (s) | polars (s) | duckdb (s) |",
             "| --- | --- | --- | ---: | ---: | ---: |"]
    for size in sizes:
        for fmt in fmts:
            for mode in ("summary", "fulldiff"):
                m = {t: statistics.median(med[(size, fmt, t, mode)])
                     for t in ("compara", "polars", "duckdb")}
                lines.append(f"| {size} | {fmt} | {mode} | {m['compara']:.3f} | "
                             f"{m['polars']:.3f} | {m['duckdb']:.3f} |")
    table = "\n".join(lines) + "\n"
    (out / "RESULTS.md").write_text(table)
    print(table, flush=True)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(table + "\n")


if __name__ == "__main__":
    main()
