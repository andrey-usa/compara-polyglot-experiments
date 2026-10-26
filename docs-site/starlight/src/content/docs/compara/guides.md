---
title: compara guides
description: "Any file argument can be a URL, and so can -o, --json and"
---
## Cloud and containers

Any file argument can be a URL, and so can `-o`, `--json` and
`--export-dir`:

```sh
compara compare s3://lake/orders/2026-09.parquet s3://lake/orders/2026-10.parquet -k order_id \
  --json s3://reports/orders/summary.json --export-dir s3://reports/orders/diff/
compara compare gs://bucket/export.csv.gz az://container/warehouse.parquet -k id
compara compare s3://lake/orders/run=41/ 's3://lake/orders/run=42/date=2026-10-*/*.parquet' -k order_id
compara compare "https://bucket.s3.amazonaws.com/a.csv?X-Amz-Signature=..." b.csv -k id
aws s3 cp s3://bucket/a.ndjson - | compara compare - b.ndjson -k id --json -
```

| Scheme | Credentials, as that cloud's own tools find them |
|---|---|
| `s3://`, `s3a://` | `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, a `~/.aws` profile (`AWS_PROFILE`) -- static keys, or SSO, an assumed role or a `credential_process`, resolved the way the AWS CLI resolves them -- web identity (EKS), the ECS task role, instance metadata. `AWS_ENDPOINT_URL` for MinIO, R2, Ceph and the like. The bucket's region is looked up when none is set. |
| `gs://` | `GOOGLE_APPLICATION_CREDENTIALS`, application default credentials (`gcloud auth application-default login`), the metadata server (GKE, Cloud Run). |
| `az://`, `abfs(s)://`, `azure://`, `adl://` | `AZURE_STORAGE_ACCOUNT_NAME` with a key (`AZURE_STORAGE_ACCOUNT_KEY`), a SAS token (`AZURE_STORAGE_SAS_KEY`), a service principal, managed or workload identity, or an `az login` when there is nothing else. |
| `http://`, `https://` | none: the URL is fetched as given, so a presigned S3 URL or an Azure SAS URL works as it is. A refused one that names a bucket or a storage account is retried signed. |

- **No credentials, no setup**: with none to be found, a public bucket
  (open data on S3 or GCS) is read unsigned, and the note on stderr says so;
  a private one fails with what to set.
- **Parquet is read as far as the comparison reads it**: the footer, then
  only the column chunks of the key and compared columns. `--compare` on two
  columns of a 200-column table fetches those two; `compara columns` fetches
  the last 64 KB.
- **Everything else is read in parallel ranges** (8 MB, sixteen at a time,
  `COMPARA_FETCH_CONCURRENCY`) into a temporary file in `TMPDIR`. Gzip and
  zstd files, local or remote, are read as what they hold; one that expands
  past 1 GB to more than 1000 times its size is refused as a decompression
  bomb (`COMPARA_MAX_EXPANSION=<times>`, 0 for no limit).
- **Link-local addresses are not read**: an `http(s)://` table at
  169.254.0.0/16, fe80::/10 or fd00:ec2::254 (where cloud machines get
  their credentials), by address, by name or by a redirect, is refused;
  `COMPARA_ALLOW_LINK_LOCAL=1` allows it. Redirects stop after 10.
- **A table of many files is one side**: a directory, a quoted glob, or an
  object storage prefix is its files read as one table, in path order --
  Spark and Hive output (`date=2026-10-01/part-00000.snappy.parquet`), a
  warehouse unload (`unload/0000_part_00.csv.gz`). CSV parts must share a
  header and Parquet parts a schema, and the error names the part that does
  not. From object storage the parts are fetched together, sixteen ranges in
  flight across all of them, and Parquet parts only as far as the comparison
  reads them. Hive partitions are columns: under `orders/`, the parts of
  `date=2026-10-01/region=eu/` carry `date` and `region` (as text), so
  `-k date,order_id` keys a table whose ids repeat from day to day, and a
  partitioned table compares with a flat export that has those columns.
  Names starting with `.` or `_` (`_SUCCESS`, `.crc`) are passed over.
- **A Delta table is read at a version**: `s3://lake/orders/` is its latest
  version, `s3://lake/orders@v41` the forty-first -- the files its log
  (`_delta_log/`, checkpoint and commits) says make that version, and no
  others -- so two versions compare like two tables:
  `compara compare s3://lake/orders@v41 s3://lake/orders -k order_id`.
  Deletion vectors and column mapping are refused rather than read wrong; an
  Iceberg table is refused.
- **Every range is read from one version of the object** (`If-Match` on its
  ETag): an object overwritten mid-read is an error, not a file stitched
  from two versions. Throttled and dropped ranges are retried.
- **Outputs go up** in one request, or as a multipart upload for a large
  export. `--json -` prints the summary on stdout and the counts on stderr.

Against DuckDB 2.0.0 reading the same objects over S3 (`httpfs`), on one
keyed comparison of 10M rows a side, both tools giving the same four counts
-- an S3 server (SeaweedFS) on the same 4-core runner, best of three (CI run
39793328982):

| input on S3 | compara | DuckDB | peak RSS, compara / DuckDB |
|---|---:|---:|---:|
| Parquet, every column | 9.5 s | 25.3 s | 2.7 / 9.7 GB |
| Parquet, one compared column | 6.7 s | 10.3 s | 2.0 / 3.9 GB |
| Parquet, 9 parts under a prefix | 3.5 s | 20.8 s | 3.8 / 9.1 GB |
| CSV, 1.7 GB a side | 21.5 s | 60.2 s | 4.1 / 13.8 GB |

