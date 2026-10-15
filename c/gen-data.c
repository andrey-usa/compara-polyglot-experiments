/*
 * Writes the same deterministic pair of files as the other generators, byte for
 * byte, as CSV, newline-delimited JSON, or uncompressed Parquet.
 *
 * The recipe is identical because it has to be: a benchmark number from one
 * generator is only comparable with a number from another if the bytes agree.
 * Every row is a pure function of its index -- the drift buckets come from a
 * hash of the row number, and money is carried in integer cents so no
 * language's rounding rule can enter into it.
 *
 * This exists so the C port can make its own fixtures and its own benchmark
 * inputs. Before it, every check of the Parquet path needed the C++ generator
 * built first, which is a whole second toolchain standing between a change and
 * knowing whether it broke anything.
 *
 *   cd c && make gen-data
 *   ./gen-data --rows 10m --out-dir data --prefix 10m
 *   ./gen-data --rows 10m --out-dir data --format parquet
 */
#define _GNU_SOURCE

#include "parallel.h"
#include "pqwrite.h"

#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include "win32.h"   /* make_dir, and O_BINARY off Windows */

#define COLUMNS 20        /* the standard recipe */
#define MAX_COLUMNS 256   /* --columns: the standard twenty and up to 236 more */
/* The most one row can take in any format, at MAX_COLUMNS. */
#define ROW_BYTES_MAX 16384

static char *const kNames[COLUMNS] = {
    "account_id", "txn_id", "posting_date", "value_date", "currency", "amount", "fee",
    "balance", "status", "channel", "region", "branch_code", "product_code", "counterparty",
    "quantity", "rate", "category", "risk_flag", "note", "updated_at"
};

static const char *const kStatus[] = { "posted", "pending", "settled", "reversed" };
static const char *const kChannel[] = { "branch", "online", "mobile", "atm", "wire" };
static const char *const kRegion[] = { "EMEA", "NA", "APAC", "LATAM" };
static const char *const kCurrency[] = { "USD", "EUR", "GBP", "JPY" };
static const char *const kCategory[] = { "retail", "corporate", "treasury", "cards", "loans" };

/* Drift buckets, against a 0..9999 hash bucket per row. */
enum { CHG_STATUS = 300, CHG_AMOUNT = 150, CHG_BALANCE = 150, CHG_VALUE_DATE = 30 };
/* With --columns, one extra column changes in another 3% of rows -- buckets no
 * standard column uses, so the wide columns carry differences of their own. */
enum { CHG_EXTRA_LO = 600, CHG_EXTRA_HI = 900 };
#define REMOVED_MOD 1000
#define ADDED_RATIO 1000
#define DUP_MOD     10000

/* 240 dates from 2026-01-01. The other generators get these from a date
 * library; this one only needs the same strings out. */
static char g_days[240][11];

/*
 * The columns this run writes: the standard twenty, then `--columns` minus
 * twenty more, named c021, c022 and so on. The extra ones exist to measure a
 * wide table at about the bytes of a long one -- 2M rows by 100 columns against
 * 10M by 20 -- so their values are sized for that rather than for realism:
 * eight digits, where a standard cell averages 8.2 characters. At the default
 * of twenty nothing here changes a byte of the output.
 */
static int g_columns = COLUMNS;
static char *g_names[MAX_COLUMNS];
static char g_extra_names[MAX_COLUMNS][8];

/* Column-name lengths, computed once. Writing a JSON row calls for all twenty
 * of them, so taking strlen each time is forty million calls on a two-million
 * row file. */
static size_t g_name_len[MAX_COLUMNS];

