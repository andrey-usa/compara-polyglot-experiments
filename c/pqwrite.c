/* The writer. See pqwrite.h for what it does and does not carry. */
#define _GNU_SOURCE

#include "parallel.h"
#include "pqwrite.h"

#include <fcntl.h>
#include "win32.h"   /* O_BINARY: a no-op off Windows */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static _Thread_local char g_error[192];

const char *pqw_error(void) { return g_error[0] ? g_error : "parquet writer: unknown failure"; }

static int fail(const char *why) {
    snprintf(g_error, sizeof g_error, "%s", why);
    return -1;
}

/* ------------------------------------------------------------------------- */
/* A growable byte buffer                                                      */
/* ------------------------------------------------------------------------- */

typedef struct {
    char  *p;
    size_t n, cap;
    int    bad;
} Buf;

static void buf_free(Buf *b) { free(b->p); memset(b, 0, sizeof *b); }

static int buf_room(Buf *b, size_t more) {
    if (b->bad) return -1;
    if (b->n + more <= b->cap) return 0;
    size_t want = b->cap ? b->cap : 4096;
    while (want < b->n + more) {
        if (want > (size_t)-1 / 2) { b->bad = 1; return -1; }
        want *= 2;
    }
    char *bigger = realloc(b->p, want);
    if (!bigger) { b->bad = 1; return -1; }
    b->p = bigger;
    b->cap = want;
    return 0;
}

static void buf_put(Buf *b, const void *src, size_t n) {
    if (buf_room(b, n) != 0) return;
    memcpy(b->p + b->n, src, n);
    b->n += n;
}

static void buf_byte(Buf *b, unsigned char c) {
    if (buf_room(b, 1) != 0) return;
    b->p[b->n++] = (char)c;
}

/* ------------------------------------------------------------------------- */
/* Thrift compact protocol                                                     */
/*                                                                             */
/* The mirror of the reader's Thrift. A field header carries a delta from the   */
/* last field id in the same struct, so nesting has to save and restore that    */
/* delta -- which is what enc_nest/enc_unnest do.                               */
/* ------------------------------------------------------------------------- */

enum { T_I32 = 5, T_I64 = 6, T_BINARY = 8, T_LIST = 9, T_STRUCT = 12 };

typedef struct {
    Buf *out;
    int  last;
} Enc;

static void put_varint(Buf *b, uint64_t v) {
    while (v >= 0x80) { buf_byte(b, (unsigned char)(v | 0x80)); v >>= 7; }
    buf_byte(b, (unsigned char)v);
}

static uint64_t zigzag(int64_t v) { return ((uint64_t)v << 1) ^ (uint64_t)(v >> 63); }

static void enc_field(Enc *e, int id, int type) {
    int delta = id - e->last;
    if (delta > 0 && delta <= 15) {
        buf_byte(e->out, (unsigned char)((delta << 4) | type));
    } else {
        buf_byte(e->out, (unsigned char)type);
        put_varint(e->out, zigzag(id));
    }
    e->last = id;
}

static void enc_stop(Enc *e) { buf_byte(e->out, 0); }
static void enc_i32(Enc *e, int id, int32_t v) { enc_field(e, id, T_I32); put_varint(e->out, zigzag(v)); }
static void enc_i64(Enc *e, int id, int64_t v) { enc_field(e, id, T_I64); put_varint(e->out, zigzag(v)); }

static void enc_str(Enc *e, int id, const char *s, size_t n) {
    enc_field(e, id, T_BINARY);
    put_varint(e->out, n);
    buf_put(e->out, s, n);
}

static void enc_list_header(Enc *e, size_t count, int elem) {
    if (count < 15) buf_byte(e->out, (unsigned char)((count << 4) | elem));
    else { buf_byte(e->out, (unsigned char)(0xF0 | elem)); put_varint(e->out, count); }
}

/* A list of i32s, which is what every enum list in this metadata is. */
static void enc_enums(Enc *e, int id, const int *v, size_t n) {
    enc_field(e, id, T_LIST);
    enc_list_header(e, n, T_I32);
    for (size_t i = 0; i < n; i++) put_varint(e->out, zigzag(v[i]));
}

static void enc_strings(Enc *e, int id, char *const *v, size_t n) {
    enc_field(e, id, T_LIST);
    enc_list_header(e, n, T_BINARY);
    for (size_t i = 0; i < n; i++) {
        size_t len = strlen(v[i]);
        put_varint(e->out, len);
        buf_put(e->out, v[i], len);
    }
}

