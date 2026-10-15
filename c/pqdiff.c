/* The columnar comparison. See pqdiff.h for the design. */
#define _GNU_SOURCE

#include "pqdiff.h"
#include "parallel.h"
#include "parquet.h"

#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "win32.h"   /* O_BINARY off Windows; the mmap shim on it */
#ifndef _WIN32
#include <sys/mman.h>
#endif
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define FNV_PRIME UINT64_C(0x100000001b3)
#define FNV_SEED  UINT64_C(0xcbf29ce484222325)

/* ------------------------------------------------------------------------- */
/* The mapping                                                                 */
/*                                                                             */
/* Read-only and whole-file, like the CSV path's Slab, but without the dialect: */
/* a Parquet file says what it is in its own footer. Columns are read one at a  */
/* time and each is contiguous, so the access pattern is a series of sequential */
/* runs rather than one, which is what WILLNEED describes and SEQUENTIAL does   */
/* not.                                                                        */
/* ------------------------------------------------------------------------- */

typedef struct {
    const char *data;
    size_t      size;
    int         fd;
} Map;

static int map_open(Map *m, const char *path) {
    m->data = NULL;
    m->size = 0;
    m->fd = open(path, O_RDONLY | O_BINARY);
    if (m->fd < 0) return pq_set_errorf("cannot read one of the files", path);
    struct stat st;
    if (fstat(m->fd, &st) != 0) { close(m->fd); m->fd = -1; return pq_set_errorf("cannot read one of the files", path); }
    m->size = (size_t)st.st_size;
    if (m->size > 0) {
        void *p = mmap(NULL, m->size, PROT_READ, MAP_PRIVATE, m->fd, 0);
        if (p == MAP_FAILED) { close(m->fd); m->fd = -1; return pq_set_errorf("cannot map one of the files", path); }
        madvise(p, m->size, MADV_WILLNEED);
        m->data = p;
    }
    return 0;
}

static void map_close(Map *m) {
    if (m->data) munmap((void *)m->data, m->size);
    if (m->fd >= 0) close(m->fd);
    m->data = NULL;
    m->fd = -1;
}

/* ------------------------------------------------------------------------- */
/* Values                                                                      */
/*                                                                             */
/* This path carries no --trim, --ignore-case, --empty-is-null or --tolerance  */
/* yet -- the caller refuses them by name before it gets here -- so a value is  */
/* its bytes and nothing is normalised before it is compared. Absent is null    */
/* or empty, the same rule the CSV path uses without the options, so the two    */
/* paths agree on which cells count as blanked and filled.                      */
/* ------------------------------------------------------------------------- */

typedef struct {
    const char *p;
    uint32_t    n;
    int         null;
} Cell;

static inline int cell_absent(Cell c) { return c.null || c.n == 0; }

static inline int cell_same(Cell x, Cell y) {
    const int xa = cell_absent(x), ya = cell_absent(y);
    if (xa || ya) return xa && ya;
    return x.n == y.n && memcmp(x.p, y.p, x.n) == 0;
}

/*
 * FNV-1a widened to eight bytes a step.
 *
 * Interning a dictionary calls this once per *distinct* value and does not care.
 * A key column the writer gave up on a dictionary for -- which a high-cardinality
 * key column usually is -- is hashed once per row, on both sides, and that is
 * the sweep that dominates building the indexes.
 *
 * Only the hash changes, never the answer: two keys are compared with memcmp
 * either way, so this decides how work is bucketed and nothing else. The
 * xor-shift at the end is not decoration -- the table takes its slot from the
 * low bits and its tag from the top twenty-four, so a multiply's weakly-mixed
 * low half would cost probes at one end or false tag hits at the other.
 */
static inline uint64_t fold_bytes(uint64_t h, const char *p, size_t n) {
    size_t i = 0;
    for (; i + 8 <= n; i += 8) {
        uint64_t w;
        memcpy(&w, p + i, 8);
        h = (h ^ w) * FNV_PRIME;
    }
    if (i < n) {
        uint64_t w = 0;
        memcpy(&w, p + i, n - i);      /* the tail, zero-padded */
        h = (h ^ w) * FNV_PRIME;
    }
    h = (h ^ n) * FNV_PRIME;
    return h ^ (h >> 29);
}
static inline uint64_t fold_absent(uint64_t h) {
    return (h ^ UINT64_C(0x9e3779b97f4a7c15)) * FNV_PRIME;
}
static inline uint64_t fold_id(uint64_t h, int32_t id) {
    return (h ^ (uint64_t)(uint32_t)id) * FNV_PRIME;
}

/* ------------------------------------------------------------------------- */
/* A column, as the comparison sees it                                         */
/*                                                                             */
/* PqColumn hands back offsets; this resolves them against the mapping and      */
/* answers "what is in row r".                                                  */
/* ------------------------------------------------------------------------- */

typedef struct {
    PqColumn    c;
    const char *base;
} Col;

static inline Cell col_of(const Col *col, PqSlice s) {
    Cell out;
    if (pq_null(s)) { out.p = NULL; out.n = 0; out.null = 1; return out; }
    out.p = col->base + pq_off(s);
    out.n = pq_len(s);
    out.null = 0;
    return out;
}

static inline Cell col_at(const Col *col, size_t row) {
    if (col->c.dictionary) {
        const int32_t k = col->c.index[row];
        if (k < 0) { Cell out = { NULL, 0, 1 }; return out; }
        return col_of(col, col->c.dict[(size_t)k]);
    }
    return col_of(col, col->c.values[row]);
}

static void col_free(Col *col) { pq_column_free(&col->c); col->base = NULL; }

