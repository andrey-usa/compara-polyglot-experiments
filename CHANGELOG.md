# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- Delta-encoded Parquet integers whose deltas need more than 32 bits (an
  INT64 column with large jumps between rows) lost their high bits and were
  read as wrong values; they are read whole now.
- At a tight heap limit (a container's, or `ulimit -d`) a run could abort
  (SIGABRT) instead of refusing with exit 2: while one side's checked
  allocation met the ceiling, the other side's thread could meet it with a
  small one. The reserve kept for the refusal now covers that too (on ARM,
  6 aborts in 4,800 runs at 4 MB before, none after).

### Security

- A Parquet page that claims more values, or more uncompressed bytes, than
  its stored bytes can hold by the decompression-bomb rule (past 1 GB, more
  than 1000 times) is refused before anything is sized from it
  (`COMPARA_MAX_EXPANSION`). Found by fuzzing: a 9.7 KB file had 2.8 GB
  reserved for a page that claimed 1.47 billion values.

- A Parquet page that claims a bit width wider than its values (found by
  fuzzing) is refused with a message; a debug build panicked on it and a
  release one read garbage.

- A gzip or zstd input that expands past 1 GB to more than 1000 times its
  size is refused as a decompression bomb; it was expanded whole into
  `TMPDIR` first (`COMPARA_MAX_EXPANSION=<times>`, 0 for no limit).
- `http(s)://` inputs and their redirects no longer reach link-local
  addresses (cloud metadata services: 169.254.169.254, fe80::/10,
  fd00:ec2::254), by address or by name (`COMPARA_ALLOW_LINK_LOCAL=1`
  allows it); redirects stop after 10, and a refused one says why.

## [0.6.0] - 2026-10-26

Tables compared where they are: in S3, Google Cloud Storage and Azure, on
the web, as directories and globs of parts, and as Delta Lake tables at a
version. A container image and a GitHub Action. Faster on every format,
and inside a container's memory limit a refusal that says why rather than
an OOM kill.

### Added

- Inputs from object storage and the web: `s3://`, `gs://`, `az://` (and
  `adl`, `azure`, `abfs`, `abfss`), `http://` and `https://`, wherever a
  file is accepted. Credentials come from each cloud's own environment --
  `AWS_*` and `~/.aws` profiles (keys, session tokens, `AWS_ENDPOINT_URL`
  for MinIO and the like, web identity for EKS, ECS task roles, instance
  metadata), `GOOGLE_*` and application default credentials, `AZURE_*`
  (account keys, SAS, service principals, managed and workload identity)
  -- and a presigned or SAS URL is read as it is. Objects are read in 8 MB
  ranges, sixteen at a time (`COMPARA_FETCH_CONCURRENCY`), into a temporary
  file. A Parquet object is read only as far as the comparison reads it:
  its footer and the column chunks of the key and compared columns, so
  `--compare` on two columns of a wide table fetches those two. `columns`
  on a remote Parquet file reads its last 64 KB. The `cloud` feature is on
  by default; without it the binary reads local files only. Every range is
  read from the version whose first bytes were read (`If-Match` on its
  ETag), so an object overwritten mid-read is an error rather than a file
  stitched from two versions; throttled and failed ranges are retried.
  A plain HTTP server that ignores ranges sends the object whole; it is
  written to disk as it arrives (the temporary directory's room checked
  first), never held in memory.
- A download that takes a while says how far it has got every ten seconds
  on stderr, so a CI job's or a pod's log shows it moving
  (`COMPARA_PROGRESS` sets the seconds; 0 turns it off).
- Outputs to the same places: `-o`, `--json` and `--export-dir` take an
  `s3://`, `gs://` or `az://` URL (or a presigned `https://` one), written
  in one request or as a multipart upload of 16 MB parts. `--json -` prints
  the summary on standard output, and the counts line goes to standard
  error, so a pipeline or a CI step reads one JSON document.
- A directory, a quoted glob (`'out/date=*/*.parquet'`, `**` for any depth)
  or an object storage prefix (`s3://bucket/orders/`, or the same without the
  slash when no object has that name) is a side: its files, read as one table
  in path order. CSV and TSV parts must share a header, Parquet parts a
  schema (names, order, types, nullability), and the error names the part
  and the difference; compressed parts are expanded on the way. Remote parts
  are fetched with sixteen ranges in flight across all of them, Parquet
  parts only as far as the comparison reads them. Hive partition
  directories (`date=2026-10-01/`) are columns of the table, as text, unless
  the files already have them: a Parquet part gets a column chunk of one
  dictionary value per row group, a CSV or JSON part the value in every row.
  `.`- and `_`-prefixed names are passed over; Iceberg tables are refused.
- Delta Lake tables, local or in object storage, at their latest version or
  at `@vN` (`s3://lake/orders@v41`): the files the log says make that
  version, from its last checkpoint and the commits after it, so two
  versions of a table compare like two tables. Deletion vectors, column
  mapping, V2 checkpoints and a log cleaned up past the version asked for
  are refused with what to do instead; a Delta table further down a
  directory is named rather than read as loose files.
- Credentials as each cloud's own tools find them, laptops included: an AWS
  profile that uses SSO, an assumed role or a `credential_process` is
  resolved as the AWS CLI resolves it; an `az login` is used when Azure has
  nothing else; and with no credentials at all a public bucket is read
  unsigned rather than refused.
- `--exit-zero`: exit 0 when the tables differ, for a scheduled job whose
  output is the report -- a Kubernetes Job, an Airflow task -- where exit 1
  would mark the run failed.
- `-` for either file reads standard input.
- Gzip and zstd files are read as what they hold, by their first bytes
  rather than their names (`.csv.gz`, `.ndjson.zst`); bzip2, xz and zip are
  named and refused rather than read as a CSV.
- `compara env`: the CPUs, memory, container limit, heap limit and
  temporary directory this process sees, with the room left in it, and
  where each cloud's credentials would come from (keys, a profile, web
  identity, a CLI sign-in, metadata) -- by what is set, nothing fetched. A
  download or a table of parts that the temporary directory has no room for
  is refused before it starts, with how much it needs and where.