/* Opens a list of structs whose encoded bodies the caller appends itself. */
static void enc_struct_list(Enc *e, int id, size_t count) {
    enc_field(e, id, T_LIST);
    enc_list_header(e, count, T_STRUCT);
}

/* Enters a nested struct: field ids inside start from zero again. */
static int enc_nest(Enc *e, int id) {
    enc_field(e, id, T_STRUCT);
    int saved = e->last;
    e->last = 0;
    return saved;
}

static void enc_unnest(Enc *e, int saved) { enc_stop(e); e->last = saved; }

/* ------------------------------------------------------------------------- */
/* RLE / bit-packed hybrid                                                     */
/* ------------------------------------------------------------------------- */

typedef struct { Buf *out; uint64_t acc; int bits; } BitPack;

static void pack_put(BitPack *b, uint32_t v, int width) {
    b->acc |= (uint64_t)v << b->bits;
    b->bits += width;
    while (b->bits >= 8) {
        buf_byte(b->out, (unsigned char)(b->acc & 0xFF));
        b->acc >>= 8;
        b->bits -= 8;
    }
}

static void pack_flush(BitPack *b) {
    if (b->bits > 0) { buf_byte(b->out, (unsigned char)(b->acc & 0xFF)); b->acc = 0; b->bits = 0; }
}

static void rle_hybrid(Buf *out, const int32_t *v, size_t n, int width) {
    const size_t bytes = (size_t)((width + 7) / 8);
    size_t i = 0;
    while (i < n) {
        size_t j = i;
        while (j < n && v[j] == v[i]) j++;
        if (j - i >= 8) {
            put_varint(out, ((uint64_t)(j - i) << 1) | 0);
            for (size_t k = 0; k < bytes; k++)
                buf_byte(out, (unsigned char)(((uint32_t)v[i] >> (8 * k)) & 0xFF));
            i = j;
            continue;
        }
        /*
         * Literals up to the next run long enough to be worth encoding -- but a
         * bit-packed run holds a whole number of groups of eight, and a short
         * one anywhere but at the very end would leave the reader taking its
         * padding for data and every value after it shifted. So the run only
         * ends on a group boundary: if a repeat starts off one, just enough of
         * it is taken as literals to land on the next.
         */
        size_t k = i;
        while (k < n) {
            size_t m = k;
            while (m < n && v[m] == v[k]) m++;
            if (m - k >= 8) {
                size_t pad = (8 - (k - i) % 8) % 8;
                if (pad == 0) break;
                k = k + pad < n ? k + pad : n;
                break;
            }
            k = m;
        }
        const size_t count = k - i;
        const size_t groups = (count + 7) / 8;
        put_varint(out, ((uint64_t)groups << 1) | 1);
        BitPack pack = { out, 0, 0 };
        for (size_t x = 0; x < groups * 8; x++)
            pack_put(&pack, x < count ? (uint32_t)v[i + x] : 0, width);
        pack_flush(&pack);
        i = k;
    }
}

/*
 * The bit width a dictionary index needs. One even for a single distinct value,
 * because a width of zero writes no indices at all and readers differ on what
 * that means -- and because every other generator here writes one.
 */
static int width_for(size_t distinct) {
    int w = 1;
    while (distinct > ((size_t)1 << w)) w++;
    return w < 32 ? w : 32;
}

/* ------------------------------------------------------------------------- */
/* Columns, row groups, and the file                                           */
/* ------------------------------------------------------------------------- */

static uint64_t value_hash(const char *p, size_t n) {
    uint64_t h = UINT64_C(0xcbf29ce484222325);
    for (size_t i = 0; i < n; i++) h = (h ^ (unsigned char)p[i]) * UINT64_C(0x100000001b3);
    return h ^ (h >> 29);
}

/* parquet.thrift constants, named as the reader names them. */
enum { PT_BYTE_ARRAY = 6 };
enum { PE_PLAIN = 0, PE_PLAIN_DICTIONARY = 2, PE_RLE = 3, PE_RLE_DICTIONARY = 8 };
enum { PC_UNCOMPRESSED = 0 };
enum { PG_DATA = 0, PG_DICTIONARY = 2 };
enum { PR_REQUIRED = 0, PR_OPTIONAL = 1 };
enum { PL_UTF8 = 0 };

