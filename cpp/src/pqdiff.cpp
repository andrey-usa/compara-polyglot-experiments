#include "pqdiff.hpp"

#include <fcntl.h>
#include "win32.hpp"   // O_BINARY off Windows; the mmap shim on it
#ifndef _WIN32
#include <sys/mman.h>
#endif
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cctype>
#include <charconv>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#if defined(CSVDIFF_SCAN_AVX2) || defined(CSVDIFF_SCAN_AVX512)
#include <immintrin.h>
#endif
#include <exception>
#include <string_view>
#include <thread>
#include <utility>

#include "parquet.hpp"

namespace csvdiff {
namespace {

// ---------------------------------------------------------------------------
// The mapping
// ---------------------------------------------------------------------------

// Read-only and whole-file, like the CSV path's Slab, but without the dialect:
// a Parquet file says what it is in its own footer.
class Map {
  public:
    explicit Map(const std::string& path) {
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
            // Columns are read one at a time and each is contiguous, so the
            // access pattern is a series of sequential runs rather than one.
            ::madvise(const_cast<void*>(p), size_, MADV_WILLNEED);
        }
    }
    ~Map() {
        if (data_) ::munmap(const_cast<char*>(data_), size_);
        if (fd_ >= 0) ::close(fd_);
    }
    Map(const Map&) = delete;
    Map& operator=(const Map&) = delete;

    const char* data() const { return data_; }
    std::size_t size() const { return size_; }

  private:
    int fd_ = -1;
    const char* data_ = nullptr;
    std::size_t size_ = 0;
};

// ---------------------------------------------------------------------------
// Cells: the same value semantics as the CSV path, on a plain byte span
// ---------------------------------------------------------------------------

// Parquet values carry no escaping -- a byte array is its own bytes -- so where
// the CSV path has to read a field through `for_each_byte` to drop doubled
// quotes, here a cell is already a span. Everything below is the same rule set
// as csvdiff.cpp's value layer, stated over a span instead of a Field. The two
// must agree exactly: cpp/test.sh compares the JSON from a CSV pair against the
// JSON from the same pair converted to Parquet, and any drift shows up there.
struct Cell {
    const char* p = nullptr;
    std::uint32_t n = 0;
    bool null = true;

    std::string_view view() const { return {p, n}; }
};

bool needs_normalising(const Options& o) {
    return o.trim || o.ignore_case || o.empty_is_null || o.tolerance > 0.0;
}

std::string_view trimmed(std::string_view s) {
    const auto space = [](char c) { return static_cast<unsigned char>(c) <= ' '; };
    while (!s.empty() && space(s.front())) s.remove_prefix(1);
    while (!s.empty() && space(s.back())) s.remove_suffix(1);
    return s;
}

bool ascii(std::string_view s) {
    return std::none_of(s.begin(), s.end(), [](unsigned char c) { return c >= 0x80; });
}

// As csvdiff.cpp's: `compare` refuses a value outside ASCII under --ignore-case,
// `display` shows it unfolded, for a value that decides nothing.
enum class Fold { compare, display };

Val value_of(Cell c, const Options& o, Fold fold = Fold::compare) {
    if (c.null) return std::nullopt;
    std::string text(c.view());
    if (text.empty()) return std::nullopt;
    if (o.trim) text = std::string(trimmed(text));
    if (o.ignore_case) {
        const bool plain = ascii(text);
        if (!plain && fold == Fold::compare)
            throw Error(
                "--ignore-case on a field outside ASCII needs Unicode case folding, which "
                "this port does not carry; use another implementation for that data");
        if (plain)
            std::transform(text.begin(), text.end(), text.begin(), [](unsigned char ch) {
                return static_cast<char>(std::tolower(ch));
            });
    }
    if (o.empty_is_null && text.empty()) return std::nullopt;
    return text;
}

// The two per-cell questions, split so the common case inlines. Without
// --trim, --ignore-case, --empty-is-null or --tolerance each is a null check, a
// length and a memcmp; the normalised form builds a string and belongs out of
// line. As one function each, neither inlined, and the call was most of the
// cost -- 1.18B instructions of a 2M-row Parquet run, once per key cell in the
// index and the join and once per compared cell outside the dictionary path.
// Absence never depends on case, so this does not fold (see csvdiff.cpp).
[[gnu::noinline]] bool absent_normalised(Cell c, const Options& o) {
    if (c.null || c.n == 0) return true;
    return o.empty_is_null && o.trim && trimmed(c.view()).empty();
}

[[gnu::noinline]] bool same_normalised(Cell x, Cell y, const Options& o) {
    return value_of(x, o) == value_of(y, o);
}

[[gnu::always_inline]] inline bool absent(Cell c, const Options& o) {
    if (c.null || c.n == 0) return true;
    return needs_normalising(o) && absent_normalised(c, o);
}

[[gnu::always_inline]] inline bool same(Cell x, Cell y, const Options& o) {
    const bool xa = absent(x, o), ya = absent(y, o);
    if (xa || ya) return xa && ya;
    // Identical bytes normalise identically; asked before the fold, so an
    // unchanged value outside ASCII is not a reason to refuse.
    if (x.n == y.n && std::memcmp(x.p, y.p, x.n) == 0) return true;
    return needs_normalising(o) && same_normalised(x, y, o);
}

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

bool cell_differs(Cell x, Cell y, const Options& o) {
    const bool xa = absent(x, o), ya = absent(y, o);
    if (xa && ya) return false;
    if (o.tolerance > 0.0 && !xa && !ya) {
        const auto nx = as_number(x.view()), ny = as_number(y.view());
        if (nx && ny) return std::fabs(*nx - *ny) > o.tolerance;
    }
    return !same(x, y, o);
}

constexpr std::uint64_t kPrime = 0x100000001b3ULL;
constexpr std::uint64_t kSeed = 0xcbf29ce484222325ULL;

// FNV-1a, eight bytes at a time.
//
// A hash here is internal in exactly the sense the CSV engine's is: nothing
// outside this file sees one, so the only property it owes anyone is that the
// two places that compute it agree. That is what lets it read a word at a time
// -- see `hash_bytes` in csvdiff.cpp, which is the same function for the same
// reason.
//
// It matters more here than the call sites suggest. Interning a dictionary
// costs one call per *distinct* value and is nothing; but a key column that is
// not dictionary-encoded -- which a high-cardinality key column usually is not,
// since a writer gives up on a dictionary that never repeats -- is hashed once
// per row, on both sides, indexed and probed. At ten million rows a
// twenty-six-byte key is four multiplies instead of twenty-six.
std::uint64_t fold_bytes(std::uint64_t h, std::string_view v) {
    const char* p = v.data();
    std::size_t n = v.size();
    while (n >= 8) {
        std::uint64_t word;
        std::memcpy(&word, p, sizeof word);
        h = (h ^ word) * kPrime;
        h ^= h >> 29;  // the xor-shift is what spreads a whole word into the low bits
        p += 8;
        n -= 8;
    }
    if (n > 0) {
        std::uint64_t tail = 0;
        std::memcpy(&tail, p, n);
        h = (h ^ tail) * kPrime;
        h ^= h >> 29;
    }
    return (h ^ v.size()) * kPrime;
}
std::uint64_t fold_absent(std::uint64_t h) { return (h ^ 0x9e3779b97f4a7c15ULL) * kPrime; }
std::uint64_t fold_id(std::uint64_t h, std::int32_t id) {
    return (h ^ static_cast<std::uint64_t>(static_cast<std::uint32_t>(id))) * kPrime;
}

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
// A column, as the comparison sees it
// ---------------------------------------------------------------------------

// `parquet::Column` hands back offsets; this resolves them against whichever
// buffer they belong to and answers "what is in row r".
class Col {
  public:
    Col() = default;
    Col(parquet::Column c, const char* mapping) : c_(std::move(c)) {
        base_ = c_.owned.empty() ? mapping : c_.owned.data();
    }

