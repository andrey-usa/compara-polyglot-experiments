#include "csvdiff.hpp"
#include "pqdiff.hpp"

#include <fcntl.h>
#include "win32.hpp"   // O_BINARY off Windows; the mmap shim on it
#ifndef _WIN32
#include <sys/mman.h>
#endif
#include <sys/stat.h>
#include <unistd.h>

#if defined(__SSE2__) || defined(CSVDIFF_SCAN_AVX2) || defined(CSVDIFF_SCAN_AVX512)
#include <immintrin.h>
#endif

#include <algorithm>
#include <atomic>
#include <memory>
#include <bit>
#include <optional>
#include <thread>
#include <charconv>
#include <chrono>
#include <cmath>
#include <cstring>
#include <functional>
#include <stdexcept>

namespace csvdiff {
namespace {

// ---------------------------------------------------------------------------
// A field packed into one word: offset, length, and whether it needs unescaping.
//
// 40 bits of offset addresses a terabyte and 23 bits of length a field of eight
// megabytes, which is more than a CSV cell has any business being. An over-long
// field is reported rather than truncated through the mask.
// ---------------------------------------------------------------------------

using Field = std::uint64_t;

constexpr Field kAbsent = ~0ULL;
constexpr Field kTooLong = ~0ULL - 1;
constexpr std::uint64_t kOffsetMask = (1ULL << 40) - 1;
constexpr unsigned kLengthShift = 40;
constexpr std::uint64_t kLengthMask = (1ULL << 23) - 1;
constexpr std::uint64_t kEscaped = 1ULL << 63;
constexpr std::uint64_t kMaxFieldLen = kLengthMask;
constexpr unsigned kProofBackoff = 64;  // a power of two: the loop tests against it

Field pack(std::size_t offset, std::size_t len, bool escaped) {
    if (len > kMaxFieldLen) return kTooLong;
    return (offset & kOffsetMask) | ((std::uint64_t(len) & kLengthMask) << kLengthShift) |
           (escaped ? kEscaped : 0);
}

std::size_t offset_of(Field f) { return f & kOffsetMask; }
std::size_t len_of(Field f) { return (f >> kLengthShift) & kLengthMask; }
bool is_escaped(Field f) { return (f & kEscaped) != 0; }
bool is_real(Field f) { return f != kAbsent && f != kTooLong; }

// ---------------------------------------------------------------------------
// SWAR scanning: eight bytes per step, using arithmetic rather than comparison.
// ---------------------------------------------------------------------------

constexpr std::uint64_t kOnes = 0x0101010101010101ULL;
constexpr std::uint64_t kHigh = 0x8080808080808080ULL;

std::uint64_t broadcast(unsigned char b) { return std::uint64_t(b) * kOnes; }

// Sets the high bit of every byte of `word` equal to `target`. Subtracting ones
// borrows across a byte only where that byte was zero, and `~diff` cancels the
// false positives the borrow creates.
std::uint64_t match_bits(std::uint64_t word, std::uint64_t target) {
    const std::uint64_t diff = word ^ target;
    return (diff - kOnes) & ~diff & kHigh;
}

// std::byteswap is C++23; this keeps the port buildable on a C++20 compiler.
// Marked maybe_unused because the branch that calls it is dead on a
// little-endian target, which clang treats as an error under -Werror.
[[maybe_unused]] constexpr std::uint64_t bswap64(std::uint64_t v) {
    return ((v & 0x00000000000000FFULL) << 56) | ((v & 0x000000000000FF00ULL) << 40) |
           ((v & 0x0000000000FF0000ULL) << 24) | ((v & 0x00000000FF000000ULL) << 8) |
           ((v & 0x000000FF00000000ULL) >> 8) | ((v & 0x0000FF0000000000ULL) >> 24) |
           ((v & 0x00FF000000000000ULL) >> 40) | ((v & 0xFF00000000000000ULL) >> 56);
}

// Reads eight bytes as one little-endian word. The SWAR tricks below all assume
// the first byte of the file is the lowest byte of the word.
// How far two byte ranges agree, a word at a time.
//
// The join's expensive half is not the lookup -- it is what happens after one:
// the mate's row is parsed a second time and every compared column is compared
// byte by byte. But rows that match usually match because they are the same row
// with one column moved, so comparing the two rows' raw bytes from the front
// finds where they first diverge, and every compared column that closes before
// that point is proven equal without the mate being parsed at all.
inline std::size_t common_prefix(const char* x, const char* y, std::size_t n);

std::uint64_t load64(const char* p) {
    std::uint64_t w;
    std::memcpy(&w, p, sizeof w);
    if constexpr (std::endian::native == std::endian::big) w = bswap64(w);
    return w;
}

inline std::size_t common_prefix(const char* x, const char* y, std::size_t n) {
    std::size_t i = 0;
    for (; i + 8 <= n; i += 8) {
        const std::uint64_t a = load64(x + i), b = load64(y + i);
        if (a != b) return i + (static_cast<std::size_t>(__builtin_ctzll(a ^ b)) >> 3);
    }
    for (; i < n && x[i] == y[i]; ++i) {}
    return i;
}

// Whether two names are the same bytes, inline and a word at a time. A
// `string == string_view` of run-time length is a `memcmp` call, and the JSON
// parse checks its guessed slot for every member of every row: 48M calls on a
// 2M ndjson pair. Names are short, so the words overlap rather than loop far.
[[gnu::always_inline]] inline bool same_name(std::string_view a, std::string_view b) {
    const std::size_t n = a.size();
    if (n != b.size()) return false;
    const char *x = a.data(), *y = b.data();
    if (n >= 8) {
        for (std::size_t i = 0; i + 8 < n; i += 8)
            if (load64(x + i) != load64(y + i)) return false;
        return load64(x + n - 8) == load64(y + n - 8);
    }
    if (n >= 4) {
        std::uint32_t x0, x1, y0, y1;
        std::memcpy(&x0, x, 4);
        std::memcpy(&y0, y, 4);
        std::memcpy(&x1, x + n - 4, 4);
        std::memcpy(&y1, y + n - 4, 4);
        return x0 == y0 && x1 == y1;
    }
    for (std::size_t i = 0; i < n; ++i)
        if (x[i] != y[i]) return false;
    return true;
}

// The three scanners, chosen at build time rather than dispatched at run time:
// a benchmark of an instruction set should not be measuring a function pointer,
// and each binary is what you would actually ship for that target. SWAR is the
// default because it is the only one that needs no CPU feature at all; the
// vector builds start where they can and hand the tail to SWAR.
std::size_t next_of2(std::string_view d, std::size_t from, std::size_t end, char a, char b) {
    std::size_t at = from;
#if defined(CSVDIFF_SCAN_AVX512)
    {
        const __m512i va = _mm512_set1_epi8(a), vb = _mm512_set1_epi8(b);
        for (; at + 64 <= end; at += 64) {
            const __m512i chunk = _mm512_loadu_si512(d.data() + at);
            const __mmask64 hits =
                _mm512_cmpeq_epi8_mask(chunk, va) | _mm512_cmpeq_epi8_mask(chunk, vb);
            if (hits) return at + static_cast<std::size_t>(__builtin_ctzll(hits));
        }
    }
#elif defined(CSVDIFF_SCAN_AVX2)
    {
        const __m256i va = _mm256_set1_epi8(a), vb = _mm256_set1_epi8(b);
        for (; at + 32 <= end; at += 32) {
            const __m256i chunk =
                _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d.data() + at));
            const unsigned hits = static_cast<unsigned>(_mm256_movemask_epi8(
                _mm256_or_si256(_mm256_cmpeq_epi8(chunk, va), _mm256_cmpeq_epi8(chunk, vb))));
            if (hits) return at + static_cast<std::size_t>(__builtin_ctz(hits));
        }
    }
#endif
    // Sixteen bytes, and it is a rung this port did not have. The ladder above
    // is opt-in and the one below is eight bytes, so a field shorter than the
    // wide step fell straight from 32 to 8 -- which is why turning the wide step
    // on by default measured *slower* on ndjson and did nothing on CSV. The C
    // port has had this rung all along and its note is the argument for it:
    // fields here average about ten bytes, "an eight-byte step needs two
    // iterations for a typical field and a sixteen-byte one needs a single
    // iteration". SSE2 is not a target question on x86-64 -- the architecture
    // always has it -- so this needs no `-march=` and no define, and anything
    // that is not x86 falls through to the same SWAR loop it always did.
#if defined(__SSE2__)
    {
        const __m128i va = _mm_set1_epi8(a), vb = _mm_set1_epi8(b);
        for (; at + 16 <= end; at += 16) {
            const __m128i w = _mm_loadu_si128(reinterpret_cast<const __m128i*>(d.data() + at));
            const unsigned hits = static_cast<unsigned>(
                _mm_movemask_epi8(_mm_or_si128(_mm_cmpeq_epi8(w, va), _mm_cmpeq_epi8(w, vb))));
            if (hits) return at + static_cast<std::size_t>(__builtin_ctz(hits));
        }
    }
#endif
    const std::uint64_t ba = broadcast(static_cast<unsigned char>(a));
    const std::uint64_t bb = broadcast(static_cast<unsigned char>(b));
    for (; at + 8 <= end; at += 8) {
        const std::uint64_t w = load64(d.data() + at);
        const std::uint64_t hits = match_bits(w, ba) | match_bits(w, bb);
        if (hits) return at + (std::countr_zero(hits) >> 3);
    }
    for (; at < end; ++at)
        if (d[at] == a || d[at] == b) return at;
    return end;
}

std::size_t next_of1(std::string_view d, std::size_t from, std::size_t end, char target) {
    std::size_t at = from;
#if defined(CSVDIFF_SCAN_AVX512)
    {
        const __m512i vt = _mm512_set1_epi8(target);
        for (; at + 64 <= end; at += 64) {
            const __m512i chunk = _mm512_loadu_si512(d.data() + at);
            const __mmask64 hits = _mm512_cmpeq_epi8_mask(chunk, vt);
            if (hits) return at + static_cast<std::size_t>(__builtin_ctzll(hits));
        }
    }
#elif defined(CSVDIFF_SCAN_AVX2)
    {
        const __m256i vt = _mm256_set1_epi8(target);
        for (; at + 32 <= end; at += 32) {
            const __m256i chunk =
                _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d.data() + at));
            const unsigned hits =
                static_cast<unsigned>(_mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, vt)));
            if (hits) return at + static_cast<std::size_t>(__builtin_ctz(hits));
        }
    }
#elif defined(__AVX2__)
    // Unlike `next_of2`'s, this wide step is on wherever the build has AVX2
    // (`-march=native`, so every runner). Its hot caller is the ndjson row end:
    // once the key-only parse has its keys, the rest of the row -- two hundred
    // bytes -- is one scan for the newline, which the SWAR loop below took
    // eight bytes at a time. The C port measured the same change 26% faster on
    // its ndjson sweep (#155).
    {
        const __m256i vt = _mm256_set1_epi8(target);
        for (; at + 32 <= end; at += 32) {
            const __m256i chunk =
                _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d.data() + at));
            const unsigned hits =
                static_cast<unsigned>(_mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, vt)));
            if (hits) return at + static_cast<std::size_t>(__builtin_ctz(hits));
        }
    }
#endif
#if defined(__SSE2__)
    {
        const __m128i vt = _mm_set1_epi8(target);
        for (; at + 16 <= end; at += 16) {
            const __m128i w = _mm_loadu_si128(reinterpret_cast<const __m128i*>(d.data() + at));
            const unsigned hits = static_cast<unsigned>(_mm_movemask_epi8(_mm_cmpeq_epi8(w, vt)));
            if (hits) return at + static_cast<std::size_t>(__builtin_ctz(hits));
        }
    }
#endif
    const std::uint64_t bt = broadcast(static_cast<unsigned char>(target));
    for (; at + 8 <= end; at += 8) {
        const std::uint64_t w = load64(d.data() + at);
        const std::uint64_t hits = match_bits(w, bt);
        if (hits) return at + (std::countr_zero(hits) >> 3);
    }
    for (; at < end; ++at)
        if (d[at] == target) return at;
    return end;
}

// Walks past a quoted field's body. A doubled quote inside is content.
std::size_t skip_quoted(std::string_view d, std::size_t from, std::size_t end) {
    std::size_t at = from;
    for (;;) {
        const std::size_t q = next_of1(d, at, end, '"');
        if (q >= end) return end;
        if (q + 1 < end && d[q + 1] == '"') {
            at = q + 2;
            continue;
        }
        return q + 1;
    }
}

// The proof's byte span without parsing the row: through the delimiter that
// closes source column `commas - 1`, which is the `commas`th delimiter from the
// row start -- countable with the delimiter cursor alone, skipping a quoted
// field whole the way `parse_csv` does. The join used to parse every column of
// every row of A to read that one offset off the guard field. Returns 0 where
// the row ends first -- a short row, or a malformed one -- and the caller
// parses instead.
//
// The count is exact on well-formed rows: a field that opens with a quote is
// skipped whole, so a delimiter inside one is never counted, and a quote
// anywhere else is data, as the parse treats it. Where the bytes are not well
// formed the span can only come out short of where the parse would put it, and
// a short span fails the proof or falls back; it never proves a row the parse
// would not. The C port has the same function (#178).
//
// The walk itself is `FieldWalker`, which the gapped proof also uses to skip
// the unwanted columns between runs of wanted ones, in both rows.
class FieldWalker {
  public:
    FieldWalker(std::string_view d, std::size_t lo, std::size_t hi, char delim)
        : d_(d), hi_(hi), delim_(delim)
#if defined(__AVX2__)
          , held_(lo), scanned_(lo), va_(_mm256_set1_epi8(delim)), vb_(_mm256_set1_epi8('\n'))
#endif
    {
        (void)lo;
    }

    static constexpr std::size_t kShort = static_cast<std::size_t>(-1);

    // Past the byte that closes the `n`th field from `at` -- or kShort where the
    // row ends first. With `may_end` the last of them may close on the line
    // ending: where the last compared column is the row's last column, that is
    // the only byte that can close it. A line ending before the last is a short
    // row either way.
    std::size_t past(std::size_t at, std::size_t n, bool may_end) {
        const std::string_view d = d_;
        for (std::size_t i = 0; i < n; ++i) {
            const std::size_t cur =
                at < hi_ && d[at] == '"' ? next_delim(skip_quoted(d, at + 1, hi_)) : next_delim(at);
            if (cur >= hi_) return kShort;
            if (d[cur] != delim_ && !(may_end && i + 1 == n)) return kShort;
            at = cur + 1;
        }
        return at;
    }

  private:
#if defined(__AVX2__)
    // The cursor `parse_csv` keeps.
    std::size_t next_delim(std::size_t from) {
        while (bits_) {
            const std::size_t at = held_ + static_cast<std::size_t>(std::countr_zero(bits_));
            bits_ &= bits_ - 1;
            if (at >= from) return at;
        }
        std::size_t at = std::max(from, scanned_);
        for (; at + 32 <= hi_; at += 32) {
            const __m256i w = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d_.data() + at));
            const auto hits = static_cast<std::uint32_t>(_mm256_movemask_epi8(
                _mm256_or_si256(_mm256_cmpeq_epi8(w, va_), _mm256_cmpeq_epi8(w, vb_))));
            scanned_ = at + 32;
            if (hits) {
                held_ = at;
                bits_ = hits & (hits - 1);
                return at + static_cast<std::size_t>(std::countr_zero(hits));
            }
        }
        const std::size_t next = next_of2(d_, at, hi_, delim_, '\n');
        scanned_ = next < hi_ ? next + 1 : hi_;
        return next;
    }
#else
    std::size_t next_delim(std::size_t from) const { return next_of2(d_, from, hi_, delim_, '\n'); }
#endif

    std::string_view d_;
    std::size_t hi_;
    char delim_;
#if defined(__AVX2__)
    std::size_t held_, scanned_;
    std::uint32_t bits_ = 0;
    __m256i va_, vb_;
#endif
};

std::size_t guard_span(std::string_view d, std::size_t lo, std::size_t hi, char delim,
                       std::size_t commas) {
    FieldWalker w(d, lo, hi, delim);
    const std::size_t at = w.past(lo, commas, true);
    return at == FieldWalker::kShort ? 0 : at - lo;
}

// ---------------------------------------------------------------------------
// The mapped file
// ---------------------------------------------------------------------------

// Which text dialect a mapped file is in. It rides on the Slab because the
// escape rule has to follow the file: CSV doubles a quote, JSON puts a
// backslash in front of one, and `for_each_byte` is the single route every
// comparison reads a field through.
enum class Dialect { Csv, Json };

class Slab {
  public:
    explicit Slab(const std::string& path) {
        fd_ = ::open(path.c_str(), O_RDONLY | O_BINARY);
        if (fd_ < 0) throw Error("cannot read " + path);
        struct stat st{};
        if (::fstat(fd_, &st) != 0) {
            ::close(fd_);
            throw Error("cannot read " + path);
        }
        size_ = static_cast<std::size_t>(st.st_size);
        if (size_ > 0) {
            void* p = ::mmap(nullptr, size_, PROT_READ, MAP_PRIVATE, fd_, 0);
            if (p == MAP_FAILED) {
                ::close(fd_);
                throw Error("cannot map " + path);
            }
            data_ = static_cast<const char*>(p);
            // The whole file is read once, front to back, exactly once per pass.
            ::madvise(const_cast<void*>(p), size_, MADV_SEQUENTIAL);
        }
    }

    ~Slab() {
        if (data_) ::munmap(const_cast<char*>(data_), size_);
        if (fd_ >= 0) ::close(fd_);
    }

    Slab(const Slab&) = delete;
    Slab& operator=(const Slab&) = delete;

    Dialect dialect() const { return dialect_; }
    void set_dialect(Dialect d) { dialect_ = d; }

    std::string_view bytes() const { return {data_, size_}; }

    // The field's raw span, still holding any doubled quotes.
    std::string_view raw(Field f) const {
        if (!is_real(f)) return {};
        return {data_ + offset_of(f), len_of(f)};
    }

  private:
    int fd_ = -1;
    const char* data_ = nullptr;
    std::size_t size_ = 0;
    Dialect dialect_ = Dialect::Csv;
};

