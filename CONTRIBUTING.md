# Contributing

This repository is where [compara](https://github.com/andrey-usa/compara)'s
engine is worked out: four byte-level implementations of one table comparison
— C (`c/`), C++ (`cpp/`), Rust (`rust/`, which is compara) and Zig (`zig/`) —
held to a single result contract and measured against each other and against
DuckDB, Polars and pandas. A technique earns its place in compara by being
measured here first.

Read this file, then [FINDINGS.md](FINDINGS.md) before starting a change: most
ideas have been tried, and most wrong turns here were measurement, not code.
Each finding has an ID (`A-` advance, `M-` mistake, `T-` technique, `R-`
real-world result, `E-` open experiment); cite the IDs a change builds on, and
add a row when a change teaches something.

## Layout

| Path | What it is |
|---|---|
| `README.md` | the user guide: every command runs from the repository root, as written, on a fresh clone |
| `FINDINGS.md` | what the project has established, as tables with stable IDs — the record that replaces reading the history |
| `RESULTS.md` | where the four ports stand today: one host, one sitting, per format |
| `BENCHMARKS.md` | the benchmark record, newest first; older weeks in `docs/benchmarks/` |
| `docs/` | `contract.schema.json` (the `--json` contract), `measuring.md` (how to tell whether a change paid), `benchmarks/` |
| `c/` | the C port: `csvdiff.c` (CSV/TSV/ndjson), `parquet.c` + `pqdiff.c` (columnar), `parallel.c`, `gen-data.c` + `pqwrite.c`; suite `test.sh` |
| `cpp/` | the C++ port; suite `test.sh`; a generator that also writes snappy |
| `rust/` | compara: engines `turbo` (default), `sortmerge`, `native`, the columnar Parquet path, the HTML report; `cargo test` |
| `zig/` | the port with an *enforced* `--max-memory`; `zig build --release=fast` + `test.sh` |
| `scripts/` | the measurement harnesses: `bench_ab.sh` (did a change pay), `bench_formats_ports.py` (every port, every format), `realworld.py` + `contenders.py` (real writers' files, against DuckDB, Polars and pandas), `fuzz_diff.py` (conformance) |
| `.github/workflows/` | CI and benchmarks, listed in the README; `parity.yml`, `formats.yml` and `conformance.yml` are the repository-wide gates |
| `.github/actions/` | `setup-zig` (the one place the Zig version is pinned) and `setup-cc` |
| `tests/fixtures/` | the awkward-input, multi-format, case-folding and hostile fixtures every suite is held to |

## Building and testing

Every command runs from the repository root.

```bash
(cd c && make && make gen-data)        # CC ?= cc
(cd c && bash test.sh)                 # its own checks, no other toolchain
(cd c && bash test.sh --with-ports)    # + the cross-port oracles

(cd cpp && make && make gen-data && bash test.sh)
(cd rust && cargo build --release && cargo test)
(cd rust && cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings)
(cd zig && zig build --release=fast && bash test.sh)   # a plain `zig build` is a Debug build, ~4x slower

scripts/build_ports.sh c c-gen cpp cpp-gen rust zig     # all at once
```

Linux is the only platform anything is *measured* on. Rust builds and tests on
Windows and macOS; C, C++ and Zig build natively on Windows and are checked
against the Rust port's answers there (`platforms.yml`).

## Invariants

- **The result contract is the API.** `rust/src/contract.rs` defines the shape;
  every port must return identical `counts` and `columns` on the same input.
  `parity.yml` gates it on generated data in every format (CSV, TSV, ndjson,
  Parquet), `conformance.yml` on inputs built to break parsers, and
  `docs/contract.schema.json` plus `scripts/contract.py` check every port's
  `--json`. A build that disagrees about a count is a bug, not a tweak.
- **Duplicate keys are first-class.** The first occurrence of each key joins;
  the rest are counted and listed. Changing that is a behaviour change.
- **Bad options are refused, not ignored.** `--key`, `--compare` and `--ignore`
  naming a column *neither* file has is an error in every port.
- **The packed field is the representation.** A field is an offset and a length
  in a 64-bit word; nothing becomes a heap string until a report or export needs
  it. Memory is this project's argument.
- **A separator is not a format.** Every port reads the separator from the header
  line (comma, semicolon, tab, pipe), so a TSV file must give the CSV answer.
- **Parquet capability refusals route; real errors fail.** The columnar path
  falls through to the row reader only for something it does not read (a page
  version, an encoding); a corrupt file or a missing key column fails with its
  own error. Read the reader each run names before reading its number.
- **The HTML report is file://-safe.** No external references (CI greps
  `src=`/`href=`), no browser storage.
- **No dependency carries a comparison engine.** No dataframe library or SQL
  engine inside a port; they appear only as contenders in `scripts/`.

## Measuring

The full rules, with the incident behind each, are in
[docs/measuring.md](docs/measuring.md) and the `M-` rows of
[FINDINGS.md](FINDINGS.md). The ones to have before starting:

- **`scripts/bench_ab.sh` answers "did that change pay?"** It interleaves the two
  builds and reports the median of per-round paired ratios with the middle half;
  where the half straddles 1.00x it says *no result*. `--self-test` rechecks the
  claim on the machine you are on.
- **Never compare across tables.** Two numbers from two sittings are two machine
  states; a hosted runner's CPU changes between jobs.
- **Not `hyperfine` for small differences.** One build's runs then the other's
  read two copies of one binary as 9-16% apart on a shared runner.
- **Bound the prize before building.** A deliberately unsound build that removes
  the cost names the upper bound in minutes.
- **Check a probe against the counts it still produces**, not only the clock.
- **Time the same task.** A tool that also renders a report, writes row samples
  or exports rows is doing more work; compare it with tools doing the same.
- **Measure the files users have.** Generated Parquet is all strings; real
  writers' files are typed and compressed their own way (`realworld.yml`).

## Gotchas

- `resource.ru_maxrss` is KB on Linux and bytes on macOS, and a forked child
  counts its parent's resident pages until it execs: keep a measuring process
  small.
- The Rust port has two Parquet readers. Never pass `--engine turbo` in a
  benchmark: it retires the columnar reader that `auto` picks.
- In Rust, an allocation that scales with the input goes through
  `rust/src/alloc.rs` (fallible), and a scoped thread through
  `rust/src/parallel.rs` (runs inline if the OS refuses a thread);
  `rust/tests/out_of_memory.rs` walks the ladder.
- A panic under a tight memory cap must not take the backtrace lock:
  `rust/src/main.rs` installs a one-line hook, and `COMPARA_BACKTRACE=1` is the
  opt-in. The C, C++ and Zig ports read `CSVDIFF_*` variables; the Rust port
  reads the same names as `COMPARA_*`.
- In C, `--compare`/`--key`/`--ignore` live in two resolvers (`csvdiff.c` and
  `pqdiff.c`); change both.
- In Zig, an allocation from the fixed buffer is spent for good, even when freed.
- `drop_caches` does not give a cold disk on hosted runners.

## How to contribute

1. **Branch from `main` and open a pull request.** One change per pull request.
2. **Commit messages say what was measured**, not only what changed:
   `adaptive 32-bit hash-table slots — 1.04x CPU, −119 MB RSS at 10M` beats
   `optimize hash table`.
3. **Check the other ports first.** The same finding usually applies to all
   four, and the right answer can differ per port.
4. **Wait for CI.** A change that breaks one port is as unmergeable as one that
   breaks another.

## Reporting issues

Use the issue templates. Name the port, the input shape (rows × columns,
format, and for Parquet the writer), the exact command, and what you expected
against what you got. Counts that disagree between ports are a bug — say which
two and attach the smallest input that reproduces it.

## Code of conduct

Be kind and precise. The full text is in `CODE_OF_CONDUCT.md`.