/* One column's values for the current row group: bytes in one arena, with an
 * offset and a length per row so nothing is copied twice. */
typedef struct {
    Buf      bytes;
    size_t  *off, *len;
    char    *isnull;
    size_t   n, cap;
} Column;

struct PqWriter {
    int      fd;
    char   **names;
    size_t   columns;
    size_t   group_rows, dict_limit;
    unsigned ways;                /* parts to split a group's columns into */
    Column  *col;
    size_t   held;                /* rows in the current group */
    int64_t  rows;                /* rows in the file so far */
    int64_t  at;                  /* the file offset written to so far */
    Buf     *groups;              /* one encoded RowGroup struct each */
    size_t   group_count, group_cap;
    int      bad;
};

static int col_push(Column *c, const PqValue *v) {
    if (c->n == c->cap) {
        size_t want = c->cap ? c->cap * 2 : 4096;
        size_t *o = realloc(c->off, want * sizeof *o);
        if (!o) return -1;
        c->off = o;
        size_t *l = realloc(c->len, want * sizeof *l);
        if (!l) return -1;
        c->len = l;
        char *k = realloc(c->isnull, want);
        if (!k) return -1;
        c->isnull = k;
        c->cap = want;
    }
    c->off[c->n] = c->bytes.n;
    c->len[c->n] = v->null ? 0 : v->n;
    c->isnull[c->n] = v->null ? 1 : 0;
    if (!v->null && v->n) buf_put(&c->bytes, v->p, v->n);
    if (c->bytes.bad) return -1;
    c->n++;
    return 0;
}

static void col_reset(Column *c) {
    c->bytes.n = 0;
    c->n = 0;
}

static int write_all(PqWriter *w, const char *p, size_t n) {
    while (n) {
        ssize_t got = write(w->fd, p, n);
        if (got <= 0) return fail("cannot write the parquet file");
        p += got;
        n -= (size_t)got;
        w->at += got;
    }
    return 0;
}

/* PLAIN byte arrays: a four-byte little-endian length, then the bytes. */
static void plain_put(Buf *b, const char *p, size_t n) {
    uint32_t len = (uint32_t)n;
    buf_put(b, &len, 4);
    buf_put(b, p, n);
}

/* A page is a header and a body; the sizes in the header describe the body. */
static void page_header(Buf *out, int type, int32_t raw, int32_t values, int encoding) {
    Enc e = { out, 0 };
    enc_i32(&e, 1, type);
    enc_i32(&e, 2, raw);
    enc_i32(&e, 3, raw);          /* uncompressed: the two sizes are the same */
    if (type == PG_DATA) {
        int saved = enc_nest(&e, 5);
        enc_i32(&e, 1, values);
        enc_i32(&e, 2, encoding);
        enc_i32(&e, 3, PE_RLE);   /* definition levels */
        enc_i32(&e, 4, PE_RLE);   /* repetition levels */
        enc_unnest(&e, saved);
    } else {
        int saved = enc_nest(&e, 7);
        enc_i32(&e, 1, values);
        enc_i32(&e, 2, encoding);
        enc_unnest(&e, saved);
    }
    enc_stop(&e);
}

/*
 * One part's share of a row group's columns.
 *
 * A column depends on nothing outside itself -- its dictionary, its definition
 * levels and its pages are its own -- so the columns of a group are the natural
 * unit of parallel work here, and there are twenty of them against four cores.
 * Parts stride rather than take blocks, which balances them without a queue.
 *
 * The scratch is per part because it used to be per group: one definition-level
 * array, one index array and one dictionary table served every column in turn,
 * which is exactly what cannot be shared once the turns overlap.
 */
typedef struct {
    PqWriter    *w;
    size_t       n;
    Buf         *bytes;
    int64_t     *dict_at, *data_at;
    size_t       seen_cap;
    unsigned     ways;
    const char **why;      /* one slot per part; NULL when it went fine */
} ColParts;

