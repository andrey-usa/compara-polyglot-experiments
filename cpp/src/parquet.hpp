// A Parquet reader shaped for comparing, not for querying.
//
// It reads exactly what this job needs and refuses the rest by name, which is
// the same bargain the rest of this port makes: BYTE_ARRAY columns, PLAIN and
// dictionary encodings, uncompressed or Snappy. That covers what DuckDB and
// polars write for string columns, and it is the shape the comparison actually
// meets.
//
// Two things it does that a general reader would not.
//
// It hands back *offsets into the mapped file* rather than strings. A PLAIN
// byte array is a four-byte length followed by its bytes, already contiguous
// and already in the mapping, so a value can stay an offset and a length --
// the same representation the CSV and JSON paths use, and the reason nothing
// here builds a string per cell.
//
// And it keeps dictionary columns encoded. Where a column is dictionary
// encoded, the rows are indices into a small table of distinct values, so two
// files can be compared by mapping one dictionary onto the other once and then
// comparing integers -- which is what makes the per-column diff a vector
// operation instead of a string compare per row.

#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace csvdiff::parquet {

// A value's bytes, as an offset and a length packed into one word: forty bits
// of offset, twenty-three of length, and one to say the value is null. That is
// the same layout the CSV engine packs a field into, for the same reason --
// eight bytes a value rather than sixteen is eighty megabytes less to carry,
// and eighty megabytes less to stream past the CPU, on a ten-million-row
// column. Forty bits of offset is a terabyte; twenty-three of length is eight
// megabytes, and a longer value is refused by name rather than truncated.
//
// The offset is into the mapped file where the page was stored uncompressed,
// and into the reader's own buffer where it had to be decompressed --
// `Column::owned` says which.
class Slice {
  public:
    static constexpr std::uint32_t kMaxLength = (1u << 23) - 1;

    Slice() = default;
    static Slice at(std::uint64_t offset, std::uint32_t length) {
        Slice s;
        s.bits_ = (offset & kOffsetMask) | (static_cast<std::uint64_t>(length) << kLengthShift);
        return s;
    }

    bool null() const { return (bits_ & kNullBit) != 0; }
    std::uint64_t offset() const { return bits_ & kOffsetMask; }
    std::uint32_t length() const {
        return static_cast<std::uint32_t>((bits_ >> kLengthShift) & kMaxLength);
    }

  private:
    static constexpr std::uint64_t kOffsetMask = (1ULL << 40) - 1;
    static constexpr unsigned kLengthShift = 40;
    static constexpr std::uint64_t kNullBit = 1ULL << 63;

    std::uint64_t bits_ = kNullBit;   // a default-built Slice is null
};

// One column of one file, decoded as far as it is useful to decode it.
//
// A dictionary column keeps `dict` and `index`: `index[row]` selects a value,
// or is kNull. A plain column keeps `values`, one Slice per row. The comparison
// reads whichever is populated, and the dictionary form is the one worth having.
struct Column {
    static constexpr std::int32_t kNull = -1;

    bool dictionary = false;
    std::vector<Slice> dict;            // dictionary form: the distinct values
    std::vector<std::int32_t> index;    // dictionary form: one per row
    std::vector<Slice> values;          // plain form: one per row, null where null
    // Decompressed pages, when the column was compressed. A column is either
    // wholly compressed or wholly not -- the reader refuses anything else --
    // so this one field says where every Slice in the column points: into
    // `owned` when it is non-empty, into the mapping when it is not.
    std::string owned;

    std::size_t rows() const { return dictionary ? index.size() : values.size(); }
};

// What a file says about itself, before any column is read.
struct Meta {
    std::vector<std::string> names;   // leaf columns, in file order
    std::int64_t rows = 0;
    std::size_t row_groups = 0;
};

// Reads the footer. Throws std::runtime_error naming the problem if the file is
// not Parquet or uses something this reader does not implement.
Meta read_meta(const char* data, std::size_t size, const std::string& path);

// Reads one column across every row group. `which` indexes into `Meta::names`.
Column read_column(const char* data, std::size_t size, std::size_t which,
                   const std::string& path);

}  // namespace csvdiff::parquet
