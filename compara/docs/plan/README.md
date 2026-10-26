# Performance plan after E-001 (2026-10-16)

Where compara stands against the C, C++ and Zig ports at 10M rows, what was
measured to get there, and the next changes in the order they are worth doing.
Every number is from a GitHub-hosted 4-vCPU runner unless it says "local";
compare rows within a table only.

## Shipped

- **Run-time AVX2 scan (E-001)** — #8, merged. A baseline x86-64 build asks
  the CPU once and parses each row with a copy compiled for AVX2 or with SWAR.
  2M rows, four threads, local, baseline build to this one: CSV 1.16x, TSV
  1.19x, NDJSON 1.02x; against an x86-64-v3 build not told apart.
- **compara-polyglot-experiments `main` (87c0efb)**: `rust/` synced to #8,
  E-001 recorded as A-135, and `scripts/build_ports.sh` builds the Rust port as
  it is installed (no `-C target-cpu`), with the `+avx2` build beside it in the
  scanner matrix.

## Measured, not shipped

| A/B (`ab.yml`, 10M CSV, 7 rounds, base-again control) | CPU | 1 thr | 4 thr |
|---|---|---|---|
| E-001 code alone, both built x86-64-v3 | EPYC 7763 | 0.998x | 0.986x |
| as installed vs x86-64-v3, same code | EPYC 7763 | 1.021x | 1.043x |
| parse copies widened to avx2,bmi1,bmi2,lzcnt,popcnt | EPYC 9V74 | 1.009x | 1.076x (no help) |
| POPCNT rank (`popcnt-rank.patch`) vs x86-64-v3 | EPYC 7763 | 1.013x | 1.025x |
| serial insert vs parallel, both as installed | EPYC 7763 ×2 | 1.008-1.011x | 0.972-0.979x, CPU −11-12% |

Ten-million-row standing (as installed, four threads):

| Format | CPU | C | C++ | Rust | Zig |
|---|---|---|---|---|---|
| CSV | EPYC 9V45 | 0.71 s | 0.81 s | 0.86 s | 0.86 s |
| CSV (Rust x86-64-v3, `main` before E-001) | Xeon 8573C | 1.16 s | 1.26 s | 1.36 s | 1.31 s |
| NDJSON | EPYC 9V74 | 2.47 s | 2.48 s | **2.32 s** | 2.62 s |
| Parquet | Xeon 8370C | **1.56 s** | 1.97 s | 1.87 s | 1.66 s |
| Parquet (phases run) | EPYC 7763 | **1.18-1.23 s** | 1.30-1.34 s | 1.43-1.44 s | 1.33 s |

## Next, in order

1. **Parquet: a direct path for default options.** callgrind, 1M Parquet, one
   thread, both ports at x86-64-v3: Rust 3.52B instructions against C's 2.77B.
   The difference is the generic per-cell path: `lookup`+`row_eq` 485M against
   C's 188M, `row_hash` 472M, `cell_differs` 293M for two plain columns. Each
   key or plain cell builds a `Look` in `Col::at` and re-checks
   `needs_normalising(opt)` about three times. Decide "no normalising options"
   once per run (a const generic or a per-column accessor resolved up front)
   and give hashing, key equality and plain compared columns a byte path.
   Compared columns are 0.75 s against C's 0.55 s at four threads on the EPYC
   7763, and 33% slower single-threaded, so this is per-cell work. The
   `formats.rs` tests already hold Parquet to turbo's answer under every
   option.
2. **POPCNT rank** — `popcnt-rank.patch` (applies to `main`). Every occupied
   slot's row becomes its key's position with one `count_ones` per key; a
   baseline build counts in a dozen instructions. The loop is compiled a
   second time for POPCNT and chosen at run time. Recovers about 40% of the
   as-installed cost above (1.043x → 1.025x). 199 tests pass; needs a PR.
3. **Serial index insert on the ordinary path.** At 10M the parallel insert
   is 0.32 s a side at four threads against 0.24 s for the serial one at one
   and C++'s 0.165 s; switching it off saved 11-12% CPU and 2-3% wall
   (`serial-insert-measurement.patch` is the measurement switch, not the
   change). A-118 measured the parallel insert winning on the fused path at
   8M and at 150M, so the threshold should key on the path, not the row
   count: measure 50M and the fused path before changing it (T-044 asks for
   exactly this).
4. **The serial insert loop itself** — 0.24 s a side against C++'s 0.165 s
   and Zig's 0.198 s, one thread. Not yet profiled.
5. **The rest of the as-installed cost** (~2.5% at four threads after item 2)
   lies outside the scan and the rank; find it with a phase A/B of the two
   builds on CI.

## Notes for the next session

- GitHub Actions starts no jobs on compara-private (every job ends in seconds
  with no runner since 2026-10-15 15:16 UTC — account billing or spending
  limit). The same code runs in the public experiments repo, where CI works:
  `ci-rust.yml` on four OSes, `ab.yml`, `phases.yml`, `bench-ladder.yml`.
- This container is not a measuring host for C: the C port runs about twice
  as slow here as on CI (gcc 13 against CI's gcc 16, and the VM's page
  faults), and timings vary several percent. Instruction counts (callgrind,
  x86-64-v3 builds — valgrind 3.22 cannot run AVX-512) are reliable here.
- `phases.yml` timings for C on CSV are not representative (1.93 s against
  1.1 s in the ladder on the same CPU); compare Rust against C++ there.
