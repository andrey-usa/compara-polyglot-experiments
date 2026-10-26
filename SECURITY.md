# Security policy

compara parses untrusted CSV, JSON and Parquet files, and its Parquet reader
decodes pages itself. Any input that makes it crash, hang, read out of bounds
or report a wrong count is a bug we want to hear about.

## What to expect

- A malformed or hostile file should end the run with exit 2 and a message;
  a panic, an abort or a hang is a bug. `tests/hostile.rs` holds one file per
  way this has failed before (deep or oversized footers, wrapping lengths,
  decompression bombs, truncated rows), and CI runs it on every push.
- No `unsafe` code depends on the file's contents: the `unsafe` blocks are
  SIMD loads behind bounds checks, prefetches, a `set_len` after the writes
  it covers, and the memory map.
- Memory grows with the row counts a file declares. A valid Parquet file can
  describe a billion equal values in a few kilobytes, so a small file is not
  a small comparison; run untrusted inputs under a memory limit (`ulimit -v`,
  a container) if that matters.
- A gzip or zstd input that expands past 1 GB to more than 1000 times its
  size is refused (`COMPARA_MAX_EXPANSION`), and an `http(s)://` input or
  redirect that leads to a link-local address (a cloud metadata service) is
  refused (`COMPARA_ALLOW_LINK_LOCAL=1`). Other private and loopback hosts
  stay readable: they are where many tables live.
- The HTML report shows cell values as text. It loads nothing from the
  network and runs no code from the data.
- Exported CSVs (`--export-dir`, the report's download) keep values exactly
  as they were. A value beginning with `=`, `+`, `-` or `@` is a formula to a
  spreadsheet application: open untrusted exports as text.
- Writing a file you are comparing while compara has it mapped can end the
  run with SIGBUS; compare a copy of a file that is still being written.

## Reporting

Please report vulnerabilities privately through
[GitHub security advisories](https://github.com/andrey-usa/compara/security/advisories/new)
rather than a public issue. Only the latest release receives fixes.