The single objects come off this server at about 90 MB/s each however many
ranges are in flight, which is most of compara's time on them; the parts,
fetched together, come at 340 MB/s.

In a container:

- `ghcr.io/andrey-usa/compara` is the release binary on
  `gcr.io/distroless/cc` for `linux/amd64` and `linux/arm64`, running as a
  non-root user with `WORKDIR /data`. Nothing else is in the image.
- It reads the container's limits: threads from the CPU quota, memory from
  the cgroup. **Out of memory is a message, not an OOM kill**: the heap is held
  a margin under the limit, so a pair that does not fit is refused with what
  needed how much (exit 2), and under `--engine auto` a CSV pair is compared
  by the engine that spills to disk instead, with the same answer.
- A read-only root filesystem works; standard input, compressed and remote
  inputs and the spilling engine need a writable `TMPDIR` -- in Kubernetes an
  `emptyDir` (disk-backed: a `medium: Memory` one counts against the memory
  limit).
- `compara env` prints what the process sees: CPUs, memory, the container's
  limit, the heap limit it set, and whether the temporary directory is
  writable.

In GitHub Actions, the action installs the release binary, writes the
counts and the most-changed columns to the job summary, and fails the step
when the tables differ (`fail-on-differences: false` to only report):

```yaml
- uses: aws-actions/configure-aws-credentials@v5   # or any other credentials
  with: {role-to-assume: arn:aws:iam::123456789012:role/ci, aws-region: us-east-1}
- uses: andrey-usa/compara@v0
  with:
    a: s3://lake/orders/baseline.parquet
    b: out/orders.parquet
    key: order_id,line_no
    args: -i updated_at --tolerance 0.01
    report: out/orders-diff.html
```

Its outputs are `identical`, `changed`, `added`, `removed` and `summary`
(the JSON summary's path).

A Kubernetes Job comparing two Parquet snapshots with the pod's IAM role
(IRSA), its summary written next to the data; without `--exit-zero` the Job
fails when they differ:

```yaml
apiVersion: batch/v1
kind: Job
metadata: {name: orders-diff}
spec:
  backoffLimit: 0
  template:
    spec:
      serviceAccountName: lake-reader          # annotated with the IAM role
      restartPolicy: Never
      containers:
        - name: compara
          image: ghcr.io/andrey-usa/compara:latest
          args: [compare, s3://lake/orders/v1.parquet, s3://lake/orders/v2.parquet,
                 -k, order_id, --json, s3://reports/orders/summary.json, --exit-zero]
          resources: {limits: {memory: 2Gi, cpu: "4"}}
          securityContext: {readOnlyRootFilesystem: true, runAsNonRoot: true}
          volumeMounts: [{name: tmp, mountPath: /tmp}]
      volumes: [{name: tmp, emptyDir: {}}]
```

## Speed

Ten million rows × 20 columns, keyed on `(account_id, txn_id)`, `--ignore
updated_at`, four threads on a 4 vCPU / 16 GB GitHub Actions runner, best of
five. Each format ran on a runner of its own, named below (CI runs
39728430053 and 39729034092 in
[compara-polyglot-experiments](https://github.com/andrey-usa/compara-polyglot-experiments)).
Each cell is wall time · CPU time · memory above the mapped input.

| Format | Input, both files | Runner CPU | compara |
|---|---:|---|---|
| CSV | 3,509 MB | AMD EPYC 7763 | 1.08 s · 3.5 s · 591 MB |
| ndjson | 8,488 MB | AMD EPYC 9V45 | 1.26 s · 3.9 s · 592 MB |
| Parquet | 1,535 MB | Intel Xeon 6973P-C | 1.17 s · 3.8 s · 1,174 MB |

Two Parquet files against DuckDB and Polars, on the same kind of runner
(CI run 39468615515: a 5M-row pair written by pyarrow, counts only, median
of three): compara 1.26 s at one thread and 0.56 s at four, DuckDB 6.73 s and
3.13 s, Polars 4.98 s and 2.73 s. With the changed, added and removed rows
written out, compara 0.72 s at four threads, DuckDB 4.00 s, Polars 2.74 s.

Contender benchmarks rerun weekly
([`bench-contenders.yml`](https://github.com/andrey-usa/compara/blob/main/.github/workflows/bench-contenders.yml)) against the
latest DuckDB.

## Engines

Three backends, one result contract: every engine returns identical counts and
column statistics for the same input, and the test suite asserts it cell for
cell. `--engine auto` takes the first one that can load the input, in this
order.

| Engine | Implementation | Memory model | Use it for |
|---|---|---|---|
| `turbo` | mapped file, byte-level index; CSV, JSON and Parquet | off-heap bytes, index in memory | the default, and the fastest |
| `sortmerge` | external sort, merge join | bounded memory, spills to disk | files past what memory can index |
| `native` | over the `csv` crate | in-memory, row-oriented | the dependency-light baseline |

All three read values as text, with no type inference, so `1.0` and `1` stay
different unless a tolerance is set, and all three treat an empty field as
absent whether or not it is quoted. A Parquet column is the exception, on
either side: it stores typed values, so a column it holds as numbers
compares by value with the other file's text (`31.40` against the DOUBLE
31.4 is no change), and a date, time or timestamp column by instant (`T` or
a space, a zero fraction or none, `Z` or nothing).
