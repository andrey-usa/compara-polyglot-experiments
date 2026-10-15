/*
 * The floor: the byte-level comparison in C, with nothing under it.
 *
 * The same design as the Java, Rust, C++ and Zig ports — map the file, pack a
 * field into one word, find delimiters eight bytes at a time, build no string
 * for a cell — written with a fixed arena and no library beyond libc.
 *
 * This port exists to answer one question the others cannot: how little memory
 * can a correct answer be had in? It is the baseline the rest are measured
 * against, not a recommendation.
 *
 * It reads CSV, and -- through parquet.c and pqdiff.c -- uncompressed Parquet,
 * which is a different comparison rather than a different parser: a column
 * store is joined on its key columns and then diffed a column at a time, and
 * never becomes rows at all. Both paths produce the same counts on the same
 * data, which is what test.sh checks.
 *
 * Build:  make            (see Makefile; it is three files now, not one)
 * Usage:  csvdiff compare A B -k COLS [-i COLS] [--json PATH] [--threads N]
 * Exit:   0 identical, 1 differences found, 2 error
 */
#define _GNU_SOURCE
#include <ctype.h>
#include <fcntl.h>
#include <math.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#if defined(__SSE2__) || defined(__AVX2__)
#include <immintrin.h>
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "win32.h"   /* O_BINARY off Windows; the mmap shim on it */
#ifndef _WIN32
#include <sys/mman.h>
#endif
#include <sys/stat.h>
#include <unistd.h>
#include <time.h>

#include "parallel.h"
#include "parquet.h"
#include "pqdiff.h"

/*
 * Text normalisation options: --trim, --ignore-case, --empty-is-null and
 * --tolerance, with the answers the other three ports give.
 *
 * An empty value is absent whatever the options. --empty-is-null adds one case:
 * under --trim, a value that trims to nothing is absent too; without it, that
 * value is "", which is present. --tolerance compares two values that both read
 * as numbers -- after trimming whitespace, and by the same strict rule as the
 * other ports, so "inf" and "0x1p3" are text -- and --ignore-case folds ASCII.
 *
 * A byte outside ASCII under --ignore-case is refused by name where the fold
 * would decide the answer: in a key, or in a compared value that differs from
 * its mate byte for byte. That is the rule C++ and Zig follow; folding such a
 * value partially would make CAFE with an acute differ from its lower case here
 * and not in Rust, with nothing in the output to say why. The refusal is raised
 * from inside the parallel parts, so it is a flag they set and the run checks
 * before it reports.
 *
 * A run that uses none of the four passes no Norm at all: a null pointer is
 * one test per cell the compiler keeps in a register, where the fields of a
 * struct are loads it repeats past every counter the loop writes.
 */
typedef struct {
    bool   trim;
    bool   ignore_case;
    bool   empty_is_null;
    double tolerance;
} Norm;

static atomic_bool fold_refused;

/* ------------------------------------------------------------------------- */
/* A field packed into one word: offset, length, and whether it needs           */
/* unescaping. 40 bits of offset addresses a terabyte and 23 bits of length a   */
/* field of eight megabytes; an over-long field is reported, not truncated.     */
/* ------------------------------------------------------------------------- */

typedef uint64_t Field;

#define ABSENT       UINT64_MAX
#define TOO_LONG     (UINT64_MAX - 1)
#define OFFSET_MASK  ((UINT64_C(1) << 40) - 1)
#define LENGTH_SHIFT 40
#define LENGTH_MASK  ((UINT64_C(1) << 23) - 1)
#define ESCAPED_BIT  (UINT64_C(1) << 63)
#define MAX_FIELD    LENGTH_MASK

static Field pack(size_t off, size_t len, bool escaped) {
    if (len > MAX_FIELD) return TOO_LONG;
    return ((uint64_t)off & OFFSET_MASK) | (((uint64_t)len & LENGTH_MASK) << LENGTH_SHIFT) |
           (escaped ? ESCAPED_BIT : 0);
}
static size_t field_off(Field f) { return (size_t)(f & OFFSET_MASK); }
static size_t field_len(Field f) { return (size_t)((f >> LENGTH_SHIFT) & LENGTH_MASK); }
static bool field_escaped(Field f) { return (f & ESCAPED_BIT) != 0; }
static bool field_real(Field f) { return f != ABSENT && f != TOO_LONG; }

/* ------------------------------------------------------------------------- */
/* Scanning for the next delimiter or newline.                                 */
/*                                                                             */
/* SWAR is the portable floor: eight bytes per step, arithmetic rather than     */
/* comparison. Subtracting ones borrows across a byte only where that byte was  */
/* zero, and ~diff cancels the false positives the borrow creates.             */
/*                                                                             */
/* `next_of2` gets a wider step where the target has one, because it is where   */
/* the CSV path spends its time: 43% of instructions, with `parse_csv_row`      */
/* above it at 27%, so seventy per cent of the work is finding field ends. The  */
/* fields here average about ten bytes, which is the whole argument -- an       */
/* eight-byte step needs two iterations for a typical field and a sixteen-byte  */
/* one needs a single iteration.                                                */
/*                                                                             */
/* Measured against the commit before it, paired and interleaved, fifteen        */
/* rounds on a 2m-row pair: 0.469s to 0.442s, ratio 0.934 with the middle half  */
/* 0.824-0.978. On the same rows as ndjson it measured 0.984 (0.942-1.057) and  */
/* on Parquet 1.004 (0.954-1.393) -- both crossing one, so nothing is claimed   */
/* for either. ndjson's fields are longer, so the scan was already finding hits */
/* in one step, and the columnar path does not come through here at all.        */
/*                                                                             */
/* `next_of1` was left as SWAR because it did not appear in the CSV profile.   */
/* ndjson is where it shows: once the key-only parse has its keys, the rest of  */
/* a two-hundred-byte row is one scan for its newline, and that SWAR loop was   */
/* 16% of the instructions on a 2m-row ndjson pair. It takes the same steps.    */
/* ------------------------------------------------------------------------- */

#define ONES UINT64_C(0x0101010101010101)
#define HIGH UINT64_C(0x8080808080808080)

static uint64_t broadcast(unsigned char b) { return (uint64_t)b * ONES; }

static uint64_t match_bits(uint64_t w, uint64_t target) {
    uint64_t diff = w ^ target;
    return (diff - ONES) & ~diff & HIGH;
}

static uint64_t load64(const char *p) {
    uint64_t w;
    memcpy(&w, p, sizeof w);
    return w; /* x86-64 and aarch64 are little-endian; this port targets those */
}

/* The scan, for callers that want it in their own loop. `skip_json_string`
 * runs it for every key and string value of an ndjson row, a few bytes each,
 * and as a call that was a sixth of the run's instructions. */
static inline __attribute__((always_inline)) size_t scan_of2(const char *d, size_t from,
                                                             size_t end, char a, char b) {
    size_t at = from;
    /* Chosen at compile time from what the build targets, so there is no
     * dispatch on the hot path. The Makefile builds with -march=native, which
     * defines these where the CPU has them; a build for a baseline x86-64 gets
     * the SSE2 step, which that architecture always has, and anything else --
     * aarch64, which this port also targets -- falls through to the SWAR loop
     * below unchanged. Each step is a load, two compares, an or and a mask. */
#if defined(__AVX2__)
    {
        const __m256i va = _mm256_set1_epi8(a), vb = _mm256_set1_epi8(b);
        for (; at + 32 <= end; at += 32) {
            const __m256i w = _mm256_loadu_si256((const __m256i *)(d + at));
            const uint32_t hits = (uint32_t)_mm256_movemask_epi8(
                _mm256_or_si256(_mm256_cmpeq_epi8(w, va), _mm256_cmpeq_epi8(w, vb)));
            if (hits) return at + (size_t)__builtin_ctz(hits);
        }
    }
#endif
#if defined(__SSE2__)
    {
        const __m128i va = _mm_set1_epi8(a), vb = _mm_set1_epi8(b);
        for (; at + 16 <= end; at += 16) {
            const __m128i w = _mm_loadu_si128((const __m128i *)(d + at));
            const uint32_t hits = (uint32_t)_mm_movemask_epi8(
                _mm_or_si128(_mm_cmpeq_epi8(w, va), _mm_cmpeq_epi8(w, vb)));
            if (hits) return at + (size_t)__builtin_ctz(hits);
        }
    }
#endif
    {
        const uint64_t ba = broadcast((unsigned char)a), bb = broadcast((unsigned char)b);
        for (; at + 8 <= end; at += 8) {
            const uint64_t w = load64(d + at);
            const uint64_t hits = match_bits(w, ba) | match_bits(w, bb);
            if (hits) return at + (size_t)(__builtin_ctzll(hits) >> 3);
        }
    }
    for (; at < end; at++)
        if (d[at] == a || d[at] == b) return at;
    return end;
}

static size_t next_of2(const char *d, size_t from, size_t end, char a, char b) {
    return scan_of2(d, from, end, a, b);
}

/*
 * A left-to-right cursor over one row's delimiters, for a vector step only.
 *
 * `next_of2` starts a fresh scan at every field: two broadcasts, then a
 * thirty-two-byte load of which a ten-byte field uses the first ten, and the
 * next field loads most of the same bytes again. Fields are found strictly in
 * order, so the cursor keeps the chunk's unreturned matches instead and every
 * byte of the row is loaded once. The Rust and Zig ports measured the same
 * cursor 15.0% and 12.9% faster at this width, and slower at SWAR's eight
 * bytes, where a step rarely spans a field boundary and there is nothing to
 * keep -- so without AVX2 `delims_next` is `next_of2` and the cursor is inert.
 *
 * `delims_next` takes the position to resume from rather than assuming the
 * last answer, because a quoted field is walked by `skip_quoted` and the
 * cursor has to drop the matches inside its body.
 */
typedef struct {
    const char *d;
    size_t      end;
    char        a, b;
    size_t      held;    /* where the held chunk begins */
    uint32_t    bits;    /* its matches not yet returned, one bit per byte */
    size_t      scanned; /* the first byte no chunk has covered */
} Delims;

static void delims_init(Delims *c, const char *d, size_t from, size_t end, char a, char b) {
    c->d = d;
    c->end = end;
    c->a = a;
    c->b = b;
    c->held = from;
    c->bits = 0;
    c->scanned = from;
}

static inline size_t delims_next(Delims *c, size_t from) {
#if defined(__AVX2__)
    while (c->bits) {
        const size_t at = c->held + (size_t)__builtin_ctz(c->bits);
        c->bits &= c->bits - 1;
        if (at >= from) return at;
    }
    size_t at = from > c->scanned ? from : c->scanned;
    const __m256i va = _mm256_set1_epi8(c->a), vb = _mm256_set1_epi8(c->b);
    for (; at + 32 <= c->end; at += 32) {
        const __m256i w = _mm256_loadu_si256((const __m256i *)(c->d + at));
        const uint32_t hits = (uint32_t)_mm256_movemask_epi8(
            _mm256_or_si256(_mm256_cmpeq_epi8(w, va), _mm256_cmpeq_epi8(w, vb)));
        c->scanned = at + 32;
        if (hits) {
            /* `at` is never below `from`, so every match in the chunk counts. */
            c->held = at;
            c->bits = hits & (hits - 1);
            return at + (size_t)__builtin_ctz(hits);
        }
    }
    /* The last few bytes of the file: plain scanning, and nothing held. */
    const size_t next = next_of2(c->d, at, c->end, c->a, c->b);
    c->scanned = next < c->end ? next + 1 : c->end;
    return next;
#else
    return next_of2(c->d, from, c->end, c->a, c->b);
#endif
}

static size_t next_of1(const char *d, size_t from, size_t end, char t) {
    size_t at = from;
    /* The same steps as `next_of2`, chosen the same way. */
#if defined(__AVX2__)
    {
        const __m256i vt = _mm256_set1_epi8(t);
        for (; at + 32 <= end; at += 32) {
            const __m256i w = _mm256_loadu_si256((const __m256i *)(d + at));
            const uint32_t hits = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(w, vt));
            if (hits) return at + (size_t)__builtin_ctz(hits);
        }
    }
#endif
#if defined(__SSE2__)
    {
        const __m128i vt = _mm_set1_epi8(t);
        for (; at + 16 <= end; at += 16) {
            const __m128i w = _mm_loadu_si128((const __m128i *)(d + at));
            const uint32_t hits = (uint32_t)_mm_movemask_epi8(_mm_cmpeq_epi8(w, vt));
            if (hits) return at + (size_t)__builtin_ctz(hits);
        }
    }
#endif
    uint64_t bt = broadcast((unsigned char)t);
    for (; at + 8 <= end; at += 8) {
        uint64_t w = load64(d + at);
        uint64_t hits = match_bits(w, bt);
        if (hits) return at + (size_t)(__builtin_ctzll(hits) >> 3);
    }
    for (; at < end; at++)
        if (d[at] == t) return at;
    return end;
}

static size_t skip_quoted(const char *d, size_t from, size_t end) {
    size_t at = from;
    for (;;) {
        size_t q = next_of1(d, at, end, '"');
        if (q >= end) return end;
        if (q + 1 < end && d[q + 1] == '"') { at = q + 2; continue; }
        return q + 1;
    }
}

/* ------------------------------------------------------------------------- */
/* The mapped file, and reading a field's logical bytes                         */
/* ------------------------------------------------------------------------- */

/*
 * Which of the two text shapes a file is.
 *
 * It decides two things that cannot be decided separately: how a row is found,
 * and how a value's bytes are read back. CSV doubles a quote to escape it; JSON
 * puts a backslash in front. A field is stored as an offset and a length either
 * way, so the dialect has to travel with the file rather than with the field.
 */
typedef enum { DIALECT_CSV = 0, DIALECT_JSON = 1 } Dialect;

typedef struct {
    const char *data;
    size_t size;
    int fd;
    Dialect dialect;
} Slab;

static bool slab_open(Slab *s, const char *path) {
    s->fd = open(path, O_RDONLY | O_BINARY);
    if (s->fd < 0) return false;
    struct stat st;
    if (fstat(s->fd, &st) != 0) { close(s->fd); return false; }
    s->size = (size_t)st.st_size;
    s->data = NULL;
    if (s->size > 0) {
        void *p = mmap(NULL, s->size, PROT_READ, MAP_PRIVATE, s->fd, 0);
        if (p == MAP_FAILED) { close(s->fd); return false; }
        madvise(p, s->size, MADV_SEQUENTIAL); /* read once, front to back */
        s->data = p;
    }
    return true;
}

static void slab_close(Slab *s) {
    if (s->data) munmap((void *)s->data, s->size);
    if (s->fd >= 0) close(s->fd);
}

/* ------------------------------------------------------------------------- */
/* Newline-delimited JSON                                                      */
/*                                                                             */
/* One object per line. A value's bytes are contiguous in the file, so a JSON   */
/* field stays an offset and a length into the mapping exactly as a CSV field   */
/* does -- nothing here builds a string per cell either.                        */
/* ------------------------------------------------------------------------- */

static bool json_space(char c) { return c == ' ' || c == '\t' || c == '\r' || c == '\n'; }

/* Past the closing quote of the string starting at `at`. Sets *escaped when it
 * holds a backslash, which is what says the value has to be decoded. */
static inline __attribute__((always_inline)) size_t skip_json_string(const char *d, size_t at,
                                                                     size_t end, bool *escaped) {
    at++;                                        /* the opening quote */
    for (;;) {
        size_t stop = scan_of2(d, at, end, '"', '\\');
        if (stop >= end) return end;
        if (d[stop] == '"') return stop + 1;
        *escaped = true;
        at = stop + 2;                           /* the backslash and what it escapes */
        if (at > end) return end;
    }
}

/* Past a nested object or array. Not a cell value; skipped, not guessed at. */
static size_t skip_json_nested(const char *d, size_t pos, size_t end) {
    int depth = 0;
    while (pos < end) {
        char c = d[pos];
        if (c == '"') {
            bool ignored = false;
            size_t next = skip_json_string(d, pos, end, &ignored);
            if (next <= pos) return end;
            pos = next;
            continue;
        }
        if (c == '{' || c == '[') depth++;
        if (c == '}' || c == ']') {
            depth--;
            pos++;
            if (depth <= 0) return pos;
            continue;
        }
        pos++;
    }
    return end;
}

/* Past the end of this object's line. Records are newline-delimited, so a
 * newline outside a string ends the row. */
/*
 * The end of a row, which for newline-delimited JSON is the next newline byte
 * and nothing subtler.
 *
 * This used to alternate a scan for `\n` or `"` with a walk over each string it
 * landed on, to avoid mistaking a newline inside a quoted value for the end of
 * the row. That cannot happen: RFC 8259 forbids the raw control characters
 * U+0000 to U+001F inside a string, and a newline is U+000A, so a valid JSON
 * string cannot contain one -- it must be written `\n`. The framing of ndjson
 * depends on exactly that.
 *
 * So it is one SWAR scan for one byte, and on a twenty-field row that replaces
 * about twenty string walks. Input that does put a raw newline inside a string
 * is not JSON, and this reader will split the row there -- which is what every
 * ndjson reader does, because the format has no other way to say where a row
 * ends.
 */
