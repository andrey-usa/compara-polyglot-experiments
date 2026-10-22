#!/usr/bin/env python3
"""compara against DuckDB on one keyed comparison, both reading from S3.

The pair is uploaded to an S3 endpoint (SeaweedFS on the runner) and each tool
reads it from there: compara with `s3://` URLs, DuckDB with httpfs and an S3
secret. The task is compara's: rows matched on (account_id, txn_id), first
occurrence of a repeated key, every other column compared but `updated_at`;
the answer is four counts -- unchanged, changed, added, removed -- and the
run fails unless both tools give the same four.

    bench_remote.py --compara BIN --endpoint http://localhost:8333 \
        --bucket bkt --a t_a.parquet --b t_b.parquet [--compare amount,note]

Prints one JSON line per run and a markdown table at the end.
"""
import argparse
import json
import os
import resource
import subprocess
import sys
import time

KEY = ["account_id", "txn_id"]
IGNORE = "updated_at"


def timed(argv, env=None):
    t0 = time.perf_counter()
    p = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    _, status, ru = os.wait4(p.pid, 0)
    wall = time.perf_counter() - t0
    out, err = p.stdout.read().decode(), p.stderr.read().decode()
    code = os.waitstatus_to_exitcode(status)
    return wall, ru.ru_utime + ru.ru_stime, ru.ru_maxrss // 1024, code, out, err


DUCK = r'''
import duckdb, json, sys
endpoint, bucket, a, b, cols, fmt = sys.argv[1:7]
cols = json.loads(cols)
con = duckdb.connect()
con.execute("SET enable_progress_bar = false")
con.execute("INSTALL httpfs; LOAD httpfs;")
host = endpoint.split("://", 1)[1]
con.execute(f"""CREATE SECRET s (TYPE s3, KEY_ID '{__import__('os').environ['AWS_ACCESS_KEY_ID']}',
    SECRET '{__import__('os').environ['AWS_SECRET_ACCESS_KEY']}', ENDPOINT '{host}',
    URL_STYLE 'path', USE_SSL false, REGION 'us-east-1')""")
def src(name):
    url = f"s3://{bucket}/{name}"
    if name.endswith("/"):
        # A directory of parts: read in name order, as compara reads them.
        return (f"read_parquet('{url}*.parquet', filename = true, file_row_number = true)",
                "filename, file_row_number")
    if fmt == "parquet":
        return f"read_parquet('{url}', file_row_number = true)", "file_row_number"
    return f"read_csv('{url}', all_varchar = true)", "rowid"
want = ", ".join(["account_id", "txn_id"] + cols)
for side, name in (("a", a), ("b", b)):
    table, order = src(name)
    if order == "rowid":
        # A CSV has no row number to read; a table loaded from it in order
        # has one, its rowid.
        con.execute(f"CREATE TABLE raw_{side} AS SELECT {want} FROM {table}")
        table = f"raw_{side}"
    con.execute(f"""CREATE TABLE {side} AS SELECT {want} FROM {table}
        QUALIFY row_number() OVER (PARTITION BY account_id, txn_id ORDER BY {order}) = 1""")
eq = " AND ".join(f"a.{c} IS NOT DISTINCT FROM b.{c}" for c in cols) or "true"
row = con.execute(f"""SELECT
  count(*) FILTER (a.account_id IS NOT NULL AND b.account_id IS NOT NULL AND ({eq})),
  count(*) FILTER (a.account_id IS NOT NULL AND b.account_id IS NOT NULL AND NOT ({eq})),
  count(*) FILTER (a.account_id IS NULL),
  count(*) FILTER (b.account_id IS NULL)
FROM a FULL OUTER JOIN b ON a.account_id = b.account_id AND a.txn_id = b.txn_id""").fetchone()
print(json.dumps(dict(zip(("unchanged", "changed", "added", "removed"), row)) | {"duckdb": duckdb.__version__}))
'''


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--compara", required=True)
    ap.add_argument("--endpoint", required=True)
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--columns", required=True, help="every column of the files, comma-separated")
    ap.add_argument("--compare", default="", help="only these columns (default: all but the key and updated_at)")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--label", default="")
    ap.add_argument("--format", default="", help="parquet or csv (default: from the name)")
    args = ap.parse_args()
    fmt = args.format or ("parquet" if args.a.endswith(".parquet") else "csv")
    everything = [c for c in args.columns.split(",") if c]
    cols = [c for c in (args.compare.split(",") if args.compare else everything)
            if c and c not in KEY and c != IGNORE]
    env = dict(os.environ, AWS_ENDPOINT_URL=args.endpoint, AWS_REGION="us-east-1")
    summary = "/tmp/bench-remote-summary.json"
    compara = [args.compara, "compare", f"s3://{args.bucket}/{args.a}", f"s3://{args.bucket}/{args.b}",
               "-k", ",".join(KEY), "-i", IGNORE, "--json", summary]
    if args.compare:
        compara += ["-c", ",".join(cols)]
    duck = [sys.executable, "-c", DUCK, args.endpoint, args.bucket, args.a, args.b, json.dumps(cols), fmt]

    results = {}
    for name, argv in (("compara", compara), ("duckdb", duck)):
        runs = []
        for _ in range(args.reps):
            wall, cpu, rss, code, out, err = timed(argv, env)
            if name == "compara":
                if code not in (0, 1):
                    sys.exit(f"compara failed ({code}): {err[-2000:]}")
                c = json.load(open(summary))["counts"]
                counts = {k: c[k] for k in ("unchanged", "changed", "added", "removed")}
                note = err.strip().replace("\n", " | ")[:300]
            else:
                if code != 0:
                    sys.exit(f"duckdb failed ({code}): {err[-2000:]}")
                got = json.loads([l for l in out.splitlines() if l.startswith("{")][-1])
                note = "duckdb " + got.pop("duckdb")
                counts = got
            runs.append({"wall": wall, "cpu": cpu, "rss_mb": rss, "counts": counts, "note": note})
            print(json.dumps({"tool": name, **runs[-1]}), flush=True)
        results[name] = runs
    ref = results["compara"][0]["counts"]
    for name, runs in results.items():
        for r in runs:
            if r["counts"] != ref:
                sys.exit(f"{name} disagrees: {r['counts']} vs compara {ref}")
    best = {n: min(runs, key=lambda r: r["wall"]) for n, runs in results.items()}
    base = best["compara"]["wall"]
    what = f"{args.label} {fmt}, {'columns ' + args.compare if args.compare else 'all columns'}".strip()
    print(f"\n#### {what} (best of {args.reps}; counts agree: {ref})\n")
    print("| tool | wall | CPU | peak RSS | vs compara |")
    print("|---|---:|---:|---:|---:|")
    for n, r in sorted(best.items(), key=lambda kv: kv[1]["wall"]):
        print(f"| {n} | {r['wall']:.2f} s | {r['cpu']:.1f} s | {r['rss_mb']:,} MB | {r['wall'] / base:.2f}x |")
    print(f"\n{best['compara']['note']}; {best['duckdb']['note']}")


if __name__ == "__main__":
    main()
