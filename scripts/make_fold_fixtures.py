#!/usr/bin/env python3
"""Writes the pair `scripts/option_matrix.py` holds --ignore-case to outside ASCII.

C, C++ and Zig fold case in ASCII only. They refuse a value outside it by name, but
only where the fold decides the answer: in a key, or in a compared value that
differs byte for byte between two matched rows. Rust folds Unicode and answers
everything. So on one pair the ports must refuse or answer according to which
columns take part, and where those three answer they must give Rust's counts.
Generated data is pure ASCII and never reaches this, which is how the ports came
to disagree on it unnoticed.

Every row is one of the places the rule draws a line:

    k1  ASCII differing only in case          equal under --ignore-case
    k2  the same value outside ASCII          unchanged: answered, not refused
    k3  empty in A, outside ASCII in B        a difference, but not one of case
    k4  `note` differs outside ASCII          refused, unless `note` is ignored
    k5  only in A, outside ASCII              a sample, never compared
    k6  only in B, outside ASCII              likewise

`name` holds a value outside ASCII in most rows, so `-k id,name` is a key that
must be refused. `amount` and `tag` exercise --tolerance and --trim on the same
rows.

`pad` holds values that are only whitespace, which generated data never does.
Under --trim such a value is "", and "" is a value; only --empty-is-null makes
it absent. Rust's and Zig's Parquet paths read it as absent under --trim alone,
so the same rows gave a different answer as Parquet than as CSV:

    k1  spaces in A, null in B       a change under --trim, none with --empty-is-null
    k2  spaces in A, "" in B         the same in CSV; in Parquet "" is not null
    k3  spaces in A and in B         equal under --trim, whatever their length

The output is committed, because the tests must run without pyarrow installed.
Regenerate with:

    pip install pyarrow
    python3 scripts/make_fold_fixtures.py tests/fixtures/fold
"""
import os
import sys

from make_parquet_fixtures import write_csv, write_ndjson

COLUMNS = ("id", "name", "note", "amount", "tag", "pad")
A = [
    ("k1", "Alpha", "plain", "10.0", "x", "  "),
    ("k2", "CAFÉ", "same", "20.0", "y", "  "),
    ("k3", None, "left", "30.0", "z", "  "),
    ("k4", "Delta", "Crème", "40.0", "w", "p"),
    ("k5", "naïve", "gone", "50.0", "v", "p"),
]
B = [
    ("k1", "ALPHA", "plain", "10.3", " x ", None),
    ("k2", "CAFÉ", "same", "20.0", "y", ""),
    ("k3", "Été", "left", "30.0", "z", "   "),
    ("k4", "delta", "crème", "40.0", "w", "p"),
    ("k6", "Ñandú", "new", "60.0", "u", "p"),
]


def table(rows):
    return {c: [r[i] for r in rows] for i, c in enumerate(COLUMNS)}


def main(argv):
    import pyarrow as pa
    import pyarrow.parquet as pq

    out_dir = argv[0] if argv else "tests/fixtures/fold"
    os.makedirs(out_dir, exist_ok=True)
    written = []
    for side, rows in (("a", A), ("b", B)):
        t = table(rows)
        for name, write in ((f"{side}.csv", write_csv), (f"{side}.ndjson", write_ndjson)):
            write(os.path.join(out_dir, name), t)
            written.append(name)
        # Both encodings: C++ and Zig compare a dictionary column through ids
        # built by folding every entry up front, and a plain one cell by cell.
        for enc, dictionary in (("dict", True), ("plain", False)):
            name = f"{side}_{enc}.parquet"
            pq.write_table(pa.table(t), os.path.join(out_dir, name), compression="none",
                           use_dictionary=dictionary)
            written.append(name)
    for name in written:
        print(f"{os.path.join(out_dir, name)}  {os.path.getsize(os.path.join(out_dir, name)):,} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
