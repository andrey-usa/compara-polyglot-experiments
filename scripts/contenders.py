#!/usr/bin/env python3
"""One keyed comparison done by DuckDB, Polars or pandas, the way their users do it.

`realworld.py` runs this once per measured round, in a process of its own, so
the wall time and the peak RSS it records are what a user of that library pays:
interpreter start, import, read, join and write, and nothing borrowed from the
harness.

Every tool is held to the answer the ports give, so each is told the same rules:

  * a key that appears twice in one file is compared by its first row, in file
    order, and the rest are left out -- a plain join multiplies them instead;
  * text inputs are read as text, every column, with an empty field as missing,
    so `1.0` and `1` differ as they do in the ports; Parquet keeps its types;
  * a compared cell is equal to another when both are missing or both hold the
    same value (`IS NOT DISTINCT FROM`).

`--rows-out DIR` also writes the rows, as the ports' `--export-dir` does: the
B rows whose key is not in A (`added.csv`), the A rows whose key is not in B
(`removed.csv`), and for each changed key its compared columns from both sides
(`changed.csv`). That is the task the counts alone leave out, and the one people
who reach for these libraries usually want.

  python3 scripts/contenders.py duckdb A B --key account_id,txn_id \
      --ignore updated_at --json out.json [--rows-out DIR] [--threads N]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def separator(path: str) -> str:
    """The field separator, from the header line, as the ports decide it."""
    with open(path, "rb") as handle:
        head = handle.readline()
    counts = {s: head.count(s.encode()) for s in (",", ";", "\t", "|")}
    return max(counts, key=lambda s: counts[s])


def is_parquet(path: str) -> bool:
    with open(path, "rb") as handle:
        return handle.read(4) == b"PAR1"


def compared(names_a: list[str], names_b: list[str], key: list[str], ignore: list[str]) -> list[str]:
    in_b = set(names_b)
    skip = set(key) | set(ignore)
    return [c for c in names_a if c in in_b and c not in skip]


def run_duckdb(args, key, ignore) -> dict:
    import duckdb

    con = duckdb.connect()
    if args.threads:
        con.execute(f"SET threads = {int(args.threads)}")

    def source(path: str) -> str:
        if is_parquet(path):
            return f"read_parquet('{path}')"
        sep = separator(path).replace("\t", "\\t")
        return f"read_csv('{path}', all_varchar = true, delim = E'{sep}', header = true)"

    q = ", ".join(f'"{k}"' for k in key)
    for side, path in (("a", args.a), ("b", args.b)):
        con.execute(f"CREATE TABLE raw_{side} AS SELECT * FROM {source(path)}")
        con.execute(f"CREATE TABLE {side} AS SELECT * FROM raw_{side} "
                    f"QUALIFY row_number() OVER (PARTITION BY {q} ORDER BY rowid) = 1")
        con.execute(f"DROP TABLE raw_{side}")
    cols_a = [r[0] for r in con.execute("DESCRIBE a").fetchall()]
    cols_b = [r[0] for r in con.execute("DESCRIBE b").fetchall()]
    cmp = compared(cols_a, cols_b, key, ignore)
    on = " AND ".join(f'a."{k}" = b."{k}"' for k in key)
    distinct = " OR ".join(f'a."{c}" IS DISTINCT FROM b."{c}"' for c in cmp) or "false"
    changed, unchanged = con.execute(
        f"SELECT count(*) FILTER ({distinct}), count(*) FILTER (NOT ({distinct})) "
        f"FROM a JOIN b ON {on}").fetchone()
    added = con.execute(f"SELECT count(*) FROM b ANTI JOIN a ON {on}").fetchone()[0]
    removed = con.execute(f"SELECT count(*) FROM a ANTI JOIN b ON {on}").fetchone()[0]
    if args.rows_out:
        out = Path(args.rows_out)
        out.mkdir(parents=True, exist_ok=True)
        con.execute(f"COPY (SELECT b.* FROM b ANTI JOIN a ON {on}) TO '{out}/added.csv' (HEADER)")
        con.execute(f"COPY (SELECT a.* FROM a ANTI JOIN b ON {on}) TO '{out}/removed.csv' (HEADER)")
        pairs = ", ".join(f'a."{c}" AS "{c} (A)", b."{c}" AS "{c} (B)"' for c in cmp)
        keys = ", ".join(f'a."{k}"' for k in key)
        con.execute(f"COPY (SELECT {keys}, {pairs} FROM a JOIN b ON {on} WHERE {distinct}) "
                    f"TO '{out}/changed.csv' (HEADER)")
    return {"unchanged": unchanged, "changed": changed, "added": added, "removed": removed}


def run_polars(args, key, ignore) -> dict:
    # POLARS_MAX_THREADS is read when the module is imported, so it is set by
    # the caller's environment rather than here.
    import polars as pl

    def read(path: str):
        if is_parquet(path):
            return pl.read_parquet(path)
        return pl.read_csv(path, separator=separator(path), infer_schema=False)

    a = read(args.a).unique(subset=key, keep="first", maintain_order=True)
    b = read(args.b).unique(subset=key, keep="first", maintain_order=True)
    cmp = compared(a.columns, b.columns, key, ignore)
    both = a.join(b, on=key, how="inner", suffix=" (B)")
    differs = pl.any_horizontal([pl.col(c).ne_missing(pl.col(f"{c} (B)")) for c in cmp]) \
        if cmp else pl.lit(False)
    changed_rows = both.filter(differs)
    added_rows = b.join(a, on=key, how="anti")
    removed_rows = a.join(b, on=key, how="anti")
    if args.rows_out:
        out = Path(args.rows_out)
        out.mkdir(parents=True, exist_ok=True)
        added_rows.write_csv(out / "added.csv")
        removed_rows.write_csv(out / "removed.csv")
        changed_rows.select(key + [x for c in cmp for x in (pl.col(c).alias(f"{c} (A)"),
                                                            pl.col(f"{c} (B)"))]) \
            .write_csv(out / "changed.csv")
    return {"unchanged": both.height - changed_rows.height, "changed": changed_rows.height,
            "added": added_rows.height, "removed": removed_rows.height}


def run_pandas(args, key, ignore) -> dict:
    import pandas as pd

    def read(path: str):
        if is_parquet(path):
            return pd.read_parquet(path)
        return pd.read_csv(path, sep=separator(path), dtype=str, keep_default_na=False,
                           na_values=[""])

    a = read(args.a).drop_duplicates(key, keep="first")
    b = read(args.b).drop_duplicates(key, keep="first")
    cmp = compared(list(a.columns), list(b.columns), key, ignore)
    m = a.merge(b, on=key, how="outer", indicator=True, suffixes=(" (A)", " (B)"))
    both = m[m["_merge"] == "both"]
    differs = pd.Series(False, index=both.index)
    for c in cmp:
        x, y = both[f"{c} (A)"], both[f"{c} (B)"]
        differs |= ~((x == y) | (x.isna() & y.isna()))
    added = int((m["_merge"] == "right_only").sum())
    removed = int((m["_merge"] == "left_only").sum())
    if args.rows_out:
        out = Path(args.rows_out)
        out.mkdir(parents=True, exist_ok=True)
        b[b.set_index(key).index.isin(m.loc[m["_merge"] == "right_only"].set_index(key).index)] \
            .to_csv(out / "added.csv", index=False)
        a[a.set_index(key).index.isin(m.loc[m["_merge"] == "left_only"].set_index(key).index)] \
            .to_csv(out / "removed.csv", index=False)
        both.loc[differs, key + [f"{c} ({s})" for c in cmp for s in "AB"]] \
            .to_csv(out / "changed.csv", index=False)
    changed = int(differs.sum())
    return {"unchanged": len(both) - changed, "changed": changed,
            "added": added, "removed": removed}


TOOLS = {"duckdb": run_duckdb, "polars": run_polars, "pandas": run_pandas}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tool", choices=sorted(TOOLS))
    parser.add_argument("a")
    parser.add_argument("b")
    parser.add_argument("--key", required=True)
    parser.add_argument("--ignore", default="")
    parser.add_argument("--json", required=True, help="write the four counts here")
    parser.add_argument("--rows-out", default=None, help="also write the rows into this directory")
    parser.add_argument("--threads", type=int, default=None)
    args = parser.parse_args(argv)
    key = [k for k in args.key.split(",") if k]
    ignore = [c for c in args.ignore.split(",") if c]
    counts = TOOLS[args.tool](args, key, ignore)
    Path(args.json).write_text(json.dumps({k: int(v) for k, v in counts.items()}))
    # The same exit codes as the ports, so the harness reads both alike.
    return 1 if counts["changed"] or counts["added"] or counts["removed"] else 0


if __name__ == "__main__":
    os.environ.setdefault("PYTHONHASHSEED", "0")
    sys.exit(main(sys.argv[1:]))
