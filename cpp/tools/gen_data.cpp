// Writes the same deterministic pair of files as scripts/gen_data.py and the
// four other generators, byte for byte, using every core -- as CSV, or as
// Parquet.
//
// The recipe is identical because it has to be: parity.yml checks that all the
// generators emit the same bytes, so a benchmark number from one is comparable
// with a number from another. What differs here is only how the bytes are
// produced. Every row is a pure function of its index -- the drift buckets come
// from a hash of the row number, and money is carried in integer cents so no
// language's rounding rule can enter into it -- which means a row can be
// formatted without knowing any other row. So chunks of rows are formatted in
// parallel and written in order.
//
// Rows are variable width, so a thread cannot know its file offset in advance.
// Rather than guess, the work goes in waves: N threads each fill their own
// buffer, the buffers are written in order, and the next wave starts. Memory is
// bounded by the wave, not by the file.
//
// Parquet comes from the same field-by-field recipe rather than from converting
// the CSV, which is what it used to mean: a pair at ten million rows took DuckDB
// 68-83s to convert and needed the 3.5 GB of CSV to exist first. The columns are
// built straight from the rows instead, and the per-column work of one row group
// -- its dictionary and its compression -- is spread over every core.
//
//   cd cpp && make gen-data
//   cpp/build/gen-data --rows 10m --out-dir data --prefix 10m
//   cpp/build/gen-data --rows 10m --out-dir data --format parquet --compression snappy

#include <fcntl.h>
#include "../src/win32.hpp"   // O_BINARY: a no-op off Windows
#include <unistd.h>

#include <algorithm>
#include <charconv>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

#include "pq_write.hpp"

