# csvdiff — C++

The byte-level comparison, in the language people reach for when they want the
fastest thing they can read. Same design as the C, Rust and Zig `turbo` engines:
the file is mapped, a field is an offset and a length packed into one word,
delimiters are found eight bytes at a time with SWAR, and nothing becomes a
`std::string` unless it reaches the report.

It reads three formats on that one representation: CSV, newline-delimited JSON,
and Parquet. CSV and JSON compare against each other, because both reduce a field
to bytes in the mapping. Parquet compares against Parquet on a different path
entirely — [columnar, and never reconstructing a
row](../FINDINGS.md#techniques-worth-it-and-not) — which is 4.8x faster than the same
data as CSV.

```bash
make                     # g++ by default
CXX=clang++ make         # or clang
build/csvdiff compare a.csv b.csv -k id --json summary.json
build/csvdiff compare a.parquet b.parquet -k account_id,txn_id --threads 4
CSVDIFF_PHASES=1 build/csvdiff compare a.parquet b.parquet -k id   # where the time goes

make gen-data            # the benchmark generator: CSV, or Parquet directly
build/gen-data --rows 10m --out-dir data --format parquet --compression snappy

./test.sh                # against the Rust port, and Parquet against its own CSV
```

## What it is and is not

This is a **benchmark and parity port**: it carries the comparison and the JSON
half of the result contract, not the HTML report — the Rust port is the only one
that renders it; what is interesting here is the engine.

Two limitations, both stated rather than papered over:

- **`--ignore-case` is ASCII-only.** Folding case outside ASCII needs a Unicode
  table this port does not carry, and folding it partially is worse than not
  folding it at all: `CAFÉ` and `café` would compare equal in the ports that do
  fold and unequal here, with nothing in the output to say why. So a non-ASCII
  byte is refused by name wherever the fold would decide the answer: in a key,
  or in a compared value that differs byte for byte between two matched rows.
  Anywhere else the fold changes nothing and the run is answered: a value that
  did not change, a row only one side has, a value whose mate is empty, a
  column left out of the comparison. `--json` does not change which it is.
- **No `--export-dir` and no `--profile`.** Neither affects the numbers.
- **The Parquet reader implements what this job meets and refuses the rest by
  name.** `BYTE_ARRAY` columns, PLAIN and dictionary encodings, uncompressed and
  snappy, data page v1. Nested columns, other types, other codecs and page v2 are
  errors that say which. Both sides of a comparison must be Parquet.

## The scanner is a build option

`make` builds the SWAR scanner, which needs no CPU feature at all. `make
scanners` builds three binaries — SWAR, AVX2 and AVX-512 — so the instruction set
can be measured without a runtime switch in the way:

```bash
make scanners
build/csvdiff-avx2 compare a.csv b.csv -k id --threads 4
```

At ten million rows on a hosted runner with AVX2, the vector scanner is about 7%
ahead of SWAR. That reverses what this repository used to say, and the reason is
in the root README: the key hash used to dominate the run, and it does not any
more.

## The compiler is worth more than the language

On a million rows, best of three, one 4-core container:

| Build | Compare | Peak RSS |
|---|---:|---:|
| `clang++ 18` | **3.64s** | 509 MB |
| `g++ 13` | 6.27s | 509 MB |

Same source, same flags (`-std=c++20 -O2 -march=native`), **1.7x apart**. For
comparison, Rust running the same design lands at 5.25s and C at 4.77s — so
whether this port is the fastest thing in the project or the slowest of them all
is decided by which compiler built it, not by the language it is written in.

That is worth knowing before reading any single-number language comparison,
including the ones in this repository's own README.

## Layout

| File | What it holds |
|---|---|
| `src/csvdiff.hpp` | the contract: options, counts, column stats, result |
| `src/csvdiff.cpp` | SWAR scanning, the mapped slab, the CSV and JSON parsers, the index, the join |
| `src/parquet.hpp` `src/parquet.cpp` | a Parquet reader shaped for comparing: Thrift footer, page decoder, RLE/bit-packed hybrid, snappy |
| `src/pqdiff.hpp` `src/pqdiff.cpp` | the columnar comparison — key join first, then one column at a time, on shared dictionary ids |
| `src/json.cpp` | the JSON half of the result contract, written by hand |
| `src/main.cpp` | the command line; exit 0 identical, 1 differences, 2 error |
| `tools/pq_dump.cpp` | `make pq-dump` — dumps a Parquet file's schema, or one column as text, to check the reader against whatever wrote the file |
| `tools/gen_data.cpp` | `make gen-data` — the benchmark generator: the same CSV bytes as the other two, or the same rows written straight to Parquet |
| `tools/pq_write.hpp` `tools/pq_write.cpp` | the Parquet writer the generator uses — Thrift footer, dictionary and plain pages, RLE/bit-packing, snappy. Test scaffolding, not part of the engine |