static void column_part(void *vctx, unsigned part) {
    ColParts *g = vctx;
    PqWriter *w = g->w;
    const size_t n = g->n;
    const size_t seen_cap = g->seen_cap;
    int64_t *dict_at = g->dict_at, *data_at = g->data_at;

    int32_t *defs = malloc(n * sizeof *defs);
    int32_t *idx  = malloc(n * sizeof *idx);
    size_t  *dict = malloc(n * sizeof *dict);   /* row indices of distinct values */
    int32_t *seen = malloc(seen_cap * sizeof *seen);
    if (!defs || !idx || !dict || !seen) {
        g->why[part] = "out of memory";
    } else {
        for (size_t c = part; c < w->columns; c += g->ways) {
            Column *col = &w->col[c];
            Buf *out = &g->bytes[c];
            Buf body = {0}, levels = {0}, header = {0};
            dict_at[c] = -1;
            /* Definition levels: one per row, 1 present and 0 null, RLE, with a
             * four-byte length in front. The same however the values are encoded. */
            for (size_t i = 0; i < n; i++) defs[i] = col->isnull[i] ? 0 : 1;
            rle_hybrid(&levels, defs, n, 1);

            /*
             * Distinct values in first-seen order, abandoned once the column is too
             * varied for a dictionary to be worth it.
             *
             * Through an open-addressed table, not a scan of what has been seen. A
             * scan is O(rows x distinct), which at a hundred and twenty thousand
             * rows a group and a dictionary of eight thousand is a billion
             * comparisons per column -- it made this generator fifty-eight times
             * slower than the C++ one it is meant to match, all of it in here.
             */
            size_t distinct = 0;
            int use_dict = 1;
            memset(seen, 0xFF, seen_cap * sizeof *seen);      /* -1 is empty */
            for (size_t i = 0; i < n && use_dict; i++) {
                if (col->isnull[i]) { idx[i] = 0; continue; }
                const char *v = col->bytes.p + col->off[i];
                const size_t vn = col->len[i];
                size_t at = value_hash(v, vn) & (seen_cap - 1);
                int32_t found = -1;
                for (;;) {
                    const int32_t k = seen[at];
                    if (k < 0) break;
                    const size_t o = dict[k];
                    if (col->len[o] == vn && memcmp(col->bytes.p + col->off[o], v, vn) == 0) {
                        found = k;
                        break;
                    }
                    at = (at + 1) & (seen_cap - 1);
                }
                if (found >= 0) { idx[i] = found; continue; }
                if (distinct >= w->dict_limit) { use_dict = 0; break; }
                dict[distinct] = i;
                seen[at] = (int32_t)distinct;
                idx[i] = (int32_t)distinct;
                distinct++;
            }

            if (use_dict) {
                /* The dictionary page, then the indices as a data page. */
                body.n = 0;
                for (size_t k = 0; k < distinct; k++)
                    plain_put(&body, col->bytes.p + col->off[dict[k]], col->len[dict[k]]);
                page_header(&header, PG_DICTIONARY, (int32_t)body.n, (int32_t)distinct, PE_PLAIN);
                dict_at[c] = (int64_t)out->n;
                buf_put(out, header.p, header.n);
                buf_put(out, body.p, body.n);

                const int width = width_for(distinct ? distinct : 1);
                body.n = 0;
                uint32_t dl = (uint32_t)levels.n;
                buf_put(&body, &dl, 4);
                buf_put(&body, levels.p, levels.n);
                buf_byte(&body, (unsigned char)width);
                {
                    /* Only the present rows carry an index. */
                    size_t m = 0;
                    for (size_t i = 0; i < n; i++)
                        if (!col->isnull[i]) idx[m++] = idx[i];
                    if (m) rle_hybrid(&body, idx, m, width);
                }
                header.n = 0;
                page_header(&header, PG_DATA, (int32_t)body.n, (int32_t)n, PE_RLE_DICTIONARY);
                data_at[c] = (int64_t)out->n;
                buf_put(out, header.p, header.n);
                buf_put(out, body.p, body.n);
            } else {
                body.n = 0;
                uint32_t dl = (uint32_t)levels.n;
                buf_put(&body, &dl, 4);
                buf_put(&body, levels.p, levels.n);
                for (size_t i = 0; i < n; i++)
                    if (!col->isnull[i]) plain_put(&body, col->bytes.p + col->off[i], col->len[i]);
                page_header(&header, PG_DATA, (int32_t)body.n, (int32_t)n, PE_PLAIN);
                data_at[c] = (int64_t)out->n;
                buf_put(out, header.p, header.n);
                buf_put(out, body.p, body.n);
            }
            int oops = body.bad || levels.bad || header.bad || out->bad;
            buf_free(&body); buf_free(&levels); buf_free(&header);
            if (oops) { g->why[part] = "out of memory"; break; }
        }
    }
    free(defs); free(idx); free(dict); free(seen);
}

