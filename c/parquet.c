/*
 * The reader. See parquet.h for what it does and does not carry.
 *
 * In reading order: the Thrift compact protocol, the slices of the file
 * metadata this reader uses, page headers, the RLE/bit-packed hybrid, PLAIN
 * byte arrays, and read_column, which folds all of it into one column.
 */
#define _GNU_SOURCE

#include "parallel.h"
#include "parquet.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------------- */
/* Errors                                                                      */
/* ------------------------------------------------------------------------- */

static _Thread_local char g_error[192];

const char *pq_error(void) { return g_error[0] ? g_error : "parquet: unknown failure"; }

int pq_set_error(const char *why) {
    snprintf(g_error, sizeof g_error, "%s", why);
    return -1;
}

int pq_set_errorf(const char *why, const char *name) {
    snprintf(g_error, sizeof g_error, "%s: %s", why, name);
    return -1;
}

/* The name the rest of this file reads better under. */
static int fail(const char *why) { return pq_set_error(why); }

static int fail_oom(void) { return fail("out of memory reading the parquet file"); }

/*
 * Growth by doubling, for the three arrays that cannot be sized in advance: a
 * dictionary, and the two per-row arrays before the footer's row count is
 * trusted. Returns 0, or -1 having left the old block intact.
 */
/*
 * What `grow` would choose, so a charged growth can price it before buying it.
 * Zero means the doubling would overflow.
 *
 * The first allocation is exact and only later ones double. Doubling exists to
 * amortise repeated appends, and the first ask is not an append -- it is
 * usually the only one, because both per-row arrays here are asked for at the
 * size the footer says the column is. Rounding that up to a power of two was
 * pure overshoot: a forty-million-row index wants 160 MB and was given 268 MB,
 * and a plain column's values wanted 320 MB and were given 537 MB, on top of
 * the twenty-odd reallocs and copies it took to climb there from 64.
 */
static size_t next_cap(size_t cap, size_t need) {
    if (cap == 0) return need;
    size_t want = cap;
    while (want < need) {
        if (want > (size_t)-1 / 2) return 0;
        want *= 2;
    }
    return want;
}

/* The footer's row count as a floor on the first ask, so the exact allocation
 * above lands on the size the column ends at rather than on the first page's
 * worth. It is a hint and not a trust: `grow_charged` still grows if the pages
 * deliver more values than the footer claimed. */
static size_t plain_need(size_t need, int64_t rows) {
    const size_t claimed = rows > 0 ? (size_t)rows : 0;
    return need > claimed ? need : claimed;
}

static int grow(void **p, size_t *cap, size_t need, size_t elem) {
    if (need <= *cap) return 0;
    const size_t want = next_cap(*cap, need);
    if (want == 0) return fail_oom();
    /*
     * Plain realloc, deliberately. These arrays were tried on huge pages too --
     * they are the same eighty megabytes the slot table is, walked just as
     * randomly -- and it cost 0.96s at ten million rows rather than saving
     * anything. The difference is how often: the two slot tables are allocated
     * once each, while a column buffer is allocated thirty-four times, on four
     * threads at once, and with `transparent_hugepage=madvise` every one of
     * those asks makes the kernel compact memory to find a 2 MB run. Huge pages
     * are worth having where an allocation is rare and long-lived, and not
     * where it is neither.
     */
    void *bigger = realloc(*p, want * elem);
    if (!bigger) return fail_oom();
    *p = bigger;
    *cap = want;
    return 0;
}

/*
 * `grow`, with the `--max-memory` ceiling told what the capacity costs.
 *
 * Only the row index used to be charged: four bytes a row, which is what a
 * dictionary-encoded column keeps. The `values` array is eight bytes a row and
 * is what a PLAIN column keeps instead, and `owned` holds whole decompressed
 * pages -- neither was counted, so on exactly the columns that cost most the
 * ceiling was reading low, by two times or more.
 *
 * Charged before the allocation rather than after, so a run that would go over
 * is refused instead of briefly going over and then being told.
 */
static int grow_charged(void **p, size_t *cap, size_t need, size_t elem, PqColumn *out) {
    if (need <= *cap) return 0;
    const size_t want = next_cap(*cap, need);
    if (want == 0) return fail_oom();
    const size_t added = (want - *cap) * elem;
    if (budget_take(added) != 0) return fail("over the --max-memory ceiling");
    /* Recorded before the grow so that a failed allocation still gives it back
     * when the column is freed: the column owns the charge, not this call. */
    out->budgeted += added;
    return grow(p, cap, need, elem);
}

/* ------------------------------------------------------------------------- */
/* Thrift compact protocol                                                     */
/*                                                                             */
/* Parquet's metadata and every page header are Thrift compact structs. The     */
/* format is small enough to read directly: a field header carries a delta from */
/* the previous field id and a type in one byte, integers are zigzag varints,   */
/* and a struct ends with a zero byte.                                          */
/*                                                                             */
/* There are no exceptions here, so a reader carries a `bad` flag instead: once */
/* set, every accessor returns a harmless zero and the caller checks once at    */
/* the end rather than after every read.                                        */
/* ------------------------------------------------------------------------- */

enum { T_STOP = 0, T_TRUE = 1, T_FALSE = 2, T_I8 = 3, T_I16 = 4, T_I32 = 5, T_I64 = 6,
       T_DOUBLE = 7, T_BINARY = 8, T_LIST = 9, T_SET = 10, T_MAP = 11, T_STRUCT = 12 };

