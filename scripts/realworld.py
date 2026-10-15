#!/usr/bin/env python3
"""The comparison on files as other tools write them, against the tools people use.

Every other benchmark here reads files this project's own generator wrote, and
its Parquet is a particular kind: every column a string, snappy or uncompressed,
written the way the readers were built to read. That is not what arrives from a
warehouse. pyarrow, Polars, DuckDB and pandas each write the same CSV as typed
columns -- dates, doubles, 64-bit integers, timestamps -- with their own codec
(Polars: zstd), their own dictionary encoding and their own row groups. Measured
on those files, the C and C++ ports refuse every one, Zig falls back to its row
reader, and only the Rust port stays on its columnar path; none of that shows in
a table built on generated Parquet.

So this script takes one generated pair and writes it every way a user's file
would arrive:

  csv, tsv            the generator's text, and the same rows tab-separated
  parquet-gen         the generator's own Parquet: all strings (the old tables)
  parquet-pyarrow     `pq.write_table(pyarrow.csv.read_csv(...))`, defaults
  parquet-polars      `pl.read_csv(...).write_parquet(...)`, defaults
  parquet-duckdb      `COPY (FROM read_csv(...)) TO ... (FORMAT parquet)`, defaults
  parquet-pandas      `pd.read_csv(...).to_parquet(...)`, defaults

for two shapes: `wide`, the standard twenty columns keyed on
`(account_id, txn_id)`, and `narrow`, five typed columns keyed on two integers
(`id, k2` with `amount, name, status` compared), B shuffled against A -- the
shape most row-level diffs in the wild are asked about.

Then it asks every port and DuckDB, Polars and pandas the same question on every
file, and holds every answer to the one the Rust port gives on the CSV of that
shape: a faster answer that is not the same answer is not a result, and is
printed as one. Two tasks, timed separately:

  counts   unchanged / changed / added / removed, nothing written
  rows     the same, plus the added, removed and changed rows written as CSV
           (`--export-dir` in the Rust port, `COPY ... TO` / `write_csv` /
           `to_csv` in the others). C, C++ and Zig have no such output.

Each run is a process of its own, measured by a blocking `wait4` for wall, CPU
and peak RSS, best of `--repeats`, every tool once per round with
the starting tool rotating. The files are written by a child process
(`--prepare`), because a forked child counts its parent's resident pages until
it execs: a harness holding pyarrow, Polars and DuckDB charged every tool here
about 400 MB of peak RSS that was the harness's own. The contenders run in Python, so their numbers
include starting the interpreter and importing the library -- which their users
pay too, and which is 0.1-0.3 s of the totals on a small pair.

  python3 scripts/realworld.py --rows 1m --repeats 3 --md-out realworld.md
"""
from __future__ import annotations

import argparse
import csv
import json
import os
import random
import re
import shutil
import subprocess
import sys
import signal
import tempfile
import time
from pathlib import Path

from bench_formats_ports import host, warm
from mdtable import render

ROOT = Path(__file__).resolve().parent.parent
GEN = ROOT / "rust/target/release/compara-gen-data"
CONTENDERS = Path(__file__).resolve().parent / "contenders.py"

PORTS = [
    ("C", ROOT / "c/csvdiff"),
    ("C++", ROOT / "cpp/build/csvdiff"),
    ("compara", ROOT / "rust/target/release/compara"),
    ("Zig", ROOT / "zig/zig-out/bin/csvdiff"),
]
TOOLS = ["duckdb", "polars", "pandas"]
SHAPES = {
    "wide": {"key": "account_id,txn_id", "ignore": "updated_at"},
    "narrow": {"key": "id,k2", "ignore": ""},
}
WRITERS = ["gen", "pyarrow", "polars", "duckdb", "pandas"]


# --------------------------------------------------------------------------
# The files.