static void build_days(void) {
    for (int i = 0; i < g_columns; i++) {
        if (i < COLUMNS) g_names[i] = kNames[i];
        else {
            snprintf(g_extra_names[i], sizeof g_extra_names[i], "c%03d", i + 1);
            g_names[i] = g_extra_names[i];
        }
        g_name_len[i] = strlen(g_names[i]);
    }
    static const int len[] = { 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31 };
    int month = 0, day = 1;
    for (int i = 0; i < 240; i++) {
        char *p = g_days[i];
        memcpy(p, "2026-", 5);
        p += 5;
        *p++ = (char)('0' + (month + 1) / 10);
        *p++ = (char)('0' + (month + 1) % 10);
        *p++ = '-';
        *p++ = (char)('0' + day / 10);
        *p++ = (char)('0' + day % 10);
        *p = '\0';
        if (++day > len[month]) { day = 1; month++; }
    }
}

/* splitmix-style mix, matching the other implementations bit for bit. */
static uint64_t mix(int64_t i, int64_t salt, int64_t seed) {
    uint64_t x = (uint64_t)(i * 31 + salt + seed);
    x = (x ^ (x >> 30)) * UINT64_C(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)) * UINT64_C(0x94D049BB133111EB);
    return x ^ (x >> 31);
}

static int mod_of(int64_t i, int64_t salt, int64_t seed, int m) {
    return (int)(mix(i, salt, seed) % (uint64_t)m);
}

static char *put(char *p, const char *s) {
    size_t n = strlen(s);
    memcpy(p, s, n);
    return p + n;
}

static char *put_int(char *p, int64_t v) {
    char tmp[24];
    int n = 0;
    if (v == 0) tmp[n++] = '0';
    bool neg = v < 0;
    uint64_t u = neg ? (uint64_t)(-v) : (uint64_t)v;
    while (u) { tmp[n++] = (char)('0' + u % 10); u /= 10; }
    if (neg) *p++ = '-';
    while (n) *p++ = tmp[--n];
    return p;
}

/* Left-pads to `width` with zeroes, which is what the other generators do with
 * a format string. */
static char *put_pad(char *p, int64_t v, int width) {
    char tmp[24];
    int n = 0;
    if (v == 0) tmp[n++] = '0';
    uint64_t u = (uint64_t)v;
    while (u) { tmp[n++] = (char)('0' + u % 10); u /= 10; }
    for (int i = n; i < width; i++) *p++ = '0';
    while (n) *p++ = tmp[--n];
    return p;
}

/* An amount held in cents, written as a two-decimal number. */
static char *put_money(char *p, int64_t cents) {
    if (cents < 0) { *p++ = '-'; cents = -cents; }
    p = put_int(p, cents / 100);
    *p++ = '.';
    return put_pad(p, cents % 100, 2);
}

/*
 * One row of one side, as its twenty fields rather than as a line of text.
 *
 * The CSV, JSON and Parquet writers all take these, so the three formats come
 * from one recipe and cannot drift. An empty field is a value the row does not
 * have: CSV has no other way to say it, and Parquet writes it as a null, which
 * is what every reader here treats an empty CSV field as anyway.
 */
typedef struct {
    char        buf[ROW_BYTES_MAX / 2];
    const char *f[MAX_COLUMNS];
    size_t      n[MAX_COLUMNS];
} Row;