/* ------------------------------------------------------------------------- */
/* One id space for two dictionaries                                           */
/*                                                                             */
/* The trick the whole columnar path turns on. Two files' dictionaries are      */
/* interned into one dense id space, which costs one hash per *distinct* value  */
/* rather than one per row; after that, two cells are equal exactly when their  */
/* ids are, and a column diff is a comparison of two int32 arrays.              */
/* ------------------------------------------------------------------------- */

typedef struct {
    int32_t     *slot;
    const char **val;
    uint32_t    *len;
    uint64_t    *hash;
    size_t       count;
    uint64_t     mask;
} Ids;

static int ids_init(Ids *ids, size_t expect) {
    size_t cap = 16;
    while (cap < expect * 2 + 16) cap <<= 1;
    ids->slot = malloc(cap * sizeof *ids->slot);
    ids->val = malloc((expect + 1) * sizeof *ids->val);
    ids->len = malloc((expect + 1) * sizeof *ids->len);
    ids->hash = malloc((expect + 1) * sizeof *ids->hash);
    if (!ids->slot || !ids->val || !ids->len || !ids->hash) return pq_set_error("out of memory");
    memset(ids->slot, 0xFF, cap * sizeof *ids->slot);   /* -1 everywhere */
    ids->count = 0;
    ids->mask = cap - 1;
    return 0;
}

static void ids_free(Ids *ids) {
    free(ids->slot); free(ids->val); free(ids->len); free(ids->hash);
    memset(ids, 0, sizeof *ids);
}

static int32_t ids_of(Ids *ids, const char *p, uint32_t n) {
    const uint64_t h = fold_bytes(FNV_SEED, p, n);
    size_t at = h & ids->mask;
    for (;;) {
        const int32_t s = ids->slot[at];
        if (s < 0) break;
        const size_t k = (size_t)s;
        if (ids->hash[k] == h && ids->len[k] == n && memcmp(ids->val[k], p, n) == 0) return s;
        at = (at + 1) & ids->mask;
    }
    const int32_t id = (int32_t)ids->count;
    ids->val[ids->count] = p;
    ids->len[ids->count] = n;
    ids->hash[ids->count] = h;
    ids->count++;
    ids->slot[at] = id;
    return id;
}

/* ------------------------------------------------------------------------- */
/* Keys                                                                        */
/*                                                                             */
/* One file's key columns. A key column both sides store as a dictionary is     */
/* reduced to a shared id per row, and from there hashing and equality are      */
/* integer work; anything else stays bytes and is compared as bytes.            */
/* ------------------------------------------------------------------------- */

typedef struct {
    Col      *col;                /* one per key column */
    int32_t **id;                 /* filled where Keys::as_id */
    size_t    rows;
} KeySide;

typedef struct {
    char   *as_id;                /* per key column, shared by both sides */
    size_t  n;
    KeySide a, b;
} Keys;

static uint64_t row_hash(const Keys *k, const KeySide *s, size_t row) {
    uint64_t h = FNV_SEED;
    for (size_t j = 0; j < k->n; j++) {
        if (k->as_id[j]) { h = fold_id(h, s->id[j][row]); continue; }
        const Cell c = col_at(&s->col[j], row);
        if (cell_absent(c)) h = fold_absent(h);
        else                h = fold_bytes(h, c.p, c.n);
    }
    return h;
}

static int row_eq(const Keys *k, const KeySide *x, size_t rx, const KeySide *y, size_t ry) {
    for (size_t j = 0; j < k->n; j++) {
        if (k->as_id[j]) {
            if (x->id[j][rx] != y->id[j][ry]) return 0;
        } else if (!cell_same(col_at(&x->col[j], rx), col_at(&y->col[j], ry))) {
            return 0;
        }
    }
    return 1;
}

/* ------------------------------------------------------------------------- */
/* The index                                                                   */
/*                                                                             */
/* An open-addressed table over one file's distinct keys, first occurrence      */
/* wins.                                                                       */
/*                                                                             */
/* A slot is one word: the top twenty-four bits of the key's hash, and the      */
/* position in `firsts` plus one, with zero meaning empty. Carrying the hash    */
/* *inside* the slot is the point -- a probe that misses is settled by the word */
/* it already loaded, where a table of bare positions would have to follow each */
/* one into a separate array of hashes and take a second cache miss to reject   */
/* it. At ten million keys those second misses were the join.                   */
/* ------------------------------------------------------------------------- */

/*
 * How many rows ahead to start the load for. Measured, not reasoned: at ten
 * million rows the index build is flat within noise from 8 to 64, but the join
 * is not -- it is best at 8 to 24 and decays steadily past 32, because a probe
 * touches a second table and reaching too far ahead evicts what the current one
 * is still using. A distance of 0 prefetches the slot about to be loaded anyway,
 * which is the control: it reproduces the un-prefetched timings exactly.
 */
#define PREFETCH_AHEAD 24

/*
 * The slot table, zeroed. `alloc_huge` puts it on 2 MB pages where the kernel
 * will, which is the difference between an insert costing 123 ns and 37 ns --
 * see parallel.h for why, and c/README.md for the measurement.
 */
static uint32_t *alloc_slots(size_t n) {
    uint32_t *p = alloc_huge(n * sizeof *p);
    if (p) memset(p, 0, n * sizeof *p);
    return p;
}

struct Index {
    uint32_t *slots;
    uint64_t  mask;
    unsigned  pos_bits;
    uint32_t  pos_mask;
    int32_t  *firsts;
    uint32_t *counts;
    uint64_t *hashes;             /* per distinct key, for probing the other side */
    size_t    unique;
    int64_t   rows, dup_keys, dup_rows;
};
typedef struct Index Index;

