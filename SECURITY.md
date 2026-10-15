# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| main    | :white_check_mark: |

This project tracks `main`; there are no release branches. Security fixes
land on `main` like everything else.

## Reporting a Vulnerability

**Do not open a public issue for a security vulnerability.**

Instead, use GitHub's private vulnerability reporting for this repository
(Security tab → "Report a vulnerability"). That opens a private channel with
the maintainers, which is where the fix gets coordinated before anything is
public.

If private vulnerability reporting is not enabled on the repository, open a
draft pull request with the fix and no description beyond "security fix —
details to follow", or contact the maintainer through the email on their
GitHub profile.

### What to include

- The port(s) affected (C, C++, Rust, Zig) and how to build them
- The smallest input that triggers the issue, or a precise description of it
- What you expected versus what happened — crash, hang, wrong output,
  out-of-bounds read/write
- Whether you believe it is exploitable beyond a local denial of service
  (these tools process local files, so most issues are availability, not
  remote code execution — say which you think it is)

### What happens next

- You will get an acknowledgment within a few days.
- The fix is developed privately, then merged to `main` with a commit
  message describing what was wrong and what changed.
- You will be credited in the commit message unless you ask not to be.

## Scope notes

csvdiff compares local files. Its attack surface is malformed input:
a hostile CSV, ndjson or Parquet file should produce an error or a wrong
answer, never an out-of-bounds access. The fuzz-relevant surfaces are the
field scanners, the Parquet page decoders, and the key-index probes in each
port. The HTML report writer (Rust) is also in scope — it must not emit
unescaped input into the page.
