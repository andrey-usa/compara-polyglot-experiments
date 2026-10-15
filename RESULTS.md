# Where the four ports stand

**Ten, fifty and one hundred fifty million rows, CSV and Parquet, all four
ports, measured on CI.**
Run 2026-09-29 (fourth edition), after the Zig guard_span (`f39da04`) and the
Rust/Zig u32 hash slots (`9d89c95`, `4f80fa2`). The 09-28 edition's numbers are
kept in the section below.

Measured with **no `--json`**, and with `--summary` for Rust, so every port is
doing the job all four perform: count and compare, and write nothing.

> **A note on CPUs.** This ladder's rungs landed on three different processors:
> 10M CSV on an Intel Xeon 6973P-C, 10M/50M Parquet on an AMD EPYC 7763, 50M
> CSV on an AMD EPYC 9V74. Per the project's own rule — compare rows within a
> table, never across tables — the tables below stand alone. The 09-28 column
> is omitted where the CPU differs.

---

## Results — 10M rows, CSV and Parquet

Run 36505346798,
`bench-ladder.yml` on `244be60` (main: Zig guard_span, Rust/Zig u32 slots),
two runs each, best kept, ports rotated.

### CSV — 3,509 MB a side, Xeon 6973P-C

| Port | Compare | CPU | Cores | Above the input | vs best |
|---|---:|---:|---:|---:|---:|
| **C** | **0.96s** | **3.0s** | 3.18x | 716 MB | — |
| **Rust** | **0.96s** | 3.2s | 3.34x | **591 MB** | — |
| C++ | 1.11s | 3.5s | 3.15x | 723 MB | 1.16x |
| Zig | 1.21s | 3.1s | 2.57x | 599 MB | 1.26x |

Rust ties C on wall time. Both u32 ports dropped ~130 MB above the input
(Rust 725 → 591 MB, Zig 725 → 599 MB) — the slot conversion paying in memory
as designed.

### Parquet — 1,535 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best |
|---|---:|---:|---:|---:|---:|
| **C** | **1.27s** | **3.8s** | 3.00x | 1,413 MB | — |
| C++ | 1.42s | 4.3s | 3.05x | 1,362 MB | 1.12x |
| Rust | 1.42s | 4.7s | 3.29x | 1,283 MB | 1.12x |
| Zig | 1.42s | 4.8s | 3.37x | **1,129 MB** | 1.12x |

Three ports tie at 1.42s behind C. Zig uses the least memory by a clear margin
(1,129 MB vs 1,283–1,413 MB).

---

## Results — 50M rows, CSV and Parquet

### CSV — 17,544 MB a side, EPYC 9V74

| Port | Compare | CPU | Cores | Peak RSS | Above the input |
|---|---:|---:|---:|---:|---:|
| C | 47.14s | 28.8s | 0.61x | 15,187 MB | **-2,357 MB** |
| C++ | 44.28s | 30.8s | 0.69x | 15,069 MB | **-2,475 MB** |
| Rust | 35.13s | 27.6s | 0.78x | 15,107 MB | **-2,438 MB** |
| Zig | 76.87s | 33.8s | 0.44x | 15,186 MB | **-2,358 MB** |

**Not a benchmark.** Every port peaked *below* its mapped input — the kernel
was taking pages back while they ran. Per [BENCHMARKS.md](BENCHMARKS.md), a
negative *above the input* ranks page faults, not parsing. The 17.5 GB input
does not fit the 16 GB runner with the indexes on top. These numbers are kept
to show the wall, not the ranking.

### Parquet — 7,673 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best |
|---|---:|---:|---:|---:|---:|
| **Rust** | **6.06s** | 19.7s | 3.26x | **5,966 MB** | — |
| Zig | 6.25s | 20.6s | 3.29x | 6,202 MB | 1.03x |
| C++ | 7.81s | 21.9s | 2.81x | 6,237 MB | 1.29x |
| C | 12.13s | 30.0s | 2.47x | 6,191 MB | 2.00x |

