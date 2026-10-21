#!/usr/bin/env python3
"""Scale ladder, one rung: Rust compara vs DuckDB vs PyArrow, summary only.

Runs ONE size (the workflow fans sizes out to one runner each, the
scale-ceiling pattern: a rung compares tools on one host, never machines,
and one rung's failure hides nothing above or below it).

Summary only -- no diff rows are written by any tool (`compara --summary`,
contenders without --rows-out). That is the cheapest run each tool can do,
so a win here cannot be attributed to export formatting.

Per cell (size x format): every tool runs in its own subprocess, timed with
perf_counter, peak RSS from wait4's rusage for that exact child. Tool order
rotates per round so no tool always gets the cold (or warm) cache. A tool
that dies (OOM kill, timeout, error) is recorded as FAIL for that cell and
the ladder continues; the count gate is checked across the tools that
finished, and any failure is printed in the table, not hidden.

Rounds: 3 timed rounds up to 50M rows, 2 above that (medians reported).

usage: python3 scripts/bench_scale_ladder.py --size 10000000 \
           --formats csv,parquet --threads 4 \
           --compara rust/target/release/compara --data data --out results
"""
import argparse
import json
import os
import re
import resource
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

KEY = "id,k2"
HERE = Path(__file__).resolve().parent
TOOLS = ("compara", "duckdb", "pyarrow")
PER_RUN_TIMEOUT = 2400  # seconds; a single run past this is a result: too slow


def run_timed(cmd):
    """Run cmd, return (returncode, seconds, peak_rss_mb, stdout, stderr_tail).

    Peak RSS is wait4 rusage of this exact child (the kernel high-water
    mark), not a rusage maximum shared with earlier children.
    """
    with tempfile.TemporaryFile() as out_f, tempfile.TemporaryFile() as err_f:
        t0 = time.perf_counter()
        proc = subprocess.Popen(cmd, stdout=out_f, stderr=err_f)
        ru = None
        rc = None
        while True:
            done_pid, status, ru_try = os.wait4(proc.pid, os.WNOHANG)
            if done_pid == proc.pid:
                rc = os.waitstatus_to_exitcode(status)
                ru = ru_try
                break
            if time.perf_counter() - t0 > PER_RUN_TIMEOUT:
                proc.kill()
                _, status, ru = os.wait4(proc.pid, 0)
                rc = os.waitstatus_to_exitcode(status)
                break
            time.sleep(0.02)
        proc.returncode = rc  # already reaped via wait4; keep Popen from re-waiting
        dt = time.perf_counter() - t0
        peak_mb = (ru.ru_maxrss / 1024) if ru else -1  # ru_maxrss is KiB on Linux
        out_f.seek(0)
        err_f.seek(0)
        out = out_f.read().decode(errors="replace")
        err = err_f.read().decode(errors="replace")
        return rc, dt, peak_mb, out, err[-400:]