/*
 * Writes every column of the current row group, then records the group.
 *
 * A column depends on nothing outside itself -- its dictionary and its
 * definition levels are its own -- so each is built into its own buffer and the
 * buffers are written in column order, with the file offsets fixed up once every
 * size is known.
 */
static int flush_group(PqWriter *w) {
    if (w->held == 0) return 0;
    const size_t n = w->held;
    int status = -1;

    Buf     *bytes = calloc(w->columns, sizeof *bytes);
    int64_t *dict_at = malloc(w->columns * sizeof *dict_at);
    int64_t *data_at = malloc(w->columns * sizeof *data_at);
    /* Sized to at least twice what can go in it, so probes stay short and it
     * can never fill. One per part, not one per group: the parts run at once. */
    size_t   seen_cap = 16;
    while (seen_cap < (w->dict_limit < n ? w->dict_limit : n) * 2 + 16) seen_cap <<= 1;
    Buf      group = {0};
    if (!bytes || !dict_at || !data_at) {
        fail("out of memory");
        goto done;
    }

    {
        ColParts cp = { w, n, bytes, dict_at, data_at, seen_cap, w->ways, NULL };
        cp.why = calloc(w->ways, sizeof *cp.why);
        if (!cp.why) { fail("out of memory"); goto done; }
        run_parts(column_part, &cp, w->ways);
        const char *why = NULL;
        for (unsigned q = 0; q < w->ways; q++) if (cp.why[q]) why = cp.why[q];
        free(cp.why);
        /* `fail` writes a thread-local buffer, so a part's message never
         * reaches here on its own; parts hand back the reason and it is set
         * once, on this thread. */
        if (why) { fail(why); goto done; }
    }

    /* Where each column's bytes land is known once every buffer's size is. */
    int64_t base = w->at, total = 0;
    for (size_t c = 0; c < w->columns; c++) total += (int64_t)bytes[c].n;

    {
        Enc e = { &group, 0 };
        enc_struct_list(&e, 1, w->columns);
        int64_t at = base;
        for (size_t c = 0; c < w->columns; c++) {
            Buf chunk = {0};
            Enc k = { &chunk, 0 };
            const int64_t d_off = at + (dict_at[c] >= 0 ? dict_at[c] : 0);
            const int64_t p_off = at + data_at[c];
            enc_i64(&k, 2, dict_at[c] >= 0 ? d_off : p_off);       /* file_offset */
            int saved = enc_nest(&k, 3);                            /* meta_data */
            enc_i32(&k, 1, PT_BYTE_ARRAY);
            {
                const int with_dict[] = { PE_PLAIN, PE_RLE, PE_RLE_DICTIONARY };
                const int plain_only[] = { PE_PLAIN, PE_RLE };
                if (dict_at[c] >= 0) enc_enums(&k, 2, with_dict, 3);
                else                 enc_enums(&k, 2, plain_only, 2);
            }
            enc_strings(&k, 3, &w->names[c], 1);
            enc_i32(&k, 4, PC_UNCOMPRESSED);
            enc_i64(&k, 5, (int64_t)n);
            enc_i64(&k, 6, (int64_t)bytes[c].n);
            enc_i64(&k, 7, (int64_t)bytes[c].n);
            enc_i64(&k, 9, p_off);
            if (dict_at[c] >= 0) enc_i64(&k, 11, d_off);
            enc_unnest(&k, saved);
            enc_stop(&k);
            buf_put(&group, chunk.p, chunk.n);
            int oops = chunk.bad || group.bad;
            buf_free(&chunk);
            if (oops) { fail("out of memory"); goto done; }
            at += (int64_t)bytes[c].n;
        }
        enc_i64(&e, 2, total);
        enc_i64(&e, 3, (int64_t)n);
        enc_stop(&e);
    }

    for (size_t c = 0; c < w->columns; c++)
        if (write_all(w, bytes[c].p, bytes[c].n) != 0) goto done;

    if (w->group_count == w->group_cap) {
        size_t want = w->group_cap ? w->group_cap * 2 : 16;
        Buf *g = realloc(w->groups, want * sizeof *g);
        if (!g) { fail("out of memory"); goto done; }
        w->groups = g;
        w->group_cap = want;
    }
    w->groups[w->group_count++] = group;
    group.p = NULL;

    for (size_t c = 0; c < w->columns; c++) col_reset(&w->col[c]);
    w->held = 0;
    status = 0;

done:
    for (size_t c = 0; bytes && c < w->columns; c++) buf_free(&bytes[c]);
    free(bytes); free(dict_at); free(data_at);
    buf_free(&group);
    return status;
}