Rust leads on wall time *and* memory in this run after the u32 slot conversion
(`174c28f`) — 6.06s wall and 5,966 MB above the input, the
leanest of the four. C, C++ and Rust all closed Zig's former memory lead
(6,201 MB vs 6,953–7,178 MB before); C's wall time here is runner noise —
local A/B on 2M and 10M Parquet shows the u32 change is performance-neutral
for C.

---

## Results — 150M rows

### Parquet — 23,020 MB a side

All four ports failed with out-of-memory: the input alone exceeds the 16 GB
runner. Rust reported `one count per distinct key needs 572 MB`; C and Zig
said `out of memory`; C++ threw `std::bad_alloc`. This size needs the
`scale-ceiling` workflow's memory cap, not the ladder.

### CSV — 52,634 MB a side

The rung completed its benchmark but the job hung in the `Swatinem/rust-cache`
post-step — the 52 GB of generated data left no disk for the cache save — and
was cancelled before the artifact uploaded. The 50M CSV rung above already
showed this size does not fit a 16 GB runner (all ports paged); 150M CSV on
the ladder is a larger version of the same wall. The meaningful 150M CSV
comparison remains the capped run from 09-28 (all four ports, identical
counts, `RLIMIT_DATA` methodology).

---

## What the fourth edition says

**The u32 slots paid in memory, as designed.** Rust and Zig dropped 120–130 MB
above the input at 10M CSV; at 50M Parquet the gap widened to 750 MB between
Zig and the next port. The slots were kept for this, not for speed — but both
ports also got faster (Rust 1.04x, Zig 1.07x on the 2M A/B).

**Zig's guard_span shows at 50M Parquet.** Zig leads the 50M Parquet table on
both wall time and memory — the first time any port but C has led a table in
this file. The 10% join win measured at 2M CSV compounds with the smaller
index at larger sizes.

**50M CSV does not fit a 16 GB runner.** All four ports paged. The meaningful
large-row CSV comparison remains the capped 150M run from 09-28 (all four
ports, identical counts, RLIMIT_DATA methodology).

---

## Previous editions

Run 2026-09-28 (third edition), after #178, #179 and the Rust guard_span +
`needs_normalising` hoisting. The 09-26 edition's numbers are kept beside them
where the same CPU measured both.

Measured with **no `--json`**, and with `--summary` for Rust, so every port is
doing the job all four perform: count and compare, and write nothing. Both
flags were once in the measurement for one port only -- `--json` made C++ emit
row samples, and Rust rendered a report whether asked or not -- and each was
worth more than a tenth of that port's time. [BENCHMARKS.md](BENCHMARKS.md) has
the before and after of both.

---

## The one rule, and what CI does to it

**Compare rows within a table. Never across tables.** This project has measured
the same code 20% apart on one machine in one morning, and a port's time swing
1.57x by nothing but where it sat in the running order.

**On hosted CI a "table" is smaller than it looks.** GitHub puts several
processor generations behind one `ubuntu-latest` label. A workflow that fans
sizes or formats out across jobs — which is how the ladder gets parallelism —
collects its pieces from *different machines*. Read as one curve they measure
the fleet as much as the code.