// A field's logical bytes: the raw span with the second quote of each doubled
// pair dropped. Equality, hashing and decoding all read a field through this one
// route, so they cannot disagree about its value — the property whose absence
// produced two silently wrong answers in the Java port.
template <typename Fn>
void for_each_byte(const Slab& s, Field f, Fn&& fn) {
    const std::string_view raw = s.raw(f);
    if (!is_real(f) || !is_escaped(f)) {
        for (char c : raw) fn(static_cast<unsigned char>(c));
        return;
    }
    if (s.dialect() == Dialect::Csv) {
        for (std::size_t i = 0; i < raw.size(); ++i) {
            const char c = raw[i];
            fn(static_cast<unsigned char>(c));
            if (c == '"' && i + 1 < raw.size() && raw[i + 1] == '"') ++i;
        }
        return;
    }
    // JSON: a backslash escape. \uXXXX is decoded to UTF-8 so that a value
    // written escaped and the same value written literally compare equal --
    // which they must, because a JSON writer is free to escape either way.
    for (std::size_t i = 0; i < raw.size(); ++i) {
        if (raw[i] != '\\' || i + 1 >= raw.size()) {
            fn(static_cast<unsigned char>(raw[i]));
            continue;
        }
        const char e = raw[++i];
        switch (e) {
            case 'n': fn('\n'); break;
            case 't': fn('\t'); break;
            case 'r': fn('\r'); break;
            case 'b': fn('\b'); break;
            case 'f': fn('\f'); break;
            case '"': fn('"'); break;
            case '\\': fn('\\'); break;
            case '/': fn('/'); break;
            case 'u': {
                unsigned cp = 0;
                if (i + 4 < raw.size()) {
                    bool ok = true;
                    for (int k = 1; k <= 4 && ok; ++k) {
                        const char h = raw[i + static_cast<std::size_t>(k)];
                        const unsigned d = h >= '0' && h <= '9'   ? unsigned(h - '0')
                                           : h >= 'a' && h <= 'f' ? unsigned(h - 'a' + 10)
                                           : h >= 'A' && h <= 'F' ? unsigned(h - 'A' + 10)
                                                                  : 16u;
                        if (d == 16u) ok = false;
                        cp = cp * 16 + (ok ? d : 0);
                    }
                    if (!ok) {  // not four hex digits: emit it as written
                        fn('\\');
                        fn(static_cast<unsigned char>(e));
                        break;
                    }
                    i += 4;
                    // A surrogate pair is one code point in two escapes.
                    if (cp >= 0xD800 && cp <= 0xDBFF && i + 6 < raw.size() &&
                        raw[i + 1] == '\\' && raw[i + 2] == 'u') {
                        unsigned lo = 0;
                        bool ok2 = true;
                        for (int k = 3; k <= 6 && ok2; ++k) {
                            const char h = raw[i + static_cast<std::size_t>(k)];
                            const unsigned d = h >= '0' && h <= '9'   ? unsigned(h - '0')
                                               : h >= 'a' && h <= 'f' ? unsigned(h - 'a' + 10)
                                               : h >= 'A' && h <= 'F' ? unsigned(h - 'A' + 10)
                                                                      : 16u;
                            if (d == 16u) ok2 = false;
                            lo = lo * 16 + (ok2 ? d : 0);
                        }
                        if (ok2 && lo >= 0xDC00 && lo <= 0xDFFF) {
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            i += 6;
                        }
                    }
                    if (cp < 0x80) {
                        fn(static_cast<unsigned char>(cp));
                    } else if (cp < 0x800) {
                        fn(static_cast<unsigned char>(0xC0 | (cp >> 6)));
                        fn(static_cast<unsigned char>(0x80 | (cp & 0x3F)));
                    } else if (cp < 0x10000) {
                        fn(static_cast<unsigned char>(0xE0 | (cp >> 12)));
                        fn(static_cast<unsigned char>(0x80 | ((cp >> 6) & 0x3F)));
                        fn(static_cast<unsigned char>(0x80 | (cp & 0x3F)));
                    } else {
                        fn(static_cast<unsigned char>(0xF0 | (cp >> 18)));
                        fn(static_cast<unsigned char>(0x80 | ((cp >> 12) & 0x3F)));
                        fn(static_cast<unsigned char>(0x80 | ((cp >> 6) & 0x3F)));
                        fn(static_cast<unsigned char>(0x80 | (cp & 0x3F)));
                    }
                } else {
                    fn('\\');
                    fn(static_cast<unsigned char>(e));
                }
                break;
            }
            default:  // not an escape this dialect defines: emit both bytes
                fn('\\');
                fn(static_cast<unsigned char>(e));
                break;
        }
    }
}

std::string text_of(const Slab& s, Field f) {
    if (!is_real(f) || !is_escaped(f)) return std::string(s.raw(f));
    std::string out;
    out.reserve(s.raw(f).size());
    for_each_byte(s, f, [&](unsigned char b) { out.push_back(static_cast<char>(b)); });
    return out;
}

// Forced inline: `same` is the call that matters, and a second caller in the
// normalising path was enough for GCC to emit one shared copy and call it --
// 8% more instructions on a 200k CSV pair, none of it on a path that normalises.
[[gnu::always_inline]] inline bool same_bytes(const Slab& a, Field x, const Slab& b, Field y) {
    const bool ex = is_real(x) && is_escaped(x);
    const bool ey = is_real(y) && is_escaped(y);
    if (!ex && !ey) return a.raw(x) == b.raw(y);
    std::string lx, ly;
    for_each_byte(a, x, [&](unsigned char c) { lx.push_back(static_cast<char>(c)); });
    for_each_byte(b, y, [&](unsigned char c) { ly.push_back(static_cast<char>(c)); });
    return lx == ly;
}

// `same_bytes` for two present values, with the escaped case -- a doubled
// quote, a JSON escape -- left out of line, so the join's loop inlines only the
// compare it nearly always makes.
[[gnu::noinline]] bool same_escaped(const Slab& a, Field x, const Slab& b, Field y) {
    return same_bytes(a, x, b, y);
}
[[gnu::always_inline]] inline bool same_plain(const Slab& a, Field x, const Slab& b, Field y) {
    if (is_escaped(x) || is_escaped(y)) return same_escaped(a, x, b, y);
    return a.raw(x) == b.raw(y);
}

// ---------------------------------------------------------------------------
// Values: raw bytes on the fast path, normalised strings when asked
// ---------------------------------------------------------------------------

bool needs_normalising(const Options& o) {
    return o.trim || o.ignore_case || o.empty_is_null || o.tolerance > 0.0;
}

std::string_view trimmed(std::string_view s) {
    const auto space = [](char c) { return static_cast<unsigned char>(c) <= ' '; };
    while (!s.empty() && space(s.front())) s.remove_prefix(1);
    while (!s.empty() && space(s.back())) s.remove_suffix(1);
    return s;
}

// How `value_of` treats a byte outside ASCII under --ignore-case.
//
// `compare` is for a value whose folded form decides an answer -- a key, which
// is hashed folded, and a cell that differs byte for byte. `display` is for a
// value that is only shown: a row sample, or a cell already known to differ.
// Refusing there made the exit status depend on `--json`, since only a run that
// writes samples builds them.
enum class Fold { compare, display };

// The field as the value a row-at-a-time engine would have built for it: empty
// is absent, then --trim, then --ignore-case, then --empty-is-null.
Val value_of(const Slab& s, Field f, const Options& o, Fold fold = Fold::compare) {
    if (!is_real(f)) return std::nullopt;
    std::string text = text_of(s, f);
    if (text.empty()) return std::nullopt;
    if (o.trim) text = std::string(trimmed(text));
    if (o.ignore_case) {
        // Folding case outside ASCII needs a Unicode table this port does not
        // carry, and folding it partially is worse than not folding it: CAFE and
        // cafe with an acute would compare equal in the ports that do fold and
        // unequal here, and nothing in the output would say why. So the ASCII
        // path is taken where it is provably right, and anything else is
        // refused by name -- where the fold decides something. A value that is
        // only displayed is shown as it is, unfolded. See cpp/README.md.
        const bool ascii = std::none_of(text.begin(), text.end(),
                                        [](unsigned char c) { return c >= 0x80; });
        if (!ascii && fold == Fold::compare)
            throw Error(
                "--ignore-case on a field outside ASCII needs Unicode case folding, which "
                "this port does not carry; use another implementation for that data");
        if (ascii)
            std::transform(text.begin(), text.end(), text.begin(), [](unsigned char c) {
                return static_cast<char>(std::tolower(c));
            });
    }
    if (o.empty_is_null && text.empty()) return std::nullopt;
    return text;
}

// The normalising tail lives out of line so `is_absent` can be inlined.
//
// `is_absent` answers the common case in about ten instructions -- two loads, a
// shift and a compare -- and then falls back to `value_of`, which builds a
// std::string. A body that can allocate is over the compiler's inlining
// threshold, so the whole predicate was a real call, prologue and all, and the
// ten instructions cost thirty-two. On a 200k CSV pair it is asked 2.45M times
// (1.21M from `same`, 800k from the index sweep, 437k from `compare`) for 78M
// instructions, 6.5% of the run. The C port spends none: it has no such
// predicate, because its absent case is a sentinel the caller tests inline.
//
// Worth 0.978 on CSV, middle half 0.947-0.994 over eleven paired rounds on a 4M
// pair, faster in nine of them. ndjson measured 0.984 with the middle half
// 0.978-1.020, which crosses one, so nothing is claimed for that format.
//
// **`same` is deliberately not given the same treatment**, and the reason is
// measured rather than aesthetic: inlining it too regressed ndjson to 1.034,
// middle half 1.023-1.069, faster in one round of eleven. It also *added* 14M
// instructions, because an inlined `same` duplicates the `is_absent` work at
// each of its call sites. Fewer calls is not the same thing as less work, and
// neither is the same thing as less time.
//
// The tail is not on a path the benchmark takes: `--trim`, `--ignore-case`,
// `--empty-is-null` and `--tolerance` are all off by default, and
// `needs_normalising` is what decides. Where one is on, the cost is the call it
// always was.
//
// Absence never depends on case, so this does not fold: an absent test that
// folded refused a non-ASCII value whose only question was whether it was there.
bool absent_normalised(const Slab& s, Field f, const Options& o) {
    if (!is_real(f)) return true;
    const std::string text = text_of(s, f);
    if (text.empty()) return true;
    return o.empty_is_null && o.trim && trimmed(text).empty();
}

inline bool is_absent(const Slab& s, Field f, const Options& o) {
    if (!is_real(f) || len_of(f) == 0) return true;
    if (!needs_normalising(o)) return false;
    return absent_normalised(s, f, o);
}

// Split out for the same reason, but `same` itself stays out of line -- see
// above for the measurement that decided it.
bool same_normalised(const Slab& a, Field x, const Slab& b, Field y, const Options& o) {
    // Identical bytes normalise identically, whatever they hold. Asked first, a
    // value outside ASCII that did not change is not a reason to refuse the run
    // under --ignore-case: the port that folds it gives the same answer.
    if (same_bytes(a, x, b, y)) return true;
    return value_of(a, x, o) == value_of(b, y, o);
}

bool same(const Slab& a, Field x, const Slab& b, Field y, const Options& o) {
    // The options are asked once. Through `is_absent` they were asked for each
    // side and then again here, three times a cell on the path every run takes.
    if (!needs_normalising(o)) {
        const bool xa = !is_real(x) || len_of(x) == 0, yb = !is_real(y) || len_of(y) == 0;
        if (xa || yb) return xa && yb;
        return same_bytes(a, x, b, y);
    }
    const bool xa = absent_normalised(a, x, o), yb = absent_normalised(b, y, o);
    if (xa || yb) return xa && yb;
    return same_normalised(a, x, b, y, o);
}

// Deliberately stricter than strtod: "inf" and "nan" are ordinary text in a CSV,
// and treating them as numbers would make two unequal strings compare equal.
std::optional<double> as_number(std::string_view s) {
    s = trimmed(s);
    if (s.empty()) return std::nullopt;
    std::string_view body = s;
    if (body.front() == '+' || body.front() == '-') body.remove_prefix(1);
    if (body.empty()) return std::nullopt;
    if (!(std::isdigit(static_cast<unsigned char>(body.front())) || body.front() == '.'))
        return std::nullopt;
    for (char c : body)
        if (!(std::isdigit(static_cast<unsigned char>(c)) || c == '.' || c == 'e' || c == 'E' ||
              c == '+' || c == '-'))
            return std::nullopt;
    double out = 0;
    const auto* first = s.data();
    const auto res = std::from_chars(first, first + s.size(), out);
    if (res.ec != std::errc() || res.ptr != first + s.size()) return std::nullopt;
    if (!std::isfinite(out)) return std::nullopt;
    return out;
}

// SQL's IS DISTINCT FROM, with the tolerance applied where both sides parse.
//
// Forced inline because the join's inner loop is the only caller that matters
// and it wants this in its body. The report path calls it too, and two call
// sites were enough for GCC to stop inlining it and emit one shared copy --
// which cost the join 3%, in both `--json` and summary-only runs. The report
// loop is cold; a second inlined copy there is free.
[[gnu::always_inline]] inline bool cell_differs(const Slab& a, Field x, const Slab& b, Field y, const Options& o) {
    const bool xa = is_absent(a, x, o), yb = is_absent(b, y, o);
    if (xa && yb) return false;
    if (o.tolerance > 0.0 && !xa && !yb) {
        const auto nx = as_number(text_of(a, x));
        const auto ny = as_number(text_of(b, y));
        if (nx && ny) return std::fabs(*nx - *ny) > o.tolerance;
    }
    return !same(a, x, b, y, o);
}

// FNV-1a over exactly the bytes equality compares, by the same route.
//
// A hash is internal -- nothing outside this file can see one -- so the only
// property it owes anyone is that the index build and the join probe compute the
// same number for the same bytes. That is what lets the common path read eight
// bytes at a time: a key of twenty-six bytes costs four multiplies instead of
// twenty-six, and at ten million rows the key hash is computed four times over
// (both files, indexed then probed), which made byte-at-a-time FNV about a
// billion dependent multiply-xor steps of the run.
//
// The escaped and normalised paths stay byte-at-a-time, because there the bytes
// are produced one at a time anyway.
// The last `rem` bytes of p[0..len), one to seven of them, as a little-endian
// word padded with zeros -- from loads rather than a copy into a zeroed buffer,
// which is a call to memcpy for a length only known at run time: two a row in
// the index build. A value of eight bytes or more has eight real bytes ending
// where the tail does, so one load and a shift; a shorter one is two
// overlapping halves, or its first, middle and last byte. The other three
// ports read their tails the same way.
[[gnu::always_inline]] inline std::uint64_t tail_word(const unsigned char* p, std::size_t len,
                                                      std::size_t rem) {
    if (len >= 8) return load64(reinterpret_cast<const char*>(p) + len - 8) >> (8 * (8 - rem));
    if (len >= 4) {
        std::uint32_t lo, hi;
        std::memcpy(&lo, p, 4);
        std::memcpy(&hi, p + len - 4, 4);
        if constexpr (std::endian::native == std::endian::big) {
            lo = __builtin_bswap32(lo);
            hi = __builtin_bswap32(hi);
        }
        return std::uint64_t(lo) | (std::uint64_t(hi) << (8 * (len - 4)));
    }
    return std::uint64_t(p[0]) | (std::uint64_t(p[len / 2]) << (8 * (len / 2))) |
           (std::uint64_t(p[len - 1]) << (8 * (len - 1)));
}

std::uint64_t hash_bytes(const unsigned char* p, std::size_t n, std::uint64_t seed) {
    constexpr std::uint64_t kPrime = 0x100000001b3ULL;
    std::uint64_t h = seed;
    const unsigned char* const base = p;
    const std::size_t total = n;
    while (n >= 8) {
        std::uint64_t word;
        std::memcpy(&word, p, sizeof word);
        h = (h ^ word) * kPrime;
        h ^= h >> 29;  // the xor-shift is what spreads a whole word into the low bits
        p += 8;
        n -= 8;
    }
    if (n > 0) {
        h = (h ^ tail_word(base, total, n)) * kPrime;
        h ^= h >> 29;
    }
    return h;
}

std::uint64_t hash_field(const Slab& s, Field f, const Options& o, std::uint64_t seed) {
    constexpr std::uint64_t kPrime = 0x100000001b3ULL;
    std::uint64_t h = seed;
    const bool normalising = needs_normalising(o);
    const bool absent = !is_real(f) || len_of(f) == 0 || (normalising && absent_normalised(s, f, o));
    if (absent) return (h ^ 0x9e3779b97f4a7c15ULL) * kPrime;
    std::uint64_t len = 0;
    if (!normalising && !is_escaped(f)) {
        const std::string_view raw = s.raw(f);
        h = hash_bytes(reinterpret_cast<const unsigned char*>(raw.data()), raw.size(), h);
        len = raw.size();
    } else if (normalising) {
        const std::string v = value_of(s, f, o).value_or(std::string());
        for (unsigned char b : v) {
            h = (h ^ b) * kPrime;
            ++len;
        }
    } else {
        // Decoded, then folded eight bytes at a time like the branch above, with
        // the words assembled as the bytes come: a field may be megabytes.
        // Folding them one at a time gave the same value a different hash, so a
        // key a JSON writer escaped (`a\/b`, `\u0041`) never found the same key
        // written literally in the other file.
        std::uint64_t word = 0;
        unsigned have = 0;
        for_each_byte(s, f, [&](unsigned char b) {
            word |= static_cast<std::uint64_t>(b) << (8 * have);
            ++have;
            ++len;
            if (have == 8) {
                h = (h ^ word) * kPrime;
                h ^= h >> 29;
                word = 0;
                have = 0;
            }
        });
        if (have > 0) {
            h = (h ^ word) * kPrime;
            h ^= h >> 29;
        }
    }
    return (h ^ len) * kPrime;
}

std::uint64_t key_hash(const Slab& s, const Field* fields, std::size_t key_size, const Options& o) {
    std::uint64_t h = 0xcbf29ce484222325ULL;
    for (std::size_t i = 0; i < key_size; ++i) h = hash_field(s, fields[i], o, h);
    return h;
}