static size_t end_of_json_row(const char *d, size_t pos, size_t end) {
    const size_t stop = next_of1(d, pos, end, '\n');
    return stop >= end ? end : stop + 1;
}

static size_t utf8_put(unsigned cp, char *buf, size_t cap, size_t at) {
    unsigned char tmp[4];
    size_t n;
    if (cp < 0x80) { tmp[0] = (unsigned char)cp; n = 1; }
    else if (cp < 0x800) {
        tmp[0] = (unsigned char)(0xC0 | (cp >> 6));
        tmp[1] = (unsigned char)(0x80 | (cp & 0x3F));
        n = 2;
    } else if (cp < 0x10000) {
        tmp[0] = (unsigned char)(0xE0 | (cp >> 12));
        tmp[1] = (unsigned char)(0x80 | ((cp >> 6) & 0x3F));
        tmp[2] = (unsigned char)(0x80 | (cp & 0x3F));
        n = 3;
    } else {
        tmp[0] = (unsigned char)(0xF0 | (cp >> 18));
        tmp[1] = (unsigned char)(0x80 | ((cp >> 12) & 0x3F));
        tmp[2] = (unsigned char)(0x80 | ((cp >> 6) & 0x3F));
        tmp[3] = (unsigned char)(0x80 | (cp & 0x3F));
        n = 4;
    }
    for (size_t i = 0; i < n; i++)
        if (buf && at + i < cap) buf[at + i] = (char)tmp[i];
    return n;
}

static int hex_digit(char h) {
    if (h >= '0' && h <= '9') return h - '0';
    if (h >= 'a' && h <= 'f') return h - 'a' + 10;
    if (h >= 'A' && h <= 'F') return h - 'A' + 10;
    return -1;
}

/*
 * Decodes a JSON string body, returning the decoded length and writing the
 * first `cap` bytes of it when `buf` is given. One function rather than a
 * length pass and a copy pass, so the two can never disagree about what a value
 * is.
 *
 * `\uXXXX` becomes UTF-8 so that a value written escaped and the same value
 * written literally compare equal -- which they must, because a JSON writer is
 * free to escape either way.
 */
static size_t json_unescape(const char *p, size_t len, char *buf, size_t cap) {
    size_t out = 0;
    for (size_t i = 0; i < len; i++) {
        if (p[i] != '\\' || i + 1 >= len) {
            if (buf && out < cap) buf[out] = p[i];
            out++;
            continue;
        }
        char e = p[++i];
        char plain = 0;
        switch (e) {
            case 'n': plain = '\n'; break;
            case 't': plain = '\t'; break;
            case 'r': plain = '\r'; break;
            case 'b': plain = '\b'; break;
            case 'f': plain = '\f'; break;
            case '"': plain = '"'; break;
            case '\\': plain = '\\'; break;
            case '/': plain = '/'; break;
            default: break;
        }
        if (plain) {
            if (buf && out < cap) buf[out] = plain;
            out++;
            continue;
        }
        if (e != 'u' || i + 4 >= len) {          /* not an escape we know: as written */
            if (buf && out < cap) buf[out] = '\\';
            out++;
            if (buf && out < cap) buf[out] = e;
            out++;
            continue;
        }
        unsigned cp = 0;
        bool ok = true;
        for (int k = 1; k <= 4 && ok; k++) {
            int dg = hex_digit(p[i + (size_t)k]);
            if (dg < 0) ok = false;
            else cp = cp * 16 + (unsigned)dg;
        }
        if (!ok) {                               /* not four hex digits: as written */
            if (buf && out < cap) buf[out] = '\\';
            out++;
            if (buf && out < cap) buf[out] = e;
            out++;
            continue;
        }
        i += 4;
        /* A surrogate pair is one code point written as two escapes. */
        if (cp >= 0xD800 && cp <= 0xDBFF && i + 6 < len && p[i + 1] == '\\' && p[i + 2] == 'u') {
            unsigned lo = 0;
            bool ok2 = true;
            for (int k = 3; k <= 6 && ok2; k++) {
                int dg = hex_digit(p[i + (size_t)k]);
                if (dg < 0) ok2 = false;
                else lo = lo * 16 + (unsigned)dg;
            }
            if (ok2 && lo >= 0xDC00 && lo <= 0xDFFF) {
                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                i += 6;
            }
        }
        out += utf8_put(cp, buf, cap, out);
    }
    return out;
}

/*
 * A field's logical length: the raw span with the second quote of each doubled
 * pair dropped. Equality, hashing and printing all read a field through the same
 * two helpers, so they cannot disagree about its value.
 */
static size_t logical_len(const Slab *s, Field f) {
    if (!field_real(f)) return 0;
    size_t len = field_len(f);
    if (!field_escaped(f)) return len;
    const char *p = s->data + field_off(f);
    if (s->dialect == DIALECT_JSON) return json_unescape(p, len, NULL, 0);
    size_t n = 0;
    for (size_t i = 0; i < len; i++) {
        n++;
        if (p[i] == '"' && i + 1 < len && p[i + 1] == '"') i++;
    }
    return n;
}

/* Copies the logical bytes into buf, returning the count written. */
static size_t logical_copy(const Slab *s, Field f, char *buf, size_t cap) {
    if (!field_real(f)) return 0;
    size_t len = field_len(f);
    const char *p = s->data + field_off(f);
    if (!field_escaped(f)) {
        size_t n = len < cap ? len : cap;
        memcpy(buf, p, n);
        return n;
    }
    if (s->dialect == DIALECT_JSON) {
        size_t n = json_unescape(p, len, buf, cap);
        return n < cap ? n : cap;
    }
    size_t n = 0;
    for (size_t i = 0; i < len && n < cap; i++) {
        buf[n++] = p[i];
        if (p[i] == '"' && i + 1 < len && p[i + 1] == '"') i++;
    }
    return n;
}

static bool same_bytes(const Slab *a, Field x, const Slab *b, Field y) {
    bool ex = field_real(x) && field_escaped(x);
    bool ey = field_real(y) && field_escaped(y);
    if (!ex && !ey) {
        size_t lx = field_len(x), ly = field_len(y);
        if (!field_real(x)) lx = 0;
        if (!field_real(y)) ly = 0;
        return lx == ly && memcmp(a->data + field_off(x), b->data + field_off(y), lx) == 0;
    }
    if (logical_len(a, x) != logical_len(b, y)) return false;
    /* Rare: only an escaped field reaches here -- a doubled quote in CSV, a
     * backslash in JSON. */
    size_t n = logical_len(a, x);
    char sx[4096], sy[4096];
    if (n > sizeof sx) return false; /* refused upstream by the length cap */
    logical_copy(a, x, sx, sizeof sx);
    logical_copy(b, y, sy, sizeof sy);
    return memcmp(sx, sy, n) == 0;
}

/*
 * Field equality under a Norm: --trim and --ignore-case normalise the bytes
 * before comparing; --tolerance applies where both sides parse as numbers.
 */
static size_t norm_copy(const Slab *s, Field f, const Norm *n, char *out, size_t cap);

/*
 * A value as a number, for --tolerance, by the rule every port uses: whitespace
 * around it is ignored, and what is left is an optional sign, then a digit or a
 * point, then only digits, points, exponents and signs -- which is stricter than
 * strtod, so "inf", "nan" and "0x10" stay text -- and it must read to a finite
 * double in full.
 */