    bool dictionary() const { return c_.dictionary; }
    std::size_t rows() const { return c_.rows(); }
    const std::vector<std::int32_t>& index() const { return c_.index; }
    std::size_t dict_size() const { return c_.dict.size(); }

    Cell dict_cell(std::size_t k) const { return of(c_.dict[k]); }

    Cell at(std::size_t row) const {
        if (c_.dictionary) {
            const std::int32_t k = c_.index[row];
            if (k < 0) return {};
            return of(c_.dict[static_cast<std::size_t>(k)]);
        }
        return of(c_.values[row]);
    }

  private:
    Cell of(parquet::Slice s) const {
        if (s.null()) return {};
        return Cell{base_ + s.offset(), s.length(), false};
    }

    parquet::Column c_;
    const char* base_ = nullptr;
};

// ---------------------------------------------------------------------------
// One id space for two dictionaries
// ---------------------------------------------------------------------------

// The trick the whole columnar path turns on. Two files' dictionaries are
// interned into one dense id space, which costs one hash per *distinct* value
// rather than one per row; after that, two cells are equal exactly when their
// ids are, and a column diff is a comparison of two int32 arrays.
class Ids {
  public:
    explicit Ids(std::size_t expect) {
        std::size_t cap = 16;
        while (cap < expect * 2 + 16) cap <<= 1;
        slot_.assign(cap, -1);
        mask_ = cap - 1;
    }

    std::int32_t of(std::string_view v) {
        const std::uint64_t h = fold_bytes(kSeed, v);
        std::size_t at = h & mask_;
        for (;;) {
            const std::int32_t s = slot_[at];
            if (s < 0) break;
            const std::size_t k = static_cast<std::size_t>(s);
            if (hash_[k] == h && val_[k] == v) return s;
            at = (at + 1) & mask_;
        }
        const std::int32_t id = static_cast<std::int32_t>(val_.size());
        val_.push_back(v);
        hash_.push_back(h);
        slot_[at] = id;
        return id;
    }

  private:
    std::vector<std::int32_t> slot_;
    std::vector<std::string_view> val_;
    std::vector<std::uint64_t> hash_;
    std::uint64_t mask_ = 0;
};

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

// One file's key columns. A key column both sides store as a dictionary is
// reduced to a shared id per row, and from there hashing and equality are
// integer work; anything else stays bytes and is compared as bytes.
struct KeySide {
    std::vector<Col> col;                        // one per key column
    std::vector<std::vector<std::int32_t>> id;   // filled where `Keys::as_id`
    std::size_t rows = 0;
};

struct Keys {
    std::vector<char> as_id;   // per key column, shared by both sides
    KeySide a, b;
    std::size_t n() const { return as_id.size(); }
};

std::uint64_t row_hash(const Keys& k, const KeySide& s, std::size_t row, const Options& o) {
    std::uint64_t h = kSeed;
    for (std::size_t j = 0; j < k.n(); ++j) {
        if (k.as_id[j]) {
            h = fold_id(h, s.id[j][row]);
            continue;
        }
        const Cell c = s.col[j].at(row);
        if (absent(c, o)) {
            h = fold_absent(h);
        } else if (!needs_normalising(o)) {
            h = fold_bytes(h, c.view());
        } else {
            h = fold_bytes(h, *value_of(c, o));
        }
    }
    return h;
}

bool row_eq(const Keys& k, const KeySide& x, std::size_t rx, const KeySide& y, std::size_t ry,
            const Options& o) {
    for (std::size_t j = 0; j < k.n(); ++j) {
        if (k.as_id[j]) {
            if (x.id[j][rx] != y.id[j][ry]) return false;
        } else if (!same(x.col[j].at(rx), y.col[j].at(ry), o)) {
            return false;
        }
    }
    return true;
}

// An open-addressed table over one file's distinct keys, first occurrence wins.
//
// A slot is one word: the top twenty-four bits of the key's hash, and the
// position in `firsts` plus one, with zero meaning empty. Carrying the hash
// *inside* the slot is the point -- a probe that misses is settled by the word
// it already loaded, where a table of bare positions would have to follow each
// one into a separate array of hashes and take a second cache miss to reject
// it. At ten million keys those second misses were the join.
//
// How many rows ahead to start the load for. Measured in the C port, not
// reasoned: at ten million rows the index build is flat within noise from 8 to
// 64, but the join is not -- best at 8 to 24, decaying past 32, because a probe
// touches a second table and reaching too far ahead evicts what the current one
// is still using. The same 24 is used here so the two ports differ in language
// and not in tuning.
inline constexpr std::size_t kPrefetchAhead = 24;

struct Index {
    std::vector<std::uint32_t> slots;
    std::uint64_t mask = 0;
    unsigned pos_bits = 1;
    std::uint32_t pos_mask = 1;
    std::vector<std::int32_t> firsts;
    std::vector<std::uint32_t> counts;
    std::vector<std::uint64_t> hashes;   // per distinct key, for probing the other side
    std::int64_t rows = 0, dup_keys = 0, dup_rows = 0;