// Orders key values the way every other port writes its sections: ascending,
// absent values last.
int compare_keys(const std::vector<Val>& x, const std::vector<Val>& y, std::size_t key_size) {
    for (std::size_t i = 0; i < key_size; ++i) {
        const bool xn = !x[i].has_value(), yn = !y[i].has_value();
        if (xn && yn) continue;
        if (xn) return 1;
        if (yn) return -1;
        const int c = x[i]->compare(*y[i]);
        if (c != 0) return c;
    }
    return 0;
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

// Splits rows into fields, projecting straight to the columns asked for. Once
// the last needed column has been read the rest of the row is skipped to its
// newline without its fields ever being delimited: on twenty columns keyed on
// the first two, most of a row is never looked at.
// One JSON object's worth of scanning: where a value starts and ends, and
// whether it carries a backslash. Values are contiguous bytes in the file, which
// is what lets a JSON field stay an offset and a length into the mapping rather
// than a string built per cell -- the same representation the CSV path uses.
struct JsonValue {
    std::size_t from = 0, to = 0;
    bool escaped = false;
    bool absent = false;  // JSON null, which compares as an empty cell
};

// Skips one JSON string starting at the opening quote, returning the offset one
// past the closing quote. `escaped` is set if it contains a backslash.
inline std::size_t skip_json_string(std::string_view d, std::size_t at, std::size_t end,
                                    bool* escaped) {
    ++at;  // the opening quote
    for (;;) {
        const std::size_t stop = next_of2(d, at, end, '"', '\\');
        if (stop >= end) return end;
        if (d[stop] == '"') return stop + 1;
        *escaped = true;
        at = stop + 2;  // the backslash and whatever it escapes
        if (at > end) return end;
    }
}

inline bool json_space(char c) { return c == ' ' || c == '\t' || c == '\r' || c == '\n'; }

class RowParser {
  public:
    RowParser(char delimiter, std::vector<int> source)
        : delimiter_(delimiter), source_(std::move(source)) {
        for (int c : source_) last_needed_ = std::max(last_needed_, c);
        key_last_ = last_needed_;  // until set_key_size says otherwise
        // Inverted once, so storing a field is a lookup rather than a walk of
        // every wanted column: twenty columns against twenty slots is four
        // hundred comparisons a row otherwise. A column can feed more than one
        // slot -- `--compare` may name a key column -- so it is a run rather
        // than a single entry, laid out flat with a start per column.
        csv_slot_starts_.assign(static_cast<std::size_t>(last_needed_) + 2, 0);
        for (int c : source_)
            if (c >= 0) ++csv_slot_starts_[static_cast<std::size_t>(c) + 1];
        for (std::size_t i = 1; i < csv_slot_starts_.size(); ++i)
            csv_slot_starts_[i] += csv_slot_starts_[i - 1];
        csv_slots_.assign(static_cast<std::size_t>(csv_slot_starts_.back()), 0);
        std::vector<int> filled(static_cast<std::size_t>(last_needed_) + 1, 0);
        for (std::size_t slot = 0; slot < source_.size(); ++slot) {
            const int c = source_[slot];
            if (c < 0) continue;
            csv_slots_[static_cast<std::size_t>(csv_slot_starts_[static_cast<std::size_t>(c)] +
                                                filled[static_cast<std::size_t>(c)])] =
                static_cast<int>(slot);
            ++filled[static_cast<std::size_t>(c)];
        }
        // The common case pulled out of the runs above: a column that feeds
        // exactly one slot, which is every column unless `--compare` names a key.
        // Walking a run of one still cost two bounds loads and a loop per field,
        // twenty fields a row -- 13% of the instructions of a 2M CSV pair.
        csv_single_.assign(static_cast<std::size_t>(last_needed_) + 1, kNoSlot);
        for (std::size_t c = 0; c < csv_single_.size(); ++c) {
            const int n = csv_slot_starts_[c + 1] - csv_slot_starts_[c];
            if (n == 1) csv_single_[c] = csv_slots_[static_cast<std::size_t>(csv_slot_starts_[c])];
            if (n > 1) csv_single_[c] = kManySlots;
        }
    }

    // The JSON form. A CSV row is addressed by column number; a JSON object is
    // addressed by key, so the wanted names are held here and looked up as the
    // object is walked. `wanted[i]` is the name whose value belongs in slot i;
    // an empty name is a column this file does not have.
    RowParser(Dialect dialect, std::vector<std::string> wanted)
        : dialect_(dialect), wanted_(std::move(wanted)) {
        // A small open-addressed table from key name to slot, so a key costs one
        // hash rather than a walk of twenty names. Sized to at least twice the
        // keys so probes stay short.
        std::size_t n = 16;
        while (n < wanted_.size() * 4) n <<= 1;
        slot_mask_ = n - 1;
        slots_.assign(n, -1);
        for (std::size_t i = 0; i < wanted_.size(); ++i) {
            if (wanted_[i].empty()) continue;
            std::size_t at = name_hash(wanted_[i]) & slot_mask_;
            while (slots_[at] >= 0) at = (at + 1) & slot_mask_;
            slots_[at] = static_cast<int>(i);
        }
        canon_.resize(wanted_.size());
        for (std::size_t i = 0; i < wanted_.size(); ++i)
            canon_[i] = wanted_[i].empty() ? -1 : slot_for(wanted_[i]);
    }

    // Parses one row into `out`, returning the offset of the next row. A row
    // shorter than the header leaves the missing fields absent, which is a
    // difference to report rather than a file to refuse.
    std::size_t parse(std::string_view d, std::size_t start, std::size_t end, Field* out) const {
        if (dialect_ == Dialect::Json) return parse_json(d, start, end, out, wanted_.size());
        return parse_csv(d, start, end, out, last_needed_, source_.size());
    }

    std::size_t parse_csv(std::string_view d, std::size_t start, std::size_t end, Field* out,
                          int last, std::size_t slots) const {
        std::fill(out, out + slots, kAbsent);
        std::size_t pos = start;
        int column = 0;

#if defined(__AVX2__)
        // The field ends, found as a cursor over the row rather than a fresh
        // scan per field: one 32-byte load answers every delimiter in it, and
        // the ones not yet asked for are held as bits. A ten-byte field used
        // to load sixteen bytes and throw away all but the first match. C, Rust
        // and Zig have this cursor; C++ tried it three times and dropped it
        // (BENCHMARKS, 2026-09-27), each time with the state in a struct -- and
        // a struct of `std::size_t` is a type every `Field` store into `out`
        // may alias, so it was reloaded after each one. These are locals whose
        // address never escapes, so they stay in registers. `from` is where to
        // resume: a quoted field is walked by `skip_quoted`, and the matches
        // inside its body are dropped here.
        std::size_t held = start, scanned = start;
        std::uint32_t bits = 0;
        const __m256i va = _mm256_set1_epi8(delimiter_), vb = _mm256_set1_epi8('\n');
        const auto next_delim = [&](std::size_t from) __attribute__((always_inline)) {
            while (bits) {
                const std::size_t at = held + static_cast<std::size_t>(std::countr_zero(bits));
                bits &= bits - 1;
                if (at >= from) return at;
            }
            std::size_t at = std::max(from, scanned);
            for (; at + 32 <= end; at += 32) {
                const __m256i w = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d.data() + at));
                const auto hits = static_cast<std::uint32_t>(_mm256_movemask_epi8(
                    _mm256_or_si256(_mm256_cmpeq_epi8(w, va), _mm256_cmpeq_epi8(w, vb))));
                scanned = at + 32;
                if (hits) {
                    // `at` is never below `from`, so every match in it counts.
                    held = at;
                    bits = hits & (hits - 1);
                    return at + static_cast<std::size_t>(std::countr_zero(hits));
                }
            }
            // The last bytes of the range: a plain scan, and nothing held.
            const std::size_t next = next_of2(d, at, end, delimiter_, '\n');
            scanned = next < end ? next + 1 : end;
            return next;
        };
#else
        const auto next_delim = [&](std::size_t from) {
            return next_of2(d, from, end, delimiter_, '\n');
        };
#endif

        while (pos <= end) {
            Field field;
            std::size_t next;
            if (pos < end && d[pos] == '"') {
                const std::size_t close = skip_quoted(d, pos + 1, end);
                const std::size_t body_end = close > pos + 1 ? close - 1 : pos + 1;
                next = next_delim(close);
                const bool escaped = next_of1(d, pos + 1, body_end, '"') < body_end;
                field = pack(pos + 1, body_end - (pos + 1), escaped);
            } else {
                next = next_delim(pos);
                std::size_t stop = next;
                if (stop > pos && d[stop - 1] == '\r') --stop;  // CRLF behaves like LF
                field = pack(pos, stop - pos, false);
            }
            store(column, field, out, last, slots);
            ++column;

            if (next >= end) return end;
            if (d[next] == '\n') return next + 1;
            pos = next + 1;
            if (column > last) {
                const std::size_t eol = end_of_row(d, pos, end);
                return eol >= end ? end : eol + 1;
            }
        }
        return end;
    }

    std::size_t width() const {
        return dialect_ == Dialect::Json ? wanted_.size() : source_.size();
    }

    /// Slot -> the column it reads. The join needs it to decide whether the two
    /// files lay their compared columns out the same way.
    const std::vector<int>& source() const { return source_; }

    /// Key slots come first, so this is how far a key-only parse has to go.
    void set_key_size(std::size_t n) {
        key_size_ = n;
        key_last_ = -1;
        for (std::size_t slot = 0; slot < n && slot < source_.size(); ++slot)
            key_last_ = std::max(key_last_, source_[slot]);
        if (key_last_ < 0) key_last_ = last_needed_;
    }

    /// The keys, and nothing else.
    ///
    /// The sweep hashes keys and stores where the row starts; it never looks at
    /// a compared column. Parsing all twenty to use two was most of what it
    /// cost -- and the gain grows with the file, because past cache the parses
    /// removed are memory traffic and not only instructions.
    std::size_t parse_keys(std::string_view d, std::size_t start, std::size_t end,
                           Field* out) const {
        return dialect_ == Dialect::Json ? parse_json(d, start, end, out, key_size_)
                                         : parse_csv(d, start, end, out, key_last_, key_size_);
    }

  private:
    // A word at a time rather than a byte at a time: the first and last eight
    // bytes (four, or three single bytes, for a shorter name) and the length. A
    // byte loop is a serial multiply per byte, twenty names a row; collisions
    // are settled by comparing the name, so this only has to spread them.
    static std::uint64_t name_hash(std::string_view s) {
        const std::size_t n = s.size();
        std::uint64_t a = 0, b = 0;
        if (n >= 8) {
            std::memcpy(&a, s.data(), 8);
            std::memcpy(&b, s.data() + n - 8, 8);
        } else if (n >= 4) {
            std::uint32_t x, y;
            std::memcpy(&x, s.data(), 4);
            std::memcpy(&y, s.data() + n - 4, 4);
            a = x;
            b = y;
        } else if (n > 0) {
            a = static_cast<std::uint64_t>(static_cast<unsigned char>(s[0])) |
                static_cast<std::uint64_t>(static_cast<unsigned char>(s[n / 2])) << 8 |
                static_cast<std::uint64_t>(static_cast<unsigned char>(s[n - 1])) << 16;
        }
        const std::uint64_t h = (a ^ (b * 0x9e3779b97f4a7c15ULL) ^ n) * 0xbf58476d1ce4e5b9ULL;
        return h ^ (h >> 31);
    }

    // Walks one JSON object, storing the values of the keys we want. One pass
    // over the object, one hash per key -- not a search per wanted column, which
    // at twenty columns would be four hundred comparisons a row.
    std::size_t parse_json(std::string_view d, std::size_t start, std::size_t end, Field* out,
                           std::size_t slots) const {
        std::fill(out, out + slots, kAbsent);
        std::size_t found = 0;  // key slots filled, for the early exit below
        std::size_t guess = 0;  // the slot the next member most likely fills
        std::size_t pos = start;
        while (pos < end && json_space(d[pos])) ++pos;
        if (pos >= end) return end;
        if (d[pos] != '{') return end_of_json_row(d, pos, end);  // not an object: skip the line
        ++pos;

        for (;;) {
            // Compact ndjson separates members with a bare `,"`, which goes
            // straight to the next key. The general path below takes the comma,
            // loops back and skips space twice to get there.
            if (pos + 1 < end && d[pos] == ',' && d[pos + 1] == '"') {
                ++pos;
            } else {
                while (pos < end && json_space(d[pos])) ++pos;
                if (pos >= end) break;
                if (d[pos] == '}') {
                    ++pos;
                    break;
                }
                if (d[pos] == ',') {
                    ++pos;
                    continue;
                }
                if (d[pos] != '"') break;  // malformed: stop reading this object
            }
            bool key_escaped = false;
            const std::size_t key_from = pos + 1;
            const std::size_t key_end = skip_json_string(d, pos, end, &key_escaped);
            if (key_end > end || key_end < 2) break;
            const std::string_view key = d.substr(key_from, key_end - 1 - key_from);
            pos = key_end;
            // And `":"` between a key and a string value, likewise.
            if (pos + 1 < end && d[pos] == ':' && d[pos + 1] == '"') {
                ++pos;
            } else {
                while (pos < end && json_space(d[pos])) ++pos;
                if (pos >= end || d[pos] != ':') break;
                ++pos;
                while (pos < end && json_space(d[pos])) ++pos;
                if (pos >= end) break;
            }

            JsonValue v;
            if (d[pos] == '"') {
                v.from = pos + 1;
                const std::size_t close = skip_json_string(d, pos, end, &v.escaped);
                v.to = close > pos + 1 ? close - 1 : pos + 1;
                pos = close;
            } else {
                // A number, true, false or null: runs to the next comma, brace
                // or space. Nested objects and arrays are not a cell value and
                // are left absent rather than guessed at.
                const std::size_t from = pos;
                if (d[pos] == '{' || d[pos] == '[') {
                    pos = skip_json_nested(d, pos, end);
                    v.absent = true;
                } else {
                    while (pos < end && d[pos] != ',' && d[pos] != '}' && !json_space(d[pos])) ++pos;
                    v.from = from;
                    v.to = pos;
                    v.absent = (pos - from == 4 && d.compare(from, 4, "null") == 0);
                }
            }
            if (!v.absent) {
                const int slot = guess < wanted_.size() && !wanted_[guess].empty() &&
                                         same_name(wanted_[guess], key)
                                     ? canon_[guess]
                                     : slot_for(key);
                if (slot >= 0) guess = static_cast<std::size_t>(slot) + 1;
                if (slot >= 0 && static_cast<std::size_t>(slot) < slots) {
                    // First occurrence wins for a key column, and only for a key
                    // column -- the C port's rule, adopted here so the two agree
                    // on a name a JSON object repeats. It is not a nicety: the
                    // key-only parse below stops as soon as it has the keys, and
                    // under last-wins it could stop on a different value than the
                    // full parse ends with, which is a lookup that misses its own
                    // row. Compared columns keep last-wins.
                    if (static_cast<std::size_t>(slot) < key_size_) {
                        if (out[slot] == kAbsent) {
                            out[slot] = pack(v.from, v.to - v.from, v.escaped);
                            if (++found == slots) break;
                        }
                    } else {
                        out[slot] = pack(v.from, v.to - v.from, v.escaped);
                    }
                }
            }
        }
        return end_of_json_row(d, pos, end);
    }

    int slot_for(std::string_view key) const {
        std::size_t at = name_hash(key) & slot_mask_;
        for (;;) {
            const int i = slots_[at];
            if (i < 0) return -1;
            if (same_name(wanted_[static_cast<std::size_t>(i)], key)) return i;
            at = (at + 1) & slot_mask_;
        }
    }

    // The end of a row, which for newline-delimited JSON is the next newline
    // byte and nothing subtler.
    //
    // This used to alternate a scan for `\n` or `"` with a walk over each string
    // it landed on, to avoid mistaking a newline inside a quoted value for the
    // end of the row. That cannot happen: RFC 8259 forbids the raw control
    // characters U+0000 to U+001F inside a string, and a newline is U+000A, so a
    // valid JSON string cannot contain one -- it must be written `\n`. The
    // framing of ndjson depends on exactly that.
    //
    // The C port made this change first and its note carries the argument. The
    // profile says what it is worth here: of this port's 56.3M calls into
    // `next_of2` on a 200k ndjson pair, 22.4M came from this function, and the
    // string walks it launched account for much of the 27.5M charged to
    // `skip_json_string` besides. Input that does put a raw newline inside a
    // string is not JSON, and this reader will split the row there -- which is
    // what every ndjson reader does, because the format has no other way to say
    // where a row ends.
    static std::size_t end_of_json_row(std::string_view d, std::size_t pos, std::size_t end) {
        const std::size_t stop = next_of1(d, pos, end, '\n');
        return stop >= end ? end : stop + 1;
    }

  public:
    static std::size_t skip_nested_public(std::string_view d, std::size_t pos, std::size_t end) {
        return skip_json_nested(d, pos, end);
    }

    /// Does the mate's tail name a column this run tracks?
    ///
    /// The CSV proof rests on a column sitting at a fixed offset. JSON makes no
    /// such promise: a value is found by name, and a name repeated in one object
    /// takes its *last* value for a compared column -- which the C, Rust and Zig
    /// ports do too, and `test.sh` cross-checks -- so a second `"amount"` past
    /// the diverging byte would carry a value the prefix never saw.
    ///
    /// It cannot be ruled out in general, but it can be ruled out *here*, because
    /// the proof only reaches this point when the two rows agree all the way
    /// through the last compared value. What is left is the trailing ignored
    /// columns, a few bytes. A name is a quoted string, so every quote in them is
    /// a candidate. Mistaking a closing quote for an opening one costs a lookup
    /// that fails, which is a fallback and not a wrong answer -- and an escaped
    /// name counts as a hit for the same reason, since the wanted names are held
    /// unescaped.
    bool json_tail_is_clean(std::string_view d, std::size_t at, std::size_t end) const {
        while (at < end) {
            const std::size_t q = next_of1(d, at, end, '"');
            if (q >= end) return true;
            bool escaped = false;
            const std::size_t close = skip_json_string(d, q, end, &escaped);
            if (escaped) return false;
            const std::size_t from = q + 1;
            const std::size_t to = close > q + 1 ? close - 1 : q + 1;
            if (to > from && slot_for(d.substr(from, to - from)) >= 0) return false;
            if (close <= q) return false;
            at = close;
        }
        return true;
    }

    /// The JSON proof, past ignored values that differ.
    ///
    /// The plain proof holds when A's row and its mate agree byte for byte
    /// through the last value either side wants and the mate's tail names
    /// nothing tracked. One ignored member in between whose value differs -- an
    /// `updated_at`, say, which differs on every row -- fails that on every row.
    /// So where the two rows first differ, this finds the member of A's object
    /// that byte is in. If it is the value of a name nobody tracks, the value is
    /// skipped in each row on its own -- in B from the same offset, since
    /// everything before it agreed, name included -- and the comparison carries
    /// on from there. A difference anywhere else, in a tracked value, or in a
    /// shape the walk does not follow fails the proof, and the row is parsed as
    /// before. The names before each skipped value are equal bytes in both rows
    /// and the skipped values belong to untracked names, so every tracked name
    /// sits at the same place with the same bytes; the tail check covers what
    /// follows. The C port has the same proof (#234).
    bool json_rows_match(std::string_view a, std::size_t a_lo, std::size_t a_hi,
                         std::size_t need, std::string_view b, std::size_t b_lo,
                         std::size_t b_hi) const {
        std::size_t ai = a_lo, bi = b_lo, left = need;
        bool after_value = false;
        for (int hops = 0; hops < 64; ++hops) {
            const std::size_t m = std::min(left, b_hi - bi);
            const std::size_t same = common_prefix(a.data() + ai, b.data() + bi, m);
            if (same == left) return json_tail_is_clean(b, bi + left, b_hi);
            if (same == m) return false;  // B ends first
            std::size_t vs = 0, ve = 0;
            if (!json_member_at(a, ai, a_hi, ai + same, after_value, vs, ve)) return false;
            if (ve - a_lo > need) return false;  // the gap runs past the last wanted value
            const std::size_t b_vs = bi + (vs - ai);
            const std::size_t b_ve = json_value_end(b, b_vs, b_hi);
            if (b_ve >= b_hi || b_ve <= b_vs) return false;
            left -= ve - ai;
            ai = ve;
            bi = b_ve;
            after_value = true;
        }
        return false;
    }

  private:
    static bool json_gap(char c) { return c == ' ' || c == '\t' || c == '\r'; }

    // Past one JSON value from `at`: a string, a scalar, or a nested object or
    // array. `end` where it does not close before `end`.
    static std::size_t json_value_end(std::string_view d, std::size_t at, std::size_t end) {
        if (at >= end) return end;
        if (d[at] == '"') {
            bool escaped = false;
            return skip_json_string(d, at, end, &escaped);
        }
        if (d[at] == '{' || d[at] == '[') return skip_json_nested(d, at, end);
        while (at < end && d[at] != ',' && d[at] != '}' && d[at] != ']' && d[at] != '\n' &&
               !json_gap(d[at]))
            ++at;
        return at;
    }

    // The member of A's object whose value holds byte `diff`, walking from `at`
    // (the row start, or just past a value already skipped): its value's bounds.
    // False where `diff` falls in a name or the punctuation between members, in
    // a tracked member, or anywhere the walk cannot follow -- an escaped name
    // included, as the tail check treats one.
    bool json_member_at(std::string_view d, std::size_t at, std::size_t end, std::size_t diff,
                        bool after_value, std::size_t& vs, std::size_t& ve) const {
        for (;;) {
            while (at < end && json_gap(d[at])) ++at;
            if (at >= end || at > diff) return false;
            if (d[at] != (after_value ? ',' : '{')) return false;
            ++at;
            while (at < end && json_gap(d[at])) ++at;
            if (at >= end || d[at] != '"' || at >= diff) return false;
            bool escaped = false;
            const std::size_t kq = at, kend = skip_json_string(d, at, end, &escaped);
            if (escaped || kend >= end || kend >= diff) return false;
            at = kend;
            while (at < end && json_gap(d[at])) ++at;
            if (at >= end || d[at] != ':') return false;
            ++at;
            while (at < end && json_gap(d[at])) ++at;
            if (at > diff) return false;
            const std::size_t v_end = json_value_end(d, at, end);
            if (v_end >= end || v_end <= at) return false;
            if (diff < v_end) {
                if (slot_for(d.substr(kq + 1, kend - kq - 2)) >= 0) return false;
                vs = at;
                ve = v_end;
                return true;
            }
            at = v_end;
            after_value = true;
        }
    }

    static std::size_t skip_json_nested(std::string_view d, std::size_t pos, std::size_t end) {
        int depth = 0;
        while (pos < end) {
            const char c = d[pos];
            if (c == '"') {
                bool ignored = false;
                pos = skip_json_string(d, pos, end, &ignored);
                continue;
            }
            if (c == '{' || c == '[') ++depth;
            if (c == '}' || c == ']') {
                --depth;
                ++pos;
                if (depth <= 0) return pos;
                continue;
            }
            ++pos;
        }
        return end;
    }

    void store(int column, Field f, Field* out, int last, std::size_t slots) const {
        if (column > last) return;
        const std::size_t at = static_cast<std::size_t>(column);
        const int one = csv_single_[at];
        if (one >= 0) {
            if (static_cast<std::size_t>(one) < slots) out[one] = f;
            return;
        }
        if (one == kNoSlot) return;
        for (int i = csv_slot_starts_[at]; i < csv_slot_starts_[at + 1]; ++i) {
            const std::size_t slot = static_cast<std::size_t>(csv_slots_[static_cast<std::size_t>(i)]);
            if (slot < slots) out[slot] = f;
        }
    }

    // The rest of a row once its last needed column is read -- on twenty
    // columns keyed on two, the sweep's key-only parse skips about 150 bytes
    // here per row. `next_of2` steps sixteen bytes, which is right for the
    // field-sized scans that are most of its calls and slow for this one, so
    // this span gets a 32-byte step of its own where the build has AVX2 -- the
    // width C's `next_of2` takes everywhere, and C++'s `next_of1` for the
    // ndjson row end.
    static std::size_t next_row_break(std::string_view d, std::size_t at, std::size_t end) {
#if defined(__AVX2__)
        const __m256i vn = _mm256_set1_epi8('\n'), vq = _mm256_set1_epi8('"');
        for (; at + 32 <= end; at += 32) {
            const __m256i w = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(d.data() + at));
            const auto hits = static_cast<std::uint32_t>(
                _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(w, vn), _mm256_cmpeq_epi8(w, vq))));
            if (hits) return at + static_cast<std::size_t>(std::countr_zero(hits));
        }
