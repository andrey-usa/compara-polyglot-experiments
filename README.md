# compara-polyglot-experiments

The workshop behind [compara](https://github.com/andrey-usa/compara): one table
comparison — two tables on a composite key, with counts, per-column statistics
and the changed, added and removed rows — written four times, in C, C++, Rust and
Zig, held to one result contract, and measured against each other and against
DuckDB, Polars and pandas.

The Rust port in [`rust/`](rust/) **is** compara. The C, C++ and Zig ports
(binaries still named `csvdiff`, after the project's first name) exist to find
out what each language and each technique is worth: a technique that wins here
is carried into compara, and one that loses is written down so nobody tries it
twice. All four read the same files, return the same counts and the same exit
codes, which is what makes a number from one directly comparable with a number
from another.

- [FINDINGS.md](FINDINGS.md) — **what the project has established**, as tables
  with stable IDs: advances, mistakes not to repeat, techniques across the four
  ports, real-world files, and what is open
- [RESULTS.md](RESULTS.md) — where the ports stood on generated data: 10M rows and up, one host per table
- [BENCHMARKS.md](BENCHMARKS.md) — the benchmark record, newest first, with CPU and memory
- [CONTRIBUTING.md](CONTRIBUTING.md) — layout, invariants, measuring rules and gotchas

---

## Real-world files

The tables further down were measured on files this project's own generator
wrote, and its Parquet is all strings. Files arrive written by pyarrow, Polars,
DuckDB or pandas — typed columns, each writer's codec — and on those the picture
changes: the C and C++ ports refuse every one, Zig falls back to its row reader,
and compara stays on its columnar path. C and C++ are built for this CPU
(`-march=native`), compara as its releases ship (baseline x86-64; built for the
CPU it counts the CSV pair 1.17x faster, level with C — E-001). One sitting, `Intel(R) Xeon(R) Processor @ 2.10GHz`,
4 cores, 1M rows a side of the standard twenty columns,
4 threads for every tool, best of 3,
2026-10-15; wall · peak RSS with the mapped input; every tool that read a file
gave the same counts (938,951 unchanged, 60,049 changed, 1,000 added, 1,000 removed).
The contenders' times include starting Python and importing the library.
[FINDINGS.md](FINDINGS.md#real-world-files) has the narrow typed shape, what each
writer put in its file, and the TSV rows; `realworld.yml` reruns it weekly.

Counts only:

| Shape | File | C | C++ | compara | Zig | DuckDB | Polars | pandas |
|---|---|---|---|---|---|---|---|---|
| wide | csv | **0.12s · 415 MB** | 0.13s · 418 MB | 0.15s · 416 MB | 0.15s · 415 MB | 4.04s · 1,259 MB | 1.70s · 2,036 MB | 19.68s · 2,071 MB |
| wide | tsv | **0.11s · 415 MB** | 0.13s · 418 MB | 0.15s · 416 MB | 0.14s · 415 MB | 4.04s · 1,319 MB | 1.80s · 1,949 MB | 19.73s · 2,070 MB |
| wide | parquet-gen | 0.17s · 323 MB | **0.17s · 322 MB** | 0.18s · 322 MB | 0.17s · 274 MB | 3.69s · 1,406 MB | 1.38s · 1,704 MB | 8.60s · 2,172 MB |
| wide | parquet-pyarrow | — | — | **0.29s · 407 MB** | 1.74s · 567 MB | 2.95s · 996 MB | 1.19s · 1,463 MB | 7.34s · 1,681 MB |
| wide | parquet-polars | — | — | **0.30s · 368 MB** | 2.55s · 522 MB | 3.16s · 1,166 MB | 1.42s · 1,736 MB | 8.79s · 1,972 MB |
| wide | parquet-duckdb | — | — | **0.32s · 383 MB** | 1.61s · 568 MB | 1.74s · 1,013 MB | 1.13s · 1,639 MB | 7.48s · 1,690 MB |
| wide | parquet-pandas | — | — | **0.29s · 406 MB** | 1.98s · 557 MB | 3.34s · 1,121 MB | 1.30s · 1,579 MB | 8.70s · 1,897 MB |

With the added, removed and changed rows written as CSV (`--export-dir`; the
contenders through `COPY … TO`, `write_csv`, `to_csv`):

| Shape | File | compara | DuckDB | Polars | pandas |
|---|---|---|---|---|---|
| wide | csv | **0.85s · 733 MB** | 4.71s · 1,252 MB | 1.78s · 2,010 MB | 20.00s · 2,298 MB |
| wide | tsv | **0.89s · 733 MB** | 4.67s · 1,176 MB | 1.92s · 1,974 MB | 20.28s · 2,299 MB |
| wide | parquet-gen | **0.32s · 351 MB** | 4.11s · 1,368 MB | 1.48s · 1,718 MB | 10.75s · 2,438 MB |
| wide | parquet-pyarrow | **0.60s · 451 MB** | 3.53s · 1,176 MB | 1.29s · 1,482 MB | 10.76s · 1,947 MB |
| wide | parquet-polars | **0.55s · 387 MB** | 3.69s · 1,064 MB | 1.42s · 1,721 MB | 10.65s · 2,213 MB |
| wide | parquet-duckdb | **0.64s · 442 MB** | 2.37s · 961 MB | 1.24s · 1,652 MB | 9.44s · 2,002 MB |
| wide | parquet-pandas | **0.55s · 441 MB** | 3.96s · 1,106 MB | 1.41s · 1,558 MB | 10.12s · 2,172 MB |

"—" is a file the port refuses ([R-002](FINDINGS.md#r-002--which-port-reads-which-file)).

---

## The example

Ten million rows × 20 columns, keyed on `(account_id, txn_id)`, `--ignore
updated_at`, a per-cell diff over seventeen columns. One GitHub Actions runner
(AMD EPYC 7763, 4 vCPU / 16 GB), every port built from this tree and compiled
for that runner, all four interleaved in one sitting, five rounds each, best
kept, 2026-09-30
(run 36672430118,
`benchmark-native.yml` on `58eb980`). Each cell is **wall · CPU · memory above
the mapped input**.

| Format | Input | C | C++ | Rust | Zig |
|---|---:|---|---|---|---|
| CSV | 3,509 MB | **1.02s** · 3.5s · 716 MB | 1.09s · 3.8s · 723 MB | 1.20s · 4.0s · **591 MB** | 1.09s · **3.0s** · 597 MB |
| ndjson | 8,487 MB | 2.72s · **8.3s** · 716 MB | **2.53s** · 9.4s · 735 MB | 2.79s · 10.4s · **592 MB** | 2.59s · **8.3s** · 600 MB |
| Parquet | 2,074 MB | **1.28s** · **4.1s** · **1,081 MB** | 1.41s · 4.5s · 1,180 MB | 1.41s · 4.7s · 1,342 MB | 1.61s · 5.4s · 1,230 MB |

All four returned identical counts on all three formats — matched 9,990,000,
changed 599,320, added 10,000, removed 10,000, duplicate keys 1,000 in A and
500 in B. That is the run's gate, not a footnote: builds that disagree about how
many rows changed mean a bug in one of them, so the run fails and names it.

Four things this table says.

**On CSV the four ports are within 1.18x of each other.** C is fastest at
1.02s, Rust last at 1.20s, and Zig does the least work of the four — 3.0
CPU-seconds against C's 3.5. That is what a text path looks like once every
port stops parsing columns nobody reads and stops comparing rows it can prove
equal from their bytes.

**ndjson has converged too.** The 09-09 edition of this table, on another
runner, had C ahead by 1.9x; here the whole field is within 1.10x (2.53–2.79s)
and C++ is fastest. C and Zig spend the least CPU,
8.3 seconds each.

**Memory splits two and two.** Rust and Zig hold 591–600 MB above the mapped
input on both text formats, against 716–735 MB for C and C++. On Parquet the
order turns over: C's reader peaks lowest, at 1,081 MB, and Rust highest.

**Parquet is the one format with a clear order.** C leads by 1.10x; C++ and
Rust tie at 1.41s; Zig is last at 1.61s.

> **Where the columns come from.** All four are built from this tree, on one
> runner, in one sitting — the two branches that used to be measured against
> each other were merged, so there is no second checkout and no pinned commit
> any more. Every port is compiled for the machine it runs on: `-march=native`
> for C and C++, `-C target-cpu=x86-64-v3` for Rust, `-Dcpu=native` for Zig.
> (Rust's flag is capped at the fleet's executable floor — resolving the host
> CPU picked `znver4` and a build-time compiler crashed with SIGILL; see
> [the defect in BENCHMARKS.md](BENCHMARKS.md).
>
> Compare rows within this table. Not with the tables in
> [BENCHMARKS.md](BENCHMARKS.md) or [RESULTS.md](RESULTS.md), and not with the
> 2026-09-09 edition of this one, which ran on a different runner: this
> project has seen one runner's ndjson numbers move 20% between two runs one
> morning with no code change at all, and the same tree produce three different
> ndjson winners on three machines inside an hour. A published ordering is only
> meaningful with its CPU printed next to it.
>
> [RESULTS.md](RESULTS.md) carries the rest of the ladder — 10M, 50M and 150M
> rows, CSV and Parquet, each rung on its own runner.

### The same bytes, five times wider

Two million rows × 100 columns: the twenty above plus eighty more of 8-digit
values (`gen-data --columns 100`), sized so the CSV pair is about the bytes of
the ten-million-row one — 3,449 MB against 3,509. Same key and `--ignore`, so
the per-cell diff now covers 97 columns, and the ignored `updated_at` sits at
column 20 of 100 rather than last. Same runner model, same workflow, 2026-09-30
(run 36741246747,
on `b8efd42`). Each cell is **wall · CPU · memory above the mapped input**.

| Format | Input | C | C++ | Rust | Zig |
|---|---:|---|---|---|---|
| CSV | 3,449 MB | **0.65s** · 2.4s · **126 MB** | 0.76s · 2.8s · 134 MB | 0.68s · 2.4s · 128 MB | 0.74s · **2.3s** · 136 MB |
| ndjson | 7,191 MB | 2.00s · 7.7s · **126 MB** | **1.91s** · 7.3s · 133 MB | 2.28s · 8.6s · 128 MB | 1.93s · **6.7s** · 134 MB |
| Parquet | 4,077 MB | **1.16s** · **4.4s** · 344 MB | 1.55s · 5.9s · 384 MB | 1.46s · 5.6s · 392 MB | 1.88s · 7.2s · **309 MB** |

All four returned identical counts on all three formats — matched 1,998,000,
changed 180,084, added 2,000, removed 2,000, duplicate keys 200 in A and 100 in
B. (Changed is 9% rather than 6%: in B, one of the eighty extra columns changes
in another 3% of rows.)

This is a separate run, so the caveat below the main table applies to setting
the two side by side: only differences well past run-to-run noise (up to 20% on
ndjson) mean anything.

**CSV is now 1.43–1.76x faster wide than long at the same bytes.** C takes
0.65s against 1.02s, Rust 0.68s against 1.20s, on 2.3–2.8 CPU-seconds against
3.0–4.0. A fifth of the rows is a fifth of the keys to index and look up, and
the row proof now settles the unchanged 91% without parsing them. The first run
of this table
(run 36704951796)
had it the other way round, 1.75–1.95x *slower*: the proof compared one byte
prefix through the last compared column, and here that prefix crosses the
ignored `updated_at`, which differs on every row — so it failed on every row
and each one was parsed and compared column by column. #229–#232 compare the
wanted columns in runs and skip the ignored ones between them; the same data
went 1.81s → 0.66s on C, 2.12s → 0.76s on C++, 2.10s → 0.74s on Rust and
1.95s → 0.73s on Zig
(run 36731199773).

**ndjson is now 1.22–1.36x faster wide than long too** (C 2.72 → 2.00s, C++
2.53 → 1.91s, Rust 2.79 → 2.28s, Zig 2.59 → 1.93s), on 6.7–8.6 CPU-seconds
against 8.3–10.4. Until #234–#237 it was the other way round, 1.19–1.42x
*slower* (C 3.26s, C++ 3.58s, Rust 3.90s, Zig 3.09s, on 11.1–14.9 CPU-seconds):
its proof took one span over every wanted value, which crossed the ignored
`updated_at` just as the CSV one did, and failed on every row. Now, at the first
differing byte, the proof finds the member holding it; if that member is not
compared, it skips the value in both rows and carries on. C++ is fastest and Zig
spends the least CPU, 6.7 seconds. The wide ndjson is 15% smaller than the long
one because every value carries its key: `c021` is four characters, the
standard names average eight.

**On the text formats, memory above the input follows rows, not bytes.**
126–136 MB here against 591–735 MB at ten million rows: a fifth of the rows,
18–23% of the memory, on every port. Parquet's readers hold a quarter to a
third of what they did (309–392 MB against 1,081–1,342), with twice the input.

**Parquet is untouched by either change.** Random eight-digit values do not
dictionary-encode, so the wide Parquet is twice the long one's bytes (4,077 MB
against 2,074) — yet C is faster than on the long file (1.16s against 1.28s),
and Rust about level. C leads by 1.26x over Rust and 1.62x over Zig, the widest
spread in either table.

---

## Building and running

> **Run every command in this section from the repository root** — the directory
> holding `c/`, `rust/` and this file. Every path below is written relative to it,
> in bash and in PowerShell alike, so that one rule covers the whole section.
>
> ```powershell
> cd C:\path\to\csvdiff     # PowerShell: the folder holding c\, rust\ and README.md
> ```
> ```bash
> cd ~/path/to/csvdiff       # bash
> ```
>
> If you are inside a port's directory, `cd ..` first. From `c\`, `c/gen-data`
> is `No such file or directory`, and `.\gen-data` would run but write its pair
> into `c\data\` rather than the `data\` the commands below then read.

For a small runnable example without generating anything, `examples/` holds a
two-file pair (`orders_2026-08.csv`, `orders_2026-09.csv`) and a sample HTML
report:

```bash
c/csvdiff compare examples/orders_2026-08.csv examples/orders_2026-09.csv -k order_id
```

### What runs where

Every cell below has a job behind it. `platforms.yml` runs all four ports on
Linux x86-64, Linux aarch64 and macOS on Apple silicon, and three of them on
Windows; the Rust port's Windows lane lives in `ci-rust.yml`.

| Port | Linux x86-64 | Linux aarch64 | macOS (arm64) | Windows |
|---|---|---|---|---|
| **C** | suite | suite | suite | native, agrees with Rust |
| **C++** | suite | suite | suite | native, agrees with Rust |
| **Rust** | suite | suite | suite | native, suite |
| **Zig** | suite | suite | suite | native, agrees with Rust |

"suite" means that port's own test suite ran there and passed — `c/test.sh`,
`cpp/test.sh`, `zig/test.sh`, `cargo test`. The three Windows cells are weaker
on purpose and the difference is worth knowing: those suites are written for a
POSIX shell and reach for binaries by their extensionless names, so they do not
run there yet. What runs instead is `scripts/win_smoke.sh`, which asks the
question those suites ask — the generator's bytes, then answers over CSV,
ndjson and Parquet at one and four threads, each against the Rust port on the
same files. Enough to catch a wrong answer, not the same coverage.

No WSL in the table any more. The three POSIX ports build natively on Windows
with mingw-w64 gcc: the mapping calls are `CreateFileMapping` and a view of it
(`c/win32.h`, `cpp/src/win32.hpp`, and `mapHandle` in `zig/src/slab.zig`), the
C port's threads are `CreateThread`, and every `open` carries `O_BINARY` so the
generator's files stay byte-identical. WSL still works and is still a fine way
to run the bash suites; it is no longer the only way to run the ports.

The Rust row taught the rule the rest of this table follows. It first said
"native" on the strength of a grep for `std::os::unix` finding nothing — which
cannot see inside a dependency, and `memmap2::Advice` is gated `#[cfg(unix)]` in
that crate, so the port did not compile on Windows at all. A reader hit it
within the hour. A claim about a platform is worth what the runner that proves
it is worth, and nothing above is claimed without one.

`test.sh` and `scripts/bench_ab.sh` are bash. On Windows they need WSL, MSYS2 or
Git Bash; `scripts/bench_ports.py` needs Python 3.

### Building

Each port builds from its own directory with its own toolchain, in seconds, and
pulls in nothing that contains a comparison engine.

```bash
# Linux, macOS, or WSL
(cd c    && make)                    # cc or clang, C11. Also builds c/gen-data
(cd cpp  && make)                    # g++ or clang++, C++20
(cd rust && cargo build --release)   # edition 2024. Also builds a gen-data
(cd zig  && zig build --release=fast)
```

```powershell
# Windows, PowerShell. The Rust port needs nothing but cargo.
# --manifest-path so this runs from the root like everything else; the
# binaries still land in rust\target\release\.
cargo build --release --manifest-path rust\Cargo.toml
```

The C, C++ and Zig ports build natively on Windows too. The first two want a
POSIX-ish shell for `make`, which is what [MSYS2](https://www.msys2.org/)
provides — `pacman -S mingw-w64-x86_64-gcc make`, then from its **MINGW64**
shell:

```bash
CC=gcc  make -C c    && make -C c   gen-data
CXX=g++ make -C cpp  && make -C cpp gen-data
```

The Zig port needs no shell at all, since `zig build` is the whole build system:

```powershell
cd zig; zig build --release=fast; cd ..
```

WSL is still a good way to run the bash test suites, and it is what the next
section is about. It is no longer needed to build anything. A fresh WSL has
neither a current Rust nor any Zig at all, and the reasons are not obvious from
inside it, so read on before you start.

### If you are on WSL

**WSL is a second machine.** It has its own filesystem, its own package manager
and its own toolchains, and it shares none of them with Windows. A rustup
install on the Windows side puts `rustc.exe` in your Windows profile; those are
Windows binaries that build Windows executables, and a Linux `cargo build`
cannot use them. So "Rust is new on Windows but old in WSL" is not a
misconfiguration — they are two separate installations, and only one of them
has been kept current.

What makes it confusing is that WSL appends the Windows `PATH` to its own, so
`cargo.exe` and `rustc.exe` **are** found from inside WSL. The toolchain looks
present. It just is not a Linux one.

Two specific symptoms, and what causes each:

| Symptom | Cause |
|---|---|
| `error: rustc 1.86.0 is not supported by the following packages: csvdiff@1.0.0 requires rustc 1.90` | Rust came from `apt install rustc cargo`. Distro packages are frozen at whatever the release shipped and never follow stable. |
| `zig: command not found` | Zig is not in Ubuntu's default repositories in a usable version. It is normally installed by unpacking a tarball, so a fresh WSL has none. |

Fix both from inside WSL. **Rust**, via rustup rather than apt:

```bash
# inside WSL -- where is the old one coming from?
which -a rustc                           # /usr/bin/rustc means apt installed it

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustc --version                          # 1.90 or newer
```

rustup installs to `~/.cargo/bin` and puts it at the front of `PATH`, which is
usually enough on its own. If `rustc --version` still reports the old one, open
a new shell; if that does not do it, `/usr/bin` is still winning:

```bash
export PATH="$HOME/.cargo/bin:$PATH"     # add to ~/.bashrc to keep it
```

Removing apt's copy with `sudo apt remove rustc cargo` also works, but check
what else goes with it first — `apt` takes reverse-dependencies too, and
shadowing it by `PATH` is the smaller change.

**Zig 0.16.0**, the version CI builds with:

```bash
# inside WSL
curl -sSL https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz \
  | sudo tar -xJ -C /opt
echo 'export PATH="/opt/zig-x86_64-linux-0.16.0:$PATH"' >> ~/.bashrc
exec bash
zig version                              # 0.16.0
```

Without `sudo`, or on a machine where `/opt` is not yours to write to, the same
release is on PyPI and needs no root:

```bash
pip install ziglang==0.16.0
mkdir -p ~/.local/bin
printf '#!/bin/sh\nexec python3 -m ziglang "$@"\n' > ~/.local/bin/zig
chmod +x ~/.local/bin/zig
```

Both routes are what the `parity` and `formats` workflows use, so they are the
two that are known to work.

> **Clone into the WSL home directory, not `/mnt/c/`.** A Windows drive is
> reachable from WSL as `/mnt/c/Users/you/...`, but it is a network filesystem
> underneath, and it is slow enough to change what you measure — which for a
> repository whose whole subject is throughput makes every number here
> meaningless. `git clone` into `~/` and work there. To open the result in a
> Windows editor, `\\wsl$\Ubuntu\home\you\csvdiff` reaches it from the
> Windows side.

It is not only slow there. The Zig build is the first thing to break on a
Windows drive, and it does not break in a way that names the cause:

```
error: failed to rename compilation results ('.zig-cache/tmp/14aaf8cf27fc0150')
  into local cache ('.zig-cache/o/7691fdba880312cb541a0fac7ef59a22'): AccessDenied
```

Zig commits a build by renaming it into `.zig-cache/o/` and hardlinking, and
the Windows drive mount supports neither properly. Two commands say which of
the two causes you have:

```bash
pwd                          # under /mnt/c ?
ls -ld .zig-cache .zig-cache/o
```

| What you see | Fix |
|---|---|
| `pwd` is under `/mnt/c` | Point the cache at a native path. The source tree can stay on the Windows drive; only `.zig-cache` cannot. |
| `.zig-cache` is owned by `root` | A `sudo zig build` left it that way: `sudo rm -rf .zig-cache zig-out`, then build **without** sudo. |

```bash
export ZIG_LOCAL_CACHE_DIR=~/.cache/zig-csvdiff
export ZIG_GLOBAL_CACHE_DIR=~/.cache/zig-global
(cd zig && zig build --release=fast && bash test.sh)   # test.sh needs the same two exports
```

This fixes the build; it does not fix the speed. For anything you intend to
measure, clone into `~/` as above -- the 9p mount is slow enough on its own
to spoil a timing regardless of where the cache lives.

The Zig install above needs `sudo` only to write into `/opt`. Nothing in this
repository should be built with it — and if `/opt` is not yours to write to,
the PyPI route needs no root at all.

### Getting a pair to compare

**Already have two files?** Skip this section, and substitute your paths and your
key columns everywhere below. Nothing here is required.

Otherwise generate a pair. Both generators write the same bytes from the same
`--seed`, so it only matters which toolchain you have:

```bash
# Linux, macOS, or WSL
c/gen-data --rows 1m --out-dir data --prefix demo
```

```powershell
# Windows, PowerShell — after the build above.
# The leading .\ is not decoration: PowerShell will not run a program from the
# current directory without it, and a relative path is safer than trusting PATH.
.\rust\target\release\compara-gen-data.exe --rows 1m --out-dir data --prefix demo
```

Either writes `data/demo_a.csv` and `data/demo_b.csv`, 184 MB each: 1,000,100
and 1,000,050 rows of 20 columns, keyed on `(account_id, txn_id)`, with 1,000
keys only in A, 1,000 only in B, a scattering of changed values, and
`updated_at` moved on **every** row — which is what the `-i` flag below is for.
`--rows` also takes `10k`, `10m`, and so on, in either spelling (`--rows 10m` or
`--rows=10m`); `--format json|parquet` writes the same rows in the other two
formats, and the Rust generator also writes `--format tsv`. `rust/target/release/compara-gen-data --help` lists the rest. Both generators
**reject a flag they do not know** rather than ignoring it — the Rust one used
to skip unknown flags silently, so `--rows=50m` produced the 10k default and
said nothing about it.

**The C generator is the faster of the two**, by 4.3x on wall time: 2m rows in
1.05s against the Rust generator's 5.49s on this 4-vCPU runner (paired rounds,
`scripts/bench_ab.sh`), because it renders rows in waves across all cores while
the Rust one is single-threaded. Both write all three formats. Reach for the
Rust generator when you want a single toolchain — it needs only cargo, where
the C one wants a C compiler and `make`. Both build natively on Windows.

### Looking at one file first

Before comparing anything you usually need two facts about a file: what its
columns are called, and what the values look like. Both are one command, and
neither reads more of the file than it has to.

```bash
# Linux, macOS, or WSL
rust/target/release/compara columns data/demo_a.parquet   # names, one per line
rust/target/release/compara head    data/demo_a.parquet   # the first 10 rows
rust/target/release/compara head    data/demo_a.csv -n 3  # or however many
```

```powershell
# Windows, PowerShell
.\rust\target\release\compara.exe columns data\demo_a.parquet
.\rust\target\release\compara.exe head    data\demo_a.parquet
.\rust\target\release\compara.exe head    data\demo_a.csv -n 3
```

```
account_id    txn_id           posting_date  value_date  currency  amount
------------  ---------------  ------------  ----------  --------  ----------
ACC-00000000  TXN-00000000000  2026-02-10    2026-03-10  USD       1792961.62
ACC-00007919  TXN-00000000001  2026-02-05    2026-01-10  JPY       844636.46
ACC-00015838  TXN-00000000002  2026-03-18    2026-08-19  GBP       1142001.50
```

Both read **CSV (and TSV), newline-delimited JSON and Parquet**, and the format
is decided by the bytes rather than the extension. `head --csv` prints machine-readable
rows instead of the aligned table.

**Neither pays for the file.** `columns` on Parquet reads the footer; `head`
stops decoding at the first page — ten rows of a 384 MB Parquet file take
**0.18s**, not the seconds a full decode would. That is the whole point of the
option: columnar data is otherwise only readable here by comparing it against
something.

`columns` writes the names to stdout and its one-line summary to stderr, so the
names pipe cleanly into the `--key` you were about to write:

```bash
# every column as the key -- which is SQL's EXCEPT, see "Duplicate keys" below
KEY=$(rust/target/release/compara columns data/demo_a.csv 2>/dev/null | paste -sd,)
```

```powershell
$KEY = (.\rust\target\release\compara.exe columns data\demo_a.csv) -join ','
```

The values come back through the engine's own parsers — a quoted CSV field is
unquoted here exactly as the comparison would unquote it, and the same rows
written as CSV, as ndjson and as Parquet preview identically. A preview that
disagreed with the comparison would be worse than none, so a test asserts they
do not.

### Running

```bash
# Linux, macOS, or WSL

# the smallest useful invocation: two files and a key
c/csvdiff compare data/demo_a.csv data/demo_b.csv -k account_id,txn_id

# -i skips columns that always move; --json writes the counts and samples;
# --threads caps the cores it takes
c/csvdiff compare data/demo_a.csv data/demo_b.csv -k account_id,txn_id \
  -i updated_at --json summary.json --threads 4

# the Rust port is the one with the self-contained HTML report
rust/target/release/compara compare data/demo_a.csv data/demo_b.csv \
  -k account_id,txn_id -i updated_at -o report.html
```

```powershell
# Windows, PowerShell — the same three, with the Rust port throughout.
# A line is continued with a backtick, not a backslash, and paths use \.

.\rust\target\release\compara.exe compare data\demo_a.csv data\demo_b.csv -k account_id,txn_id

.\rust\target\release\compara.exe compare data\demo_a.csv data\demo_b.csv `
  -k account_id,txn_id -i updated_at --json summary.json --threads 4

.\rust\target\release\compara.exe compare data\demo_a.csv data\demo_b.csv `
  -k account_id,txn_id -i updated_at -o report.html
```

The second and third print `matched 999,000 (changed 60,049) | added 1,000 |
removed 1,000`. Without `-i updated_at` the first one reports all 999,000
matched rows as changed, correctly: that column really did move on every row.

**Three PowerShell details** worth having up front, because each of them stops a
copied command dead:

| | PowerShell | bash |
|---|---|---|
| run a program here | `.\rust\target\release\compara.exe` — the `.\` is required | `rust/target/release/compara` |
| continue a line | a backtick `` ` `` at the end | a backslash `\` |
| the exit code | `$LASTEXITCODE` | `$?` |

`$LASTEXITCODE` matters more than it looks: **a comparison that finds differences
exits 1**, which is the normal result and not a failure. In a script that stops
on errors, catch it rather than let it end the run:

```powershell
.\rust\target\release\compara.exe compare data\demo_a.csv data\demo_b.csv -k account_id,txn_id
if ($LASTEXITCODE -ge 2) { throw "compara failed with $LASTEXITCODE" }
```

**Exit codes** make any of them a CI or pipeline gate directly:

| Code | Meaning |
|---:|---|
| `0` | the two files are identical on the compared columns |
| `1` | differences found — this is a normal result, not a failure |
| `2` | an error: a missing key column, an unreadable file, out of memory |
| `3` | duplicate keys, where `--fail-on-dups` is given — the Rust port only |

`1` is the usual outcome, so a script that treats any non-zero status as failure
will misread a successful comparison. Test for `2` and above.

Format is detected from the bytes, not the extension, and the separator of a
text file from its header line (comma, semicolon, tab or pipe), so TSV is read by
the CSV scanner in every port and must give the CSV answer. In C and C++ a
Parquet file may only be compared against another Parquet file; Rust and Zig
take either side in any format.

### What each port carries

| | Reads | Notable | Not there |
|---|---|---|---|
| **[`c/`](c/)** | CSV, TSV, ndjson, Parquet (uncompressed, snappy, lz4; string columns only) | fastest on generated data in most published tables; lowest peak memory on Parquet; `--max-memory MB` bounds what grows with the input; threaded on every path; writes CSV, ndjson and Parquet itself (`c/gen-data`) | no HTML report, no row export; `--trim`, `--ignore-case`, `--empty-is-null` and `--tolerance` on text only (Parquet refuses them by name), and `--ignore-case` is ASCII-only; gzip and zstd; **typed Parquet columns** — so it refuses every file pyarrow, Polars, DuckDB or pandas writes by default ([R-002](FINDINGS.md#real-world-files)) |
| **[`cpp/`](cpp/)** | CSV, TSV, ndjson, Parquet (uncompressed and snappy; string columns only) | the full normalisation flags; `--ignore-case` is ASCII-only | no HTML report, no row export; no codec but snappy; typed Parquet columns, as C |
| **[`rust/`](rust/)** (compara) | CSV, TSV, ndjson, Parquet (uncompressed, snappy, gzip, zstd, lz4; string, boolean, integer, float, date, time, timestamp and decimal columns) | the full contract with the **HTML report** and uncapped row files (`--export-dir`); engines `turbo` (default), `sortmerge` (spills to disk) and `native`; the only port that reads every default writer's Parquet on its columnar path | brotli, and LZO |
| **[`zig/`](zig/)** | CSV, TSV, ndjson, Parquet (uncompressed, snappy, gzip, zstd, lz4) | `--max-memory MB` is **enforced** by a fixed buffer, not hoped for | no HTML report, no row export; typed Parquet columns through its row reader only; brotli, and LZO |

No port reads brotli or LZO; Zig's codec table names both as unsupported and
Rust's rejects brotli by name. C reads snappy and LZ4 and refuses gzip and zstd:
the first two are byte-copy loops written out in `c/parquet.c`, and the other
two are real decoders that would mean a dependency this port does not take. The codec lists above said otherwise until a
reader's zstd file was refused — see **Parquet codecs** below.

**Two ports take a memory ceiling, and they mean different things by it.** Zig's
`--max-memory` is enforced by a fixed buffer: nothing outside it is allocated at
all. C's is a declaration made before the allocations that scale with the input
-- the per-row index arrays and the Parquet column buffers -- so it bounds the
part that grows rather than the whole process, and the mapped files are not in
it. Both turn "the machine ran out" into an error naming the ceiling. Neither is
on by default. The Rust port's `--memory` is a plan rather than a ceiling: it
decides when a large text pair is joined in one pass instead of two and how many
Parquet columns are read at once, and refuses a Parquet pair that needs more
than a stated budget before reading it.

That distinction is worth the sentence because checking what `malloc` returns is
necessary and not sufficient. Under Linux's default heuristic overcommit there
is a band -- on a 15 GB machine, around 14 GB -- where `malloc` hands back a
pointer and the kernel kills the process when the pages are touched: exit 137,
no message, nothing to catch. A ceiling declared up front is the only thing that
turns that into a diagnosis.

Every port builds from its own toolchain alone, in seconds, and carries no
runtime dependency with a comparison engine in it. What was removed to get
there, and what it measured before it went, is in
[FINDINGS.md](FINDINGS.md#implementations-no-longer-carried).

### Parquet codecs

The Rust and Zig ports each carry **two** Parquet readers, and which one runs
matters. The table below is Rust's; Zig splits the same way and routes the same
way, and its report names the reader that ran too. Zig's columnar path reads
string columns only, so every typed file goes to its row reader.

| | Reads | Speed |
|---|---|---|
| the columnar path (`parquet` in the report) | uncompressed, snappy, gzip, zstd and lz4; string, boolean, integer, float, date, time, timestamp and decimal columns of up to eight bytes; v1 pages; PLAIN and dictionary encodings | the fast one — it joins on key columns and compares whole columns, a dictionary id or a word per value, without ever building a row; `--export-dir` writes the row files from it too |
| `turbo` (`turbo` in the report) | the above plus v2 pages, the delta encodings, and a column the two files store as different types | decodes pages into rows; **3.7x** the columnar path's wall time on a pair both can read |

Every file pyarrow, Polars, DuckDB and pandas write by default stays on the
columnar path ([R-002](FINDINGS.md#real-world-files)). Until 2026-10-15 a
zstd file did not — and zstd is what Polars writes unless told otherwise.

**A codec costs almost nothing; the fall-through costs 3.7x.** Held to one
reader, snappy is 1.10x an uncompressed run, gzip 1.19x, lz4 1.04x, and zstd
1.00x while reading 3.82x fewer bytes — so the 3.5-4x a default zstd run shows
is the routing below, not the decompression. Write zstd. The numbers are in
[BENCHMARKS.md](docs/benchmarks/2026-W37.md#2026-09-09-codecs--what-compression-costs-and-what-the-fall-through-costs).

By default a Parquet pair goes to the columnar path, and **falls through to
`turbo` when that path does not read the file** — a codec it does not carry, a
column type it does not decode. `--engine turbo` takes it there directly. The
report's engine field says which one ran, so a run that fell through is visible
rather than silent.

That fall-through is new. Before it, a real file — NYC TLC trip data, zstd —
was refused outright:

```
error: the parquet engine failed: only uncompressed and snappy parquet are read here: …
```

by a binary that reads zstd perfectly well, with `--engine turbo` ignored
because the Parquet-pair branch ran before the engine was consulted. A refusal
on capability grounds now routes; a corrupt file or a missing key column still
fails, and says so as the columnar path's own error.

### Duplicate keys

Every port joins on the **first occurrence** of each key, counts *keys* rather
than rows, and reports duplicates as their own section. That is a choice, and a
stated one: a plain `FULL OUTER JOIN` multiplies duplicates into the diff
instead, silently. See [FINDINGS.md](FINDINGS.md#the-field-measured-once-2026-survey).

---

## Working on it

```bash
(cd c && bash test.sh)                # 42 checks, a few seconds, no other toolchain
(cd c && bash test.sh --with-ports)   # adds the cross-port oracles: 60

# the data, in any of the three formats, on every core
c/gen-data --rows 10m --out-dir /tmp/d --prefix p [--format json|parquet] [--threads N]

# two builds of one engine -- the inner loop of working on a port
scripts/bench_ab.sh old/csvdiff new/csvdiff -- compare A.csv B.csv -k id
scripts/bench_ab.sh --self-test c/csvdiff  -- compare A.csv B.csv -k id

# every port that reads the pair, interleaved, with a counts gate and peak RSS
python3 scripts/bench_ports.py A.csv B.csv --repeats 5
```

### Measuring, and what this machine will let you see

`bench_ab.sh` answers "did that change pay?" and is the one to reach for while
working. It runs both builds once per round and reports the **median of the
per-round ratios**, with the middle half of them beside it; where that half
straddles 1.00x it says there is no result rather than leaving a ratio to be
argued about.

That design is not taste. Running all of one build's rounds and then all of the
other's — which is what `hyperfine` and most harnesses do — reads **two copies
of the same binary as 15.8% apart** on a shared runner, and going from five
rounds to fifteen makes it *worse*, because what a shared machine does is drift
rather than jitter and averaging does not touch drift. Comparing the two builds
inside each round does: the same A/A test then reads 1.01x. `--self-test` runs
exactly that, one build against a copy of itself, so the claim can be rechecked
on whatever machine you are on rather than believed. The numbers are in
[BENCHMARKS.md](BENCHMARKS.md#how-to-read-these).

`hyperfine` is still worth having installed for a quick look — its warmup,
standard deviation and `1.07 ± 0.20 times faster` are all better than a bare
ratio, and that ± is the honest part. Just do not read its point estimate on
anything under about 1.2x here: on two identical binaries it named a winner
twice out of three and changed its mind about which.

`c/gen-data` writes the same bytes as the C++ generator and is checked against
it on sixteen shapes, including every thread count — it renders rows in waves
across all cores, and a threading bug that shifted one row would produce a file
that is still valid, still parses, and is wrong. Five million rows of CSV take
3.94s on four cores; the numbers are in [BENCHMARKS.md](BENCHMARKS.md).

`scripts/bench_ports.py` also takes ports built elsewhere, through
`CSVDIFF_PORTS_EXTRA` — a JSON array of `[label, path, flags]`. That is how a
branch's build gets measured against this one on the same rows in the same
sitting, which is the only way two builds can be compared at all. The benchmark
workflow uses it: `alt_ref` checks a second branch out beside this one, builds
its ports, and gives each its own column.

Every port has a `test.sh` holding it to the Rust port's answers on
`tests/fixtures/awkward_*.csv` — a fixture built from every shape that has
broken an engine here: non-ASCII case folding, a Kelvin sign that folds from
three bytes to one, doubled quotes as both value and key, CRLF, ragged rows, a
blank row, and a short key in the last bytes of the file.

| Workflow | Runs |
|---|---|
| `ci-c.yml` | the C port on gcc and clang, sanitizers, and the cross-port checks |
| `ci-rust.yml` | compara: `fmt`, `clippy -D warnings`, `cargo test` on Linux x86 and arm, macOS and Windows, its three engines agreeing on 200k rows of CSV and of TSV (counts, columns and row files), `cargo audit`, the minimum Rust version |
| `parity.yml` | every port returns identical counts on one dataset in CSV, TSV, ndjson and Parquet, TSV gives each port's CSV answer, and every generator writes byte-identical files |
| `formats.yml` | every port reads every format the same way, with mixed pairs (Parquet against CSV, TSV against CSV) |
| `conformance.yml` | fuzzed inputs built to break parsers, every port, every `--json` held to the schema |
| `realworld.yml` | Parquet as pyarrow, Polars, DuckDB and pandas write it by default, and TSV: every port and DuckDB, Polars and pandas on the same files, held to one answer — weekly and on demand ([R- rows](FINDINGS.md#real-world-files)) |
| `bench-2m.yml` | two million rows, every port and format (CSV and TSV in one sitting), on every pull request |
| `benchmark-native.yml` | on demand only: one tree, or this tree beside another branch's for an A/B |

The generator carries money in integer cents and applies drift to those
integers, never to a float, so byte-identity between generators does not depend
on any language's floating-point rounding.

```
c/     the leading port on all three formats: CSV, ndjson, Parquet, plus gen-data and test.sh
cpp/   the C++ port, and the generator that also writes Snappy
rust/  compara: the full contract, the HTML report and row exports
zig/   the enforced memory budget
scripts/bench_ab.sh      two builds of one engine, paired by round, with a no-result verdict
scripts/bench_ports.py   every port on one pair, interleaved, with a counts gate
scripts/bench_scale.py   one engine across every size, generating and deleting in turn
scripts/realworld.py     real writers' Parquet and TSV, every port and DuckDB, Polars, pandas
tests/fixtures/          every shape that has broken an engine here
```

---

## What's open

The current list is the `E-` table in [FINDINGS.md](FINDINGS.md#open-experiments),
ranked by what each is expected to be worth to compara. What follows is the list
as it stood on 2026-10-02, before the Rust port became compara, kept for the
measurements in it.

### What was open on 2026-10-02

1. **All four ports settle an ndjson row from its bytes now.** Settling a matched
   row without parsing it is not C's alone for CSV, and since today it is not
   C's alone for JSON either. The JSON form is harder: a value is found by name,
   the two files need not list the names in the same order, and a name repeated
   in one object takes its *last* value — so a second `"amount"` past the
   diverging byte carries a value the proven prefix never saw. A bounded scan of
   the mate's tail rules that out, and it is cheap because by then only the
   trailing ignored columns are left.

   One table, 4M rows a side on a 4-core host, best of three, all five rows from
   one sitting:

   | Port | Wall | CPU | Cores |
   |------|-----:|----:|------:|
   | Rust | **1.84s** | 6.02s | 3.27x |
   | Zig | 2.05s | 7.38s | 3.60x |
   | C | 2.42s | 6.70s | 2.77x |
   | C++ (now) | 4.35s | 11.54s | 2.65x |
   | C++ (before) | 5.03s | 13.68s | 2.72x |

   Rust gained 14% of CPU, Zig 19%, C++ 15%; C had it already. Counts identical
   across all four ports on a fixture built for the ways this goes wrong.

   **Where it goes is not the same in every port, and that is the finding worth
   keeping.** The proof only pays if it runs before the mate's row is parsed. C
   and C++ compare keys with a key-only parse, so their proof sits after the
   lookup and saves the full parse that follows. Rust and Zig materialise the
   mate's whole row *inside* the lookup to compare its keys, so a proof placed
   there saves nothing — put after it, it cost 2% instead of saving 14%, which is
   what the first Rust attempt measured. Theirs had to move inside the lookup,
   and to cover the keys as well (`width`, not `nc`) to be allowed to.

   What is left on ndjson is not the proof, and it is now located rather than
   guessed at. **C++'s JSON sweep costs about 1.9x Rust's** — eight runs of each
   at 4M rows, C++ 2.20-2.29s a side against Rust's 1.12-1.19s — which is the
   largest phase in both ports and most of the gap. The join is second. C++ has
   phase timings on the text path now, under the same `CSVDIFF_PHASES` switch the
   other three use, which is what made that readable; before it, the only port
   that could not be profiled was the one slowest on this format.

   Two explanations for that sweep gap have been tested and are wrong: it is not
   the vector scanner (C++'s AVX2 build is *slower* on ndjson — vectors pay for
   delimiter runs, not for JSON's short hops between quotes) and it is not the
   early exit in `parse_json` (removing it changes nothing). BENCHMARKS.md has
   both. Finding the rest wants a profiler, not a third hypothesis.

   Take the *sweep* ratio and not the other rows of that table. A first reading
   of it also reported Rust's serial index insert at 0.668s on the B side against
   0.267s on A, and that was noise: repeated, the two sides come out 0.778/0.812,
   then 0.161/0.158, then 0.601/0.319 with A the slower one. The insert is small
   and scheduling-bound, and which side loses flips. The sweep gap is the one that
   reproduces.

2. **Withdrawn: the Rust Parquet reader does not wait at fifty million rows.**
   This item ran for three rounds on one observation. Re-measured with
   `--repeats 3`, the 50M Parquet rung is:

   | Build | Compare |   CPU | Cores | Above the input |  Budget |
   |-------|--------:|------:|------:|----------------:|--------:|
   | C     |   8.65s | 25.2s | 2.92x |        6,905 MB | 7,972 MB |
   | C++   |  13.83s | 44.4s | 3.21x |        6,934 MB | 7,941 MB |
   | Rust  |  11.97s | 40.6s | **3.39x** |    7,169 MB | 7,916 MB |
   | Zig   |  12.56s | 44.2s | 3.52x |        6,663 MB | **9,217 MB** |

   Rust at **3.39x cores and 11.97s**, against the 1.04x and 29.70s this item was
   built on. It is the second most parallel of the four, not the least.

   The giveaway is the C column. C's Parquet path has not changed between the two
   runs, and it went from 16.60s to 8.65s — 1.9x, on identical code. The whole
   table was slow, not the Rust row in it, and one run at `--repeats 1` on a
   hosted runner is what recorded it. Rust's own reader did gain the key-column
   fan-out in between, but that measured 1.08x on an 8M pair and cannot account
   for 29.70s becoming 11.97s.

   So there is nothing here to explain. What the three rounds produced instead is
   worth keeping, and is elsewhere: the columnar path had no phase timings and now
   has them, its key read was two jobs whatever the key and is now a queue,
   `--threads` did nothing on that path and now does, and the memory cap turned
   out to bound a quantity the table was not reporting — which is item 3.

   The lesson is the one this file has now learnt three times. A single
   observation on a shared runner is not a finding, and the cost of treating one
   as a finding is measured in afternoons. `--repeats` is cheap.

   One row of that table does still say something: **Zig's budget is 9,217 MB
   against the other three at about 7,940**, while holding the *least* above the
   input. That is the `VmData` gap of item 3, reproducing at 50M on Parquet, and
   it is the one number in this table that is about a port rather than a runner.

3. **The ceiling is not one number, it is four.** This line used to say "CSV
   finishes at 150M and dies at 200M", as a fact about the tool. It is a fact
   about one run. On a 16 GB runner with each port capped at 13,941 MB
   (`RLIMIT_DATA`), the 2026-09-14 run had only C finish at 150M. Two weeks of
   memory work later, the 2026-09-28 rerun under the same cap had **all four
   ports finish** with identical counts:

   | port | wall | peak RSS |
   |------|-----:|---------:|
   | Rust | 289s | 13,881 MB |
   | C    | 335s | 14,106 MB |
   | Zig  | 343s | 15,007 MB |
   | C++  | 583s | 13,939 MB |

   The 09-14 table below is kept for the archaeology — it shows where the
   ceiling *was*, not where it is:

   | rows |  C  | C++ | Rust | Zig |
   |-----:|:---:|:---:|:----:|:---:|
   | 100M | yes | yes | yes  | **no** |
   | 150M | yes | no  | no   | no  |

   Each refusal names what did not fit: Rust's `one hash per row needs 1144 MB`
   is 150,015,000 × 8 bytes. Where C's own ceiling sits is untested and stays
   that way here — one port's ceiling is not the tool's, which is the mistake
   this item is correcting.

   **Why Zig goes first is settled, and it is not that it holds more.** It holds
   less than Rust. `--memory-cap` sets `RLIMIT_DATA`, which bounds `VmData` — the
   *virtual* size of the private writable mappings — while the table reported peak
   RSS, which counts the mapped input and misses a reservation never touched. On a
   6M CSV pair the four want 448, 747, 748 and 955 MB of `VmData` while holding
   405, 511, 697 and 592 MB above the input, and each one's refusal threshold
   tracks the first number. Zig reserves 61% more than it touches, against Rust's
   7%, in a 236 MB transient at the sweep-to-index boundary that is then given
   back. Scaling those four figures to the rungs puts every port on the right side
   of the cap in all eight cells. The harness prints a `Budget` column now, because
   without it the table could not explain its own refusals. Narrowing that
   transient — the index reserves its per-row arrays beside the sweep chunks that
   already hold two of them — is the next thing to do on this item.

   **Parquet finishes at 50M for all four ports and at 100M for none of them**,
   and 150M is none of them too. Rust's 100M refusal says `one dictionary index
   per row needs 381 MB`, exactly 100,005,000 × 4 bytes. So the Parquet ceiling is
   between 50M and 100M and the rungs between are still undispatched. That is also
   where `sortmerge` stops being the conservative choice and becomes the only one.

   Two things these rungs expose about the harness rather than the ports. Cores
   collapse to 0.45–1.11x, because a 35 GB input cannot stay in a 16 GB page cache
   and every port spends most of its wall clock reading — so nothing in those
   tables compares engines. And "Above the input" goes **negative** (-21,649 MB at
   100M), because it is peak RSS minus the input size and there is no such
   quantity once the input passes the host's memory. Both columns want suppressing
   past that point; BENCHMARKS.md has the detail.
4. **What compression saves in bytes read is still unmeasured**, though what it
   *costs* now is. The container these numbers come from cannot answer the other
   half: `drop_caches` leaves the hypervisor's copy warm, and the one genuine
   cold read available ran at about 18 MB/s. On a host with a characterisable
   disk, zstd reading 281 MB where uncompressed reads 1,073 MB is worth whatever
   that difference costs there.

**All four ports now refuse a budget they cannot meet**, and all four exit 2, so
*how* the Rust port fails is off this list — what it costs is item 2 above, and
the two are not the same thing. It died on signal 6 because `Vec::with_capacity`
ends in `handle_alloc_error`, which aborts and cannot be replaced on stable Rust.
The allocations that scale with the input go through `rust/src/alloc.rs` now and
return instead, naming what did not fit — `out of memory: the key index needs
32 MB`. Two worse failures turned up behind it while walking a cap down one rung
at a time: `Scope::spawn` *panics* when the OS refuses a thread, which under a
memory cap is exactly when it happens, because a thread's stack is a private
mapping and that is what `RLIMIT_DATA` bounds; and printing that panic with
`RUST_BACKTRACE=1` set deadlocked against the backtrace lock, so the process
hung — on a benchmark runner the one outcome worse than an abort, because nothing
after it runs, not even the rung reporting that it failed. Both are fixed, and
`rust/tests/out_of_memory.rs` walks the ladder on every build. Cost: four paired
A/B runs, three formats at 2M and CSV again at 6M, all four "no result" — the
checks do not show up.

One band is left and it is a property of the language rather than of this code: an
individual `String` has no fallible form, so a budget that fits the comparison but
not the report's row samples still aborts. The samples are capped by `--max-rows`,
which makes that band a constant rather than a function of the input. Measured
on a 2M-row CSV pair by walking the cap in 20 MB steps: 320 MB and above answers,
220 MB and below refuses, and the rungs between abort on a `String` of six to
seventeen bytes. Invisible against a 13 GB comparison at 50M, which is why the
Parquet ladder is clean at every rung from 4 GB down to 4 MB.

**Zig builds and runs on Windows**, so that is off this list too. Both blockers
it named are gone: `src/slab.zig` has a `CreateFileMappingW` / `MapViewOfFile`
backend beside the `mmap` one, and `src/main.zig` reads `CSVDIFF_PHASES` through
`environ.contains` rather than the POSIX-only `getPosix`. It is not an inference
from the source either — `platforms.yml` builds all four ports on Linux x86 and
arm and on macOS, builds C, C++ and Zig natively on `windows-latest`, and checks
each one's answers against the Rust port's on the same file. Fifteen jobs, green.

**Zig's zstd has an explanation now**, and it is not in this repository:
[BENCHMARKS.md](BENCHMARKS.md) has it. Twenty-five stack samples land in
`compress.zstd.Decompress`'s own decode loop — `readInFrame`, `decodeLiterals`,
`HuffmanTree.query` — not in the reader around it, and the two candidates in our
code priced out at 1.03x cpu and nothing at all. Rust's zstd is `zstd-sys`, C
libzstd 1.5.7; Zig's is std's pure-Zig decoder in a port that deliberately links
no libc. So the wall-clock half is a decision — give that up, or write a zstd
decoder — rather than a mystery. The investigation did find a real bug, and it
was a memory one: a sliding window allocated per page, worth 3x on `--max-memory`
and nothing on the clock.

`--ignore` used to be on this list, accepted in silence in all four ports where
`--key` and `--compare` refuse. It is an error now in all four — a name that
*neither* file has, since `--ignore` is subtractive and a name only one side
carries is real. The check found a `-i x,y,z` against a `z2` column in this
repository's own C suite, where the counts came out the same either way and
nothing could see it.

**Compression has been measured off this list**, and the numbers are in
[BENCHMARKS.md](docs/benchmarks/2026-W37.md#2026-09-09-codecs--what-compression-costs-and-what-the-fall-through-costs).
The short answer is that a codec costs between nothing and 19% of wall time with
the reader held still, and zstd costs nothing measurable while reading 3.82x
fewer bytes. The 3.5-4x a default run shows for gzip, zstd and lz4 is the
fall-through to the row reader, not the codec.

Three other things have been measured off it, and those numbers are in
[BENCHMARKS.md](docs/benchmarks/2026-W37.md#2026-09-09-profiling--three-questions-and-what-the-answers-cost):
the serial insert (blocked by allocation rather than ordering, and 1.4% of a
200-column run), ndjson's cost per byte (the format, not a defect — name lookups
are 12% of a 46% gap), and wide files (throughput flat from 20 columns to 200,
and the SIMD question does not re-open there).

## License

MIT — see [LICENSE](LICENSE). Contributions are welcome; see
[CONTRIBUTING.md](CONTRIBUTING.md).