typedef struct {
    const char *d;
    size_t      n;
    size_t      at;
    int         bad;
} Thrift;

static uint8_t th_byte(Thrift *t) {
    if (t->bad) return 0;
    if (t->at >= t->n) { t->bad = 1; return 0; }
    return (uint8_t)t->d[t->at++];
}

static uint64_t th_varint(Thrift *t) {
    uint64_t out = 0;
    int shift = 0;
    for (;;) {
        const uint8_t b = th_byte(t);
        if (t->bad) return 0;
        out |= (uint64_t)(b & 0x7F) << shift;
        if ((b & 0x80) == 0) return out;
        shift += 7;
        if (shift > 63) { t->bad = 1; return 0; }
    }
}

static int64_t th_zigzag(Thrift *t) {
    const uint64_t v = th_varint(t);
    return (int64_t)((v >> 1) ^ (~(v & 1) + 1));
}

/* The bytes stay in place; `len` is set to how many there are. */
static const char *th_binary(Thrift *t, size_t *len) {
    const size_t n = (size_t)th_varint(t);
    if (t->bad || t->at + n > t->n) { t->bad = 1; *len = 0; return t->d; }
    const char *out = t->d + t->at;
    t->at += n;
    *len = n;
    return out;
}

/* Reads a field header: the type, or T_STOP at the end of a struct. */
static int th_field(Thrift *t, int16_t *id, int16_t *last) {
    const uint8_t h = th_byte(t);
    if (t->bad || h == 0) return T_STOP;
    const int type = h & 0x0F;
    const int delta = (h & 0xF0) >> 4;
    *id = delta == 0 ? (int16_t)th_zigzag(t) : (int16_t)(*last + delta);
    *last = *id;
    return type;
}

/* Reads a list header, returning the element type and setting `count`. */
static int th_list(Thrift *t, uint32_t *count) {
    const uint8_t h = th_byte(t);
    *count = (h & 0xF0) >> 4;
    if (*count == 15) *count = (uint32_t)th_varint(t);
    return h & 0x0F;
}

/* Steps over a value without interpreting it, so a struct can be read for the
 * few fields that matter and the rest skipped. */
static void th_skip(Thrift *t, int type) {
    if (t->bad) return;
    switch (type) {
        case T_TRUE: case T_FALSE: return;
        case T_I8: th_byte(t); return;
        case T_I16: case T_I32: case T_I64: th_zigzag(t); return;
        case T_DOUBLE: t->at += 8; if (t->at > t->n) t->bad = 1; return;
        case T_BINARY: { size_t n; th_binary(t, &n); return; }
        case T_LIST: case T_SET: {
            uint32_t count = 0;
            const int elem = th_list(t, &count);
            for (uint32_t i = 0; i < count && !t->bad; i++) th_skip(t, elem);
            return;
        }
        case T_MAP: {
            uint32_t count = (uint32_t)th_varint(t);
            if (count == 0 || t->bad) return;
            const uint8_t kv = th_byte(t);
            for (uint32_t i = 0; i < count && !t->bad; i++) {
                th_skip(t, (kv & 0xF0) >> 4);
                th_skip(t, kv & 0x0F);
            }
            return;
        }
        case T_STRUCT: {
            int16_t id = 0, last = 0;
            for (;;) {
                const int ty = th_field(t, &id, &last);
                if (ty == T_STOP || t->bad) return;
                th_skip(t, ty);
            }
        }
        default: t->bad = 1; return;
    }
}

/* ------------------------------------------------------------------------- */
/* The slices of the file metadata this reader uses                            */
/* ------------------------------------------------------------------------- */

/* parquet.thrift Type */
enum { P_BOOLEAN = 0, P_INT32 = 1, P_INT64 = 2, P_INT96 = 3, P_FLOAT = 4, P_DOUBLE = 5,
       P_BYTE_ARRAY = 6, P_FIXED_LEN = 7 };
/* parquet.thrift Encoding */
enum { E_PLAIN = 0, E_PLAIN_DICTIONARY = 2, E_RLE = 3, E_RLE_DICTIONARY = 8 };
/* parquet.thrift CompressionCodec */
enum { C_UNCOMPRESSED = 0, C_SNAPPY = 1, C_GZIP = 2, C_LZ4_RAW = 7 };

/* ------------------------------------------------------------------------- */
/* Page decompression: snappy and LZ4, written out rather than linked          */
/*                                                                             */
/* Both are byte-copy loops with no tables and no allocation of their own: the */
/* output buffer is sized from the page header before either is called, so a   */
/* corrupt length is a refusal rather than an allocation the size of whatever  */
/* number was in the file. Gzip and zstd are deliberately absent -- their      */
/* decoders are real programs, and this port carries no dependency.            */
/* ------------------------------------------------------------------------- */

/* A cursor that fills `out` and refuses to run past it, which is what turns a
 * corrupt length into an error rather than a wild write. */
typedef struct { char *out; size_t cap, at; } Sink;

static int sink_literal(Sink *s, const char *from, size_t n) {
    if (n > s->cap - s->at) return fail("a compressed parquet page overruns its declared size");
    memcpy(s->out + s->at, from, n);
    s->at += n;
    return 0;
}

/* Copies `n` bytes from `distance` back, which may overlap what it is writing:
 * a run of one byte is encoded as a one-byte match repeated, so this cannot be
 * memcpy or memmove. */