#endif
        return next_of2(d, at, end, '\n', '"');
    }

    static std::size_t end_of_row(std::string_view d, std::size_t pos, std::size_t end) {
        std::size_t at = pos;
        while (at < end) {
            const std::size_t next = next_row_break(d, at, end);
            if (next >= end) return end;
            if (d[next] == '"') {
                at = skip_quoted(d, next + 1, end);
                continue;
            }
            return next;
        }
        return end;
    }

    char delimiter_ = ',';
    std::vector<int> source_;
    int last_needed_ = 0;
    int key_last_ = 0;
    std::size_t key_size_ = 0;
    // The inverse of `source_`: which slots each column of the file feeds.
    std::vector<int> csv_slot_starts_, csv_slots_;
    // Per column: its one slot, or one of these.
    static constexpr int kNoSlot = -1;
    static constexpr int kManySlots = -2;
    std::vector<int> csv_single_;
    Dialect dialect_ = Dialect::Csv;
    std::vector<std::string> wanted_;   // JSON: the key whose value goes in each slot
    std::vector<int> slots_;            // JSON: open-addressed name -> slot
    std::size_t slot_mask_ = 0;
    // JSON: what `slot_for` answers for `wanted_[i]` -- i itself unless the
    // same name also fills an earlier slot. Names arrive in the same order row
    // after row, usually slot order, so the member after slot i is tried as
    // i + 1 before any hashing, and a right guess has to give exactly the slot
    // the table would.
    std::vector<int> canon_;
};

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

// What a sweep hands each row to when the join runs inside it: the row's bytes,
// from its start to the next row's start -- what `RowIndex::row_end` will say --
// and its key hash. `part` is the sweep chunk, so each can count into its own
// totals. `reset` clears a chunk's totals when a wrong split guess makes the
// sweep start over.
class RowSink {
  public:
    virtual void row(std::size_t part, std::size_t lo, std::size_t hi, std::uint64_t hash) = 0;
    virtual void reset(std::size_t part) = 0;

  protected:
    ~RowSink() = default;
};

class FnSink final : public RowSink {
  public:
    std::function<void(std::size_t, std::size_t, std::size_t, std::uint64_t)> on_row;
    std::function<void(std::size_t)> on_reset;
    void row(std::size_t part, std::size_t lo, std::size_t hi, std::uint64_t hash) override {
        on_row(part, lo, hi, hash);
    }
    void reset(std::size_t part) override { on_reset(part); }
};

// Open addressing over one file's rows. Everything is a primitive array: row
// starts, key hashes, and a table of key numbers masked into a power-of-two slot
// count. The index stores where a row starts rather than its fields, because an
// offset is eight bytes where the fields would be twenty times that; re-parsing
// is cheap because the parser stops at the last needed column.
class RowIndex {
  public:
    // Rows are found and hashed in `threads` parallel chunks, then inserted in
    // file order on this one. The split is safe because the two halves need
    // different things: parsing a row depends on nothing but where it starts,
    // while the table depends on the order rows arrive -- first occurrence of a
    // key wins, and duplicate counts follow from that. Doing the second half in
    // parallel would make the answer depend on thread scheduling.
    /// Phase timings, on stderr, when CSVDIFF_PHASES is set -- the same switch,
    /// the same shape and the same column widths the C, Rust and Zig ports use,
    /// so four runs can be read side by side.
    ///
    /// `pqdiff.cpp` has its own copy of this for the columnar path. Fifteen lines
    /// duplicated across two translation units, against moving it into the public
    /// header to share it: the duplication is the cheaper of the two, and the
    /// output format is what has to agree, not the class.
    class Phases {
      public:
        explicit Phases(const char* tag) : on_(std::getenv("CSVDIFF_PHASES") != nullptr), tag_(tag) {}
        void mark(const char* what) {
            if (!on_) return;
            const auto now = clock::now();
            char label[48];
            std::snprintf(label, sizeof label, "%s%s", tag_, what);
            std::fprintf(stderr, "  %-26s %7.3fs\n", label,
                         std::chrono::duration<double>(now - last_).count());
            last_ = now;
        }

      private:
        using clock = std::chrono::steady_clock;
        bool on_;
        const char* tag_;
        clock::time_point last_ = clock::now();
    };

    // How far ahead the insert loop asks for its cache line. Twenty-four is what
    // the columnar join in pqdiff.cpp settled on for the same access pattern.
    static constexpr std::size_t kInsertPrefetch = 24;

    RowIndex(const Slab& slab, const RowParser& parser, std::size_t from, std::size_t key_size,
             const Options& opt, unsigned threads = 1, const char* tag = "",
             RowSink* sink = nullptr)
        : slab_(slab), parser_(parser), key_size_(key_size), opt_(opt), track_later_(sink != nullptr) {
        Phases phase(tag);
        // A placeholder until the sweep says how many rows there are; an empty
        // file returns before that and needs a valid mask.
        table_.assign(1 << 12, kEmpty);
        mask_ = table_.size() - 1;
        size_slots(table_.size());
        scratch_.assign(parser.width() * 2, kAbsent);  // probe, then this row's key

        const std::string_view d = slab.bytes();
        const std::size_t end = d.size();
        if (from >= end) return;

        // The CSV splits are guessed, then checked.
        //
        // Counted bounds cost a pass over the front of the file before any
        // chunk starts -- (threads-1)/threads of it, and at the two chunks a
        // file this port uses, half the file on one thread. In memory that is a
        // quick scan for one byte. On an input larger than memory it is a disk
        // read that the sweep then repeats, because by the time the sweep gets
        // there those pages are gone: "chunk bounds" was 61s and 65s on the
        // 150M CI pair, 26 GB a side, before either sweep began.
        //
        // So each split is taken at the next newline, as JSON's are, and
        // checked the way the Rust and C ports check it. Chunk 0 starts at a
        // real row, so the last row it parses ends at the first real row start
        // past its boundary; if that is where chunk 1 began, chunk 1 started at
        // a real row too, and so on down the line. Only a guess that landed on
        // a newline inside a quoted field breaks that, and then the sweep runs
        // again on counted bounds, which is what it always did before. A chunk
        // that started on a wrong guess parsed garbage, so its rows and its
        // failure are thrown away unread; a failure in a chunk whose start is
        // proven is real, and is thrown as it always was.
        const bool json = slab.dialect() == Dialect::Json;
        std::vector<std::size_t> bounds = chunk_bounds(d, from, threads, json, /*count_quotes=*/false);
        phase.mark("chunk bounds");
        std::vector<Chunk> chunks;
        std::vector<std::exception_ptr> failures;
        auto sweep_all = [&] {
            const std::size_t n = bounds.size() - 1;
            chunks.assign(n, Chunk{});
            failures.assign(n, nullptr);
            auto scan = [&](std::size_t i) {
                try {
                    chunks[i].stopped =
                        sweep(d, parser, bounds[i], bounds[i + 1], end, chunks[i], i, sink);
                } catch (...) {
                    failures[i] = std::current_exception();
                }
            };
            std::vector<std::thread> workers;
            workers.reserve(n - 1);
            for (std::size_t i = 1; i < n; ++i) {
                try {
                    workers.emplace_back(scan, i);
                } catch (const std::system_error&) {
                    scan(i);  // no thread to be had: the same work, here
                }
            }
            scan(0);
            for (auto& w : workers) w.join();
        };
        sweep_all();
        bool held = true;
        for (std::size_t i = 0; i + 1 < chunks.size() && held; ++i) {
            if (failures[i]) std::rethrow_exception(failures[i]);  // its start is proven
            held = json || chunks[i].stopped == bounds[i + 1];
        }
        if (!held) {
            // Whatever the sink was handed came from rows that were not rows.
            if (sink)
                for (std::size_t p = 0; p < chunks.size(); ++p) sink->reset(p);
            bounds = chunk_bounds(d, from, threads, json, /*count_quotes=*/true);
            sweep_all();
        }
        for (const auto& f : failures)
            if (f) std::rethrow_exception(f);
        // Each chunk's last row ends where the next chunk's first row starts,
        // which only the next chunk knows, so those rows are handed over now.
        if (sink) {
            for (std::size_t i = 0; i < chunks.size(); ++i) {
                if (chunks[i].starts.empty()) continue;
                std::size_t hi = end;
                for (std::size_t j = i + 1; j < chunks.size(); ++j) {
                    if (!chunks[j].starts.empty()) {
                        hi = chunks[j].starts.front();
                        break;
                    }
                }
                sink->row(i, chunks[i].starts.back(), hi, chunks[i].hashes.back());
            }
        }
        phase.mark("sweep (parallel)");

        std::size_t total = 0;
        // Each part's first row, for a sink that numbered its rows within the
        // part: the parts are drained in order, so part i starts after the rows
        // of every part before it.
        part_first_.clear();
        for (const auto& c : chunks) {
            part_first_.push_back(static_cast<std::int64_t>(total));
            total += c.starts.size();
        }
        row_start_.reserve(total);
        row_hash_.reserve(total);

        // The chunks are drained into the flat arrays and released *before* the
        // table exists, which is the order the C port uses and the reason its
        // high-water mark is lower.
        //
        // Both ports keep two copies of every row's start and hash for a while:
        // the chunks the sweep filled and the arrays being filled from them.
        // Releasing each chunk as it is taken means the high-water mark is the
        // flat arrays plus one chunk rather than plus all of them, and the note
        // below the loop has said so for a while. What it did not account for is
        // *what else is alive at the same time*: the table, `first_row_` and
        // `occurrences_` were allocated first and so were resident through the
        // whole transfer. At ten million rows that is 134 MB of table and 80 MB
        // of the other two, a side, sitting under the duplication instead of
        // after it.
        //
        // Measured on a 4M pair, the anonymous high-water mark is a transient:
        // this port peaked at 384 MB a third of the way in and settled at 273
        // MB, where C stayed flat at 276 MB throughout. The steady states match;
        // only the spike did not.
        for (auto& c : chunks) {
            row_start_.insert(row_start_.end(), c.starts.begin(), c.starts.end());
            row_hash_.insert(row_hash_.end(), c.hashes.begin(), c.hashes.end());
            std::vector<std::size_t>().swap(c.starts);
            std::vector<std::uint64_t>().swap(c.hashes);
        }
        std::vector<Chunk>().swap(chunks);
        rows_ = static_cast<std::int64_t>(total);

        // Sized once, from a row count the sweep has already produced. Growing
        // into it instead costs a rehash per doubling -- thirteen of them at ten
        // million rows, each one a full pass of random probes over a table far
        // too big to cache, to arrive at the size that was known before the
        // first insert. Under a half load, so nothing ever rehashes.
        std::size_t cap = 1u << 12;
        while (cap < total * 2 + 16) cap <<= 1;
        table_.assign(cap, kEmpty);
        mask_ = cap - 1;
        size_slots(cap);
        first_row_.reserve(total);
        occurrences_.reserve(total);
        struct AtExit {
            Phases& p;
            ~AtExit() { p.mark("index insert (serial)"); }
        } mark_insert{phase};
        // Each chunk is released as soon as it has been inserted. Holding all of
        // them until the end would keep two copies of every row's start and hash
        // alive at once -- the chunks and the arrays being filled from them --
        // which is sixteen bytes a row of pure duplication, 320 MB at ten
        // million rows across both files. Freed as it goes, the two curves cross
        // instead of adding.
        //
        // The insert is latency-bound, not throughput-bound. Every row lands on
        // a slot the hash chose, the table is 32 MB at ten million rows across
        // both sides, and nothing about one row predicts the next one's line --
        // so each probe is a cache miss the core waits out with nothing else to
        // do. Measured at about 90 ns a row -- 0.74s for the eight million rows
        // of a 4M pair's two indexes -- which is the shape of a trip to memory
        // rather than of the dozen instructions the probe actually runs.
        //
        // The hash of the row `kInsertPrefetch` ahead is already in hand, so the
        // line it will want can be asked for now and be resident by the time the
        // loop arrives. `pqdiff.cpp` does the same thing in its join for the same
        // reason; this is that, on the other side of the index.
        //
        // C, Rust and Zig all already did this in their own text index inserts.
        // This port was the only one that did not, which is why its insert was
        // the worst of the four in absolute terms.
        //
        // A prefetch of the wrong address is a wasted instruction and never a
        // wrong answer, so the rehash case below needs no special handling --
        // the table is sized from the sweep's row count and does not rehash
        // anyway.
        //
        // Worth 0.800x on this phase at one thread and 0.773x at four, median of
        // seven runs each. End to end over eleven paired rounds on a 4M pair,
        // counts gated: csv 0.947 at four threads (middle half 0.929-0.975) and
        // 0.986 at one (0.953-0.999); ndjson 0.975 at four (0.965-0.984, faster
        // in all eleven) and 0.989 at one, which crosses one and is not claimed.
        for (std::size_t i = 0; i < total; ++i) {
            if (i + kInsertPrefetch < total)
                __builtin_prefetch(&table_[slot_of(row_hash_[i + kInsertPrefetch])], 1, 0);
            insert(static_cast<int>(i));
        }
    }

    void fields_of(int row, Field* out) const {
        parser_.parse(slab_.bytes(), row_start_[row], slab_.bytes().size(), out);
    }

    /// Where this row's bytes stop: the next row's start, or the end of the file.
    std::size_t row_end(int row) const {
        const std::size_t next = static_cast<std::size_t>(row) + 1;
        return next < row_start_.size() ? row_start_[next] : slab_.bytes().size();
    }
    std::size_t row_begin(int row) const { return row_start_[static_cast<std::size_t>(row)]; }

    /// The parser that read this side, so a caller can ask what it tracks.
    const RowParser& parser() const { return parser_; }

    /// This row's key fields, for a caller that compares nothing else.
    void keys_of(int row, Field* out) const {
        parser_.parse_keys(slab_.bytes(), row_start_[row], slab_.bytes().size(), out);
    }

    // The row carrying `fields`' key, or -1. `other` is the slab those fields
    // live in, which is the opposite file when this is a join probe. `probe` is
    // scratch the caller owns: the join runs both directions at once, and a
    // buffer hanging off the index would be shared between those threads.
    int lookup(const Slab& other, const Field* fields, std::uint64_t hash, Field* probe) const {
        std::size_t slot = slot_of(hash);
        for (;;) {
            const std::uint32_t v = table_[slot];
            if (v == kEmpty) return -1;
            // The tag rejects a foreign key from this word alone; only a tag
            // that matches is worth two more misses to disprove.
            if (!slot_tag_is(v, hash)) {
                slot = (slot + 1) & mask_;
                continue;
            }
            const int candidate = first_row_[slot_pos(v)];
            if (row_hash_[candidate] == hash) {
                keys_of(candidate, probe);
                bool ok = true;
                for (std::size_t i = 0; i < key_size_ && ok; ++i)
                    ok = same(slab_, probe[i], other, fields[i], opt_);
                if (ok) return candidate;
            }
            slot = (slot + 1) & mask_;
        }
    }

    /// The first row whose whole hash is `hash`, or -1: the candidate
    /// `lookup_proof` settles on when it is given no fields. The gapped proof
    /// checks it itself.
    int first_match(std::uint64_t hash) const {
        std::size_t slot = slot_of(hash);
        for (;;) {
            const std::uint32_t v = table_[slot];
            if (v == kEmpty) return -1;
            if (slot_tag_is(v, hash)) {
                const int candidate = first_row_[slot_pos(v)];
                if (row_hash_[candidate] == hash) return candidate;
            }
            slot = (slot + 1) & mask_;
        }
    }

