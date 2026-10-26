#!/usr/bin/env python3
"""Writes the small files the format readers' tests are held to.

compara's Parquet reader decodes its own pages, so the tests
have to cover what a page can actually be: plain and dictionary encodings, the
version-2 delta encodings, five compression codecs, several row groups, nulls,
and the typed columns that have to be rendered as text before they can be
compared. Those are all *writer* choices, which is why these are written by
pyarrow rather than by this project -- a reader tested only against files its own
writer produced tests nothing.

The same 300-row table is written as CSV, TSV and newline-delimited JSON too, so a
test can assert the thing that actually matters: the comparison returns the same
counts whichever format each side arrives in.

The output is committed, because the tests must run without pyarrow installed.
Regenerate with:

    pip install pyarrow
    python3 scripts/make_parquet_fixtures.py tests/fixtures/formats
"""
import csv
import datetime as dt
import decimal
import json
import os
import sys

ROWS = 300


def sides():
    """Two versions of one table, with a known drift between them.

    B changes one column in every fourth row, blanks one value, drops a row, adds
    a row and repeats a key, so the counts a test asserts are not all zero and
    each of them is reached by a different path through the engine.
    """
    a = {
        "id": [f"K-{i:04d}" for i in range(ROWS)],
        "status": [["open", "closed", "pending", "settled"][i % 4] for i in range(ROWS)],
        "amount": [f"{i * 3}.{i % 100:02d}" for i in range(ROWS)],
        "note": ["" if i % 7 == 0 else f"line {i}" for i in range(ROWS)],
        "maybe": [None if i % 5 == 0 else f"v{i % 11}" for i in range(ROWS)],
    }
    b = {k: list(v) for k, v in a.items()}
    for i in range(ROWS):
        if i % 4 == 0:
            b["status"][i] = "reversed"
        if i % 50 == 0:
            b["amount"][i] = ""
    # A row only in B, then a key repeated within B.
    for column, extra in (("id", "K-9000"), ("status", "open"), ("amount", "1.00"),
                          ("note", "added"), ("maybe", "v1")):
        b[column].append(extra)
    for column in b:
        b[column].append(b[column][0])
    # A row only in A.
    for column in a:
        a[column].append("K-9999" if column == "id" else "only-in-a")
    return a, b


def word_sides():
    """Two versions of a table of word-sized columns.

    B changes some value in every column on its own rows, nulls some and fills
    others (a null against a 0 among them), writes -0.0 where A has 0.0 and NaN
    where A has NaN (both the same value as text), and has one row A lacks,
    lacks one A has, and repeats a key.
    """
    rows = 400
    nan, inf = float("nan"), float("inf")
    base = dt.datetime(2026, 1, 1)
    a = {
        "id": [10_000_000_000 + i for i in range(rows)],
        "name": [None if i % 37 == 0 else f"n{i % 50}" for i in range(rows)],
        "flag": [None if i % 23 == 0 else i % 3 == 0 for i in range(rows)],
        "small": [(i * 7919) % 2001 - 1000 for i in range(rows)],
        "level": [i % 5 for i in range(rows)],
        "count": [None if i % 11 == 0 else i * 1_000_003 - 2**40 for i in range(rows)],
        "wide": [(i * 2_654_435_761) % 2**32 for i in range(rows)],
        "ratio": [[0.0, nan, inf, -inf, None][i] if i < 5 else (i - 200) / 8 for i in range(rows)],
        "single": [[0.0, nan][i] if i < 2 else (i % 40) / 4 for i in range(rows)],
        "day": [dt.date(1969, 12, 1) + dt.timedelta(days=i * 13) for i in range(rows)],
        "at_ms": [base + dt.timedelta(milliseconds=i * 1_001) for i in range(rows)],
        "at_us": [None if i % 17 == 0 else base + dt.timedelta(microseconds=i * 999_999)
                  for i in range(rows)],
        "at_ns": [base + dt.timedelta(seconds=i * 61) for i in range(rows)],
        "clock": [dt.time(i % 24, i % 60, i % 60, (i * 1000) % 1_000_000) for i in range(rows)],
        "money": [None if i % 19 == 0 else decimal.Decimal(i * 1001 - 200_000) / 100
                  for i in range(rows)],
    }
    b = {k: list(v) for k, v in a.items()}
    b["ratio"][0], b["single"][0] = -0.0, -0.0  # the same value as 0.0
    b["ratio"][1], b["single"][1] = nan, nan     # NaN reads as NaN either way
    changes = {
        "name": lambda i, v: None if i % 41 == 0 else v,
        "flag": lambda i, v: (not v) if v is not None and i % 29 == 0 else v,
        "small": lambda i, v: v + 1 if i % 31 == 0 else v,
        # A null against a zero, both ways round: equal words, unequal cells.
        "level": lambda i, v: None if i % 89 == 0 else (v + 1) % 5 if i % 43 == 0 else v,
        "count": lambda i, v: 0 if v is None and i % 22 == 0 else v,
        "wide": lambda i, v: (v + 2**31) % 2**32 if i % 47 == 0 else v,
        "ratio": lambda i, v: v + 0.25 if i > 4 and i % 13 == 0 else v,
        "single": lambda i, v: v + 0.5 if i > 1 and i % 53 == 0 else v,
        "day": lambda i, v: v + dt.timedelta(days=1) if i % 59 == 0 else v,
        "at_ms": lambda i, v: v + dt.timedelta(milliseconds=1) if i % 61 == 0 else v,
        "at_us": lambda i, v: None if i % 67 == 0 else v,
        "at_ns": lambda i, v: v + dt.timedelta(microseconds=1) if i % 71 == 0 else v,
        "clock": lambda i, v: dt.time(v.hour, v.minute, v.second) if i % 73 == 0 else v,
        "money": lambda i, v: v + decimal.Decimal("0.01") if v is not None and i % 79 == 0 else v,
    }
    for column, change in changes.items():
        b[column] = [change(i, v) for i, v in enumerate(b[column])]
    # A row only in B, a key B repeats, and a row only in A.
    for column in b:
        b[column].append(99_999_999_999 if column == "id" else b[column][7])
        b[column].append(b[column][3])
        del b[column][5]
    return a, b


