# Measuring

How to tell whether a change paid. The five rules that matter before anything else are also in
[CONTRIBUTING.md](../CONTRIBUTING.md#measuring), and every rule here has a row in
[FINDINGS.md](../FINDINGS.md) naming the mistake behind it. The full reasoning behind them is in [BENCHMARKS.md](../BENCHMARKS.md#how-to-read-these).

The rules in short:

- **`scripts/bench_ab.sh` for "did that change pay?"** It interleaves the two builds and reports
  the median of the per-round paired ratios with the middle half; where that half straddles 1.00x
  it says *no result*. `--self-test` rechecks the claim on whatever machine you are on.
- **Never compare across tables.** Two numbers from two sittings are two machine states — this
  runner's ndjson figures moved 20% in one morning with no code change.
- **Not `hyperfine`.** Its model (one build's runs, then the other's) reads two copies of the same
  binary as 9-16% apart on a shared runner, and fifteen rounds instead of five makes it worse. The
  machine drifts; pairing cancels drift, averaging does not.
- **Bound the prize before building anything.** A deliberately unsound build that removes the cost
  names the upper bound — ten minutes that has repeatedly replaced a day of implementation.
- **Check a probe against the counts it still produces, not just the clock.** A probe that skipped
  the index insert measured 2.08x and meant nothing: `matched 0` said why.
- **Agreeing on the counts is not doing the same job.** The gate proves the ports found the same
  differences; it says nothing about what else each was asked to produce. Every harness passed
  `--json` to all four ports for most of this project's life, and only C++ emits row samples — 4.9
  MB naming 58,600 rows on a 4M pair, against about a kilobyte from the others, which have no such
  feature. That was a third of the published CSV gap. Timed runs no longer pass `--json`; the gate
  gets its own untimed run. `scripts/report_cost.py` prints what each port's document *contains*
  before what it costs, because the shape is the reason for the cost.
- **Run the invocation in an empty directory and look at what appears.** The same fault was on the
  other side of the table for another week: the Rust port defaults `--out` to
  `<a>__vs__<b>.html`, so the ladder's flagless invocation had it rendering, gzipping and writing a
  report the other three never built. `-o /dev/null` was already there and only moved the write.
  It is 1.31x wall on a 4M pair. Ports are compared with `--summary` now; `gate_flags` hands the
  report back to the untimed counts gate.
- **Never put a shell wrapper between `bench_ab.sh` and the binary.** Two one-line `bash` scripts
  around one binary, differing only in a flag, reported 1.08x where the binary-against-binary
  measurement said 1.31x, with one arm bimodal. Build the second binary.
- **A memory-cap scan is one run per rung, and one run per rung is a coin flip at the boundary.**
  At the cap where the process is a single allocation from its ceiling, which allocation fails
  first decides whether it refuses or aborts — the same binary at the same cap came out exit 2,
  134, 2, 134, 2 on five consecutive runs. Count the rungs that abort in *any* of three passes,
  not the ones that abort in one.
- **A fixed range of caps does not compare two builds that need different amounts.** A change that
  lowers the requirement moves its boundary region down into the middle of the range, so more of
  that region is scanned and the count goes up — which reads as a regression and is partly an
  artifact. The honest reading is what the change converts: dropping the index's peak turned
  graceful index refusals at 12.5-16.5 MB into report-path refusals at the same caps, and the
  report path is the one that cannot always refuse.
- **Rounds scale with how short the run is.** *No result* at nine rounds has become 1.29x at
  twenty-five.
- **Outside 1.00 is not the same as outside the floor.** `--self-test` runs one build against
  itself and names what this machine can resolve today. It measured 0.99x [0.92-1.07] on the
  afternoon a 0.89x [0.84-0.94] "result" was written into BENCHMARKS.md -- outside 1.00, inside
  the floor, and taken back the next day. `bench_ab.sh` now says so itself when the middle half
  clears 1.00 by less than a tenth, but the habit is to run `--self-test` on a machine you have
  not measured on before quoting a number from it.
- **Peak RSS is not the memory answer for anything that maps its input.** `scripts/memory_floor.sh`
  takes memory away until the run dies; that is the honest floor.
- **A benchmark rung dies of memory, not disk — cap it with `RLIMIT_DATA`, never `RLIMIT_AS`.**
  Five dispatched ladder runs failed and every one was `exit 143` with "the runner has received a
  shutdown signal", during the comparison, after the data generated cleanly: 200m csv wrote
  70,176 MB with 107 GB still free, 50m parquet wrote 7,673 MB and died three minutes in. The
  workflow blamed disk for weeks. A reclaimed runner runs **no** further step, `if: always()`
  included, so the rung reported nothing — not even that it had run out of memory.
  `bench_formats_ports.py --memory-cap MB` bounds each port through `RLIMIT_DATA`, which since
  Linux 4.7 covers the heap and private anonymous mappings but **not** file-backed ones, so the
  mapped input is free and only what scales with the row count is capped. Measured: a 702 MB pair
  compares fine under a 256 MB cap and is refused under 96. `RLIMIT_AS` would refuse the mapping
  itself and report a port as failing at a size it handles comfortably. Under the cap all four
  ports now fail legibly instead of taking the host, and all four exit 2 — C and Zig
  "out of memory", C++ `std::bad_alloc`, Rust naming the structure that did not fit
  (`out of memory: the key index needs 32 MB`).