static int sink_copy(Sink *s, size_t distance, size_t n) {
    if (distance == 0 || distance > s->at) return fail("a compressed parquet page copies from outside itself");
    if (n > s->cap - s->at) return fail("a compressed parquet page overruns its declared size");
    char *dst = s->out + s->at;
    const char *src = dst - distance;
    /* Eight bytes at a time where the source is at least eight behind, which
     * is most matches: each chunk reads only bytes already written, so the
     * byte-by-byte meaning is preserved. A closer match is a repeating run --
     * a one-byte distance is how a run of one byte is encoded -- and has to
     * stay a byte loop. */
    size_t i = 0;
    if (distance >= 8) {
        for (; i + 8 <= n; i += 8) memcpy(dst + i, src + i, 8);
    }
    for (; i < n; i++) dst[i] = src[i];
    s->at += n;
    return 0;
}

static int snappy_decode(const char *in, size_t in_len, char *out, size_t out_len) {
    size_t at = 0, declared = 0;
    unsigned shift = 0;
    for (;;) {
        if (at >= in_len) return fail("a snappy parquet page ends inside its length");
        const uint8_t b = (uint8_t)in[at++];
        declared |= (size_t)(b & 0x7f) << shift;
        if (!(b & 0x80)) break;
        if (shift >= 28) return fail("a snappy parquet page has a corrupt length");
        shift += 7;
    }
    if (declared != out_len) return fail("a snappy parquet page is not the size its header claims");

    Sink s = { out, out_len, 0 };
    while (at < in_len) {
        const uint8_t tag = (uint8_t)in[at++];
        if ((tag & 0x03) == 0) {
            /* A literal: a short length in the tag, or one to four bytes of it. */
            size_t n = tag >> 2;
            if (n >= 60) {
                const size_t extra = n - 59;
                if (in_len - at < extra) return fail("a snappy parquet page ends inside a literal length");
                n = 0;
                for (size_t i = 0; i < extra; i++) n |= (size_t)(uint8_t)in[at + i] << (8 * i);
                at += extra;
            }
            n += 1;
            if (in_len - at < n) return fail("a snappy parquet page ends inside a literal");
            if (sink_literal(&s, in + at, n) != 0) return -1;
            at += n;
            continue;
        }
        size_t n = 0, distance = 0;
        if ((tag & 0x03) == 1) {
            if (at >= in_len) return fail("a snappy parquet page ends inside a copy");
            n = 4 + ((tag >> 2) & 0x07);
            distance = ((size_t)(tag >> 5) << 8) | (uint8_t)in[at];
            at += 1;
        } else if ((tag & 0x03) == 2) {
            if (in_len - at < 2) return fail("a snappy parquet page ends inside a copy");
            n = (size_t)(tag >> 2) + 1;
            distance = (size_t)(uint8_t)in[at] | ((size_t)(uint8_t)in[at + 1] << 8);
            at += 2;
        } else {
            if (in_len - at < 4) return fail("a snappy parquet page ends inside a copy");
            n = (size_t)(tag >> 2) + 1;
            distance = (size_t)(uint8_t)in[at] | ((size_t)(uint8_t)in[at + 1] << 8) |
                       ((size_t)(uint8_t)in[at + 2] << 16) | ((size_t)(uint8_t)in[at + 3] << 24);
            at += 4;
        }
        if (sink_copy(&s, distance, n) != 0) return -1;
    }
    if (s.at != out_len) return fail("a snappy parquet page is shorter than its header claims");
    return 0;
}

static int lz4_decode(const char *in, size_t in_len, char *out, size_t out_len) {
    Sink s = { out, out_len, 0 };
    size_t at = 0;
    while (at < in_len) {
        const uint8_t token = (uint8_t)in[at++];
        size_t literals = token >> 4;
        if (literals == 15) {
            for (;;) {
                if (at >= in_len) return fail("an lz4 parquet page ends inside a literal length");
                const uint8_t b = (uint8_t)in[at++];
                literals += b;
                if (b != 255) break;
            }
        }
        if (in_len - at < literals) return fail("an lz4 parquet page ends inside a literal");
        if (sink_literal(&s, in + at, literals) != 0) return -1;
        at += literals;
        /* The last sequence of a block is literals only, with no match after. */
        if (at >= in_len) break;
        if (in_len - at < 2) return fail("an lz4 parquet page ends inside a match offset");
        const size_t distance = (size_t)(uint8_t)in[at] | ((size_t)(uint8_t)in[at + 1] << 8);
        at += 2;
        size_t n = token & 0x0f;
        if (n == 15) {
            for (;;) {
                if (at >= in_len) return fail("an lz4 parquet page ends inside a match length");
                const uint8_t b = (uint8_t)in[at++];
                n += b;
                if (b != 255) break;
            }
        }
        if (sink_copy(&s, distance, n + 4) != 0) return -1;
    }
    if (s.at != out_len) return fail("an lz4 parquet page is shorter than its header claims");
    return 0;
}
/* parquet.thrift PageType */
enum { PG_DATA = 0, PG_INDEX = 1, PG_DICTIONARY = 2, PG_DATA_V2 = 3 };

typedef struct {
    int     type;
    int     codec;
    int64_t num_values;
    int64_t data_page_offset;
    int64_t dictionary_page_offset;
    int64_t total_compressed_size;
    int64_t total_uncompressed_size;
} ChunkMeta;

