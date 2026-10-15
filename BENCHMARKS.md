# Benchmark history

Every run this project has kept, newest first: the methodology and the latest entries
here, everything older in [docs/benchmarks/](docs/benchmarks/) (indexed at the bottom of
this file). Each entry names the host, the
date and the way it was measured, because that is what makes two numbers
comparable — and what makes numbers from two different entries **not**
comparable. Compare rows within a table. Never across tables.

Terse verdicts on things the project no longer carries, and every finding
these entries led to, are in [FINDINGS.md](FINDINGS.md).

## How to read these

**One CPU per table, and CI does not give you that for free.** Every number is
only comparable with another taken on the same processor. On GitHub's hosted
fleet that is not a formality: one `ubuntu-latest` label covers several
generations, so two jobs in one workflow run can land on a Xeon Platinum 8370C
and an EPYC 7763 — different cache, different core layout, a different AVX-512
story. A ladder that runs one size per job, or a matrix that runs one format per
job, produces pieces measured on *different machines*; read as a single curve
they measure the fleet as much as the code. So `bench_formats_ports.py` records
the CPU in its JSON beside the numbers, and `scripts/bench_group.py` groups on it
and prints one table per processor, naming any rung that is missing from a group
rather than filling it in from another. A run whose summary shows two CPUs has
produced two tables, whatever it looks like.

**Interleaved, not one build at a time.** A machine's speed drifts under the
runs themselves — the page cache fills, the kernel's supply of free 2 MB pages
is picked over. Every build runs once per round and the rounds repeat. A number
taken now and one taken twenty minutes ago compare machine states, not builds.

**The starting build rotates between rounds.** In a fixed order one build is
always first into a cold page cache and one is always last, and that position
quietly becomes part of its number. Rotating the start spreads both ends over
every build instead of assigning them. It also decides who survives a run that
is killed partway: a table that always ran the ports in the same order left the
ones at the back with no measurement at all, which reads like slowness and is
not. Tables are still printed in the declared order, so two runs can be read
side by side, and each format's rows are written out as soon as they are
measured rather than at the end.

**Best, median and worst.** A build that is quick once and slow twice is not
quick, and the spread is where memory pressure shows.

**CPU is the honest measure of work.** Wall time mixes work with how many cores
the design manages to use; CPU seconds do not. Where a build wins on wall and
loses on CPU, it is winning on threading.

**Peak RSS includes the mapped input.** These engines map their files, so
resident pages include the files themselves. The column that carries
information is *above the input*, which subtracts them.

**A negative *above the input* is not a benchmark.** It is the same column read
the other way: a port that peaked *below* the bytes it mapped did not hold its
own input, because the kernel was taking pages back while it was still using
them. Every time in such a row is a page-fault ranking, not a parsing one, and
does not belong beside a rung that fit. `bench_formats_ports.py` now says so
before the rung runs -- it refuses to call a pair a benchmark once it passes
80% of the machine's RAM, leaving room for the indexes the ports build on top
-- and `bench_group.py` says so again afterwards, naming the rows that did it.
The rung still runs either way: which port degrades worst under paging is its
own kind of answer, and a number with a stated caveat beats no number.

**Counts are gated, not assumed.** Every run below ends with every build
returning identical counts. A build that disagrees fails the run and is named;
none of the tables here contains a build that was fast because it was answering
a different question.

**Same counts is not the same task.** The gate above proves the ports agree on
*what changed*. It says nothing about what else they were asked to produce, and
for most of this file's life they were not asked for the same thing. Every
harness passed `--json` to all four ports, and only the C++ port emits row
samples: C and Zig write `counts` and `columns` -- 1,196 bytes on a 4M pair --
Rust adds a `meta` block for 3,057, and C++ writes **4,917,335 bytes naming
58,600 rows**. C has no flag to turn samples on, because it has no such feature.

So `--json` cost C 39,250 instructions on a 400k pair, 0.0%, and cost C++ 808
million, 44%: it switches on a second full random-probed pass over B and then
materialises, sorts and writes every sampled row. Measured on a 4M pair at four
threads, warmed and with the two modes interleaved, **C++ is 1.81x C on the task
all four ports perform and 2.25x once C++ alone is asked for a report** -- about
a third of the published CSV gap was the report.

The timed runs no longer pass `--json`. The counts gate still needs the
document, so it gets its own untimed run first. `scripts/report_cost.py` prints
what each port's document contains beside what producing it costs, in that
order, because the shape is the reason for the cost. This is the same fault as
the `-march=native` one below, and `ports()` already fixed the matching case for
Rust by passing `-o /dev/null` so its HTML was not charged against ports that
render none; C++'s samples were the half that was missed.

**What this harness can and cannot resolve.** Measured, not assumed: running one
build against a copy of itself, where the true answer is 1.00x.