So the harness records the CPU in its JSON beside the numbers, and
`scripts/bench_group.py` groups on it and prints one table per processor. A CI
summary showing two CPUs is showing two tables. The section below on
[the ndjson ranking](#the-ndjson-ranking-is-not-a-property-of-the-code) is what
happens when you ignore this.

**And a ratio of bests hides about a tenth.** Each cell is the best of three
runs. Differences under about 10% between two rows are not a ranking; this file
calls them a tie.

---

## The workload

| | |
|---|---|
| Rows | 10,001,000 in A, 10,000,500 in B, 20 columns |
| Key | `(account_id, txn_id)` |
| Ignored | `updated_at` |
| Compared | a per-cell diff over the remaining 17 columns |

Every port returned identical counts on every format. That is the run's gate,
not a footnote — a port that disagrees fails the run and is named.

```
matched 9,990,000 · changed 599,320 · unchanged 9,390,680
added 10,000 · removed 10,000
duplicate keys 1,000 in A / 500 in B
```

---

## Results — 10m rows, three formats

Run 36388954996,
`bench-ladder.yml` on `e784e23` (main: C guard_span #178, C++ guard_span #179,
Rust guard_span + `needs_normalising` hoisting), three runs each, best kept,
ports rotated.

**Two processors again.** CSV and Parquet ran on an **AMD EPYC 7763** — the CPU
the 09-26 evening edition used — so those two tables carry an `09-26 pm` column
and read as before and after. ndjson landed on an **AMD EPYC 9V74** and is a
table of its own.

Every port compiled for the machine it ran on: `-march=native` for C and C++
(clang), `-C target-cpu=x86-64-v3` for Rust, `-Dcpu=native` for Zig.

### CSV — 3,509 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best | 09-26 pm |
|---|---:|---:|---:|---:|---:|---:|
| **C** | **1.06s** | **3.4s** | 3.26x | **716 MB** | — | 1.71s |
| C++ | 1.21s | 4.0s | 3.32x | 723 MB | 1.14x | 1.82s |
| Rust | 1.21s | 4.0s | 3.28x | 725 MB | 1.14x | 1.62s |
| Zig | 1.36s | 3.8s | 2.79x | 725 MB | 1.28x | 1.81s |

### Parquet — 1,535 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best | 09-26 pm |
|---|---:|---:|---:|---:|---:|---:|
| **C** | **1.21s** | **3.7s** | 3.03x | 1,424 MB | — | 1.32s |
| C++ | 1.36s | 4.3s | 3.13x | 1,386 MB | 1.12x | 1.61s |
| Zig | 1.36s | 4.6s | **3.38x** | 1,312 MB | 1.12x | 1.46s |
| Rust | 1.41s | 4.5s | 3.22x | **1,293 MB** | 1.17x | 1.77s |

### ndjson — 8,487 MB a side, EPYC 9V74

| Port | Compare | CPU | Cores | Above the input | vs best |
|---|---:|---:|---:|---:|---:|
| **C** | **2.42s** | 8.6s | 3.58x | **716 MB** | — |
| C++ | 2.43s | 8.7s | 3.59x | 723 MB | 1.00x |
| Rust | 2.67s | 9.6s | 3.60x | 725 MB | 1.10x |
| Zig | 2.77s | **8.4s** | 3.03x | 719 MB | 1.14x |

**A caution on the CSV column.** Every port is 25–38% faster than the 09-26
evening edition on the same CPU — well beyond the isolated guard_span gains
(10–13% C, 7–12% C++, ~neutral Rust). Both editions are best-of-three on an
EPYC 7763, so the direction is real, but the size of the move likely mixes code
gains with runner variance. Read the column as the new standing, not as the
size of any one PR.

## What the new tables say

**CSV has a leader again.** C at 1.06s, then C++ and Rust tied at 1.21s
(1.14x), then Zig at 1.36s (1.28x). The 09-26 evening edition was a four-way
tie at 1.62–1.82s; guard_span broke it, and C took the most from it.

**Parquet's spread stayed closed.** 1.21–1.41s, 1.17x first to last, C still
first. Every port moved 8–20% on the same CPU.

**ndjson is C and C++ tied**, 2.42s and 2.43s on the 9V74, Rust 1.10x behind,
Zig 1.14x. By this file's rule the whole table is nearly a tie.

**Memory above the input has converged on text.** All four ports now sit at
716–725 MB on CSV and ndjson — Rust's old ~150 MB gap and Zig's ~300 MB gap
are gone. On Parquet every port still pays over a gigabyte, 1,293–1,424 MB,
with Rust now the lightest.

---

## The 09-26 edition

Run 36264523793,
`bench-ladder.yml` on `015ecad` (#143 merged), three runs each, best kept,
ports rotated.

**Two processors this time.** CSV and Parquet ran on an **AMD EPYC 7763** — the
CPU the morning edition got for all three — so those two tables carry an
`09-26 am` column and can be read as before and after. ndjson landed on an **AMD
EPYC 9V45** and is a table of its own; a single-job run on an EPYC 7763 below it
gives the before-and-after for ndjson.

Every port compiled for the machine it ran on: `-march=native` for C and C++
(clang), `-C target-cpu=x86-64-v3` for Rust (capped at the fleet's executable
floor), `-Dcpu=native` for Zig.

### CSV — 3,509 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best | 09-26 am |
|---|---:|---:|---:|---:|---:|---:|
| **Rust** | **1.62s** | 5.5s | 3.40x | 871 MB | — | 1.66s |
| C | 1.71s | 6.0s | **3.51x** | **716 MB** | 1.06x | 1.71s |
| Zig | 1.81s | **4.8s** | 2.63x | 1,023 MB | 1.12x | 1.76s |
| C++ | 1.82s | 6.3s | 3.45x | 724 MB | 1.12x | 1.76s |

### Parquet — 1,535 MB a side, EPYC 7763

| Port | Compare | CPU | Cores | Above the input | vs best | 09-26 am |
|---|---:|---:|---:|---:|---:|---:|
| **C** | **1.32s** | **4.1s** | 3.14x | 1,416 MB | — | 1.26s |
| Zig | 1.46s | 5.0s | **3.40x** | 1,307 MB | 1.11x | **2.42s** |
| C++ | 1.61s | 4.9s | 3.04x | **1,270 MB** | 1.22x | 1.71s |
| Rust | 1.77s | 5.8s | 3.28x | 1,276 MB | 1.34x | 1.96s |

### ndjson — 8,487 MB a side, EPYC 9V45

| Port | Compare | CPU | Cores | Above the input | vs best |
|---|---:|---:|---:|---:|---:|
| **Rust** | **2.02s** | 7.4s | 3.65x | 875 MB | — |
| C++ | 2.07s | 7.6s | 3.67x | 723 MB | 1.02x |
| C | 2.12s | 7.8s | **3.69x** | **716 MB** | 1.05x |
| Zig | 2.17s | **6.5s** | 3.00x | 1,029 MB | 1.07x |

And the same inputs on an EPYC 7763, from one `benchmark-native.yml` job
(run 36264522081,
same commit), best of three. That workflow drives `bench_ports.py` rather than
the ladder's harness; the ndjson inputs the two generate are byte-identical, and
the morning column is the ladder's:

| Port | Compare | CPU | 09-26 am, EPYC 7763 |
|---|---:|---:|---:|
| Zig | **3.61s** | **11.3s** | 4.03s |
| Rust | 3.63s | 13.6s | 3.58s |
| C | 3.91s | 15.0s | 3.93s |

C++ is left out of that one on purpose: that workflow builds C++ with the
default `c++`, which is g++, and every other table here is clang. It measured
4.47s, which is a compiler comparison and not a port comparison — see
[cpp/README.md](cpp/README.md#the-compiler-is-worth-more-than-the-language).

---

## What this table says

*The 09-26 edition's tables, analyzed when they were new.*

**Parquet closed most of its spread.** On the same EPYC 7763 the four ports went
from 1.26–2.42s (1.92x first to last) to 1.32–1.77s (1.34x). Zig moved furthest,
2.42s → 1.46s and from last to second: its key reads and column pass ignored
`--threads` (#137), its page decoder spent a third of its instructions on
`ArrayList` bookkeeping (#138), and it walked B's keys through A's table to count
what the A pass already knew (#142). Rust and C++ took the same page copy (#139,
#140), and C++ stopped calling out for every cell check (#141). C still leads,
by 1.11x, which is at the edge of what this file calls a tie. C's own 1.26s →
1.32s is inside the noise; no C Parquet code changed.

**CSV is still a four-way tie.** 1.62s to 1.82s, 1.12x first to last, on the same
CPU as the morning's 1.66–1.76s. Nothing in this batch was aimed at CSV except
#143, and that did not move Zig's best-of-three — see the next paragraph.

**Zig's text sweep was bimodal, and #143 took away the bad mode, not the good
one.** `smp_allocator` starts every thread on the same slot and aligns small
allocations only to their size, so the sixteen-byte buffer A's sweep parses its
key into and B's usually landed on one cache line. How often depended on the
run: on CI the same unsplit sweep measured 0.50s one time and 0.78s the next. A
best-of-three can land on a good run, which is what the morning's 1.76s did. The
`phases.yml` pairs, which take medians, show the change: at 10M on an EPYC 7763,
Zig's CSV went 2.12s → 1.76s and its ndjson 4.05s → 3.63s, with C and Rust moving
0–5% in the same pairs. On the single-job ndjson table above, Zig is first on
that CPU for the first time, at 3.61s against the morning's 4.03s.

This is also the correction to #126, which kept Zig's sweep at one thread per
file and blamed the allocator for splitting measuring slower. The allocator was
where the cause was hiding. Splitting was re-measured on CI with the key buffer
fixed, and it still does not pay on these runners: it cost CSV 0.62s → 0.80s of
sweep on a Xeon 6973P-C and bought ndjson 3.6% of wall for 14% more CPU. So Zig
still runs narrowest — 2.63x and 3.00x cores against 3.4–3.7x — and still spends
the least CPU on text: 4.8s on CSV against 5.5–6.3s, and 6.5s on ndjson against
7.4–7.8s.

**ndjson on the 9V45 is 1.07x first to last**, Rust, C++, C, Zig — a tie by this
file's own rule, and not the order of any earlier column below.

**Memory above the input is unchanged in shape.** C and C++ 716–724 MB on text,
Rust about 150 MB above them, Zig about 300 MB. On Parquet every port pays over a
gigabyte. C++ and Rust now pay least (1,270 and 1,276 MB), and Zig's rose from
1,226 to 1,307 MB, most likely the price of reading its key columns in parallel
(#137).

---

## The biggest number here is one no port owns

> **Measured before #124–#133.** The tables in this section were taken on a
> local 4M CSV pair with the builds of 2026-09-22. The C and C++ sweeps have
> since been unshared and re-split (#124, #125), Rust's no longer waits on a
> serial pre-pass (#133), and Zig's no longer splits (#126), so the sweep row
> below does not describe any port as it now stands. The method is still the
> one to use; the numbers need re-taking.

Every port scales badly from one core to four, and by nearly the same amount:

| port | 1 thread | 4 threads | 1→4 | implied serial | insert, critical path |
|---|---:|---:|---:|---:|---:|
| C | 1.52s | 0.82s | 1.86x | 38% | 14% |
| C++ | 2.74s | 1.61s | 1.70x | 45% | 15% |
| Rust | 2.00s | 1.10s | 1.81x | 40% | 14% |
| Zig | 1.48s | 0.97s | 1.53x | 54% | 16% |

(4M CSV pair, this host. `scripts/scaling_curve.py` and `scripts/serial_share.py`.)

Every port is between a third and a half serial, measured from its own 1-to-4
speedup. For C++ that time is accounted for. Re-measured on the path the
benchmark now times — **no `--json`**, 4M CSV pair, five rounds, medians, the two
index sides taken as a critical path because they run concurrently:

| phase | share of the 4-thread wall | scales 1→4 |
|---|---:|---:|
| sweep | **24.6%** | **0.73x** |
| index insert | 19.1% | 1.14x |
| join and compare | 56.1% | 2.35x |
| assemble | 0.1% | — |

Only the join scales. The **sweep is the largest non-scaling phase and gets
slower with more threads** — the signature of a phase limited by memory bandwidth
rather than by cores, on a scan of 1.5 GB. It is worse than this page used to
say: 0.73x, not 0.79x.

`assemble` is 0.1% and no longer worth a row. It was 11% when this table was
first taken and 7.8% on the same pair with `--json` still passed; gating it
behind the flag and then building only the cells the report prints took it out
of the measured path entirely.

Two checks that this decomposition is real, not fitted. The critical path sums
to **1.699s against a 1.70s wall** — 100%. And the non-scaling phases are
24.6% + 19.1% = **43.7%**, against the **43.3%** Amdahl implies from the
measured 1.74x speedup: two routes to one number, agreeing to four-tenths of a
point.

See [BENCHMARKS.md](BENCHMARKS.md) for the correction and how the error was
made.

---

## The ndjson ranking is not a property of the code

Four machines have measured this same tree at 10M. The ndjson ordering is not
the same on all of them — but it is not arbitrary either:

| ndjson, 10M | container<br>Xeon @2.10GHz, avx512 | CI ladder<br>Xeon Plat. 8370C, avx512 | CI<br>EPYC 9V74, avx2 | CI<br>Xeon Plat. 8573C, avx512 | CI 09-20<br>EPYC 9V74, avx512<br>*no `--json`* | CI 09-26 am<br>EPYC 7763, avx2<br>*after #117–#133* | CI 09-26 pm<br>EPYC 9V45, avx512<br>*after #117–#143* |
|---|---|---|---|---|---|---|---|
| 1st | **Rust** 5.08s | **C** 7.95s | **Zig** 7.20s | **Zig** 5.47s | **C** 5.24s | **C++** 3.57s | **Rust** 2.02s |
| 2nd | Zig 5.48s | Zig 8.26s | C 7.41s | C 5.68s | Zig 5.79s | Rust 3.58s | C++ 2.07s |
| 3rd | C 6.13s | C++ 8.96s | Rust 8.04s | Rust 6.34s | C++ 5.88s | C 3.93s | C 2.12s |
| 4th | C++ 8.76s | **Rust** 9.96s | C++ 10.39s | C++ 7.87s | **Rust** 6.24s | Zig 4.03s | Zig 2.17s |

The last two columns are different code, not only a different CPU: #127, #128,
#129, #130 and #131 all change the ndjson path, in every port, and #143 changes
Zig's. They are here for the record and not as a reproduction of anything to
their left. The seventh is 1.07x first to last, a tie by this file's own rule.

**Columns three and four agree exactly** — different vendor, different vector
width, same order. That was the first ndjson ordering this project reproduced on
independent hardware.

**Column five does not agree with either, on the same CPU model as column
three.** C and Zig swap, and Rust falls from third to last. The two are 1.03x
apart in column three, which this file already called a tie, so the swap at the
top is the noise floor behaving as documented — but Rust moving 3rd → 4th is
not, and no code change between the runs touched Rust's ndjson path. Read it as
one more column, not a correction of the others.

Rust is first on one machine and last on two, so no ordering here is universal,
and any published one needs its CPU printed beside it. "Three machines, three
winners" was too strong a reading of the first three columns — the orderings are
not arbitrary, they are just not portable — and five columns have not made them
more portable.

**The 8370C column was suspected of a harness confound. It is not.** It is the
only one measured by the ladder harness, which additionally passes
`--memory-cap`, and it is the column where Rust looks worst — so the cap was
tested directly: one machine, one binary, one input, the cap set to exactly what
the ladder computes, twenty-one paired rounds with the arms rotated.

| ndjson 4M, capped / free | median | middle half | slower in |
|---|---:|---|---|
| Rust | 1.002 | 0.980–1.017 | 11/21 |
| C | 1.002 | 0.971–1.018 | 11/21 |

A coin flip for both. The cap costs nothing, so it does not explain that column,
which therefore stands as an ordinary result from an ordinary CPU.

(Nine rounds of the same experiment had given Rust 1.034, slower in 6 of 9, which
looked like a finding. It was not. That is the noise floor at the top of this
file doing exactly what it says it does.)

What survived the first five columns — **C++ is last or second-to-last on
ndjson everywhere** — does not survive the sixth, where C++ is first. That is
the code changing under the claim, which is what the claim was for. C is still
never worse than third.

Column five moves C++ from last to third, and that is the report coming out of
the measurement rather than the engine getting faster — the same 30% of CPU it
stopped spending on CSV.

---

## What is established, and what is not

| Claim | Evidence |
|---|---|
| All four ports agree on every count, every format | the gate on every rung of the 09-26 run |
| CSV is a tie among all four at 10M on the EPYC 7763 | 1.66–1.76s in the morning, 1.62–1.82s in the evening, same CPU |
| C leads Parquet | first in both 09-26 runs and in 09-20, same CPU — but by 1.11x now, not 1.36x |
| The Parquet spread is mostly gone | 1.92x → 1.34x first to last on the EPYC 7763, after #137–#142 |
| C++'s CSV gap was threading, not work | 2.61x → 3.54x cores and 2.82s → 1.76s on the same CPU after #125 and #132 |
| C and C++ use the least memory above their input on text | 716–726 MB on both text formats, 09-26 |
| Zig does the least work on text and runs narrowest | least CPU and fewest cores on CSV and ndjson in both 09-26 runs |
| Zig's text sweep was slowed by the allocator | **corrected** — by the allocator's layout: two threads' key buffers on one cache line (#143) |
| C++ is last on ndjson | **no longer true** — first in the 09-26 run, after #127 and #131 |
| Any port "leads ndjson" in general | **refuted** — six columns, four different winners |
| The ndjson row-end fix is worth 1.26x on C++, 1.04x on Rust | [paired A/B, 11 rounds, CSV as control](BENCHMARKS.md) |
| The same fix is worth anything on Zig | **not established** — the band crosses 1.00x |
| Instruction count predicts wall time across ports | **refuted** |
| Instruction count tracks wall time within one port | [0.790 against 0.792 measured](BENCHMARKS.md) |

---

## Two things that are not comparable, and look like they should be

**The two harnesses generate different Parquet.** `bench_formats_ports.py` uses
the Rust generator and `bench_ports.py` uses the C one, and for the same ten
million rows they emit 1,535 MB and 2,074 MB respectively. The Parquet rows from
the two harnesses are therefore not comparable even on one CPU. The CSV and
ndjson inputs are byte-identical between them (3,509 MB and 8,487 MB either way).

**The README's headline table is a different host.** It is kept as the
GitHub-runner reference point from 2026-09-09 and is not the latest measurement.

---

## Reproducing this

```sh
# The ladder: one job per size and format, so FASTER but several CPUs.
# Its collect job groups by processor and says so. This is what the tables
# above came from; on 09-26 am it happened to land on one CPU, on 09-26 pm,
# 09-28 and 09-20 on two.
gh workflow run bench-ladder.yml -f sizes=10m -f formats=csv,ndjson,parquet -f repeats=3

# All three formats in ONE job, which is the only way to get one CPU
# across formats -- at the cost of running them in series.
gh workflow run benchmark-native.yml -f rows=10m -f formats=csv,json,parquet -f repeats=3

# Locally, one host by construction
python3 scripts/bench_formats_ports.py --rows 10m --repeats 3

# Merge any collection of result JSON, grouped by the CPU that produced it
python3 scripts/bench_group.py rungs/

# Two builds of one port -- the only way to price a single change
bash scripts/bench_ab.sh
```

`bench_ab.sh` is the one to reach for when asking *did my change help*. It prints
the middle half of the per-round paired ratios beside the median, and where that
half straddles 1.00x there is no result to report. On this class of machine about
10% is invisible to a ratio of bests and about 3% is the floor for the paired
one — which is why the 1.03x above is called a tie.