typedef struct {
    ChunkMeta *columns;
    size_t     columns_len;
    int64_t    rows;
} RowGroupMeta;

typedef struct {
    char        **names;    size_t names_len;
    int          *optional;              /* 1 where the column may be null */
    RowGroupMeta *groups;   size_t groups_len;
    int64_t       rows;
} FileMeta;

static void file_meta_free(FileMeta *m) {
    for (size_t i = 0; i < m->names_len; i++) free(m->names[i]);
    free(m->names);
    free(m->optional);
    for (size_t i = 0; i < m->groups_len; i++) free(m->groups[i].columns);
    free(m->groups);
    memset(m, 0, sizeof *m);
}

static void read_column_meta(Thrift *t, ChunkMeta *out) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = th_field(t, &id, &last);
        if (type == T_STOP || t->bad) return;
        switch (id) {
            case 1: out->type = (int)th_zigzag(t); break;
            case 2: {                                     /* encodings */
                uint32_t n = 0;
                const int elem = th_list(t, &n);
                for (uint32_t i = 0; i < n && !t->bad; i++) th_skip(t, elem);
                break;
            }
            case 3: {                                     /* path_in_schema */
                uint32_t n = 0;
                const int elem = th_list(t, &n);
                for (uint32_t i = 0; i < n && !t->bad; i++) th_skip(t, elem);
                break;
            }
            case 4:  out->codec = (int)th_zigzag(t); break;
            case 5:  out->num_values = th_zigzag(t); break;
            case 6:  out->total_uncompressed_size = th_zigzag(t); break;
            case 7:  out->total_compressed_size = th_zigzag(t); break;
            case 9:  out->data_page_offset = th_zigzag(t); break;
            case 11: out->dictionary_page_offset = th_zigzag(t); break;
            default: th_skip(t, type); break;
        }
    }
}

static void read_chunk(Thrift *t, ChunkMeta *out) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = th_field(t, &id, &last);
        if (type == T_STOP || t->bad) return;
        if (id == 3) read_column_meta(t, out);   /* meta_data */
        else         th_skip(t, type);
    }
}

static int read_row_group(Thrift *t, RowGroupMeta *out) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = th_field(t, &id, &last);
        if (type == T_STOP || t->bad) return 0;
        if (id == 1) {                            /* columns */
            uint32_t n = 0;
            th_list(t, &n);
            if (t->bad) return 0;
            out->columns = calloc(n ? n : 1, sizeof *out->columns);
            if (!out->columns) return fail_oom();
            out->columns_len = n;
            for (uint32_t i = 0; i < n; i++) {
                out->columns[i].type = -1;
                read_chunk(t, &out->columns[i]);
            }
        } else if (id == 3) {                     /* num_rows */
            out->rows = th_zigzag(t);
        } else {
            th_skip(t, type);
        }
    }
}

/*
 * The footer: `PAR1`, then the metadata length, then `PAR1` again at the very
 * end. Everything this reader knows about the file comes from here.
 */
static int read_file_meta(const char *data, size_t size, FileMeta *out) {
    memset(out, 0, sizeof *out);
    if (size < 12 || memcmp(data, "PAR1", 4) != 0 || memcmp(data + size - 4, "PAR1", 4) != 0)
        return fail("not a parquet file");

    uint32_t meta_len = 0;
    memcpy(&meta_len, data + size - 8, 4);
    if ((size_t)meta_len + 8u > size) return fail("the parquet footer is longer than the file");

    Thrift t = { data + size - 8 - meta_len, meta_len, 0, 0 };
    size_t names_cap = 0, opt_cap = 0;
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = th_field(&t, &id, &last);
        if (type == T_STOP || t.bad) break;
        switch (id) {
            case 2: {                                     /* schema */
                uint32_t n = 0;
                th_list(&t, &n);
                for (uint32_t i = 0; i < n && !t.bad; i++) {
                    /* SchemaElement: repetition_type 3, name 4, num_children 5. */
                    int16_t sid = 0, slast = 0;
                    const char *name = NULL;
                    size_t name_len = 0;
                    int repetition = -1, children = 0;
                    for (;;) {
                        const int st = th_field(&t, &sid, &slast);
                        if (st == T_STOP || t.bad) break;
                        if (sid == 3)      repetition = (int)th_zigzag(&t);
                        else if (sid == 4) name = th_binary(&t, &name_len);
                        else if (sid == 5) children = (int)th_zigzag(&t);
                        else               th_skip(&t, st);
                    }
                    /* The first element is the root, and anything with children
                     * is a group rather than a column this reader can read. */
                    if (i == 0 || t.bad) continue;
                    if (children > 0) {
                        file_meta_free(out);
                        return fail("nested parquet columns are not read here");
                    }
                    if (grow((void **)&out->names, &names_cap, out->names_len + 1,
                             sizeof *out->names) != 0 ||
                        grow((void **)&out->optional, &opt_cap, out->names_len + 1,
                             sizeof *out->optional) != 0) {
                        file_meta_free(out);
                        return -1;
                    }
                    char *copy = malloc(name_len + 1);
                    if (!copy) { file_meta_free(out); return fail_oom(); }
                    memcpy(copy, name, name_len);
                    copy[name_len] = '\0';
                    out->optional[out->names_len] = repetition == 1 ? 1 : 0;  /* 1 = OPTIONAL */
                    out->names[out->names_len++] = copy;
                }
                break;
            }
            case 3: out->rows = th_zigzag(&t); break;
            case 4: {                                     /* row_groups */
                uint32_t n = 0;
                th_list(&t, &n);
                if (t.bad) break;
                out->groups = calloc(n ? n : 1, sizeof *out->groups);
                if (!out->groups) { file_meta_free(out); return fail_oom(); }
                out->groups_len = n;
                for (uint32_t i = 0; i < n; i++)
                    if (read_row_group(&t, &out->groups[i]) != 0) {
                        file_meta_free(out);
                        return -1;
                    }
                break;
            }
            default: th_skip(&t, type); break;
        }
    }
    if (t.bad) { file_meta_free(out); return fail("the parquet footer is malformed"); }
    if (out->names_len == 0) { file_meta_free(out); return fail("the parquet schema has no columns"); }
    return 0;
}