    /// `lookup`, with the proof tried first.
    ///
    /// `need` bytes from `a_row` cover every column either file wants, keys
    /// included, so a candidate whose row begins with the same bytes carries
    /// the same key: that is the key comparison, settled without parsing the
    /// candidate's keys. Only a candidate the bytes do not settle is parsed and
    /// compared as `lookup` does. `proven` says which of the two found it, and
    /// a proven mate needs no comparing either -- the C and Zig ports do the
    /// same.
    int lookup_proof(const Slab& other, const Field* fields, std::uint64_t hash, Field* probe,
                     const char* a_row, std::size_t need, std::size_t a_hi, bool json,
                     bool& proven) const {
        std::size_t slot = slot_of(hash);
        for (;;) {
            const std::uint32_t v = table_[slot];
            if (v == kEmpty) return -1;
            if (!slot_tag_is(v, hash)) {
                slot = (slot + 1) & mask_;
                continue;
            }
            const int candidate = first_row_[slot_pos(v)];
            if (row_hash_[candidate] == hash) {
                const std::size_t b_lo = row_begin(candidate);
                const std::size_t b_n = row_end(candidate) - b_lo;
                if (json ? parser_.json_rows_match(other.bytes(),
                                                   static_cast<std::size_t>(a_row - other.bytes().data()),
                                                   a_hi, need, slab_.bytes(), b_lo, b_lo + b_n)
                         : need <= b_n &&
                               common_prefix(a_row, slab_.bytes().data() + b_lo, need) == need) {
                    proven = true;
                    return candidate;
                }
                // No fields: the caller has not parsed its row and will check
                // this candidate's keys itself after it does.
                if (!fields) return candidate;
                keys_of(candidate, probe);
                bool ok = true;
                for (std::size_t i = 0; i < key_size_ && ok; ++i)
                    ok = same(slab_, probe[i], other, fields[i], opt_);
                if (ok) return candidate;
            }
            slot = (slot + 1) & mask_;
        }
    }

    /// The key hash the sweep computed for this row.
    std::uint64_t hash_of(int row) const { return row_hash_[static_cast<std::size_t>(row)]; }

    /// Asks for the table line a later `lookup(hash)` will read first. The join
    /// probes this table once per key of the other file, in that file's order,
    /// so every probe is a miss on a table too big to cache -- and the hash
    /// that decides which line is already in hand, rows ahead. The insert loop
    /// above does the same, and so does the C port's join.
    void prefetch(std::uint64_t hash) const { __builtin_prefetch(&table_[slot_of(hash)], 0, 0); }

    /// How many rows ahead a caller of `prefetch` should look.
    static constexpr std::size_t kLookupPrefetch = kInsertPrefetch;

    const std::vector<int>& first_rows() const { return first_row_; }
    const std::vector<std::uint32_t>& occurrences() const { return occurrences_; }
    std::int64_t rows() const { return rows_; }
    std::int64_t unique_keys() const { return static_cast<std::int64_t>(first_row_.size()); }
    std::int64_t dup_keys() const { return dup_keys_; }
    std::int64_t dup_rows() const { return dup_rows_; }
    /// Rows that repeat a key already seen, in file order -- kept only when the
    /// build was given a sink, which joined them as though each were first.
    const std::vector<int>& later() const { return later_; }
    /// The first row of each sweep part, in part order.
    const std::vector<std::int64_t>& part_first() const { return part_first_; }

  private:
    static constexpr std::uint32_t kEmpty = 0;

    // `pos + 1` so that a packed slot is never zero, which is what marks empty.
    void size_slots(std::size_t keys) {
        pos_bits_ = 1;
        while (pos_bits_ < 32 && (keys + 1) > ((std::size_t{1} << pos_bits_) - 1)) ++pos_bits_;
        pos_mask_ = pos_bits_ >= 32 ? 0xFFFFFFFFu
                                    : static_cast<std::uint32_t>((1u << pos_bits_) - 1);
    }
    std::uint32_t slot_pack(std::uint64_t hash, std::size_t pos) const {
        const unsigned tag_bits = 32u - pos_bits_;
        const std::uint32_t tag =
            tag_bits ? static_cast<std::uint32_t>(hash >> (64u - tag_bits)) : 0u;
        return (tag << pos_bits_) | static_cast<std::uint32_t>(pos + 1);
    }
    bool slot_tag_is(std::uint32_t v, std::uint64_t hash) const {
        const unsigned tag_bits = 32u - pos_bits_;
        if (!tag_bits) return true;
        return (v >> pos_bits_) == static_cast<std::uint32_t>(hash >> (64u - tag_bits));
    }
    std::size_t slot_pos(std::uint32_t v) const {
        return static_cast<std::size_t>(v & pos_mask_) - 1;
    }

    // One chunk's rows, in the order they appear in it.
    // Padded to a cache line. One of these per sweep thread in one
    // `std::vector<Chunk>`, forty-eight bytes apart, and `push_back` on either
    // vector touches this struct's pointers on every row -- so two threads
    // sweeping concurrently move the line between cores once a row each.
    //
    // This is why the split below looked unprofitable when it was measured. See
    // the note there.
    struct alignas(64) Chunk {
        std::vector<std::size_t> starts;
        std::vector<std::uint64_t> hashes;
        std::size_t stopped = 0;  // where the sweep left off: the first row start past the chunk
        char pad[64]{};
    };

    // Where each chunk begins, as offsets of real row starts. The nominal
    // split is `size / threads`, then walked forward to the next row.
    //
    // Walking forward is the whole difficulty: a newline inside a quoted field
    // is not a row boundary, and a thread starting mid-file cannot tell whether
    // it is inside such a field. Parity settles it. Every `"` toggles in-quote
    // state -- including both halves of a doubled quote, which toggles twice and
    // so leaves the state alone, which is exactly right -- so the count of
    // quotes before a position says whether that position is inside a field.
    // Counting them is a scan for one byte, far cheaper than parsing, and it
    // splits across the same threads.
    // JSON needs no quote count: a raw newline inside a string is not valid
    // JSON, so every newline ends a record. Counting anyway is not only wasted
    // -- ndjson quotes every key and most values, so the count stops every few
    // bytes and cost 0.47s a side on a 2M-row pair, more than the sweep it
    // splits -- it is also wrong: an escaped `\"` toggles the parity, and a
    // split whose count comes out odd can walk to the end of the file looking for
    // a newline outside quotes, and the chunk is dropped.
    //
    // With `count_quotes` false, CSV takes the next newline as well, which is a
    // guess; the constructor checks it after the sweep and counts only when a
    // guess was wrong.
    static std::vector<std::size_t> chunk_bounds(std::string_view d, std::size_t from,
                                                 unsigned threads, bool json, bool count_quotes) {
        if (!count_quotes) json = true;  // the newline rule, without the count
        const std::size_t end = d.size();
        // Below this there is nothing to divide: the boundary work would cost
        // more than the parsing it splits.
        if (threads <= 1 || end - from < (4u << 20)) return {from, end};

        std::vector<std::size_t> nominal;
        for (unsigned i = 1; i < threads; ++i)
            nominal.push_back(from + (end - from) * i / threads);

        // Quotes before each nominal split: each slice counted once, in
        // parallel, then a prefix sum.
        //
        // Asking each task for the count from `from` to its own split reads the
        // first slice of the file in every task, the second in all but one, and
        // so on -- (threads-1)/2 passes over the file in total, and the last task
        // alone reads nearly all of it, so the step costs about a whole pass
        // however many threads run it. That grows with the thread count while
        // the work it prepares shrinks. Counting the slice *since* the previous
        // split gives the same numbers from one pass split evenly.
        std::vector<std::size_t> quotes(nominal.size(), 0);
        if (!json) {
            std::vector<std::thread> counters;
            auto count = [&](std::size_t i) {
                const std::size_t begin = i == 0 ? from : nominal[i - 1];
                std::size_t n = 0;
                for (std::size_t at = begin; at < nominal[i];) {
                    const std::size_t q = next_of1(d, at, nominal[i], '"');
                    if (q >= nominal[i]) break;
                    ++n;
                    at = q + 1;
                }
                quotes[i] = n;
            };
            counters.reserve(nominal.size() - 1);
            for (std::size_t i = 1; i < nominal.size(); ++i) {
                try {
                    counters.emplace_back(count, i);
                } catch (const std::system_error&) {
                    count(i);
                }
            }
            count(0);
            for (auto& c : counters) c.join();
            // Each entry holds its own slice; the running total is what the
            // parity test below wants.
            for (std::size_t i = 1; i < quotes.size(); ++i) quotes[i] += quotes[i - 1];
        }

        std::vector<std::size_t> bounds{from};
        for (std::size_t i = 0; i < nominal.size(); ++i) {
            bool in_quotes = (quotes[i] & 1) != 0;
            std::size_t at = nominal[i];
            for (; at < end; ++at) {
                const char c = d[at];
                if (c == '"' && !json) {
                    in_quotes = !in_quotes;
                } else if (c == '\n' && !in_quotes) {
                    ++at;
                    break;
                }
            }
            if (at > bounds.back() && at < end) bounds.push_back(at);
        }
        bounds.push_back(end);
        return bounds;
    }

    // Parses and hashes every row that *starts* in [begin, stop), running past
    // `stop` to finish the last one. `end` is the end of the file. This is the
    // work worth splitting: it reads the members but writes only `out`, so any
    // number of threads may be inside it at once.
    // Returns where it left off: the first row start at or past `stop`, or
    // short of it if a malformed tail stopped it.
    std::size_t sweep(std::string_view d, const RowParser& parser, std::size_t begin,
                      std::size_t stop, std::size_t end, Chunk& out, std::size_t part,
                      RowSink* sink) const {
        std::vector<Field> fields(parser.width());
        // Sized before the first row, from the line count of the chunk's first
        // 64 KB, with an eighth to spare. Grown by doubling instead, each of
        // these was copied five or six times over on its way to a million rows,
        // and every copy wrote pages the kernel had to fault in fresh: on a 4M
        // pair this port took 145k minor faults where the C port, whose
        // `realloc` grows in place, took 95k. A guess that comes up short still
        // grows the ordinary way; one that comes up long reserves address
        // space the sweep never touches, which costs no memory.
        if (stop > begin) {
            const std::size_t span = stop - begin;
            const std::size_t probe = std::min<std::size_t>(span, std::size_t{64} << 10);
            const auto lines = static_cast<std::size_t>(
                std::count(d.data() + begin, d.data() + begin + probe, '\n'));
            const std::size_t guess = span / probe * lines + span % probe * lines / probe;
            out.starts.reserve(guess + guess / 8 + 16);
            out.hashes.reserve(guess + guess / 8 + 16);
        }
        std::size_t pos = begin;
        while (pos < stop) {
            // A line with nothing on it is not a row.
            if (d[pos] == '\n') {
                ++pos;
                continue;
            }
            if (d[pos] == '\r' && pos + 1 < end && d[pos + 1] == '\n') {
                pos += 2;
                continue;
            }
            // Keys only. This loop hashes the key and remembers where the row
            // starts; it never looks at a compared column, and parsing all
            // twenty to use two was most of what the sweep cost.
            const std::size_t next = parser.parse_keys(d, pos, end, fields.data());
            for (std::size_t i = 0; i < key_size_; ++i)
                if (fields[i] == kTooLong)
                    throw Error("a field larger than " + std::to_string(kMaxFieldLen) +
                                " bytes is more than this engine packs");
            out.starts.push_back(pos);
            out.hashes.push_back(key_hash(slab_, fields.data(), key_size_, opt_));
            // The row before this one ends where this one starts, which is what
            // `row_end` will say; its pages are still in memory.
            if (sink && out.starts.size() >= 2) {
                const std::size_t n = out.starts.size();
                sink->row(part, out.starts[n - 2], pos, out.hashes[n - 2]);
            }
            if (next <= pos) break;  // no progress: a malformed tail, not an endless loop
            pos = next;
        }
        return pos;
    }

    // `row` indexes `row_start_` and `row_hash_`, which the transfer above has
    // already filled: this adds the row to the table and nothing else.
    void insert(int row) {
        const std::size_t start = row_start_[static_cast<std::size_t>(row)];
        const std::uint64_t hash = row_hash_[static_cast<std::size_t>(row)];

        std::size_t slot = slot_of(hash);
        // Two buffers, both only touched here, and only on the one thread that
        // runs the insertions: the candidate's key and this row's. This row's
        // fields are re-parsed rather than carried over from the sweep because
        // the sweep produced twenty million of them and this branch wants two.
        Field* probe = scratch_.data();
        Field* mine = scratch_.data() + parser_.width();
        bool mine_parsed = false;
        for (;;) {
            const std::uint32_t v = table_[slot];
            if (v == kEmpty) {
                table_[slot] = slot_pack(hash, first_row_.size());
                first_row_.push_back(row);
                occurrences_.push_back(1);
                if (first_row_.size() * 2 > table_.size()) rehash();
                return;
            }
            // As in `lookup`: one word decides, or it does not and the two
            // misses behind it are earned.
            if (!slot_tag_is(v, hash)) {
                slot = (slot + 1) & mask_;
                continue;
            }
            const std::size_t at = slot_pos(v);
            const int candidate = first_row_[at];
            if (row_hash_[candidate] == hash) {
                if (!mine_parsed) {
                    // Keys only, like the candidate it is about to be compared
                    // with: this branch decides whether two rows carry the same
                    // key, and looks at nothing else.
                    parser_.parse_keys(slab_.bytes(), start, slab_.bytes().size(), mine);
                    mine_parsed = true;
                }
                keys_of(candidate, probe);
                bool ok = true;
                for (std::size_t i = 0; i < key_size_ && ok; ++i)
                    ok = same(slab_, probe[i], slab_, mine[i], opt_);
                if (ok) {
                    if (++occurrences_[at] == 2) {
                        ++dup_keys_;
                        ++dup_rows_;  // the first occurrence counts once the key repeats
                    }
                    ++dup_rows_;
                    if (track_later_) later_.push_back(row);
                    return;
                }
            }
            slot = (slot + 1) & mask_;
        }
    }

    // The high bits of an FNV hash are the well-mixed ones; fold them down.
    std::size_t slot_of(std::uint64_t hash) const { return (hash ^ (hash >> 32)) & mask_; }

    void rehash() {
        table_.assign(table_.size() * 2, kEmpty);
        mask_ = table_.size() - 1;
        // The slot width is a property of how many keys there are, so it is
        // recomputed here rather than carried over: a table that doubled is a
        // table that expects more of them.
        size_slots(table_.size());
        for (std::size_t key = 0; key < first_row_.size(); ++key) {
            const std::uint64_t h = row_hash_[static_cast<std::size_t>(first_row_[key])];
            std::size_t slot = slot_of(h);
            while (table_[slot] != kEmpty) slot = (slot + 1) & mask_;
            table_[slot] = slot_pack(h, key);
        }
    }

    const Slab& slab_;
    const RowParser& parser_;
    std::size_t key_size_;
    const Options& opt_;
    std::vector<std::size_t> row_start_;
    std::vector<std::uint64_t> row_hash_;
    // A slot is empty (0) or holds a key index in its low `pos_bits_` with the
    // top bits of that key's hash above it, so a probe that lands on the wrong
    // key is rejected by the word it has already loaded.
    //
    // Without it, rejecting a collision costs two more dependent loads --
    // `first_row_[at]`, then `row_hash_[candidate]` -- each a miss on an array
    // far too big to cache, and each waiting on the one before it. The C port
    // has had this since its own insert was the slow one; this port was probing
    // three cache lines deep to answer what one word can answer.
    //
    // The width is chosen from the row count rather than fixed, so the slot
    // stays four bytes and the table stays the size it was: at ten million rows
    // the index needs 24 bits and the tag gets the other 8. A tag that runs out
    // of bits degrades to no tag, not to a wrong answer, because the key
    // comparison behind it is unchanged.
    std::vector<std::uint32_t> table_;
    unsigned pos_bits_ = 32;
    std::uint32_t pos_mask_ = 0xFFFFFFFFu;
    std::size_t mask_ = 0;
    std::vector<int> first_row_;
    bool track_later_ = false;
    std::vector<int> later_;
    std::vector<std::int64_t> part_first_;
    std::vector<std::uint32_t> occurrences_;
    // Re-used by every probe. A lookup happens once per distinct key, so a
    // vector constructed here would be one heap allocation per row of the file.
    mutable std::vector<Field> scratch_;
    std::int64_t rows_ = 0, dup_keys_ = 0, dup_rows_ = 0;
};

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

// Guesses the delimiter from the header line, defaulting to a comma.
char detect_delimiter(std::string_view header) {
    char best = ',';
    long best_count = -1;
    for (char c : {',', ';', '\t', '|'}) {
        const long n = std::count(header.begin(), header.end(), c);
        if (n > best_count) {
            best = c;
            best_count = n;
        }
    }
    return best;
}

// Newline-delimited JSON if the first thing that is not whitespace is a brace.
// A CSV header can begin with anything else, and a `{` in the first column of a
// CSV header is not something this project has ever had to read.
Dialect sniff_dialect(std::string_view d) {
    for (std::size_t i = 0; i < d.size() && i < 64; ++i) {
        if (json_space(d[i])) continue;
        return d[i] == '{' ? Dialect::Json : Dialect::Csv;
    }
    return Dialect::Csv;
}

// A JSON file has no header row, so the column names are the keys of the first
// object, in the order it lists them. Two files may list them in different
// orders and still compare: the join is by name.
std::vector<std::string> json_header(const Slab& s, const std::string& path) {
    const std::string_view d = s.bytes();
    std::vector<std::string> names;
    std::size_t pos = 0;
    while (pos < d.size() && json_space(d[pos])) ++pos;
    if (pos >= d.size() || d[pos] != '{') throw Error("file has no JSON object to read: " + path);
    ++pos;
    for (;;) {
        while (pos < d.size() && json_space(d[pos])) ++pos;
        if (pos >= d.size() || d[pos] == '}') break;
        if (d[pos] == ',') {
            ++pos;
            continue;
        }
        if (d[pos] != '"') break;
        bool escaped = false;
        const std::size_t from = pos + 1;
        const std::size_t close = skip_json_string(d, pos, d.size(), &escaped);
        if (close <= from) break;
        names.emplace_back(d.substr(from, close - 1 - from));
        pos = close;
        while (pos < d.size() && json_space(d[pos])) ++pos;
        if (pos >= d.size() || d[pos] != ':') break;
        ++pos;
        while (pos < d.size() && json_space(d[pos])) ++pos;
        if (pos >= d.size()) break;
        if (d[pos] == '"') {
            bool ignore = false;
            pos = skip_json_string(d, pos, d.size(), &ignore);
        } else if (d[pos] == '{' || d[pos] == '[') {
            pos = RowParser::skip_nested_public(d, pos, d.size());
        } else {
            while (pos < d.size() && d[pos] != ',' && d[pos] != '}' && !json_space(d[pos])) ++pos;
        }
    }
    if (names.empty()) throw Error("the first JSON object has no keys: " + path);
    return names;
}