def parse_compara_summary(out):
    m = re.search(r"matched ([\d,]+) \(changed ([\d,]+)\) \| added ([\d,]+) \| removed ([\d,]+)", out)
    if not m:
        raise AssertionError(f"unparsed compara output: {out[:300]}")
    matched, changed, added, removed = (int(x.replace(",", "")) for x in m.groups())
    return {"added": added, "removed": removed, "changed": changed}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size", required=True)
    ap.add_argument("--formats", required=True)
    ap.add_argument("--threads", required=True)
    ap.add_argument("--compara", default="",
                    help="path to the compara binary (only needed when compara is in --tools)")
    ap.add_argument("--data", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--tools", default="compara,duckdb,pyarrow",
                    help="comma-separated subset of compara,duckdb,pyarrow")
    args = ap.parse_args()
    tools = tuple(t.strip() for t in args.tools.split(",") if t.strip())

    size = args.size
    n = int(size)
    fmts = [f.strip() for f in args.formats.split(",") if f.strip()]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    nrounds = 3 if n <= 50_000_000 else 2

    versions = {}
    if args.compara and Path(args.compara).exists():
        p = subprocess.run([args.compara, "--version"], capture_output=True, text=True)
        versions["compara"] = (p.stdout or p.stderr).strip()
    else:
        versions["compara"] = "(not in this job)"
    p = subprocess.run([sys.executable, "-c",
                        "import importlib\n"
                        "out = []\n"
                        "for m in ('duckdb', 'pyarrow'):\n"
                        "    try:\n"
                        "        mod = importlib.import_module(m); out.append(f'{m} {mod.__version__}')\n"
                        "    except ImportError:\n"
                        "        pass\n"
                        "print(' / '.join(out))"],
                       capture_output=True, text=True)
    versions["contenders"] = p.stdout.strip()
    print("versions:", versions, flush=True)

    # records[(fmt, tool)] = {"secs": [...], "peak_mb": max, "counts": {...}|None, "error": str|None}
    records = {}

    for fmt in fmts:
        a = str(Path(args.data) / size / f"A.{fmt}")
        b = str(Path(args.data) / size / f"B.{fmt}")
        print(f"=== {size} rows {fmt} ===", flush=True)

        def variant(tool):
            workdir = out / size / fmt / tool
            workdir.mkdir(parents=True, exist_ok=True)
            if tool == "compara":
                cmd = [args.compara, "compare", a, b, "-k", KEY,
                       "--threads", args.threads, "--summary"]
                rc, dt, peak, stdout, err = run_timed(cmd)
                if rc not in (0, 1):
                    return dt, peak, None, f"rc={rc}: {err}"
                return dt, peak, parse_compara_summary(stdout), None
            script = HERE / ("contenders.py" if tool == "duckdb" else "contenders_pyarrow.py")
            js = workdir / "counts.json"
            cmd = [sys.executable, str(script)] + ([tool] if tool == "duckdb" else []) + \
                  [a, b, "--key", KEY, "--json", str(js), "--threads", args.threads]
            rc, dt, peak, stdout, err = run_timed(cmd)
            if rc not in (0, 1):
                return dt, peak, None, f"rc={rc}: {err}"
            c = json.loads(js.read_text())
            return dt, peak, {"added": c["added"], "removed": c["removed"],
                              "changed": c["changed"]}, None

        for tool in tools:
            records[(fmt, tool)] = {"secs": [], "peak_mb": 0.0,
                                    "counts": None, "error": None}
        for r in range(nrounds):
            order = tools[r % len(tools):] + tools[:r % len(tools)]
            for tool in order:
                rec = records[(fmt, tool)]
                if rec["error"] and r > 0:
                    continue  # a dead tool stays dead; don't pay for it again
                dt, peak, counts, error = variant(tool)
                rec["peak_mb"] = max(rec["peak_mb"], peak)
                if error:
                    rec["error"] = error
                    print(f"  {tool:8s} round {r}: FAIL {error[:200]}", flush=True)
                else:
                    rec["secs"].append(dt)
                    rec["counts"] = counts
                    print(f"  {tool:8s} round {r}: {dt:9.3f}s peak {peak:8.0f} MB", flush=True)
        gate = {t: (records[(fmt, t)]["counts"]["added"], records[(fmt, t)]["counts"]["removed"],
                    records[(fmt, t)]["counts"]["changed"])
                for t in tools if records[(fmt, t)]["counts"]}
        if len(set(gate.values())) > 1:
            raise SystemExit(f"COUNT MISMATCH at {size}/{fmt}: {gate}")
        print(f"  gate OK (finishers agree): {gate}", flush=True)
        dump(records, fmts, tools, versions, size, nrounds, args, out)

    table = dump(records, fmts, tools, versions, size, nrounds, args, out)
    print(table, flush=True)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(table + "\n")


def dump(records, fmts, tools, versions, size, nrounds, args, out):
    """(Re)write results.tsv + RESULTS.md from whatever has completed so far.

    Called after every format, not just at the end: run 39669226917 lost its
    finished 100M CSV rounds (compara 32.3s, DuckDB 236s) because the table
    was only written after the last cell, and the step died before that.
    """
    with (out / "results.tsv").open("w") as f:
        f.write("size\tfmt\ttool\tstatus\tmedian_s\trounds_s\tpeak_mb\tadded\tremoved\tchanged\n")
        for fmt in fmts:
            for tool in tools:
                rec = records.get((fmt, tool))
                if rec is None:
                    continue
                if rec["error"] or not rec["secs"]:
                    f.write(f"{size}\t{fmt}\t{tool}\tFAIL\t\t\t{rec['peak_mb']:.0f}\t\t\t\t{rec['error'] or ''}\n")
                else:
                    c = rec["counts"]
                    f.write(f"{size}\t{fmt}\t{tool}\tok\t{statistics.median(rec['secs']):.3f}\t"
                            f"{','.join(f'{s:.2f}' for s in rec['secs'])}\t{rec['peak_mb']:.0f}\t"
                            f"{c['added']}\t{c['removed']}\t{c['changed']}\n")

    lines = [f"# Scale ladder rung: {size} rows", "",
             f"compara {versions['compara']}; {versions['contenders']}; "
             f"threads={args.threads}; summary only (no diff rows written); "
             f"median of {nrounds} rounds, tool order rotated per round", "",
             "| fmt | compara (s) | duckdb (s) | pyarrow (s) | compara peak MB | duckdb peak MB | pyarrow peak MB |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for fmt in fmts:
        def cell(tool):
            rec = records.get((fmt, tool))
            if rec is None:
                return "—"
            return f"{statistics.median(rec['secs']):.3f}" if rec["secs"] else "FAIL"
        def peak(tool):
            rec = records.get((fmt, tool))
            return f"{rec['peak_mb']:.0f}" if rec else "—"
        lines.append(f"| {fmt} | {cell('compara')} | {cell('duckdb')} | {cell('pyarrow')} | "
                     f"{peak('compara')} | {peak('duckdb')} | {peak('pyarrow')} |")
    fails = [f"{fmt}/{t}: {records[(fmt, t)]['error']}" for fmt in fmts for t in tools
             if records.get((fmt, t)) and records[(fmt, t)]["error"]]
    if fails:
        lines += ["", "Failures:"] + [f"- {x}" for x in fails]
    table = "\n".join(lines) + "\n"
    (out / "RESULTS.md").write_text(table)
    return table


if __name__ == "__main__":
    main()