static inline uint32_t slot_pack(const Index *ix, uint64_t hash, size_t pos) {
    const unsigned tag_bits = 32u - ix->pos_bits;
    const uint32_t tag = tag_bits ? (uint32_t)(hash >> (64u - tag_bits)) : 0u;
    return (tag << ix->pos_bits) | (uint32_t)(pos + 1);
}

static inline int slot_tag_is(const Index *ix, uint32_t v, uint64_t hash) {
    const unsigned tag_bits = 32u - ix->pos_bits;
    if (!tag_bits) return 1;
    return (v >> ix->pos_bits) == (uint32_t)(hash >> (64u - tag_bits));
}

static inline size_t slot_pos(const Index *ix, uint32_t v) {
    return (size_t)(v & ix->pos_mask) - 1;
}

static void index_free(Index *ix) {
    free(ix->slots); free(ix->firsts); free(ix->counts); free(ix->hashes);
    memset(ix, 0, sizeof *ix);
}

/* ------------------------------------------------------------------------- */
/* Errors out of a worker                                                      */
/*                                                                             */
/* pq_error() is per-thread, so a worker's message would be lost when its       */
/* thread ends. Each part copies its own into the shared context instead, and   */
/* the caller re-raises the first one it finds.                                 */
/* ------------------------------------------------------------------------- */

typedef struct {
    int  failed;
    char text[192];
} Failure;

static void note_failure(Failure *f) {
    f->failed = 1;
    snprintf(f->text, sizeof f->text, "%s", pq_error());
}

static int raise_first(const Failure *f, size_t n) {
    for (size_t i = 0; i < n; i++)
        if (f[i].failed) return pq_set_error(f[i].text);
    return 0;
}

/* ------------------------------------------------------------------------- */
/* Building the index                                                          */
/*                                                                             */
/* Hashes are computed in parallel because a row's hash depends on nothing but  */
/* that row; insertion is serial because first-occurrence-wins depends on the   */
/* order rows arrive, and threading it would make the answer depend on the      */
/* scheduler. Same split as the CSV path.                                       */
/* ------------------------------------------------------------------------- */

typedef struct {
    const Keys    *k;
    const KeySide *s;
    uint64_t      *hs;
    unsigned       ways;
} SweepCtx;

static void sweep_part(void *vctx, unsigned p) {
    SweepCtx *c = vctx;
    const size_t rows = c->s->rows;
    const size_t lo = rows * p / c->ways, hi = rows * (p + 1) / c->ways;
    for (size_t r = lo; r < hi; r++) c->hs[r] = row_hash(c->k, c->s, r);
}

static int build_index(const Keys *k, const KeySide *s, unsigned threads, Index *ix) {
    memset(ix, 0, sizeof *ix);
    ix->rows = (int64_t)s->rows;

    uint64_t *hs = malloc((s->rows ? s->rows : 1) * sizeof *hs);
    if (!hs) return pq_set_error("out of memory");
    SweepCtx sc = { k, s, hs, s->rows < (1u << 15) ? 1u : (threads ? threads : 1u) };
    run_parts(sweep_part, &sc, sc.ways);

    /* Sized to about a two-thirds load: linear probing is still short there,
     * and a smaller table is a smaller working set, which is what this phase is
     * actually limited by. */
    size_t cap = 1u << 12;
    while (cap * 2 < s->rows * 3 + 16) cap <<= 1;
    {
        const size_t n = s->rows ? s->rows : 1;
        if (budget_take(cap * sizeof *ix->slots +
                        n * (sizeof *ix->firsts + sizeof *ix->counts + sizeof *ix->hashes)) != 0) {
            free(hs);
            index_free(ix);
            return pq_set_error("over the --max-memory ceiling");
        }
    }
    ix->slots = alloc_slots(cap);
    ix->firsts = malloc((s->rows ? s->rows : 1) * sizeof *ix->firsts);
    ix->counts = malloc((s->rows ? s->rows : 1) * sizeof *ix->counts);
    ix->hashes = malloc((s->rows ? s->rows : 1) * sizeof *ix->hashes);
    if (!ix->slots || !ix->firsts || !ix->counts || !ix->hashes) {
        free(hs);
        index_free(ix);
        return pq_set_error("out of memory");
    }
    ix->mask = cap - 1;
    /* Adaptive slot width: position bits from the row count, the rest is hash tag. */
    ix->pos_bits = 1;
    while (ix->pos_bits < 32 && ((size_t)1 << ix->pos_bits) < (size_t)s->rows + 2) ix->pos_bits++;
    ix->pos_mask = ix->pos_bits >= 32 ? 0xFFFFFFFFu : (uint32_t)(((uint64_t)1 << ix->pos_bits) - 1);

    /*
     * Insertion is one DRAM miss per row and nothing else.
     *
     * The table is a quarter of a gigabyte at ten million keys, the slot a row
     * lands in is a hash away from anything the last row touched, and the loop
     * is serial because first-occurrence-wins depends on the order rows arrive.
     * So the processor spends the phase stalled on loads it could have started
     * earlier -- and it *could* have, because `hs` already holds every hash. A
     * fixed distance ahead is enough to cover a memory latency without evicting
     * what the current row is using.
     */
    for (size_t r = 0; r < s->rows; r++) {
        if (r + PREFETCH_AHEAD < s->rows)
            __builtin_prefetch(&ix->slots[hs[r + PREFETCH_AHEAD] & ix->mask], 1, 0);
        const uint64_t h = hs[r];
        size_t at = h & ix->mask;
        for (;;) {
            const uint32_t slot = ix->slots[at];
            if (slot == 0) {
                ix->slots[at] = slot_pack(ix, h, ix->unique);
                ix->firsts[ix->unique] = (int32_t)r;
                ix->counts[ix->unique] = 1;
                ix->hashes[ix->unique] = h;
                ix->unique++;
                break;
            }
            if (slot_tag_is(ix, slot, h)) {
                const size_t pos = slot_pos(ix, slot);
                if (row_eq(k, s, (size_t)ix->firsts[pos], s, r)) {
                    if (++ix->counts[pos] == 2) {
                        ix->dup_keys++;
                        ix->dup_rows++;   /* the first occurrence counts once the key repeats */
                    }
                    ix->dup_rows++;
                    break;
                }
            }
            at = (at + 1) & ix->mask;
        }
    }
    free(hs);
    return 0;
}