// The header row's names, and where the first data row starts.
std::pair<std::vector<std::string>, std::size_t> read_header(const Slab& s, char delimiter,
                                                             const std::string& path) {
    const std::string_view d = s.bytes();
    if (d.empty()) throw Error("file has no header row: " + path);
    std::vector<std::string> names;
    std::size_t pos = 0;
    for (;;) {
        Field field;
        std::size_t next;
        if (pos < d.size() && d[pos] == '"') {
            const std::size_t close = skip_quoted(d, pos + 1, d.size());
            const std::size_t body_end = close > pos + 1 ? close - 1 : pos + 1;
            next = next_of2(d, close, d.size(), delimiter, '\n');
            field = pack(pos + 1, body_end - (pos + 1),
                         next_of1(d, pos + 1, body_end, '"') < body_end);
        } else {
            next = next_of2(d, pos, d.size(), delimiter, '\n');
            std::size_t stop = next;
            if (stop > pos && d[stop - 1] == '\r') --stop;
            field = pack(pos, stop - pos, false);
        }
        names.push_back(text_of(s, field));
        if (next >= d.size()) return {names, d.size()};
        if (d[next] == '\n') return {names, next + 1};
        pos = next + 1;
    }
}

struct Resolved {
    std::vector<std::string> compared, only_in_a, only_in_b;
};

Resolved resolve(const std::vector<std::string>& a, const std::vector<std::string>& b,
                 const Options& opt) {
    const auto has = [](const std::vector<std::string>& v, const std::string& n) {
        return std::find(v.begin(), v.end(), n) != v.end();
    };
    for (const auto& k : opt.key)
        if (!has(a, k) || !has(b, k)) throw Error("key column(s) missing from one of the files: " + k);

    // A name in neither file is a typo, and this is the typo that hides: `--key`
    // makes the answer impossible and `--compare` refuses, but a misspelled
    // `--ignore` silently compares the column it meant to drop and calls it
    // changed on every row. In *neither* file rather than in both -- ignore is
    // subtractive, so a name only one side carries is real and harmless.
    for (const auto& c : opt.ignore)
        if (!has(a, c) && !has(b, c))
            throw Error("ignore column(s) present in neither file: " + c);

    Resolved out;
    for (const auto& c : a)
        if (!has(b, c) && !has(opt.key, c)) out.only_in_a.push_back(c);
    for (const auto& c : b)
        if (!has(a, c) && !has(opt.key, c)) out.only_in_b.push_back(c);

    if (!opt.compare.empty()) {
        // Named explicitly: the order is the caller's, and a name that is not in
        // both files is an error. A key or an ignored column is dropped rather
        // than compared -- `-k id -c id,a` compares `a`, and `-c a,b -i b`
        // compares `a` -- which is the rule the C and Rust ports apply. This port
        // used to compare them, and with `-i` that is a wrong answer: every row
        // whose ignored column differs was counted as changed.
        for (const auto& c : opt.compare) {
            if (!has(a, c) || !has(b, c))
                throw Error("compared column missing from one of the files: " + c);
            if (has(opt.key, c) || has(opt.ignore, c)) continue;
            out.compared.push_back(c);
        }
        return out;
    }
    for (const auto& c : a)
        if (has(b, c) && !has(opt.key, c) && !has(opt.ignore, c)) out.compared.push_back(c);
    return out;
}

// ---------------------------------------------------------------------------
// The join
// ---------------------------------------------------------------------------

// A row list that stops growing at the report cap but keeps counting: the counts
// are always exact and the embedded rows always capped, so holding more than the
// cap would only ever serve an export this port does not write.
struct Capped {
    std::vector<std::pair<int, int>> held;  // row, and the row it matched
    std::size_t cap;
    std::int64_t total = 0;

    explicit Capped(std::size_t c) : cap(c) {}

    void push(int row, int mate) {
        ++total;
        if (held.size() <= cap) held.emplace_back(row, mate);  // one past, to detect truncation
    }

    bool truncated() const { return total > static_cast<std::int64_t>(cap); }
};

// One row's key, and nothing else.
//
// Two callers want this: the changed rows, which carry the key beside their
// per-cell diffs, and the duplicate-key section, which keeps one row per
// repeated key and nothing but the key of it. The second was calling
// `row_values` and resizing the result away, which on a twenty-column file
// built eighteen strings per duplicated key to drop them.
//
// `fields_of` fills `width` fields however few the caller goes on to read, so
// the Field buffer is full width here even though only `key_size` of them become
// strings. Passing a shorter width to `row_values` instead would hand the parser
// a buffer smaller than it writes.
std::vector<Val> key_values(const Slab& s, const RowIndex& idx, int row, std::size_t width,
                            std::size_t key_size, const Options& o) {
    std::vector<Field> fields(width);
    idx.fields_of(row, fields.data());
    std::vector<Val> out;
    out.reserve(key_size);
    for (std::size_t i = 0; i < key_size; ++i) out.push_back(value_of(s, fields[i], o, Fold::display));
    return out;
}

std::vector<Val> row_values(const Slab& s, const RowIndex& idx, int row, std::size_t width,
                            const Options& o) {
    std::vector<Field> fields(width);
    idx.fields_of(row, fields.data());
    std::vector<Val> out;
    out.reserve(width);
    for (Field f : fields) out.push_back(value_of(s, f, o, Fold::display));
    return out;
}

// ---------------------------------------------------------------------------
// The byte proof
// ---------------------------------------------------------------------------
//
// Two rows that agree byte for byte through the end of the last column either
// file wants are equal in every column that matters, and neither needs parsing
// to say so. In two files that mostly agree that is most of the rows, which is
// why the join tries it before it parses anything. Three pieces: what the
// proof can promise for this pair of files (ProofPlan), when to stop trying it
// (ProofBackoff), and the ways a row can be proven (ProofRows).

// What the proof can promise for one pair of files, decided once.
struct ProofPlan {
    // Both files are ndjson. No fixed offset to promise, so the run is built per
    // row and the mate's tail is scanned. See `RowParser::json_tail_is_clean`.
    bool json = false;
    // Both files are CSV read with the same delimiter and every compared column
    // sits at the same column number on both sides, ascending -- then equal
    // bytes mean equal columns, and the last compared column is the one that
    // closes last. Two headers that order the same columns differently would
    // put the same bytes under different names, which is exactly what this
    // refuses. JSON is excluded: it has no fixed offset to be aligned about. A
    // value there is found by name, and a name repeated in one object takes its
    // last value for a compared column, so a duplicate past the diverging byte
    // could carry a value the prefix never saw; the bounded tail scan is what
    // rules that out.
    bool aligned = false;
    // Whether the proof's bytes also cover the keys, so that it can stand in for
    // the key comparison and run before the mate is parsed: always on objects,
    // where the run is taken over every wanted value; on CSV when every key sits
    // at the same column in both files, ahead of the guard.
    bool keys_in_proof = false;
    // How many delimiters from a CSV row's start close its guard column: where
    // `guard_span` can find the proof's span without the parse. 0 where it
    // cannot -- JSON, or keys the proof does not cover. A guard that is the last
    // column of A closes on the line ending, which `guard_span` accepts there;
    // until it did, such a file never took this path at all.
    std::size_t guard_commas = 0;
    // The same fields in runs, where a column nobody wants sits among them --
    // an ignored `updated_at` in the middle of the row, say. One run is the
    // plain prefix. More, and the proof compares each run of wanted columns as
    // bytes and skips the unwanted ones between them in each row on its own: an
    // ignored value that differs cannot stop a row proving equal, and one that
    // differs in length cannot misalign what follows, because the skip counts
    // fields rather than bytes. `seg_fields[j]` wanted fields are followed by
    // `gap_fields[j]` unwanted ones; the last run has no gap. The C port has the
    // same proof (#229).
    std::vector<std::size_t> seg_fields, gap_fields;
    std::size_t runs() const { return seg_fields.empty() ? 1 : seg_fields.size(); }
};

// `a_src[i]` is the CSV column slot `i` is read from in A, -1 where A lacks it;
// `a_columns` is how many columns A's header has.
ProofPlan plan_proof(const Slab& a, const Slab& b, char a_delim, char b_delim, std::size_t nc,
                     std::size_t width, std::size_t key_size, const std::vector<int>& a_src,
                     const std::vector<int>& b_src, std::size_t a_columns) {
    ProofPlan plan;
    plan.json = a.dialect() == Dialect::Json && b.dialect() == Dialect::Json && nc > 0;
    plan.aligned = a.dialect() != Dialect::Json && b.dialect() != Dialect::Json &&
                   a_delim == b_delim && nc > 0 && a_src.size() == width && b_src.size() == width;
    for (std::size_t i = key_size; plan.aligned && i < width; ++i)
        plan.aligned = a_src[i] >= 0 && a_src[i] == b_src[i] &&
                       (i == key_size || a_src[i] > a_src[i - 1]);
    plan.keys_in_proof = plan.json;
    if (plan.aligned) {
        plan.keys_in_proof = true;
        for (std::size_t i = 0; plan.keys_in_proof && i < key_size; ++i)
            plan.keys_in_proof =
                a_src[i] >= 0 && a_src[i] == b_src[i] && a_src[i] < a_src[width - 1];
    }
    (void)a_columns;
    if (plan.keys_in_proof && !plan.json) {
        const auto g = static_cast<std::size_t>(a_src[width - 1]);
        plan.guard_commas = g + 1;
        std::vector<char> wanted(g + 1, 0);
        for (std::size_t i = 0; i < width; ++i) wanted[static_cast<std::size_t>(a_src[i])] = 1;
        std::vector<std::size_t> seg{0}, gap{0};
        for (std::size_t col = 0; col <= g; ++col) {
            if (!wanted[col]) {
                ++gap.back();
                continue;
            }
            if (gap.back()) {
                seg.push_back(0);
                gap.push_back(0);
            }
            ++seg.back();
        }
        if (seg.size() > 1) {
            plan.seg_fields = std::move(seg);
            plan.gap_fields = std::move(gap);
        }
    }
    return plan;
}

// A proof that keeps failing is a scan for nothing -- two files where every row
// really has changed pay for it on every row -- so after kProofBackoff failures
// in a row it is only attempted every kProofBackoff rows, until one succeeds and
// it is on again.
class ProofBackoff {
  public:
    // Whether to try the proof on the row at position `at` of this worker's range.
    bool attempt(std::size_t at) const {
        return refused_ < kProofBackoff || (at & (kProofBackoff - 1)) == 0;
    }
    void proved() { refused_ = 0; }
    void failed() {
        if (refused_ < kProofBackoff) ++refused_;
    }

  private:
    unsigned refused_ = 0;
};

// The furthest end of any real field among `n` fields, or `floor` if none reaches it.
inline std::size_t run_end(const Field* fields, std::size_t n, std::size_t floor) {
    std::size_t t = floor;
    for (std::size_t i = 0; i < n; ++i) {
        if (!is_real(fields[i])) continue;
        const std::size_t e = offset_of(fields[i]) + len_of(fields[i]);
        if (e > t) t = e;
    }
    return t;
}

// From the end of a CSV field's value to the delimiter or line ending that closes it.
inline std::size_t field_close(const char* d, std::size_t t, std::size_t end, char delim) {
    while (t < end && d[t] != delim && d[t] != '\n' && d[t] != '\r') ++t;
    return t;
}

// The ways one row of A can be proven equal to its mate in B without comparing
// its columns. Each takes the row and answers; the caller keeps the counts and
// the backoff, so this holds no state of its own.
// One row of A as the join sees it: its bytes, from its start to the next
// row's, and its key hash. By offsets rather than by row number, so the join
// can run while A's index is still being built.
struct ARow {
    std::size_t lo, hi;
    std::uint64_t hash;
};

struct ProofRows {
    const Slab& a;
    const Slab& b;
    const RowIndex& bi;
    const ProofPlan plan;  // a copy, one per worker, and the loop reads it from here
    char a_delim;
    std::size_t width, key_size, nc;

    enum class Guard {
        Untried,   // the span could not be found (a short row, or no guard); parse as before
        Removed,   // no row of B has this key
        Proven,    // B's first hash match opens with the same bytes
        Unsettled  // a candidate the bytes do not settle: parse, and look up by the keys
    };

    // The proof's span found by counting delimiters, without parsing the row.
    // `seg` is the worker's scratch for the gapped proof, two words a run.
    Guard guard(const ARow& row, Field* probe, std::size_t* seg, int& mate) const {
        if (!plan.guard_commas) return Guard::Untried;
        if (plan.runs() > 1) return gapped(row, seg, mate);
        const std::size_t a_lo = row.lo;
        const std::size_t span =
            guard_span(a.bytes(), a_lo, row.hi, a_delim, plan.guard_commas);
        if (!span) return Guard::Untried;
        bool proven = false;
        mate = bi.lookup_proof(a, nullptr, row.hash, probe,
                               a.bytes().data() + a_lo, span, 0, false, proven);
        if (mate < 0) return Guard::Removed;
        return proven ? Guard::Proven : Guard::Unsettled;
    }

    // The gapped form: where each run of wanted columns starts in A's row and
    // how many bytes it takes, through the byte that closes it; then B's first
    // hash match, and each run's bytes where B's own gaps put it. A run's bytes
    // include its closing byte, so equal bytes leave B at the start of its next
    // field, as they leave A.
    Guard gapped(const ARow& row, std::size_t* seg, int& mate) const {
        const std::size_t runs = plan.runs();
        const std::size_t a_lo = row.lo, a_hi = row.hi;
        FieldWalker wa(a.bytes(), a_lo, a_hi, a_delim);
        std::size_t at = a_lo;
        for (std::size_t j = 0; j < runs; ++j) {
            const bool last = j + 1 == runs;
            const std::size_t end = wa.past(at, plan.seg_fields[j], last);
            if (end == FieldWalker::kShort) return Guard::Untried;
            seg[2 * j] = at;
            seg[2 * j + 1] = end - at;
            if (last) break;
            at = wa.past(end, plan.gap_fields[j], false);
            if (at == FieldWalker::kShort) return Guard::Untried;
        }
        mate = bi.first_match(row.hash);
        if (mate < 0) return Guard::Removed;
        const char* da = a.bytes().data();
        const char* db = b.bytes().data();
        const std::size_t b_hi = bi.row_end(mate);
        FieldWalker wb(b.bytes(), bi.row_begin(mate), b_hi, a_delim);
        at = bi.row_begin(mate);
        for (std::size_t j = 0; j < runs; ++j) {
            const std::size_t n = seg[2 * j + 1];
            if (n > b_hi - at || common_prefix(da + seg[2 * j], db + at, n) != n)
                return Guard::Unsettled;
            at += n;
            if (j + 1 == runs) break;
            at = wb.past(at, plan.gap_fields[j], false);
            if (at == FieldWalker::kShort) return Guard::Unsettled;
        }
        return Guard::Proven;
    }

    // How many bytes of A's row prove the whole row, keys included -- through the
    // byte that closes the last value either file wants -- or 0 where this row
    // cannot be proven that way. Needs the row's fields.
    std::size_t run_before_lookup(const ARow& row, const Field* fa) const {
        if (!plan.keys_in_proof) return 0;
        const char* d = a.bytes().data();
        const std::size_t a_lo = row.lo, a_end = row.hi;
        std::size_t t = a_lo;
        if (plan.json) {
            t = run_end(fa, width, a_lo);
        } else {
            const Field g = fa[width - 1];
            if (!is_real(g)) return 0;
            t = field_close(d, offset_of(g) + len_of(g), a_end, a_delim);
        }
        return t < a_end ? t + 1 - a_lo : 0;
    }

    enum class Proof { NotTried, Proven, Failed };

    // The JSON form, after the lookup has compared the keys: through the byte that
    // closes the last compared value -- whichever it turns out to be, since two
    // objects need not list their names in the same order. The keys are not in the
    // run and do not need to be.
    Proof json_after_lookup(const ARow& row, int mate, const Field* fa) const {
        const std::size_t a_lo = row.lo, a_end = row.hi;
        const std::size_t b_lo = bi.row_begin(mate);
        const std::size_t b_n = bi.row_end(mate) - b_lo;
        const std::size_t t = run_end(fa + key_size, nc, a_lo);
        const std::size_t need = t + 1 - a_lo;
        const bool same = t < a_end && bi.parser().json_rows_match(a.bytes(), a_lo, a_end, need,
                                                                   b.bytes(), b_lo, b_lo + b_n);
        return same ? Proof::Proven : Proof::Failed;
    }

    // The CSV form, after the lookup: through the byte that closes the last
    // compared column, not up to it. Agreeing as far as the field's last byte says
    // only that the mate's field starts the same way: `cc` is a prefix of
    // `cccccccc`, and a quoted field the mate carries on with a doubled quote reads
    // the same that far too. It is the delimiter or the line ending after it that
    // says the mate's field stopped where this one did.
    Proof csv_after_lookup(const ARow& row, int mate, const Field* fa) const {
        const Field g = fa[width - 1];
        if (!is_real(g)) return Proof::NotTried;
        const char* d = a.bytes().data();
        const std::size_t a_lo = row.lo, a_end = row.hi;
        const std::size_t b_lo = bi.row_begin(mate);
        const std::size_t b_n = bi.row_end(mate) - b_lo;
        const std::size_t t = field_close(d, offset_of(g) + len_of(g), a_end, a_delim);
        const std::size_t need = t + 1 - a_lo;
        const bool same = t < a_end && need <= b_n &&
                          common_prefix(d + a_lo, b.bytes().data() + b_lo, need) == need;
        return same ? Proof::Proven : Proof::Failed;
    }
};

}  // namespace