static void fields(Row *out, int64_t i, bool b, int64_t seed) {
    const int bucket = mod_of(i, 0, seed, 10000);
    int64_t amount_cents = mod_of(i, 21, seed, 900000000) - 100000000;
    int64_t balance_cents = mod_of(i, 31, seed, 2000000000);
    const char *st = kStatus[mod_of(i, 11, seed, 4)];
    const char *value_date = g_days[mod_of(i, 41, seed, 240)];

    if (b) {
        if (bucket < CHG_STATUS) {
            st = kStatus[(mod_of(i, 11, seed, 4) + 1) % 4];
        } else if (bucket < CHG_STATUS + CHG_AMOUNT) {
            amount_cents += 1234;
        } else if (bucket < CHG_STATUS + CHG_AMOUNT + CHG_BALANCE) {
            balance_cents = (balance_cents * 101 + 50) / 100;   /* +1%, half up, in cents */
        }
        if (bucket < CHG_VALUE_DATE) value_date = "";
    }

    char *p = out->buf;
    int c = 0;
#define DONE(end) do { char *e_ = (end); out->f[c] = p; out->n[c] = (size_t)(e_ - p); c++; p = e_; } while (0)

    DONE(put_pad(put(p, "ACC-"), (i * 7919) % 250000, 8));
    DONE(put_pad(put(p, "TXN-"), i, 11));
    DONE(put(p, g_days[mod_of(i, 1, seed, 240)]));
    DONE(put(p, value_date));
    DONE(put(p, kCurrency[mod_of(i, 51, seed, 4)]));
    DONE(put_money(p, amount_cents));
    DONE(put_money(p, mod_of(i, 61, seed, 5000)));
    DONE(put_money(p, balance_cents));
    DONE(put(p, st));
    DONE(put(p, kChannel[mod_of(i, 71, seed, 5)]));
    DONE(put(p, kRegion[mod_of(i, 81, seed, 4)]));
    DONE(put_pad(put(p, "BR"), mod_of(i, 91, seed, 900) + 100, 4));
    DONE(put_pad(put(p, "P"), mod_of(i, 101, seed, 5000), 5));
    DONE(put_pad(put(p, "CP-"), mod_of(i, 111, seed, 90000), 6));
    DONE(put_int(p, mod_of(i, 121, seed, 500) + 1));
    DONE(put_pad(put(p, "0."), mod_of(i, 131, seed, 1200), 4));
    DONE(put(p, kCategory[mod_of(i, 141, seed, 5)]));
    *p = mod_of(i, 151, seed, 20) == 0 ? 'Y' : 'N';
    DONE(p + 1);
    {
        char *q = put(p, "batch ");
        q = put_int(q, i % 997 + 1);
        q = put(q, " line ");
        DONE(put_int(q, i % 53 + 1));
    }
    DONE(put(p, b ? "2026-09-01 02:15:00" : "2026-08-01 02:15:00"));
    if (g_columns > COLUMNS) {
        const int changed = b && bucket >= CHG_EXTRA_LO && bucket < CHG_EXTRA_HI
            ? COLUMNS + mod_of(i, 171, seed, g_columns - COLUMNS) : -1;
        for (int x = COLUMNS; x < g_columns; x++) {
            int64_t v = mod_of(i, 200 + x, seed, 100000000);
            if (x == changed) v = (v + 1) % 100000000;
            DONE(put_pad(p, v, 8));
        }
    }
#undef DONE
}

/*
 * Which row indices each side holds.
 *
 * A has every row plus a few duplicates; B drops one row in a thousand, repeats
 * some of the first half, and gains a tail of rows A never had. That is what
 * puts added, removed and duplicate keys into every comparison.
 */
/*
 * Rows `lo` to `hi` of the main sequence, in the order they appear in the file.
 * Split out from EACH_ROW so that a thread can be given a range of it: whether
 * a row is emitted depends on nothing but its own index, so any contiguous
 * range can be rendered without having seen the rows before it.
 */
#define RANGE_ROWS(b, rows, lo, hi, EMIT)                                           \
    do {                                                                            \
        for (int64_t i_ = (lo); i_ < (hi); i_++) {                                  \
            if (!(b)) { EMIT(i_); continue; }                                       \
            if (i_ % REMOVED_MOD != 7) EMIT(i_);                                    \
            if (i_ % DUP_MOD == 3 && i_ < (rows) / 2) EMIT(i_);                     \
        }                                                                           \
    } while (0)

/* The rows appended after the main sequence: A's repeats, B's tail of new ones. */
#define TAIL_ROWS(b, rows, EMIT)                                                    \
    do {                                                                            \
        const int64_t dup_extra_ = (rows) / DUP_MOD > 1 ? (rows) / DUP_MOD : 1;      \
        const int64_t added_ = (rows) / ADDED_RATIO > 1 ? (rows) / ADDED_RATIO : 1;  \
        if (!(b)) { for (int64_t i_ = 0; i_ < dup_extra_; i_++) EMIT(i_); }          \
        else { for (int64_t i_ = (rows); i_ < (rows) + added_; i_++) EMIT(i_); }     \
    } while (0)