- A container image, `ghcr.io/andrey-usa/compara`: the release binary on
  distroless for `linux/amd64` and `linux/arm64`, non-root, with build
  provenance and an SBOM.
- A GitHub Action (`uses: andrey-usa/compara@v0`): installs the release
  binary, compares, writes the counts and the most-changed columns to the job
  summary, and fails the step on differences; its outputs are the counts and
  the JSON summary's path.

### Changed

- A closed pipe ends the process quietly, as it ends `cat`: `compara head
  big.csv | head -1` used to print an internal error.
- Faster on every format and thread count. On 2M-row pairs, 4-core GitHub
  Actions runners, best of five (CI run 39728278163), the time against the
  C port of the same tool: CSV 0.75x at one thread and 0.83x at four, TSV
  0.74x and 0.81x, NDJSON 0.52x and 0.67x, Parquet 0.57x and 0.73x.
  - Quoted CSV: the row end and the join's register count follow quotes by
    parity, a register at a time, instead of a scan per quoted field and a
    field walk per quoted row. On the same 2M rows with their text fields
    quoted, 0.61x the C port's time at one thread and 0.63x at four (CI run
    39729954571).
  - Parquet: the join reads each key column through a form settled once per
    column, and compares short keys by a load or two instead of `memcmp`:
    the match sweep runs in half the instructions.
  - Buffers of 4 MB or more are advised onto transparent huge pages, so
    filling them takes a page fault per 2 MB rather than per 4 KB: on the
    2M-row Parquet pair 75,651 faults become 16,889.
  - NDJSON rows are proved equal before they are parsed, and the CSV join's
    register count, key hash and index insert do less per row.
- Inside a container with a memory limit, the heap is held under that limit
  (`RLIMIT_DATA`, a sixteenth below it), so running out of memory is a
  refusal that says what needed how much -- exit 2 -- rather than an OOM kill
  with no message (exit 137). Under `--engine auto` a CSV pair whose indexes
  do not fit is then compared by the `sortmerge` engine, which spills to
  disk, with the same answer. `COMPARA_CONTAIN=0` turns this off. In a
  container, files being written -- spilled runs, downloads, expanded gzip
  -- are handed to the disk as they go rather than left dirty in the page
  cache, where they count against the same limit: a 160 MB container whose
  heap stayed under 120 MB was OOM-killed by them.
- `sortmerge` sizes its batches from the heap it may have, and builds the
  report's rows only when there is a report: a summary run of a 2M-row CSV
  pair now completes in a 40 MB heap (`ulimit -d`), where it failed in
  100 MB. Named with `--engine sortmerge` in a heap under 24 MB, it is
  refused before it starts rather than aborted midway; under 48 MB `auto`
  doesn't fall back to it on its own, and its refusal says that
  `--engine sortmerge` tries it.
- The README has a table of comparisons read from S3 against DuckDB's
  `httpfs` (CI run 39793328982): at 10M rows a side, 2.7 times faster on a
  Parquet pair, 2.8 on CSV, 6.0 on a table of Parquet parts, in a third to
  a half of its memory.