PqWriter *pqw_open(const char *path, char *const *names, size_t columns,
                   size_t row_group_rows, size_t dict_limit) {
    PqWriter *w = calloc(1, sizeof *w);
    if (!w) { fail("out of memory"); return NULL; }
    w->fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, 0644);
    w->columns = columns;
    w->group_rows = row_group_rows ? row_group_rows : 1;
    w->dict_limit = dict_limit;
    /* Never more parts than columns: a part with nothing to do is a thread
     * created, joined, and paid for. */
    w->ways = cpu_count();
    if (w->ways > columns) w->ways = (unsigned)columns;
    if (w->ways < 1) w->ways = 1;
    w->names = malloc(columns * sizeof *w->names);
    w->col = calloc(columns, sizeof *w->col);
    if (w->fd < 0 || !w->names || !w->col) {
        fail(w->fd < 0 ? "cannot create the parquet file" : "out of memory");
        if (w->fd >= 0) close(w->fd);
        free(w->names); free(w->col); free(w);
        return NULL;
    }
    for (size_t c = 0; c < columns; c++) w->names[c] = names[c];
    if (write_all(w, "PAR1", 4) != 0) { pqw_close(w); return NULL; }
    return w;
}

int pqw_row(PqWriter *w, const PqValue *cells) {
    if (w->bad) return -1;
    for (size_t c = 0; c < w->columns; c++)
        if (col_push(&w->col[c], &cells[c]) != 0) { w->bad = 1; return fail("out of memory"); }
    w->held++;
    w->rows++;
    if (w->held >= w->group_rows && flush_group(w) != 0) { w->bad = 1; return -1; }
    return 0;
}

int pqw_close(PqWriter *w) {
    if (!w) return -1;
    int status = w->bad ? -1 : 0;
    if (status == 0) status = flush_group(w);

    if (status == 0) {
        Buf meta = {0};
        Enc e = { &meta, 0 };
        enc_i32(&e, 1, 1);   /* version */

        /* A root group with one child per column, all of them optional UTF8
         * byte arrays -- the shape every reader here agrees on. */
        enc_struct_list(&e, 2, w->columns + 1);
        {
            Buf root = {0};
            Enc r = { &root, 0 };
            enc_i32(&r, 3, PR_REQUIRED);
            enc_str(&r, 4, "csvdiff", 7);
            enc_i32(&r, 5, (int32_t)w->columns);
            enc_stop(&r);
            buf_put(&meta, root.p, root.n);
            buf_free(&root);
        }
        for (size_t c = 0; c < w->columns; c++) {
            Buf leaf = {0};
            Enc l = { &leaf, 0 };
            enc_i32(&l, 1, PT_BYTE_ARRAY);
            enc_i32(&l, 3, PR_OPTIONAL);
            enc_str(&l, 4, w->names[c], strlen(w->names[c]));
            enc_i32(&l, 6, PL_UTF8);
            enc_stop(&l);
            buf_put(&meta, leaf.p, leaf.n);
            buf_free(&leaf);
        }
        enc_i64(&e, 3, w->rows);
        enc_struct_list(&e, 4, w->group_count);
        for (size_t g = 0; g < w->group_count; g++) buf_put(&meta, w->groups[g].p, w->groups[g].n);
        enc_str(&e, 6, "csvdiff gen-data", 16);
        enc_stop(&e);

        if (meta.bad) status = fail("out of memory");
        else {
            uint32_t len = (uint32_t)meta.n;
            buf_put(&meta, &len, 4);
            buf_put(&meta, "PAR1", 4);
            status = write_all(w, meta.p, meta.n);
        }
        buf_free(&meta);
    }

    if (w->fd >= 0) close(w->fd);
    for (size_t c = 0; c < w->columns; c++) {
        buf_free(&w->col[c].bytes);
        free(w->col[c].off); free(w->col[c].len); free(w->col[c].isnull);
    }
    for (size_t g = 0; g < w->group_count; g++) buf_free(&w->groups[g]);
    free(w->groups); free(w->col); free(w->names);
    free(w);
    return status;
}
