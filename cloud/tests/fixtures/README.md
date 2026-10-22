# Awkward input

One pair of files holding every shape that has broken an engine in this project, or plausibly
could. Both wrong answers this tool has shipped needed neither an unusual option nor a malformed
file — one wanted a key in the last eight bytes of the file, the other a key outside ASCII — so the
generated benchmark data, which is uniform and pure ASCII, could never have found either.

| Row | What it is there for |
|---|---|
| `CAFÉ` / `café` | a non-ASCII key differing only in case |
| `  padded  ` / `padded` | whitespace around a key, which only `--trim` may ignore |
| `K` (U+212A) / `k` | a case fold that crosses the ASCII boundary and changes the byte length |
| `"has,comma"` | a quoted key holding the delimiter |
| `"a""b"` | a quoted key holding a doubled quote, the one value that is not a slice of the file |
| `"two\nlines"` | a quoted key holding a newline, which row splitting must not treat as a row end |
| `dup` twice | a duplicate key |
| `blank` | an empty value |
| `gone` / `extra` | a key on one side only |
| `z` | a short key in the last row, so it lands within eight bytes of the end of the file |

What the tests assert is not a particular answer but that the implementations cannot disagree about
one. Under the default options `CAFÉ` and `café` are different keys; under `--ignore-case` they are
the same. Either is a defensible answer. Two implementations giving different ones is not.

`tests/sortmerge.rs` runs it through every engine under each combination of `--trim` and
`--ignore-case`.

# Formats

`formats/` holds one small table written nine ways, and the point of it is the same as above: what
the tests assert is not a particular answer but that the readers cannot disagree about one. The
comparison has to return identical counts and identical per-column stats whether a side arrives as
CSV, as newline-delimited JSON, or as any of the Parquet encodings a writer might choose.

| File | The writer decision it covers |
|---|---|
| `a.csv`, `b.csv` | the reference answer everything else is held to |
| `a.ndjson`, `b.ndjson` | the same rows as one JSON object per line |
| `a_dict_snappy.parquet` | dictionary encoding, snappy — what DuckDB and pyarrow write by default |
| `a_plain_none.parquet` | no dictionary, no compression: the plain byte-array path |
| `a_dict_gzip.parquet` | gzip pages |
| `a_plain_zstd.parquet` | zstd pages |
| `a_plain_lz4.parquet` | LZ4 pages, whose block format the readers decode themselves |
| `a_delta_v2.parquet` | version-2 data pages with the DELTA_BYTE_ARRAY family, a different decoder |
| `a_dict_row_groups.parquet` | five row groups, so a dictionary has to be carried per column chunk |
| `typed.parquet` + `typed.csv` | every non-string type, against the text it is required to render as |
| `words_a.parquet`, `words_b.parquet` | a pair of numbers, dates, times and booleans, with nulls, both zeros, NaN and infinities, on dictionary pages: the columnar path compares these a word per value and must give the row-by-row answer |
| `words_a_plain.parquet`, `words_b_plain.parquet` | the same pair on plain uncompressed pages, with the decimals stored as integers, as DuckDB writes them |
| `words_b_wider.parquet` | B with one column stored as INT64, which the columnar path does not compare word for word with A's INT32 |
| `rg_snappy.parquet`, `rg_zstd.parquet`, `rg_none.parquet` | four row groups, a string column dictionary encoded in two of them and plain in the other two, nulls in most columns: a column read in runs of row groups on several threads must read as it does in one pass. Written by DuckDB (`scripts/make_row_group_fixtures.sql`) |

They are written by pyarrow (`scripts/make_parquet_fixtures.py`), not by this project: a reader
tested only against files its own writer produced tests nothing. `typed.csv` is the stated
expectation for how a typed column becomes text — integers and decimals in full, floats in the
shortest round-trip form, dates `YYYY-MM-DD`, timestamps ISO 8601 — so a change to that rule fails
a test rather than passing quietly.

# Fold

`fold/` is one small pair, written as CSV, newline-delimited JSON and dictionary and plain Parquet,
for `--ignore-case` outside ASCII: compara folds Unicode case, so keys and values whose case differs
only beyond ASCII (`CAFÉ`/`café`, `K` U+212A/`k`) must match the same way in every format.
`scripts/make_fold_fixtures.py` writes the files and says what each row is for.

Its `pad` column holds values that are only whitespace, which generated data never does. Under
`--trim` such a value is `""`, and `""` is a value: only `--empty-is-null` makes it absent. The
Parquet path once read it as absent under `--trim` alone, and gave a different answer
for the same rows as Parquet than as CSV.

# Hostile

`hostile/` holds Parquet files that are not meant to be compared at all: each broke a reader once,
and `tests/hostile.rs` requires a clean refusal from all of them. The rest of that test's cases are
footers small enough to write inline.

| File | What it was |
|---|---|
| `zstd_bomb.parquet` | one zstd page whose header says 36 KB and whose stream inflates to 256 MB of zeros (written by pyarrow, then the page body swapped) |
| `negative_page_size.parquet` | a mutant of `formats/b_dict_snappy.parquet` with a negative compressed page size |
| `delta_header.parquet` | a mutant of `formats/a_delta_v2.parquet` whose `DELTA_BINARY_PACKED` header asks for a 10^16-value miniblock |