/* ------------------------------------------------------------------------- */
/* Page headers                                                                */
/* ------------------------------------------------------------------------- */

typedef struct {
    int     type;
    int32_t uncompressed;
    int32_t compressed;
    int32_t num_values;
    int     encoding;
    int     def_encoding;
    size_t  after;        /* offset of the page body */
} PageHead;

static int read_page_head(const char *data, size_t size, size_t at, PageHead *out) {
    Thrift t = { data, size, at, 0 };
    memset(out, 0, sizeof *out);
    out->type = -1;
    out->encoding = -1;
    out->def_encoding = E_RLE;

    int16_t id = 0, last = 0;
    for (;;) {
        const int type = th_field(&t, &id, &last);
        if (type == T_STOP || t.bad) break;
        switch (id) {
            case 1: out->type = (int)th_zigzag(&t); break;
            case 2: out->uncompressed = (int32_t)th_zigzag(&t); break;
            case 3: out->compressed = (int32_t)th_zigzag(&t); break;
            case 5: {                                     /* data_page_header */
                int16_t hid = 0, hlast = 0;
                for (;;) {
                    const int ht = th_field(&t, &hid, &hlast);
                    if (ht == T_STOP || t.bad) break;
                    if (hid == 1)      out->num_values = (int32_t)th_zigzag(&t);
                    else if (hid == 2) out->encoding = (int)th_zigzag(&t);
                    else if (hid == 3) out->def_encoding = (int)th_zigzag(&t);
                    else               th_skip(&t, ht);
                }
                break;
            }
            case 7: {                                     /* dictionary_page_header */
                int16_t hid = 0, hlast = 0;
                for (;;) {
                    const int ht = th_field(&t, &hid, &hlast);
                    if (ht == T_STOP || t.bad) break;
                    if (hid == 1)      out->num_values = (int32_t)th_zigzag(&t);
                    else if (hid == 2) out->encoding = (int)th_zigzag(&t);
                    else               th_skip(&t, ht);
                }
                break;
            }
            case 8: return fail("parquet data page v2 is not read here");
            default: th_skip(&t, type); break;
        }
    }
    if (t.bad) return fail("a parquet page header is malformed");
    out->after = t.at;
    return 0;
}

/* ------------------------------------------------------------------------- */
/* RLE / bit-packed hybrid                                                     */
/*                                                                             */
/* How definition levels and dictionary indices are written. A run header is a  */
/* varint: the low bit says which kind, the rest is the length.                */
/* ------------------------------------------------------------------------- */

typedef struct {
    const char *d;
    size_t      n;
    int         width;
    uint64_t    mask;
    size_t      at;
    size_t      left;
    size_t      bit;
    int         packed;
    int32_t     value;
} Rle;

static void rle_init(Rle *r, const char *d, size_t n, int width) {
    r->d = d; r->n = n; r->width = width;
    r->mask = width >= 64 ? ~UINT64_C(0) : (UINT64_C(1) << width) - 1;
    r->at = 0; r->left = 0; r->bit = 0; r->packed = 0; r->value = 0;
}

static int rle_header(Rle *r) {
    uint64_t h = 0;
    int shift = 0;
    for (;;) {
        if (r->at >= r->n) return 0;
        const uint8_t b = (uint8_t)r->d[r->at++];
        h |= (uint64_t)(b & 0x7F) << shift;
        if ((b & 0x80) == 0) break;
        shift += 7;
        if (shift > 63) return 0;
    }
    if ((h & 1) == 0) {                       /* RLE run: a count and one value */
        r->packed = 0;
        r->left = (size_t)(h >> 1);
        const size_t bytes = (size_t)((r->width + 7) / 8);
        r->value = 0;
        if (r->at + bytes > r->n) return 0;
        for (size_t i = 0; i < bytes; i++)
            r->value |= (int32_t)((uint8_t)r->d[r->at + i]) << (8 * i);
        r->at += bytes;
    } else {                                  /* bit-packed run, in groups of eight */
        r->packed = 1;
        const size_t groups = (size_t)(h >> 1);
        r->left = groups * 8;
        r->bit = r->at * 8;
        /* A group of eight values is exactly `width` bytes, so the whole run's
         * length is known and `at` can jump straight to the next header while
         * `bit` walks inside it. */
        const size_t run = groups * (size_t)r->width;
        r->at = r->at + run > r->n ? r->n : r->at + run;
    }
    return r->left > 0;
}