- The README's speed table is rerun on this code (CI runs 39728430053 and
  39729034092): at ten million rows and four threads, CSV 1.20 s to 1.08 s,
  NDJSON 2.79 s to 1.26 s, Parquet 1.41 s to 1.17 s. Its input column was
  labelled "a side" but held both files; it says so now.

## [0.5.3] - 2026-10-22

Two Parquet files compared up to 2.5 times faster, JSON files that are one
top-level array, and two silent wrong answers on JSON input fixed.

### Added

- A JSON file can be one top-level array of objects, compact or
  pretty-printed, as well as one object per line: the rows are the array's
  objects and the columns the keys of the first one. Newline-delimited JSON
  reads as fast as before.

### Fixed

- A JSON value that is an object or an array was read as absent, so two
  different nested values compared as the same missing value and the change
  went unreported (0.5.2 and earlier). It is now compared as its JSON text,
  as written, and the report and export show that text. The README says what
  that means for key order and spacing, and when to flatten first.
- A pretty-printed top-level JSON array whose rows hold objects on lines of
  their own lost rows when read on several threads: a split guessed at the
  next line opening an object could land inside a row, and the rows read
  from there were dropped without a word (200,000 rows counted as 100,718
  at four threads). Splits are now checked as CSV splits are, and redone on
  bounds that follow the array's depth when a guess was wrong; the guess
  itself takes the column the first row opens at. Top-level arrays were new
  in 0.5.3-alpha.1; 0.5.2 did not read them.

### Changed

- Two Parquet files compare faster on the columnar path. Snappy pages are
  expanded with whole-word copies; a column's row groups are read in runs on
  threads of their own, both sides at once, so a file with fewer columns
  than cores no longer leaves cores idle; a slot of the key index holds its
  row, so a match costs one cache miss less, the cells a probe compares are
  prefetched, and integer, date and other word keys are hashed a column at a
  time; the compared columns go largest first and each is compared on
  several threads; the footer is parsed once per file. In CI (run
  39468615515, a 4-core runner, a 5M-row pair written by pyarrow, counts
  only) the comparison took 1.26 s at one thread, 0.79 s at two and 0.56 s
  at four, against 2.34 s, 1.53 s and 1.40 s for 0.5.3-alpha.1; DuckDB took
  6.73 s, 3.94 s and 3.13 s and Polars 4.98 s, 3.32 s and 2.73 s on the same
  runner. With the changed rows written out: 1.29 s, 1.09 s and 0.72 s,
  against 2.43 s, 2.19 s and 1.90 s. Answers are the same; a footer the
  row-group runs cannot trust is read in one pass, as before.
- `--export-dir` on two Parquet files keeps the decoded compared columns for
  the export when the memory budget has room for them, instead of reading
  each column a second time.
- The key index is built on one thread unless `COMPARA_PARALLEL_INSERT` is
  set: the parallel build, taken from 1M rows on, measured slower.
- Each occupied slot of the key index is ranked with one POPCNT per key where
  the CPU has it, chosen at run time.
- `COMPARA_PHASES=1` also prints each compared Parquet column's read and
  compare times.
- The README lists TSV and JSON as input formats of their own, with how each
  is read: TSV through the CSV reader, JSON as one object per line or one
  top-level array of objects, its columns the keys of the first object.

## [0.5.2] - 2026-10-16

### Added

- `compara-gen-data --format tsv` writes the CSV rows tab-separated, so a TSV
  pair can be benchmarked against the same rows in CSV. On a 1M-row pair TSV
  is within 5% of CSV on every engine (`turbo` 0.15 s against 0.14 s,
  `native` 1.33 s against 1.28 s, `sortmerge` 2.07 s against 1.98 s, four
  threads) and gives the same answer and export files, which
  `tests/formats.rs` now holds every engine to.

### Changed

- On x86-64 the CSV, TSV and NDJSON readers scan with AVX2 when the CPU has
  it, asked at run time, so the release binaries and `cargo install` get the
  vector scan that before needed a build made with `-C target-cpu=x86-64-v3`.
  On a 2M-row pair, four threads, CSV counts in 0.24 s against 0.28 s (paired
  1.16x), TSV 1.19x, NDJSON 1.02x, level with the x86-64-v3 build on all three;
  answers and export files are the same, byte for byte. A CPU without AVX2,
  and aarch64, scan eight bytes at a time as before.
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

[0.6.0]: https://github.com/andrey-usa/compara/releases/tag/v0.6.0
[0.5.3]: https://github.com/andrey-usa/compara/releases/tag/v0.5.3
[0.5.2]: https://github.com/andrey-usa/compara/releases/tag/v0.5.2
[0.5.1]: https://github.com/andrey-usa/compara/releases/tag/v0.5.1
[0.5.0]: https://github.com/andrey-usa/compara/releases/tag/v0.5.0