#define EACH_ROW(b, rows, EMIT)                                                     \
    do {                                                                            \
        RANGE_ROWS(b, rows, 0, (rows), EMIT);                                       \
        TAIL_ROWS(b, rows, EMIT);                                                   \
    } while (0)

/* A JSON string body. None of the generated values need escaping, but a
 * generator that only happens to be correct for its own data is a trap for
 * whoever changes the recipe next. */
static char *put_json(char *p, const char *v, size_t n) {
    for (size_t i = 0; i < n; i++) {
        char c = v[i];
        switch (c) {
            case '"':  *p++ = '\\'; *p++ = '"'; break;
            case '\\': *p++ = '\\'; *p++ = '\\'; break;
            case '\n': *p++ = '\\'; *p++ = 'n'; break;
            case '\r': *p++ = '\\'; *p++ = 'r'; break;
            case '\t': *p++ = '\\'; *p++ = 't'; break;
            default:
                if ((unsigned char)c < 0x20) p += sprintf(p, "\\u%04x", c);
                else *p++ = c;
        }
    }
    return p;
}

/* ------------------------------------------------------------------------- */
/* The three writers                                                           */
/* ------------------------------------------------------------------------- */

/*
 * A megabyte of output at a time, written with one `fwrite` per buffer rather
 * than one per row. A row is a couple of hundred bytes, so the per-call cost --
 * the lock, the branch on the stream's state, the memcpy into libc's own buffer
 * -- was being paid two million times for work that is a memcpy either way.
 */
static int put_csv_header(FILE *fh) {
    for (int c = 0; c < g_columns; c++)
        if ((c && fputc(',', fh) == EOF) || fputs(g_names[c], fh) < 0) return -1;
    return fputc('\n', fh) == EOF ? -1 : 0;
}

/* One row's bytes. A row is bounded by ROW_BYTES_MAX, so a caller only has to
 * guarantee that much room. */
static char *render_csv(char *p, const Row *r) {
    for (int c = 0; c < g_columns; c++) {
        if (c) *p++ = ',';
        memcpy(p, r->f[c], r->n[c]);
        p += r->n[c];
    }
    *p++ = '\n';
    return p;
}

static char *render_json(char *p, const Row *r) {
    *p++ = '{';
    for (int c = 0; c < g_columns; c++) {
        if (c) *p++ = ',';
        *p++ = '"';
        memcpy(p, g_names[c], g_name_len[c]);
        p += g_name_len[c];
        *p++ = '"';
        *p++ = ':';
        if (r->n[c] == 0) { memcpy(p, "null", 4); p += 4; continue; }
        *p++ = '"';
        p = put_json(p, r->f[c], r->n[c]);
        *p++ = '"';
    }
    *p++ = '}';
    *p++ = '\n';
    return p;
}

/*
 * The text formats, on every core.
 *
 * A row is a pure function of its index and whether a row is emitted at all
 * depends on nothing but that index, so any contiguous range of the sequence
 * can be rendered by itself. Threads take ranges of one wave, each into its own
 * buffer; the buffers are written in wave order, so the bytes are the bytes a
 * single thread would have produced. Memory is bounded by the wave rather than
 * by the file, which is what makes this work at fifty million rows.
 */
typedef struct { char *p; size_t n, cap; } Blob;

static int blob_room(Blob *b, size_t more) {
    if (b->n + more <= b->cap) return 0;
    size_t next = b->cap ? b->cap : (size_t)1 << 20;
    while (next < b->n + more) next *= 2;
    char *q = realloc(b->p, next);
    if (!q) return -1;
    b->p = q;
    b->cap = next;
    return 0;
}

typedef struct {
    Blob    *blob;
    int64_t  lo, hi;      /* this wave's slice of the main sequence */
    int64_t  rows, seed;
    bool     b, json;
    unsigned ways;
    bool     oom;
} Wave;

