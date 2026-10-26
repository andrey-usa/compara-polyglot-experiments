---
title: CLI reference compara
description: CLI reference compara generated from --help.
---

# CLI reference compara


Generated from `compara main f3274c7`.

Binary reports: `compara 0.6.0`.

## Top level

```text
usage: compara <command> [options]

commands:
  compare A B     Compare two tables on a composite key: print the counts, and with -o
                  write an HTML report
  columns FILE    Print one file's column names, one per line
  head FILE       Print the first rows of one file (default 10)
  version         Print the version (also --version)
  env             Print what this process sees: CPUs, memory and its limits, the
                  temporary directory, and whether URLs can be read

A, B and FILE are paths, `-` for standard input, or with cloud support s3://,
gs://, az:// and https:// URLs. Gzip and zstd files are read as what they hold.
A directory, a quoted glob ('out/date=*/*.parquet', `**` for any depth) or an
object storage prefix (s3://bucket/orders/) is its files read as one table, in
path order; names starting with `.` or `_` in a directory are passed over, and
Hive partition directories (date=2026-10-01/) are columns. A Delta table is read
at its latest version, or at TABLE@vN (s3://lake/orders@v41).

columns and head read CSV, newline-delimited JSON and Parquet, and neither reads
more of the file than it has to: `columns` on Parquet reads the footer, and
`head` stops at the first page. Both take --delimiter and --encoding.

head options:
  -n, --rows N            Rows to show (default 10)
      --csv               Print as CSV instead of an aligned table

compare options:
  -k, --key COLS          Comma-separated key column(s) (or from --profile)
  -c, --compare COLS      Columns to compare (default: all common non-key columns)
  -i, --ignore COLS       Columns to skip
  -p, --profile NAME      Profile name from compara.toml
      --config PATH       Path to compara.toml
      --trim              Strip whitespace before comparing
      --ignore-case
      --empty-is-null     Treat empty string and null as equal
      --tolerance N       Absolute numeric tolerance
      --max-rows N        Rows embedded per section (default 50000)
      --delimiter D       Force delimiter (default: auto)
      --encoding ENC
      --engine E          auto | turbo | sortmerge | native
      --threads N
      --memory SIZE       Memory to plan for, e.g. 8G (default: what the machine or
                          container has free); decides when large text files are
                          joined in one pass instead of two, and how many columns of
                          two Parquet files are read at once; Parquet that needs
                          more than SIZE one column at a time is refused
      --export-dir DIR    Write full changed/added/removed CSVs here (a directory, or a
                          URL prefix such as s3://bucket/run-42/)
  -o, --out PATH          Write the HTML report here, a path or a URL (no report without it)
      --json PATH         Write a JSON summary (counts + column stats) here, a path or a
                          URL; `-` prints it, and the counts line goes to stderr
      --no-compress       Report: embed plain JSON instead of gzip (older browsers)
      --summary           Print the counts and write nothing, and refuse any flag that would
                          (the default without -o, --json or --export-dir)
      --fail-on-dups      Exit 3 when either file has duplicate keys
      --exit-zero         Exit 0 when the tables differ: for a scheduled job whose
                          output is the report, where a difference is not a failure
                          (a Kubernetes Job, an Airflow task)

Exit codes: 0 identical, 1 differences found, 2 error, 3 duplicate keys (with --fail-on-dups).
```


## `version`

```text
compara 0.6.0
```


## `env`

```text
compara 0.6.0
cloud inputs: s3://, gs://, az://, http(s):// (credentials from the environment)
  s3:// credentials: instance metadata, if this is an EC2 instance; else none (public buckets are read unsigned)
  gs:// credentials: the metadata server, if this runs on Google Cloud; else none
  az:// credentials: managed identity, if this runs on Azure; else none
cpus: 2
memory: 7935 MB limit, 1294 MB available
container memory limit: none
heap limit: none set
temp dir: /tmp (writable, 486.3 MB free)
```