def narrow_pair(rows: int, out: Path, seed: int = 7) -> tuple[Path, Path]:
    """Five columns, two integer keys, B shuffled: 0.5% of keys only in A, 0.5%
    only in B, 3% of amounts, 1% of names and 1% of statuses changed, and ten
    keys repeated in A."""
    rng = random.Random(seed)
    names = [f"customer {i:05d}" for i in range(5000)]
    statuses = ["open", "paid", "late", "void", "held"]
    a = []
    for i in range(rows):
        a.append([i + 1, i % 4, f"{rng.randrange(1, 10_000_000) / 100:.2f}",
                  rng.choice(names), rng.choice(statuses)])
    b = []
    for row in a:
        r = rng.random()
        if r < 0.005:
            continue  # removed
        row = list(row)
        if r < 0.035:
            row[2] = f"{float(row[2]) + 0.01:.2f}"
        elif r < 0.045:
            row[3] = rng.choice(names)
        elif r < 0.055:
            row[4] = rng.choice(statuses)
        b.append(row)
    for i in range(rows // 200):
        b.append([rows + i + 1, (rows + i) % 4, f"{rng.randrange(1, 10_000_000) / 100:.2f}",
                  rng.choice(names), rng.choice(statuses)])
    rng.shuffle(b)
    a.extend(list(a[i * (rows // 10)]) for i in range(10))  # repeated keys, first row wins
    paths = (out / "narrow_a.csv", out / "narrow_b.csv")
    for path, table in zip(paths, (a, b)):
        with path.open("w", newline="") as handle:
            w = csv.writer(handle, lineterminator="\n")
            w.writerow(["id", "k2", "amount", "name", "status"])
            w.writerows(table)
    return paths


def write_variants(shape: str, a_csv: Path, b_csv: Path, rows: str, out: Path) -> dict[str, tuple[Path, Path]]:
    """Every way this pair can arrive, keyed by variant name."""
    import duckdb
    import pandas as pd
    import polars as pl
    import pyarrow.csv as pc
    import pyarrow.parquet as pq

    variants: dict[str, tuple[Path, Path]] = {"csv": (a_csv, b_csv)}
    tsv = []
    for src in (a_csv, b_csv):
        dst = src.with_suffix(".tsv")
        # Generated values hold no quote, comma or tab, so the swap is exact.
        with src.open("rb") as fi, dst.open("wb") as fo:
            fo.write(fi.read().replace(b",", b"\t"))
        tsv.append(dst)
    variants["tsv"] = (tsv[0], tsv[1])
    for writer in WRITERS:
        if writer == "gen":
            # The generator writes the same rows itself, every column a string;
            # it only knows the wide shape.
            if shape == "wide":
                subprocess.run([str(GEN), "--rows", rows, "--out-dir", str(out), "--prefix",
                                f"{shape}_gen", "--format", "parquet"], check=True,
                               capture_output=True)
                variants["parquet-gen"] = (out / f"{shape}_gen_a.parquet",
                                           out / f"{shape}_gen_b.parquet")
            continue
        pair = []
        for side, src in (("a", a_csv), ("b", b_csv)):
            dst = out / f"{shape}_{writer}_{side}.parquet"
            if writer == "pyarrow":
                pq.write_table(pc.read_csv(src), dst)
            elif writer == "polars":
                pl.read_csv(src).write_parquet(dst)
            elif writer == "duckdb":
                duckdb.sql(f"COPY (FROM read_csv('{src}')) TO '{dst}' (FORMAT parquet)")
            elif writer == "pandas":
                pd.read_csv(src).to_parquet(dst)
            pair.append(dst)
        if len(pair) == 2:
            variants[f"parquet-{writer}"] = (pair[0], pair[1])
    return variants


def layout(path: Path) -> dict:
    """What a Parquet file is made of: the writer, codecs, page kinds, encodings,
    row groups, and how many columns are typed rather than strings."""
    import pyarrow.parquet as pq

    md = pq.ParquetFile(path).metadata
    raw = path.read_bytes()
    codecs, pages = set(), set()
    for g in range(md.num_row_groups):
        rg = md.row_group(g)
        for c in range(md.num_columns):
            col = rg.column(c)
            codecs.add(col.compression.lower())
            kind, enc = first_page(raw, col.data_page_offset)
            pages.add(kind)
            if enc:
                pages.add(enc)
    typed = sum(1 for c in range(md.num_columns)
                if md.schema.column(c).physical_type != "BYTE_ARRAY")
    return {"writer": md.created_by.split("(")[0].strip(), "codecs": sorted(codecs),
            "pages": sorted(pages), "row_groups": md.num_row_groups,
            "typed": typed, "columns": md.num_columns,
            "mb": path.stat().st_size / (1 << 20)}


PAGE_KIND = {0: "v1", 2: "dict", 3: "v2"}
ENCODING = {0: "plain", 2: "plain-dict", 3: "rle", 5: "delta", 6: "delta-length",
            7: "delta-bytes", 8: "rle-dict", 9: "byte-stream-split"}


def first_page(raw: bytes, offset: int) -> tuple[str, str]:
    """The kind and encoding of the data page at `offset`, from its Thrift
    compact header: field 1 is the type; field 5 (v1) or 8 (v2) is a struct
    whose second field is the encoding."""
    i, fid, kind, enc = offset, 0, "?", ""

    def varint(j: int) -> tuple[int, int]:
        shift = value = 0
        while True:
            x = raw[j]
            j += 1
            value |= (x & 0x7F) << shift
            shift += 7
            if x < 0x80:
                return value, j

    for _ in range(16):
        h = raw[i]
        i += 1
        if h == 0:
            break
        fid += h >> 4
        t = h & 0x0F
        if t == 5:  # i32
            v, i = varint(i)
            v = (v >> 1) ^ -(v & 1)
            if fid == 1:
                kind = PAGE_KIND.get(v, str(v))
        elif t == 12 and fid in (5, 8):
            sub = 0
            for _ in range(2):
                hh = raw[i]
                i += 1
                sub += hh >> 4
                v, i = varint(i)
                if sub == 2:
                    enc = ENCODING.get((v >> 1) ^ -(v & 1), "?")
            break
        else:
            break
    return kind, enc


def prepare(shape: str, rows: str, data: Path) -> dict:
    """Writes every variant of one shape and describes the Parquet ones. Runs in
    a child process of its own; see the note on peak RSS at the top."""
    if shape == "wide":
        subprocess.run([str(GEN), "--rows", rows, "--out-dir", str(data), "--prefix", "wide"],
                       check=True, capture_output=True)
        a, b = data / "wide_a.csv", data / "wide_b.csv"
    else:
        a, b = narrow_pair(parse_rows(rows), data)
    variants = write_variants(shape, a, b, rows, data)
    return {"variants": {v: [str(x), str(y)] for v, (x, y) in variants.items()},
            "layouts": {v: layout(x) for v, (x, _) in variants.items() if v.startswith("parquet-")}}


def parse_rows(text: str) -> int:
    t = text.strip().lower()
    mult = {"k": 1_000, "m": 1_000_000}.get(t[-1:], 1)
    return int(float(t[:-1] if mult > 1 else t) * mult)


# --------------------------------------------------------------------------
# The runs.

def run(argv: list[str], timeout: float) -> tuple[float, float, float, int, str]:
    """Wall seconds, peak RSS in MB, CPU seconds, exit code and the last line of
    stderr for one child.

    Not `bench_formats_ports.run`: that one polls the child every 50 ms to sample
    `VmData`, which is right for runs of seconds and minutes and puts a 50 ms
    grain on every wall time -- a quarter of a 0.2 s run. This waits in the
    kernel, so the wall time is the child's own; the rusage is `wait4`'s for
    that child, reaped here rather than by `Popen`.
    """
    def expire(signum, frame):
        raise TimeoutError

    with tempfile.TemporaryFile() as err:
        started = time.perf_counter()
        child = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=err)
        previous = signal.signal(signal.SIGALRM, expire)
        signal.alarm(max(1, int(timeout)))
        try:
            _, status, usage = os.wait4(child.pid, 0)
        except TimeoutError:
            child.kill()
            _, status, usage = os.wait4(child.pid, 0)
        finally:
            signal.alarm(0)
            signal.signal(signal.SIGALRM, previous)
        seconds = time.perf_counter() - started
        child.returncode = os.waitstatus_to_exitcode(status)
        err.seek(0)
        lines = err.read().decode(errors="replace").strip().splitlines()
    return (seconds, usage.ru_maxrss / 1024, usage.ru_utime + usage.ru_stime,
            child.returncode, lines[-1][:200] if lines else "")



SUMMARY = re.compile(r"\|\s*([a-z]+)(?:\s+[\d.]+s)?\s*$")


def reader_of(stdout: str) -> str:
    """The engine a port says it used, from the end of its summary line:
    `parquet` is the columnar path, `turbo` the row reader."""
    line = stdout.strip().splitlines()[0] if stdout.strip() else ""
    m = SUMMARY.search(line)
    return {"parquet": "columnar", "turbo": "row reader"}.get(m.group(1), m.group(1)) if m else ""


def counts_of(doc: dict) -> dict:
    c = doc.get("counts", doc)
    return {k: c[k] for k in ("unchanged", "changed", "added", "removed")}


def plan(shape, threads, tmp: Path, task: str):
    """(label, argv, gate argv, json path, rows dir) for every tool on this task."""
    spec = SHAPES[shape]
    out = []
    for label, path in PORTS:
        if task == "rows" and label != "compara":
            continue
        if not path.exists():
            continue
        flags = ["-k", spec["key"]] + (["-i", spec["ignore"]] if spec["ignore"] else [])
        if threads:
            flags += ["--threads", str(threads)]
        doc = tmp / f"{label}.json"
        rows_dir = tmp / f"rows-{label}"
        if task == "rows":
            timed = flags + ["--export-dir", str(rows_dir)]
        else:
            timed = flags + (["--summary"] if label == "compara" else [])
        gate = flags + ["--json", str(doc)] + (["--export-dir", str(rows_dir)] if task == "rows" else [])
        out.append((label, [str(path), "compare"], timed, gate, doc, rows_dir))
    for tool in TOOLS:
        doc = tmp / f"{tool}.json"
        rows_dir = tmp / f"rows-{tool}"
        flags = ["--key", spec["key"], "--ignore", spec["ignore"], "--json", str(doc)]
        if threads:
            flags += ["--threads", str(threads)]
        if task == "rows":
            flags += ["--rows-out", str(rows_dir)]
        out.append((tool, [sys.executable, str(CONTENDERS), tool], flags, flags, doc, rows_dir))
    return out


def reference_answer(variants, shape, tmp: Path, timeout: float) -> dict:
    """The answer every run is held to: the Rust port on this shape's CSV."""
    spec = SHAPES[shape]
    a, b = variants["csv"]
    doc = tmp / "reference.json"
    argv = [str(dict(PORTS)["compara"]), "compare", str(a), str(b), "-k", spec["key"],
            "--json", str(doc)] + (["-i", spec["ignore"]] if spec["ignore"] else [])
    done = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    if done.returncode not in (0, 1):
        raise SystemExit(f"the reference run failed: {done.stderr.strip()[-400:]}")
    return counts_of(json.loads(doc.read_text()))


def measure(variants, shape, args, tmp: Path):
    """Every tool on every variant, both tasks, held to the reference answer."""
    reference = reference_answer(variants, shape, tmp, args.timeout)
    print(f"  reference (compara on csv): {reference}", flush=True)
    results = []
    env_threads = {"POLARS_MAX_THREADS": str(args.threads)} if args.threads else {}
    os.environ.update(env_threads)
    for variant, (a, b) in variants.items():
        warm(a, b)
        for task in ("counts", "rows"):
            entries = plan(shape, args.threads, tmp, task)
            live = []
            for label, prefix, timed, gate, doc, rows_dir in entries:
                doc.unlink(missing_ok=True)
                shutil.rmtree(rows_dir, ignore_errors=True)
                done = subprocess.run(prefix + [str(a), str(b)] + gate, capture_output=True,
                                      text=True, timeout=args.timeout)
                row = {"shape": shape, "variant": variant, "task": task, "tool": label}
                if done.returncode not in (0, 1) or not doc.exists():
                    why = (done.stderr.strip().splitlines() or ["no output"])[-1]
                    # The message, not the path some ports append to it.
                    why = re.sub(r":\s*/\S*$", "", why.replace("error: ", ""))
                    row.update(state="refused", why=why[:120])
                    results.append(row)
                    continue
                got = counts_of(json.loads(doc.read_text()))
                # Which reader ran matters only for Parquet, and only the Rust
                # and Zig ports have two; C and C++ name every engine `turbo`.
                parquet = variant.startswith("parquet-") and label in ("compara", "Zig")
                row.update(state="ok" if got == reference else "differs", counts=got,
                           reader=reader_of(done.stdout) if parquet else "")
                results.append(row)
                live.append((row, prefix + [str(a), str(b)] + timed))
            for rnd in range(args.repeats):
                turn = rnd % len(live) if live else 0
                for row, argv in live[turn:] + live[:turn]:
                    seconds, rss, cpu, code, why = run(argv, args.timeout)
                    if code not in (0, 1):
                        row.update(state="failed", why=why[:120])
                        continue
                    if "seconds" not in row or seconds < row["seconds"]:
                        row.update(seconds=seconds, cpu=cpu, rss=rss)
            for row, _ in live:
                if "seconds" in row:
                    print(f"  {shape:6s} {variant:16s} {task:6s} {row['tool']:8s} "
                          f"{row['seconds']:7.2f}s {row['cpu']:7.2f}s cpu {row['rss']:7.0f} MB"
                          f"  {row.get('reader', '')}  {row['state']}", flush=True)
            for row in results:
                if row["variant"] == variant and row["task"] == task and row["state"] == "refused":
                    print(f"  {shape:6s} {variant:16s} {task:6s} {row['tool']:8s} refused: {row['why']}",
                          flush=True)
    return results


# --------------------------------------------------------------------------
# The tables.

def cell(row: dict | None) -> str:
    if row is None:
        return "-"
    if row["state"] == "refused":
        return f"refused: {row['why']}"
    if row["state"] == "failed":
        return f"failed: {row['why']}"
    if "seconds" not in row:
        return "-"
    text = f"{row['seconds']:.2f}s · {row['rss']:,.0f} MB"
    if row.get("reader") == "row reader":
        text += " (row reader)"
    if row["state"] == "differs":
        text = f"**different answer** {row['counts']}"
    return text


def tables(results: list[dict], layouts: dict) -> str:
    out = []
    grid = []
    for (shape, variant), lay in layouts.items():
        grid.append([shape, variant, lay["writer"], ", ".join(lay["codecs"]),
                     ", ".join(lay["pages"]), str(lay["row_groups"]),
                     f"{lay['typed']} of {lay['columns']}", f"{lay['mb']:,.1f}"])
    out.append("### What each writer put in the file\n")
    out.append(render(["Shape", "File", "Written by", "Codec", "Pages", "Row groups",
                       "Typed columns", "MB (A)"], ["l"] * 6 + ["r", "r"], grid))
    for task, title in (("counts", "Counts only"), ("rows", "Counts and the rows written as CSV")):
        tools = ([p for p, _ in PORTS] if task == "counts" else ["compara"]) + TOOLS
        by = {(r["shape"], r["variant"], r["tool"]): r for r in results if r["task"] == task}
        keys = sorted({(r["shape"], r["variant"]) for r in results if r["task"] == task},
                      key=lambda k: (list(SHAPES).index(k[0]), ORDER.index(k[1])))
        grid = [[s, v] + [cell(by.get((s, v, t))) for t in tools] for s, v in keys]
        out.append(f"\n### {title} — wall (best) · peak RSS\n")
        out.append(render(["Shape", "File"] + tools, ["l", "l"] + ["l"] * len(tools), grid))
    return "\n".join(out)


ORDER = ["csv", "tsv"] + [f"parquet-{w}" for w in WRITERS]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rows", default="1m")
    parser.add_argument("--shapes", default="wide,narrow")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--threads", type=int, default=None,
                        help="threads for every tool; the default is what each picks")
    parser.add_argument("--timeout", type=float, default=1800)
    parser.add_argument("--data-dir", default=str(ROOT / "data/realworld"))
    parser.add_argument("--md-out", type=Path, default=None)
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--prepare", default=None, metavar="SHAPE", help=argparse.SUPPRESS)
    parser.add_argument("--require", default="compara",
                        help="tools that must read every file and give the CSV answer "
                             "(comma-separated); the run fails otherwise")
    args = parser.parse_args(argv)

    data = Path(args.data_dir)
    if args.prepare:
        print(json.dumps(prepare(args.prepare, args.rows, data)))
        return 0
    shutil.rmtree(data, ignore_errors=True)
    data.mkdir(parents=True)
    tmp = data / "out"
    tmp.mkdir()

    results, layouts = [], {}
    for shape in [s.strip() for s in args.shapes.split(",") if s.strip()]:
        done = subprocess.run([sys.executable, __file__, "--prepare", shape, "--rows", args.rows,
                               "--data-dir", str(data)], capture_output=True, text=True)
        if done.returncode != 0:
            raise SystemExit(f"writing the {shape} files failed:\n{done.stderr[-2000:]}")
        prepared = json.loads(done.stdout)
        variants = {v: (Path(a), Path(b)) for v, (a, b) in prepared["variants"].items()}
        for variant, lay in prepared["layouts"].items():
            layouts[(shape, variant)] = lay
        print(f"\n{shape}: {', '.join(variants)}", flush=True)
        results.extend(measure(variants, shape, args, tmp))

    h = host()
    md = (f"Measured on `{h['cpu']}`, {h['cores']} cores, {args.rows} rows a side, "
          f"best of {args.repeats}" + (f", {args.threads} threads" if args.threads else "")
          + ".\n\n" + tables(results, layouts))
    print("\n" + md)
    if args.md_out:
        args.md_out.write_text(md + "\n")
    if args.json_out:
        args.json_out.write_text(json.dumps(
            {"rows": args.rows, "repeats": args.repeats, "threads": args.threads,
             "host": h, "results": results,
             "layouts": {f"{s}/{v}": lay for (s, v), lay in layouts.items()}}, indent=2))

    bad = [r for r in results if r["state"] == "differs"
           or (r["tool"] in args.require.split(",") and r["state"] != "ok")]
    for r in bad:
        print(f"!! {r['tool']} on {r['shape']}/{r['variant']} ({r['task']}): {r['state']} "
              f"{r.get('why', r.get('counts', ''))}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
