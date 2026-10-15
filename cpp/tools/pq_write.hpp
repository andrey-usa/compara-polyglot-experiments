// A Parquet writer for making benchmark and test data, not for production.
//
// It exists because every Parquet file in this project used to be made by
// handing DuckDB a CSV, which costs 68-83s a pair at ten million rows and needs
// the 3.5 GB of CSV to exist first. Generating the columns straight from the row
// recipe skips both.
//
// It writes what the reader in cpp/src/parquet.cpp reads and what DuckDB reads:
// flat schemas of optional UTF8 BYTE_ARRAY columns, PLAIN and dictionary
// encodings chosen per column per row group by cardinality, RLE definition
// levels, v1 data pages, uncompressed or snappy. That is deliberately the same
// envelope the reader accepts -- a generator that could write shapes the reader
// cannot read would be testing nothing.
//
// Choosing the encoding per row group rather than per column is what reproduces
// the case that matters: a column whose distinct values outgrow the dictionary
// budget partway through starts as dictionary pages and continues as plain ones,
// which is exactly what DuckDB does to a high-cardinality string at scale.
#pragma once

#include <cstdint>
#include <exception>
#include <string>
#include <string_view>
#include <vector>

namespace pqwrite {

enum class Codec { None, Snappy };

// One cell. A null is not the same as an empty string in Parquet, and the
// difference has to survive the trip, so it is said explicitly.
struct Value {
    std::string_view text;
    bool null = false;

    static Value of(std::string_view s) { return Value{s, false}; }
    static Value none() { return Value{{}, true}; }
};

// Thrown for anything the caller can fix; the message names the problem.
struct Error : std::exception {
    std::string message;
    explicit Error(std::string m) : message(std::move(m)) {}
    const char* what() const noexcept override { return message.c_str(); }
};

class Writer {
  public:
    // `dict_limit` is the number of distinct values a column may have in one row
    // group before it gives up on the dictionary and writes plain pages, which
    // is the knob that produces mixed-encoding columns.
    // `threads` is how many columns of one row group are built at once: a
    // column's dictionary and its compression depend on nothing outside it.
    // 0 means as many as the machine has.
    Writer(const std::string& path, std::vector<std::string> names, Codec codec,
           std::int64_t row_group_rows = 122880, std::size_t dict_limit = 8192,
           unsigned threads = 0);
    ~Writer();

    Writer(const Writer&) = delete;
    Writer& operator=(const Writer&) = delete;

    // One row. `values` must have one entry per column name.
    void row(const std::vector<Value>& values);

    // Writes the last row group and the footer. Must be called; the destructor
    // will not do it, because a failure there has nowhere to go.
    void close();

    std::size_t columns() const { return names_.size(); }

  private:
    struct Column;

    void flush_row_group();
    void write_all(const std::string& bytes);

    std::string path_;
    std::vector<std::string> names_;
    Codec codec_;
    std::int64_t rg_rows_;
    std::size_t dict_limit_;
    unsigned threads_;
    int fd_ = -1;
    std::int64_t at_ = 0;        // bytes written so far, which is the file offset
    std::int64_t rows_ = 0;      // rows in the whole file
    std::int64_t held_ = 0;      // rows in the row group being built
    std::vector<Column> cols_;
    std::string footer_;         // row group metadata, encoded as it is written
    std::vector<std::string> groups_;
    bool closed_ = false;
};

}  // namespace pqwrite
