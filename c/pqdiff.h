/*
 * Comparing two Parquet files without turning them back into rows.
 *
 * The CSV path in csvdiff.c has one shape: map the file, find every row, and
 * reduce each row to a handful of (offset, length) fields. That shape is right
 * for a text format, where a value's boundaries are only known by scanning for
 * them.
 *
 * Parquet is not that. A value's boundaries are written down, values of one
 * column are contiguous, and -- this is the part worth exploiting -- a
 * low-cardinality column is stored as small integers indexing a dictionary of
 * its distinct values. Reading such a file back into rows in order to compare
 * them row by row throws away the one thing the format gives you.
 *
 * So this path is columnar end to end. It reads the key columns, joins on them
 * once to produce a list of matched (a_row, b_row) pairs, and then walks the
 * compared columns one at a time, releasing each before reading the next. Where
 * both sides of a column are dictionary encoded, the two dictionaries are
 * mapped onto one shared id space once -- a few thousand string comparisons --
 * after which "did this cell change" is `int32 != int32`, which the compiler
 * vectorises, and the resulting mismatch mask is scanned eight bytes at a time
 * with the same SWAR trick the CSV parser uses to find delimiters.
 *
 * The answer is the same as comparing the same rows as CSV. That equivalence is
 * the test: test.sh generates a pair in both formats and requires the counts and
 * the per-column statistics to match.
 */
#ifndef CSVDIFF_PQDIFF_H
#define CSVDIFF_PQDIFF_H

#include <stddef.h>
#include <stdint.h>

typedef struct {
    char   *name;
    int64_t changed, blanked, filled;
} PqColStat;

typedef struct {
    int64_t    a_rows, b_rows, a_keys, b_keys;
    int64_t    matched, changed, added, removed;
    int64_t    a_dup_keys, a_dup_rows, b_dup_keys, b_dup_rows;
    PqColStat *cols;
    size_t     ncols;
} PqResult;

/* Whether the file begins with Parquet's `PAR1` magic: 1 yes, 0 no, -1 it cannot be
 * opened. Cheap: four bytes. */
int pq_is_parquet(const char *path);

/*
 * Both files must be Parquet. Returns 0, or -1 with pq_error() naming the
 * problem. `threads` of 0 means one per core.
 */
int pq_compare(const char *a_path, const char *b_path,
               char *const *key, size_t nkey,
               char *const *ignore, size_t nignore,
               char *const *compare, size_t ncompare,
               unsigned threads, PqResult *out);

void pq_result_free(PqResult *r);

#endif
