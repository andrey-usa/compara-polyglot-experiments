/*
 * A Parquet writer for generating fixtures, in C, uncompressed only.
 *
 * The mirror of parquet.c, and deliberately the same bargain: BYTE_ARRAY
 * columns, PLAIN and dictionary encodings, no codec. It exists so this port can
 * make its own test data and its own benchmark inputs rather than borrowing the
 * C++ generator's, which is one toolchain fewer to build before a check can run.
 *
 * The bytes it emits match what cpp/tools/pq_write.cpp emits for the same rows
 * and the same options, because the two are read by the same readers and a
 * benchmark number from one has to be comparable with a number from the other.
 */
#ifndef CSVDIFF_PQWRITE_H
#define CSVDIFF_PQWRITE_H

#include <stddef.h>
#include <stdint.h>

/* A cell: bytes, or absent. An absent cell is written as a Parquet null, which
 * is what every reader here treats an empty CSV field as anyway. */
typedef struct {
    const char *p;
    size_t      n;
    int         null;
} PqValue;

typedef struct PqWriter PqWriter;

/*
 * `dict_limit` is how many distinct values a column may hold in one row group
 * before the writer gives up on the dictionary for it. Giving up partway is the
 * point rather than a limitation: it is what produces a column dictionary
 * encoded in one row group and plain in the next, which is what a real writer
 * does to a high-cardinality string and what a reader has to fold together.
 */
PqWriter *pqw_open(const char *path, char *const *names, size_t columns,
                   size_t row_group_rows, size_t dict_limit);

/* Appends one row. `cells` has `columns` entries. Returns 0, or -1. */
int pqw_row(PqWriter *w, const PqValue *cells);

/* Writes the footer and closes. Returns 0, or -1. Frees the writer either way. */
int pqw_close(PqWriter *w);

/* Why the last call failed. */
const char *pqw_error(void);

#endif