/*
 * Both files' indexes are built at once, each on half the budget.
 *
 * Not for the parallelism alone: the hash sweep inside each build is parallel
 * but the insertion after it is serial, so run one after the other the machine
 * sits half idle through both serial tails. Overlapped, one side's insertion
 * runs against the other side's sweep.
 */
typedef struct {
    const Keys *k;
    Index      *ix[2];
    unsigned    threads;
    Failure     fail[2];
} IndexCtx;

static void index_part(void *vctx, unsigned p) {
    IndexCtx *c = vctx;
    const KeySide *s = p == 0 ? &c->k->a : &c->k->b;
    if (build_index(c->k, s, c->threads, c->ix[p]) != 0) note_failure(&c->fail[p]);
}

/* Looks one side's row up in the other side's table. */
static int32_t lookup(const Keys *k, const Index *into, const KeySide *there,
                      const KeySide *here, size_t row, uint64_t h) {
    size_t at = h & into->mask;
    for (;;) {
        const uint32_t slot = into->slots[at];
        if (slot == 0) return -1;
        if (slot_tag_is(into, slot, h)) {
            const int32_t first = into->firsts[slot_pos(into, slot)];
            if (row_eq(k, there, (size_t)first, here, row)) return first;
        }
        at = (at + 1) & into->mask;
    }
}

/* ------------------------------------------------------------------------- */
/* Phase timings                                                               */
/*                                                                             */
/* On stderr when CSVDIFF_PHASES is set, in the same shape the C++ port prints  */
/* them, so the two can be read side by side. A columnar comparison has three   */
/* costs that move independently -- getting the key columns out of the file,    */
/* joining on them, and walking the compared columns -- and knowing which one   */
/* grew is the difference between tuning and guessing.                          */
/* ------------------------------------------------------------------------- */

typedef struct { int on; struct timespec last; } Phases;

static void phases_init(Phases *p) {
    p->on = getenv("CSVDIFF_PHASES") != NULL;
    clock_gettime(CLOCK_MONOTONIC, &p->last);
}

static void phase_mark(Phases *p, const char *what) {
    if (!p->on) return;
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    const double secs = (double)(now.tv_sec - p->last.tv_sec) +
                        (double)(now.tv_nsec - p->last.tv_nsec) / 1e9;
    fprintf(stderr, "  %-22s %6.3fs\n", what, secs);
    p->last = now;
}

/* ------------------------------------------------------------------------- */
/* The mismatch mask                                                           */
/*                                                                             */
/* `neq` holds one byte per pair, set where the two cells differ. The scan      */
/* reads it eight bytes at a time -- the same SWAR idiom the CSV scanner uses   */
/* to find a delimiter, applied to finding a changed cell. Most words are zero: */
/* on a typical run over ninety per cent of eight-pair steps hold no change at  */
/* all, and the whole step is then one load and one test.                       */
/* ------------------------------------------------------------------------- */

#define BLOCK 4096

typedef struct {
    int64_t   changed, blanked, filled;
    uint64_t *bits;               /* one per matched pair */
} ColOut;

static inline void hit_pair(size_t p, const Col *A, const Col *B,
                            const int32_t *pair_a, const int32_t *pair_b, ColOut *out) {
    const Cell x = col_at(A, (size_t)pair_a[p]);
    const Cell y = col_at(B, (size_t)pair_b[p]);
    out->changed++;
    if (cell_absent(y)) out->blanked++;
    if (cell_absent(x)) out->filled++;
    out->bits[p >> 6] |= UINT64_C(1) << (p & 63);
}

static void scan_mask(const uint8_t *neq, size_t m, size_t base, const Col *A, const Col *B,
                      const int32_t *pair_a, const int32_t *pair_b, ColOut *out) {
    size_t i = 0;
    for (; i + 8 <= m; i += 8) {
        uint64_t w;
        memcpy(&w, neq + i, 8);
        while (w) {
            const unsigned byte = (unsigned)__builtin_ctzll(w) >> 3;
            hit_pair(base + i + byte, A, B, pair_a, pair_b, out);
            w &= ~(UINT64_C(0xFF) << (byte * 8));
        }
    }
    for (; i < m; i++)
        if (neq[i]) hit_pair(base + i, A, B, pair_a, pair_b, out);
}

/* ------------------------------------------------------------------------- */
/* Reading both files' key columns at once                                     */
/* ------------------------------------------------------------------------- */

typedef struct {
    const Map   *m[2];
    const char  *path[2];
    const PqMeta *meta[2];
    char *const *key;
    size_t       nkey;
    KeySide     *side[2];
    Failure      fail[2];
} KeyReadCtx;