namespace {

constexpr const char* kColumns =
    "account_id,txn_id,posting_date,value_date,currency,amount,fee,balance,status,channel,"
    "region,branch_code,product_code,counterparty,quantity,rate,category,risk_flag,note,"
    "updated_at\n";

const char* const kStatus[] = {"posted", "pending", "settled", "reversed"};
const char* const kChannel[] = {"branch", "online", "mobile", "atm", "wire"};
const char* const kRegion[] = {"EMEA", "NA", "APAC", "LATAM"};
const char* const kCurrency[] = {"USD", "EUR", "GBP", "JPY"};
const char* const kCategory[] = {"retail", "corporate", "treasury", "cards", "loans"};

// Drift buckets, against a 0..9999 hash bucket per row.
constexpr int kChgStatus = 300, kChgAmount = 150, kChgBalance = 150, kChgValueDate = 30;
constexpr std::int64_t kRemovedMod = 1000, kAddedRatio = 1000, kDupMod = 10000;

// 240 dates from 2026-01-01, precomputed once: the other generators get these
// from a date library, and this one only needs the same strings out.
struct Days {
    char text[240][11];
    Days() {
        // Written a digit at a time rather than with snprintf: the compiler
        // cannot see that month and day are bounded, so a format string draws a
        // truncation warning that -Werror turns into a build failure.
        static const int len[] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
        int month = 0, day = 1;
        for (int i = 0; i < 240; ++i) {
            char* p = text[i];
            *p++ = '2'; *p++ = '0'; *p++ = '2'; *p++ = '6'; *p++ = '-';
            *p++ = static_cast<char>('0' + (month + 1) / 10);
            *p++ = static_cast<char>('0' + (month + 1) % 10);
            *p++ = '-';
            *p++ = static_cast<char>('0' + day / 10);
            *p++ = static_cast<char>('0' + day % 10);
            *p = '\0';
            if (++day > len[month]) {
                day = 1;
                ++month;
            }
        }
    }
};
const Days kDays;

// splitmix-style mix, matching the other implementations bit for bit.
std::uint64_t mix(std::int64_t i, std::int64_t salt, std::int64_t seed) {
    std::uint64_t x = static_cast<std::uint64_t>(i * 31 + salt + seed);
    x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9ULL;
    x = (x ^ (x >> 27)) * 0x94D049BB133111EBULL;
    return x ^ (x >> 31);
}

int mod(std::int64_t i, std::int64_t salt, std::int64_t seed, int m) {
    return static_cast<int>(mix(i, salt, seed) % static_cast<std::uint64_t>(m));
}

char* put(char* p, const char* s) {
    const std::size_t n = std::strlen(s);
    std::memcpy(p, s, n);
    return p + n;
}

char* put_int(char* p, std::int64_t v) {
    const auto done = std::to_chars(p, p + 24, v);
    return done.ptr;
}

// Left-pads to `width` with zeroes, which is what the other generators do with
// a format string.
char* put_pad(char* p, std::int64_t v, int width) {
    char tmp[24];
    const auto done = std::to_chars(tmp, tmp + sizeof tmp, v);
    const int n = static_cast<int>(done.ptr - tmp);
    for (int i = n; i < width; ++i) *p++ = '0';
    std::memcpy(p, tmp, static_cast<std::size_t>(n));
    return p + n;
}

// An amount held in cents, written as a two-decimal number.
char* put_money(char* p, std::int64_t cents) {
    if (cents < 0) {
        *p++ = '-';
        cents = -cents;
    }
    p = put_int(p, cents / 100);
    *p++ = '.';
    return put_pad(p, cents % 100, 2);
}

constexpr int kFieldCount = 20;

// One row of one side, as its twenty fields rather than as a line of text.
//
// The CSV writer joins these with commas and the Parquet writer hands them to
// column builders, so both formats come from one recipe and cannot drift. An
// empty field is a value the row does not have: CSV has no other way to say it,
// and Parquet writes it as a null, which is what every reader here treats an
// empty CSV field as anyway.
struct Row {
    char buf[512];
    std::string_view f[kFieldCount];
};

void fields(Row& out, std::int64_t i, bool b, std::int64_t seed) {
    const int bucket = mod(i, 0, seed, 10000);
    std::int64_t amount_cents = mod(i, 21, seed, 900000000) - 100000000;
    std::int64_t balance_cents = mod(i, 31, seed, 2000000000);
    const char* st = kStatus[mod(i, 11, seed, 4)];
    const char* value_date = kDays.text[mod(i, 41, seed, 240)];

    if (b) {
        if (bucket < kChgStatus) {
            st = kStatus[(mod(i, 11, seed, 4) + 1) % 4];
        } else if (bucket < kChgStatus + kChgAmount) {
            amount_cents += 1234;
        } else if (bucket < kChgStatus + kChgAmount + kChgBalance) {
            balance_cents = (balance_cents * 101 + 50) / 100;  // +1%, half up, in cents
        }
        if (bucket < kChgValueDate) value_date = "";
    }

    char* p = out.buf;
    int c = 0;
    // Closes the field that started at `p` and begins the next one.
    const auto done = [&](char* end) {
        out.f[c++] = std::string_view(p, static_cast<std::size_t>(end - p));
        p = end;
    };

    done(put_pad(put(p, "ACC-"), (i * 7919) % 250000, 8));
    done(put_pad(put(p, "TXN-"), i, 11));
    done(put(p, kDays.text[mod(i, 1, seed, 240)]));
    done(put(p, value_date));
    done(put(p, kCurrency[mod(i, 51, seed, 4)]));
    done(put_money(p, amount_cents));
    done(put_money(p, mod(i, 61, seed, 5000)));
    done(put_money(p, balance_cents));
    done(put(p, st));
    done(put(p, kChannel[mod(i, 71, seed, 5)]));
    done(put(p, kRegion[mod(i, 81, seed, 4)]));
    done(put_pad(put(p, "BR"), mod(i, 91, seed, 900) + 100, 4));
    done(put_pad(put(p, "P"), mod(i, 101, seed, 5000), 5));
    done(put_pad(put(p, "CP-"), mod(i, 111, seed, 90000), 6));
    done(put_int(p, mod(i, 121, seed, 500) + 1));
    done(put_pad(put(p, "0."), mod(i, 131, seed, 1200), 4));
    done(put(p, kCategory[mod(i, 141, seed, 5)]));
    *p = mod(i, 151, seed, 20) == 0 ? 'Y' : 'N';
    done(p + 1);
    {
        char* q = put(p, "batch ");
        q = put_int(q, i % 997 + 1);
        q = put(q, " line ");
        done(put_int(q, i % 53 + 1));
    }
    done(put(p, b ? "2026-09-01 02:15:00" : "2026-08-01 02:15:00"));
}

// The same row as a CSV line. `dst` must have at least 256 bytes of room.
char* row(char* dst, Row& scratch, std::int64_t i, bool b, std::int64_t seed) {
    fields(scratch, i, b, seed);
    for (int c = 0; c < kFieldCount; ++c) {
        if (c) *dst++ = ',';
        std::memcpy(dst, scratch.f[c].data(), scratch.f[c].size());
        dst += scratch.f[c].size();
    }
    *dst++ = '\n';
    return dst;
}

// The column names, split out of the CSV header once so the JSON and Parquet
// writers can name their fields.
const std::vector<std::string>& column_names() {
    static const std::vector<std::string> names = [] {
        std::vector<std::string> out;
        for (const char* p = kColumns; *p;) {
            const char* q = p;
            while (*q && *q != ',' && *q != '\n') ++q;
            out.emplace_back(p, static_cast<std::size_t>(q - p));
            p = *q ? q + 1 : q;
        }
        return out;
    }();
    return names;
}

// A JSON string body. None of the generated values need escaping, but a
// generator that only happens to be correct for its own data is a trap for
// whoever changes the recipe next.
char* put_json(char* p, std::string_view v) {
    for (char c : v) {
        switch (c) {
            case '"': *p++ = '\\'; *p++ = '"'; break;
            case '\\': *p++ = '\\'; *p++ = '\\'; break;
            case '\n': *p++ = '\\'; *p++ = 'n'; break;
            case '\r': *p++ = '\\'; *p++ = 'r'; break;
            case '\t': *p++ = '\\'; *p++ = 't'; break;
            default:
                if (static_cast<unsigned char>(c) < 0x20) {
                    static const char kHex[] = "0123456789abcdef";
                    *p++ = '\\'; *p++ = 'u'; *p++ = '0'; *p++ = '0';
                    *p++ = kHex[(c >> 4) & 0xF];
                    *p++ = kHex[c & 0xF];
                } else {
                    *p++ = c;
                }
        }
    }
    return p;
}

// The same row as one line of newline-delimited JSON. An empty field becomes
// `null`, which is what the empty CSV field means and what the Parquet writer
// puts there. `dst` must have at least 1024 bytes of room.
char* row_json(char* dst, Row& scratch, std::int64_t i, bool b, std::int64_t seed) {
    fields(scratch, i, b, seed);
    const std::vector<std::string>& names = column_names();
    *dst++ = '{';
    for (int c = 0; c < kFieldCount; ++c) {
        if (c) *dst++ = ',';
        *dst++ = '"';
        std::memcpy(dst, names[c].data(), names[c].size());
        dst += names[c].size();
        *dst++ = '"';
        *dst++ = ':';
        if (scratch.f[c].empty()) {
            std::memcpy(dst, "null", 4);
            dst += 4;
            continue;
        }
        *dst++ = '"';
        dst = put_json(dst, scratch.f[c]);
        *dst++ = '"';
    }
    *dst++ = '}';
    *dst++ = '\n';
    return dst;
}

// Every row index in [from, to) as it appears in file A, then in file B. The
// two sides differ: B drops one row in a thousand and repeats one in ten
// thousand, both decided by the index alone, which is what lets a chunk be
// built without seeing the chunks before it.
void fill(std::int64_t from, std::int64_t to, std::int64_t rows, std::int64_t seed, bool json,
          std::string& a, std::string& b) {
    a.clear();
    b.clear();
    Row scratch;
    char line[1024];
    const auto one = [&](std::int64_t i, bool side_b, std::string& out) {
        const char* end = json ? row_json(line, scratch, i, side_b, seed)
                               : row(line, scratch, i, side_b, seed);
        out.append(line, static_cast<std::size_t>(end - line));
    };
    for (std::int64_t i = from; i < to; ++i) {
        one(i, false, a);
        if (i % kRemovedMod != 7) one(i, true, b);
        if (i % kDupMod == 3 && i < rows / 2) one(i, true, b);
    }
}

// The two sides' row sequences, said once so both formats emit the same rows in
// the same order. A is every row plus a tail of repeated keys; B drops one row
// in a thousand, repeats one in ten thousand, and gains a tail of its own.
template <typename Fn>
void each_row(bool b, std::int64_t rows, Fn&& emit) {
    const std::int64_t dup_extra = std::max<std::int64_t>(1, rows / kDupMod);
    const std::int64_t added = std::max<std::int64_t>(1, rows / kAddedRatio);
    for (std::int64_t i = 0; i < rows; ++i) {
        if (!b) {
            emit(i);
            continue;
        }
        if (i % kRemovedMod != 7) emit(i);
        if (i % kDupMod == 3 && i < rows / 2) emit(i);
    }
    if (!b) {
        for (std::int64_t i = 0; i < dup_extra; ++i) emit(i);
    } else {
        for (std::int64_t i = rows; i < rows + added; ++i) emit(i);
    }
}

// One side, as Parquet. An empty field becomes a null, which is what a Parquet
// writer fed the equivalent CSV would do and what every reader here treats an
// empty CSV field as.
void write_parquet(const std::string& path, std::int64_t rows, std::int64_t seed, bool b,
                   pqwrite::Codec codec, std::int64_t rg_rows, std::size_t dict_limit,
                   unsigned threads) {
    pqwrite::Writer w(path, column_names(), codec, rg_rows, dict_limit, threads);
    Row scratch;
    std::vector<pqwrite::Value> cells(kFieldCount);
    each_row(b, rows, [&](std::int64_t i) {
        fields(scratch, i, b, seed);
        for (int c = 0; c < kFieldCount; ++c)
            cells[c] = scratch.f[c].empty() ? pqwrite::Value::none()
                                            : pqwrite::Value::of(scratch.f[c]);
        w.row(cells);
    });
    w.close();
}

void write_all(int fd, const char* data, std::size_t n, const char* path) {
    while (n > 0) {
        const ssize_t wrote = ::write(fd, data, n);
        if (wrote <= 0) {
            std::fprintf(stderr, "error: cannot write %s\n", path);
            std::exit(2);
        }
        data += wrote;
        n -= static_cast<std::size_t>(wrote);
    }
}

std::int64_t parse_rows(const std::string& s) {
    std::string t;
    for (char c : s)
        if (c != '_' && c != ',') t += static_cast<char>(std::tolower(c));
    if (t.empty()) return -1;
    std::int64_t mult = 1;
    if (t.back() == 'k') mult = 1000;
    else if (t.back() == 'm') mult = 1000000;
    else if (t.back() == 'g') mult = 1000000000;
    if (mult > 1) t.pop_back();
    try {
        return static_cast<std::int64_t>(std::stod(t) * static_cast<double>(mult) + 0.5);
    } catch (...) {
        return -1;
    }
}

}  // namespace