    void set_pos_bits(std::size_t n) {
        pos_bits = 1;
        while (pos_bits < 32 && (n + 1) > ((std::size_t{1} << pos_bits) - 1)) ++pos_bits;
        pos_mask = pos_bits >= 32 ? 0xFFFFFFFFu
                                  : static_cast<std::uint32_t>((1u << pos_bits) - 1);
    }
    std::uint32_t slot_pack(std::uint64_t hash, std::size_t pos) const {
        const unsigned tag_bits = 32u - pos_bits;
        const std::uint32_t tag =
            tag_bits ? static_cast<std::uint32_t>(hash >> (64u - tag_bits)) : 0u;
        return (tag << pos_bits) | static_cast<std::uint32_t>(pos + 1);
    }
    bool slot_tag_is(std::uint32_t v, std::uint64_t hash) const {
        const unsigned tag_bits = 32u - pos_bits;
        if (!tag_bits) return true;
        return (v >> pos_bits) == static_cast<std::uint32_t>(hash >> (64u - tag_bits));
    }
    std::size_t slot_pos(std::uint32_t v) const {
        return static_cast<std::size_t>(v & pos_mask) - 1;
    }

    std::int64_t unique() const { return static_cast<std::int64_t>(firsts.size()); }
};

// Hashes are computed in parallel because a row's hash depends on nothing but
// that row; insertion is serial because first-occurrence-wins depends on the
// order rows arrive, and threading it would make the answer depend on the
// scheduler. Same split as the CSV path.
Index build_index(const Keys& k, const KeySide& s, const Options& o, unsigned threads) {
    Index ix;
    ix.rows = static_cast<std::int64_t>(s.rows);

    std::vector<std::uint64_t> hs(s.rows);
    const unsigned ways = std::max(1u, s.rows < (1u << 15) ? 1u : threads);
    std::vector<std::exception_ptr> failures(ways);
    auto sweep = [&](unsigned p) {
        try {
            const std::size_t lo = s.rows * p / ways, hi = s.rows * (p + 1) / ways;
            for (std::size_t r = lo; r < hi; ++r) hs[r] = row_hash(k, s, r, o);
        } catch (...) {
            failures[p] = std::current_exception();
        }
    };
    {
        std::vector<std::thread> workers;
        workers.reserve(ways - 1);
        for (unsigned p = 1; p < ways; ++p) {
            try {
                workers.emplace_back(sweep, p);
            } catch (const std::system_error&) {
                sweep(p);
            }
        }
        sweep(0);
        for (auto& w : workers) w.join();
    }
    for (const auto& f : failures)
        if (f) std::rethrow_exception(f);

    // Sized to about a two-thirds load: linear probing is still short there,
    // and a smaller table is a smaller working set, which is what this phase is
    // actually limited by.
    std::size_t cap = 1 << 12;
    while (cap * 2 < s.rows * 3 + 16) cap <<= 1;
    ix.slots.assign(cap, 0);
    ix.mask = cap - 1;
    ix.set_pos_bits(s.rows ? s.rows : 1);
    ix.firsts.reserve(s.rows);
    ix.counts.reserve(s.rows);
    ix.hashes.reserve(s.rows);

    for (std::size_t r = 0; r < s.rows; ++r) {
        // The slot this row will land in is a random word of a table far larger
        // than the cache, and the hash that picks it is already in hand rows
        // ahead of where it is needed.
        if (r + kPrefetchAhead < s.rows)
            __builtin_prefetch(&ix.slots[hs[r + kPrefetchAhead] & ix.mask], 1, 0);
        const std::uint64_t h = hs[r];
        std::size_t at = h & ix.mask;
        for (;;) {
            const std::uint32_t slot = ix.slots[at];
            if (slot == 0) {
                ix.slots[at] = ix.slot_pack(h, ix.firsts.size());
                ix.firsts.push_back(static_cast<std::int32_t>(r));
                ix.counts.push_back(1);
                ix.hashes.push_back(h);
                break;
            }
            if (ix.slot_tag_is(slot, h)) {
                const std::size_t pos = ix.slot_pos(slot);
                if (row_eq(k, s, static_cast<std::size_t>(ix.firsts[pos]), s, r, o)) {
                    if (++ix.counts[pos] == 2) {
                        ++ix.dup_keys;
                        ++ix.dup_rows;  // the first occurrence counts once the key repeats
                    }
                    ++ix.dup_rows;
                    break;
                }
            }
            at = (at + 1) & ix.mask;
        }
    }
    return ix;
}

// Looks one side's row up in the other side's table.
std::int32_t lookup(const Keys& k, const Index& into, const KeySide& there, const KeySide& here,
                    std::size_t row, std::uint64_t h, const Options& o) {
    std::size_t at = h & into.mask;
    for (;;) {
        const std::uint32_t slot = into.slots[at];
        if (slot == 0) return -1;
        if (into.slot_tag_is(slot, h)) {
            const std::int32_t first = into.firsts[into.slot_pos(slot)];
            if (row_eq(k, there, static_cast<std::size_t>(first), here, row, o)) return first;
        }
        at = (at + 1) & into.mask;
    }
}

// ---------------------------------------------------------------------------
// Row lists, capped but exactly counted
// ---------------------------------------------------------------------------

struct Capped {
    std::vector<std::int32_t> held;
    std::size_t cap;
    std::int64_t total = 0;