| Comparing two builds by | Same build twice reads as |
|---|---:|
| separate bests, one build's runs then the other's (hyperfine's model) | 15.8% apart, worst of eight |
| separate bests, interleaved rounds | 8.3% apart, worst of eight |
| separate bests, interleaved, 5 rounds → 15 rounds | 9.1% → **9.4%** |
| the median of the per-round paired ratios | **1.01x** |

The third row is the one that matters: three times the rounds did not help,
because what this machine does is *drift*, not jitter, and averaging does not
touch drift. Comparing the two builds *within* each round does, because the two
runs are seconds apart under one machine state. `scripts/bench_ab.sh` does that
and prints the middle half of the per-round ratios beside the median; where that
half straddles 1.00x, there is no result to report.

So: a difference under about 10% is invisible to a ratio of bests here, and
about 3% is the floor for the paired one. Ratios below those are the machine.

**Every cross-port table before the 11:23 run was built unfairly**, and is kept
rather than deleted because the numbers within each port are still that port's.
C and C++ carried `-march=native`; Rust and Zig were given a generic baseline,
which compiles their wide scanners out entirely — the Rust one is selected by
`cfg!(target_feature = "avx2")` and the Zig one by the cpu it is handed. So the
C-versus-Rust and C-versus-Zig ratios in those tables are partly build flags,
and correcting it cost about two thirds of the published C lead on CSV and on
Parquet. Every port is now built for the runner it is measured on. It was the
other branch's record that pointed this out.

---

## 2026-09-30 (c++ guessed splits) — "chunk bounds" was a disk read the sweep repeated

C's change above, ported to C++ (#221). C++ counted quotes over the front of
each file before either sweep began: at its two chunks a file, half the file on
one thread. At 150M, 26 GB a side against ~16 GB of RAM, that was a serial disk
read which the sweep then repeated once the pages were evicted. The splits are
now guessed at the next newline and checked after the sweep.

Locally, cold, 5M pair, memory cgroup capped at 600 MB (400 MB kills the old
build, which holds both indexes at once): storage reads 2.52x input -> 2.02x on
every run; both indexes 1.53x [1.37–1.63], wall 1.20x [1.14–1.29], against an
A/A of 1.01x [0.97–1.06] and 1.01x [0.98–1.03]. Warm, `bench_ab.sh` does not
tell the builds apart on the 5M or the 200k pair.

**On CI, 150M.** `sizes=150m`, `mem_cap_mb=13941`: 36656925225 before, and
36667822301 with #221. One run each, each port on its own runner.

| C++, phase run | before | after |
|---|---|---|
| chunk bounds, A / B | 61.2s / 65.0s | 0.001s / 0.000s |
| sweep, A / B | 131.7s / 129.0s | 130.0s / 130.0s |
| join and compare | 131.3s | 131.4s |
| phase-run total | 334.1s | 271.9s |
| **wall, timed run** | **354.4s** | **297.9s (1.19x)** |

The sweeps did not get slower for starting cold: the counted region was being
read twice, and now it is read once. The join, untouched, is the same to a
tenth of a second, so the runners look alike. For scale, C's timed wall was
272.8s in the earlier run and 287.5s in this one on the same code, which is
the runner-to-runner spread; 354 to 298 is well outside it.

The same run, all four ports: Rust 269.0s and C++ 271.9s (phase-run totals),
C 287.5s and Zig 335.1s (timed runs). Zig still counts quotes inside its sweep
timer; the same change applies there.

---

## 2026-09-30 (c guessed splits) — the C sweep read half of every file twice once it outgrew memory

The one live lead left on C at 150M was its sweep on cold reads. The CI phase
run (36634384428, 26 GB a side against ~16 GB of RAM) shows C spending about
138s *before* its sweep clock started: `chunk_bounds` counts quotes on one
thread from the start of the file to the last split, so the CSV split cannot
land inside a quoted newline. In memory that is a quick byte scan. On an input
larger than memory it is a serial disk read of the counted region, and by the
time the sweep reaches those bytes they have been evicted, so the sweep reads
them again. Rust does not count: it guesses each split at the next newline and
checks after the sweep that each chunk ended where the next began, counting
only when a guess was wrong. C now does the same.

Reproduced here by capping the process's memory cgroup below the counted region
(400 MB against a 5M pair of 0.9 GB a side, cold, both indexes built one after
the other as the >8 GB path does):

| | before | after |
|---|---|---|
| bytes read from storage, every run | 4.63 GB (2.52x input) | 3.71 GB (2.02x input) |
| both indexes, 15 interleaved rounds | 5.32s | 4.30s — **1.21x [1.19–1.28]** |
| wall | 8.52s | 7.56s — **1.11x [1.10–1.13]** |
| A/A, same harness | | 1.00x [0.98–1.04], wall 1.02x [0.995–1.05] |

The 0.5x of input removed is the counted half of each file (two sweep threads a
file on that path). Warm, `bench_ab.sh` does not tell the builds apart on the
5M or the 200k pair.

**On CI, 150M.** Two scale-ceiling runs the same night, `sizes=150m`,
`mem_cap_mb=13941`: 36655830861 on main just before this change, and
36656925225 with it. One run each, each port on its own runner. The join, which
this does not touch, took 131.1s and 131.2s, so the two C runners look alike.

| C, phase run | before | after |
|---|---|---|
| before the sweep clock, A / B | 30s / 33s | 0s / 0s |
| A: count + sweep | 30 + 35 = 65s | 65s |
| B: count + sweep | 33 + 67 = 100s | 71s |
| both indexes | 176.0s | 147.2s |
| join and compare | 131.1s | 131.2s |
| **wall, timed run** | **305.8s** | **272.8s (1.12x)** |

All of the saving is on B. A's counted pages were still in memory when its
sweep reached them, so A cost 65s either way. By the time B was built, A's index
held that memory, B's counted half had been evicted, and the sweep read it
again: the case this removes. The ~138s above was the older layout, both
indexes at once; after #214 the prelude was ~63s, and it is now gone. The same
run, all four ports: Rust 267.3s (its phase-run total), C 272.8s, Zig 339.0s,
C++ 354.4s, which still spends 61s and 65s in "chunk bounds" before its sweeps.

The remaining 1.0x of re-reading is the join, which walks B in A's order.
C++ and Zig still count before their sweeps; the same change applies to both.
The >8 GB sequential path also still gives each build half the threads.

---

## 2026-09-30 (reverted: zig key-list reservation) — #213 cost memory for a gap that was not there

#213 reserved `first_row` and `occurrences` to `total` in `RowIndex.build`,
to close a ~24s insert gap to Rust in the 150M CI run. That gap was two
machines compared: on one machine Zig inserts about 1.15x faster than Rust.
Measured against its parent (b432409) on a 5M CSV pair, one machine:

| | parent | #213 |
|---|---|---|
| peak `VmData`, every run of six | 402 MB | 430 MB (+7%) |
| peak RSS (the mapped inputs dominate it) | 2049 MB | 2049 MB |

`bench_ab.sh`, 11 interleaved rounds: by default "5% less work", with a middle
half reaching only 1.02x, against a `--self-test` of 1.00x [0.95–1.10] on the
same machine; with `-i updated_at`, no result. The CPU side was not told apart,
and the memory side is what #73 measured for the same configuration in
September (key lists reserved: 790–815 MB against 735–761 MB with none, on 6M),
and the number that decides how many rows this port can do. Reverted; the
comment in `RowIndex.build` records it so it is not tried a third time.

---

## 2026-09-28 (zig guard span) — the Zig join stopped parsing rows the proof settles

The C change from 09-27, ported to Zig. `Join.range` called `fieldsOf` on every
row of A before the byte proof; now `text.guardSpan` counts the guard column's
delimiters with the delimiter cursor alone — no slot map, no field packing —
and rows the bytes settle never parse. On the 2M CSV pair 93.8% of attempted
rows prove out, matching the 94% identical rows in the data.

One bug found by the numbers, not the compiler: the first version returned
`cur + 1 - lo`, including the delimiter in the span, the way C's does — but
Zig's `rowMatches` expects the span to end at the field boundary and checks the
*next* byte is a delimiter, so the proof never fired (`proved=0` on every
row). Corrected to `cur - lo`; the proof then succeeded on 60,526 of 64,516
attempts per thread.

`bench_ab.sh`, 2M rows CSV, `--threads 1`, 7 interleaved rounds, two binaries.
Paired ratio (baseline / guard_span): **1.10x [1.09–1.12] CPU, every quarter
above 1.00.** `--self-test` on the same machine: 1.00x [0.96–1.02] — the gain
is well above the floor. All 44 `zig/test.sh` checks pass; counts identical
(matched 1,998,000, changed 119,625, added/removed 2,000 each).

That closes the port set: guard_span now ships in C (#178), C++ (#179), Rust
and Zig. It pays where the parser is expensive (C, C++) and where the join
parsed every row regardless (Zig); Rust's parser was already cheap enough that
the counting ate the win.

Committed as `f39da04`, pushed straight to main.

---

## 2026-09-28 (rust and zig u32 hash slots) — the index stopped spending 64 bits per slot

Both ports kept one `u64` per hash-table slot: 40 bits of row position, the
rest hash tag. Forty position bits address a trillion rows; the largest input
this project has measured is 150M. C and C++ already sized their slots to the
row count. Rust (`9d89c95`) and Zig (`4f80fa2`) now do the same: the table is
`Vec<u32>` when 32 bits of position suffice, with the tag taking the high bits
that remain, and the build refuses rather than truncates past ~4 billion rows.

`bench_ab.sh`, 2M rows CSV, `--threads 1`, interleaved rounds, two binaries
each port. Paired ratios (baseline / u32):

- Rust: **1.04x [1.03–1.08] CPU** against a self-test floor of 1.00x [0.98–1.03]
  — at the edge of resolvability, kept for the memory: peak RSS 4,218 → 4,099
  MB (−119 MB on one side; the saving doubles at 50M).
- Zig: **1.07x [1.05–1.08] CPU** against a floor of 1.03x [0.99–1.04] — a real
  win, and larger than Rust's.

Counts identical in both ports. One bug caught before the push: Zig computed
`64 - tag_bits` into a `u6`, which cannot shift a `u32` — fixed with a guarded
`@intCast` to `u5` at every u32 shift site, `u6` kept where the shift is u64.
The bug survived this long because no local Zig toolchain existed to compile
it; CI caught nothing either, since the branch had never built Zig there.

## 2026-09-29 (phased 150M) — where the seconds go, all four ports

Run `36551803636`, `scale-ceiling.yml` with `CSVDIFF_PHASES=1` — one extra
run per port after the timed run, phases from the port's own instrumentation.
Caveat: the phase run did not pass `--summary` to Rust, so Rust's phases
include HTML report work the others skip; fixed after this run.

| Port | Sweep A | Sweep B | Insert A | Insert B | Join |
|---|---:|---:|---:|---:|---:|
| Rust | ~128s | — | 6.02s | 8.49s | 130.89s |
| Zig | ~141s | — | 26.51s | 26.63s | 161.59s |
| C++ | 131.52s | 129.94s | 6.71s | 9.78s | 131.83s |
| C | 215.51s | 208.99s | 5.94s | 20.08s | 202.13s |

(C++ also pays a chunk-bounds pre-pass, 61.45s + 64.50s, which the other
ports do not have; phases overlap, so the rows do not sum to the total.)

The sweep is I/O-bound and broadly similar — except C, whose 215s+209s is
~60% slower than the others for reasons not yet understood. The clearest
gap is Zig's serial index insert: 26.5s+26.6s vs Rust's 6.0s+8.5s, ~38s.
The insert functions are structurally identical (same probing, same
prefetch distance, same slot math) — the gap is in codegen or a subtle
effect invisible in the source, and local 10M A/B cannot resolve it. C's
insert is asymmetric (5.9s vs 20.1s); C's join is also slow at 202s vs
Rust's 131s.

## 2026-09-29 (parquet u32 hash slots) — the same conversion, three more ports

The CSV ports all had u32 slots by 09-28, but the Parquet ports were still on
64-bit: C `pqdiff`, C++ `pqdiff`, Rust `pqdiff`. Zig's `pqdiff` already had
u32 (`4f80fa2`) — which is why Zig led the 50M Parquet memory table in the
fourth ladder edition (6,201 MB above the input vs 6,953–7,178 MB for the
other three).

Commit `174c28f` converts all three to the adaptive u32 scheme: position bits
from the row count, hash tag in the high bits, refuse past ~4B rows. Same
pattern as the CSV conversion, same guarantee — slot width is not in the
comparison contract, so counts are identical by construction.

Tests: C 73 ok, C++ 25 ok, Rust 58 unit + 15 integration ok. No A/B on speed
yet — the local machine cannot fit a meaningful Parquet pair (2 GB free), and
the improvement is primarily memory: at 50M Parquet the slot table is ~400 MB
per side, halved by this change.

## 2026-09-29 (50M Parquet, post-u32) — Rust takes the table

Run `36560345563`, `bench-ladder.yml`, 50M Parquet, one sitting (EPYC 7763).
First measurement after `174c28f`:

| Port | Wall | CPU | Above the input |
|---|---:|---:|---:|
| Rust | 6.06s | 19.7s | 5,966 MB |
| Zig | 6.25s | 20.6s | 6,202 MB |
| C++ | 7.81s | 21.9s | 6,237 MB |
| C | 12.13s | 30.0s | 6,191 MB |

Pre-u32 (same workflow, 09-28): Zig 7.37s/6,201 MB, Rust 8.02s/6,953 MB,
C++ 8.43s/6,964 MB, C 8.88s/7,178 MB.

Rust is fastest *and* leanest in this run — 6.06s wall and the
smallest footprint at 5,966 MB. C and C++ closed Zig's former memory lead as
expected. C's 12.13s wall (vs 8.88s before) is runner noise, not a regression:
local A/B of the u32 change on 2M and 10M Parquet pairs shows no measurable
difference for C (harness: "no result" both times), and the change is
memory-only by construction.

## 2026-09-29 (capped 150M re-run) — the u32 slots do not move the ceiling

Run `36512611216`, same methodology as 09-28 (`mem_cap_mb=13941` via
`RLIMIT_DATA`, 52.6 GB CSV input, 16 GB runner). All four ports pass:

- Rust: 287s / 13,655 MB (09-28: 289s / 13,881 MB)
- C: 378s / 14,045 MB (09-28: 335s / 14,106 MB)
- Zig: 341s / 14,982 MB (09-28: 343s / 15,007 MB)
- C++: 363s / 13,985 MB (09-28: 583s / 13,939 MB)

The CSV u32 slots (Rust `9d89c95`, Zig `4f80fa2`, both merged 09-28 after the
previous capped run) do not move the 150M ceiling in any meaningful way —
times and peak RSS are in the same band. Expected: under a 14 GB cap with
52 GB of input, the run is bound by page-cache eviction and I/O, not by the
slot table. The u32 win lives at 10M–50M, where the table is a real fraction
of the working set. C++'s jump from 583s to 363s is runner variability on the
09-28 sitting (a throttled runner), not a code change — C++ CSV already had
u32 slots before both runs.

A drive-by fix rode with the Rust change: `--summary` no longer builds the
pick lists it then discards (an 11 MB abort on small caps).

---

## 2026-09-28 (withdrawn: the C sweep gap) — a 2.5x parser lead that was not there

An earlier entry in the working notes claimed the C sweep parsed 2M CSV rows in
0.165s against 0.385–0.425s for Rust, Zig and C++ — a 2–2.5x lead, and the
obvious next target. It does not reproduce. A fresh `phases_ports.py` run on
the same machine, five rounds at one and two threads, puts all four sweeps at
0.16–0.21s. The gap was a measurement artefact, most likely machine load during
the first sitting — the numbers were never re-run before being written down.

Withdrawn in full. There is no sweep investigation to continue.

What survived the re-measurement is the join gap on larger inputs: 10M CSV,
Zig 0.433s against 0.365s for the other three (+19%). That is what the Zig
guard_span entry above addresses.

---

## 2026-09-28 (rust hoist needs_normalising) — the sweep stopped asking about options it already knew

`hash_field` is called for every key field of every row — twenty million calls
on the 10M benchmark. Each call checked `needs_normalising(opt)` twice: once
via `is_absent`, once directly. Forty million checks of four booleans that
never change mid-run. C's `hash_field` takes no `opt` at all.

`key_hash` now computes the flag once and passes it down as a `bool`. The fast
path (no normalization, the common case) checks absence directly from the field
word, skipping the `is_absent` call entirely. The normalized path is unchanged.

`bench_ab.sh`, 10M rows CSV, `--summary`, interleaved rounds, EPYC 9D64.
Paired ratio (main / hoisted): **1.03x [1.01–1.10] CPU, in every quarter of the
rounds.** The self-test floor on this machine is ~1.07x, so this is just at the
edge of resolvability — a small, consistent win, not a breakthrough. (Absolute
timings are omitted: the two arms ran in different sittings, so their wall/CPU
numbers are not comparable — only the paired ratio is valid.) Output identical
on the 10M pair; all 58 `cargo test` checks pass; `fmt` and `clippy` clean.

Merged alongside the hoist, all in `rust/src/engine/turbo.rs`, each measured
neutral on its own paired run: unchecked indexing in `Index::lookup` (1.04x
[0.96–1.07]), combined `Chunk::push` (1.00x [0.96–1.01]), and a
`same()`/`plain_absent` fast-path (0.98x [0.95–1.01]). Kept for architectural
alignment with the C port, not for speed.

Probed and rejected in the same session: removing the matched-B bitmap entirely
(10M atomic `mark` ops + 1.25MB allocation + 10M-bit walk) measured 1.01x
[0.99–1.05] — the atomics are not the bottleneck. `#[inline]` on `key_hash`
measured nothing.

The remaining gap to C (Rust ~7.5s vs C 5.3s CPU on the same 10M pair) is
architectural.

---

<!-- archive-index:start (generated by scripts/archive_benchmarks.py) -->

## Earlier entries

Moved to `docs/benchmarks/` to keep this file readable; unchanged. `scripts/archive_benchmarks.py` does the moving.

### [2026-W39](docs/benchmarks/2026-W39.md), 2026-09-21 to 2026-09-27

- [2026-09-27 (cpp guard span) — the C++ join stopped parsing rows the proof settles](docs/benchmarks/2026-W39.md#2026-09-27-cpp-guard-span--the-c-join-stopped-parsing-rows-the-proof-settles)
- [2026-09-27 (c guard span) — the join stopped parsing rows the proof settles](docs/benchmarks/2026-W39.md#2026-09-27-c-guard-span--the-join-stopped-parsing-rows-the-proof-settles)
- [2026-09-27 (rust and zig cursor inline) — the delimiter cursor lived on the stack](docs/benchmarks/2026-W39.md#2026-09-27-rust-and-zig-cursor-inline--the-delimiter-cursor-lived-on-the-stack)
- [2026-09-27 (cpp wide row skip) — the sweep skipped each CSV row sixteen bytes at a time](docs/benchmarks/2026-W39.md#2026-09-27-cpp-wide-row-skip--the-sweep-skipped-each-csv-row-sixteen-bytes-at-a-time)
- [2026-09-27 (cpp delimiter cursor, fourth try) — C++ CSV 10–14% faster at one thread](docs/benchmarks/2026-W39.md#2026-09-27-cpp-delimiter-cursor-fourth-try--c-csv-1014-faster-at-one-thread)
- [2026-09-27 (compact ndjson separators, every port) — 2–10% on ndjson](docs/benchmarks/2026-W39.md#2026-09-27-compact-ndjson-separators-every-port--210-on-ndjson)
- [2026-09-27 (why clang 23 is slower; -O3, LTO and hardening) — measured, nothing adopted](docs/benchmarks/2026-W39.md#2026-09-27-why-clang-23-is-slower--o3-lto-and-hardening--measured-nothing-adopted)
- [2026-09-27 (clang 23 for c++, latest lockfile) — lockfile taken, C++ stays on clang 22](docs/benchmarks/2026-W39.md#2026-09-27-clang-23-for-c-latest-lockfile--lockfile-taken-c-stays-on-clang-22)
- [2026-09-27 (zig cc for c and c++) — measured, and not adopted](docs/benchmarks/2026-W39.md#2026-09-27-zig-cc-for-c-and-c--measured-and-not-adopted)
- [2026-09-27 (c with clang 22) — measured, and C stays on gcc 16](docs/benchmarks/2026-W39.md#2026-09-27-c-with-clang-22--measured-and-c-stays-on-gcc-16)
- [2026-09-27 (pinned compilers) — C and C++ were built with compilers years behind the others](docs/benchmarks/2026-W39.md#2026-09-27-pinned-compilers--c-and-c-were-built-with-compilers-years-behind-the-others)
- [2026-09-27 (cpp inline name compare) — C++'s ndjson called memcmp for every member name](docs/benchmarks/2026-W39.md#2026-09-27-cpp-inline-name-compare--cs-ndjson-called-memcmp-for-every-member-name)
- [2026-09-27 (zig inline name compare) — Zig's ndjson called std.mem.eql for every member name](docs/benchmarks/2026-W39.md#2026-09-27-zig-inline-name-compare--zigs-ndjson-called-stdmemeql-for-every-member-name)
- [2026-09-27 (c inline name compare) — C's ndjson called memcmp for every member name](docs/benchmarks/2026-W39.md#2026-09-27-c-inline-name-compare--cs-ndjson-called-memcmp-for-every-member-name)
- [2026-09-27 (cpp hash tail) — C++ copied each key's last word into a buffer](docs/benchmarks/2026-W39.md#2026-09-27-cpp-hash-tail--c-copied-each-keys-last-word-into-a-buffer)
- [2026-09-27 (c inline json string) — C's ndjson paid the same call Rust did](docs/benchmarks/2026-W39.md#2026-09-27-c-inline-json-string--cs-ndjson-paid-the-same-call-rust-did)
- [2026-09-27 (zig sweep push) — Zig's sweep called for room it had](docs/benchmarks/2026-W39.md#2026-09-27-zig-sweep-push--zigs-sweep-called-for-room-it-had)
- [2026-09-27 (rust inline json string) — Rust's ndjson paid a call per string](docs/benchmarks/2026-W39.md#2026-09-27-rust-inline-json-string--rusts-ndjson-paid-a-call-per-string)
- [2026-09-27 (rust hash tail and cursor inline) — two calls Rust's CSV run made for nothing](docs/benchmarks/2026-W39.md#2026-09-27-rust-hash-tail-and-cursor-inline--two-calls-rusts-csv-run-made-for-nothing)
- [2026-09-27 (cpp wide next_of1) — C++ found ndjson's row ends eight bytes at a time too](docs/benchmarks/2026-W39.md#2026-09-27-cpp-wide-next_of1--c-found-ndjsons-row-ends-eight-bytes-at-a-time-too)
- [2026-09-27 (cpp proof before probe) — the same reordering, in C++](docs/benchmarks/2026-W39.md#2026-09-27-cpp-proof-before-probe--the-same-reordering-in-c)
- [2026-09-27 (c proof before probe) — C parsed the mate's keys before the proof that makes them unnecessary](docs/benchmarks/2026-W39.md#2026-09-27-c-proof-before-probe--c-parsed-the-mates-keys-before-the-proof-that-makes-them-unnecessary)
- [2026-09-27 (c ndjson scans) — C found every ndjson row's end eight bytes at a time](docs/benchmarks/2026-W39.md#2026-09-27-c-ndjson-scans--c-found-every-ndjson-rows-end-eight-bytes-at-a-time)
- [2026-09-27 (zig scan32 default) — Zig scanned eight bytes where every other port scanned thirty-two](docs/benchmarks/2026-W39.md#2026-09-27-zig-scan32-default--zig-scanned-eight-bytes-where-every-other-port-scanned-thirty-two)
- [2026-09-27 (cpp single slot) — C++ walked a slot list for every field](docs/benchmarks/2026-W39.md#2026-09-27-cpp-single-slot--c-walked-a-slot-list-for-every-field)
- [2026-09-27 (c delim cursor) — C scanned every field from scratch](docs/benchmarks/2026-W39.md#2026-09-27-c-delim-cursor--c-scanned-every-field-from-scratch)
- [2026-09-26 (adopt sweep lists) — Zig and Rust copied every row's address and hash twice](docs/benchmarks/2026-W39.md#2026-09-26-adopt-sweep-lists--zig-and-rust-copied-every-rows-address-and-hash-twice)
- [2026-09-26 (cpp parquet join inline) — C++'s cell checks were calls, and its join parts shared lines](docs/benchmarks/2026-W39.md#2026-09-26-cpp-parquet-join-inline--cs-cell-checks-were-calls-and-its-join-parts-shared-lines)
- [2026-09-26 (c word key hash) — C hashed its keys a byte at a time](docs/benchmarks/2026-W39.md#2026-09-26-c-word-key-hash--c-hashed-its-keys-a-byte-at-a-time)
- [2026-09-26 (cpp rle split) — the same per-value bounds test in C++'s bit-packed runs](docs/benchmarks/2026-W39.md#2026-09-26-cpp-rle-split--the-same-per-value-bounds-test-in-cs-bit-packed-runs)
- [2026-09-26 (rust parquet hot paths) — Rust's Parquet path ran 35% more instructions than C's](docs/benchmarks/2026-W39.md#2026-09-26-rust-parquet-hot-paths--rusts-parquet-path-ran-35-more-instructions-than-cs)
- [2026-09-26 (zig hash tail) — the last bytes of every hashed key went through memcpy](docs/benchmarks/2026-W39.md#2026-09-26-zig-hash-tail--the-last-bytes-of-every-hashed-key-went-through-memcpy)
- [2026-09-26 (zig key buffer) — A's and B's sweeps parsed their keys into one cache line](docs/benchmarks/2026-W39.md#2026-09-26-zig-key-buffer--as-and-bs-sweeps-parsed-their-keys-into-one-cache-line)
- [2026-09-26 (zig parquet b pass) — Zig walked B's keys through A's table to count what it already knew](docs/benchmarks/2026-W39.md#2026-09-26-zig-parquet-b-pass--zig-walked-bs-keys-through-as-table-to-count-what-it-already-knew)
- [2026-09-26 (cpp parquet cells) — the per-cell checks were calls](docs/benchmarks/2026-W39.md#2026-09-26-cpp-parquet-cells--the-per-cell-checks-were-calls)
- [2026-09-26 (cpp parquet append) — the same page copy, in C++](docs/benchmarks/2026-W39.md#2026-09-26-cpp-parquet-append--the-same-page-copy-in-c)
- [2026-09-26 (rust parquet append) — Rust's page decoder pushed every value it could have copied](docs/benchmarks/2026-W39.md#2026-09-26-rust-parquet-append--rusts-page-decoder-pushed-every-value-it-could-have-copied)
- [2026-09-26 (zig parquet append) — a third of Zig's Parquet instructions were ArrayList bookkeeping](docs/benchmarks/2026-W39.md#2026-09-26-zig-parquet-append--a-third-of-zigs-parquet-instructions-were-arraylist-bookkeeping)
- [2026-09-26 (zig parquet threads) — Zig's Parquet path ignored `--threads` in two of its four passes](docs/benchmarks/2026-W39.md#2026-09-26-zig-parquet-threads--zigs-parquet-path-ignored---threads-in-two-of-its-four-passes)
- [2026-09-26 (cpp parquet report) — C++ built a Parquet report on every run and printed a line of counts](docs/benchmarks/2026-W39.md#2026-09-26-cpp-parquet-report--c-built-a-parquet-report-on-every-run-and-printed-a-line-of-counts)
- [2026-09-26 (speculative split) — Rust's sweep did not scale because of the step before it](docs/benchmarks/2026-W39.md#2026-09-26-speculative-split--rusts-sweep-did-not-scale-because-of-the-step-before-it)
- [2026-09-26 (cpp join prefetch) — the one join in four that did not prefetch](docs/benchmarks/2026-W39.md#2026-09-26-cpp-join-prefetch--the-one-join-in-four-that-did-not-prefetch)
- [2026-09-26 (cpp slot guess) — the name lookup change, in C++](docs/benchmarks/2026-W39.md#2026-09-26-cpp-slot-guess--the-name-lookup-change-in-c)
- [2026-09-26 (json slot guess) — the C name lookup change, in Rust and Zig](docs/benchmarks/2026-W39.md#2026-09-26-json-slot-guess--the-c-name-lookup-change-in-rust-and-zig)
- [2026-09-26 (json names) — C looked up every member's name as if it had never seen the row before](docs/benchmarks/2026-W39.md#2026-09-26-json-names--c-looked-up-every-members-name-as-if-it-had-never-seen-the-row-before)
- [2026-09-26 (json keys) — Zig and Rust parsed every field of an ndjson row to find two](docs/benchmarks/2026-W39.md#2026-09-26-json-keys--zig-and-rust-parsed-every-field-of-an-ndjson-row-to-find-two)
- [2026-09-26 (json bounds) — C and C++ counted quotes in ndjson, where they mean nothing](docs/benchmarks/2026-W39.md#2026-09-26-json-bounds--c-and-c-counted-quotes-in-ndjson-where-they-mean-nothing)
- [2026-09-24 (zig split) — the sweep got slower with more threads, and it was the allocator](docs/benchmarks/2026-W39.md#2026-09-24-zig-split--the-sweep-got-slower-with-more-threads-and-it-was-the-allocator)
- [2026-09-24 (cpp sweep) — #109 measured the right thing and named the wrong cause](docs/benchmarks/2026-W39.md#2026-09-24-cpp-sweep--109-measured-the-right-thing-and-named-the-wrong-cause)
- [2026-09-24 (false sharing) — the C port's threads were fighting over two cache lines](docs/benchmarks/2026-W39.md#2026-09-24-false-sharing--the-c-ports-threads-were-fighting-over-two-cache-lines)
- [2026-09-24 (zig scan width) — the phase got faster and the run did not](docs/benchmarks/2026-W39.md#2026-09-24-zig-scan-width--the-phase-got-faster-and-the-run-did-not)
- [2026-09-24 (parallel insert) — deterministic, and it does not pay](docs/benchmarks/2026-W39.md#2026-09-24-parallel-insert--deterministic-and-it-does-not-pay)
- [2026-09-24 (parquet join) — the Rust port walked B for a sample nobody asked for](docs/benchmarks/2026-W39.md#2026-09-24-parquet-join--the-rust-port-walked-b-for-a-sample-nobody-asked-for)
- [2026-09-22 (summary only) — the same fault as #106, on the other side](docs/benchmarks/2026-W39.md#2026-09-22-summary-only--the-same-fault-as-106-on-the-other-side)
- [2026-09-21 (duplicate keys, C++) — the same shape, and the helper was already there](docs/benchmarks/2026-W39.md#2026-09-21-duplicate-keys-c--the-same-shape-and-the-helper-was-already-there)
- [2026-09-21 (duplicate keys) — the section decoded every column to keep two](docs/benchmarks/2026-W39.md#2026-09-21-duplicate-keys--the-section-decoded-every-column-to-keep-two)
- [2026-09-21 (insert peak) — the memory gap was a transient, and it was an ordering bug](docs/benchmarks/2026-W39.md#2026-09-21-insert-peak--the-memory-gap-was-a-transient-and-it-was-an-ordering-bug)
- [2026-09-21 (sweep realloc) — a real cost with no room to pay it back](docs/benchmarks/2026-W39.md#2026-09-21-sweep-realloc--a-real-cost-with-no-room-to-pay-it-back)
- [2026-09-21 (slot tag) — C++ probed three cache lines deep to answer what one word can answer](docs/benchmarks/2026-W39.md#2026-09-21-slot-tag--c-probed-three-cache-lines-deep-to-answer-what-one-word-can-answer)
- [2026-09-21 (join_ways) — the join reserved a thread for a pass that was not running](docs/benchmarks/2026-W39.md#2026-09-21-join_ways--the-join-reserved-a-thread-for-a-pass-that-was-not-running)

### [2026-W38](docs/benchmarks/2026-W38.md), 2026-09-14 to 2026-09-20

- [2026-09-20 (per_file) — the sweep was split two ways, which is the worst width available](docs/benchmarks/2026-W38.md#2026-09-20-per_file--the-sweep-was-split-two-ways-which-is-the-worst-width-available)
- [2026-09-20 (phases, corrected path) — the sweep is worse than this file said](docs/benchmarks/2026-W38.md#2026-09-20-phases-corrected-path--the-sweep-is-worse-than-this-file-said)
- [2026-09-20 (10M, before and after) — what the report was costing, on CI](docs/benchmarks/2026-W38.md#2026-09-20-10m-before-and-after--what-the-report-was-costing-on-ci)
- [2026-09-20 (four tasks) — the cross-port tables were never measuring one job](docs/benchmarks/2026-W38.md#2026-09-20-four-tasks--the-cross-port-tables-were-never-measuring-one-job)
- [2026-09-19 (json runs) — a sentry per character, and no wall time to show for removing it](docs/benchmarks/2026-W38.md#2026-09-19-json-runs--a-sentry-per-character-and-no-wall-time-to-show-for-removing-it)
- [2026-09-19 (run 35427804365) — the first ladder CI grouped by itself](docs/benchmarks/2026-W38.md#2026-09-19-run-35427804365--the-first-ladder-ci-grouped-by-itself)
- [2026-09-19 (the ladder CI threw away) — six rungs, three CPUs, and a grouped table that never printed](docs/benchmarks/2026-W38.md#2026-09-19-the-ladder-ci-threw-away--six-rungs-three-cpus-and-a-grouped-table-that-never-printed)
- [2026-09-19 (lazy cells) — the changed-row report materialised nine values for every one it printed](docs/benchmarks/2026-W38.md#2026-09-19-lazy-cells--the-changed-row-report-materialised-nine-values-for-every-one-it-printed)
- [2026-09-18 (report gate) — C++ built the row samples nobody asked for](docs/benchmarks/2026-W38.md#2026-09-18-report-gate--c-built-the-row-samples-nobody-asked-for)
- [2026-09-18 (where it goes) — the sweep is the phase that does not scale](docs/benchmarks/2026-W38.md#2026-09-18-where-it-goes--the-sweep-is-the-phase-that-does-not-scale)
- [2026-09-18 (correction) — the insert is 15% of the run, not half of it](docs/benchmarks/2026-W38.md#2026-09-18-correction--the-insert-is-15-of-the-run-not-half-of-it)
- [2026-09-18 (insert prefetch) — the first bite out of the serial half](docs/benchmarks/2026-W38.md#2026-09-18-insert-prefetch--the-first-bite-out-of-the-serial-half)
- [2026-09-17 (scaling) — every port is half serial, and it is the same half](docs/benchmarks/2026-W38.md#2026-09-17-scaling--every-port-is-half-serial-and-it-is-the-same-half)
- [2026-09-17 (C++ writes) — the write misses are the report path, not the join](docs/benchmarks/2026-W38.md#2026-09-17-c-writes--the-write-misses-are-the-report-path-not-the-join)
- [2026-09-17 (C++ CSV gap) — what it is not, and the one lead that survived](docs/benchmarks/2026-W38.md#2026-09-17-c-csv-gap--what-it-is-not-and-the-one-lead-that-survived)
- [2026-09-17 (memory cap) — the confound that was not one, and nine rounds that lied](docs/benchmarks/2026-W38.md#2026-09-17-memory-cap--the-confound-that-was-not-one-and-nine-rounds-that-lied)
- [2026-09-17 (C++ predicates) — one inline pays, the next one costs, and a fourth CPU](docs/benchmarks/2026-W38.md#2026-09-17-c-predicates--one-inline-pays-the-next-one-costs-and-a-fourth-cpu)
- [2026-09-17 (10M, on CI) — the ndjson ranking is not portable](docs/benchmarks/2026-W38.md#2026-09-17-10m-on-ci--the-ndjson-ranking-is-not-portable)
- [2026-09-17 (json row end) — the fix C wrote down and three ports never took](docs/benchmarks/2026-W38.md#2026-09-17-json-row-end--the-fix-c-wrote-down-and-three-ports-never-took)
- [2026-09-16 (profiles) — where ndjson time goes, and why instruction counts were the wrong thing to count](docs/benchmarks/2026-W38.md#2026-09-16-profiles--where-ndjson-time-goes-and-why-instruction-counts-were-the-wrong-thing-to-count)
- [2026-09-16 (rust scan) — the rung again, and what three ports say about it](docs/benchmarks/2026-W38.md#2026-09-16-rust-scan--the-rung-again-and-what-three-ports-say-about-it)
- [2026-09-16 (zig scan) — the same rung, and it does not transfer](docs/benchmarks/2026-W38.md#2026-09-16-zig-scan--the-same-rung-and-it-does-not-transfer)
- [2026-09-16 (scanners) — the rung C++ was missing, and it pays where C says it should not](docs/benchmarks/2026-W38.md#2026-09-16-scanners--the-rung-c-was-missing-and-it-pays-where-c-says-it-should-not)
- [2026-09-16 (scanners) — a row that measured itself, and a wider step that does not pay](docs/benchmarks/2026-W38.md#2026-09-16-scanners--a-row-that-measured-itself-and-a-wider-step-that-does-not-pay)
- [2026-09-16 (ndjson) — where C++'s time goes, and what it is not](docs/benchmarks/2026-W38.md#2026-09-16-ndjson--where-cs-time-goes-and-what-it-is-not)
- [2026-09-16 (C++ columnar) — what two changes were worth, and a ranking this host cannot give](docs/benchmarks/2026-W38.md#2026-09-16-c-columnar--what-two-changes-were-worth-and-a-ranking-this-host-cannot-give)
- [2026-09-15 (method) — the C and C++ rows are not answering the same question](docs/benchmarks/2026-W38.md#2026-09-15-method--the-c-and-c-rows-are-not-answering-the-same-question)
- [2026-09-15 (measurement) — the C port's time depends on what ran before it](docs/benchmarks/2026-W38.md#2026-09-15-measurement--the-c-ports-time-depends-on-what-ran-before-it)
- [2026-09-15 (week over week) — 20M of Parquet, against the tree from seven days ago](docs/benchmarks/2026-W38.md#2026-09-15-week-over-week--20m-of-parquet-against-the-tree-from-seven-days-ago)
- [2026-09-14 (scale) — the 50M rung, with repeats, and the 1.04x is gone](docs/benchmarks/2026-W38.md#2026-09-14-scale--the-50m-rung-with-repeats-and-the-104x-is-gone)
- [2026-09-14 (zig) — the index reserved four lists it should have let grow](docs/benchmarks/2026-W38.md#2026-09-14-zig--the-index-reserved-four-lists-it-should-have-let-grow)
- [2026-09-14 (memory) — the cap measures a quantity the table did not report](docs/benchmarks/2026-W38.md#2026-09-14-memory--the-cap-measures-a-quantity-the-table-did-not-report)
- [2026-09-14 (parallelism) — the Rust Parquet key read was two jobs whatever the key](docs/benchmarks/2026-W38.md#2026-09-14-parallelism--the-rust-parquet-key-read-was-two-jobs-whatever-the-key)
- [2026-09-14 (scale) — the CSV ceiling is not one number, it is four](docs/benchmarks/2026-W38.md#2026-09-14-scale--the-csv-ceiling-is-not-one-number-it-is-four)
- [2026-09-14 (ndjson) — two explanations for the C++ sweep, both wrong](docs/benchmarks/2026-W38.md#2026-09-14-ndjson--two-explanations-for-the-c-sweep-both-wrong)
- [2026-09-14 (instrumentation) — where C++'s ndjson time actually goes](docs/benchmarks/2026-W38.md#2026-09-14-instrumentation--where-cs-ndjson-time-actually-goes)
- [2026-09-14 (ndjson) — the fourth port, and the placement is a property of the port](docs/benchmarks/2026-W38.md#2026-09-14-ndjson--the-fourth-port-and-the-placement-is-a-property-of-the-port)
- [2026-09-14 (ndjson) — and a third port, where the placement was known in advance](docs/benchmarks/2026-W38.md#2026-09-14-ndjson--and-a-third-port-where-the-placement-was-known-in-advance)
- [2026-09-14 (ndjson) — the byte proof reaches a second port, and where it has to sit](docs/benchmarks/2026-W38.md#2026-09-14-ndjson--the-byte-proof-reaches-a-second-port-and-where-it-has-to-sit)
- [2026-09-14 (contention) — separate hosted runners do not contend, and the first run said otherwise](docs/benchmarks/2026-W38.md#2026-09-14-contention--separate-hosted-runners-do-not-contend-and-the-first-run-said-otherwise)
- [2026-09-14 (parallelism) — the Rust Parquet reader gets *better* with size, so 50M is something else](docs/benchmarks/2026-W38.md#2026-09-14-parallelism--the-rust-parquet-reader-gets-better-with-size-so-50m-is-something-else)

### [2026-W37](docs/benchmarks/2026-W37.md), 2026-09-08 to 2026-09-13

- [2026-09-13 (scale) — Parquet's ceiling is between 50M and 100M, for all four ports](docs/benchmarks/2026-W37.md#2026-09-13-scale--parquets-ceiling-is-between-50m-and-100m-for-all-four-ports)
- [2026-09-13 (memory) — the Rust port stops aborting, and the Parquet column was wrong](docs/benchmarks/2026-W37.md#2026-09-13-memory--the-rust-port-stops-aborting-and-the-parquet-column-was-wrong)
- [2026-09-13 (scale) — 50M rows of Parquet, and the port that cannot do it](docs/benchmarks/2026-W37.md#2026-09-13-scale--50m-rows-of-parquet-and-the-port-that-cannot-do-it)
- [2026-09-10 (build flags) — the Rust column was compiled for a different machine](docs/benchmarks/2026-W37.md#2026-09-10-build-flags--the-rust-column-was-compiled-for-a-different-machine)
- [2026-09-10 (measurement) — what this host cannot tell you about phases](docs/benchmarks/2026-W37.md#2026-09-10-measurement--what-this-host-cannot-tell-you-about-phases)
- [2026-09-10 (scale) — sixty million rows, and nobody's ceiling](docs/benchmarks/2026-W37.md#2026-09-10-scale--sixty-million-rows-and-nobodys-ceiling)
- [2026-09-10 (rle_fill) — 18.5% of the instructions, none of the time](docs/benchmarks/2026-W37.md#2026-09-10-rle_fill--185-of-the-instructions-none-of-the-time)
- [2026-09-10 (C field scan) — a wider step where the target has one](docs/benchmarks/2026-W37.md#2026-09-10-c-field-scan--a-wider-step-where-the-target-has-one)
- [2026-09-10 (memory) — what each port needs, and what each does when it cannot have it](docs/benchmarks/2026-W37.md#2026-09-10-memory--what-each-port-needs-and-what-each-does-when-it-cannot-have-it)
- [2026-09-10 (C codecs) — the columnar path, on compressed files](docs/benchmarks/2026-W37.md#2026-09-10-c-codecs--the-columnar-path-on-compressed-files)
- [2026-09-09 (codecs) — what compression costs, and what the fall-through costs](docs/benchmarks/2026-W37.md#2026-09-09-codecs--what-compression-costs-and-what-the-fall-through-costs)
- [2026-09-09 (parquet readers) — what the columnar path is worth](docs/benchmarks/2026-W37.md#2026-09-09-parquet-readers--what-the-columnar-path-is-worth)
- [2026-09-09 (verification) — the same two changes, through `bench_ab.sh`](docs/benchmarks/2026-W37.md#2026-09-09-verification--the-same-two-changes-through-bench_absh)
- [2026-09-09 (generators) — the two generators a reader can reach](docs/benchmarks/2026-W37.md#2026-09-09-generators--the-two-generators-a-reader-can-reach)
- [2026-09-09 (after the `added` pass) — what a row parse costs, and a change that did not pay](docs/benchmarks/2026-W37.md#2026-09-09-after-the-added-pass--what-a-row-parse-costs-and-a-change-that-did-not-pay)
- [2026-09-09 (profiling) — three questions, and what the answers cost](docs/benchmarks/2026-W37.md#2026-09-09-profiling--three-questions-and-what-the-answers-cost)
- [2026-09-09 (joint run, fourth) — one tree, four ports](docs/benchmarks/2026-W37.md#2026-09-09-joint-run-fourth--one-tree-four-ports)
- [2026-09-09 (joint run, third) — the first one built fairly](docs/benchmarks/2026-W37.md#2026-09-09-joint-run-third--the-first-one-built-fairly)
- [2026-09-09 (joint run, second) — the byte proof at ten million rows](docs/benchmarks/2026-W37.md#2026-09-09-joint-run-second--the-byte-proof-at-ten-million-rows)
- [2026-09-09 (later still, again) — proving a row unchanged from its bytes](docs/benchmarks/2026-W37.md#2026-09-09-later-still-again--proving-a-row-unchanged-from-its-bytes)
- [2026-09-09 (joint run) — ten million rows, seven builds, one runner](docs/benchmarks/2026-W37.md#2026-09-09-joint-run--ten-million-rows-seven-builds-one-runner)
- [2026-09-09 (later still) — the second join pass, removed](docs/benchmarks/2026-W37.md#2026-09-09-later-still--the-second-join-pass-removed)
- [2026-09-09 (later) — a tag in the text index's slot](docs/benchmarks/2026-W37.md#2026-09-09-later--a-tag-in-the-text-indexs-slot)
- [2026-09-09 — the other branch's Parquet, on its own runner](docs/benchmarks/2026-W37.md#2026-09-09--the-other-branchs-parquet-on-its-own-runner)
- [2026-09-09 (later) — the C ndjson path](docs/benchmarks/2026-W37.md#2026-09-09-later--the-c-ndjson-path)
- [2026-09-09 — seven builds against the other branch's rewritten ports](docs/benchmarks/2026-W37.md#2026-09-09--seven-builds-against-the-other-branchs-rewritten-ports)
- [2026-09-08 (later still) — the C generator, on every core](docs/benchmarks/2026-W37.md#2026-09-08-later-still--the-c-generator-on-every-core)
- [2026-09-08 (later) — the same seven builds, after the C parse work](docs/benchmarks/2026-W37.md#2026-09-08-later--the-same-seven-builds-after-the-c-parse-work)
- [2026-09-08 — ten million rows, seven builds, three formats](docs/benchmarks/2026-W37.md#2026-09-08--ten-million-rows-seven-builds-three-formats)
- [2026-09-08 — the C CSV path, before and after](docs/benchmarks/2026-W37.md#2026-09-08--the-c-csv-path-before-and-after)

<!-- archive-index:end -->