/*
 * Fills `want` values. Bulk rather than one at a time on purpose: an RLE run
 * becomes a fill, and a bit-packed run becomes one 64-bit load, one shift and
 * one mask per value.
 *
 * Values are packed end to end with no padding, so a value straddles bytes more
 * often than not. Reading eight bytes around it and shifting picks any of them
 * out without a loop -- the same "look at eight bytes at once" trick the CSV
 * scanner uses to find a delimiter, here reading rather than searching. It is
 * exact for every width Parquet allows: seven bits of misalignment plus
 * thirty-two of value still fits in a word.
 */
static int rle_fill(Rle *r, int32_t *out, size_t want) {
    size_t done = 0;
    while (done < want) {
        if (r->left == 0 && !rle_header(r)) return 0;
        size_t take = want - done;
        if (take > r->left) take = r->left;
        if (!r->packed) {
            for (size_t i = 0; i < take; i++) out[done + i] = r->value;
        } else if (r->width == 0) {
            memset(out + done, 0, take * sizeof *out);
        } else {
            /*
             * Whether an eight-byte window still fits inside the buffer is only
             * in question for the last few values of the last run, so how many
             * are safe is computed once and the common case runs with no test
             * at all. The tail then reassembles its word a byte at a time.
             */
            size_t i = 0;
            if (r->n >= 8) {
                const size_t last = (r->n - 8) * 8;   /* highest bit with a whole word above it */
                if (r->bit <= last) {
                    size_t safe = (last - r->bit) / (size_t)r->width + 1;
                    if (safe > take) safe = take;
                    for (; i < safe; i++) {
                        uint64_t w;
                        memcpy(&w, r->d + (r->bit >> 3), 8);
                        out[done + i] = (int32_t)((w >> (r->bit & 7)) & r->mask);
                        r->bit += (size_t)r->width;
                    }
                }
            }
            for (; i < take; i++) {
                const size_t byte = r->bit >> 3;
                uint64_t w = 0;
                for (size_t k = 0; k < 8 && byte + k < r->n; k++)
                    w |= (uint64_t)((uint8_t)r->d[byte + k]) << (8 * k);
                out[done + i] = (int32_t)((w >> (r->bit & 7)) & r->mask);
                r->bit += (size_t)r->width;
            }
        }
        r->left -= take;
        done += take;
    }
    return 1;
}

/* ------------------------------------------------------------------------- */
/* PLAIN byte arrays                                                           */
/*                                                                             */
/* A four-byte little-endian length, then the bytes, repeated. The slices point */
/* straight into the mapping, so nothing is copied.                             */
/* ------------------------------------------------------------------------- */

static int plain_slices(const char *page, size_t n, uint64_t base, int32_t count,
                        PqSlice **out, size_t *len, size_t *cap) {
    if (grow((void **)out, cap, *len + (size_t)count, sizeof **out) != 0) return -1;
    size_t at = 0;
    for (int32_t i = 0; i < count; i++) {
        if (at + 4 > n) return fail("a parquet page ends inside a value");
        uint32_t vl = 0;
        memcpy(&vl, page + at, 4);
        at += 4;
        if (at + vl > n) return fail("a parquet value runs past its page");
        if (vl > PQ_MAX_LENGTH) return fail("a parquet value is larger than eight megabytes");
        (*out)[(*len)++] = pq_slice(base + at, vl);
        at += vl;
    }
    return 0;
}

/* ------------------------------------------------------------------------- */
/* The public entry points                                                     */
/* ------------------------------------------------------------------------- */

void pq_meta_free(PqMeta *m) {
    for (size_t i = 0; i < m->names_len; i++) free(m->names[i]);
    free(m->names);
    memset(m, 0, sizeof *m);
}

void pq_column_free(PqColumn *c) {
    budget_give(c->budgeted);
    free(c->dict);
    free(c->index);
    free(c->values);
    free(c->owned);
    memset(c, 0, sizeof *c);
}

int pq_read_meta(const char *data, size_t size, PqMeta *out) {
    memset(out, 0, sizeof *out);
    FileMeta fm;
    if (read_file_meta(data, size, &fm) != 0) return -1;
    out->names = fm.names;             /* taken over wholesale */
    out->names_len = fm.names_len;
    out->rows = fm.rows;
    out->row_groups = fm.groups_len;
    fm.names = NULL;
    fm.names_len = 0;
    file_meta_free(&fm);
    return 0;
}

/*
 * Reads one column across every row group.
 *
 * The column starts out held as dictionary indices and stays that way only if
 * every page cooperates. A writer that gives up on the dictionary partway --
 * DuckDB does, once a column's distinct values outgrow its budget, which at ten
 * million rows is most of them -- forces the whole column into the plain form.
 * Expanding what has been read costs no bytes: a dictionary entry is already a
 * slice, so `degrade` copies eight-byte handles, not values.
 */