static size_t name_slot(const PqMeta *m, const char *n) {
    for (size_t i = 0; i < m->names_len; i++)
        if (strcmp(m->names[i], n) == 0) return i;
    return m->names_len;
}

static void read_keys_part(void *vctx, unsigned p) {
    KeyReadCtx *c = vctx;
    KeySide *s = c->side[p];
    for (size_t j = 0; j < c->nkey; j++) {
        if (pq_read_column(c->m[p]->data, c->m[p]->size,
                           name_slot(c->meta[p], c->key[j]), &s->col[j].c) != 0) {
            note_failure(&c->fail[p]);
            return;
        }
        s->col[j].base = pq_base(&s->col[j].c, c->m[p]->data);
    }
}

/* ------------------------------------------------------------------------- */
/* The join, both directions                                                   */
/* ------------------------------------------------------------------------- */

typedef struct {
    int32_t *pa, *pb;
    size_t   n;
    int64_t  missing;
    Failure  fail;
} Part;

typedef struct {
    const Keys  *k;
    const Index *ai, *bi;
    Part        *parts;           /* the A direction */
    Part        *b_parts;
    unsigned     ways, b_ways;
} JoinCtx;

static void join_part(void *vctx, unsigned p) {
    JoinCtx *c = vctx;
    if (p < c->ways) {
        Part *out = &c->parts[p];
        const size_t lo = c->ai->unique * p / c->ways;
        const size_t hi = c->ai->unique * (p + 1) / c->ways;
        out->pa = malloc((hi - lo + 1) * sizeof *out->pa);
        out->pb = malloc((hi - lo + 1) * sizeof *out->pb);
        if (!out->pa || !out->pb) { pq_set_error("out of memory"); note_failure(&out->fail); return; }
        for (size_t at = lo; at < hi; at++) {
            /* The same stall as the insertion above, in the other table: the
             * hash of the key this side will probe with is already in hand. */
            if (at + PREFETCH_AHEAD < hi)
                __builtin_prefetch(&c->bi->slots[c->ai->hashes[at + PREFETCH_AHEAD] &
                                                 c->bi->mask], 0, 0);
            const int32_t row = c->ai->firsts[at];
            const int32_t mate = lookup(c->k, c->bi, &c->k->b, &c->k->a, (size_t)row,
                                        c->ai->hashes[at]);
            if (mate < 0) { out->missing++; continue; }
            out->pa[out->n] = row;
            out->pb[out->n] = mate;
            out->n++;
        }
        return;
    }
    Part *out = &c->b_parts[p - c->ways];
    const unsigned q = p - c->ways;
    const size_t lo = c->bi->unique * q / c->b_ways;
    const size_t hi = c->bi->unique * (q + 1) / c->b_ways;
    for (size_t at = lo; at < hi; at++) {
        if (at + PREFETCH_AHEAD < hi)
            __builtin_prefetch(&c->ai->slots[c->bi->hashes[at + PREFETCH_AHEAD] &
                                             c->ai->mask], 0, 0);
        const int32_t row = c->bi->firsts[at];
        if (lookup(c->k, c->ai, &c->k->a, &c->k->b, (size_t)row, c->bi->hashes[at]) < 0)
            out->missing++;
    }
}

/* ------------------------------------------------------------------------- */
/* The compared columns, one at a time                                         */
/*                                                                             */
/* Each worker owns a whole column, so nothing is shared but the pair arrays    */
/* and the two mappings, which are read-only from here on. How many are in      */
/* flight is a memory choice as much as a parallelism one: a column of ten      */
/* million values costs a couple of hundred megabytes on each side while it is  */
/* being read, and it is released before the next is asked for.                 */
/* ------------------------------------------------------------------------- */

typedef struct {
    const Map      *am, *bm;
    const PqMeta   *ameta, *bmeta;
    char          **compared;
    size_t          nc;
    const int32_t  *pair_a, *pair_b;
    size_t          npairs, words;
    size_t          a_rows, b_rows;
    ColOut         *cols;
    Failure        *fail;         /* one per column */
    atomic_size_t   next;
} ColCtx;

static int code_dictionary(const Col *col, Ids *ids, int32_t **map_out) {
    int32_t *map = malloc((col->c.dict_len ? col->c.dict_len : 1) * sizeof *map);
    if (!map) return pq_set_error("out of memory");
    for (size_t k = 0; k < col->c.dict_len; k++) {
        const Cell cell = col_of(col, col->c.dict[k]);
        map[k] = cell_absent(cell) ? -1 : ids_of(ids, cell.p, cell.n);
    }
    *map_out = map;
    return 0;
}

