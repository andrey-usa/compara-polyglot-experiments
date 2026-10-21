#!/usr/bin/env python3
"""One keyed comparison done by PyArrow, the way its users do it -- summary
counts only (added / removed / changed / unchanged), no diff rows written.

This is the `contenders.py` contract (same --key/--json interface, same exit
codes) for the tool that file does not cover. The comparison is three joins,
which is what a PyArrow user writes for this job:

  * changed / unchanged: an inner join on the key, then a per-column
    not-distinct-from test across the compared columns;
  * added: a left-anti join of B against A; removed: of A against B.

Rules, held the same as the ports and `contenders.py`: a compared cell equals
another when both are missing or both hold the same value. Parquet keeps its
types; CSV is read with PyArrow's own inference (its CSV reader has no
all-text mode a user would normally reach for).

The size-ladder fixtures have unique keys by construction (the generator
permutates distinct ids), so the plain joins here are exact -- contenders.py's
first-row dedup rule for duplicate keys never fires on them, and the ladder's
per-cell count gate across all three tools would catch it if it did.

  python3 scripts/contenders_pyarrow.py A B --key id,k2 --json out.json [--threads N]
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def is_parquet(path: str) -> bool:
    with open(path, "rb") as handle:
        return handle.read(4) == b"PAR1"


def read(path: str):
    import pyarrow.csv as pcsv
    import pyarrow.parquet as pq

    if is_parquet(path):
        return pq.read_table(path)
    return pcsv.read_csv(path)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("a")
    parser.add_argument("b")
    parser.add_argument("--key", required=True)
    parser.add_argument("--ignore", default="")
    parser.add_argument("--json", required=True, help="write the four counts here")
    parser.add_argument("--threads", type=int, default=None)
    args = parser.parse_args(argv)

    import pyarrow as pa
    import pyarrow.compute as pc

    if args.threads:
        pa.set_cpu_count(args.threads)
        pa.set_io_thread_count(args.threads)

    key = [k for k in args.key.split(",") if k]
    ignore = {c for c in args.ignore.split(",") if c}
    a, b = read(args.a), read(args.b)
    cmp_cols = [c for c in a.column_names
                if c in b.column_names and c not in key and c not in ignore]

    both = a.join(b, keys=key, join_type="inner",
                  left_suffix=" (A)", right_suffix=" (B)")
    differs = None
    for c in cmp_cols:
        x, y = both[f"{c} (A)"], both[f"{c} (B)"]
        eq = pc.fill_null(pc.equal(x, y), False)
        both_missing = pc.and_(pc.is_null(x), pc.is_null(y))
        col_differs = pc.invert(pc.or_(eq, both_missing))
        differs = col_differs if differs is None else pc.or_(differs, col_differs)
    changed = int(pc.sum(pc.cast(differs, pa.int64())).as_py()) if differs is not None else 0
    added = b.join(a, keys=key, join_type="left anti").num_rows
    removed = a.join(b, keys=key, join_type="left anti").num_rows

    counts = {"unchanged": both.num_rows - changed, "changed": changed,
              "added": added, "removed": removed}
    Path(args.json).write_text(json.dumps(counts))
    return 1 if changed or added or removed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