/*
 * Renders one row into `out`, growing it. `b`, `seed` and the renderer are
 * taken as names rather than read out of the wave, because reading them per row
 * and calling through a branch cost more than the threading saved: the first
 * version of this did that and spent 3.5 CPU-seconds on the CSV the serial one
 * had written with 1.9.
 */
#define EMIT_ONE(out, b, seed, RENDER, i)                                      \
    do {                                                                       \
        if (blob_room((out), ROW_BYTES_MAX) != 0) { oom = true; break; }       \
        Row r_;                                                                \
        fields(&r_, (i), (b), (seed));                                         \
        (out)->n = (size_t)(RENDER((out)->p + (out)->n, &r_) - (out)->p);       \
    } while (0)

#define EMIT_CSV_ROW(i)  EMIT_ONE(out, b, seed, render_csv, (i))
#define EMIT_JSON_ROW(i) EMIT_ONE(out, b, seed, render_json, (i))

static void wave_part(void *vctx, unsigned part) {
    Wave *w = vctx;
    Blob *out = &w->blob[part];
    out->n = 0;
    const int64_t span = w->hi - w->lo;
    const int64_t lo = w->lo + span * part / w->ways;
    const int64_t hi = w->lo + span * (part + 1) / w->ways;
    /* In registers for the loop, not fields fetched once a row. */
    const bool b = w->b;
    const int64_t rows = w->rows, seed = w->seed;
    bool oom = false;
    if (w->json) RANGE_ROWS(b, rows, lo, hi, EMIT_JSON_ROW);
    else         RANGE_ROWS(b, rows, lo, hi, EMIT_CSV_ROW);
    if (oom) w->oom = true;
}

static int write_text(const char *path, bool b, int64_t rows, int64_t seed, bool json,
                      unsigned ways) {
    /* Rows per wave, across all parts. Bounded by the wave rather than the
     * file, so this is the same however many rows are being written. */
#ifndef WAVE_ROWS_SHIFT
#define WAVE_ROWS_SHIFT 13
#endif
    const int64_t WAVE_ROWS = (int64_t)1 << WAVE_ROWS_SHIFT;

    /* Failures come back as a negative errno rather than a bare -1: "write
     * failed" on a full disk and on a missing directory read identically, and
     * one of them cost a reader their first command. errno is captured at the
     * point of failure, because the free/fclose on the way out would clobber
     * it. */
    FILE *fh = fopen(path, "wb");
    if (!fh) return -errno;
    if (!json && put_csv_header(fh) != 0) { const int e = errno; fclose(fh); return -e; }

    Blob *blob = calloc(ways, sizeof *blob);
    if (!blob) { fclose(fh); return -ENOMEM; }
    Wave wave = { blob, 0, 0, rows, seed, b, json, ways, false };
    Wave *w = &wave;   /* a pointer, because EMIT_ROW is shared with the parts */
    int bad = 0, err = 0;

    for (int64_t at = 0; at < rows && !bad; at += WAVE_ROWS) {
        w->lo = at;
        w->hi = at + WAVE_ROWS < rows ? at + WAVE_ROWS : rows;
        run_parts(wave_part, w, ways);
        if (w->oom) { bad = 1; err = ENOMEM; break; }
        for (unsigned p = 0; p < ways; p++)
            if (blob[p].n && fwrite(blob[p].p, 1, blob[p].n, fh) != blob[p].n) {
                bad = 1; err = errno; break;
            }
    }

    if (!bad) {   /* the rows appended after the sequence, on this thread */
        Blob *out = &blob[0];
        out->n = 0;
        bool oom = false;
        if (json) TAIL_ROWS(b, rows, EMIT_JSON_ROW);
        else      TAIL_ROWS(b, rows, EMIT_CSV_ROW);
        if (oom) { bad = 1; err = ENOMEM; }
        else if (out->n && fwrite(out->p, 1, out->n, fh) != out->n) { bad = 1; err = errno; }
    }

    for (unsigned p = 0; p < ways; p++) free(blob[p].p);
    free(blob);
    if (fclose(fh) != 0) { bad = 1; if (!err) err = errno; }
    return bad ? -(err ? err : EIO) : 0;
}