Result compare(const std::string& a_path, const std::string& b_path, const Options& opt) {
    const auto started = std::chrono::steady_clock::now();
    if (opt.key.empty()) throw Error("at least one key column is required");

    // Parquet is not a text format and is not read as one: it goes to the
    // columnar path in pqdiff.cpp, which never materialises a row. Both sides
    // have to be Parquet -- comparing a column store against a byte stream
    // would mean building rows out of one of them, and that is the cost the
    // columnar path exists to avoid.
    {
        const bool ap = is_parquet(a_path), bp = is_parquet(b_path);
        if (ap != bp)
            throw Error("one file is parquet and the other is not; convert one of them first");
        if (ap) return compare_parquet(a_path, b_path, opt);
    }

    Slab a(a_path), b(b_path);
    // The two sides may be in different formats: comparing a CSV export against
    // the JSON the same pipeline emits is the case that motivates this.
    a.set_dialect(sniff_dialect(a.bytes()));
    b.set_dialect(sniff_dialect(b.bytes()));

    const char a_delim =
        opt.delimiter.value_or(detect_delimiter(a.bytes().substr(
            0, next_of1(a.bytes(), 0, a.bytes().size(), '\n'))));
    const char b_delim =
        opt.delimiter.value_or(detect_delimiter(b.bytes().substr(
            0, next_of1(b.bytes(), 0, b.bytes().size(), '\n'))));

    std::vector<std::string> a_header, b_header;
    std::size_t a_start = 0, b_start = 0;
    if (a.dialect() == Dialect::Json) {
        a_header = json_header(a, a_path);
    } else {
        std::tie(a_header, a_start) = read_header(a, a_delim, a_path);
    }
    if (b.dialect() == Dialect::Json) {
        b_header = json_header(b, b_path);
    } else {
        std::tie(b_header, b_start) = read_header(b, b_delim, b_path);
    }
    const Resolved resolved = resolve(a_header, b_header, opt);

    const std::size_t key_size = opt.key.size();
    const std::size_t nc = resolved.compared.size();
    const std::size_t width = key_size + nc;

    std::vector<std::string> wanted = opt.key;
    wanted.insert(wanted.end(), resolved.compared.begin(), resolved.compared.end());
    const auto positions = [&](const std::vector<std::string>& header) {
        std::vector<int> out;
        for (const auto& n : wanted) {
            const auto it = std::find(header.begin(), header.end(), n);
            out.push_back(it == header.end() ? -1 : static_cast<int>(it - header.begin()));
        }
        return out;
    };

    // A CSV parser is told which column number each slot comes from; a JSON one
    // is told which key. `wanted` is the same list either way.
    const auto names_for = [&](const std::vector<std::string>& header) {
        std::vector<std::string> out;
        for (const auto& n : wanted)
            out.push_back(std::find(header.begin(), header.end(), n) == header.end() ? "" : n);
        return out;
    };
    RowParser ap = a.dialect() == Dialect::Json ? RowParser(Dialect::Json, names_for(a_header))
                                                : RowParser(a_delim, positions(a_header));
    RowParser bp = b.dialect() == Dialect::Json ? RowParser(Dialect::Json, names_for(b_header))
                                                : RowParser(b_delim, positions(b_header));
    // Key slots come first in both, so this is all a key-only parse needs to
    // know: where to stop, and how many slots to fill.
    ap.set_key_size(key_size);
    bp.set_key_size(key_size);

    // Can a prefix prove a row equal, and how? See ProofPlan.
    const ProofPlan plan = plan_proof(a, b, a_delim, b_delim, nc, width, key_size, ap.source(),
                                      bp.source(), a_header.size());

    // The two indexes share nothing, so they are built at the same time, and
    // each is split further into chunks. Two files across N cores is N/2 chunks
    // each -- so the whole machine is busy, not just two of it. An exception
    // thrown on the worker is carried back and rethrown here, because a parse
    // error has to reach the caller as an error and not as a crash.
    unsigned budget = opt.threads;
    if (budget == 0) budget = std::max(1u, std::thread::hardware_concurrency());
    // ... but only where splitting a sweep pays for itself, which a two-way
    // split does not.
    //
    // Chunking costs a whole extra pass over the bytes before any row is read:
    // `chunk_bounds` has to count quotes to know whether a nominal split lands
    // inside a quoted field. Split four ways that pass is a quarter of the file
    // per thread and the sweep it enables is four times narrower. Split two
    // ways it is half the file, single-threaded -- the counter loop has one
    // entry and so spawns nothing -- to make the sweep only twice as narrow,
    // and it runs four concurrent read streams instead of two.
    //
    // Measured, 4M CSV pair, `--threads 4`, nine interleaved rounds, wall time
    // against the two-way split this used to pick:
    //
    //     per_file  1     2      3      4      6      8
    //     vs two    0.923 1.000  0.965  0.936  0.936  0.903
    //
    // Two is not a point on a curve, it is the worst value available, and it
    // was what every four-core runner got. Not splitting at all is 0.925 of it
    // over eleven paired rounds, faster in all eleven, and the whole phase then
    // costs what it cost on one thread -- the sweep does not scale on this
    // machine at any width, so the split was buying nothing and paying for it.
    //
    // **That conclusion was right about the measurement and wrong about the
    // cause, and the cause has since been fixed.** The sweep did not scale
    // because two threads' `Chunk`s shared a cache line and each row either
    // appended moved it; the split was paying for the sharing rather than for
    // the chunking. `Chunk` is padded now, and the curve is a different one:
    //
    //     against no split at all, 4M pair, --threads 4, 15 paired rounds
    //     per_file      budget/2        budget
    //     wall          1.11x           1.11x
    //     cpu           0.96x           0.94x
    //
    // Eleven percent of the wall for four percent more CPU, which is what
    // threading is supposed to trade, and `budget / 2` buys it for less than
    // `budget` does. That is also the rule the C port has always used, and both
    // files are swept at once, so half the machine each is the whole of it.
    //
    // (The quote-counting pass described above is no longer paid up front: the
    // splits are now guessed and checked after the sweep, and counted only when
    // a guess lands inside a quoted field -- see the constructor. These
    // measurements predate that and were not re-taken.)
    const unsigned per_file = budget > 1 ? budget / 2 : 1;

    // What one part of the join counts into.
    struct Part {
        std::int64_t matched = 0;
        std::vector<ColumnStat> columns;
        std::vector<std::pair<int, int>> changed, removed;
        std::int64_t changed_total = 0, removed_total = 0;
    };
    // One worker's buffers, and its proof backoff.
    struct Scratch {
        std::vector<Field> fa, fb, probe;
        std::vector<std::size_t> seg;
        ProofBackoff backoff;
        Scratch(std::size_t width, std::size_t runs)
            : fa(width), fb(width), probe(width), seg(2 * runs) {}
    };
    // One row of A against its mate in B, counted into `out`. `row` is A's row
    // number for the report's row lists, and -1 when the join runs inside A's
    // sweep, which only happens without them (see `fused`). `at` is the row's
    // place in the worker's run, for the backoff; it decides how the answer is
    // reached, never what it is.
    // B's rows a row of A matched, one bit each, when the join runs inside A's
    // sweep with row lists: the added section is then B's keys whose bit is
    // clear, read off in one ordered walk instead of a second probed pass over
    // A's index -- a pass that past page cache reads A's keys back from disk.
    std::unique_ptr<std::atomic<std::uint64_t>[]> b_seen;
    const auto mark_seen = [&](int mate) {
        if (b_seen)
            b_seen[static_cast<std::size_t>(mate) >> 6].fetch_or(
                std::uint64_t{1} << (static_cast<std::size_t>(mate) & 63), std::memory_order_relaxed);
    };
    // Whether any flag changes what equal means, asked once. Asked per cell --
    // `cell_differs`, then `same`, then `is_absent` twice -- it was four loads
    // and four tests of the options each time, kept in the loop because `same`
    // is a call the compiler cannot see through: 250M of 2.69G instructions on
    // a 400k-row pair. With none set a cell is absent or it is its bytes, and
    // the loop below says so directly, as the C port's does.
    const bool plain = !needs_normalising(opt);
    auto join_row = [&](const ProofRows& proof, Part& out, Scratch& s, const ARow& ar, int row,
                        std::size_t at) {
        const bool attempt = s.backoff.attempt(at);
        // The proof's span without the parse: the guard's delimiter sits a
        // fixed count from the row start. Most rows prove out here and never
        // pay for their fields; a row the bytes do not settle is parsed
        // below, looked up again by its keys -- which also covers the hash
        // collision `lookup_proof` passed over -- and compared.
        bool spanned = false;
        if (attempt) {
            int guarded = -1;
            switch (proof.guard(ar, s.probe.data(), s.seg.data(), guarded)) {
                case ProofRows::Guard::Untried:
                    break;
                case ProofRows::Guard::Removed:
                    ++out.removed_total;
                    if (out.removed.size() <= opt.max_rows) out.removed.emplace_back(row, -1);
                    return;
                case ProofRows::Guard::Proven:
                    ++out.matched;
                    mark_seen(guarded);
                    s.backoff.proved();
                    return;
                case ProofRows::Guard::Unsettled:
                    s.backoff.failed();
                    spanned = true;
                    break;
            }
        }
        ap.parse(a.bytes(), ar.lo, a.bytes().size(), s.fa.data());
        // How many bytes of A's row prove the whole row, keys included --
        // through the byte that closes the last value either file wants --
        // or 0 where this row cannot be proven that way.
        const std::size_t need = !spanned && attempt ? proof.run_before_lookup(ar, s.fa.data()) : 0;
        // The hash is the one the sweep computed for this row: the same
        // bytes through the same function, so computing it again here would
        // be a second pass over every key in the file for the same number.
        bool proven = false;
        const int mate =
            need ? proof.bi.lookup_proof(a, s.fa.data(), ar.hash, s.probe.data(),
                                   a.bytes().data() + ar.lo, need, ar.hi,
                                   proof.plan.json, proven)
                 : proof.bi.lookup(a, s.fa.data(), ar.hash, s.probe.data());
        if (mate < 0) {
            ++out.removed_total;
            if (out.removed.size() <= opt.max_rows) out.removed.emplace_back(row, -1);
            return;
        }
        ++out.matched;
        mark_seen(mate);
        if (need) {
            if (proven) {
                s.backoff.proved();
                return;
            }
            s.backoff.failed();
        }
        // The proofs that need the mate. They sit here rather than inside
        // `lookup` because this port's `lookup` compares only the keys --
        // `keys_of`, not `fields_of` -- so the mate's row is still unparsed
        // at this point and a proof saves the whole of it. The Rust and Zig
        // ports had to put theirs *inside* the lookup for that reason:
        // theirs materialise the mate's whole row to compare its keys, and a
        // proof after that saves only the column comparison. Same proof,
        // different place, because the surrounding code differs.
        if (!need && attempt) {
            const ProofRows::Proof after = proof.plan.json ? proof.json_after_lookup(ar, mate, s.fa.data())
                                           : !spanned && proof.plan.aligned
                                               ? proof.csv_after_lookup(ar, mate, s.fa.data())
                                               : ProofRows::Proof::NotTried;
            if (after == ProofRows::Proof::Proven) {
                s.backoff.proved();
                return;
            }
            if (after == ProofRows::Proof::Failed) s.backoff.failed();
        }
        proof.bi.fields_of(mate, s.fb.data());
        bool any = false;
        if (plain) {
            for (std::size_t i = 0; i < nc; ++i) {
                const Field x = s.fa[key_size + i], y = s.fb[key_size + i];
                const bool xa = !is_real(x) || len_of(x) == 0, ya = !is_real(y) || len_of(y) == 0;
                if ((xa || ya) ? xa != ya : !same_plain(a, x, b, y)) {
                    any = true;
                    ++out.columns[i].changed;
                    if (ya) ++out.columns[i].blanked;
                    if (xa) ++out.columns[i].filled;
                }
            }
        } else {
            for (std::size_t i = 0; i < nc; ++i) {
                const Field x = s.fa[key_size + i], y = s.fb[key_size + i];
                if (cell_differs(a, x, b, y, opt)) {
                    any = true;
                    ++out.columns[i].changed;
                    if (is_absent(b, y, opt)) ++out.columns[i].blanked;
                    if (is_absent(a, x, opt)) ++out.columns[i].filled;
                }
            }
        }
        if (any) {
            ++out.changed_total;
            if (out.changed.size() <= opt.max_rows) out.changed.emplace_back(row, mate);
        }
    };

    // Past page cache the join runs inside A's sweep instead of after it.
    //
    // Past memory every pass is a read from disk, and the join used to be the
    // fourth: both sweeps, then both files again for the rows to compare -- at
    // 150M rows as long as a cold read of both files. Built first, B's index is
    // all the join needs from B, so A's rows can be joined as A's sweep finds
    // them, while their pages are in memory, and A is read once.
    //
    // The join compares a key's first row, and which row is first is only known
    // once the insertion has seen them all. So every row is joined as though it
    // were first, and the rows that turn out to repeat a key -- `later()`, kept
    // by the insertion -- are joined again afterwards and subtracted. Counts are
    // sums, so that is exact. The C port does the same (#241).
    //
    // With --json too: each part keeps its own first changed and removed rows,
    // and the repeats among them are dropped once the insertion has named them
    // (`fused_lists` below), the way the Rust port does (#245). Not with the
    // normalisation flags: --ignore-case refuses from inside a comparison, and a
    // repeat's refusal could not be taken back.
    //
    // CSVDIFF_FUSED_JOIN=1 takes this path at any size, which is how the tests
    // reach it. With --json as well -- which is how the fuzzer runs -- the
    // join after the sweep runs too, and its counts and its row lists must be
    // the ones the join inside the sweep found, or the run fails.
    const bool normalising = opt.trim || opt.ignore_case || opt.empty_is_null || opt.tolerance > 0.0;
    const bool fused_forced = [] {
        const char* v = std::getenv("CSVDIFF_FUSED_JOIN");
        return v && *v && std::string_view(v) != "0";
    }();
    const bool fused = !normalising &&
                       ((a.bytes().size() + b.bytes().size() > (std::size_t{8} << 30) && budget > 1) ||
                        fused_forced);
    std::vector<Part> fused_parts;

    std::optional<RowIndex> ai_slot, bi_slot;
    std::exception_ptr worker_failure;
    if (fused) {
        bi_slot.emplace(b, bp, b_start, key_size, opt, budget, "B ");
        if (opt.row_lists)
            b_seen = std::make_unique<std::atomic<std::uint64_t>[]>(
                static_cast<std::size_t>(bi_slot->rows()) / 64 + 1);
        const ProofRows proof{a, b, *bi_slot, plan, a_delim, width, key_size, nc};
        fused_parts.resize(budget);
        for (auto& part : fused_parts) part.columns.resize(nc);
        std::vector<Scratch> scratch(budget, Scratch(width, plan.runs()));
        std::vector<std::size_t> seen(budget, 0);
        FnSink sink;
        // A row is named by its place in its part: which row of the file it is
        // depends on the parts before it, which are still being swept. The
        // lists are moved onto file rows once the sweep is done.
        sink.on_row = [&](std::size_t p, std::size_t lo, std::size_t hi, std::uint64_t hash) {
            const std::size_t local = seen[p]++;
            join_row(proof, fused_parts[p], scratch[p], ARow{lo, hi, hash}, static_cast<int>(local),
                     local);
        };
        sink.on_reset = [&](std::size_t p) {
            fused_parts[p] = Part{};
            fused_parts[p].columns.resize(nc);
            scratch[p].backoff = ProofBackoff{};
            seen[p] = 0;
        };
        ai_slot.emplace(a, ap, a_start, key_size, opt, budget, "A ", &sink);
    } else {
        std::thread worker([&] {
            try {
                bi_slot.emplace(b, bp, b_start, key_size, opt, per_file, "B ");
            } catch (...) {
                worker_failure = std::current_exception();
            }
        });
        try {
            ai_slot.emplace(a, ap, a_start, key_size, opt, per_file, "A ");
        } catch (...) {
            worker.join();
            throw;
        }
        worker.join();
    }
    if (worker_failure) std::rethrow_exception(worker_failure);
    const RowIndex& ai = *ai_slot;
    const RowIndex& bi = *bi_slot;
    // The join after the sweep marks B's matched rows too, so that its added
    // rows are the same walk over the bitmap rather than a probed pass over A's
    // index. Not when the two joins are being held against each other: the
    // probed pass is then the independent answer the walk is checked against.
    if (opt.row_lists && !b_seen && !fused_forced)
        b_seen = std::make_unique<std::atomic<std::uint64_t>[]>(
            static_cast<std::size_t>(bi.rows()) / 64 + 1);
    // From here on there is one thread again, so one timer covers it.
    RowIndex::Phases whole("");

    Result r;
    r.key = opt.key;
    r.compared = resolved.compared;
    r.only_in_a = resolved.only_in_a;
    r.only_in_b = resolved.only_in_b;
    r.a_cols = a_header.size();
    r.b_cols = b_header.size();
    r.columns.resize(nc);
    for (std::size_t i = 0; i < nc; ++i) r.columns[i].name = resolved.compared[i];

    Capped changed(opt.max_rows), added(opt.max_rows), removed(opt.max_rows);
    std::int64_t matched = 0;

    // The two directions of the join touch different outputs -- one fills
    // changed, removed and the column counts, the other only added -- and read
    // both indexes without writing either, so they run at the same time. Each
    // owns its field buffers and its probe scratch; nothing is shared but the
    // two finished indexes and the two mappings, which are const from here on.
    // A's side is the long pole: every distinct key is looked up in B, both rows
    // are re-parsed, and every compared column is examined. It splits over
    // contiguous ranges of A's keys. Each range accumulates into its own counts,
    // its own column stats and its own capped row lists; because the ranges are
    // contiguous and merged in order, the result is identical to one thread's,
    // including which rows survive the cap.

    const std::vector<int>& a_keys = ai.first_rows();
    // The whole budget, unless a B-side pass is going to run beside this one.
    //
    // `budget - 1` was unconditional, and with `--json` it is at least
    // defensible: `b_side` runs concurrently and takes `b_ways` threads of its
    // own, so the two together already oversubscribe the machine and holding one
    // back trims it. Without `--json` there is no `b_side` at all -- it is gated
    // on `row_lists` -- and the subtraction just left the join short of the
    // machine for no one.
    //
    // At `--threads 2` that is the whole difference between a split join and a
    // serial one, because `budget - 1` is 1. The phase is 56% of the wall, so
    // the second thread bought nothing: measured, the run was **1.01x** of
    // `--threads 1`, where C's second thread is worth 1.46x.
    //
    //     --threads   1       2       4
    //     before      2.83s   2.79s   1.46s
    //     after       2.81s   1.84s   1.31s
    //
    // The `--json` path keeps the old arithmetic. Whether `budget - 1` is the
    // right reservation when `b_side` itself takes `b_ways` threads is a
    // separate question this does not answer.
    //
    // That separate question has gone away: the added rows are now read off a
    // bitmap the A side fills (`b_seen`), after it, so nothing runs beside it
    // and it takes the whole budget. Only the forced cross-check still runs
    // the probed B side beside it.
    unsigned join_ways = opt.row_lists && fused_forced ? std::max(1u, budget > 1 ? budget - 1 : 1u)
                                                       : std::max(1u, budget);
    if (a_keys.size() < 1u << 14) join_ways = 1;  // too few keys to be worth splitting
    std::vector<Part> parts(join_ways);
    for (auto& part : parts) part.columns.resize(nc);

    auto a_range = [&](unsigned p) {
        // Built here, once per worker, and not outside the lambda: as a local its
        // references are known to the loop, where a captured one is loaded again
        // after every call the compiler cannot see through (about 1% of the
        // instructions on a 200k pair).
        const ProofRows proof{a, b, bi, plan, a_delim, width, key_size, nc};
        Part& out = parts[p];
        const std::size_t lo = a_keys.size() * p / join_ways;
        const std::size_t hi = a_keys.size() * (p + 1) / join_ways;
        Scratch s(width, proof.plan.runs());
        for (std::size_t at = lo; at < hi; ++at) {
            const int row = a_keys[at];
            if (at + RowIndex::kLookupPrefetch < hi)
                bi.prefetch(ai.hash_of(a_keys[at + RowIndex::kLookupPrefetch]));
            join_row(proof, out, s, ARow{ai.row_begin(row), ai.row_end(row), ai.hash_of(row)}, row,
                     at);
        }
    };

    auto a_side = [&] {
        std::vector<std::thread> workers;
        std::vector<std::exception_ptr> failures(join_ways);
        workers.reserve(join_ways - 1);
        auto guarded = [&](unsigned p) {
            try {
                a_range(p);
            } catch (...) {
                failures[p] = std::current_exception();
            }
        };
        for (unsigned p = 1; p < join_ways; ++p) {
            try {
                workers.emplace_back(guarded, p);
            } catch (const std::system_error&) {
                guarded(p);
            }
        }
        guarded(0);
        for (auto& w : workers) w.join();
        for (const auto& f : failures)
            if (f) std::rethrow_exception(f);
        whole.mark("join and compare");

        // Merged in range order, so the rows kept under the cap are the same
        // rows one thread would have kept.
        for (const Part& part : parts) {
            matched += part.matched;
            for (std::size_t i = 0; i < nc; ++i) {
                r.columns[i].changed += part.columns[i].changed;
                r.columns[i].blanked += part.columns[i].blanked;
                r.columns[i].filled += part.columns[i].filled;
            }
            for (const auto& [row, mate] : part.changed) changed.push(row, mate);
            changed.total += part.changed_total - static_cast<std::int64_t>(part.changed.size());
            for (const auto& [row, mate] : part.removed) removed.push(row, mate);
            removed.total += part.removed_total - static_cast<std::int64_t>(part.removed.size());
        }
    };
    // B's keys, over every core rather than one.
    //
    // This pass was the wall clock. The A side already ran on `join_ways`
    // threads while this one walked all of B on a single thread beside it, and
    // a throwaway build that skipped it entirely measured 1.29x of wall -- so it
    // was not a tail on the run, it was most of it. Every key here is
    // independent of every other, exactly as on the A side.
    //
    // Each part keeps its own added rows, and the parts are concatenated in
    // order afterwards. That matters: the report keeps the first `max_rows` of
    // them, and pushing from several threads into one list would make *which*
    // rows a truncated report shows depend on thread scheduling. `first_rows()`
    // is in first-occurrence order, each part takes a contiguous range of it,
    // so the concatenation is the order the single thread produced.
    const std::vector<int>& b_keys = bi.first_rows();
    const unsigned b_ways = b_keys.size() < (1u << 14) ? 1u : join_ways;
    std::vector<std::vector<std::pair<int, int>>> b_parts(b_ways);
    std::vector<std::int64_t> b_totals(b_ways, 0);
    auto b_range = [&](unsigned p) {
        std::vector<Field> fb(width), probe(width);
        const std::size_t lo = b_keys.size() * p / b_ways;
        const std::size_t hi = b_keys.size() * (p + 1) / b_ways;
        for (std::size_t at = lo; at < hi; ++at) {
            const int row = b_keys[at];
            if (at + RowIndex::kLookupPrefetch < hi)
                ai.prefetch(bi.hash_of(b_keys[at + RowIndex::kLookupPrefetch]));
            // Keys only: this pass asks whether B's key exists in A, and the
            // lookup compares key fields. The row's other nineteen columns are
            // parsed later, and only for the rows the report actually keeps.
            bi.keys_of(row, fb.data());
            if (ai.lookup(b, fb.data(), bi.hash_of(row), probe.data()) < 0) {
                ++b_totals[p];
                if (b_parts[p].size() <= opt.max_rows) b_parts[p].emplace_back(row, -1);
            }
        }
    };
    auto b_side = [&] {
        std::vector<std::thread> workers;
        std::vector<std::exception_ptr> failures(b_ways);
        workers.reserve(b_ways - 1);
        auto guarded = [&](unsigned p) {
            try {
                b_range(p);
            } catch (...) {
                failures[p] = std::current_exception();
            }
        };
        for (unsigned p = 1; p < b_ways; ++p) {
            try {
                workers.emplace_back(guarded, p);
            } catch (const std::system_error&) {
                guarded(p);  // no thread to be had: the same work, here
            }
        }
        guarded(0);
        for (auto& w : workers) w.join();
        for (const auto& f : failures)
            if (f) std::rethrow_exception(f);
        for (unsigned p = 0; p < b_ways; ++p) {
            added.total += b_totals[p];
            for (const auto& e : b_parts[p])
                if (added.held.size() <= added.cap) added.held.push_back(e);
        }
    };
    // `added` does not need a pass of its own to be *counted*.
    //
    // Every distinct key of A finds at most one distinct key of B, distinct
    // keys of A cannot find the same key of B, and the comparison behind the
    // lookup is symmetric -- so the number of B's keys with an A counterpart is
    // exactly the `matched` the A pass already produced, and `added` is B's
    // distinct keys minus it.
    //
    // What the pass is still for is the *sample*: this port has a report, and
    // its added section names rows. So it runs when something will print them
    // and not otherwise. Skipping it is worth 1.29x of wall -- it is a full
    // random-probed pass over a second table, and threading it changed nothing
    // because the machine was already busy with the A side. That is the whole
    // difference between work moved and work removed.
    struct Totals {
        std::int64_t matched = 0, changed = 0, removed = 0;
        std::vector<ColumnStat> columns;
    } fused_totals;
    fused_totals.columns.resize(nc);
    if (fused) {
        // A was joined as it was swept. What is left is to take back the rows
        // that repeat a key, joined again into parts of their own.
        const std::vector<int>& later = ai.later();
        const unsigned back_ways = later.size() < (1u << 14) ? 1u : join_ways;
        std::vector<Part> back(back_ways);
        for (auto& part : back) part.columns.resize(nc);
        std::vector<std::exception_ptr> failures(back_ways);
        auto take_back = [&](unsigned p) {
            try {
                const ProofRows proof{a, b, bi, plan, a_delim, width, key_size, nc};
                Scratch s(width, proof.plan.runs());
                const std::size_t lo = later.size() * p / back_ways;
                const std::size_t hi = later.size() * (p + 1) / back_ways;
                for (std::size_t at = lo; at < hi; ++at) {
                    const int row = later[at];
                    join_row(proof, back[p], s, ARow{ai.row_begin(row), ai.row_end(row), ai.hash_of(row)},
                             -1, at);
                }
            } catch (...) {
                failures[p] = std::current_exception();
            }
        };
        std::vector<std::thread> workers;
        for (unsigned p = 1; p < back_ways; ++p) {
            try {
                workers.emplace_back(take_back, p);
            } catch (const std::system_error&) {
                take_back(p);
            }
        }
        take_back(0);
        for (auto& w : workers) w.join();
        for (const auto& f : failures)
            if (f) std::rethrow_exception(f);
        whole.mark("repeated keys taken back");

        const auto fold = [&](const std::vector<Part>& from, std::int64_t sign) {
            for (const Part& part : from) {
                fused_totals.matched += sign * part.matched;
                fused_totals.changed += sign * part.changed_total;
                fused_totals.removed += sign * part.removed_total;
                for (std::size_t i = 0; i < nc; ++i) {
                    fused_totals.columns[i].changed += sign * part.columns[i].changed;
                    fused_totals.columns[i].blanked += sign * part.columns[i].blanked;
                    fused_totals.columns[i].filled += sign * part.columns[i].filled;
                }
            }
        };
        fold(fused_parts, 1);
        fold(back, -1);
        if (!opt.row_lists) {
            matched = fused_totals.matched;
            changed.total = fused_totals.changed;
            removed.total = fused_totals.removed;
            for (std::size_t i = 0; i < nc; ++i) {
                r.columns[i].changed = fused_totals.columns[i].changed;
                r.columns[i].blanked = fused_totals.columns[i].blanked;
                r.columns[i].filled = fused_totals.columns[i].filled;
            }
            added.total = bi.unique_keys() - matched;
        }
    }
    // The row lists from the join inside the sweep.
    //
    // Each part kept the first changed and removed rows of its own stretch of
    // A, as though each row were the first of its key. In part order, with the
    // rows that repeat a key dropped, that is the order the join after the
    // sweep keeps them in -- first occurrences, in file order -- so the first
    // `max_rows` of it are the same rows. Unless a part ran out: it stopped
    // keeping at `max_rows`, and if repeats it kept then leave the merged list
    // short while that part had more, the rows after its last one are not
    // here. Then the lists come from the join after the sweep, as before.
    //
    // The added rows are B's keys no row of A matched, in B's order: its
    // first rows whose bit is clear.
    Capped f_changed(opt.max_rows), f_added(opt.max_rows), f_removed(opt.max_rows);
    const auto added_from_seen = [&](Capped& into) {
        for (const int row : bi.first_rows()) {
            const auto bit = static_cast<std::size_t>(row);
            if (!(b_seen[bit >> 6].load(std::memory_order_relaxed) & (std::uint64_t{1} << (bit & 63))))
                into.push(row, -1);
        }
    };
    const auto fused_lists = [&]() -> bool {
        const std::vector<int>& later = ai.later();
        const std::vector<std::int64_t>& first = ai.part_first();
        const auto repeat = [&](std::int64_t row) {
            return std::binary_search(later.begin(), later.end(), static_cast<int>(row));
        };
        bool complete = true;
        const auto gather = [&](Capped& into, const std::vector<std::pair<int, int>>& list,
                                std::int64_t list_total, std::int64_t base) {
            for (const auto& [local, mate] : list) {
                const std::int64_t row = base + local;
                if (!repeat(row) && into.held.size() <= into.cap)
                    into.held.emplace_back(static_cast<int>(row), mate);
            }
            if (list_total > static_cast<std::int64_t>(list.size()) && into.held.size() <= into.cap)
                complete = false;
        };
        for (std::size_t p = 0; p < fused_parts.size(); ++p) {
            const Part& part = fused_parts[p];
            const std::int64_t base = p < first.size() ? first[p] : 0;
            gather(f_changed, part.changed, part.changed_total, base);
            gather(f_removed, part.removed, part.removed_total, base);
        }
        f_changed.total = fused_totals.changed;
        f_removed.total = fused_totals.removed;
        added_from_seen(f_added);
        return complete && f_added.total == bi.unique_keys() - fused_totals.matched;
    };
    const bool listed = fused && opt.row_lists && fused_lists();
    whole.mark("fused row lists");

    if (fused && !opt.row_lists) {
        // Counted above; nothing more to do.
    } else if (listed && !fused_forced) {
        matched = fused_totals.matched;
        for (std::size_t i = 0; i < nc; ++i) {
            r.columns[i].changed = fused_totals.columns[i].changed;
            r.columns[i].blanked = fused_totals.columns[i].blanked;
            r.columns[i].filled = fused_totals.columns[i].filled;
        }
        changed = std::move(f_changed);
        removed = std::move(f_removed);
        added = std::move(f_added);
    } else if (!opt.row_lists) {
        a_side();
        std::int64_t seen = 0;
        for (const Part& part : parts) seen += part.matched;
        added.total = bi.unique_keys() - seen;
    } else if (b_seen && !fused_forced) {
        a_side();
        added_from_seen(added);
        whole.mark("added rows");
    } else {
        std::exception_ptr failure;
        std::thread worker([&] {
            try {
                b_side();
            } catch (...) {
                failure = std::current_exception();
            }
        });
        try {
            a_side();
        } catch (...) {
            worker.join();
            throw;
        }
        worker.join();
        if (failure) std::rethrow_exception(failure);
    }
    // CSVDIFF_FUSED_JOIN=1 with --json: both joins ran, and they must agree.
    if (fused && opt.row_lists) {
        bool same = fused_totals.matched == matched && fused_totals.changed == changed.total &&
                    fused_totals.removed == removed.total;
        for (std::size_t i = 0; i < nc && same; ++i)
            same = fused_totals.columns[i].changed == r.columns[i].changed &&
                   fused_totals.columns[i].blanked == r.columns[i].blanked &&
                   fused_totals.columns[i].filled == r.columns[i].filled;
        // And with lists of its own, the same rows in them.
        if (listed && fused_forced)
            same = same && f_changed.held == changed.held && f_changed.total == changed.total &&
                   f_removed.held == removed.held && f_removed.total == removed.total &&
                   f_added.held == added.held && f_added.total == added.total;
        if (!same)
            throw Error("CSVDIFF_FUSED_JOIN: the join inside the sweep disagrees with the join after it");
    }

    // Only now does anything become a string, and only for the rows kept -- and
    // only when something is going to read them.
    //
    // `row_lists` is false exactly when no `--json` was given, and then the only
    // output is the summary line, which is counts. Everything from here to
    // `r.counts` builds the row samples: two `row_values` per kept row, each one
    // a `std::string` per cell, then three sorts over them, then the per-cell
    // diff, then both duplicate sections. At the default `--max-rows` of 50,000
    // that is about two million allocations for output nobody asked for.
    //
    // The B-side pass above is already gated on this flag for the same reason
    // and with the same comment in main.cpp -- this is the other half of it,
    // which was left running. Measured at 0.19s of a 1.68s four-thread run on a
    // 4M CSV pair: 11%, all of it in the serial tail after the join.
    //
    // What still has to be right without the lists: `r.counts`, which comes from
    // the join and the indexes and not from here, and the three `*_truncated`
    // flags, which come from `Capped::truncated()` and are counts too. The
    // duplicate-section flags stay false, which is what an uncollected section
    // is; nothing reads them unless `to_json` runs, and `to_json` runs only when
    // this branch did.
    if (opt.row_lists) {
        for (const auto& [row, mate] : removed.held) r.removed.push_back(row_values(a, ai, row, width, opt));
        for (const auto& [row, mate] : added.held) r.added.push_back(row_values(b, bi, row, width, opt));

        // A changed row reports its key and the cells that differ -- and on this
        // data that is 1.05 cells of nineteen columns. Materialising both whole
        // rows to find them made 1,900,000 `Val`s to emit 204,910: a factor of
        // 9.3, and 0.5s of a 1.9s run, which is where `--json` mode's time went.
        //
        // So the key is built for the sort, and the cells are found from the
        // Fields -- `cell_differs` reads bytes and allocates nothing -- and only
        // a cell that differs becomes a string.
        //
        // The rows are re-parsed in the second pass rather than carried through
        // the sort. Parsing a row is field arithmetic over bytes already mapped;
        // holding the Fields instead would be 16 MB moved by the sort to save
        // that, and it is the allocations that cost, not the parse.
        //
        // `cell_differs` is the predicate the join itself uses, so the report now
        // names cells by the same rule that decided the row was changed. The
        // `Val`-level test here before it said the same thing by a different
        // route; agreeing with the join by construction is the better of the two.
        struct KeptChange {
            std::vector<Val> key;
            int row, mate;
        };
        // Both passes over the kept rows -- their keys for the sort, then their
        // differing cells -- run on the whole thread budget, each worker over a
        // contiguous range writing only its own slots. They were serial: at 4M
        // rows 0.14s, on its own, after the join, for the 50,000 rows the report
        // keeps by default. A range's failure (the --ignore-case refusal) is
        // rethrown in range order, so it is the one the serial loop met first.
        const auto over_kept = [&](std::size_t n, const auto& body) {
            const unsigned ways = n < (std::size_t{1} << 12) ? 1u : budget;
            std::vector<std::exception_ptr> failures(ways);
            const auto range = [&](unsigned p) {
                try {
                    body(n * p / ways, n * (p + 1) / ways);
                } catch (...) {
                    failures[p] = std::current_exception();
                }
            };
            std::vector<std::thread> workers;
            workers.reserve(ways - 1);
            for (unsigned p = 1; p < ways; ++p) {
                try {
                    workers.emplace_back(range, p);
                } catch (const std::system_error&) {
                    range(p);
                }
            }
            range(0);
            for (auto& w : workers) w.join();
            for (const auto& f : failures)
                if (f) std::rethrow_exception(f);
        };

        std::vector<KeptChange> kept(changed.held.size());
        over_kept(kept.size(), [&](std::size_t lo, std::size_t hi) {
            for (std::size_t i = lo; i < hi; ++i) {
                const auto [row, mate] = changed.held[i];
                kept[i] = {key_values(a, ai, row, width, key_size, opt), row, mate};
            }
        });

        const auto by_key = [&](const std::vector<Val>& x, const std::vector<Val>& y) {
            return compare_keys(x, y, key_size) < 0;
        };
        std::stable_sort(r.removed.begin(), r.removed.end(), by_key);
        std::stable_sort(r.added.begin(), r.added.end(), by_key);
        std::stable_sort(kept.begin(), kept.end(),
                         [&](const KeptChange& x, const KeptChange& y) { return by_key(x.key, y.key); });

        r.changed.resize(kept.size());
        over_kept(kept.size(), [&](std::size_t lo, std::size_t hi) {
            std::vector<Field> fa(width), fb(width);
            for (std::size_t at = lo; at < hi; ++at) {
                KeptChange& k = kept[at];
                ChangedRow& out = r.changed[at];
                out.key = std::move(k.key);
                ai.fields_of(k.row, fa.data());
                bi.fields_of(k.mate, fb.data());
                for (std::size_t i = 0; i < nc; ++i) {
                    const Field x = fa[key_size + i], y = fb[key_size + i];
                    if (cell_differs(a, x, b, y, opt))
                        out.cells.push_back(
                            {i, value_of(a, x, opt, Fold::display), value_of(b, y, opt, Fold::display)});
                }
            }
        });

        const auto dup_section = [&](const Slab& s, const RowIndex& idx, std::vector<DupRow>& out,
                                     bool& truncated) {
            std::vector<DupRow> all;
            const auto& firsts = idx.first_rows();
            const auto& counts = idx.occurrences();
            for (std::size_t i = 0; i < firsts.size(); ++i) {
                if (counts[i] < 2) continue;
                all.push_back({key_values(s, idx, firsts[i], width, key_size, opt),
                               static_cast<std::int64_t>(counts[i])});
            }
            std::stable_sort(all.begin(), all.end(), [&](const DupRow& x, const DupRow& y) {
                if (x.count != y.count) return x.count > y.count;
                return compare_keys(x.key, y.key, key_size) < 0;
            });
            truncated = all.size() > opt.max_rows;
            all.resize(std::min(all.size(), opt.max_rows));
            out = std::move(all);
        };
        dup_section(a, ai, r.dup_a, r.dup_a_truncated);
        dup_section(b, bi, r.dup_b, r.dup_b_truncated);
    }  // opt.row_lists

    r.counts = {ai.rows(),      bi.rows(),        ai.unique_keys(), bi.unique_keys(),
                matched,        matched - changed.total, changed.total,
                added.total,    removed.total,    ai.dup_keys(),    ai.dup_rows(),
                bi.dup_keys(),  bi.dup_rows()};
    r.changed_truncated = changed.truncated();
    r.added_truncated = added.truncated();
    r.removed_truncated = removed.truncated();
    r.changed.resize(std::min(r.changed.size(), opt.max_rows));
    r.added.resize(std::min(r.added.size(), opt.max_rows));
    r.removed.resize(std::min(r.removed.size(), opt.max_rows));

    const std::chrono::duration<double> elapsed = std::chrono::steady_clock::now() - started;
    whole.mark("assemble");
    r.seconds = std::round(elapsed.count() * 1000.0) / 1000.0;
    return r;
}

}  // namespace csvdiff