static bool as_number(const Slab *s, Field f, double *out) {
    char buf[4096];
    const size_t len = logical_len(s, f);
    if (len >= sizeof buf) return false;
    logical_copy(s, f, buf, sizeof buf);
    size_t lo = 0, hi = len;
    while (lo < hi && (unsigned char)buf[lo] <= ' ') lo++;
    while (hi > lo && (unsigned char)buf[hi - 1] <= ' ') hi--;
    if (lo == hi) return false;
    size_t body = lo;
    if (buf[body] == '+' || buf[body] == '-') body++;
    if (body == hi || !(isdigit((unsigned char)buf[body]) || buf[body] == '.')) return false;
    for (size_t i = body; i < hi; i++) {
        const char c = buf[i];
        if (!(isdigit((unsigned char)c) || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-'))
            return false;
    }
    buf[hi] = '\0';
    char *end;
    const double v = strtod(buf + lo, &end);
    if (end != buf + hi || !isfinite(v)) return false;
    *out = v;
    return true;
}

/* Field equality under the options, for two values the caller knows are both
 * present. Out of line: the path that uses no option never gets here. */
static bool same_norm_slow(const Slab *a, Field x, const Slab *b, Field y, const Norm *n) {
    if (n->tolerance > 0.0) {
        double dx, dy;
        if (as_number(a, x, &dx) && as_number(b, y, &dy)) return fabs(dx - dy) <= n->tolerance;
    }
    /* Identical bytes are equal under any option, and deciding so before the
     * fold is what keeps an unchanged value outside ASCII from being refused. */
    if (same_bytes(a, x, b, y)) return true;
    if (!n->trim && !n->ignore_case) return false;
    char nx[4096], ny[4096];
    const size_t lx = norm_copy(a, x, n, nx, sizeof nx);
    const size_t ly = norm_copy(b, y, n, ny, sizeof ny);
    if (lx == (size_t)-1 || ly == (size_t)-1) {
        /* The fold would decide this, and this port does not fold outside ASCII. */
        atomic_store_explicit(&fold_refused, true, memory_order_relaxed);
        return false;
    }
    return lx == ly && memcmp(nx, ny, lx) == 0;
}

static inline bool same_norm(const Slab *a, Field x, const Slab *b, Field y, const Norm *n) {
    if (!n) return same_bytes(a, x, b, y);
    return same_norm_slow(a, x, b, y, n);
}

/* Under --trim and --empty-is-null, a value that is only whitespace is absent
 * as well; under anything else it is a value. Out of line for the same reason
 * as same_norm_slow. */
static bool blank_is_absent(const Slab *s, Field f) {
    char buf[4096];
    const size_t len = logical_len(s, f);
    if (len >= sizeof buf) return false;
    logical_copy(s, f, buf, sizeof buf);
    for (size_t i = 0; i < len; i++)
        if ((unsigned char)buf[i] > ' ') return false;
    return true;
}

static inline bool absent_norm(const Slab *s, Field f, const Norm *n) {
    if (!field_real(f) || field_len(f) == 0) return true;
    return n && n->trim && n->empty_is_null && blank_is_absent(s, f);
}

/*
 * The last `rem` bytes of p[0..len), one to seven of them, as a little-endian
 * word padded with zeros -- from loads rather than a copy into a zeroed buffer,
 * which is a call to memcpy for a length only known at run time. A value of
 * eight bytes or more has eight real bytes ending where the tail does, so one
 * load and a shift; a shorter one is two overlapping halves, or its first,
 * middle and last byte. The Rust and Zig ports read their tails the same way.
 */
static inline uint64_t tail_word(const char *p, size_t len, size_t rem) {
    uint64_t w;
    if (len >= 8) {
        memcpy(&w, p + len - 8, 8);
        return w >> (8 * (8 - rem));
    }
    if (len >= 4) {
        uint32_t lo, hi;
        memcpy(&lo, p, 4);
        memcpy(&hi, p + len - 4, 4);
        return (uint64_t)lo | ((uint64_t)hi << (8 * (len - 4)));
    }
    const unsigned char *u = (const unsigned char *)p;
    return (uint64_t)u[0] | ((uint64_t)u[len / 2] << (8 * (len / 2))) |
           ((uint64_t)u[len - 1] << (8 * (len - 1)));
}

/*
 * Normalises a field's bytes into `out`: unescapes if needed, then applies
 * --trim (strip bytes <= ' ') and --ignore-case (ASCII fold A-Z to a-z).
 * Returns the normalised length, or (size_t)-1 if --ignore-case meets a byte
 * outside ASCII; the caller raises the refusal.
 */
static size_t norm_copy(const Slab *s, Field f, const Norm *n, char *out, size_t cap) {
    char tmp[4096];
    const char *p;
    size_t len;
    if (field_escaped(f)) {
        len = logical_copy(s, f, tmp, sizeof tmp);
        p = tmp;
    } else {
        p = s->data + field_off(f);
        len = field_len(f);
    }
    /* --trim: strip leading/trailing whitespace (bytes <= ' '). */
    if (n && n->trim) {
        while (len > 0 && (unsigned char)p[0] <= ' ') { p++; len--; }
        while (len > 0 && (unsigned char)p[len - 1] <= ' ') len--;
    }
    if (len > cap) len = cap; /* refused upstream by the length cap */
    if (n && n->ignore_case) {
        for (size_t i = 0; i < len; i++) {
            unsigned char c = (unsigned char)p[i];
            if (c >= 0x80) return (size_t)-1; /* non-ASCII: refused */
            out[i] = (c >= 'A' && c <= 'Z') ? (char)(c + 32) : (char)c;
        }
    } else {
        memcpy(out, p, len);
    }
    return len;
}

/*
 * The key hash, over exactly the bytes equality compares: a field's logical
 * value, whatever escaping it arrived in, so that two spellings of one value
 * hash alike.
 *
 * It took a byte at a time, one dependent multiply per byte, which on this
 * workload's two keys of twelve and fifteen bytes was 1.04B instructions of the
 * 8.5B a 2M CSV pair costs. Eight bytes a step is what the Rust and Zig ports
 * have done all along; an escaped value is decoded first -- the rare case, a
 * doubled quote or a backslash -- and folded by the same loop.
 */
static uint64_t hash_field_norm(const Slab *s, Field f, uint64_t seed, const Norm *n) {
    const uint64_t PRIME = UINT64_C(0x100000001b3);
    uint64_t h = seed;
    if (absent_norm(s, f, n)) return (h ^ UINT64_C(0x9e3779b97f4a7c15)) * PRIME;
    /* Fast path: no normalisation, no escaping -- hash the bytes in place. */
    if ((!n || (!n->trim && !n->ignore_case)) && !field_escaped(f)) {
        const char *p = s->data + field_off(f);
        size_t len = field_len(f);
        size_t i = 0;
        for (; i + 8 <= len; i += 8) {
            uint64_t w;
            memcpy(&w, p + i, 8);
            h = (h ^ w) * PRIME;
            h ^= h >> 29;
        }
        if (i < len) {
            h = (h ^ tail_word(p, len, len - i)) * PRIME;
            h ^= h >> 29;
        }
        return (h ^ (uint64_t)len) * PRIME;
    }
    /* Slow path: normalise into a buffer, then hash it. */
    char buf[4096];
    size_t len = norm_copy(s, f, n, buf, sizeof buf);
    if (len == (size_t)-1) {
        /* A key outside ASCII under --ignore-case: its fold is its identity. */
        atomic_store_explicit(&fold_refused, true, memory_order_relaxed);
        return 0;
    }
    size_t i = 0;
    for (; i + 8 <= len; i += 8) {
        uint64_t w;
        memcpy(&w, buf + i, 8);
        h = (h ^ w) * PRIME;
        h ^= h >> 29;
    }
    if (i < len) {
        h = (h ^ tail_word(buf, len, len - i)) * PRIME;
        h ^= h >> 29;
    }
    return (h ^ (uint64_t)len) * PRIME;
}

/* ------------------------------------------------------------------------- */
/* Parsing                                                                     */
/* ------------------------------------------------------------------------- */

/*
 * One entry of the JSON name table: the wanted name's hash and length ride
 * along with its slot, so a probe rejects a different name on one compare and
 * reads the name's bytes only when it is almost certainly the one. Without
 * them every lookup took a `strlen` of the candidate before comparing -- twenty
 * a row, more than the hash.
 */
typedef struct {
    uint64_t hash;
    uint32_t len;
    int32_t  idx;   /* the slot, or -1 for an empty entry */
} NameSlot;

typedef struct {
    char delimiter;
    int *source;   /* CSV: where each projected column sits in the file, or -1 */
    size_t width;
    int last_needed;
    Dialect dialect;
    /*
     * The key columns are the first `key_size` slots, and most of the parsing
     * this program does wants only those: the sweep hashes them, and a probe
     * that lands on a matching hash confirms them. `key_last` is the file
     * column the last of them sits at, so a key-only parse of a twenty-column
     * row delimits two fields and then scans once to the newline.
     */
    size_t key_size;
    int key_last;
    /*
     * CSV addresses a value by column number, and the projection has to get
     * from that number to the slot it fills. Scanning `source` for it cost
     * `width` comparisons per column and so `width * width` per row -- four
     * hundred at twenty columns, which is the same four hundred the JSON path
     * has a hash table to avoid. `col_first[c]` is the first slot fed by file
     * column c and `slot_next` chains the rest, so a column costs one load.
     */
    int32_t *col_first;
    int32_t *slot_next;
    /*
     * JSON addresses a value by key where CSV addresses it by column number, so
     * the wanted names live here. `want[i]` is the key whose value belongs in
     * slot i, or NULL for a column this file does not have. `slot` is a small
     * open-addressed table from name to slot, so a key in the file costs one
     * hash rather than a walk of twenty names -- which at twenty columns would
     * be four hundred comparisons a row.
     */
    char **want;
    NameSlot *slot;
    size_t slot_mask;
    /*
     * Names arrive in the same order row after row, and usually in slot order,
     * so the member after slot i is most often slot i + 1. `want_len` lets a
     * guess be checked with one compare and a `memcmp`, and `want_slot[i]` is
     * what the table answers for `want[i]` -- slot i itself unless the same
     * name also fills an earlier slot -- so a right guess gives exactly the
     * slot the table would have. `want_n` is how many there are.
     */
    uint32_t *want_len;
    int32_t  *want_slot;
    size_t    want_n;
} RowParser;

/*
 * Inverts `source` into the column-to-slot chains. One file column can in
 * principle feed several slots, so this is a chain rather than an array, and it
 * is built back to front so those slots are visited in slot order -- the order
 * the scan it replaces filled them in. This port's flags cannot produce that
 * case today (the compared set is the common columns minus the keys, so a
 * column is never both), which is precisely why the chain is here rather than
 * an assumption that it cannot happen.
 */
static bool parser_index_columns(RowParser *p) {
    const size_t n = p->last_needed >= 0 ? (size_t)p->last_needed + 1 : 1;
    p->col_first = malloc(n * sizeof *p->col_first);
    p->slot_next = malloc((p->width ? p->width : 1) * sizeof *p->slot_next);
    if (!p->col_first || !p->slot_next) return false;
    for (size_t i = 0; i < n; i++) p->col_first[i] = -1;
    for (size_t i = p->width; i-- > 0;) {
        p->slot_next[i] = -1;
        if (p->source[i] < 0 || p->source[i] > p->last_needed) continue;
        p->slot_next[i] = p->col_first[p->source[i]];
        p->col_first[p->source[i]] = (int32_t)i;
    }
    return true;
}

/*
 * Whether two names of length n are the same bytes, inline and a word at a
 * time. The JSON parse checks its guessed slot for every member of every row,
 * and as a `memcmp` of run-time length that was a libc call each time -- 48M
 * calls on a 2M ndjson pair. Names are short, so the words overlap rather than
 * loop far.
 */
static inline bool same_name(const char *a, const char *b, size_t n) {
    if (n >= 8) {
        size_t i = 0;
        for (; i + 8 < n; i += 8)
            if (load64(a + i) != load64(b + i)) return false;
        return load64(a + n - 8) == load64(b + n - 8);
    }
    if (n >= 4) {
        uint32_t a0, a1, b0, b1;
        memcpy(&a0, a, 4);
        memcpy(&b0, b, 4);
        memcpy(&a1, a + n - 4, 4);
        memcpy(&b1, b + n - 4, 4);
        return a0 == b0 && a1 == b1;
    }
    for (size_t i = 0; i < n; i++)
        if (a[i] != b[i]) return false;
    return true;
}

static uint64_t name_hash(const char *p, size_t n) {
    uint64_t a = 0, b = 0;
    if (n >= 8) {
        memcpy(&a, p, 8);
        memcpy(&b, p + n - 8, 8);
    } else if (n >= 4) {
        uint32_t x, y;
        memcpy(&x, p, 4);
        memcpy(&y, p + n - 4, 4);
        a = x;
        b = y;
    } else if (n > 0) {
        a = (uint64_t)(unsigned char)p[0] | (uint64_t)(unsigned char)p[n / 2] << 8 |
            (uint64_t)(unsigned char)p[n - 1] << 16;
    }
    uint64_t h = (a ^ (b * UINT64_C(0x9e3779b97f4a7c15)) ^ n) * UINT64_C(0xbf58476d1ce4e5b9);
    return h ^ (h >> 31);
}

/* Builds the name-to-slot table. `want` is borrowed, not owned. */
static int parser_slot_for(const RowParser *p, const char *key, size_t len);

static bool parser_index_names(RowParser *p, char **want, size_t width) {
    p->want = want;
    size_t n = 16;
    while (n < width * 4) n <<= 1;
    p->slot = malloc(n * sizeof *p->slot);
    if (!p->slot) return false;
    for (size_t i = 0; i < n; i++) p->slot[i] = (NameSlot){ 0, 0, -1 };
    p->slot_mask = n - 1;
    for (size_t i = 0; i < width; i++) {
        if (!want[i]) continue;
        const size_t len = strlen(want[i]);
        if (len > UINT32_MAX) return false;
        const uint64_t h = name_hash(want[i], len);
        size_t at = h & p->slot_mask;
        while (p->slot[at].idx >= 0) at = (at + 1) & p->slot_mask;
        p->slot[at] = (NameSlot){ h, (uint32_t)len, (int32_t)i };
    }
    p->want_len = malloc((width ? width : 1) * sizeof *p->want_len);
    p->want_slot = malloc((width ? width : 1) * sizeof *p->want_slot);
    if (!p->want_len || !p->want_slot) return false;
    p->want_n = width;
    for (size_t i = 0; i < width; i++) {
        p->want_len[i] = want[i] ? (uint32_t)strlen(want[i]) : 0;
        p->want_slot[i] = want[i] ? (int32_t)parser_slot_for(p, want[i], p->want_len[i]) : -1;
    }
    return true;
}

static int parser_slot_for(const RowParser *p, const char *key, size_t len) {
    const uint64_t h = name_hash(key, len);
    size_t at = h & p->slot_mask;
    for (;;) {
        const NameSlot *e = &p->slot[at];
        if (e->idx < 0) return -1;
        if (e->hash == h && e->len == len && same_name(p->want[e->idx], key, len))
            return (int)e->idx;
        at = (at + 1) & p->slot_mask;
    }
}

static size_t end_of_row(const char *d, size_t pos, size_t end) {
    size_t at = pos;
    while (at < end) {
        size_t next = next_of2(d, at, end, '\n', '"');
        if (next >= end) return end;
        if (d[next] == '"') { at = skip_quoted(d, next + 1, end); continue; }
        return next;
    }
    return end;
}

/*
 * Parses one row into out, returning the offset of the next row. Once the last
 * needed column has been read the rest of the row is skipped to its newline: on
 * twenty columns keyed on the first two, most of a row is never delimited.
 */
/*
 * Walks one JSON object, keeping the values of the keys wanted. One pass over
 * the object, one hash per key it holds -- not a search per wanted column.
 */
static size_t parse_json_row(const RowParser *p, const char *d, size_t start, size_t end,
                             Field *out, size_t slots, bool to_end) {
    for (size_t i = 0; i < slots; i++) out[i] = ABSENT;
    size_t found = 0;   /* key slots filled, for the early exit below */
    size_t guess = 0;   /* the slot the next member most likely fills */
    size_t pos = start;
    while (pos < end && json_space(d[pos])) pos++;
    if (pos >= end) return end;
    if (d[pos] != '{') return end_of_json_row(d, pos, end);   /* not an object: skip the line */
    pos++;

    for (;;) {
        /* Compact ndjson separates members with a bare `,"`, which goes
         * straight to the next key; the general path takes the comma, loops
         * back and skips space twice to get there. */
        if (pos + 1 < end && d[pos] == ',' && d[pos + 1] == '"') {
            pos++;
        } else {
            while (pos < end && json_space(d[pos])) pos++;
            if (pos >= end) break;
            if (d[pos] == '}') { pos++; break; }
            if (d[pos] == ',') { pos++; continue; }
            if (d[pos] != '"') break;                         /* malformed: stop this object */
        }

        bool key_escaped = false;
        size_t key_from = pos + 1;
        size_t key_end = skip_json_string(d, pos, end, &key_escaped);
        if (key_end > end || key_end < 2 || key_end - 1 < key_from) break;
        size_t key_len = key_end - 1 - key_from;
        pos = key_end;
        /* And `":"` between a key and a string value, likewise. */
        if (pos + 1 < end && d[pos] == ':' && d[pos + 1] == '"') {
            pos++;
        } else {
            while (pos < end && json_space(d[pos])) pos++;
            if (pos >= end || d[pos] != ':') break;
            pos++;
            while (pos < end && json_space(d[pos])) pos++;
            if (pos >= end) break;
        }

        size_t from, to;
        bool escaped = false, absent = false;
        if (d[pos] == '"') {
            from = pos + 1;
            size_t close = skip_json_string(d, pos, end, &escaped);
            to = close > pos + 1 ? close - 1 : pos + 1;
            pos = close;
        } else if (d[pos] == '{' || d[pos] == '[') {
            /* Nested: not a cell value, and left absent rather than guessed at. */
            from = to = pos;
            pos = skip_json_nested(d, pos, end);
            absent = true;
        } else {
            /* A number, true, false or null: to the next comma, brace or space. */
            from = pos;
            while (pos < end && d[pos] != ',' && d[pos] != '}' && !json_space(d[pos])) pos++;
            to = pos;
            absent = (to - from == 4 && memcmp(d + from, "null", 4) == 0);
        }
        if (!absent) {
            int slot;
            if (guess < p->want_n && p->want[guess] && p->want_len[guess] == key_len &&
                same_name(p->want[guess], d + key_from, key_len))
                slot = p->want_slot[guess];
            else
                slot = parser_slot_for(p, d + key_from, key_len);
            if (slot >= 0) guess = (size_t)slot + 1;
            if (slot >= 0 && (size_t)slot < slots) {
                /*
                 * First occurrence wins for a key column, and only for a key
                 * column. A JSON object is not supposed to repeat a name and
                 * this generator never does, but the rule has to be *stated*
                 * rather than incidental, because two parses of the same row
                 * have to agree on what its key is: the key-only parse below
                 * stops as soon as it has the keys, and a last-wins rule would
                 * let it stop on a different value than the full parse ends
                 * with, which is a lookup that misses its own row. Compared
                 * columns keep last-wins, which is what the C++ port does and
                 * what test.sh cross-checks against.
                 */
                if ((size_t)slot < p->key_size) {
                    if (out[slot] == ABSENT) {
                        out[slot] = pack(from, to - from, escaped);
                        /* Every key found and nothing else wanted: the rest of
                         * the object is bytes to skip, not fields to parse. */
                        if (++found == slots) {
                            if (!to_end) return pos;
                            break;
                        }
                    }
                } else {
                    out[slot] = pack(from, to - from, escaped);
                }
            }
        }
    }
    return end_of_json_row(d, pos, end);
}

static size_t parse_csv_row(const RowParser *p, const char *d, size_t start, size_t end,
                            Field *out, int last_needed, size_t slots, bool to_end) {
    for (size_t i = 0; i < slots; i++) out[i] = ABSENT;
    size_t pos = start;
    int column = 0;
    Delims delims;
    delims_init(&delims, d, start, end, p->delimiter, '\n');

    while (pos <= end) {
        Field field;
        size_t next;
        if (pos < end && d[pos] == '"') {
            size_t close = skip_quoted(d, pos + 1, end);
            size_t body_end = close > pos + 1 ? close - 1 : pos + 1;
            next = delims_next(&delims, close);
            field = pack(pos + 1, body_end - (pos + 1),
                         next_of1(d, pos + 1, body_end, '"') < body_end);
        } else {
            next = delims_next(&delims, pos);
            size_t stop = next;
            if (stop > pos && d[stop - 1] == '\r') stop--; /* CRLF behaves like LF */
            field = pack(pos, stop - pos, false);
        }
        if (column <= last_needed)
            for (int32_t sl = p->col_first[column]; sl >= 0; sl = p->slot_next[sl])
                if ((size_t)sl < slots) out[sl] = field;
        column++;

        if (next >= end) return end;
        if (d[next] == '\n') return next + 1;
        pos = next + 1;
        if (column > last_needed) {
            if (!to_end) return pos;
            size_t eol = end_of_row(d, pos, end);
            return eol >= end ? end : eol + 1;
        }
    }
    return end;
}

static size_t parse_row(const RowParser *p, const char *d, size_t start, size_t end, Field *out) {
    if (p->dialect == DIALECT_JSON) return parse_json_row(p, d, start, end, out, p->width, true);
    return parse_csv_row(p, d, start, end, out, p->last_needed, p->width, true);
}

/*
 * The key columns alone, into the first `key_size` slots. Everything that only
 * needs a key -- the sweep, and the probe that confirms a hash match -- goes
 * through this rather than parsing twenty columns to look at two.
 */
static size_t parse_keys(const RowParser *p, const char *d, size_t start, size_t end,
                         Field *out) {
    if (p->dialect == DIALECT_JSON) return parse_json_row(p, d, start, end, out, p->key_size, true);
    return parse_csv_row(p, d, start, end, out, p->key_last, p->key_size, true);
}

/*
 * The key columns alone, for a row whose end is already known: the probe that
 * confirms a hash match. It stops at the last key rather than walking the rest
 * of the row to its newline for an offset nobody reads -- which, on a row of
 * twenty columns keyed on the first two, was most of the bytes the probe
 * touched. The return value is not a row boundary.
 */
static void probe_keys(const RowParser *p, const char *d, size_t start, size_t end,
                       Field *out) {
    if (p->dialect == DIALECT_JSON) (void)parse_json_row(p, d, start, end, out, p->key_size, false);
    else (void)parse_csv_row(p, d, start, end, out, p->key_last, p->key_size, false);
}

/* ------------------------------------------------------------------------- */
/* Phase timings                                                               */
/*                                                                             */
/* On stderr when CSVDIFF_PHASES is set, the same shape pqdiff.c prints. The    */
/* text path has three costs that move independently -- finding and hashing the */
/* rows, inserting them in order, and the join that re-reads both sides -- and  */
/* until this existed the only way to tell which had grown was to guess.        */
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
/* The index: open addressing over primitive arrays                            */
/* ------------------------------------------------------------------------- */

typedef struct {
    const Slab *slab;
    const RowParser *parser;
    size_t key_size;
    uint64_t *row_start;
    uint64_t *row_hash;
    size_t rows, rows_cap;
    /*
     * Tag in the slot. A slot is empty (0) or holds a key index in its low
     * `pos_bits` and the top bits of that key's hash above them, so a probe
     * that lands on the wrong key is rejected by the word it has already
     * loaded. Without it, rejecting a collision costs two more dependent loads
     * -- `first_row[at]`, then `row_hash[candidate]` -- each a miss on an array
     * far too big to cache, and each waiting on the one before it.
     *
     * The width is chosen from the row count rather than fixed, so the slot
     * stays four bytes and the table stays the size it was: at ten million rows
     * the index needs 24 bits and the tag gets the other 8. A tag that runs out
     * of bits (past two billion rows) degrades to no tag, not to a wrong
     * answer, because the key comparison behind it is unchanged.
     */
    uint32_t *table;
    unsigned  pos_bits;
    uint32_t  pos_mask;
    size_t mask;
    int32_t *first_row;
    uint32_t *occurrences;
    size_t keys, keys_cap;
    Field *probe;      /* re-used by every lookup, so a probe is not an allocation */
    Field *probe2;     /* the other side of a lazy equality check, same reason */
    int64_t dup_keys, dup_rows;
    /* Rows that repeat a key already seen, in file order -- only when the
     * build was given a sink, which joined them as if each were its key's
     * first and needs them to take that back. */
    int32_t *later;
    size_t n_later, later_cap;
    bool failed;       /* a field too long for the packed length */
    const Norm *norm;  /* text normalisation for key comparison; NULL for none */
} RowIndex;

#define TABLE_EMPTY 0u

static inline uint32_t slot_pack(const RowIndex *ix, uint64_t hash, size_t pos) {
    const unsigned tag_bits = 32u - ix->pos_bits;
    const uint32_t tag = tag_bits ? (uint32_t)(hash >> (64u - tag_bits)) : 0u;
    return (tag << ix->pos_bits) | (uint32_t)(pos + 1);
}

static inline int slot_tag_is(const RowIndex *ix, uint32_t v, uint64_t hash) {
    const unsigned tag_bits = 32u - ix->pos_bits;
    if (!tag_bits) return 1;
    return (v >> ix->pos_bits) == (uint32_t)(hash >> (64u - tag_bits));
}

static inline size_t slot_pos(const RowIndex *ix, uint32_t v) {
    return (size_t)(v & ix->pos_mask) - 1;
}

static size_t slot_of(const RowIndex *ix, uint64_t hash) {
    /* The high bits of an FNV hash are the well-mixed ones; fold them down. */
    return (size_t)((hash ^ (hash >> 32)) & ix->mask);
}

static void index_fields(const RowIndex *ix, int32_t row, Field *out) {
    parse_row(ix->parser, ix->slab->data, (size_t)ix->row_start[row], ix->slab->size, out);
}

static void index_keys(const RowIndex *ix, int32_t row, Field *out) {
    probe_keys(ix->parser, ix->slab->data, (size_t)ix->row_start[row], ix->slab->size, out);
}

/* ------------------------------------------------------------------------- */
/* Finding the rows, on every core                                             */
/* ------------------------------------------------------------------------- */

/* How many rows ahead to start the load for. See pqdiff.c, where the same
 * constant was measured against the same kind of table. */
#define PREFETCH_AHEAD 24

/* Below this there is nothing worth dividing: the boundary work would cost more
 * than the parsing it splits. */
#define SPLIT_FROM (4u << 20)

/* One chunk's rows, in the order they appear in it.
 *
 * Padded for the same reason as `CmpPart`: these are one per sweep thread in a
 * single `calloc`ed array, forty bytes apart, and `chunk_push` touches `n`,
 * `cap` and the two pointers on *every row*. Unpadded, two threads' chunks
 * share a cache line and every row one of them appends moves it. */
typedef struct {
    _Alignas(64)
    uint64_t *start;
    uint64_t *hash;
    size_t    n, cap;
    size_t    stopped;  /* where the sweep left off: the first row start past the chunk */
    bool      failed;   /* a field too long for the packed length */
    bool      oom;
    char      pad[64];
} Chunk;

/*
 * Where each chunk begins, as offsets of real row starts.
 *
 * Walking forward from a nominal split is the whole difficulty: a newline inside
 * a quoted field is not a row boundary, and a thread starting mid-file cannot
 * tell whether it is inside such a field. Parity settles it. Every `"` toggles
 * in-quote state -- including both halves of a doubled quote, which toggles
 * twice and so leaves the state alone, which is exactly right -- so the number
 * of quotes before a position says whether that position is inside a field.
 * Counting them is a scan for one byte, far cheaper than parsing.
 *
 * JSON needs none of that: a raw newline inside a string is not valid JSON, so
 * every newline ends a record. Counting anyway is not only wasted -- ndjson
 * quotes every key and most values, so the count stops every few bytes, and on
 * a 2M-row pair it took about 0.47s a side, serially, before any thread started
 * -- it is also wrong: an escaped `\"` toggles the parity, and a split whose
 * count comes out odd can walk to the end of the file looking for a newline
 * outside quotes, and the chunk is dropped.
 *
 * With `count_quotes` false, CSV takes the next newline as well, which is a
 * guess; the caller checks it after the sweep (see `index_build`) and counts
 * only when a guess was wrong.
 */
static unsigned chunk_bounds(const Slab *s, size_t from, unsigned threads, size_t *bounds,
                             bool count_quotes) {
    const char *d = s->data;
    const size_t end = s->size;
    if (threads <= 1 || end - from < SPLIT_FROM) {
        bounds[0] = from;
        bounds[1] = end;
        return 1;
    }
    unsigned n = 0;
    bounds[n++] = from;
    /*
     * The count is carried forward rather than restarted.
     *
     * Counting from `from` to each split in turn reads the first slice of the
     * file for every split, the second for all but one, and so on: (threads-1)/2
     * passes over the file, all of them here, to prepare a sweep whose whole
     * purpose is to use the other cores. Only the slice since the last split is
     * new, so counting that and adding it keeps a running total of the quotes
     * before `nominal` for one pass in total.
     */
    const bool json = s->dialect == DIALECT_JSON || !count_quotes;
    size_t quotes = 0;
    size_t counted = from;
    for (unsigned i = 1; i < threads; i++) {
        const size_t nominal = from + (end - from) * i / threads;
        for (size_t at = counted; !json && at < nominal;) {
            const size_t q = next_of1(d, at, nominal, '"');
            if (q >= nominal) break;
            quotes++;
            at = q + 1;
        }
        counted = nominal;
        bool in_quotes = (quotes & 1) != 0;
        size_t at = nominal;
        for (; at < end; at++) {
            if (d[at] == '"' && !json) in_quotes = !in_quotes;
            else if (d[at] == '\n' && !in_quotes) { at++; break; }
        }
        if (at > bounds[n - 1] && at < end) bounds[n++] = at;
    }
    bounds[n] = end;
    return n;
}

static bool chunk_push(Chunk *c, size_t start, uint64_t hash) {
    if (c->n == c->cap) {
        size_t next = c->cap ? c->cap * 2 : 8192;
        uint64_t *st = realloc(c->start, next * sizeof *st);
        if (!st) return false;
        c->start = st;
        uint64_t *hs = realloc(c->hash, next * sizeof *hs);
        if (!hs) return false;
        c->hash = hs;
        c->cap = next;
    }
    c->start[c->n] = start;
    c->hash[c->n] = hash;
    c->n++;
    return true;
}

/*
 * What the sweep hands each row to, when the join runs inside it: the row's
 * bytes, from its start to the next row's, and its key hash. `k` is the row's
 * position in its chunk, for the proof's backoff. `reset` clears a part's
 * counts when a wrong split guess makes the sweep start over.
 */
typedef struct {
    void (*row)(void *ctx, unsigned part, size_t lo, size_t hi, uint64_t hash, size_t k);
    void (*reset)(void *ctx, unsigned part);
    void *ctx;
} RowSink;

typedef struct {
    const Slab      *slab;
    const RowParser *parser;
    size_t           key_size;
    const size_t    *bounds;
    Chunk           *chunk;
    const Norm      *norm;
    const RowSink   *sink;
    /* Per-thread timing, nanoseconds. Aggregated after the join. */
    uint64_t        *parse_ns;
    uint64_t        *hash_ns;
    uint64_t        *push_ns;
    uint64_t        *rows;
} SweepCtx;

/*
 * Parses and hashes every row that *starts* in this chunk, running past its end
 * to finish the last one. This is the work worth splitting: it reads the file
 * and writes only its own chunk, so any number of threads may be inside it.
 *
 * When CSVDIFF_PHASES is set, each thread records where its nanoseconds go --
 * parsing, hashing, pushing -- so a slow sweep can be blamed on the right part.
 */
static inline uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static void sweep_part(void *vctx, unsigned p) {
    SweepCtx *c = vctx;
    Chunk *out = &c->chunk[p];
    const char *d = c->slab->data;
    const size_t end = c->slab->size, stop = c->bounds[p + 1];
    Field *fields = malloc(c->parser->width * sizeof *fields);
    if (!fields) { out->oom = true; return; }

    const bool timed = c->parse_ns != NULL;
    uint64_t t_parse = 0, t_hash = 0, t_push = 0, n_rows = 0;
    uint64_t t0 = timed ? now_ns() : 0;

    size_t pos = c->bounds[p];
    while (pos < stop) {
        if (d[pos] == '\n') { pos++; continue; }            /* an empty line is not a row */
        if (d[pos] == '\r' && pos + 1 < end && d[pos + 1] == '\n') { pos += 2; continue; }
        if (timed) t0 = now_ns();
        const size_t next = parse_keys(c->parser, d, pos, end, fields);
        for (size_t i = 0; i < c->key_size; i++)
            if (fields[i] == TOO_LONG) out->failed = true;
        /*
         * A field can only be longer than the packed length if its row is, and
         * a row that long is a once-in-a-file event -- so the columns this
         * sweep no longer reads are still checked, exactly rather than
         * conservatively, by re-parsing just those rows in full.
         */
        if (!out->failed && next - pos > MAX_FIELD) {
            parse_row(c->parser, d, pos, end, fields);
            for (size_t i = 0; i < c->parser->width; i++)
                if (fields[i] == TOO_LONG) out->failed = true;
        }
        if (timed) t_parse += now_ns() - t0;
        if (out->failed) break;
        if (timed) t0 = now_ns();
        uint64_t hash = UINT64_C(0xcbf29ce484222325);
        const Norm *n = c->norm;
        for (size_t i = 0; i < c->key_size; i++) hash = hash_field_norm(c->slab, fields[i], hash, n);
        if (timed) { t_hash += now_ns() - t0; t0 = now_ns(); }
        if (!chunk_push(out, pos, hash)) { out->oom = true; break; }
        if (timed) t_push += now_ns() - t0;
        n_rows++;
        /* The row before this one ends where this one starts, which is what
         * the index will say its end is; its pages are still in memory. */
        if (c->sink && out->n >= 2)
            c->sink->row(c->sink->ctx, p, out->start[out->n - 2], pos, out->hash[out->n - 2],
                         out->n - 2);
        if (next <= pos) break; /* no progress: a malformed tail, not an endless loop */
        pos = next;
    }
    out->stopped = pos;
    /* The chunk's last row ends where the next row in the file starts: past
     * the blank lines the next chunk will skip, or at the end of the file. */
    if (c->sink && out->n >= 1 && !out->failed && !out->oom) {
        size_t e = pos;
        while (e < end) {
            if (d[e] == '\n') e++;
            else if (d[e] == '\r' && e + 1 < end && d[e + 1] == '\n') e += 2;
            else break;
        }
        c->sink->row(c->sink->ctx, p, out->start[out->n - 1], e, out->hash[out->n - 1], out->n - 1);
    }
    free(fields);
    if (timed) {
        c->parse_ns[p] = t_parse;
        c->hash_ns[p] = t_hash;
        c->push_ns[p] = t_push;
        c->rows[p] = n_rows;
    }
}

/*
 * Rows are found and hashed on every core; they are inserted on one.
 *
 * The split is not arbitrary. A row's hash depends on nothing but that row, so
 * finding and hashing divides perfectly. Insertion does not: first-occurrence
 * wins, and which occurrence is first depends on the order rows arrive, so
 * threading it would make the answer depend on the scheduler.
 *
 * Two things the serial half does that the old single walk did not. The table
 * is sized once from the row count the sweep just established, where before it
 * started at 4,096 and doubled -- thirteen rehashes at ten million rows, each
 * one a full pass of random probes. And equality is checked lazily: a row's
 * fields are only re-parsed when a probe lands on an occupied slot whose hash
 * matches, which on ten million rows with a thousand duplicate keys is about a
 * thousand parses rather than ten million.
 */
static bool index_build(RowIndex *ix, const Slab *slab, const RowParser *parser, size_t from,
                        size_t key_size, unsigned threads, const Norm *norm,
                        const RowSink *sink) {
    memset(ix, 0, sizeof *ix);
    ix->slab = slab;
    ix->parser = parser;
    ix->key_size = key_size;
    ix->norm = norm;
    ix->probe = malloc(parser->width * sizeof *ix->probe);
    ix->probe2 = malloc(parser->width * sizeof *ix->probe2);
    if (!ix->probe || !ix->probe2) return false;

    if (threads < 1) threads = 1;
    size_t *bounds = malloc((threads + 1) * sizeof *bounds);
    Chunk *chunks = calloc(threads, sizeof *chunks);
    if (!bounds || !chunks) { free(bounds); free(chunks); return false; }

    Phases ph;
    phases_init(&ph);
    /* Per-thread sweep timings, only when the phase breakdown is on. */
    const bool timed = ph.on;
    uint64_t *t_parse = timed ? calloc(threads, sizeof *t_parse) : NULL;
    uint64_t *t_hash = timed ? calloc(threads, sizeof *t_hash) : NULL;
    uint64_t *t_push = timed ? calloc(threads, sizeof *t_push) : NULL;
    uint64_t *t_rows = timed ? calloc(threads, sizeof *t_rows) : NULL;
    SweepCtx sc = { slab, parser, key_size, bounds, chunks, norm, sink,
                    t_parse, t_hash, t_push, t_rows };

    /*
     * The CSV splits are guessed, then checked.
     *
     * Counted bounds cost a pass over the front of the file -- (threads-1)/threads
     * of it -- on one thread, before any chunk starts. In memory that is a quick
     * scan for one byte. On an input larger than memory it is a disk read of most
     * of the file that the sweep then reads again, because by the time the sweep
     * gets there those pages are gone: on the 150M CI pair, 26 GB a side, about
     * 138 seconds before the sweep began, against a sweep of 230.
     *
     * So each split is taken at the next newline, as JSON's are, and checked the
     * way the Rust port checks it. Chunk 0 starts at a real row, so the last row
     * it parses ends at the first real row start past its boundary; if that is
     * where chunk 1 began, chunk 1 started at a real row too, and so on down the
     * line. Only a guess that landed on a newline inside a quoted field breaks
     * that, and then the sweep runs again on counted bounds, which is exactly
     * what it did before. A chunk that started on a wrong guess parsed garbage,
     * so its rows and its failures are both thrown away unread; one that failed
     * for real stops short of its boundary, looks like a wrong guess, and fails
     * again on the counted run, where it is reported.
     */
    unsigned ways = chunk_bounds(slab, from, threads, bounds, false);
    run_parts(sweep_part, &sc, ways);
    bool held = true;
    if (slab->dialect != DIALECT_JSON)
        for (unsigned p = 0; p + 1 < ways && held; p++) held = chunks[p].stopped == bounds[p + 1];
    if (!held) {
        for (unsigned p = 0; p < ways; p++) { free(chunks[p].start); free(chunks[p].hash); }
        memset(chunks, 0, threads * sizeof *chunks);
        /* Whatever the sink was handed came from rows that were not rows. */
        if (sink)
            for (unsigned p = 0; p < threads; p++) sink->reset(sink->ctx, p);
        ways = chunk_bounds(slab, from, threads, bounds, true);
        run_parts(sweep_part, &sc, ways);
    }
    free(bounds);
    phase_mark(&ph, "sweep rows");
    if (timed) {
        uint64_t p = 0, h = 0, u = 0, r = 0;
        for (unsigned i = 0; i < ways; i++) {
            p += t_parse[i]; h += t_hash[i]; u += t_push[i]; r += t_rows[i];
        }
        /* ru_minflt/ru_majflt from getrusage would need a before/after pair;
         * the per-part split is the actionable half. */
        fprintf(stderr, "    sweep detail: parse %.3fs hash %.3fs push %.3fs (%llu rows)\n",
                p / 1e9, h / 1e9, u / 1e9, (unsigned long long)r);
        free(t_parse); free(t_hash); free(t_push); free(t_rows);
    }

    bool ok = true;
    for (unsigned p = 0; p < ways; p++) {
        if (chunks[p].failed) ix->failed = true;
        if (chunks[p].oom) ok = false;
        ix->rows += chunks[p].n;
    }
    if (ix->failed) ok = false;

    if (ok) {
        const size_t n = ix->rows ? ix->rows : 1;
        if (budget_take(n * (sizeof *ix->row_start + sizeof *ix->row_hash)) != 0) {
            for (unsigned p = 0; p < ways; p++) { free(chunks[p].start); free(chunks[p].hash); }
            free(chunks);
            return false;
        }
        ix->row_start = malloc(n * sizeof *ix->row_start);
        ix->row_hash = malloc(n * sizeof *ix->row_hash);
        ok = ix->row_start && ix->row_hash;
    }
    if (ok) {
        /* Copied chunk by chunk, and each chunk released as it is taken, so the
         * high-water mark is the flat arrays plus one chunk rather than plus
         * all of them. */
        size_t at = 0;
        for (unsigned p = 0; p < ways; p++) {
            memcpy(ix->row_start + at, chunks[p].start, chunks[p].n * sizeof *ix->row_start);
            memcpy(ix->row_hash + at, chunks[p].hash, chunks[p].n * sizeof *ix->row_hash);
            at += chunks[p].n;
            free(chunks[p].start);
            free(chunks[p].hash);
            chunks[p].start = NULL;
            chunks[p].hash = NULL;
        }
        ix->rows_cap = ix->rows;
    }
    for (unsigned p = 0; p < ways; p++) { free(chunks[p].start); free(chunks[p].hash); }
    free(chunks);
    if (!ok) return false;

    /* Sized once, to under a half load, so nothing ever rehashes. */
    size_t cap = 1u << 12;
    while (cap < ix->rows * 2 + 16) cap <<= 1;
    /* Wide enough to hold every key index plus the +1 that keeps 0 for empty. */
    ix->pos_bits = 1;
    while (ix->pos_bits < 32 && ((size_t)1 << ix->pos_bits) < ix->rows + 2) ix->pos_bits++;
    ix->pos_mask = ix->pos_bits >= 32 ? 0xFFFFFFFFu : (uint32_t)(((uint64_t)1 << ix->pos_bits) - 1);
    {
        const size_t n = ix->rows ? ix->rows : 1;
        if (budget_take(cap * sizeof *ix->table +
                        n * (sizeof *ix->first_row + sizeof *ix->occurrences)) != 0)
            return false;
    }
    ix->table = alloc_huge(cap * sizeof *ix->table);
    ix->first_row = malloc((ix->rows ? ix->rows : 1) * sizeof *ix->first_row);
    ix->occurrences = malloc((ix->rows ? ix->rows : 1) * sizeof *ix->occurrences);
    if (!ix->table || !ix->first_row || !ix->occurrences) return false;
    ix->keys_cap = ix->rows;
    ix->mask = cap - 1;
    memset(ix->table, 0, cap * sizeof *ix->table);      /* TABLE_EMPTY is 0 */

    for (size_t r = 0; r < ix->rows; r++) {
        /* Every insert is a cache miss on a table too big to hold, and the hash
         * that decides which line is already in hand. */
        if (r + PREFETCH_AHEAD < ix->rows)
            __builtin_prefetch(&ix->table[slot_of(ix, ix->row_hash[r + PREFETCH_AHEAD])], 1, 0);
        const uint64_t hash = ix->row_hash[r];
        size_t slot = slot_of(ix, hash);
        for (;;) {
            const uint32_t v = ix->table[slot];
            if (v == TABLE_EMPTY) {
                ix->table[slot] = slot_pack(ix, hash, ix->keys);
                ix->first_row[ix->keys] = (int32_t)r;
                ix->occurrences[ix->keys] = 1;
                ix->keys++;
                break;
            }
            /* The tag rejects a foreign key from this word alone; only a tag
             * that matches is worth two more misses to disprove. */
            if (!slot_tag_is(ix, v, hash)) { slot = (slot + 1) & ix->mask; continue; }
            const size_t at = slot_pos(ix, v);
            const int32_t candidate = ix->first_row[at];
            if (ix->row_hash[candidate] == hash) {
                index_keys(ix, candidate, ix->probe);
                index_keys(ix, (int32_t)r, ix->probe2);
                const Norm *n = ix->norm;
                bool same = true;
                for (size_t i = 0; i < key_size && same; i++) {
                    const bool xa = absent_norm(slab, ix->probe[i], n);
                    const bool ya = absent_norm(slab, ix->probe2[i], n);
                    same = (xa || ya) ? (xa && ya)
                                      : same_norm(slab, ix->probe[i], slab, ix->probe2[i], n);
                }
                if (same) {
                    if (++ix->occurrences[at] == 2) {
                        ix->dup_keys++;
                        ix->dup_rows++;  /* the first occurrence counts once the key repeats */
                    }
                    ix->dup_rows++;
                    if (sink) {
                        if (ix->n_later == ix->later_cap) {
                            const size_t next = ix->later_cap ? ix->later_cap * 2 : 1024;
                            if (budget_take((next - ix->later_cap) * sizeof *ix->later) != 0)
                                return false;
                            int32_t *grown = realloc(ix->later, next * sizeof *grown);
                            if (!grown) return false;
                            ix->later = grown;
                            ix->later_cap = next;
                        }
                        ix->later[ix->n_later++] = (int32_t)r;
                    }
                    break;
                }
            }
            slot = (slot + 1) & ix->mask;
        }
    }
    phase_mark(&ph, "insert in order");
    return true;
}

/* Both files' indexes, built at once. */
typedef struct {
    RowIndex        *ix[2];
    const Slab      *slab[2];
    const RowParser *parser[2];
    size_t           from[2];
    size_t           key_size;
    unsigned         threads;
    const Norm      *norm;
    bool             ok[2];
} BuildCtx;

static void build_part(void *vctx, unsigned p) {
    BuildCtx *c = vctx;
    c->ok[p] = index_build(c->ix[p], c->slab[p], c->parser[p], c->from[p], c->key_size,
                           c->threads, c->norm, NULL);
}

/*
 * `probe` is the caller's scratch, not the index's, because several threads are
 * inside this at once. It used to hang off the RowIndex, which was safe only
 * while the comparison ran on one thread.
 */
static int32_t index_lookup(const RowIndex *ix, const Slab *other, const Field *fields,
                            uint64_t hash, Field *probe) {
    size_t slot = slot_of(ix, hash);
    for (;;) {
        const uint32_t v = ix->table[slot];
        if (v == TABLE_EMPTY) return -1;
        if (!slot_tag_is(ix, v, hash)) { slot = (slot + 1) & ix->mask; continue; }
        const size_t at = slot_pos(ix, v);
        int32_t candidate = ix->first_row[at];
        if (ix->row_hash[candidate] == hash) {
            index_keys(ix, candidate, probe);
            const Norm *n = ix->norm;
            bool ok = true;
            for (size_t i = 0; i < ix->key_size && ok; i++) {
                bool xa = absent_norm(ix->slab, probe[i], n), ya = absent_norm(other, fields[i], n);
                ok = (xa || ya) ? (xa && ya) : same_norm(ix->slab, probe[i], other, fields[i], n);
            }
            if (ok) return candidate;
        }
        slot = (slot + 1) & ix->mask;
    }
}

/* ------------------------------------------------------------------------- */
/* Comparing, on every core                                                    */
/*                                                                             */
/* Every distinct key is independent of every other: it is looked up in the     */
/* other file's table and its columns compared, and nothing it does affects     */
/* what any other key finds. So the work splits over contiguous ranges of one   */
/* side's keys, each range counting into its own totals, and the totals are     */
/* summed at the end. Counts are sums, so the order they are added in cannot    */
/* change the answer.                                                          */
/* ------------------------------------------------------------------------- */

/*
 * Two rows that agree byte for byte over a prefix agree, column for column,
 * over every column that ends inside it.
 *
 * The join's expensive half is not the lookup -- it is what happens after one:
 * the mate's row is parsed a second time and every compared column is compared
 * byte by byte. But rows that match usually match because they are the same
 * row, differing in one column that moved. Comparing the two rows' raw bytes
 * from the front, a word at a time, finds where they first diverge; every
 * compared column that ends before that point is proven equal without parsing
 * the mate at all, and when that covers all of them the mate is never touched.
 *
 * The proof needs the two files to lay their columns out the same way, which
 * `aligned` below decides once per run, and it is CSV only -- see there.
 */
static inline size_t row_end(const RowIndex *ix, int32_t row) {
    const size_t next = (size_t)row + 1;
    return next < ix->rows ? (size_t)ix->row_start[next] : ix->slab->size;
}

#define PROOF_BACKOFF 64u /* a power of two: the loop tests k against it */

static inline size_t common_prefix(const char *x, const char *y, size_t n) {
    size_t i = 0;
    for (; i + 8 <= n; i += 8) {
        const uint64_t a = load64(x + i), b = load64(y + i);
        if (a != b) return i + ((size_t)__builtin_ctzll(a ^ b) >> 3);
    }
    for (; i < n && x[i] == y[i]; i++) {}
    return i;
}

/*
 * Does the mate's tail name anything this run tracks?
 *
 * The CSV proof rests on a column sitting at a fixed offset. JSON has no such
 * promise: a value is found by name, and a name repeated in one object takes
 * its *last* value for a compared column -- which the C++ port does too and
 * test.sh cross-checks -- so a second `"amount"` past the diverging byte would
 * carry a value the prefix never saw.
 *
 * It cannot be ruled out in general, but it can be ruled out here, because the
 * proof only reaches this point when the two rows agree all the way through the
 * last compared value: whatever is left is the trailing ignored columns, a few
 * bytes. A name is a quoted string, so every quote in those bytes is a
 * candidate. Taking a closing quote for an opening one costs a lookup that
 * fails, which is a fallback rather than a wrong answer -- and an escaped name
 * is treated as a hit for the same reason, since `want` holds names unescaped.
 */
static bool json_tail_is_clean(const RowParser *p, const char *d, size_t at, size_t end) {
    while (at < end) {
        const size_t q = next_of1(d, at, end, '"');
        if (q >= end) return true;
        bool escaped = false;
        const size_t close = skip_json_string(d, q, end, &escaped);
        if (escaped) return false;
        const size_t from = q + 1, to = close > q + 1 ? close - 1 : q + 1;
        if (to > from && parser_slot_for(p, d + from, to - from) >= 0) return false;
        if (close <= q) return false;
        at = close;
    }
    return true;
}

static inline bool json_ws(char c) { return c == ' ' || c == '\t' || c == '\r'; }

/* Past one JSON value from `at`: a string, a scalar, or a nested object or
 * array. `end` where it does not close before `end`. */
static size_t json_value_end(const char *d, size_t at, size_t end) {
    if (at >= end) return end;
    if (d[at] == '"') {
        bool escaped = false;
        return skip_json_string(d, at, end, &escaped);
    }
    if (d[at] == '{' || d[at] == '[') return skip_json_nested(d, at, end);
    while (at < end && d[at] != ',' && d[at] != '}' && d[at] != ']' && d[at] != '\n' &&
           !json_ws(d[at]))
        at++;
    return at;
}

/*
 * The member of A's object whose value holds byte `diff`, walking from `at`
 * (the row start, or just past a value already skipped): its value's bounds in
 * `*vs`, `*ve`. False where `diff` falls in a name or the punctuation between
 * members, in a tracked member, or anywhere the walk cannot follow -- an
 * escaped name included, as the tail check treats one.
 */
static bool json_member_at(const RowParser *p, const char *d, size_t at, size_t end,
                           size_t diff, bool after_value, size_t *vs, size_t *ve) {
    for (;;) {
        while (at < end && json_ws(d[at])) at++;
        if (at >= end || at > diff) return false;
        if (!after_value) {
            if (d[at] != '{') return false;
        } else if (d[at] != ',') {
            return false;                        /* the object closed first */
        }
        at++;
        while (at < end && json_ws(d[at])) at++;
        if (at >= end || d[at] != '"' || at >= diff) return false;
        bool escaped = false;
        const size_t kq = at, kend = skip_json_string(d, at, end, &escaped);
        if (escaped || kend >= end || kend >= diff) return false;
        at = kend;
        while (at < end && json_ws(d[at])) at++;
        if (at >= end || d[at] != ':') return false;
        at++;
        while (at < end && json_ws(d[at])) at++;
        if (at > diff) return false;
        const size_t v_end = json_value_end(d, at, end);
        if (v_end >= end || v_end <= at) return false;
        if (diff < v_end) {
            if (parser_slot_for(p, d + kq + 1, kend - kq - 2) >= 0) return false;
            *vs = at;
            *ve = v_end;
            return true;
        }
        at = v_end;
        after_value = true;
    }
}

/*
 * The JSON proof, past ignored values that differ.
 *
 * The plain proof holds when A's row and its mate agree byte for byte through
 * the last value either side wants and the mate's tail names nothing tracked.
 * One ignored member in between whose value differs -- an `updated_at`, say,
 * which differs on every row -- fails that on every row. So where the two rows
 * first differ, this finds the member of A's object that byte is in. If it is
 * the value of a name nobody tracks, the value is skipped in each row on its
 * own -- in B from the same offset, since everything before it agreed, which
 * includes the name -- and the comparison carries on from there. A difference
 * anywhere else, or in a tracked value, or in a shape the walk does not follow,
 * fails the proof, and the row is parsed as it was before.
 *
 * The names before each skipped value are equal bytes in both rows and the
 * skipped values belong to untracked names, so every tracked name appears in
 * both at the same place with the same bytes; the tail check covers what
 * follows, as it did.
 */
static bool json_rows_match(const RowParser *p, const char *a, size_t a_lo, size_t a_hi,
                            size_t need, const char *b, size_t b_lo, size_t b_hi) {
    size_t ai = a_lo, bi = b_lo, left = need;
    bool after_value = false;
    for (int hops = 0; hops < 64; hops++) {
        const size_t room = b_hi - bi;
        const size_t m = left < room ? left : room;
        const size_t same = common_prefix(a + ai, b + bi, m);
        if (same == left) return json_tail_is_clean(p, b, bi + left, b_hi);
        if (same == m) return false;             /* B ends first */
        size_t vs, ve;
        if (!json_member_at(p, a, ai, a_hi, ai + same, after_value, &vs, &ve)) return false;
        if (ve - a_lo > need) return false;      /* the gap runs past the last wanted value */
        const size_t b_vs = bi + (vs - ai);
        const size_t b_ve = json_value_end(b, b_vs, b_hi);
        if (b_ve >= b_hi || b_ve <= b_vs) return false;
        left -= ve - ai;
        ai = ve;
        bi = b_ve;
        after_value = true;
    }
    return false;
}

/*
 * `index_lookup`, with the proof tried first.
 *
 * `need` bytes from `a_row` cover every column either file wants, keys
 * included, so a candidate whose row begins with the same bytes carries the
 * same key: that is the key comparison, and it is settled without parsing the
 * candidate's keys at all -- which was a tenth of the ndjson run's
 * instructions. Only a candidate the bytes do not settle is parsed and
 * compared as `index_lookup` does. `*proven` says which of the two found it,
 * and a proven mate needs no comparing either: every compared column is inside
 * the same bytes.
 *
 * `fields` may be NULL, when the caller has the span but has not parsed the
 * row: then a candidate the bytes do not settle is returned unproven, and the
 * caller parses and re-looks-up with the key check, which also covers the
 * hash-collision case the deferred check would have continued past.
 */
static int32_t index_lookup_proof(const RowIndex *ix, const Slab *other, const Field *fields,
                                  uint64_t hash, Field *probe, const char *a_row, size_t need,
                                  size_t a_hi, bool json, bool *proven) {
    size_t slot = slot_of(ix, hash);
    for (;;) {
        const uint32_t v = ix->table[slot];
        if (v == TABLE_EMPTY) return -1;
        if (!slot_tag_is(ix, v, hash)) { slot = (slot + 1) & ix->mask; continue; }
        const size_t at = slot_pos(ix, v);
        int32_t candidate = ix->first_row[at];
        if (ix->row_hash[candidate] == hash) {
            const size_t b_lo = (size_t)ix->row_start[candidate];
            const size_t b_n = row_end(ix, candidate) - b_lo;
            const char *b = ix->slab->data;
            if (json ? json_rows_match(ix->parser, other->data, (size_t)(a_row - other->data),
                                       a_hi, need, b, b_lo, b_lo + b_n)
                     : need <= b_n && common_prefix(a_row, b + b_lo, need) == need) {
                *proven = true;
                return candidate;
            }
            if (!fields) { *proven = false; return candidate; }
            index_keys(ix, candidate, probe);
            const Norm *n = ix->norm;
            bool ok = true;
            for (size_t i = 0; i < ix->key_size && ok; i++) {
                bool xa = absent_norm(ix->slab, probe[i], n), ya = absent_norm(other, fields[i], n);
                ok = (xa || ya) ? (xa && ya) : same_norm(ix->slab, probe[i], other, fields[i], n);
            }
            if (ok) return candidate;
        }
        slot = (slot + 1) & ix->mask;
    }
}

/*
 * One per thread, and padded so that two of them cannot share a cache line.
 *
 * Without the padding these sit in one `calloc`ed array about ninety bytes
 * apart, so two of them share a line. `out->matched++` on one thread then
 * dirties the line another thread is incrementing its own counters in, and the
 * line moves between cores. Four million rows of that does not show up as any
 * single slow thing: it shows up as the join scaling 2.8x on four cores, where
 * the same work in the Rust port -- whose per-thread accumulator is a value
 * returned from a closure and so never adjacent to another thread's -- reaches
 * 3.9x. Padded, this port reaches 3.9x too.
 *
 * The counters could equally be locals merged at the end, which is what the
 * Rust port does by accident of language. Padding is the smaller change to a
 * structure the caller already allocates as an array.
 */
typedef struct {
    _Alignas(64)
    int64_t  matched, changed, removed, added;
    int64_t *col_changed, *col_blanked, *col_filled;
    Field   *fa, *fb, *probe;
    size_t  *seg_lo, *seg_len;   /* this row's runs of wanted columns, for the gapped proof */
    unsigned refused;            /* the proof's backoff, when rows arrive from the sweep */
    bool     oom;
    char     pad[64];
} CmpPart;

typedef struct {
    const RowIndex *ai, *bi;
    const Slab     *a, *b;
    size_t          key_size, nc, width;
    unsigned        ways, b_ways;
    CmpPart        *parts;
    /*
     * Set when both files are CSV, read with the same delimiter, and put every
     * compared column at the same column number in ascending order. Then a
     * column's bytes sit at the same offset in both rows, and the compared
     * column that ends last is the one with the highest column number -- so one
     * check settles the whole row.
     *
     * JSON is deliberately excluded. A value there is found by name, and a name
     * repeated in one object takes its *last* value for a compared column, so a
     * duplicate past the diverging byte in the mate could carry a value the
     * prefix never saw.
     */
    bool            aligned;
    size_t          guard;
    char            delim;
    /* The same proof, on objects: no column order to check, because a name maps
     * to the same slot in both files whatever order the objects list them in. */
    bool            json;
    /* Whether the proof's bytes also cover the keys, so that it can stand in
     * for the key comparison and run before the mate is parsed: always on
     * objects, where the span is taken over every wanted value; on CSV when
     * every key sits at the same column in both files, before the guard. */
    bool            keys_in_proof;
    /* Delimiters from the row start to the one that ends the guard column, so
     * the join can count the proof's span without parsing the row. Only for
     * the aligned CSV proof; 0 disables it. */
    int             guard_commas;
    const Norm     *norm;
    /*
     * The same fields in runs, where a column nobody wants sits among them --
     * an ignored `updated_at` in the middle of the row, say. One run is the
     * plain prefix above. More, and the proof compares each run of wanted
     * columns as bytes and skips the unwanted ones between them in each row on
     * its own: an ignored value that differs cannot stop a row proving equal,
     * and one that differs in length cannot misalign what follows, because the
     * skip counts fields rather than bytes. `seg_fields[j]` wanted fields are
     * followed by `gap_fields[j]` unwanted ones; the last run has no gap.
     */
    int             nseg;
    const int      *seg_fields, *gap_fields;
} CmpCtx;

/*
 * `added` does not need a pass of its own.
 *
 * Every distinct key of A finds at most one distinct key of B, distinct keys of
 * A cannot find the same key of B, and the comparison behind the lookup is
 * symmetric -- so the number of B's keys that have an A counterpart is exactly
 * the `matched` the A pass already counted, and `added` is `B's keys` minus it.
 *
 * That is an argument, and arguments about symmetry are exactly the kind that
 * are wrong once. `CSVDIFF_VERIFY_ADDED=1` runs the old pass -- every key of B
 * looked up in A -- and refuses the run if the two disagree, so the argument is
 * checked rather than believed. test.sh runs it on the awkward fixture and on
 * generated rows.
 */
static int verify_added(void) { return getenv("CSVDIFF_VERIFY_ADDED") != NULL; }

/* One mate's compared columns under `n`. Always inlined, so compare_mate's
 * call with a literal NULL is the byte compare the run without options was
 * before they existed: left to the compiler, the test for `n` stayed in the
 * loop, and that was 1.5% of a default run. */
static inline __attribute__((always_inline))
bool compare_cells(const CmpCtx *c, CmpPart *out, const Norm *n) {
    const size_t key_size = c->key_size, nc = c->nc;
    const Slab *a = c->a, *b = c->b;
    bool any = false;
    for (size_t i = 0; i < nc; i++) {
        const Field x = out->fa[key_size + i], y = out->fb[key_size + i];
        const bool xa = absent_norm(a, x, n), ya = absent_norm(b, y, n);
        const bool differs = (xa || ya) ? (xa != ya) : !same_norm(a, x, b, y, n);
        if (differs) {
            any = true;
            out->col_changed[i]++;
            if (ya) out->col_blanked[i]++;
            if (xa) out->col_filled[i]++;
        }
    }
    return any;
}

/* Parses the mate in full and counts the compared columns that differ. */
static void compare_mate(const CmpCtx *c, CmpPart *out, int32_t mate) {
    index_fields(c->bi, mate, out->fb);
    const bool any = c->norm ? compare_cells(c, out, c->norm) : compare_cells(c, out, NULL);
    if (any) out->changed++;
}

/*
 * The proof's byte span without parsing the row.
 *
 * `proof_span` reads it off the guard field's packed offsets, which means
 * parsing every column first -- and the join parses all ten million rows of A
 * that way, though the proof only needs one offset: through the delimiter
 * that ends the guard column. The guard is source column `guard_src`, so its
 * end is the (`guard_src`+1)th delimiter from the row start -- countable with
 * the delimiter cursor alone, skipping a quoted field the way the row parse
 * does, and without packing a field or walking the slot map. Returns 0 when
 * the row ends first -- a short row, or a malformed one -- which falls back
 * to the parse.
 *
 * The count is exact on well-formed rows: a field that opens with a quote is
 * skipped whole, so a comma inside one is never counted, and a quote anywhere
 * else is data, exactly as the row parse treats it. Where the bytes are not
 * well-formed the span can only come out short of the row end, and a short
 * span fails the proof's length check and falls back -- it never proves.
 *
 * The byte that closes the guard may be the line ending as well as a
 * delimiter: where the last compared column is the row's last column, it is
 * the only byte that can. Refusing it -- as this did until the 2M x 100 run --
 * turned the whole fast path off for any file whose last column is compared,
 * and every row paid for a parse. A line ending before the last count is a
 * short row, and still returns 0.
 */
static size_t past_fields(Delims *c, const char *d, size_t at, size_t hi, char delim, int n,
                          bool may_end) {
    for (int i = 0; i < n; i++) {
        size_t cur;
        if (at < hi && d[at] == '"') {
            const size_t close = skip_quoted(d, at + 1, hi);
            cur = delims_next(c, close);
        } else {
            cur = delims_next(c, at);
        }
        if (cur >= hi) return SIZE_MAX;
        if (d[cur] != delim && !(may_end && i == n - 1)) return SIZE_MAX;
        at = cur + 1;
    }
    return at;
}

static size_t guard_span(const char *d, size_t lo, size_t hi, char delim, int commas) {
    Delims c;
    delims_init(&c, d, lo, hi, delim, '\n');
    const size_t at = past_fields(&c, d, lo, hi, delim, commas, true);
    return at == SIZE_MAX || commas == 0 ? 0 : at - lo;
}

/*
 * The gapped proof's A side: where each run of wanted columns starts in this
 * row and how many bytes it takes, through the byte that closes it. False
 * where the row ends first, which falls back to the parse.
 */
static bool segments_of(const CmpCtx *c, CmpPart *out, size_t lo, size_t hi) {
    const char *d = c->a->data;
    Delims cur;
    delims_init(&cur, d, lo, hi, c->delim, '\n');
    size_t at = lo;
    for (int j = 0; j < c->nseg; j++) {
        const bool last = j == c->nseg - 1;
        const size_t end = past_fields(&cur, d, at, hi, c->delim, c->seg_fields[j], last);
        if (end == SIZE_MAX) return false;
        out->seg_lo[j] = at;
        out->seg_len[j] = end - at;
        if (last) break;
        at = past_fields(&cur, d, end, hi, c->delim, c->gap_fields[j], false);
        if (at == SIZE_MAX) return false;
    }
    return true;
}

/*
 * The B side: each run's bytes equal A's where B's own gaps put it. A run's
 * bytes include the byte that closes it, so equal bytes leave B at the start
 * of its next field, as they leave A.
 */
static bool segments_match(const CmpCtx *c, const CmpPart *out, size_t b_lo, size_t b_hi) {
    const char *a = c->a->data, *b = c->b->data;
    Delims cur;
    delims_init(&cur, b, b_lo, b_hi, c->delim, '\n');
    size_t at = b_lo;
    for (int j = 0; j < c->nseg; j++) {
        const size_t n = out->seg_len[j];
        if (n > b_hi - at || common_prefix(a + out->seg_lo[j], b + at, n) != n) return false;
        at += n;
        if (j == c->nseg - 1) break;
        at = past_fields(&cur, b, at, b_hi, c->delim, c->gap_fields[j], false);
        if (at == SIZE_MAX) return false;
    }
    return true;
}

/*
 * `index_lookup_proof` with no fields, for the gapped proof: the first
 * candidate whose whole hash matches, proven when its runs match A's.
 * Unproven, the caller parses and looks up again with the key check.
 */
static int32_t lookup_by_segments(const CmpCtx *c, const CmpPart *out, uint64_t hash,
                                  bool *proven) {
    const RowIndex *ix = c->bi;
    size_t slot = slot_of(ix, hash);
    for (;;) {
        const uint32_t v = ix->table[slot];
        if (v == TABLE_EMPTY) return -1;
        if (slot_tag_is(ix, v, hash)) {
            const int32_t candidate = ix->first_row[slot_pos(ix, v)];
            if (ix->row_hash[candidate] == hash) {
                *proven = segments_match(c, out, (size_t)ix->row_start[candidate],
                                         row_end(ix, candidate));
                return candidate;
            }
        }
        slot = (slot + 1) & ix->mask;
    }
}

/*
 * How many bytes of A's row the proof compares, keys included -- or 0 where
 * this row cannot be proven that way. Through the byte that closes the last
 * value either file wants, for the reason the CSV proof below gives.
 */
static size_t proof_span(const CmpCtx *c, size_t a_lo, size_t a_end, const Field *fa) {
    size_t t = a_lo;
    if (c->json) {
        for (size_t i = 0; i < c->width; i++) {
            if (!field_real(fa[i])) continue;
            const size_t e = field_off(fa[i]) + field_len(fa[i]);
            if (e > t) t = e;
        }
    } else {
        const Field g = fa[c->guard];
        if (!field_real(g)) return 0;
        const char *d = c->a->data;
        t = field_off(g) + field_len(g);
        while (t < a_end && d[t] != c->delim && d[t] != '\n' && d[t] != '\r') t++;
    }
    return t < a_end ? t + 1 - a_lo : 0;
}

/* A part's scratch. False, and the part marked, when there is no memory. */
static bool part_init(const CmpCtx *c, CmpPart *out) {
    const size_t nc = c->nc;
    out->fa = malloc(c->width * sizeof *out->fa);
    out->fb = malloc(c->width * sizeof *out->fb);
    out->probe = malloc(c->width * sizeof *out->probe);
    out->col_changed = calloc(nc ? nc : 1, sizeof *out->col_changed);
    out->col_blanked = calloc(nc ? nc : 1, sizeof *out->col_blanked);
    out->col_filled = calloc(nc ? nc : 1, sizeof *out->col_filled);
    if (c->nseg > 1) {
        out->seg_lo = malloc((size_t)c->nseg * sizeof *out->seg_lo);
        out->seg_len = malloc((size_t)c->nseg * sizeof *out->seg_len);
    }
    if (!out->fa || !out->fb || !out->probe || !out->col_changed || !out->col_blanked ||
        !out->col_filled || (c->nseg > 1 && (!out->seg_lo || !out->seg_len))) {
        out->oom = true;
        return false;
    }
    return true;
}

/*
 * One row of A against its mate in B, counted into `out`: matched or removed,
 * and if matched, changed or not and in which columns. The row is the bytes
 * from `row_lo` to `row_hi`, the next row's start, as `row_end` gives it.
 * `k` and `refused` are the proof's backoff; they decide only how the answer
 * is reached, never what it is.
 */
static void join_row(const CmpCtx *c, CmpPart *out, size_t row_lo, size_t row_hi, uint64_t hash,
                     size_t k, unsigned *refused) {
    const size_t key_size = c->key_size, nc = c->nc;
    const bool attempt = *refused < PROOF_BACKOFF || (k & (PROOF_BACKOFF - 1)) == 0;
    /*
     * The proof's span without the parse: the guard's delimiter sits a
     * fixed count past the keys, countable with the delimiter cursor
     * alone. Most rows prove out here and never pay for fields; a row
     * the bytes do not settle is parsed below.
     */
    if (attempt && c->guard_commas > 0) {
        const size_t a_lo = row_lo;
        const size_t a_hi = row_hi;
        bool proven = false, tried = false;
        int32_t mate = -1;
        if (c->nseg > 1) {
            tried = segments_of(c, out, a_lo, a_hi);
            if (tried) mate = lookup_by_segments(c, out, hash, &proven);
        } else {
            const size_t need =
                guard_span(c->a->data, a_lo, a_hi, c->delim, c->guard_commas);
            tried = need != 0;
            if (tried)
                mate = index_lookup_proof(c->bi, c->a, NULL, hash, out->probe,
                                          c->a->data + a_lo, need, a_hi, false, &proven);
        }
        if (tried) {
            if (mate < 0) { out->removed++; return; }
            if (proven) { out->matched++; *refused = 0; return; }
            /*
             * A candidate the bytes did not settle: parse and let the
             * key check confirm it, which also covers the hash
             * collision the deferred check skipped past.
             */
            parse_row(c->ai->parser, c->a->data, row_lo, c->a->size, out->fa);
            const int32_t mate2 =
                index_lookup(c->bi, c->a, out->fa, hash, out->probe);
            if (mate2 < 0) { out->removed++; return; }
            out->matched++;
            if (*refused < PROOF_BACKOFF) (*refused)++;
            compare_mate(c, out, mate2);
            return;
        }
    }
    parse_row(c->ai->parser, c->a->data, row_lo, c->a->size, out->fa);
    const size_t need = attempt && c->keys_in_proof ? proof_span(c, row_lo, row_hi, out->fa) : 0;
    if (need) {
        bool proven = false;
        const int32_t mate = index_lookup_proof(
            c->bi, c->a, out->fa, hash, out->probe,
            c->a->data + row_lo, need, row_hi,
            c->json, &proven);
        if (mate < 0) { out->removed++; return; }
        out->matched++;
        if (proven) { *refused = 0; return; }
        if (*refused < PROOF_BACKOFF) (*refused)++;
        compare_mate(c, out, mate);
        return;
    }
    const int32_t mate = index_lookup(c->bi, c->a, out->fa, hash, out->probe);
    if (mate < 0) { out->removed++; return; }
    out->matched++;
    if (c->json &&
        (*refused < PROOF_BACKOFF || (k & (PROOF_BACKOFF - 1)) == 0)) {
        const size_t a_lo = row_lo;
        const size_t b_lo = (size_t)c->bi->row_start[mate];
        const size_t a_end = row_hi;
        const size_t b_n = row_end(c->bi, mate) - b_lo;
        /* Through the byte that closes the last compared value --
         * whichever it turns out to be, since objects need not list
         * their names in the same order twice. */
        size_t t = a_lo;
        for (size_t i = 0; i < nc; i++) {
            const Field x = out->fa[key_size + i];
            if (!field_real(x)) continue;
            const size_t e = field_off(x) + field_len(x);
            if (e > t) t = e;
        }
        const size_t need = t + 1 - a_lo;
        if (t < a_end && json_rows_match(c->bi->parser, c->a->data, a_lo, a_end, need,
                                         c->b->data, b_lo, b_lo + b_n)) {
            *refused = 0;
            return;
        }
        if (*refused < PROOF_BACKOFF) (*refused)++;
    }
    /*
     * A proof that keeps failing is a scan for nothing -- two files
     * where every row really has changed pay for it on every row. So
     * after PROOF_BACKOFF failures in a row it is only attempted every
     * PROOF_BACKOFF rows, until one succeeds and it is on again.
     */
    const Field g = c->aligned ? out->fa[c->guard] : ABSENT;
    if (c->aligned && field_real(g) &&
        (*refused < PROOF_BACKOFF || (k & (PROOF_BACKOFF - 1)) == 0)) {
        const size_t a_lo = row_lo;
        const size_t b_lo = (size_t)c->bi->row_start[mate];
        /*
         * How far the two rows have to agree: through the byte that
         * closes the last compared column, and no further -- whatever
         * trails it is ignored, and reading it is work for nothing.
         *
         * Through the closing byte, not up to it. Agreeing as far as
         * the field's last byte says only that the mate's field starts
         * the same way: `cc` is a prefix of `cccccccc`, and a quoted
         * field the mate carries on with a doubled quote reads the same
         * that far too. It is the delimiter or the line ending after it
         * that says the mate's field stopped where this one did.
         */
        const char *d = c->a->data;
        const size_t a_end = row_hi;
        size_t t = field_off(g) + field_len(g);
        while (t < a_end && d[t] != c->delim && d[t] != '\n' && d[t] != '\r') t++;
        const size_t need = t + 1 - a_lo;
        if (t < a_end && need <= row_end(c->bi, mate) - b_lo &&
            common_prefix(d + a_lo, c->b->data + b_lo, need) == need) {
            *refused = 0;
            return;
        }
        if (*refused < PROOF_BACKOFF) (*refused)++;
    }
    compare_mate(c, out, mate);
}

static void compare_part(void *vctx, unsigned p) {
    CmpCtx *c = vctx;
    CmpPart *out = &c->parts[p];
    if (!part_init(c, out)) return;

    if (p < c->ways) {
        const size_t lo = c->ai->keys * p / c->ways, hi = c->ai->keys * (p + 1) / c->ways;
        unsigned refused = 0;
        for (size_t k = lo; k < hi; k++) {
            const int32_t row = c->ai->first_row[k];
            /* The sweep already hashed this row; re-deriving it here meant
             * hashing every key twice per run. */
            const uint64_t hash = c->ai->row_hash[row];
            if (k + PREFETCH_AHEAD < hi)
                __builtin_prefetch(
                    &c->bi->table[slot_of(c->bi,
                                          c->ai->row_hash[c->ai->first_row[k + PREFETCH_AHEAD]])],
                    0, 0);
            join_row(c, out, (size_t)c->ai->row_start[row], row_end(c->ai, row), hash, k,
                     &refused);
        }
        return;
    }

    const unsigned q = p - c->ways;
    const size_t lo = c->bi->keys * q / c->b_ways, hi = c->bi->keys * (q + 1) / c->b_ways;
    for (size_t k = lo; k < hi; k++) {
        const int32_t row = c->bi->first_row[k];
        const uint64_t hash = c->bi->row_hash[row];
        if (k + PREFETCH_AHEAD < hi)
            __builtin_prefetch(
                &c->ai->table[slot_of(c->ai,
                                      c->bi->row_hash[c->bi->first_row[k + PREFETCH_AHEAD]])],
                0, 0);
        index_keys(c->bi, row, out->fb);
        if (index_lookup(c->ai, c->b, out->fb, hash, out->probe) < 0) out->added++;
    }
}

/*
 * The join inside A's sweep. See `fused` in main. Each row is joined as the
 * sweep finds it, as though it were the first row of its key; `fix_part` then
 * takes back the rows the insertion found repeating one.
 */
static void fused_row(void *ctx, unsigned p, size_t lo, size_t hi, uint64_t hash, size_t k) {
    const CmpCtx *c = ctx;
    CmpPart *out = &c->parts[p];
    if (!out->oom) join_row(c, out, lo, hi, hash, k, &out->refused);
}

static void fused_reset(void *ctx, unsigned p) {
    const CmpCtx *c = ctx;
    CmpPart *out = &c->parts[p];
    const size_t n = c->nc ? c->nc : 1;
    out->matched = out->changed = out->removed = 0;
    out->refused = 0;
    if (out->col_changed) memset(out->col_changed, 0, n * sizeof *out->col_changed);
    if (out->col_blanked) memset(out->col_blanked, 0, n * sizeof *out->col_blanked);
    if (out->col_filled) memset(out->col_filled, 0, n * sizeof *out->col_filled);
}

/* The rows that repeat a key, joined again into parts of their own, to be
 * subtracted: the same bytes against the same index give the same counts. */
typedef struct {
    const CmpCtx *cc;
    CmpPart      *parts;
    unsigned      ways;
} FixCtx;

static void fix_part(void *vctx, unsigned p) {
    const FixCtx *f = vctx;
    const CmpCtx *c = f->cc;
    CmpPart *out = &f->parts[p];
    const RowIndex *ai = c->ai;
    const size_t lo = ai->n_later * p / f->ways, hi = ai->n_later * (p + 1) / f->ways;
    for (size_t i = lo; i < hi && !out->oom; i++) {
        const int32_t row = ai->later[i];
        join_row(c, out, (size_t)ai->row_start[row], row_end(ai, row), ai->row_hash[row], i,
                 &out->refused);
    }
}

/* B's half, when CSVDIFF_VERIFY_ADDED asks for it, after a fused join. */
static void b_pass(void *vctx, unsigned p) {
    CmpCtx *c = vctx;
    compare_part(vctx, c->ways + p);
}

static int fused_forced(void) {
    const char *v = getenv("CSVDIFF_FUSED_JOIN");
    return v && *v && strcmp(v, "0") != 0;
}

static void index_free(RowIndex *ix) {
    free(ix->row_start);
    free(ix->row_hash);
    free(ix->table);
    free(ix->first_row);
    free(ix->occurrences);
    free(ix->later);
    free(ix->probe);
    free(ix->probe2);
}

/* ------------------------------------------------------------------------- */
/* Columns and the command line                                                */
/* ------------------------------------------------------------------------- */

typedef struct {
    char **items;
    size_t len;
} Names;

static void names_free(Names *n) {
    for (size_t i = 0; i < n->len; i++) free(n->items[i]);
    free(n->items);
}

static bool names_push(Names *n, char *v) {
    char **fresh = realloc(n->items, (n->len + 1) * sizeof *n->items);
    if (!fresh) return false;
    n->items = fresh;
    n->items[n->len++] = v;
    return true;
}

static int name_index(const Names *n, const char *needle) {
    for (size_t i = 0; i < n->len; i++)
        if (strcmp(n->items[i], needle) == 0) return (int)i;
    return -1;
}

static Names split_commas(const char *s) {
    Names out = {0};
    const char *start = s;
    for (;;) {
        const char *comma = strchr(start, ',');
        size_t len = comma ? (size_t)(comma - start) : strlen(start);
        if (len > 0) {
            char *piece = malloc(len + 1);
            if (!piece) break;
            memcpy(piece, start, len);
            piece[len] = 0;
            if (!names_push(&out, piece)) { free(piece); break; }
        }
        if (!comma) break;
        start = comma + 1;
    }
    return out;
}

static char detect_delimiter(const char *line, size_t len) {
    const char candidates[] = {',', ';', '\t', '|'};
    char best = ',';
    long best_count = -1;
    for (size_t c = 0; c < sizeof candidates; c++) {
        long n = 0;
        for (size_t i = 0; i < len; i++)
            if (line[i] == candidates[c]) n++;
        if (n > best_count) { best = candidates[c]; best_count = n; }
    }
    return best;
}

/* The header row's names, and where the first data row starts. */
/*
 * Which shape the file is, from its first non-space byte. A JSON record starts
 * with `{`; nothing in a CSV header row can.
 */
static Dialect detect_dialect(const Slab *s) {
    for (size_t i = 0; i < s->size; i++) {
        if (json_space(s->data[i])) continue;
        return s->data[i] == '{' ? DIALECT_JSON : DIALECT_CSV;
    }
    return DIALECT_CSV;
}

/*
 * A JSON file has no header row, so the column names are the keys of its first
 * object. That is the same rule the C++ port uses, and the same limitation: a
 * key that appears only in later objects is not a column. It is the only rule
 * that costs one line to establish rather than a pass over the file.
 */
static bool json_header(const Slab *s, Names *out) {
    const char *d = s->data;
    size_t end = s->size, pos = 0;
    while (pos < end && json_space(d[pos])) pos++;
    if (pos >= end || d[pos] != '{') return false;
    pos++;
    for (;;) {
        while (pos < end && json_space(d[pos])) pos++;
        if (pos >= end || d[pos] == '}') break;
        if (d[pos] == ',') { pos++; continue; }
        if (d[pos] != '"') break;
        bool escaped = false;
        size_t from = pos + 1;
        size_t close = skip_json_string(d, pos, end, &escaped);
        if (close > end || close < 2 || close - 1 < from) break;
        size_t len = close - 1 - from;
        pos = close;
        while (pos < end && json_space(d[pos])) pos++;
        if (pos >= end || d[pos] != ':') break;
        pos++;
        while (pos < end && json_space(d[pos])) pos++;
        if (pos >= end) break;
        /* The key name is stored decoded, because that is how the parser will
         * see it and how --key spells it. */
        char *name = malloc(len + 1);
        if (!name) return false;
        size_t n = escaped ? json_unescape(d + from, len, name, len) : len;
        if (!escaped) memcpy(name, d + from, len);
        name[n <= len ? n : len] = '\0';
        if (!names_push(out, name)) { free(name); return false; }
        /* Step over the value. */
        if (d[pos] == '"') { bool ig = false; pos = skip_json_string(d, pos, end, &ig); }
        else if (d[pos] == '{' || d[pos] == '[') pos = skip_json_nested(d, pos, end);
        else while (pos < end && d[pos] != ',' && d[pos] != '}' && !json_space(d[pos])) pos++;
    }
    return out->len > 0;
}

static bool read_header(const Slab *s, char delimiter, Names *out, size_t *start) {
    const char *d = s->data;
    if (s->size == 0) return false;
    size_t pos = 0;
    for (;;) {
        Field field;
        size_t next;
        if (pos < s->size && d[pos] == '"') {
            size_t close = skip_quoted(d, pos + 1, s->size);
            size_t body_end = close > pos + 1 ? close - 1 : pos + 1;
            next = next_of2(d, close, s->size, delimiter, '\n');
            field = pack(pos + 1, body_end - (pos + 1),
                         next_of1(d, pos + 1, body_end, '"') < body_end);
        } else {
            next = next_of2(d, pos, s->size, delimiter, '\n');
            size_t stop = next;
            if (stop > pos && d[stop - 1] == '\r') stop--;
            field = pack(pos, stop - pos, false);
        }
        size_t n = logical_len(s, field);
        char *name = malloc(n + 1);
        if (!name) return false;
        logical_copy(s, field, name, n);
        name[n] = 0;
        if (!names_push(out, name)) { free(name); return false; }

        if (next >= s->size) { *start = s->size; return true; }
        if (d[next] == '\n') { *start = next + 1; return true; }
        pos = next + 1;
    }
}

static int fail(const char *message) {
    fprintf(stderr, "error: %s\n", message);
    return 2;
}

/* A refusal that names the thing refused -- the column that is missing, the
 * file that would not open -- in the form the C++ and Rust ports use:
 * `<message>: <name>`. "Key column(s) missing" on a twelve-column key was a
 * search; the name makes it a fix. */
static int fail_named(const char *message, const char *name) {
    fprintf(stderr, "error: %s: %s\n", message, name);
    return 2;
}

/* The one refusal worth naming in full: it is the answer to the question
 * --max-memory asked. */
static int fail_budget(void) {
    fprintf(stderr,
            "error: the comparison needs more than the %zu MB it was given\n",
            budget_limit() / (1024 * 1024));
    return 2;
}

/* ------------------------------------------------------------------------- */
/* The report                                                                  */
/*                                                                             */
/* Both paths end here, because the whole claim of the Parquet path is that it  */
/* is the same comparison on the same rows -- if it could print its own summary */
/* the two could drift apart without anything noticing.                         */
/* ------------------------------------------------------------------------- */

typedef struct {
    const char *name;
    long long   changed, blanked, filled;
} OutCol;

typedef struct {
    const char   *engine;
    long long     a_rows, b_rows, a_keys, b_keys;
    long long     matched, changed, added, removed;
    long long     a_dup_keys, a_dup_rows, b_dup_keys, b_dup_rows;
    const OutCol *cols;
    size_t        ncols;
} Summary;

/* Returns the process exit status: 0 identical, 1 differences, 2 error. */
static int emit(const Summary *s, const char *json_path) {
    if (json_path) {
        FILE *out = fopen(json_path, "wb");
        if (!out) return fail("cannot write the JSON summary");
        fprintf(out,
                "{\"counts\":{\"a_rows\":%lld,\"b_rows\":%lld,\"a_keys\":%lld,\"b_keys\":%lld,"
                "\"matched\":%lld,\"unchanged\":%lld,\"changed\":%lld,\"added\":%lld,"
                "\"removed\":%lld,\"a_dup_keys\":%lld,\"a_dup_rows\":%lld,"
                "\"b_dup_keys\":%lld,\"b_dup_rows\":%lld},\"columns\":[",
                s->a_rows, s->b_rows, s->a_keys, s->b_keys, s->matched,
                s->matched - s->changed, s->changed, s->added, s->removed,
                s->a_dup_keys, s->a_dup_rows, s->b_dup_keys, s->b_dup_rows);
        for (size_t i = 0; i < s->ncols; i++)
            fprintf(out, "%s{\"name\":\"%s\",\"changed\":%lld,\"blanked\":%lld,\"filled\":%lld}",
                    i ? "," : "", s->cols[i].name, s->cols[i].changed, s->cols[i].blanked,
                    s->cols[i].filled);
        fprintf(out, "]}");
        fclose(out);
    }

    printf("A %lld rows | B %lld rows | matched %lld (changed %lld) | added %lld | removed %lld"
           " | dup keys A %lld B %lld | %s\n",
           s->a_rows, s->b_rows, s->matched, s->changed, s->added, s->removed,
           s->a_dup_keys, s->b_dup_keys, s->engine);
    return (s->changed == 0 && s->added == 0 && s->removed == 0) ? 0 : 1;
}

/* ------------------------------------------------------------------------- */
/* The Parquet path                                                            */
/* ------------------------------------------------------------------------- */

static int compare_parquet(const char *a_path, const char *b_path, const Names *key,
                           const Names *ignore, const Names *compare, unsigned threads,
                           const char *json_path) {
    PqResult r;
    if (pq_compare(a_path, b_path, key->items, key->len, ignore->items, ignore->len,
                   compare->items, compare->len, threads, &r) != 0)
        return budget_exceeded() ? fail_budget() : fail(pq_error());

    OutCol *cols = calloc(r.ncols ? r.ncols : 1, sizeof *cols);
    if (!cols) { pq_result_free(&r); return fail("out of memory"); }
    for (size_t i = 0; i < r.ncols; i++) {
        cols[i].name = r.cols[i].name;
        cols[i].changed = r.cols[i].changed;
        cols[i].blanked = r.cols[i].blanked;
        cols[i].filled = r.cols[i].filled;
    }
    const Summary s = { "parquet", r.a_rows, r.b_rows, r.a_keys, r.b_keys, r.matched,
                        r.changed, r.added, r.removed, r.a_dup_keys, r.a_dup_rows,
                        r.b_dup_keys, r.b_dup_rows, cols, r.ncols };
    const int status = emit(&s, json_path);
    free(cols);
    pq_result_free(&r);
    return status;
}

int main(int argc, char **argv) {
    if (argc < 2 || strcmp(argv[1], "-h") == 0 || strcmp(argv[1], "--help") == 0) {
        printf("csvdiff - composite-key comparison, byte-level, in C\n\n"
               "usage:\n  csvdiff compare A B -k COLS [-i COLS] [--json PATH] [--threads N]\n"
               "                  [--trim] [--ignore-case] [--empty-is-null] [--tolerance X]\n\n"
               "CSV, newline-delimited JSON, and uncompressed Parquet when both files\n"
               "are Parquet. The dialect is detected from the bytes.\n"
               "exit codes: 0 identical, 1 differences found, 2 error\n");
        return argc < 2 ? 2 : 0;
    }
    if (strcmp(argv[1], "compare") != 0) return fail("unknown command");

    Names key = {0}, ignore = {0}, compare = {0};
    const char *a_path = NULL, *b_path = NULL, *json_path = NULL;
    unsigned threads = 0;   /* 0 means one per core, on the Parquet path */
    size_t   max_memory_mb = 0;   /* 0 means no ceiling */
    bool trim = false, ignore_case = false, empty_is_null = false;
    double tolerance = 0.0;
    const char *first_norm = NULL;   /* the first normalisation flag given, to name it */
    /* Three lists to release now, and a fourth would be a fourth place to
     * forget one: every exit from the scan goes through here. */
#define ARGS_FAIL(msg) \
    do { names_free(&key); names_free(&ignore); names_free(&compare); return fail(msg); } while (0)
    for (int i = 2; i < argc; i++) {
        const char *f = argv[i];
        /* A known flag with nothing after it used to fall through to the
         * unknown-option arm, so `compare a b -k` said "unknown option" and
         * blamed the flag for not being recognised when it was recognised and
         * empty. Naming the flag costs one comparison at startup. */
        const int wants_value =
            !strcmp(f, "-k") || !strcmp(f, "--key") ||
            !strcmp(f, "-i") || !strcmp(f, "--ignore") ||
            !strcmp(f, "-c") || !strcmp(f, "--compare") ||
            !strcmp(f, "--json") ||
            !strcmp(f, "-t") || !strcmp(f, "--threads") ||
            !strcmp(f, "--max-memory") ||
            !strcmp(f, "--tolerance") ||
            !strcmp(f, "-o") || !strcmp(f, "--out") || !strcmp(f, "--engine");
        if (wants_value && i + 1 >= argc) {
            if (!strcmp(f, "-k") || !strcmp(f, "--key")) ARGS_FAIL("--key needs a value");
            if (!strcmp(f, "-i") || !strcmp(f, "--ignore")) ARGS_FAIL("--ignore needs a value");
            if (!strcmp(f, "-c") || !strcmp(f, "--compare")) ARGS_FAIL("--compare needs a value");
            if (!strcmp(f, "--json")) ARGS_FAIL("--json needs a value");
            if (!strcmp(f, "-t") || !strcmp(f, "--threads")) ARGS_FAIL("--threads needs a value");
            if (!strcmp(f, "--max-memory")) ARGS_FAIL("--max-memory needs a value");
            if (!strcmp(f, "--tolerance")) ARGS_FAIL("--tolerance needs a value");
            ARGS_FAIL("that option needs a value");
        }
        if (!strcmp(f, "-k") || !strcmp(f, "--key")) key = split_commas(argv[++i]);
        else if (!strcmp(f, "-i") || !strcmp(f, "--ignore")) ignore = split_commas(argv[++i]);
        else if (!strcmp(f, "-c") || !strcmp(f, "--compare")) compare = split_commas(argv[++i]);
        else if (!strcmp(f, "--json")) json_path = argv[++i];
        else if (!strcmp(f, "-t") || !strcmp(f, "--threads"))
            threads = (unsigned)strtoul(argv[++i], NULL, 10);
        else if (!strcmp(f, "--max-memory")) max_memory_mb = strtoul(argv[++i], NULL, 10);
        else if (!strcmp(f, "--trim")) { trim = true; if (!first_norm) first_norm = f; }
        else if (!strcmp(f, "--ignore-case")) { ignore_case = true; if (!first_norm) first_norm = f; }
        else if (!strcmp(f, "--empty-is-null")) { empty_is_null = true; if (!first_norm) first_norm = f; }
        else if (!strcmp(f, "--tolerance")) {
            if (!first_norm) first_norm = f;
            char *end;
            tolerance = strtod(argv[++i], &end);
            if (*end != '\0' || !(tolerance >= 0.0) || !isfinite(tolerance))
                ARGS_FAIL("--tolerance needs a non-negative number");
        }
        else if (!strcmp(f, "-o") || !strcmp(f, "--out") || !strcmp(f, "--engine")) i++;
        else if (f[0] == '-') ARGS_FAIL("unknown option");
        else if (!a_path) a_path = f;
        else if (!b_path) b_path = f;
    }
    if (!a_path || !b_path) ARGS_FAIL("compare needs two files");
    if (key.len == 0) ARGS_FAIL("--key is required");
#undef ARGS_FAIL
    budget_set(max_memory_mb);

    /* A column store and a byte stream have no common ground to be compared on:
     * one of them would have to be turned into the other, which is the cost the
     * columnar path exists to avoid. So a mixed pair is refused by name rather
     * than half-answered. */
    {
        const int ap = pq_is_parquet(a_path), bp = pq_is_parquet(b_path);
        /* A file that will not open is neither format, and reporting it as a
         * format mismatch sent people looking for a conversion they did not need. */
        if (ap < 0 || bp < 0) {
            names_free(&key);
            names_free(&ignore);
            names_free(&compare);
            return fail_named("cannot read one of the files", ap < 0 ? a_path : b_path);
        }
        if (ap != bp) {
            names_free(&key);
            names_free(&ignore);
            names_free(&compare);
            return fail("one file is parquet and the other is not; convert one of them first");
        }
        /* The columnar path does not carry the normalisation options yet. It
         * accepted them and compared the raw bytes, which is a wrong answer with
         * nothing to say so; refusing names the option instead. */
        if (ap && first_norm) {
            names_free(&key);
            names_free(&ignore);
            names_free(&compare);
            return fail_named("this port does not support this option on parquet input yet",
                              first_norm);
        }
        if (ap) {
            const int st = compare_parquet(a_path, b_path, &key, &ignore, &compare,
                                           threads, json_path);
            names_free(&key);
            names_free(&ignore);
            names_free(&compare);
            return st;
        }
    }

    int status = 2;
    Slab a = {0}, b = {0};
    a.fd = b.fd = -1;
    Names a_head = {0}, b_head = {0}, compared = {0};
    RowIndex ai = {0}, bi = {0};
    int *a_src = NULL, *b_src = NULL;
    char **want_a = NULL, **want_b = NULL;
    /* Declared here rather than where they are filled: the cleanup below frees
     * their name tables, and every `goto done` above that point would otherwise
     * be freeing an uninitialised pointer. */
    RowParser ap = {0}, bp = {0};
    Field *fa = NULL, *fb = NULL;
    int64_t *col_changed = NULL;
    int64_t *col_blanked = NULL;
    int64_t *col_filled = NULL;

    Phases whole;
    phases_init(&whole);
    if (!slab_open(&a, a_path)) { fail_named("cannot read one of the files", a_path); goto done; }
    if (!slab_open(&b, b_path)) { fail_named("cannot read one of the files", b_path); goto done; }

    a.dialect = detect_dialect(&a);
    b.dialect = detect_dialect(&b);
    size_t a_nl = next_of1(a.data, 0, a.size, '\n'), b_nl = next_of1(b.data, 0, b.size, '\n');
    char a_delim = detect_delimiter(a.data, a_nl), b_delim = detect_delimiter(b.data, b_nl);
    size_t a_start = 0, b_start = 0;
    /* A JSON file has no header row to skip, so its rows begin at byte zero. */
    bool a_ok = a.dialect == DIALECT_JSON ? json_header(&a, &a_head)
                                          : read_header(&a, a_delim, &a_head, &a_start);
    bool b_ok = b.dialect == DIALECT_JSON ? json_header(&b, &b_head)
                                          : read_header(&b, b_delim, &b_head, &b_start);
    if (!a_ok || !b_ok) {
        fail("file has no header row");
        goto done;
    }
    for (size_t i = 0; i < key.len; i++)
        if (name_index(&a_head, key.items[i]) < 0 || name_index(&b_head, key.items[i]) < 0) {
            fail_named("key column(s) missing from one of the files", key.items[i]);
            goto done;
        }
    /* A name in neither file is a typo, and this is the typo that hides: --key
     * makes the answer impossible and --compare refuses, but a misspelled
     * --ignore silently compares the column it meant to drop and calls it
     * changed on every row. In *neither* file rather than in both: ignore is
     * subtractive, so a name only one side carries is real and harmless. */
    for (size_t i = 0; i < ignore.len; i++)
        if (name_index(&a_head, ignore.items[i]) < 0 && name_index(&b_head, ignore.items[i]) < 0) {
            fail_named("ignore column(s) present in neither file", ignore.items[i]);
            goto done;
        }
    if (compare.len > 0) {
        /* Named explicitly: the order is the caller's, and a name that is not
         * in both files is an error rather than a column quietly dropped --
         * the same rule the other three ports apply. Key and ignored columns
         * are filtered out here as they are below, so `-k id -c id,a` compares
         * `a` rather than refusing. */
        for (size_t i = 0; i < compare.len; i++) {
            const char *c = compare.items[i];
            if (name_index(&a_head, c) < 0 || name_index(&b_head, c) < 0) {
                fail_named("compare column(s) not present in both files", c);
                goto done;
            }
            if (name_index(&key, c) >= 0 || name_index(&ignore, c) >= 0) continue;
            char *dup = strdup(c);
            if (!dup || !names_push(&compared, dup)) { free(dup); fail("out of memory"); goto done; }
        }
    } else {
        for (size_t i = 0; i < a_head.len; i++) {
            const char *c = a_head.items[i];
            if (name_index(&b_head, c) >= 0 && name_index(&key, c) < 0 && name_index(&ignore, c) < 0) {
                char *dup = strdup(c);
                if (!dup || !names_push(&compared, dup)) { free(dup); fail("out of memory"); goto done; }
            }
        }
    }

    size_t key_size = key.len, nc = compared.len, width = key_size + nc;
    a_src = malloc(width * sizeof *a_src);
    b_src = malloc(width * sizeof *b_src);
    want_a = calloc(width, sizeof *want_a);
    want_b = calloc(width, sizeof *want_b);
    fa = malloc(width * sizeof *fa);
    fb = malloc(width * sizeof *fb);
    col_changed = calloc(nc ? nc : 1, sizeof *col_changed);
    col_blanked = calloc(nc ? nc : 1, sizeof *col_blanked);
    col_filled = calloc(nc ? nc : 1, sizeof *col_filled);
    if (!a_src || !b_src || !want_a || !want_b || !fa || !fb || !col_changed || !col_blanked ||
        !col_filled) {
        fail("out of memory");
        goto done;
    }
    for (size_t i = 0; i < width; i++) {
        const char *n = i < key_size ? key.items[i] : compared.items[i - key_size];
        a_src[i] = name_index(&a_head, n);
        b_src[i] = name_index(&b_head, n);
    }

    ap.delimiter = a_delim; ap.source = a_src; ap.width = width; ap.dialect = a.dialect;
    bp.delimiter = b_delim; bp.source = b_src; bp.width = width; bp.dialect = b.dialect;
    ap.key_size = bp.key_size = key_size;
    const Norm norm_opts = { trim, ignore_case, empty_is_null, tolerance };
    const Norm *norm = trim || ignore_case || empty_is_null || tolerance > 0.0 ? &norm_opts : NULL;
    ap.last_needed = bp.last_needed = ap.key_last = bp.key_last = -1;
    for (size_t i = 0; i < width; i++) {
        if (a_src[i] > ap.last_needed) ap.last_needed = a_src[i];
        if (b_src[i] > bp.last_needed) bp.last_needed = b_src[i];
        if (i < key_size) {
            if (a_src[i] > ap.key_last) ap.key_last = a_src[i];
            if (b_src[i] > bp.key_last) bp.key_last = b_src[i];
        }
    }
    if (!parser_index_columns(&ap) || !parser_index_columns(&bp)) {
        fail("out of memory");
        goto done;
    }
    /* JSON looks a value up by name, so each parser is told which names it
     * wants and where each one belongs. A name the file does not have stays
     * NULL and its slot stays absent, which is how a column present on one side
     * only already behaves. */
    for (size_t i = 0; i < width; i++) {
        const char *n = i < key_size ? key.items[i] : compared.items[i - key_size];
        if (a_src[i] >= 0) want_a[i] = (char *)n;
        if (b_src[i] >= 0) want_b[i] = (char *)n;
    }
    if ((a.dialect == DIALECT_JSON && !parser_index_names(&ap, want_a, width)) ||
        (b.dialect == DIALECT_JSON && !parser_index_names(&bp, want_b, width))) {
        fail("out of memory");
        goto done;
    }

    /*
     * Past page cache the join runs inside A's sweep instead of after it.
     *
     * Past memory every pass is a read from disk, and the join used to be the
     * fourth: A's sweep, B's sweep, then both files again, front to back, for
     * the rows to compare -- on the 150M pair 131s of a 274s run, the time a
     * cold read of both files takes. Built first, B's index is all the join
     * needs from B, so A's rows can be joined as A's sweep finds them, while
     * their pages are still in memory, and A is read once instead of twice.
     *
     * The catch is repeated keys. The join compares a key's first row, and
     * which row is first is only known once the insertion has seen them all.
     * So every row is joined as though it were first, and the rows that turn
     * out to repeat a key -- `later`, recorded by the insertion -- are joined
     * again afterwards and subtracted. Counts are sums, so that is exact.
     *
     * Not with the normalisation flags: --ignore-case can refuse the whole run
     * from inside a comparison, and a row that is later found to repeat a key
     * would have refused for a row the join never compares.
     *
     * CSVDIFF_FUSED_JOIN=1 takes this path at any size, which is how the tests
     * reach it.
     */
    const unsigned all_threads = threads ? threads : cpu_count();
    const bool fused =
        !norm && ((a.size + b.size > (size_t)8 << 30 && all_threads > 1) || fused_forced());

    {
        /*
         * Both files at once, each on half the budget -- except when the input
         * is larger than page cache. The sweep inside a build is parallel but
         * the insertion after it is not, so run one after the other the machine
         * sits half idle through both serial tails; overlapped, one file's
         * insertion runs against the other's sweep.
         *
         * That overlap helps when both files fit in cache. When they do not --
         * 52GB of input against 14GB of RAM -- the two parallel sweeps thrash
         * the cache against each other, each evicting the other's pages, and
         * the overlap costs far more than the serial tails it hides. Sequential
         * keeps one file's pages hot through its sweep and insert before the
         * other starts.
         *
         * The threshold is deliberately simple: if the two files together exceed
         * 8GB, they are unlikely to both stay resident, so build sequentially.
         */
        unsigned budget = threads ? threads : cpu_count();
        BuildCtx bc = { { &ai, &bi }, { &a, &b }, { &ap, &bp }, { a_start, b_start },
                        key_size, budget > 1 ? budget / 2 : 1, norm, { false, false } };
        if (a.size + b.size > (size_t)8 << 30) {
            /*
             * Alone on the machine: the sweep inside each build gets the whole
             * budget, not the half the parallel overlap divides. Halving here
             * would leave half the cores idle through both sweeps for no
             * reason -- the point of going sequential was the page cache, not
             * the threads.
             */
            bc.threads = budget ? budget : 1;
            if (!fused) build_part(&bc, 0);
            build_part(&bc, 1);
        } else if (fused) {
            bc.threads = budget ? budget : 1;
            build_part(&bc, 1);
        } else {
            run_parts(build_part, &bc, 2);
        }
        phase_mark(&whole, fused ? "B index" : "both indexes");
        if ((!fused && !bc.ok[0]) || !bc.ok[1]) {
            if (budget_exceeded()) fail_budget();
            else fail(ai.failed || bi.failed ? "a field is larger than this engine packs"
                                             : "out of memory");
            goto done;
        }
    }

    int64_t matched = 0, changed = 0, added = 0, removed = 0;
    {
        unsigned budget = threads ? threads : cpu_count();
        /* Fused, A's keys are not counted yet: one part a sweep thread. */
        unsigned ways = fused ? (all_threads ? all_threads : 1)
                              : (ai.keys < (1u << 14) ? 1u : budget);
        /* The B pass only runs when it is being used to check the derivation. */
        const int verify = verify_added();
        unsigned b_ways = !verify ? 0u : (bi.keys < (1u << 14) ? 1u : budget);
        /* Fused, another `ways` parts at the end hold what is taken back. */
        const unsigned n_parts = ways + b_ways + (fused ? ways : 0);
        CmpPart *parts = calloc(n_parts, sizeof *parts);
        if (!parts) { fail("out of memory"); goto done; }
        /*
         * Can a prefix prove a row equal? Only where both files are CSV read
         * with the same delimiter and every compared column sits at the same
         * column number on both sides, ascending -- then equal bytes mean equal
         * columns, and the last compared column is the one that ends last.
         */
        bool aligned = a.dialect == DIALECT_CSV && b.dialect == DIALECT_CSV &&
                       a_delim == b_delim && nc > 0;
        for (size_t i = key_size; aligned && i < width; i++)
            aligned = a_src[i] >= 0 && a_src[i] == b_src[i] &&
                      (i == key_size || a_src[i] > a_src[i - 1]);
        const bool json_proof =
            a.dialect == DIALECT_JSON && b.dialect == DIALECT_JSON && nc > 0;
        bool keys_in_proof = json_proof;
        if (aligned) {
            keys_in_proof = true;
            for (size_t i = 0; keys_in_proof && i < key_size; i++)
                keys_in_proof = a_src[i] >= 0 && a_src[i] == b_src[i] &&
                                a_src[i] < a_src[width - 1];
        }
        /* The runs of wanted columns through the guard, and the unwanted ones
         * between them -- see `nseg`. */
        int nseg = 1, *runs = NULL;
        if (aligned && keys_in_proof && !json_proof) {
            const size_t g = (size_t)a_src[width - 1];
            unsigned char *wanted = calloc(g + 1, 1);
            runs = calloc(2 * (g + 2), sizeof *runs);
            if (!wanted || !runs) {
                free(wanted); free(runs); free(parts);
                fail("out of memory");
                goto done;
            }
            for (size_t i = 0; i < width; i++) wanted[a_src[i]] = 1;
            int *seg = runs, *gap = runs + g + 2;
            int j = 0;
            for (size_t col = 0; col <= g; col++) {
                if (!wanted[col]) { gap[j]++; continue; }
                if (gap[j]) j++;
                seg[j]++;
            }
            nseg = j + 1;
            free(wanted);
        }
        CmpCtx cc = { &ai, &bi, &a, &b, key_size, nc, width, ways, b_ways, parts,
                      aligned, width - 1, a_delim, json_proof, keys_in_proof,
                      /* Delimiters from the row start to the one that ends the
                       * guard column: the guard is source column a_src[width-1],
                       * so its end is the (a_src[width-1]+1)th delimiter. The
                       * keys sit before it when keys_in_proof holds. */
                      (aligned && keys_in_proof && !json_proof)
                          ? (int)(a_src[width - 1] + 1)
                          : 0,
                      norm, nseg, runs, runs ? runs + a_src[width - 1] + 2 : NULL };
        bool built = true;
        if (fused) {
            bool ok = true;
            for (unsigned p = 0; p < ways; p++) ok = part_init(&cc, &parts[p]) && ok;
            const RowSink sink = { fused_row, fused_reset, &cc };
            built = ok && index_build(&ai, &a, &ap, a_start, key_size, ways, norm, &sink);
            phase_mark(&whole, "A index, joined as it was read");
            if (built) {
                FixCtx fx = { &cc, parts + ways + b_ways, ways };
                for (unsigned p = 0; p < ways; p++) part_init(&cc, &fx.parts[p]);
                run_parts(fix_part, &fx, ways);
                if (b_ways) run_parts(b_pass, &cc, b_ways);
                phase_mark(&whole, "repeated keys taken back");
            }
        } else {
            run_parts(compare_part, &cc, ways + b_ways);
            phase_mark(&whole, "join and compare");
        }
        free(runs);

        bool oom = false;
        for (unsigned p = 0; p < n_parts; p++) {
            const CmpPart *q = &parts[p];
            /* The parts past `ways + b_ways` are the taken-back rows. */
            const int64_t sign = p < ways + b_ways ? 1 : -1;
            oom = oom || q->oom;
            matched += sign * q->matched;
            changed += sign * q->changed;
            removed += sign * q->removed;
            added += sign * q->added;
            for (size_t i = 0; i < nc && q->col_changed; i++) {
                col_changed[i] += sign * q->col_changed[i];
                col_blanked[i] += sign * q->col_blanked[i];
                col_filled[i] += sign * q->col_filled[i];
            }
        }
        for (unsigned p = 0; p < n_parts; p++) {
            free(parts[p].fa); free(parts[p].fb); free(parts[p].probe);
            free(parts[p].seg_lo); free(parts[p].seg_len);
            free(parts[p].col_changed); free(parts[p].col_blanked); free(parts[p].col_filled);
        }
        free(parts);
        if (!built) {
            if (budget_exceeded()) fail_budget();
            else fail(ai.failed ? "a field is larger than this engine packs" : "out of memory");
            goto done;
        }
        if (oom) { fail("out of memory"); goto done; }
        if (atomic_load(&fold_refused)) {
            fail("--ignore-case on a field outside ASCII needs Unicode case folding, which "
                 "this port does not carry; use another implementation for that data");
            goto done;
        }

        const int64_t derived = (int64_t)bi.keys - matched;
        if (verify && added != derived) {
            fprintf(stderr, "error: added counted %lld, derived %lld -- the join is "
                            "not symmetric on this input\n",
                    (long long)added, (long long)derived);
            status = 2;
            goto done;
        }
        added = derived;
    }

    {
        OutCol *out_cols = calloc(nc ? nc : 1, sizeof *out_cols);
        if (!out_cols) { fail("out of memory"); goto done; }
        for (size_t i = 0; i < nc; i++) {
            out_cols[i].name = compared.items[i];
            out_cols[i].changed = (long long)col_changed[i];
            out_cols[i].blanked = (long long)col_blanked[i];
            out_cols[i].filled = (long long)col_filled[i];
        }
        const Summary s = { "turbo", (long long)ai.rows, (long long)bi.rows,
                            (long long)ai.keys, (long long)bi.keys, matched, changed, added,
                            removed, (long long)ai.dup_keys, (long long)ai.dup_rows,
                            (long long)bi.dup_keys, (long long)bi.dup_rows, out_cols, nc };
        status = emit(&s, json_path);
        free(out_cols);
    }

done:
    index_free(&ai);
    index_free(&bi);
    free(a_src); free(b_src); free(want_a); free(want_b); free(fa); free(fb);
    free(ap.slot); free(bp.slot);
    free(ap.want_len); free(bp.want_len);
    free(ap.want_slot); free(bp.want_slot);
    free(ap.col_first); free(bp.col_first);
    free(ap.slot_next); free(bp.slot_next);
    free(col_changed); free(col_blanked); free(col_filled);
    names_free(&a_head); names_free(&b_head); names_free(&compared);
    names_free(&key); names_free(&ignore); names_free(&compare);
    slab_close(&a);
    slab_close(&b);
    return status;
}