static int write_parquet(const char *path, bool b, int64_t rows, int64_t seed,
                         size_t group_rows, size_t dict_limit) {
    PqWriter *w = pqw_open(path, g_names, (size_t)g_columns, group_rows, dict_limit);
    if (!w) return -1;
    Row r;
    PqValue cells[MAX_COLUMNS];
    int bad = 0;
#define EMIT_PQ(i)                                                         \
    do {                                                                   \
        if (bad) break;                                                    \
        fields(&r, (i), b, seed);                                          \
        for (int c = 0; c < g_columns; c++) {                              \
            cells[c].p = r.f[c];                                           \
            cells[c].n = r.n[c];                                           \
            cells[c].null = r.n[c] == 0;                                   \
        }                                                                  \
        if (pqw_row(w, cells) != 0) bad = 1;                               \
    } while (0)
    EACH_ROW(b, rows, EMIT_PQ);
#undef EMIT_PQ
    return pqw_close(w) != 0 || bad ? -1 : 0;
}

/* One side of the pair, so that both can be written at once. */
typedef struct {
    char     path[2][4096];
    int      ok[2];
    int64_t  rows, seed;
    size_t   group_rows, dict_limit;
    bool     parquet, json;
    unsigned threads;
} Sides;

static void write_side(void *vctx, unsigned side) {
    Sides *s = vctx;
    const bool b = side == 1;
    s->ok[side] = s->parquet
        ? write_parquet(s->path[side], b, s->rows, s->seed, s->group_rows, s->dict_limit)
        : write_text(s->path[side], b, s->rows, s->seed, s->json, s->threads);
}

/* ------------------------------------------------------------------------- */
/* The command line                                                            */
/* ------------------------------------------------------------------------- */

/* "10m" and "20k" as well as a plain count, which is how every table in this
 * project names a row count. */
static int64_t parse_rows(const char *s) {
    char *end = NULL;
    long long v = strtoll(s, &end, 10);
    if (end && (*end == 'k' || *end == 'K')) v *= 1000;
    else if (end && (*end == 'm' || *end == 'M')) v *= 1000000;
    return (int64_t)v;
}

static int usage(void) {
    fprintf(stderr,
            "usage: gen-data --rows 10m --out-dir DIR [--prefix P]\n"
            "                [--format csv|json|ndjson|parquet] [--seed N]\n"
            "                [--row-group-size N] [--dict-limit N] [--columns N]\n\n"
            "Writes DIR/P_a.EXT and DIR/P_b.EXT. Parquet is uncompressed: this port\n"
            "carries no codec, by the same choice its reader makes.\n\n"
            "--dict-limit is how many distinct values a column may have in one row\n"
            "group before it gives up on the dictionary; lowering it produces the\n"
            "mixed-encoding columns a real writer emits at scale.\n\n"
            "--columns widens the standard twenty columns to N (up to 256) with\n"
            "extra columns c021, c022 ... of 8-digit values, about the bytes of a\n"
            "standard cell, so 2m x 100 is close to the size of 10m x 20.\n");
    return 2;
}