    explicit Capped(std::size_t c) : cap(c) {}
    void push(std::int32_t row) {
        ++total;
        if (held.size() <= cap) held.push_back(row);  // one past, to detect truncation
    }
    bool truncated() const { return total > static_cast<std::int64_t>(cap); }
};

// ---------------------------------------------------------------------------
// What one compared column produced
// ---------------------------------------------------------------------------

struct ColOut {
    std::int64_t changed = 0, blanked = 0, filled = 0;
    std::vector<std::uint64_t> bits;   // one per matched pair
    // The first cap+1 pairs where this column differs, with their two values.
    // Every pair that reaches the report is among these -- a row only reaches
    // the report by being one of the first cap+1 *changed* pairs, and the pairs
    // where this column differs are a subset of those, in the same order.
    std::vector<std::pair<std::size_t, std::pair<Val, Val>>> held;
    std::vector<Val> added_vals, removed_vals;
};

constexpr std::size_t kBlock = 4096;

// The vectorised core. `xa` and `xb` hold one block of shared ids, gathered
// through each side's dictionary; the compiler turns the comparison below into
// packed int32 compares, and the mismatch mask is then read a register at a
// time -- the same scanner the CSV path uses to find a delimiter, applied to
// finding a changed cell, and built the same three ways so the same question
// can be asked of it.
//
// The arithmetic that makes this worth doing: about 0.7% of cells differ per
// column, so an eight-byte word is all-zero 94.6% of the time, a 32-byte vector
// 80%, a 64-byte one 64%. Every one of those is a correctly predicted
// not-taken branch and eight, thirty-two or sixty-four cells retired. The work
// on a *hit* is identical in all three -- one `ctz` per changed cell -- so the
// only thing a wider register buys is fewer trips round the skip path, which is
// where nearly all the loop is. That is the opposite trade from the CSV
// scanner, where a wider register also means fewer loads, and it is why both
// are measured rather than assumed.
template <typename Fn>
void scan_mask(const std::uint8_t* neq, std::size_t m, std::size_t base, Fn&& hit) {
    std::size_t i = 0;
#if defined(CSVDIFF_SCAN_AVX512)
    for (; i + 64 <= m; i += 64) {
        const __m512i v = _mm512_loadu_si512(neq + i);
        std::uint64_t bits = _mm512_test_epi8_mask(v, v);
        while (bits) {
            hit(base + i + static_cast<unsigned>(__builtin_ctzll(bits)));
            bits &= bits - 1;
        }
    }
#elif defined(CSVDIFF_SCAN_AVX2)
    const __m256i zero = _mm256_setzero_si256();
    for (; i + 32 <= m; i += 32) {
        const __m256i v = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(neq + i));
        std::uint32_t bits = static_cast<std::uint32_t>(
            ~_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, zero)));
        while (bits) {
            hit(base + i + static_cast<unsigned>(__builtin_ctz(bits)));
            bits &= bits - 1;
        }
    }
#endif
    for (; i + 8 <= m; i += 8) {
        std::uint64_t w;
        std::memcpy(&w, neq + i, 8);
        while (w) {
            const unsigned byte = static_cast<unsigned>(__builtin_ctzll(w)) >> 3;
            hit(base + i + byte);
            w &= ~(0xFFULL << (byte * 8));
        }
    }
    for (; i < m; ++i)
        if (neq[i]) hit(base + i);
}

// Phase timings, on stderr, when CSVDIFF_PHASES is set. A columnar comparison
// has three costs that move independently -- getting the key columns out of the
// file, joining on them, and walking the compared columns -- and knowing which
// one grew is the difference between tuning and guessing.
class Phases {
  public:
    Phases() : on_(std::getenv("CSVDIFF_PHASES") != nullptr), last_(clock::now()) {}
    void mark(const char* what) {
        if (!on_) return;
        const auto now = clock::now();
        std::fprintf(stderr, "  %-22s %7.3fs\n", what,
                     std::chrono::duration<double>(now - last_).count());
        last_ = now;
    }

  private:
    using clock = std::chrono::steady_clock;
    bool on_;
    clock::time_point last_;
};

}  // namespace

bool is_parquet(const std::string& path) {
    const int fd = ::open(path.c_str(), O_RDONLY | O_BINARY);
    if (fd < 0) return false;
    char magic[4] = {0, 0, 0, 0};
    const ssize_t got = ::read(fd, magic, 4);
    ::close(fd);
    return got == 4 && std::memcmp(magic, "PAR1", 4) == 0;
}