int pq_read_column(const char *data, size_t size, size_t which, PqColumn *out) {
    memset(out, 0, sizeof *out);

    FileMeta fm;
    if (read_file_meta(data, size, &fm) != 0) return -1;
    if (which >= fm.names_len) { file_meta_free(&fm); return fail("no such column in the parquet file"); }
    const int optional = fm.optional[which] != 0;

    int      status = -1;
    int      dictionary = 1;
    size_t   dict_cap = 0, index_cap = 0, values_cap = 0;
    int32_t *defs = NULL;  size_t defs_cap = 0;
    /* Where decompressed pages land, when there are any. A column is wholly
     * compressed or wholly not, so this being non-NULL is what tells every
     * slice in the column what its offset counts from. */
    size_t owned_cap = 0;
    int    col_codec = -1;
    int32_t *idx = NULL;   size_t idx_cap = 0;
    PqSlice *got = NULL;   size_t got_len = 0, got_cap = 0;

    /* Sized once from the footer's row count, so appending a page never has to
     * move eighty megabytes of what is already decoded. */
    if (fm.rows > 0) {
        const size_t want = (size_t)fm.rows * sizeof *out->index;
        if (budget_take(want) != 0) {
            fail("over the --max-memory ceiling");
            goto done;
        }
        /* Recorded on the column, not in a local, because what gives it back is
         * `pq_column_free`, which runs wherever the caller is finished with it
         * -- including the error paths below. */
        out->budgeted = want;
    }
    if (fm.rows > 0 &&
        grow((void **)&out->index, &index_cap, (size_t)fm.rows, sizeof *out->index) != 0)
        goto done;

    for (size_t g = 0; g < fm.groups_len; g++) {
        if (which >= fm.groups[g].columns_len) { fail("a row group is missing a column"); goto done; }
        const ChunkMeta *c = &fm.groups[g].columns[which];
        if (c->type != P_BYTE_ARRAY) { fail("only BYTE_ARRAY parquet columns are read here"); goto done; }
        if (c->codec != C_UNCOMPRESSED && c->codec != C_SNAPPY && c->codec != C_LZ4_RAW) {
            fail(c->codec == C_GZIP
                     ? "gzip parquet is not read here; this port carries snappy and lz4 only"
                     : "that parquet codec is not read here; this port carries snappy and lz4 only");
            goto done;
        }
        /* Every chunk has to agree, because a slice is an offset with no room
         * to say what it is an offset *into*. */
        if (col_codec < 0) col_codec = c->codec;
        else if (col_codec != c->codec) {
            fail("a parquet column compresses some chunks and not others");
            goto done;
        }

        size_t       at = (size_t)(c->dictionary_page_offset > 0 ? c->dictionary_page_offset
                                                                 : c->data_page_offset);
        const size_t stop = at + (size_t)c->total_compressed_size;
        int64_t      seen = 0;
        const size_t dict_base = out->dict_len;

        while (at < stop && at < size && seen < c->num_values) {
            PageHead h;
            if (read_page_head(data, size, at, &h) != 0) goto done;
            const size_t body = (size_t)h.compressed;
            if (h.after + body > size) { fail("a parquet page runs past the file"); goto done; }

            /* Uncompressed pages are read where they lie, so every offset is
             * into the mapping and nothing is copied. A compressed one is
             * decompressed onto the end of `owned`, and its offsets count from
             * there instead -- `owned` may move as it grows, which is exactly
             * why a slice is an offset and not a pointer. */
            const char  *page;
            size_t       page_len;
            uint64_t     page_base;
            if (col_codec == C_UNCOMPRESSED) {
                page = data + h.after;
                page_len = body;
                page_base = h.after;
            } else {
                if (h.uncompressed < 0) { fail("a compressed parquet page declares no size"); goto done; }
                const size_t want = (size_t)h.uncompressed;
                if (grow_charged((void **)&out->owned, &owned_cap, out->owned_len + want, 1,
                                 out) != 0) goto done;
                char *dst = out->owned + out->owned_len;
                if ((col_codec == C_SNAPPY ? snappy_decode : lz4_decode)(
                        data + h.after, body, dst, want) != 0)
                    goto done;
                page_base = out->owned_len;
                out->owned_len += want;
                page = dst;
                page_len = want;
            }

            if (h.type == PG_DICTIONARY) {
                if (h.encoding != E_PLAIN && h.encoding != E_PLAIN_DICTIONARY) {
                    fail("only PLAIN parquet dictionaries are read here");
                    goto done;
                }
                if (plain_slices(page, page_len, page_base, h.num_values,
                                 &out->dict, &out->dict_len, &dict_cap) != 0)
                    goto done;
            } else if (h.type == PG_DATA) {
                const size_t n_vals = (size_t)h.num_values;
                size_t vat = 0, real = n_vals;
                if (optional) {
                    /* Definition levels: RLE, four-byte length prefix in v1. */
                    if (h.def_encoding != E_RLE) { fail("parquet definition levels are not RLE"); goto done; }
                    if (page_len < 4) { fail("a parquet page has no definition levels"); goto done; }
                    uint32_t dl = 0;
                    memcpy(&dl, page, 4);
                    if (4 + (size_t)dl > page_len) { fail("a parquet page ends inside its levels"); goto done; }
                    if (grow((void **)&defs, &defs_cap, n_vals, sizeof *defs) != 0) goto done;
                    Rle r;
                    rle_init(&r, page + 4, dl, 1);
                    if (!rle_fill(&r, defs, n_vals)) { fail("a parquet page ran out of definition levels"); goto done; }
                    real = 0;
                    for (size_t i = 0; i < n_vals; i++) real += defs[i] ? 1 : 0;
                    vat = 4 + dl;
                }
                if (vat > page_len) { fail("a parquet page ends inside its levels"); goto done; }

                if (h.encoding == E_PLAIN_DICTIONARY || h.encoding == E_RLE_DICTIONARY) {
                    if (out->dict_len == 0) { fail("a parquet data page wants a dictionary there is none of"); goto done; }
                    if (vat + 1 > page_len) { fail("a parquet page has no bit width"); goto done; }
                    const int width = (int)(uint8_t)page[vat];
                    if (width > 32) { fail("a parquet dictionary index is wider than 32 bits"); goto done; }
                    if (grow((void **)&idx, &idx_cap, real ? real : 1, sizeof *idx) != 0) goto done;
                    Rle r;
                    rle_init(&r, page + vat + 1, page_len - vat - 1, width);
                    if (!rle_fill(&r, idx, real)) { fail("a parquet page ran out of dictionary indices"); goto done; }
                    /*
                     * One bounds check for the page rather than one per value.
                     * A max-reduction has no loop-carried dependency, so it
                     * vectorises, where the per-value check it replaces sat in
                     * the middle of a dependent load-modify-store.
                     */
                    uint32_t widest = 0;
                    for (size_t i = 0; i < real; i++) {
                        /* Unsigned on purpose. A width of 32 can decode to a
                         * value with the top bit set, which as int32 is
                         * negative -- it would pass a signed max but then index
                         * the dictionary at a vast offset. Read as uint32 it is
                         * simply enormous, and fails the one check below, which
                         * is what the per-value check this replaced did. */
                        const uint32_t v = (uint32_t)idx[i];
                        if (v > widest) widest = v;
                    }
                    if (real && dict_base + (size_t)widest >= out->dict_len) {
                        fail("a parquet dictionary index is out of range");
                        goto done;
                    }
                    /*
                     * The push below used to be a second pass over the page,
                     * after the one that rebased the indices, and it tested
                     * `dictionary` and `optional` once per value although
                     * neither changes inside a column. Rebasing now happens
                     * where the value is stored, and the two invariants are
                     * branched on once, outside.
                     */
                    if (dictionary) {
                        if (grow_charged((void **)&out->index, &index_cap, out->index_len + n_vals,
                                         sizeof *out->index, out) != 0) goto done;
                        int32_t *dst = out->index + out->index_len;
                        if (optional) {
                            size_t k = 0;
                            for (size_t i = 0; i < n_vals; i++)
                                dst[i] = defs[i] ? (int32_t)(dict_base + (size_t)idx[k++])
                                                 : PQ_NULL_INDEX;
                        } else {
                            for (size_t i = 0; i < n_vals; i++)
                                dst[i] = (int32_t)(dict_base + (size_t)idx[i]);
                        }
                        out->index_len += n_vals;
                    } else {
                        if (grow_charged((void **)&out->values, &values_cap,
                                         plain_need(out->values_len + n_vals, fm.rows),
                                         sizeof *out->values, out) != 0) goto done;
                        PqSlice *dst = out->values + out->values_len;
                        if (optional) {
                            size_t k = 0;
                            for (size_t i = 0; i < n_vals; i++)
                                dst[i] = defs[i]
                                             ? out->dict[dict_base + (size_t)idx[k++]]
                                             : PQ_SLICE_NULL;
                        } else {
                            for (size_t i = 0; i < n_vals; i++)
                                dst[i] = out->dict[dict_base + (size_t)idx[i]];
                        }
                        out->values_len += n_vals;
                    }
                } else if (h.encoding == E_PLAIN) {
                    if (dictionary) {                     /* degrade */
                        /* Sized from the footer's row count, not from what has
                         * been decoded so far. Growing by doubling from the
                         * first plain page means reallocating an eighty-megabyte
                         * array eighteen times on a ten-million-row column, and
                         * copying it every time -- which is most of what reading
                         * a plain column used to cost. */
                        const size_t want = (size_t)(fm.rows > 0 ? fm.rows : 1);
                        if (grow_charged((void **)&out->values, &values_cap,
                                         want > out->index_len ? want : out->index_len,
                                         sizeof *out->values, out) != 0)
                            goto done;
                        for (size_t i = 0; i < out->index_len; i++) {
                            const int32_t k = out->index[i];
                            out->values[i] = k >= 0 ? out->dict[(size_t)k] : PQ_SLICE_NULL;
                        }
                        out->values_len = out->index_len;
                        free(out->index);
                        out->index = NULL;
                        out->index_len = 0;
                        index_cap = 0;
                        dictionary = 0;
                    }
                    got_len = 0;
                    if (plain_slices(page + vat, page_len - vat, page_base + vat, (int32_t)real,
                                     &got, &got_len, &got_cap) != 0)
                        goto done;
                    if (grow_charged((void **)&out->values, &values_cap,
                                     plain_need(out->values_len + n_vals, fm.rows),
                                     sizeof *out->values, out) != 0) goto done;
                    PqSlice *dst = out->values + out->values_len;
                    if (optional) {
                        size_t k = 0;
                        for (size_t i = 0; i < n_vals; i++)
                            dst[i] = defs[i] ? got[k++] : PQ_SLICE_NULL;
                    } else {
                        memcpy(dst, got, n_vals * sizeof *dst);
                    }
                    out->values_len += n_vals;
                } else {
                    fail("only PLAIN and dictionary parquet encodings are read here");
                    goto done;
                }
                seen += h.num_values;
            } else if (h.type != PG_INDEX) {
                fail("unknown parquet page type");
                goto done;
            }
            at = h.after + body;
        }
    }

    out->dictionary = dictionary;
    /* A column that never met a page at all is plain and empty, not dictionary. */
    if (dictionary && out->index_len == 0 && out->dict_len == 0) out->dictionary = 0;
    status = 0;

done:
    free(defs);
    free(idx);
    free(got);
    file_meta_free(&fm);
    if (status != 0) pq_column_free(out);
    return status;
}
