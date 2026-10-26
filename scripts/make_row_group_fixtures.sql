-- Row-group fixtures for the column reader's run splitting, written by
-- DuckDB (`duckdb :memory: < scripts/make_row_group_fixtures.sql`, run in
-- tests/fixtures/formats). Four row groups of 2048; `label` is dictionary
-- encoded where it repeats (groups 0 and 2) and plain where every value is
-- new (groups 1 and 3), so a column changes form between runs; every column
-- but `id` and `flag` has nulls.
CREATE TABLE t AS
SELECT
  i AS id,
  CASE WHEN i % 13 = 0 THEN NULL
       WHEN (i // 2048) % 2 = 0 THEN 't' || (i % 9)
       ELSE 'u' || i END AS label,
  CASE WHEN i % 11 = 0 THEN NULL ELSE 'n' || (i % 5) END AS note,
  CASE WHEN i % 7 = 0 THEN NULL ELSE (i * 37) % 1000 END::INTEGER AS qty,
  CASE WHEN i % 9 = 0 THEN NULL ELSE i * 1.25 END::DOUBLE AS price,
  (i % 3 = 0) AS flag,
  CASE WHEN i % 17 = 0 THEN NULL ELSE DATE '2020-01-01' + (i % 400)::INTEGER END AS day
FROM range(8000) r(i);
COPY t TO 'rg_snappy.parquet' (FORMAT parquet, ROW_GROUP_SIZE 2048, COMPRESSION snappy);
-- Uncompressed pages are read where they lie in the file, not expanded, so
-- that path gets a file of its own; the narrower columns keep it small.
COPY (SELECT id, label, note, flag FROM t) TO 'rg_none.parquet' (FORMAT parquet, ROW_GROUP_SIZE 2048, COMPRESSION uncompressed);
COPY t TO 'rg_zstd.parquet' (FORMAT parquet, ROW_GROUP_SIZE 2048, COMPRESSION zstd);
