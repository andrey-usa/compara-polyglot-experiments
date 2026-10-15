# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `compara-gen-data --format tsv` writes the CSV rows tab-separated, so a TSV
  pair can be benchmarked against the same rows in CSV. On a 1M-row pair TSV
  is within 5% of CSV on every engine (`turbo` 0.15 s against 0.14 s,
  `native` 1.33 s against 1.28 s, `sortmerge` 2.07 s against 1.98 s, four
  threads) and gives the same answer and export files, which
  `tests/formats.rs` now holds every engine to.

### Changed

- `--export-dir` on two Parquet files runs on the columnar path. Before, it
  stopped with "--export-dir is not supported on the parquet path" unless
  `--engine turbo` was given, which reads the files row by row: on a 1M-row
  pair with 60,000 changed rows that took 1.48 s and 766 MB, now 0.31 s and
  363 MB; at 5M rows 11.3 s and 3.8 GB, now 1.8 s and 1.7 GB. The files are
  the same, byte for byte.
- `--export-dir` without `-o` no longer gathers the report's rows, which
  nothing then reads.
- Two Parquet files compressed with gzip, zstd or LZ4 are compared on the
  columnar path, as snappy and uncompressed ones were. Before, any of them
  sent the pair through the row-by-row reader, and zstd is what polars writes
  by default: a 2M-row pair it wrote took 1.14 s at two threads, now 0.39 s,
  what the same pair written with snappy takes.

## [0.5.1] - 2026-10-14

### Changed

- `compare` writes the HTML report only with `-o`/`--out`. Without an output
  flag it prints the counts and writes nothing; before, it wrote
  `<a>__vs__<b>.html` into the working directory. `--json` and
  `--export-dir` write their files on their own, and `--no-compress` without
  `-o` is an error.
- `--engine sortmerge` or `--engine native` on a Parquet or JSON input exits 2
  with a message. Before, a Parquet pair took the columnar path anyway and
  the report said `parquet`.
- `--memory` and `COMPARA_MEMORY` apply to two Parquet files: they decide how
  many columns are read at once, and a pair that needs more than a stated
  budget is refused before anything is read. Before, the columnar path
  ignored them.
- Two Parquet files with boolean, integer, float, date, time, timestamp or
  decimal columns are compared on the columnar path, a word per value. Before,
  one such column sent the pair through the row-by-row reader: a 1M-row file
  written by DuckDB took 1.9 s and 4.5 s of CPU, now 0.4 s and 1.0 s.
- `native` holds rows packed and indexes keys without copying them: a 1M-row
  pair took 27 s and 2.9 GB, now 1.6 s and 0.7 GB; 10M rows take 33 s and
  6.7 GB, where 72 s and 25.7 GB were reported.
- `sortmerge` reads and sorts the two files at once and merges with a heap:
  a 1M-row pair took 9.8 s, now 2-3 s; 10M rows 209 s, now 35 s, in 260 MB.

### Fixed

- A CSV or JSON side compared with a Parquet side showed changed cells that
  held the same value: `31.40` against the DOUBLE 31.4, `12345` against the
  DECIMAL 12345.00, `2026-09-01 02:15:00` against the timestamp
  `2026-09-01T02:15:00`. A column Parquet stores as numbers now compares by
  exact value, and a temporal one by instant; a 100k-row CSV against its own
  Parquet export went from 100,000 changed rows to none.
- A UTF-8 byte order mark at the start of a CSV or NDJSON file became part of
  the first column's name in the default engine (`key column(s) missing`),
  and made an NDJSON file read as CSV.
- The weekly contender benchmark now holds DuckDB and pandas to compara's
  counts on the same question: it counted duplicate keys per row, read empty
  strings as NULL and did not map `unchanged`.

Hostile Parquet and NDJSON files are refused with exit 2 and a message,
where before a few bytes could crash or exhaust the process (found by
mutation fuzzing; each case is a test in `tests/hostile.rs`):

- a zstd page whose stream expands past its header's size was decompressed
  in full first: a 133 KB file used 8 GB of memory. Decompression now
  stops one byte past the declared size (zstd, gzip, snappy, LZ4).
- a footer nested 200,000 structs deep overflowed the stack; nesting past
  64 levels is refused.
- a list count larger than the footer could hold sized an allocation of
  up to 100 GB (abort) or, for lists of bools, looped for hours.
- a string length, page size or value count near 2^64 or negative wrapped
  the bounds checks and panicked.
- a `DELTA_BINARY_PACKED` header claiming a 10^16-value miniblock, and an
  RLE run of billions of values, were materialized up front (abort).
- an NDJSON file cut off right after a key's opening quote panicked
  `head` and `compare`.
- `--no-compress` reports: a value holding `<!--<script>` kept the
  report's script from running (a blank page). Every `<` in the embedded
  JSON is now written `\u003c`.

### Security

- Release workflow: read-only token by default, write access only for the
  job that creates the release; signed build provenance for the archives;
  every action pinned to a commit. CI runs `cargo audit`.

## [0.5.0] - 2026-10-09

First release as a standalone project.

### Added

- `compare`, `columns` and `head` over CSV, newline-delimited JSON and Parquet,
  with either side in any format.
- Three engines with one result contract: `turbo` (default), `sortmerge`
  (bounded memory, spills to disk) and `native`.
- A self-contained HTML report, a JSON summary (`--json`), uncapped CSV exports
  (`--export-dir`) and `--summary` for counts only.
- Profiles in `compara.toml` for recurring comparisons.
- `compara --version`.
- `compara-gen-data --row-group-size` and `--dict-limit` for the Parquet
  layout.
- Prebuilt binaries for Linux (x86_64, aarch64; glibc 2.28+), macOS (arm64,
  x86_64) and Windows (x64); packages for crates.io, npm, PyPI, Homebrew and
  Scoop.

### Changed

- The data generator and the benchmark harness are `compara-gen-data` and
  `compara-bench` (the latter behind the `bench` feature).

[0.5.1]: https://github.com/andrey-usa/compara/releases/tag/v0.5.1
[0.5.0]: https://github.com/andrey-usa/compara/releases/tag/v0.5.0