def shortest(value):
    """A float as the ports render it: the shortest digits that read back the
    same, laid out the way JavaScript's number-to-string does it.

    The rule has to be stated somewhere, because "the shortest round trip" does
    not say whether 1e-10 is written out in full. Every port implements this one,
    so a float column compares equal across all of them.
    """
    if value != value or value in (float("inf"), float("-inf")):
        return {float("inf"): "Infinity", float("-inf"): "-Infinity"}.get(value, "NaN")
    if value == 0:
        return "0"
    text = repr(abs(value))
    if "e" in text:
        digits, exponent = text.split("e")
        exponent = int(exponent)
    else:
        digits, exponent = text, 0
    digits = digits.replace(".", "").rstrip("0") or "0"
    point = (text.split("e")[0].find(".") if "." in text else len(text.split("e")[0]))
    n = point + exponent
    k = len(digits)
    sign = "-" if value < 0 else ""
    if k <= n <= 21:
        return sign + digits + "0" * (n - k)
    if 0 < n <= 21:
        return sign + digits[:n] + "." + digits[n:]
    if -6 < n <= 0:
        return sign + "0." + "0" * -n + digits
    tail = digits[0] + ("." + digits[1:] if k > 1 else "")
    return f"{sign}{tail}e{'+' if n > 0 else '-'}{abs(n - 1)}"


def write_csv(path, table, delimiter=","):
    with open(path, "w", newline="") as fh:
        out = csv.writer(fh, delimiter=delimiter, lineterminator="\n")
        out.writerow(table.keys())
        for row in zip(*table.values()):
            out.writerow(["" if v is None else v for v in row])


def write_ndjson(path, table):
    names = list(table)
    with open(path, "w") as fh:
        for row in zip(*table.values()):
            fh.write(json.dumps(dict(zip(names, row))) + "\n")


