# Contributing to compara

Bug reports and pull requests are welcome. For anything larger than a fix,
open an issue first so we can agree on the approach.

## Build and test

```sh
cargo build --release
cargo test --release
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

A cold build takes about twenty seconds. The tests assert one result contract
across every engine and every input format, cell for cell: a change that makes
two engines, or a CSV and a Parquet copy of the same rows, disagree fails
them.

### Test data

```sh
cargo run --release --bin compara-gen-data -- --rows 1m --out-dir data
cargo run --release --bin compara-gen-data -- --rows 1m --out-dir data --format parquet
compara compare data/1m_a.csv data/1m_b.csv -k account_id,txn_id -i updated_at
```

`compara-gen-data` writes a deterministic 20-column pair keyed on
`(account_id, txn_id)`, as CSV, newline-delimited JSON or Parquet, from one
recipe. Money is carried in integer cents and drifted as integers, so the
files are byte for byte reproducible:

| Drift | Share of rows |
|---|---|
| `status` changed | 3.0% |
| `amount` changed (+12.34) | 1.5% |
| `balance` changed (+1%, half up) | 1.5% |
| `value_date` blanked | 0.3% |
| `updated_at` changed | 100% (excluded with `--ignore`) |
| rows only in B | 0.10% |
| rows only in A | 0.10% |
| duplicate keys | 0.01% per file |

The small files in `tests/fixtures/` are written by pyarrow, not by this
project (a reader tested only against its own writer's files tests nothing);
`tests/fixtures/README.md` says what each one is for, and `scripts/` holds the
scripts that write them.

### Benchmarks

```sh
cargo run --release --features bench --bin compara-bench -- --rows 1m --engine turbo --out-dir bench
```

`compara-bench` runs the comparison in a child process so peak RSS is
measured honestly, records generation time, wall time, throughput, report
size and counts, and fails when a scale exceeds its budget. Compare numbers
from one machine and one sitting only.

### Diagnostics

| variable | effect |
|---|---|
| `COMPARA_PHASES=1` | print where the time went, phase by phase |
| `COMPARA_PRINT_PEAK_RSS=1` | print peak memory at exit (Linux) |
| `COMPARA_MEMORY=<size>` | the memory budget, as `--memory` |
| `COMPARA_BACKTRACE=1` | a backtrace on panic |

## Layout

```
src/main.rs               argument parsing and exit codes
src/lib.rs                the library's public surface
src/contract.rs           the result contract
src/options.rs            runtime parameters and engine names
src/engine.rs             compare() entry point and the engine registry
src/engine/turbo.rs       the byte-level engine: threading, the index, the join
src/engine/turbo/         field packing, SWAR and AVX2 scans, CSV/JSON readers, the Parquet reader
src/engine/pqdiff.rs      the columnar path for a Parquet pair
src/engine/sortmerge.rs   external sort and merge join
src/engine/native.rs      csv-crate parse, in-memory join
src/report.rs, report.html  the HTML report and its embedded template
src/profiles.rs           compara.toml profiles
src/gendata.rs            the test data generator (and src/gendata/parquet.rs, its writer)
src/bin/                  compara-gen-data and compara-bench
tests/                    the contract, every engine and format, out-of-memory behaviour
```

## Releasing

1. Bump `version` in `Cargo.toml` and add a `CHANGELOG.md` entry on `main`.
2. Actions → release → Run workflow with that version. The run checks the
   version, builds every target, tags `v<version>`, creates the GitHub release
   and publishes to each registry that is switched on.

Registries are switched on per repository with Actions variables, once the
account and trusted publisher exist:

| variable | publishes to | trusted publisher to configure |
|---|---|---|
| `PUBLISH_CRATES=true` | crates.io | crate `compara`, workflow `release.yml` |
| `PUBLISH_NPM=true` | npm (`compara` + one package per platform) | each package, workflow `release.yml` |
| `PUBLISH_PYPI=true` | PyPI (platform wheels) | project `compara`, workflow `release.yml`, environment `release` |
| `HOMEBREW_TAP=owner/homebrew-tap` | Homebrew formula | secret `TAP_TOKEN` with write access to the tap |
| `SCOOP_BUCKET=owner/scoop-bucket` | Scoop manifest | the same `TAP_TOKEN` |

Re-running the workflow for a version that already exists rebuilds its files
and publishes it to any registry that doesn't have it yet.