int main(int argc, char **argv) {
    int64_t rows = 0, seed = 7;   /* the same default every generator here uses */
    unsigned threads = 0;         /* 0 means as many as there are cores */
    const char *out_dir = NULL, *prefix = NULL, *format = "csv";
    size_t group_rows = 122880, dict_limit = 8192;   /* the same defaults as the other generators */

    for (int i = 1; i < argc; i++) {
        const char *f = argv[i];
        if (!strcmp(f, "--rows") && i + 1 < argc) rows = parse_rows(argv[++i]);
        else if (!strcmp(f, "--out-dir") && i + 1 < argc) out_dir = argv[++i];
        else if (!strcmp(f, "--prefix") && i + 1 < argc) prefix = argv[++i];
        else if (!strcmp(f, "--format") && i + 1 < argc) format = argv[++i];
        else if (!strcmp(f, "--seed") && i + 1 < argc) seed = parse_rows(argv[++i]);
        else if (!strcmp(f, "--row-group-size") && i + 1 < argc) group_rows = (size_t)parse_rows(argv[++i]);
        else if (!strcmp(f, "--dict-limit") && i + 1 < argc) dict_limit = (size_t)parse_rows(argv[++i]);
        else if (!strcmp(f, "--threads") && i + 1 < argc) threads = (unsigned)parse_rows(argv[++i]);
        else if (!strcmp(f, "--columns") && i + 1 < argc) {
            g_columns = (int)parse_rows(argv[++i]);
            if (g_columns < COLUMNS || g_columns > MAX_COLUMNS) {
                fprintf(stderr, "error: --columns takes %d to %d\n", COLUMNS, MAX_COLUMNS);
                return 2;
            }
        }
        else if (!strcmp(f, "--compression") && i + 1 < argc) {
            const char *c = argv[++i];
            if (strcmp(c, "none") != 0) {
                fprintf(stderr, "error: this generator writes uncompressed parquet only\n");
                return 2;
            }
        } else return usage();
    }
    if (rows <= 0 || !out_dir) return usage();
    if (!prefix) prefix = "data";
    build_days();

    /* `ndjson` and `json` name the same format. The Rust generator has always
     * taken both; this port and the C++ one took only `json`, so a command that
     * worked against one generator failed against another -- in a project whose
     * whole premise is that the ports agree. Normalised here so the rest of the
     * function still has one spelling to test. */
    if (!strcmp(format, "ndjson")) format = "json";

    const char *ext;
    if (!strcmp(format, "csv")) ext = ".csv";
    else if (!strcmp(format, "json")) ext = ".ndjson";
    else if (!strcmp(format, "parquet")) ext = ".unc.parquet";
    else return usage();

    /* Create the output directory rather than fail on it. `data/` is in
     * .gitignore, so on a fresh clone the README's own first command wrote
     * into a directory that was not there. One level only: a missing parent
     * still fails, and now says so. */
    if (make_dir(out_dir) != 0 && errno != EEXIST) {
        fprintf(stderr, "error: cannot create %s: %s\n", out_dir, strerror(errno));
        return 1;
    }

    if (threads == 0) threads = cpu_count();
    if (threads == 0) threads = 1;

    Sides sd = { { { 0 }, { 0 } }, { -1, -1 }, rows, seed, group_rows, dict_limit,
                 !strcmp(format, "parquet"), !strcmp(format, "json"), threads };
    for (int side = 0; side < 2; side++)
        snprintf(sd.path[side], sizeof sd.path[side], "%s/%s_%c%s", out_dir, prefix,
                 side ? 'b' : 'a', ext);

    /*
     * Parquet writes both sides at once; the text formats divide inside the
     * file instead and running the sides together as well would only
     * oversubscribe the same cores.
     *
     * That split is measured, not assumed. Parquet's columns do divide -- they
     * are independent by construction and pqwrite splits them -- but that alone
     * was worth only 1.28x, because most of a Parquet run is the serial feeding
     * of rows into the column arenas rather than the encoding of them. Two
     * sides at once is worth 2.0x, and the two together 2.2x. So both are on
     * here, oversubscribed on purpose, and the column split earns its 12% on
     * top rather than being the main event it looks like it should be.
     */
    if (sd.parquet) run_parts(write_side, &sd, 2);
    else for (unsigned side = 0; side < 2; side++) write_side(&sd, side);

    for (int side = 0; side < 2; side++)
        if (sd.ok[side] != 0) {
            fprintf(stderr, "error: cannot write %s: %s\n", sd.path[side],
                    sd.parquet ? pqw_error() : strerror(-sd.ok[side]));
            return 1;
        }
    return 0;
}