def main(argv):
    import pyarrow as pa
    import pyarrow.parquet as pq

    out_dir = argv[0] if argv else "tests/fixtures/formats"
    os.makedirs(out_dir, exist_ok=True)
    a, b = sides()
    written = []

    def record(path):
        written.append(path)
        return path

    write_csv(record(os.path.join(out_dir, "a.csv")), a)
    write_csv(record(os.path.join(out_dir, "b.csv")), b)
    write_csv(record(os.path.join(out_dir, "a.tsv")), a, delimiter="\t")
    write_csv(record(os.path.join(out_dir, "b.tsv")), b, delimiter="\t")
    write_ndjson(record(os.path.join(out_dir, "a.ndjson")), a)
    write_ndjson(record(os.path.join(out_dir, "b.ndjson")), b)

    table_a, table_b = pa.table(a), pa.table(b)

    def write(name, table, **kwargs):
        pq.write_table(table, record(os.path.join(out_dir, name)), **kwargs)

    # One file per writer decision the reader has to survive.
    write("a_dict_snappy.parquet", table_a, compression="snappy", use_dictionary=True)
    write("b_dict_snappy.parquet", table_b, compression="snappy", use_dictionary=True)
    write("a_plain_none.parquet", table_a, compression="none", use_dictionary=False)
    write("a_dict_gzip.parquet", table_a, compression="gzip", use_dictionary=True)
    write("a_plain_zstd.parquet", table_a, compression="zstd", use_dictionary=False)
    write("a_plain_lz4.parquet", table_a, compression="lz4", use_dictionary=False)
    # Version 2 pages with the delta encodings, which are a different decoder.
    write("a_delta_v2.parquet", table_a, compression="none", use_dictionary=False,
          version="2.6", data_page_version="2.0",
          column_encoding={"id": "DELTA_BYTE_ARRAY", "status": "DELTA_LENGTH_BYTE_ARRAY",
                           "amount": "DELTA_BYTE_ARRAY", "note": "DELTA_LENGTH_BYTE_ARRAY",
                           "maybe": "DELTA_BYTE_ARRAY"})
    # Several row groups, so a dictionary has to be carried per column chunk.
    write("a_dict_row_groups.parquet", table_a, compression="snappy",
          use_dictionary=True, row_group_size=64)

    # The typed columns: every one of these has to become the text a CSV of the
    # same data would hold. `typed.csv` is that text, written from the same
    # values by this script, so a test compares the reader's rendering against a
    # stated expectation rather than against another copy of itself.
    typed = {
        "id": [f"K-{i:04d}" for i in range(8)],
        "count": [0, 1, -1, 42, 1000, -1000, 2**31 - 1, -(2**31)],
        "big": [0, 1, -1, 2**40, -(2**40), 7, 8, 9],
        "ratio": [0.0, 0.5, -0.25, 1.5, 3.0, 1e10, 1e-10, 2.25],
        "flag": [True, False, True, True, False, False, True, False],
        "day": [dt.date(1970, 1, 1), dt.date(2024, 1, 1), dt.date(2024, 2, 29),
                dt.date(1969, 12, 31), dt.date(2000, 3, 1), dt.date(1999, 12, 31),
                dt.date(2038, 1, 19), dt.date(2026, 9, 7)],
        "at": [dt.datetime(2024, 1, 1, 0, 0, 0), dt.datetime(2024, 1, 1, 12, 34, 56),
               dt.datetime(1970, 1, 1, 0, 0, 0), dt.datetime(2026, 9, 7, 22, 15, 0),
               dt.datetime(1999, 12, 31, 23, 59, 59), dt.datetime(2000, 1, 1, 0, 0, 1),
               dt.datetime(2024, 6, 30, 6, 30, 30), dt.datetime(2024, 12, 25, 18, 45, 15)],
        "money": [0, 1, -1, 12345, -12345, 999999, 100, -100],
    }
    write("typed.parquet", pa.table({
        "id": pa.array(typed["id"]),
        "count": pa.array(typed["count"], pa.int32()),
        "big": pa.array(typed["big"], pa.int64()),
        "ratio": pa.array(typed["ratio"], pa.float64()),
        "flag": pa.array(typed["flag"]),
        "day": pa.array(typed["day"]),
        "at": pa.array(typed["at"], pa.timestamp("us")),
        "money": pa.array(typed["money"], pa.decimal128(12, 2)),
    }), compression="snappy")

    def as_text(name, value):
        """The text the reader is required to render this value as."""
        if name == "flag":
            return "true" if value else "false"
        if name == "day":
            return value.isoformat()
        if name == "at":
            # Microsecond timestamps, with a zero fraction dropped.
            return value.isoformat().replace("+00:00", "")
        if name == "money":
            # pyarrow reads a Python int for a decimal(12, 2) column as that many
            # whole units, so 12345 is stored as 12345.00 rather than as 123.45.
            return f"{value}.00"
        if name == "ratio":
            return shortest(value)
        return str(value)

    write_csv(record(os.path.join(out_dir, "typed.csv")),
              {name: [as_text(name, v) for v in values] for name, values in typed.items()})

    # A pair stored as words: every word-sized type the columnar path keeps as
    # one integer per row, with nulls, both zeros, NaN and infinities, and drift
    # between the sides, so a test can hold its answer to the row-by-row one.
    wa, wb = word_sides()
    schema = pa.schema([
        ("id", pa.int64()), ("name", pa.string()), ("flag", pa.bool_()),
        ("small", pa.int32()), ("level", pa.int32()), ("count", pa.int64()),
        ("wide", pa.uint32()), ("ratio", pa.float64()), ("single", pa.float32()),
        ("day", pa.date32()), ("at_ms", pa.timestamp("ms")),
        ("at_us", pa.timestamp("us", tz="UTC")), ("at_ns", pa.timestamp("ns")),
        ("clock", pa.time64("us")), ("money", pa.decimal128(12, 2)),
    ])
    words_a, words_b = pa.table(wa, schema=schema), pa.table(wb, schema=schema)
    # Dictionary pages and snappy, as DuckDB and pyarrow write by default; and
    # plain uncompressed pages, with the decimals stored as integers, as DuckDB
    # writes a DECIMAL of up to eighteen digits.
    write("words_a.parquet", words_a, compression="snappy", row_group_size=150)
    write("words_b.parquet", words_b, compression="snappy", row_group_size=150)
    write("words_a_plain.parquet", words_a, compression="none", use_dictionary=False,
          store_decimal_as_integer=True)
    write("words_b_plain.parquet", words_b, compression="none", use_dictionary=False,
          store_decimal_as_integer=True)
    # And zstd with dictionary pages, which is what polars writes by default.
    write("words_a_zstd.parquet", words_a, compression="zstd", row_group_size=150)
    write("words_b_zstd.parquet", words_b, compression="zstd", row_group_size=150)
    # The same B with one column stored wider, which the columnar path does not
    # compare word for word with A's.
    write("words_b_wider.parquet", words_b.cast(schema.set(3, pa.field("small", pa.int64()))),
          compression="snappy")

    for path in written:
        print(f"{path}  {os.path.getsize(path):,} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