static void column_part(void *vctx, unsigned p) {
    ColCtx *c = vctx;
    (void)p;
    for (;;) {
        const size_t j = atomic_fetch_add(&c->next, 1);
        if (j >= c->nc) return;

        ColOut  *out = &c->cols[j];
        Col      A = { {0}, NULL }, B = { {0}, NULL };
        Ids      ids = {0};
        int32_t *amap = NULL, *bmap = NULL;
        int32_t *xa = NULL, *xb = NULL;
        uint8_t *neq = NULL;

        out->bits = calloc(c->words ? c->words : 1, sizeof *out->bits);
        if (!out->bits) { pq_set_error("out of memory"); note_failure(&c->fail[j]); goto next; }

        if (pq_read_column(c->am->data, c->am->size, name_slot(c->ameta, c->compared[j]), &A.c) != 0) {
            note_failure(&c->fail[j]); goto next;
        }
        A.base = pq_base(&A.c, c->am->data);
        if (pq_read_column(c->bm->data, c->bm->size, name_slot(c->bmeta, c->compared[j]), &B.c) != 0) {
            note_failure(&c->fail[j]); goto next;
        }
        B.base = pq_base(&B.c, c->bm->data);
        if (pq_rows(&A.c) != c->a_rows || pq_rows(&B.c) != c->b_rows) {
            pq_set_error("parquet columns disagree about how many rows the file has");
            note_failure(&c->fail[j]);
            goto next;
        }

        neq = malloc(BLOCK);
        if (!neq) { pq_set_error("out of memory"); note_failure(&c->fail[j]); goto next; }

        /* The fast path: both sides dictionary encoded, so the two dictionaries
         * go into one id space and the per-row work is two gathers and an
         * integer compare. */
        if (A.c.dictionary && B.c.dictionary) {
            xa = malloc(BLOCK * sizeof *xa);
            xb = malloc(BLOCK * sizeof *xb);
            if (!xa || !xb || ids_init(&ids, A.c.dict_len + B.c.dict_len) != 0 ||
                code_dictionary(&A, &ids, &amap) != 0 || code_dictionary(&B, &ids, &bmap) != 0) {
                if (!xa || !xb) pq_set_error("out of memory");
                note_failure(&c->fail[j]);
                goto next;
            }
            const int32_t *aix = A.c.index, *bix = B.c.index;
            for (size_t base = 0; base < c->npairs; base += BLOCK) {
                size_t m = c->npairs - base;
                if (m > BLOCK) m = BLOCK;
                for (size_t i = 0; i < m; i++) {
                    const int32_t k = aix[(size_t)c->pair_a[base + i]];
                    xa[i] = k < 0 ? -1 : amap[(size_t)k];
                }
                for (size_t i = 0; i < m; i++) {
                    const int32_t k = bix[(size_t)c->pair_b[base + i]];
                    xb[i] = k < 0 ? -1 : bmap[(size_t)k];
                }
                for (size_t i = 0; i < m; i++) neq[i] = xa[i] != xb[i];
                scan_mask(neq, m, base, &A, &B, c->pair_a, c->pair_b, out);
            }
        } else {
            for (size_t base = 0; base < c->npairs; base += BLOCK) {
                size_t m = c->npairs - base;
                if (m > BLOCK) m = BLOCK;
                for (size_t i = 0; i < m; i++)
                    neq[i] = !cell_same(col_at(&A, (size_t)c->pair_a[base + i]),
                                        col_at(&B, (size_t)c->pair_b[base + i]));
                scan_mask(neq, m, base, &A, &B, c->pair_a, c->pair_b, out);
            }
        }

    next:
        free(neq); free(xa); free(xb); free(amap); free(bmap);
        ids_free(&ids);
        col_free(&A);
        col_free(&B);
    }
}

/* ------------------------------------------------------------------------- */
/* The entry points                                                            */
/* ------------------------------------------------------------------------- */

int pq_is_parquet(const char *path) {
    int fd = open(path, O_RDONLY | O_BINARY);
    if (fd < 0) return -1;
    char magic[4] = {0};
    const ssize_t got = read(fd, magic, 4);
    close(fd);
    return got == 4 && memcmp(magic, "PAR1", 4) == 0;
}

void pq_result_free(PqResult *r) {
    for (size_t i = 0; i < r->ncols; i++) free(r->cols[i].name);
    free(r->cols);
    memset(r, 0, sizeof *r);
}

static int has_name(char *const *v, size_t n, const char *needle) {
    for (size_t i = 0; i < n; i++)
        if (strcmp(v[i], needle) == 0) return 1;
    return 0;
}

