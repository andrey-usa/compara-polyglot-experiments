// The JSON half of the result contract, spelled out by hand.
//
// The key order and the snake_case names match what the other five ports emit,
// because the cross-language parity job compares these documents directly.
#include "csvdiff.hpp"

#include <array>
#include <charconv>
#include <cmath>
#include <concepts>
#include <cstdio>

namespace csvdiff {
namespace {

// The bytes JSON may not carry raw: the C0 controls, the quote and the
// backslash. Everything else, 0x80 and up included, goes through untouched --
// which is what the other ports emit and what the parity job compares.
constexpr std::array<bool, 256> kEscape = [] {
    std::array<bool, 256> t{};
    for (int i = 0; i < 0x20; ++i) t[static_cast<std::size_t>(i)] = true;
    t['"'] = true;
    t['\\'] = true;
    return t;
}();

// The payload, built in a plain string.
//
// It was a `std::ostringstream`, and every `<<` on one -- a comma, a bracket,
// a count -- constructs an `ostream::sentry`, which checks the stream's state
// and flushes whatever is tied to it, before the byte or two that follows. The
// runs of ordinary bytes were already written one call apiece (it had been one
// `put` per byte), but the punctuation between them still went through the
// stream: on a 400k pair `to_json` was 164M instructions, 60M of them in
// `ostream::put` and most of the rest in the sentry and the stream's buffer.
// The C port's writer does not show in its profile at all.
//
// `Out` keeps the `<<` spelling so the writer below reads as it did, and each
// one is an append. The only double, `seconds`, is printed as the stream
// printed it -- `%g`, six significant digits -- since the parity job compares
// these documents byte for byte apart from the timing.
struct Out {
    std::string s;
    Out& operator<<(std::string_view v) {
        s.append(v);
        return *this;
    }
    Out& operator<<(char c) {
        s.push_back(c);
        return *this;
    }
    template <std::integral T>
    Out& operator<<(T v) {
        char buf[24];
        const auto res = std::to_chars(buf, buf + sizeof buf, v);
        s.append(buf, static_cast<std::size_t>(res.ptr - buf));
        return *this;
    }
    Out& operator<<(double v) {
        char buf[32];
        const int n = std::snprintf(buf, sizeof buf, "%g", v);
        s.append(buf, static_cast<std::size_t>(n));
        return *this;
    }
    void write(const char* p, std::size_t n) { s.append(p, n); }
    void put(char c) { s.push_back(c); }
};

// One `write` per run of ordinary bytes, not one `put` per byte. Escapes are
// rare enough that a `write` apiece for them costs nothing worth saving.
void write_string(Out& o, std::string_view s) {
    o.put('"');
    std::size_t run = 0;
    for (std::size_t i = 0; i < s.size(); ++i) {
        const auto c = static_cast<unsigned char>(s[i]);
        if (!kEscape[c]) {
            ++run;
            continue;
        }
        if (run) {
            o.write(s.data() + i - run, run);
            run = 0;
        }
        switch (c) {
            case '"': o.write("\\\"", 2); break;
            case '\\': o.write("\\\\", 2); break;
            case '\n': o.write("\\n", 2); break;
            case '\r': o.write("\\r", 2); break;
            case '\t': o.write("\\t", 2); break;
            default: {
                char buf[8];
                std::snprintf(buf, sizeof buf, "\\u%04x", c);
                o.write(buf, 6);
            }
        }
    }
    if (run) o.write(s.data() + s.size() - run, run);
    o.put('"');
}

void write_val(Out& o, const Val& v) {
    if (!v) {
        o << "null";
        return;
    }
    write_string(o, *v);
}

void write_strings(Out& o, const std::vector<std::string>& v) {
    o << '[';
    for (std::size_t i = 0; i < v.size(); ++i) {
        if (i) o << ',';
        write_string(o, v[i]);
    }
    o << ']';
}

void write_row(Out& o, const std::vector<Val>& row) {
    o << '[';
    for (std::size_t i = 0; i < row.size(); ++i) {
        if (i) o << ',';
        write_val(o, row[i]);
    }
    o << ']';
}

}  // namespace

std::string to_json(const Result& r, const std::string& a_path, const std::string& b_path,
                    const Options& opt) {
    Out o;
    o.s.reserve(std::size_t{1} << 16);
    o << "{\"meta\":{";
    o << "\"key\":";
    write_strings(o, r.key);
    o << ",\"compared\":";
    write_strings(o, r.compared);
    o << ",\"only_in_a\":";
    write_strings(o, r.only_in_a);
    o << ",\"only_in_b\":";
    write_strings(o, r.only_in_b);
    o << ",\"a_cols\":" << r.a_cols << ",\"b_cols\":" << r.b_cols;
    o << ",\"a\":{\"name\":";
    write_string(o, a_path);
    o << "},\"b\":{\"name\":";
    write_string(o, b_path);
    o << "},\"engine\":\"turbo\",\"seconds\":" << r.seconds << "}";

    const Counts& c = r.counts;
    o << ",\"counts\":{";
    o << "\"a_rows\":" << c.a_rows << ",\"b_rows\":" << c.b_rows;
    o << ",\"a_keys\":" << c.a_keys << ",\"b_keys\":" << c.b_keys;
    o << ",\"matched\":" << c.matched << ",\"unchanged\":" << c.unchanged;
    o << ",\"changed\":" << c.changed << ",\"added\":" << c.added << ",\"removed\":" << c.removed;
    o << ",\"a_dup_keys\":" << c.a_dup_keys << ",\"a_dup_rows\":" << c.a_dup_rows;
    o << ",\"b_dup_keys\":" << c.b_dup_keys << ",\"b_dup_rows\":" << c.b_dup_rows << "}";

    o << ",\"columns\":[";
    for (std::size_t i = 0; i < r.columns.size(); ++i) {
        if (i) o << ',';
        o << "{\"name\":";
        write_string(o, r.columns[i].name);
        o << ",\"changed\":" << r.columns[i].changed << ",\"blanked\":" << r.columns[i].blanked
          << ",\"filled\":" << r.columns[i].filled << '}';
    }
    o << ']';

    std::vector<std::string> row_cols = r.key;
    row_cols.insert(row_cols.end(), r.compared.begin(), r.compared.end());

    o << ",\"changed\":{\"cols\":";
    write_strings(o, r.key);
    o << ",\"rows\":[";
    for (std::size_t i = 0; i < r.changed.size(); ++i) {
        if (i) o << ',';
        o << '[';
        for (const Val& v : r.changed[i].key) {
            write_val(o, v);
            o << ',';
        }
        o << '[';
        for (std::size_t j = 0; j < r.changed[i].cells.size(); ++j) {
            if (j) o << ',';
            const CellDiff& d = r.changed[i].cells[j];
            o << '[' << d.column << ',';
            write_val(o, d.a);
            o << ',';
            write_val(o, d.b);
            o << ']';
        }
        o << "]]";
    }
    o << "],\"truncated\":" << (r.changed_truncated ? "true" : "false") << '}';

    const auto section = [&](const char* name, const std::vector<std::vector<Val>>& rows,
                             bool truncated) {
        o << ",\"" << name << "\":{\"cols\":";
        write_strings(o, row_cols);
        o << ",\"rows\":[";
        for (std::size_t i = 0; i < rows.size(); ++i) {
            if (i) o << ',';
            write_row(o, rows[i]);
        }
        o << "],\"truncated\":" << (truncated ? "true" : "false") << '}';
    };
    section("added", r.added, r.added_truncated);
    section("removed", r.removed, r.removed_truncated);

    const auto dups = [&](const char* name, const std::vector<DupRow>& rows, bool truncated) {
        std::vector<std::string> cols = r.key;
        cols.push_back("count");
        o << ",\"" << name << "\":{\"cols\":";
        write_strings(o, cols);
        o << ",\"rows\":[";
        for (std::size_t i = 0; i < rows.size(); ++i) {
            if (i) o << ',';
            o << '[';
            for (const Val& v : rows[i].key) {
                write_val(o, v);
                o << ',';
            }
            o << rows[i].count << ']';
        }
        o << "],\"truncated\":" << (truncated ? "true" : "false") << '}';
    };
    dups("dup_a", r.dup_a, r.dup_a_truncated);
    dups("dup_b", r.dup_b, r.dup_b_truncated);

    (void)opt;
    o << '}';
    return std::move(o.s);
}

}  // namespace csvdiff
