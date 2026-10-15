/*
 * A Parquet reader for comparing, in C: uncompressed, snappy and LZ4.
 *
 * Snappy and LZ4 are written out here rather than pulled in, the same choice
 * the Zig port made and for the same reason -- each is a byte-copy loop of
 * about eighty lines, and a port whose selling point is that it carries no
 * dependency should not acquire one to read the codec every Parquet writer
 * defaults to. Gzip and zstd are not here: their decoders are real programs,
 * and writing those out would be a library in this file rather than a loop.
 * Both are refused by name.
 *
 * Two things it does that a general reader would not, both inherited from the
 * C++ design and both the reason the comparison above it can stay columnar.
 *
 * It hands back offsets rather than strings. A PLAIN byte array is a four-byte
 * length followed by its bytes, already contiguous, so a value stays an offset
 * and a length -- the same eight-byte packing the CSV engine uses for a field,
 * and the reason nothing here builds a string per cell.
 *
 * What those offsets count from is `owned` when it is non-empty and the mapping
 * when it is not. A column is wholly compressed or wholly not -- a column whose
 * chunks disagree is refused -- so that one field settles it for every slice in
 * the column, and `pq_base()` is the only place a caller has to ask. Reading an
 * uncompressed file is unchanged: `owned` stays NULL and every offset is into
 * the mapping, as it always was.
 *
 * And it keeps dictionary columns encoded, so two files can be compared by
 * mapping one dictionary onto the other once and then comparing integers.
 */
#ifndef CSVDIFF_PARQUET_H
#define CSVDIFF_PARQUET_H

#include <stddef.h>
#include <stdint.h>

/*
 * A value's bytes as one word: forty bits of offset, twenty-three of length,
 * and the top bit set when the value is null. Forty bits is a terabyte of file;
 * twenty-three is eight megabytes of value, and a longer one is refused by name
 * rather than truncated.
 */
typedef uint64_t PqSlice;

#define PQ_OFFSET_MASK  ((UINT64_C(1) << 40) - 1)
#define PQ_LENGTH_SHIFT 40
#define PQ_MAX_LENGTH   ((UINT32_C(1) << 23) - 1)
#define PQ_NULL_BIT     (UINT64_C(1) << 63)
#define PQ_SLICE_NULL   PQ_NULL_BIT

/* A dictionary index of -1 is a null cell. */
#define PQ_NULL_INDEX (-1)

static inline PqSlice pq_slice(uint64_t off, uint32_t len) {
    return (off & PQ_OFFSET_MASK) | ((uint64_t)len << PQ_LENGTH_SHIFT);
}
static inline int      pq_null(PqSlice s) { return (s & PQ_NULL_BIT) != 0; }
static inline uint64_t pq_off(PqSlice s)  { return s & PQ_OFFSET_MASK; }
static inline uint32_t pq_len(PqSlice s)  {
    return (uint32_t)((s >> PQ_LENGTH_SHIFT) & PQ_MAX_LENGTH);
}

/*
 * One column of one file, decoded as far as it is useful to decode it.
 *
 * A dictionary column keeps `dict` and `index`: index[row] selects a value, or
 * is PQ_NULL_INDEX. A plain column keeps `values`, one slice per row. The
 * comparison reads whichever is populated, and the dictionary form is the one
 * worth having.
 */
typedef struct {
    int      dictionary;
    PqSlice *dict;    size_t dict_len;
    int32_t *index;   size_t index_len;
    PqSlice *values;  size_t values_len;
    /* Decompressed pages, when the column was compressed. Empty means every
     * slice above counts from the mapping instead. See pq_base(). */
    char    *owned;   size_t owned_len;
    /* What this column took from the `--max-memory` ceiling, so freeing it can
     * give exactly that back. A column is read, compared and released before
     * the next is asked for, so without this the ceiling counted every column
     * a run ever read rather than the one or two it holds. */
    size_t   budgeted;
} PqColumn;

/* What a file says about itself, before any column is read. */
typedef struct {
    char  **names;  size_t names_len;   /* leaf columns, in file order */
    int64_t rows;
    size_t  row_groups;
} PqMeta;

static inline size_t pq_rows(const PqColumn *c) {
    return c->dictionary ? c->index_len : c->values_len;
}

/* What this column's slices count from: its own buffer, or the mapping. */
static inline const char *pq_base(const PqColumn *c, const char *mapping) {
    return c->owned ? c->owned : mapping;
}

/*
 * Why a call failed, on the calling thread. Columns are read on several threads
 * at once, so the message has to be per-thread or two failures would race.
 */
const char *pq_error(void);

/* Sets it, and returns -1 so a caller can `return pq_set_error(...)`. */
int pq_set_error(const char *why);
/* The same, naming what it is about: `<why>: <name>`, as the text path says it. */
int pq_set_errorf(const char *why, const char *name);

/* Both return 0, or -1 with pq_error() set. */
int  pq_read_meta(const char *data, size_t size, PqMeta *out);
int  pq_read_column(const char *data, size_t size, size_t which, PqColumn *out);

void pq_meta_free(PqMeta *m);
void pq_column_free(PqColumn *c);

#endif
