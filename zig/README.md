# csvdiff — Zig

The byte-level comparison with **the memory it may use passed in**, not assumed.

Same design as the C, Rust and C++ `turbo` engines: the file is mapped, a
field is an offset and a length packed into one word, delimiters are found eight
bytes at a time with SWAR, and nothing becomes a string unless it reaches the
report.

It reads **CSV, newline-delimited JSON and Parquet**, decoding Parquet's pages
itself, and splits the comparison across every core. A Parquet pair takes a
different path entirely — [columnar, and never reconstructing a
row](../FINDINGS.md#techniques-worth-it-and-not) — and the budget bounds that path
exactly as it bounds this one. Where only one side is Parquet there is no column
to compare a byte stream against, so it is materialised into rows and read by
the text engine: slower, and still an answer rather than a refusal.

```bash
zig build --release=fast
zig-out/bin/csvdiff compare a.csv b.csv -k id --json summary.json
zig-out/bin/csvdiff compare a.csv b.parquet -k id        # either side, any format
zig-out/bin/csvdiff compare a.csv b.csv -k id --threads 4
zig-out/bin/csvdiff compare a.csv b.csv -k id --max-memory 256
zig-out/bin/csvdiff compare a.parquet b.parquet -k account_id,txn_id
./test.sh                # against the Rust port, and Parquet against its own CSV
```

**Build it with `--release=fast`.** A plain `zig build` is a Debug build and is
about four times slower on the Parquet path. `--release=safe` keeps the bounds
and overflow checks at release speed (useful when chasing a crash), and
`-Doptimize=<mode>` names any mode directly.

## Why this port exists

Every other implementation here can only be *measured* for memory and hoped
about. This one takes an allocator, so `--max-memory` is not a target the engine
tries to respect — it is a `FixedBufferAllocator` that cannot hand out more than
it was given. A comparison that would exceed the budget fails at the allocation
that would have crossed the line, naming the budget:

```
$ csvdiff compare 1m_a.csv 1m_b.csv -k account_id,txn_id -i updated_at --max-memory 192
error: the comparison needs more than the 192 MB it was given
$ csvdiff compare 1m_a.csv 1m_b.csv -k account_id,txn_id -i updated_at --max-memory 256
A 1000100 rows | B 1000050 rows | matched 999000 (changed 60049) | ...
```

Bounded memory stops being a number someone reads afterwards and becomes a
property the program cannot violate. On a million rows the threshold is between
192 and 256 MB, and the answer either arrives correct or does not arrive.

## The build mode is worth more than the language

On a million rows, best of three, one 4-core container:

| Build | Compare | Peak RSS |
|---|---:|---:|
| `zig build --release=fast` | **5.13s** | 413 MB |
| `zig build` (Debug, the default) | 32.74s | 420 MB |

**6.4x apart** from the same source. `zig build` with no arguments produces the
slow one, and a `preferred_optimize_mode` in `build.zig` does not change that —
it sets the default for `-Drelease`, which Zig 0.16 spells `--release`.

Against the other ports of the same design, after the hash and threading changes
described in the root README (a million rows, best of three, one container):

| | Compare | Peak RSS |
|---|---:|---:|
| **Zig 0.16** | **0.89s** | **426 MB** |
| C++ (clang 18) | 0.94s | 514 MB |
| Rust, engine only | 0.77s | 439 MB |
| Rust, with its HTML report | 1.58s | 584 MB |

The three are within a few per cent of each other, which is the point: this is
one design in three languages, and where they differ it is by what they were
asked to produce. The Rust row is the only one that is not comparing like with
like — its default run renders the report the other two do not produce at all,
and that is the 0.8s between its two rows. Zig still holds the least memory.

The scanner is a 32-byte vector wherever the build targets a CPU with AVX2 --
which `zig build` does by default, since it targets the host -- and SWAR
everywhere else (`-Dcpu=baseline`, aarch64). `-Dscan=8`, `32` or `64` picks one
explicitly; the numbers are in BENCHMARKS.md.

## Input formats

The format is decided by what is in the file rather than by its name: a Parquet
magic number, a leading `{`, or a CSV header.

| Format | How it is read |
|---|---|
| CSV | mapped, scanned 32 bytes at a time (8 without AVX2); a field is an offset and a length |
| newline-delimited JSON | the same, addressed by key rather than by column number; `\uXXXX` is decoded, so a character written escaped and the same character written literally compare equal |
| Parquet | pages decoded into an arena taken from the same allocator; a field is an offset into it, and a dictionary-encoded column points *at the dictionary entry* |

The Parquet reader is written here — Thrift metadata, page decode, the RLE and
delta encodings, and snappy and LZ4 by hand with gzip and zstd from the standard
library. It reads PLAIN, dictionary, RLE and the version-2 delta encodings, data
pages v1 and v2, and definition levels for optional columns; it refuses nested
schemas, `BYTE_STREAM_SPLIT`, LZO and Brotli **by name** rather than by wrong
answer.

A typed Parquet value has to become text before it can be compared, and the rule
is the one the Rust port implements, cell for cell: integers and decimals in
full, floats in the shortest round-trip form laid out as JavaScript lays it out,
dates `YYYY-MM-DD`, timestamps ISO 8601, booleans `true` and `false`.

The budget covers all of it. Reading Parquet allocates its arena from the same
allocator, so `--max-memory` bounds a Parquet comparison exactly as it bounds a
CSV one — which is the one thing this port can say that no other implementation
here can.

## Threads

`--threads N`, default every core. Each file is split at row boundaries, parsed
and hashed in parallel, then inserted into its index in file order; the join
splits over contiguous ranges of A's keys while one thread walks B's. Ordering is
kept where it is load-bearing — first occurrence of a key wins, and the duplicate
counts follow from it — so the answer does not depend on the thread count, which
`test.sh` checks at 1, 2, 3, 4 and 8 on a file with quoted newlines at the chunk
boundaries.

Finding those boundaries is the interesting part for CSV: a newline inside a
quoted field is not a row boundary, and a thread starting mid-file cannot tell
whether it is inside one. Quote parity settles it — every `"` toggles the state,
including both halves of a doubled quote, which toggles twice and so leaves it
alone. JSON needs none of that: a raw newline inside a string is not valid JSON,
so every newline ends a record.

## What it is and is not

A **benchmark and parity port**: the comparison and a JSON summary, not the HTML
report the Rust port produces.

`--ignore-case` is ASCII-only. Folding outside ASCII needs a Unicode table this
port does not carry, and folding partially is worse than not folding at all —
`CAFÉ` and `café` would compare equal in the ports that do fold and unequal here,
with nothing in the output to say why. So a non-ASCII byte is refused by name
wherever the fold would decide the answer: in a key, or in a compared value that
differs byte for byte between two matched rows. Anywhere else the fold changes
nothing and the run is answered: a value that did not change, a row only one side
has, a value whose mate is empty, a column left out of the comparison, a number
under `--tolerance`.

## Layout

| File | What it holds |
|---|---|
| `src/scan.zig` | delimiter scanning: a vector register where the target has one, SWAR otherwise |
| `src/field.zig` | the packed field word |
| `src/slab.zig` | the bytes a field points into, and how they are unescaped |
| `src/text.zig` | the CSV and newline-delimited JSON readers |
| `src/parquet.zig` | a Parquet reader shaped for comparing: Thrift footer, page decoder, RLE/bit-packed hybrid, snappy |
| `src/pqdiff.zig` | the columnar comparison — key join first, then one column at a time, on shared dictionary ids |
| `src/pqread.zig` | the same file read as rows, for the mixed parquet/text pair |
| `src/thrift.zig` | the compact protocol the Parquet footer is written in |
| `src/codec.zig` | snappy and LZ4 by hand; gzip and zstd from the standard library |
| `src/encoding.zig` | the RLE/bit-packed hybrid and the delta encodings |
| `src/csvdiff.zig` | the engine: threading, the index, the join |
| `src/main.zig` | the command line and the memory budget |
| `src/tests.zig` | the test root (`zig build test`); `test.sh` runs the rest |
