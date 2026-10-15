// A composite-key CSV comparison, byte-level.
//
// This is the same design as the Java and Rust `turbo` engines: the file is
// mapped, a field is an offset and a length rather than a string, and nothing
// becomes a std::string unless it reaches the report. It exists as the reference
// point everyone can read — the implementation a C++ programmer would expect —
// measured against the same data as the other ports.
//
// It carries the comparison and the JSON contract, not the HTML report; the five
// full ports already produce that, and what is interesting here is the engine.
#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace csvdiff {

// What varies per comparison. Names match the JSON the other ports emit.
struct Options {
    std::vector<std::string> key;
    std::vector<std::string> compare;   // empty means every common non-key column
    std::vector<std::string> ignore;
    bool trim = false;
    bool ignore_case = false;
    bool empty_is_null = false;
    double tolerance = 0.0;
    std::size_t max_rows = 50000;
    std::optional<char> delimiter;      // unset means sniff it from the header
    // 0 means "as many as the machine has". Both files are read at once, and
    // each is split into this many chunks, so the work is spread this wide in
    // total rather than per file.
    unsigned threads = 0;
    // Whether anything will print the row lists. The counts are always exact
    // either way; this only decides whether the *sample* of added rows is worth
    // a pass over B to collect. See the note on `b_side` in csvdiff.cpp.
    bool row_lists = true;
};

// Row counts. Always exact, even when the embedded row lists are capped.
struct Counts {
    std::int64_t a_rows = 0, b_rows = 0;
    std::int64_t a_keys = 0, b_keys = 0;
    std::int64_t matched = 0, unchanged = 0, changed = 0;
    std::int64_t added = 0, removed = 0;
    std::int64_t a_dup_keys = 0, a_dup_rows = 0;
    std::int64_t b_dup_keys = 0, b_dup_rows = 0;
};

// How one compared column fared across the matched rows.
struct ColumnStat {
    std::string name;
    std::int64_t changed = 0, blanked = 0, filled = 0;
};

// A cell value, absent when the field was empty or missing.
using Val = std::optional<std::string>;

// One differing cell of a changed row.
struct CellDiff {
    std::size_t column;
    Val a, b;
};

struct ChangedRow {
    std::vector<Val> key;
    std::vector<CellDiff> cells;
};

struct DupRow {
    std::vector<Val> key;
    std::int64_t count;
};

struct Result {
    std::vector<std::string> key, compared, only_in_a, only_in_b;
    std::size_t a_cols = 0, b_cols = 0;
    Counts counts;
    std::vector<ColumnStat> columns;
    std::vector<ChangedRow> changed;
    std::vector<std::vector<Val>> added, removed;
    std::vector<DupRow> dup_a, dup_b;
    bool changed_truncated = false, added_truncated = false, removed_truncated = false;
    bool dup_a_truncated = false, dup_b_truncated = false;
    double seconds = 0.0;

    bool identical() const {
        return counts.changed == 0 && counts.added == 0 && counts.removed == 0;
    }
};

// Thrown for anything the caller can fix: a missing file, an absent key column.
struct Error : std::exception {
    std::string message;
    explicit Error(std::string m) : message(std::move(m)) {}
    const char* what() const noexcept override { return message.c_str(); }
};

Result compare(const std::string& a_path, const std::string& b_path, const Options& opt);

// The result as the JSON every port of this tool writes with --json.
std::string to_json(const Result& r, const std::string& a_path, const std::string& b_path,
                    const Options& opt);

}  // namespace csvdiff