Result compare_parquet(const std::string& a_path, const std::string& b_path, const Options& opt) {
    const auto started = std::chrono::steady_clock::now();
    if (opt.key.empty()) throw Error("at least one key column is required");

    Phases phase;
    Map am(a_path), bm(b_path);
    const parquet::Meta ameta = parquet::read_meta(am.data(), am.size(), a_path);
    const parquet::Meta bmeta = parquet::read_meta(bm.data(), bm.size(), b_path);

    const auto has = [](const std::vector<std::string>& v, const std::string& n) {
        return std::find(v.begin(), v.end(), n) != v.end();
    };
    for (const auto& k : opt.key)
        if (!has(ameta.names, k) || !has(bmeta.names, k))
            throw Error("key column(s) missing from one of the files: " + k);
    // The text path's rule, which this path had not carried: a name in neither
    // file is a misspelling, and a misspelled --ignore silently compares the
    // column it meant to drop and calls it changed on every row.
    for (const auto& c : opt.ignore)
        if (!has(ameta.names, c) && !has(bmeta.names, c))
            throw Error("ignore column(s) present in neither file: " + c);

    Result r;
    r.key = opt.key;
    for (const auto& c : ameta.names)
        if (!has(bmeta.names, c) && !has(opt.key, c)) r.only_in_a.push_back(c);
    for (const auto& c : bmeta.names)
        if (!has(ameta.names, c) && !has(opt.key, c)) r.only_in_b.push_back(c);
    if (!opt.compare.empty()) {
        for (const auto& c : opt.compare) {
            if (!has(ameta.names, c) || !has(bmeta.names, c))
                throw Error("compared column missing from one of the files: " + c);
            if (has(opt.key, c) || has(opt.ignore, c)) continue;  // as on the text path
            r.compared.push_back(c);
        }
    } else {
        for (const auto& c : ameta.names)
            if (has(bmeta.names, c) && !has(opt.key, c) && !has(opt.ignore, c))
                r.compared.push_back(c);
    }
    r.a_cols = ameta.names.size();
    r.b_cols = bmeta.names.size();

    const std::size_t key_size = opt.key.size();
    const std::size_t nc = r.compared.size();
    r.columns.resize(nc);
    for (std::size_t i = 0; i < nc; ++i) r.columns[i].name = r.compared[i];

    const auto slot_of = [&](const std::vector<std::string>& names, const std::string& n) {
        return static_cast<std::size_t>(
            std::find(names.begin(), names.end(), n) - names.begin());
    };

    unsigned budget = opt.threads;
    if (budget == 0) budget = std::max(1u, std::thread::hardware_concurrency());

    // --- keys -------------------------------------------------------------
    //
    // Both files' key columns are read at once, then any column both sides
    // store as a dictionary is reduced to one shared id per row.
    Keys keys;
    keys.as_id.assign(key_size, 0);
    keys.a.col.resize(key_size);
    keys.b.col.resize(key_size);
    keys.a.id.resize(key_size);
    keys.b.id.resize(key_size);
    {
        std::exception_ptr failure;
        std::thread worker([&] {
            try {
                for (std::size_t j = 0; j < key_size; ++j)
                    keys.b.col[j] = Col(parquet::read_column(bm.data(), bm.size(),
                                                             slot_of(bmeta.names, opt.key[j]),
                                                             b_path),
                                        bm.data());
            } catch (...) {
                failure = std::current_exception();
            }
        });
        try {
            for (std::size_t j = 0; j < key_size; ++j)
                keys.a.col[j] = Col(parquet::read_column(am.data(), am.size(),
                                                         slot_of(ameta.names, opt.key[j]), a_path),
                                    am.data());
        } catch (...) {
            worker.join();
            throw;
        }
        worker.join();
        if (failure) std::rethrow_exception(failure);
    }
    phase.mark("read key columns");
    keys.a.rows = key_size ? keys.a.col[0].rows() : 0;
    keys.b.rows = key_size ? keys.b.col[0].rows() : 0;
    for (std::size_t j = 1; j < key_size; ++j) {
        if (keys.a.col[j].rows() != keys.a.rows || keys.b.col[j].rows() != keys.b.rows)
            throw Error("parquet columns disagree about how many rows the file has");
    }

    for (std::size_t j = 0; j < key_size; ++j) {
        if (!keys.a.col[j].dictionary() || !keys.b.col[j].dictionary()) continue;
        keys.as_id[j] = 1;
        Ids ids(keys.a.col[j].dict_size() + keys.b.col[j].dict_size());
        std::vector<std::string> owned;
        if (needs_normalising(opt))
            owned.reserve(keys.a.col[j].dict_size() + keys.b.col[j].dict_size());
        const auto code = [&](const Col& c) {
            std::vector<std::int32_t> out(c.dict_size());
            for (std::size_t k = 0; k < c.dict_size(); ++k) {
                const Cell cell = c.dict_cell(k);
                if (absent(cell, opt)) {
                    out[k] = -1;
                    continue;
                }
                if (!needs_normalising(opt)) {
                    out[k] = ids.of(cell.view());
                } else {
                    owned.push_back(*value_of(cell, opt));
                    out[k] = ids.of(owned.back());
                }
            }
            return out;
        };
        const std::vector<std::int32_t> a_code = code(keys.a.col[j]);
        const std::vector<std::int32_t> b_code = code(keys.b.col[j]);
        const auto apply = [](const Col& c, const std::vector<std::int32_t>& map) {
            const std::vector<std::int32_t>& ix = c.index();
            std::vector<std::int32_t> out(ix.size());
            for (std::size_t i = 0; i < ix.size(); ++i)
                out[i] = ix[i] < 0 ? -1 : map[static_cast<std::size_t>(ix[i])];
            return out;
        };
        keys.a.id[j] = apply(keys.a.col[j], a_code);
        keys.b.id[j] = apply(keys.b.col[j], b_code);
    }

    phase.mark("code key dictionaries");

    // --- the join ---------------------------------------------------------
    Index ai, bi;
    {
        std::exception_ptr failure;
        std::thread worker([&] {
            try {
                bi = build_index(keys, keys.b, opt, std::max(1u, budget / 2));
            } catch (...) {
                failure = std::current_exception();
            }
        });
        try {
            ai = build_index(keys, keys.a, opt, std::max(1u, budget / 2));
        } catch (...) {
            worker.join();
            throw;
        }
        worker.join();
        if (failure) std::rethrow_exception(failure);
    }

    phase.mark("build key indexes");

    // Both directions of the join split over contiguous ranges of one side's
    // distinct keys. Each range accumulates into its own lists, and the ranges
    // merge in order, so which rows survive the report cap is the same as one
    // thread would have kept. The A direction is the longer of the two -- it
    // produces the pair list the whole column pass then walks -- but B's is not
    // free either, and leaving it on one thread made it the tail of the phase.
    struct Part {
        std::vector<std::int32_t> pa, pb;
        std::vector<std::int32_t> held;
        std::int64_t total = 0;
    };
    const auto split = [&](std::size_t keys_here) {
        unsigned w = std::max(1u, budget);
        if (keys_here < (1u << 14)) w = 1;
        return w;
    };
    const unsigned ways = split(ai.firsts.size());
    // `added` does not need a pass of its own to be *counted*. Every distinct
    // key of A finds at most one distinct key of B, distinct keys of A cannot
    // find the same key of B, and the comparison behind the lookup is
    // symmetric -- so the number of B's keys with an A counterpart is exactly
    // the pair count the A pass produces, and `added` is B's distinct keys
    // minus it.
    //
    // What the pass is still for is the *sample*: the report's added section
    // names rows. So it runs when something will print them and not otherwise,
    // which is the rule `csvdiff.cpp` has followed since it was measured there.
    // This path never asked -- it walked B whether or not anything would read
    // the result, which is a full random-probed pass over a second table.
    const unsigned b_ways = opt.row_lists ? split(bi.firsts.size()) : 0u;
    std::vector<Part> parts(ways), b_parts(b_ways);
    std::vector<std::exception_ptr> failures(ways + b_ways);

    // Each range builds its Part locally and moves it into place once. The parts
    // sit side by side in one vector, and a push_back writes the vector's end
    // pointer on every row: filled in place, several threads' end pointers
    // share a cache line and it bounces between their cores once a row.
    auto a_range = [&](unsigned p) {
        Part out;
        const std::size_t lo = ai.firsts.size() * p / ways;
        const std::size_t hi = ai.firsts.size() * (p + 1) / ways;
        out.pa.reserve(hi - lo);
        out.pb.reserve(hi - lo);
        for (std::size_t at = lo; at < hi; ++at) {
            // The same stall as the insert above, in the other side's table:
            // the hash this row will probe with is already in hand.
            if (at + kPrefetchAhead < hi)
                __builtin_prefetch(&bi.slots[ai.hashes[at + kPrefetchAhead] & bi.mask], 0, 0);
            const std::int32_t row = ai.firsts[at];
            const std::int32_t mate = lookup(keys, bi, keys.b, keys.a,
                                             static_cast<std::size_t>(row), ai.hashes[at], opt);
            if (mate < 0) {
                ++out.total;
                if (out.held.size() <= opt.max_rows) out.held.push_back(row);
                continue;
            }
            out.pa.push_back(row);
            out.pb.push_back(mate);
        }
        parts[p] = std::move(out);
    };
    auto b_range = [&](unsigned p) {
        Part out;
        const std::size_t lo = bi.firsts.size() * p / b_ways;
        const std::size_t hi = bi.firsts.size() * (p + 1) / b_ways;
        for (std::size_t at = lo; at < hi; ++at) {
            if (at + kPrefetchAhead < hi)
                __builtin_prefetch(&ai.slots[bi.hashes[at + kPrefetchAhead] & ai.mask], 0, 0);
            const std::int32_t row = bi.firsts[at];
            if (lookup(keys, ai, keys.a, keys.b, static_cast<std::size_t>(row), bi.hashes[at],
                       opt) >= 0)
                continue;
            ++out.total;
            if (out.held.size() <= opt.max_rows) out.held.push_back(row);
        }
        b_parts[p] = std::move(out);
    };
    {
        auto guarded = [&](unsigned p) {
            try {
                if (p < ways) a_range(p);
                else b_range(p - ways);
            } catch (...) {
                failures[p] = std::current_exception();
            }
        };
        std::vector<std::thread> more;
        more.reserve(ways + b_ways - 1);
        for (unsigned p = 1; p < ways + b_ways; ++p) {
            try {
                more.emplace_back(guarded, p);
            } catch (const std::system_error&) {
                guarded(p);
            }
        }
        guarded(0);
        for (auto& w : more) w.join();
    }
    for (const auto& f : failures)
        if (f) std::rethrow_exception(f);

    Capped added(opt.max_rows);
    for (const Part& p : b_parts) {
        for (std::int32_t row : p.held) added.push(row);
        added.total += p.total - static_cast<std::int64_t>(p.held.size());
    }

    // Merged in range order, so what survives the cap is what one thread would
    // have kept.
    std::vector<std::int32_t> pair_a, pair_b;
    Capped removed(opt.max_rows);
    {
        std::size_t total = 0;
        for (const Part& p : parts) total += p.pa.size();
        pair_a.reserve(total);
        pair_b.reserve(total);
        for (Part& p : parts) {
            pair_a.insert(pair_a.end(), p.pa.begin(), p.pa.end());
            pair_b.insert(pair_b.end(), p.pb.begin(), p.pb.end());
            for (std::int32_t row : p.held) removed.push(row);
            removed.total += p.total - static_cast<std::int64_t>(p.held.size());
            std::vector<std::int32_t>().swap(p.pa);
            std::vector<std::int32_t>().swap(p.pb);
        }
    }
    // Derived rather than counted when B was not walked. `held` stays empty,
    // which is what `row_lists` being false means: the count is exact either
    // way, and nothing will print the sample.
    if (!opt.row_lists)
        added.total = bi.unique() - static_cast<std::int64_t>(pair_a.size());
    phase.mark("join");
    const std::size_t npairs = pair_a.size();
    const std::size_t words = (npairs + 63) / 64;

    // --- the compared columns, one at a time ------------------------------
    std::vector<ColOut> cols(nc);
    {
        std::atomic<std::size_t> next{0};
        std::vector<std::exception_ptr> col_failures(nc);
        // Each worker owns a whole column, so nothing is shared but the pair
        // arrays and the two mappings, which are const from here on. Four in
        // flight is a memory choice, not a parallelism one: a column of ten
        // million values costs a couple of hundred megabytes on each side while
        // it is being read, and it is released before the next is asked for.
        const unsigned lanes =
            std::max(1u, std::min<unsigned>(budget, static_cast<unsigned>(nc ? nc : 1)));
        auto work = [&] {
            for (;;) {
                const std::size_t c = next.fetch_add(1);
                if (c >= nc) return;
                try {
                    ColOut& out = cols[c];
                    out.bits.assign(words, 0);
                    const Col A(parquet::read_column(am.data(), am.size(),
                                                     slot_of(ameta.names, r.compared[c]), a_path),
                                am.data());
                    const Col B(parquet::read_column(bm.data(), bm.size(),
                                                     slot_of(bmeta.names, r.compared[c]), b_path),
                                bm.data());
                    if (A.rows() != keys.a.rows || B.rows() != keys.b.rows)
                        throw Error("parquet columns disagree about how many rows the file has");

                    auto hit = [&](std::size_t p) {
                        const Cell x = A.at(static_cast<std::size_t>(pair_a[p]));
                        const Cell y = B.at(static_cast<std::size_t>(pair_b[p]));
                        ++out.changed;
                        if (absent(y, opt)) ++out.blanked;
                        if (absent(x, opt)) ++out.filled;
                        out.bits[p >> 6] |= 1ULL << (p & 63);
                        if (out.held.size() <= opt.max_rows)
                            out.held.emplace_back(p, std::make_pair(value_of(x, opt, Fold::display),
                                                                    value_of(y, opt, Fold::display)));
                    };

                    // The fast path: both sides dictionary encoded, so the two
                    // dictionaries go into one id space and the per-row work is
                    // two gathers and an integer compare.
                    //
                    // Not under --ignore-case with a dictionary entry outside
                    // ASCII: coding folds every entry, including ones only an
                    // unchanged or unmatched row holds, and this port refuses
                    // to fold those. The row path folds only a cell that
                    // differs byte for byte, which is where the refusal belongs.
                    const auto foldable = [&](const Col& col) {
                        if (!opt.ignore_case) return true;
                        for (std::size_t k = 0; k < col.dict_size(); ++k)
                            if (!ascii(col.dict_cell(k).view())) return false;
                        return true;
                    };
                    const bool coded = A.dictionary() && B.dictionary() &&
                                       opt.tolerance == 0.0 && foldable(A) && foldable(B);
                    if (coded) {
                        Ids ids(A.dict_size() + B.dict_size());
                        std::vector<std::string> owned;
                        if (needs_normalising(opt)) owned.reserve(A.dict_size() + B.dict_size());
                        const auto code = [&](const Col& col) {
                            std::vector<std::int32_t> map(col.dict_size());
                            for (std::size_t k = 0; k < col.dict_size(); ++k) {
                                const Cell cell = col.dict_cell(k);
                                if (absent(cell, opt)) {
                                    map[k] = -1;
                                } else if (!needs_normalising(opt)) {
                                    map[k] = ids.of(cell.view());
                                } else {
                                    owned.push_back(*value_of(cell, opt));
                                    map[k] = ids.of(owned.back());
                                }
                            }
                            return map;
                        };
                        const std::vector<std::int32_t> amap = code(A), bmap = code(B);
                        const std::vector<std::int32_t>& aix = A.index();
                        const std::vector<std::int32_t>& bix = B.index();
                        std::int32_t xa[kBlock], xb[kBlock];
                        std::uint8_t neq[kBlock];
                        for (std::size_t base = 0; base < npairs; base += kBlock) {
                            const std::size_t m = std::min(kBlock, npairs - base);
                            for (std::size_t i = 0; i < m; ++i) {
                                const std::int32_t k = aix[static_cast<std::size_t>(pair_a[base + i])];
                                xa[i] = k < 0 ? -1 : amap[static_cast<std::size_t>(k)];
                            }
                            for (std::size_t i = 0; i < m; ++i) {
                                const std::int32_t k = bix[static_cast<std::size_t>(pair_b[base + i])];
                                xb[i] = k < 0 ? -1 : bmap[static_cast<std::size_t>(k)];
                            }
                            for (std::size_t i = 0; i < m; ++i)
                                neq[i] = xa[i] != xb[i] ? 1 : 0;
                            scan_mask(neq, m, base, hit);
                        }
                    } else {
                        std::uint8_t neq[kBlock];
                        for (std::size_t base = 0; base < npairs; base += kBlock) {
                            const std::size_t m = std::min(kBlock, npairs - base);
                            for (std::size_t i = 0; i < m; ++i)
                                neq[i] = cell_differs(A.at(static_cast<std::size_t>(pair_a[base + i])),
                                                      B.at(static_cast<std::size_t>(pair_b[base + i])),
                                                      opt)
                                             ? 1
                                             : 0;
                            scan_mask(neq, m, base, hit);
                        }
                    }

                    // The rows that reach the report as added or removed need
                    // every column's value, and this is the only time this
                    // column is in memory.
                    out.added_vals.reserve(added.held.size());
                    for (std::int32_t row : added.held)
                        out.added_vals.push_back(
                            value_of(B.at(static_cast<std::size_t>(row)), opt, Fold::display));
                    out.removed_vals.reserve(removed.held.size());
                    for (std::int32_t row : removed.held)
                        out.removed_vals.push_back(
                            value_of(A.at(static_cast<std::size_t>(row)), opt, Fold::display));
                } catch (...) {
                    col_failures[c] = std::current_exception();
                }
            }
        };
        std::vector<std::thread> workers;
        workers.reserve(lanes - 1);
        for (unsigned i = 1; i < lanes; ++i) {
            try {
                workers.emplace_back(work);
            } catch (const std::system_error&) {
                break;
            }
        }
        work();
        for (auto& w : workers) w.join();
        for (const auto& f : col_failures)
            if (f) std::rethrow_exception(f);
    }

    phase.mark("compared columns");

    for (std::size_t c = 0; c < nc; ++c) {
        r.columns[c].changed = cols[c].changed;
        r.columns[c].blanked = cols[c].blanked;
        r.columns[c].filled = cols[c].filled;
    }

    // --- which pairs changed ---------------------------------------------
    std::vector<std::uint64_t> any(words, 0);
    for (const ColOut& c : cols)
        for (std::size_t w = 0; w < words; ++w) any[w] |= c.bits[w];

    std::int64_t changed_total = 0;
    std::vector<std::size_t> keep;   // the first cap+1 changed pairs, in pair order
    for (std::size_t w = 0; w < words; ++w) {
        std::uint64_t word = any[w];
        changed_total += __builtin_popcountll(word);
        while (opt.row_lists && word && keep.size() <= opt.max_rows) {
            const unsigned bit = static_cast<unsigned>(__builtin_ctzll(word));
            keep.push_back(w * 64 + bit);
            word &= word - 1;
        }
    }

    // --- the report -------------------------------------------------------
    //
    // Only when something will print it. Without `--json` the run ends in one
    // line of counts, and everything below is rows nobody reads: the key of up
    // to `max_rows` changed pairs decoded into strings, the added and removed
    // rows built from their columns, three sorts over them, and a pass over
    // every distinct key of both files for the duplicate sections. That was
    // 0.046s of a 0.54s run on 2M rows of Parquet, charged to a count. The
    // CSV engine has always skipped it under the same flag; the counts below
    // never depended on any of it.
    if (opt.row_lists) {
    const auto key_values = [&](const KeySide& s, std::int32_t row) {
        std::vector<Val> out;
        out.reserve(key_size);
        for (std::size_t j = 0; j < key_size; ++j)
            out.push_back(value_of(s.col[j].at(static_cast<std::size_t>(row)), opt, Fold::display));
        return out;
    };

    std::vector<ChangedRow> rows(keep.size());
    for (std::size_t i = 0; i < keep.size(); ++i)
        rows[i].key = key_values(keys.a, pair_a[keep[i]]);
    for (std::size_t c = 0; c < nc; ++c) {
        for (auto& [p, vals] : cols[c].held) {
            const auto it = std::lower_bound(keep.begin(), keep.end(), p);
            if (it == keep.end() || *it != p) continue;
            rows[static_cast<std::size_t>(it - keep.begin())].cells.push_back(
                {c, std::move(vals.first), std::move(vals.second)});
        }
        std::vector<std::pair<std::size_t, std::pair<Val, Val>>>().swap(cols[c].held);
    }

    const std::size_t width = key_size + nc;
    const auto full_row = [&](const KeySide& s, std::int32_t row, std::size_t at, bool from_b) {
        std::vector<Val> out = key_values(s, row);
        out.resize(width);
        for (std::size_t c = 0; c < nc; ++c)
            out[key_size + c] = from_b ? cols[c].added_vals[at] : cols[c].removed_vals[at];
        return out;
    };
    for (std::size_t i = 0; i < added.held.size(); ++i)
        r.added.push_back(full_row(keys.b, added.held[i], i, true));
    for (std::size_t i = 0; i < removed.held.size(); ++i)
        r.removed.push_back(full_row(keys.a, removed.held[i], i, false));

    const auto by_key = [&](const std::vector<Val>& x, const std::vector<Val>& y) {
        return compare_keys(x, y, key_size) < 0;
    };
    std::stable_sort(r.added.begin(), r.added.end(), by_key);
    std::stable_sort(r.removed.begin(), r.removed.end(), by_key);
    std::stable_sort(rows.begin(), rows.end(), [&](const ChangedRow& x, const ChangedRow& y) {
        return compare_keys(x.key, y.key, key_size) < 0;
    });
    r.changed = std::move(rows);

    const auto dup_section = [&](const KeySide& s, const Index& ix, std::vector<DupRow>& out,
                                 bool& truncated) {
        std::vector<DupRow> all;
        for (std::size_t i = 0; i < ix.firsts.size(); ++i) {
            if (ix.counts[i] < 2) continue;
            all.push_back({key_values(s, ix.firsts[i]), static_cast<std::int64_t>(ix.counts[i])});
        }
        std::stable_sort(all.begin(), all.end(), [&](const DupRow& x, const DupRow& y) {
            if (x.count != y.count) return x.count > y.count;
            return compare_keys(x.key, y.key, key_size) < 0;
        });
        truncated = all.size() > opt.max_rows;
        all.resize(std::min(all.size(), opt.max_rows));
        out = std::move(all);
    };
    dup_section(keys.a, ai, r.dup_a, r.dup_a_truncated);
    dup_section(keys.b, bi, r.dup_b, r.dup_b_truncated);
    }

    const std::int64_t matched = static_cast<std::int64_t>(npairs);
    r.counts.a_rows = ai.rows;
    r.counts.b_rows = bi.rows;
    r.counts.a_keys = ai.unique();
    r.counts.b_keys = bi.unique();
    r.counts.matched = matched;
    r.counts.changed = changed_total;
    r.counts.unchanged = matched - changed_total;
    r.counts.added = added.total;
    r.counts.removed = removed.total;
    r.counts.a_dup_keys = ai.dup_keys;
    r.counts.a_dup_rows = ai.dup_rows;
    r.counts.b_dup_keys = bi.dup_keys;
    r.counts.b_dup_rows = bi.dup_rows;

    r.changed_truncated = changed_total > static_cast<std::int64_t>(opt.max_rows);
    r.added_truncated = added.truncated();
    r.removed_truncated = removed.truncated();
    r.changed.resize(std::min(r.changed.size(), opt.max_rows));
    r.added.resize(std::min(r.added.size(), opt.max_rows));
    r.removed.resize(std::min(r.removed.size(), opt.max_rows));

    phase.mark("report");
    const std::chrono::duration<double> elapsed = std::chrono::steady_clock::now() - started;
    r.seconds = std::round(elapsed.count() * 1000.0) / 1000.0;
    return r;
}

}  // namespace csvdiff