int pq_compare(const char *a_path, const char *b_path,
               char *const *key, size_t nkey,
               char *const *ignore, size_t nignore,
               char *const *compare, size_t ncompare,
               unsigned threads, PqResult *out) {
    memset(out, 0, sizeof *out);

    Phases phase;
    phases_init(&phase);

    int      status = -1;
    Map      am = { NULL, 0, -1 }, bm = { NULL, 0, -1 };
    PqMeta   ameta = {0}, bmeta = {0};
    char   **compared = NULL;
    size_t   nc = 0;
    Keys     keys = {0};
    Index    ai = {0}, bi = {0};
    int32_t *pair_a = NULL, *pair_b = NULL;
    Part    *parts = NULL, *b_parts = NULL;
    unsigned ways = 0, b_ways = 0;
    ColOut  *cols = NULL;
    Failure *col_fail = NULL;
    uint64_t *any = NULL;

    unsigned budget = threads ? threads : cpu_count();

    if (map_open(&am, a_path) != 0 || map_open(&bm, b_path) != 0) goto done;
    if (pq_read_meta(am.data, am.size, &ameta) != 0) goto done;
    if (pq_read_meta(bm.data, bm.size, &bmeta) != 0) goto done;

    for (size_t j = 0; j < nkey; j++)
        if (name_slot(&ameta, key[j]) == ameta.names_len ||
            name_slot(&bmeta, key[j]) == bmeta.names_len) {
            pq_set_errorf("key column(s) missing from one of the files", key[j]);
            goto done;
        }
    /* Same rule as the text path: a name in neither file is a misspelling, and
     * a misspelled --ignore widens the comparison without saying so. */
    for (size_t j = 0; j < nignore; j++)
        if (name_slot(&ameta, ignore[j]) == ameta.names_len &&
            name_slot(&bmeta, ignore[j]) == bmeta.names_len) {
            pq_set_errorf("ignore column(s) present in neither file", ignore[j]);
            goto done;
        }

    /* Sized for whichever list is longer: --compare may name a column twice,
     * which is harmless but would run past ameta.names_len entries. */
    {
        const size_t room = ncompare > ameta.names_len ? ncompare : ameta.names_len;
        compared = malloc((room ? room : 1) * sizeof *compared);
    }
    if (!compared) { pq_set_error("out of memory"); goto done; }
    if (ncompare > 0) {
        /* Named explicitly: the order is the caller's, and a name that is not
         * in both files is an error rather than a column quietly dropped. Key
         * and ignored columns are filtered out here as they are below, so
         * `-k id -c id,a` compares `a` rather than refusing. */
        for (size_t j = 0; j < ncompare; j++) {
            const size_t sa = name_slot(&ameta, compare[j]);
            if (sa == ameta.names_len || name_slot(&bmeta, compare[j]) == bmeta.names_len) {
                pq_set_errorf("compare column(s) not present in both files", compare[j]);
                goto done;
            }
            if (has_name(key, nkey, compare[j]) || has_name(ignore, nignore, compare[j])) continue;
            compared[nc++] = ameta.names[sa];  /* borrowed from ameta, freed with it */
        }
    } else {
        for (size_t i = 0; i < ameta.names_len; i++) {
            const char *n = ameta.names[i];
            if (name_slot(&bmeta, n) == bmeta.names_len) continue;
            if (has_name(key, nkey, n) || has_name(ignore, nignore, n)) continue;
            compared[nc++] = ameta.names[i];   /* borrowed from ameta, freed with it */
        }
    }

    /* --- keys ------------------------------------------------------------ */
    keys.n = nkey;
    keys.as_id = calloc(nkey ? nkey : 1, sizeof *keys.as_id);
    keys.a.col = calloc(nkey ? nkey : 1, sizeof *keys.a.col);
    keys.b.col = calloc(nkey ? nkey : 1, sizeof *keys.b.col);
    keys.a.id = calloc(nkey ? nkey : 1, sizeof *keys.a.id);
    keys.b.id = calloc(nkey ? nkey : 1, sizeof *keys.b.id);
    if (!keys.as_id || !keys.a.col || !keys.b.col || !keys.a.id || !keys.b.id) {
        pq_set_error("out of memory");
        goto done;
    }
    {
        KeyReadCtx kc = {0};
        kc.m[0] = &am; kc.m[1] = &bm;
        kc.path[0] = a_path; kc.path[1] = b_path;
        kc.meta[0] = &ameta; kc.meta[1] = &bmeta;
        kc.key = key; kc.nkey = nkey;
        kc.side[0] = &keys.a; kc.side[1] = &keys.b;
        run_parts(read_keys_part, &kc, 2);
        if (raise_first(kc.fail, 2) != 0) goto done;
    }
    phase_mark(&phase, "read key columns");

    keys.a.rows = nkey ? pq_rows(&keys.a.col[0].c) : 0;
    keys.b.rows = nkey ? pq_rows(&keys.b.col[0].c) : 0;
    for (size_t j = 1; j < nkey; j++)
        if (pq_rows(&keys.a.col[j].c) != keys.a.rows || pq_rows(&keys.b.col[j].c) != keys.b.rows) {
            pq_set_error("parquet columns disagree about how many rows the file has");
            goto done;
        }

    for (size_t j = 0; j < nkey; j++) {
        if (!keys.a.col[j].c.dictionary || !keys.b.col[j].c.dictionary) continue;
        keys.as_id[j] = 1;
        Ids ids = {0};
        int32_t *acode = NULL, *bcode = NULL;
        int ok = ids_init(&ids, keys.a.col[j].c.dict_len + keys.b.col[j].c.dict_len) == 0 &&
                 code_dictionary(&keys.a.col[j], &ids, &acode) == 0 &&
                 code_dictionary(&keys.b.col[j], &ids, &bcode) == 0;
        if (ok) {
            keys.a.id[j] = malloc((keys.a.rows ? keys.a.rows : 1) * sizeof **keys.a.id);
            keys.b.id[j] = malloc((keys.b.rows ? keys.b.rows : 1) * sizeof **keys.b.id);
            ok = keys.a.id[j] && keys.b.id[j];
            if (ok) {
                const int32_t *aix = keys.a.col[j].c.index;
                for (size_t i = 0; i < keys.a.rows; i++)
                    keys.a.id[j][i] = aix[i] < 0 ? -1 : acode[(size_t)aix[i]];
                const int32_t *bix = keys.b.col[j].c.index;
                for (size_t i = 0; i < keys.b.rows; i++)
                    keys.b.id[j][i] = bix[i] < 0 ? -1 : bcode[(size_t)bix[i]];
            } else {
                pq_set_error("out of memory");
            }
        }
        free(acode); free(bcode);
        ids_free(&ids);
        if (!ok) goto done;
    }
    phase_mark(&phase, "code key dictionaries");

    /* --- the join -------------------------------------------------------- */
    {
        IndexCtx ic = {0};
        ic.k = &keys;
        ic.ix[0] = &ai; ic.ix[1] = &bi;
        ic.threads = budget > 1 ? budget / 2 : 1;
        run_parts(index_part, &ic, 2);
        if (raise_first(ic.fail, 2) != 0) goto done;
    }
    phase_mark(&phase, "build key indexes");

    {
        /*
         * `added` is derived, not counted -- see the same argument and the same
         * `CSVDIFF_VERIFY_ADDED` switch in csvdiff.c. Every distinct key of A
         * finds at most one distinct key of B, two of A's cannot find the same
         * one, and here the comparison behind the lookup is an integer compare
         * over one interned id space, which is as symmetric as it gets. So B's
         * pass exists only to check the derivation when asked.
         */
        const int verify = getenv("CSVDIFF_VERIFY_ADDED") != NULL;
        ways = ai.unique < (1u << 14) ? 1u : (budget ? budget : 1u);
        b_ways = !verify ? 0u : (bi.unique < (1u << 14) ? 1u : (budget ? budget : 1u));
        parts = calloc(ways, sizeof *parts);
        /* Never zero: `calloc(0, n)` may return NULL, which the check below
         * would read as out of memory rather than as nothing to allocate. */
        b_parts = calloc(b_ways ? b_ways : 1u, sizeof *b_parts);
        if (!parts || !b_parts) { pq_set_error("out of memory"); goto done; }
        JoinCtx jc = { &keys, &ai, &bi, parts, b_parts, ways, b_ways };
        run_parts(join_part, &jc, ways + b_ways);
        for (unsigned p = 0; p < ways; p++)
            if (parts[p].fail.failed) { pq_set_error(parts[p].fail.text); goto done; }

        size_t total = 0;
        for (unsigned p = 0; p < ways; p++) total += parts[p].n;
        pair_a = malloc((total ? total : 1) * sizeof *pair_a);
        pair_b = malloc((total ? total : 1) * sizeof *pair_b);
        if (!pair_a || !pair_b) { pq_set_error("out of memory"); goto done; }
        size_t at = 0;
        for (unsigned p = 0; p < ways; p++) {
            memcpy(pair_a + at, parts[p].pa, parts[p].n * sizeof *pair_a);
            memcpy(pair_b + at, parts[p].pb, parts[p].n * sizeof *pair_b);
            at += parts[p].n;
            out->removed += parts[p].missing;
        }
        out->matched = (int64_t)total;
        {
            int64_t counted = 0;
            for (unsigned p = 0; p < b_ways; p++) counted += b_parts[p].missing;
            const int64_t derived = (int64_t)bi.unique - out->matched;
            if (verify && counted != derived) {
                pq_set_error("added counted and derived disagree -- the join is "
                             "not symmetric on this input");
                goto done;
            }
            out->added = derived;
        }
        out->ncols = 0;

        phase_mark(&phase, "join");

        /* --- the compared columns ---------------------------------------- */
        const size_t npairs = total, words = (npairs + 63) / 64;
        cols = calloc(nc ? nc : 1, sizeof *cols);
        col_fail = calloc(nc ? nc : 1, sizeof *col_fail);
        if (!cols || !col_fail) { pq_set_error("out of memory"); goto done; }

        ColCtx cc = {0};
        cc.am = &am; cc.bm = &bm; cc.ameta = &ameta; cc.bmeta = &bmeta;
        cc.compared = compared; cc.nc = nc;
        cc.pair_a = pair_a; cc.pair_b = pair_b;
        cc.npairs = npairs; cc.words = words;
        cc.a_rows = keys.a.rows; cc.b_rows = keys.b.rows;
        cc.cols = cols; cc.fail = col_fail;
        atomic_init(&cc.next, 0);
        unsigned lanes = budget < nc ? budget : (unsigned)nc;
        if (lanes < 1) lanes = 1;
        run_parts(column_part, &cc, lanes);
        if (raise_first(col_fail, nc) != 0) goto done;

        phase_mark(&phase, "compared columns");

        /* --- which pairs changed ----------------------------------------- */
        any = calloc(words ? words : 1, sizeof *any);
        if (!any) { pq_set_error("out of memory"); goto done; }
        for (size_t j = 0; j < nc; j++)
            for (size_t w = 0; w < words; w++) any[w] |= cols[j].bits[w];
        for (size_t w = 0; w < words; w++) out->changed += __builtin_popcountll(any[w]);
    }

    out->a_rows = ai.rows;
    out->b_rows = bi.rows;
    out->a_keys = (int64_t)ai.unique;
    out->b_keys = (int64_t)bi.unique;
    out->a_dup_keys = ai.dup_keys;
    out->a_dup_rows = ai.dup_rows;
    out->b_dup_keys = bi.dup_keys;
    out->b_dup_rows = bi.dup_rows;

    out->cols = calloc(nc ? nc : 1, sizeof *out->cols);
    if (!out->cols) { pq_set_error("out of memory"); goto done; }
    out->ncols = nc;
    for (size_t j = 0; j < nc; j++) {
        out->cols[j].name = strdup(compared[j]);
        if (!out->cols[j].name) { pq_set_error("out of memory"); goto done; }
        out->cols[j].changed = cols[j].changed;
        out->cols[j].blanked = cols[j].blanked;
        out->cols[j].filled = cols[j].filled;
    }
    status = 0;

done:
    if (cols) for (size_t j = 0; j < nc; j++) free(cols[j].bits);
    free(cols);
    free(col_fail);
    free(any);
    if (parts)
        for (unsigned p = 0; p < ways; p++) { free(parts[p].pa); free(parts[p].pb); }
    free(parts);
    free(b_parts);
    free(pair_a);
    free(pair_b);
    for (size_t j = 0; j < nkey; j++) {
        if (keys.a.col) col_free(&keys.a.col[j]);
        if (keys.b.col) col_free(&keys.b.col[j]);
        if (keys.a.id) free(keys.a.id[j]);
        if (keys.b.id) free(keys.b.id[j]);
    }
    free(keys.as_id); free(keys.a.col); free(keys.b.col); free(keys.a.id); free(keys.b.id);
    index_free(&ai);
    index_free(&bi);
    free(compared);
    pq_meta_free(&ameta);
    pq_meta_free(&bmeta);
    map_close(&am);
    map_close(&bm);
    if (status != 0) pq_result_free(out);
    return status;
}