int main(int argc, char** argv) {
    std::string rows_arg = "10k", out_dir = "data", prefix, format = "csv", compression = "snappy";
    std::int64_t seed = 7, rg_rows = 122880;
    std::size_t dict_limit = 8192;
    unsigned threads = 0;
    for (int i = 1; i < argc; ++i) {
        const std::string f = argv[i];
        auto next = [&]() -> std::string { return i + 1 < argc ? argv[++i] : ""; };
        if (f == "--rows" || f == "-n") rows_arg = next();
        else if (f == "--out-dir" || f == "-o") out_dir = next();
        else if (f == "--prefix") prefix = next();
        else if (f == "--seed") seed = std::stoll(next());
        else if (f == "--threads") threads = static_cast<unsigned>(std::stoul(next()));
        else if (f == "--format") format = next();
        else if (f == "--compression") compression = next();
        else if (f == "--row-group-size") rg_rows = std::stoll(next());
        else if (f == "--dict-limit") dict_limit = static_cast<std::size_t>(std::stoull(next()));
        else if (f == "-h" || f == "--help") {
            std::printf(
                "usage: gen-data --rows 10m --out-dir DIR [--prefix P] [--threads N]\n"
                "                [--format csv|json|ndjson|parquet] [--compression snappy|none]\n"
                "                [--row-group-size N] [--dict-limit N]\n"
                "\n"
                "--dict-limit is how many distinct values a column may have in one row\n"
                "group before it gives up on the dictionary; lowering it produces the\n"
                "mixed-encoding columns a real writer emits at scale.\n");
            return 0;
        }
    }
    const std::int64_t rows = parse_rows(rows_arg);
    if (rows <= 0) {
        std::fprintf(stderr, "error: --rows must be a positive number\n");
        return 2;
    }
    if (prefix.empty()) prefix = rows_arg;
    if (threads == 0) threads = std::max(1u, std::thread::hardware_concurrency());

    // `ndjson` and `json` name the same format. The Rust generator has always
    // taken both; this port and the C one took only `json`, so a command that
    // worked against one generator failed against another. Normalised here so
    // the checks below still have one spelling to test.
    if (format == "ndjson") format = "json";

    if (format == "parquet") {
        if (compression != "snappy" && compression != "none") {
            std::fprintf(stderr, "error: --compression must be snappy or none\n");
            return 2;
        }
        const pqwrite::Codec codec =
            compression == "snappy" ? pqwrite::Codec::Snappy : pqwrite::Codec::None;
        const std::string ext = compression == "snappy" ? ".parquet" : ".unc.parquet";
        // The two sides are independent files, so they are written at once and
        // each spreads its own columns over half the machine.
        const unsigned per_side = std::max(1u, threads / 2);
        std::string failure;
        std::thread other([&] {
            try {
                write_parquet(out_dir + "/" + prefix + "_b" + ext, rows, seed, true, codec,
                              rg_rows, dict_limit, per_side);
            } catch (const std::exception& e) {
                failure = e.what();
            }
        });
        try {
            write_parquet(out_dir + "/" + prefix + "_a" + ext, rows, seed, false, codec, rg_rows,
                          dict_limit, per_side);
        } catch (const std::exception& e) {
            other.join();
            std::fprintf(stderr, "error: %s\n", e.what());
            return 2;
        }
        other.join();
        if (!failure.empty()) {
            std::fprintf(stderr, "error: %s\n", failure.c_str());
            return 2;
        }
        std::printf("c++: %lld rows x 20 columns, parquet (%s) on %u threads\n",
                    static_cast<long long>(rows), compression.c_str(), threads);
        return 0;
    }
    if (format != "csv" && format != "json") {
        std::fprintf(stderr, "error: --format must be csv, json (ndjson) or parquet\n");
        return 2;
    }

    // CSV and newline-delimited JSON are both line-oriented, so they take the
    // same path: waves of threads formatting their own chunk, written in order.
    // Only the line differs, and the header, which JSON does not have -- its
    // field names are on every row.
    const bool json = format == "json";
    const std::string ext = json ? ".ndjson" : ".csv";
    const std::string a_path = out_dir + "/" + prefix + "_a" + ext;
    const std::string b_path = out_dir + "/" + prefix + "_b" + ext;
    const int fa = ::open(a_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, 0644);
    const int fb = ::open(b_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, 0644);
    if (fa < 0 || fb < 0) {
        std::fprintf(stderr, "error: cannot create the output files in %s\n", out_dir.c_str());
        return 2;
    }
    if (!json) {
        write_all(fa, kColumns, std::strlen(kColumns), a_path.c_str());
        write_all(fb, kColumns, std::strlen(kColumns), b_path.c_str());
    }

    // Enough rows per chunk that a thread has real work, few enough that a wave
    // of them stays comfortably inside the page cache.
    const std::int64_t per_chunk = std::max<std::int64_t>(1 << 14, rows / (threads * 16));
    std::vector<std::string> a_buf(threads), b_buf(threads);
    for (unsigned t = 0; t < threads; ++t) {
        a_buf[t].reserve(static_cast<std::size_t>(per_chunk) * (json ? 512 : 200));
        b_buf[t].reserve(static_cast<std::size_t>(per_chunk) * (json ? 512 : 200));
    }

    for (std::int64_t start = 0; start < rows; start += per_chunk * threads) {
        std::vector<std::thread> workers;
        unsigned live = 0;
        for (unsigned t = 0; t < threads; ++t) {
            const std::int64_t from = start + static_cast<std::int64_t>(t) * per_chunk;
            if (from >= rows) break;
            const std::int64_t to = std::min(from + per_chunk, rows);
            ++live;
            if (t == 0) continue;  // this thread takes chunk 0 itself
            workers.emplace_back(
                [&, t, from, to] { fill(from, to, rows, seed, json, a_buf[t], b_buf[t]); });
        }
        if (live == 0) break;
        fill(start, std::min(start + per_chunk, rows), rows, seed, json, a_buf[0], b_buf[0]);
        for (auto& w : workers) w.join();
        for (unsigned t = 0; t < live; ++t) {
            write_all(fa, a_buf[t].data(), a_buf[t].size(), a_path.c_str());
            write_all(fb, b_buf[t].data(), b_buf[t].size(), b_path.c_str());
        }
    }

    // The tails: duplicate keys appended to A, and rows only in B.
    const std::int64_t dup_extra = std::max<std::int64_t>(1, rows / kDupMod);
    const std::int64_t added = std::max<std::int64_t>(1, rows / kAddedRatio);
    std::string tail;
    Row scratch;
    char line[1024];
    const auto one = [&](std::int64_t i, bool side_b) {
        const char* end = json ? row_json(line, scratch, i, side_b, seed)
                               : row(line, scratch, i, side_b, seed);
        tail.append(line, static_cast<std::size_t>(end - line));
    };
    for (std::int64_t i = 0; i < dup_extra; ++i) one(i, false);
    write_all(fa, tail.data(), tail.size(), a_path.c_str());
    tail.clear();
    for (std::int64_t i = rows; i < rows + added; ++i) one(i, true);
    write_all(fb, tail.data(), tail.size(), b_path.c_str());

    ::close(fa);
    ::close(fb);
    std::printf("c++: %lld rows x 20 columns, %s on %u threads\n",
                static_cast<long long>(rows), json ? "ndjson" : "csv", threads);
    return 0;
}
