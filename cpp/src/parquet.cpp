#include "parquet.hpp"

#include <cstring>
#include <stdexcept>

namespace csvdiff::parquet {
namespace {

[[noreturn]] void fail(const std::string& why, const std::string& path) {
    throw std::runtime_error(why + ": " + path);
}

// ---------------------------------------------------------------------------
// Thrift compact protocol
//
// Parquet's metadata and every page header are Thrift compact structs. The
// format is small enough to read directly: a field header carries a delta from
// the previous field id and a type in one byte, integers are zigzag varints,
// and a struct ends with a zero byte.
// ---------------------------------------------------------------------------

enum : int { kStop = 0, kTrue = 1, kFalse = 2, kI8 = 3, kI16 = 4, kI32 = 5, kI64 = 6,
             kDouble = 7, kBinary = 8, kList = 9, kSet = 10, kMap = 11, kStruct = 12 };

class Thrift {
  public:
    Thrift(const char* data, std::size_t size, const std::string& path)
        : d_(data), n_(size), path_(path) {}

    std::size_t at() const { return at_; }
    void seek(std::size_t to) { at_ = to; }

    std::uint8_t byte() {
        if (at_ >= n_) fail("parquet metadata ends early", path_);
        return static_cast<std::uint8_t>(d_[at_++]);
    }

    std::uint64_t varint() {
        std::uint64_t out = 0;
        int shift = 0;
        for (;;) {
            const std::uint8_t b = byte();
            out |= static_cast<std::uint64_t>(b & 0x7F) << shift;
            if ((b & 0x80) == 0) return out;
            shift += 7;
            if (shift > 63) fail("a varint in the metadata is malformed", path_);
        }
    }

    std::int64_t zigzag() {
        const std::uint64_t v = varint();
        return static_cast<std::int64_t>((v >> 1) ^ (~(v & 1) + 1));
    }

    std::string_view binary() {
        const std::size_t len = static_cast<std::size_t>(varint());
        if (at_ + len > n_) fail("a string in the metadata runs past the end", path_);
        const std::string_view out(d_ + at_, len);
        at_ += len;
        return out;
    }

    // Reads a field header. Returns the type, or kStop at the end of a struct;
    // `id` is set to the field number.
    int field(int16_t& id, int16_t& last) {
        const std::uint8_t h = byte();
        if (h == 0) return kStop;
        const int type = h & 0x0F;
        const int delta = (h & 0xF0) >> 4;
        id = delta == 0 ? static_cast<int16_t>(zigzag()) : static_cast<int16_t>(last + delta);
        last = id;
        return type;
    }

    // Reads a list header, returning the element type and setting `count`.
    int list(std::uint32_t& count) {
        const std::uint8_t h = byte();
        count = (h & 0xF0) >> 4;
        if (count == 15) count = static_cast<std::uint32_t>(varint());
        return h & 0x0F;
    }

    // Steps over a value of `type` without interpreting it, so a struct can be
    // read for the few fields that matter and the rest skipped.
    void skip(int type) {
        switch (type) {
            case kTrue: case kFalse: return;
            case kI8: byte(); return;
            case kI16: case kI32: case kI64: zigzag(); return;
            case kDouble: at_ += 8; return;
            case kBinary: binary(); return;
            case kList: case kSet: {
                std::uint32_t count = 0;
                const int elem = list(count);
                for (std::uint32_t i = 0; i < count; ++i) skip(elem);
                return;
            }
            case kMap: {
                std::uint32_t count = static_cast<std::uint32_t>(varint());
                if (count == 0) return;
                const std::uint8_t kv = byte();
                for (std::uint32_t i = 0; i < count; ++i) {
                    skip((kv & 0xF0) >> 4);
                    skip(kv & 0x0F);
                }
                return;
            }
            case kStruct: {
                int16_t id = 0, last = 0;
                for (;;) {
                    const int t = field(id, last);
                    if (t == kStop) return;
                    skip(t);
                }
            }
            default: fail("unknown type in the metadata", path_);
        }
    }

  private:
    const char* d_;
    std::size_t n_;
    std::string path_;
    std::size_t at_ = 0;
};

// ---------------------------------------------------------------------------
// The slices of the file metadata this reader uses
// ---------------------------------------------------------------------------

// parquet.thrift Type
enum : int { kBoolean = 0, kInt32 = 1, kInt64 = 2, kInt96 = 3, kFloat = 4, kDouble_ = 5,
             kByteArray = 6, kFixedLen = 7 };
// parquet.thrift Encoding
enum : int { kPlain = 0, kPlainDictionary = 2, kRle = 3, kRleDictionary = 8 };
// parquet.thrift CompressionCodec
enum : int { kUncompressed = 0, kSnappy = 1 };
// parquet.thrift PageType
enum : int { kDataPage = 0, kIndexPage = 1, kDictionaryPage = 2, kDataPageV2 = 3 };

struct ChunkMeta {
    int type = -1;
    int codec = kUncompressed;
    std::int64_t num_values = 0;
    std::int64_t data_page_offset = 0;
    std::int64_t dictionary_page_offset = 0;
    std::int64_t total_compressed_size = 0;
    std::int64_t total_uncompressed_size = 0;
    std::string name;
};

struct RowGroupMeta {
    std::vector<ChunkMeta> columns;
    std::int64_t rows = 0;
};

struct FileMeta {
    std::vector<std::string> names;    // leaf names, in order
    std::vector<int> optional;         // 1 where the column may be null
    std::vector<RowGroupMeta> groups;
    std::int64_t rows = 0;
};

void read_column_meta(Thrift& t, ChunkMeta& out, const std::string& path) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = t.field(id, last);
        if (type == kStop) return;
        switch (id) {
            case 1: out.type = static_cast<int>(t.zigzag()); break;          // type
            case 2: {                                                        // encodings
                std::uint32_t n = 0;
                const int elem = t.list(n);
                for (std::uint32_t i = 0; i < n; ++i) t.skip(elem);
                break;
            }
            case 3: {                                                        // path_in_schema
                std::uint32_t n = 0;
                const int elem = t.list(n);
                for (std::uint32_t i = 0; i < n; ++i) {
                    const std::string_view part = elem == kBinary ? t.binary() : std::string_view{};
                    if (elem != kBinary) t.skip(elem);
                    if (out.name.empty()) out.name = std::string(part);
                }
                break;
            }
            case 4: out.codec = static_cast<int>(t.zigzag()); break;
            case 5: out.num_values = t.zigzag(); break;
            case 6: out.total_uncompressed_size = t.zigzag(); break;
            case 7: out.total_compressed_size = t.zigzag(); break;
            case 9: out.data_page_offset = t.zigzag(); break;
            case 11: out.dictionary_page_offset = t.zigzag(); break;
            default: t.skip(type); break;
        }
    }
    (void)path;
}

void read_chunk(Thrift& t, ChunkMeta& out, const std::string& path) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = t.field(id, last);
        if (type == kStop) return;
        if (id == 3) {  // meta_data
            read_column_meta(t, out, path);
        } else {
            t.skip(type);
        }
    }
}

void read_row_group(Thrift& t, RowGroupMeta& out, const std::string& path) {
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = t.field(id, last);
        if (type == kStop) return;
        if (id == 1) {  // columns
            std::uint32_t n = 0;
            t.list(n);
            out.columns.resize(n);
            for (std::uint32_t i = 0; i < n; ++i) read_chunk(t, out.columns[i], path);
        } else if (id == 3) {  // num_rows
            out.rows = t.zigzag();
        } else {
            t.skip(type);
        }
    }
}

FileMeta read_file_meta(const char* data, std::size_t size, const std::string& path) {
    if (size < 12 || std::memcmp(data, "PAR1", 4) != 0 ||
        std::memcmp(data + size - 4, "PAR1", 4) != 0) {
        fail("not a parquet file", path);
    }
    std::uint32_t meta_len = 0;
    std::memcpy(&meta_len, data + size - 8, 4);
    if (meta_len + 8u > size) fail("the parquet footer is longer than the file", path);
    Thrift t(data + size - 8 - meta_len, meta_len, path);

    FileMeta out;
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = t.field(id, last);
        if (type == kStop) break;
        switch (id) {
            case 2: {  // schema
                std::uint32_t n = 0;
                t.list(n);
                for (std::uint32_t i = 0; i < n; ++i) {
                    // SchemaElement: name is 4, repetition_type 3, num_children 5.
                    int16_t sid = 0, slast = 0;
                    std::string name;
                    int repetition = -1, children = 0;
                    for (;;) {
                        const int st = t.field(sid, slast);
                        if (st == kStop) break;
                        if (sid == 3) repetition = static_cast<int>(t.zigzag());
                        else if (sid == 4) name = std::string(t.binary());
                        else if (sid == 5) children = static_cast<int>(t.zigzag());
                        else t.skip(st);
                    }
                    // The first element is the root, and anything with children
                    // is a group rather than a column this reader can read.
                    if (i == 0) continue;
                    if (children > 0) fail("nested parquet columns are not read here", path);
                    out.names.push_back(name);
                    out.optional.push_back(repetition == 1 ? 1 : 0);  // 1 = OPTIONAL
                }
                break;
            }
            case 3: out.rows = t.zigzag(); break;
            case 4: {  // row_groups
                std::uint32_t n = 0;
                t.list(n);
                out.groups.resize(n);
                for (std::uint32_t i = 0; i < n; ++i) read_row_group(t, out.groups[i], path);
                break;
            }
            default: t.skip(type); break;
        }
    }
    if (out.names.empty()) fail("the parquet schema has no columns", path);
    return out;
}

// ---------------------------------------------------------------------------
// Page headers and page data
// ---------------------------------------------------------------------------

struct PageHead {
    int type = -1;
    std::int32_t uncompressed = 0;
    std::int32_t compressed = 0;
    std::int32_t num_values = 0;
    int encoding = -1;
    int def_encoding = kRle;
    std::size_t after = 0;  // offset of the page body
};

PageHead read_page_head(const char* data, std::size_t size, std::size_t at,
                        const std::string& path) {
    Thrift t(data, size, path);
    t.seek(at);
    PageHead out;
    int16_t id = 0, last = 0;
    for (;;) {
        const int type = t.field(id, last);
        if (type == kStop) break;
        switch (id) {
            case 1: out.type = static_cast<int>(t.zigzag()); break;
            case 2: out.uncompressed = static_cast<std::int32_t>(t.zigzag()); break;
            case 3: out.compressed = static_cast<std::int32_t>(t.zigzag()); break;
            case 5: {  // data_page_header
                int16_t hid = 0, hlast = 0;
                for (;;) {
                    const int ht = t.field(hid, hlast);
                    if (ht == kStop) break;
                    if (hid == 1) out.num_values = static_cast<std::int32_t>(t.zigzag());
                    else if (hid == 2) out.encoding = static_cast<int>(t.zigzag());
                    else if (hid == 3) out.def_encoding = static_cast<int>(t.zigzag());
                    else t.skip(ht);
                }
                break;
            }
            case 7: {  // dictionary_page_header
                int16_t hid = 0, hlast = 0;
                for (;;) {
                    const int ht = t.field(hid, hlast);
                    if (ht == kStop) break;
                    if (hid == 1) out.num_values = static_cast<std::int32_t>(t.zigzag());
                    else if (hid == 2) out.encoding = static_cast<int>(t.zigzag());
                    else t.skip(ht);
                }
                break;
            }
            case 8: fail("parquet data page v2 is not read here", path);
            default: t.skip(type); break;
        }
    }
    out.after = t.at();
    return out;
}

// Snappy raw block format: a varint of the uncompressed length, then a stream of
// literal and copy elements. Written out here rather than linked because it is
// sixty lines and the alternative is a dependency.
//
// It appends to `out` and writes through a raw pointer rather than push_back,
// and a back-reference is copied eight bytes at a time where the distance
// allows it. Both matter more than they look: this is the one loop that touches
// every byte of a compressed file, and a byte-at-a-time version of it was the
// single largest cost in reading a ten-million-row column.
bool snappy_append(const char* in, std::size_t n, std::string& out) {
    std::size_t at = 0;
    std::uint64_t want = 0;
    int shift = 0;
    for (;;) {
        if (at >= n) return false;
        const std::uint8_t b = static_cast<std::uint8_t>(in[at++]);
        want |= static_cast<std::uint64_t>(b & 0x7F) << shift;
        if ((b & 0x80) == 0) break;
        shift += 7;
        if (shift > 63) return false;
    }

    const std::size_t base = out.size();
    out.resize(base + want);
    char* const begin = out.data() + base;
    char* dst = begin;
    char* const end = begin + want;

    while (at < n) {
        const std::uint8_t tag = static_cast<std::uint8_t>(in[at++]);
        if ((tag & 0x03) == 0) {  // literal
            std::size_t len = (tag >> 2) + 1;
            if (len > 60) {
                const std::size_t extra = len - 60;
                if (at + extra > n) return false;
                std::size_t v = 0;
                for (std::size_t i = 0; i < extra; ++i)
                    v |= static_cast<std::size_t>(static_cast<std::uint8_t>(in[at + i])) << (8 * i);
                at += extra;
                len = v + 1;
            }
            if (at + len > n || len > static_cast<std::size_t>(end - dst)) return false;
            std::memcpy(dst, in + at, len);
            dst += len;
            at += len;
            continue;
        }
        std::size_t len = 0, offset = 0;
        if ((tag & 0x03) == 1) {
            len = ((tag >> 2) & 0x07) + 4;
            if (at >= n) return false;
            offset = ((static_cast<std::size_t>(tag) >> 5) << 8) |
                     static_cast<std::uint8_t>(in[at++]);
        } else if ((tag & 0x03) == 2) {
            if (at + 2 > n) return false;
            len = (tag >> 2) + 1;
            offset = static_cast<std::uint8_t>(in[at]) |
                     (static_cast<std::size_t>(static_cast<std::uint8_t>(in[at + 1])) << 8);
            at += 2;
        } else {
            if (at + 4 > n) return false;
            len = (tag >> 2) + 1;
            offset = 0;
            for (int i = 0; i < 4; ++i)
                offset |= static_cast<std::size_t>(static_cast<std::uint8_t>(in[at + i])) << (8 * i);
            at += 4;
        }
        if (offset == 0 || offset > static_cast<std::size_t>(dst - begin)) return false;
        if (len > static_cast<std::size_t>(end - dst)) return false;
        const char* src = dst - offset;
        if (offset >= 8) {
            // The source of the next eight bytes is entirely behind the
            // destination, so whole words can be moved.
            std::size_t i = 0;
            for (; i + 8 <= len; i += 8) std::memcpy(dst + i, src + i, 8);
            for (; i < len; ++i) dst[i] = src[i];
        } else {
            // A short distance means the copy repeats a pattern it is still
            // writing, so it has to go a byte at a time.
            for (std::size_t i = 0; i < len; ++i) dst[i] = src[i];
        }
        dst += len;
    }
    return dst == end;
}

// RLE / bit-packed hybrid, which is how definition levels and dictionary
// indices are written. A run header is a varint: the low bit says which kind,
// the rest is the length.
class RleReader {
  public:
    RleReader(const char* d, std::size_t n, int width)
        : d_(d), n_(n), width_(width),
          mask_(width >= 64 ? ~0ULL : (1ULL << width) - 1) {}

    // Fills `want` values. Bulk rather than one at a time on purpose: an RLE
    // run becomes a fill, and a bit-packed run becomes one 64-bit load, one
    // shift and one mask per value.
    //
    // Values are packed end to end with no padding, so a value straddles bytes
    // more often than not. Reading eight bytes around it and shifting picks any
    // of them out without a loop -- the same "look at eight bytes at once"
    // trick the CSV scanner uses to find a delimiter, here reading rather than
    // searching. It is exact for every width Parquet allows: seven bits of
    // misalignment plus thirty-two of value still fits in a word.
    bool fill(std::int32_t* out, std::size_t want) {
        std::size_t done = 0;
        while (done < want) {
            if (left_ == 0 && !header()) return false;
            const std::size_t take = std::min(want - done, left_);
            if (!packed_) {
                std::fill_n(out + done, take, value_);
            } else if (width_ == 0) {
                std::fill_n(out + done, take, 0);
            } else {
                // Split once per run rather than tested once per value: the
                // values whose eight-byte window lies inside the buffer, which
                // is all but the last few of a page, then the tail a byte at a
                // time. The Rust port does the same.
                const std::size_t width = static_cast<std::size_t>(width_);
                std::size_t bit = bit_;
                std::size_t safe = 0;
                if (n_ >= 8) {
                    const std::size_t last = (n_ - 8) * 8 + 7;  // last bit a whole window starts at
                    if (bit <= last) safe = std::min(take, (last - bit) / width + 1);
                }
                std::int32_t* o = out + done;
                for (std::size_t i = 0; i < safe; ++i) {
                    std::uint64_t w;
                    std::memcpy(&w, d_ + (bit >> 3), 8);
                    o[i] = static_cast<std::int32_t>((w >> (bit & 7)) & mask_);
                    bit += width;
                }
                for (std::size_t i = safe; i < take; ++i) {
                    const std::size_t byte = bit >> 3;
                    std::uint64_t w = 0;
                    for (std::size_t k = 0; k < 8 && byte + k < n_; ++k)
                        w |= static_cast<std::uint64_t>(static_cast<std::uint8_t>(d_[byte + k]))
                             << (8 * k);
                    o[i] = static_cast<std::int32_t>((w >> (bit & 7)) & mask_);
                    bit += width;
                }
                bit_ = bit;
            }
            left_ -= take;
            done += take;
        }
        return true;
    }

  private:
    bool header() {
        std::uint64_t h = 0;
        int shift = 0;
        for (;;) {
            if (at_ >= n_) return false;
            const std::uint8_t b = static_cast<std::uint8_t>(d_[at_++]);
            h |= static_cast<std::uint64_t>(b & 0x7F) << shift;
            if ((b & 0x80) == 0) break;
            shift += 7;
            if (shift > 63) return false;
        }
        if ((h & 1) == 0) {  // RLE run: a count and one value
            packed_ = false;
            left_ = static_cast<std::size_t>(h >> 1);
            const std::size_t bytes = static_cast<std::size_t>((width_ + 7) / 8);
            value_ = 0;
            if (at_ + bytes > n_) return false;
            for (std::size_t i = 0; i < bytes; ++i)
                value_ |= static_cast<std::int32_t>(static_cast<std::uint8_t>(d_[at_ + i]))
                          << (8 * i);
            at_ += bytes;
        } else {  // bit-packed run, in groups of eight
            packed_ = true;
            const std::size_t groups = static_cast<std::size_t>(h >> 1);
            left_ = groups * 8;
            bit_ = at_ * 8;
            // A group of eight values is exactly `width` bytes, so the whole
            // run's length is known and `at_` can jump straight to the next
            // header while `bit_` walks inside it.
            const std::size_t run = groups * static_cast<std::size_t>(width_);
            at_ = at_ + run > n_ ? n_ : at_ + run;
        }
        return left_ > 0;
    }

    const char* d_;
    std::size_t n_;
    int width_;
    std::uint64_t mask_;
    std::size_t at_ = 0;
    std::size_t left_ = 0;
    std::size_t bit_ = 0;
    bool packed_ = false;
    std::int32_t value_ = 0;
};

// PLAIN byte arrays: a four-byte little-endian length, then the bytes, repeated.
// The slices point straight at `base`, so nothing is copied.
void plain_slices(const char* page, std::size_t n, std::uint64_t base, std::int32_t count,
                  std::vector<Slice>& out, const std::string& path) {
    std::size_t at = 0;
    for (std::int32_t i = 0; i < count; ++i) {
        if (at + 4 > n) fail("a parquet page ends inside a value", path);
        std::uint32_t len = 0;
        std::memcpy(&len, page + at, 4);
        at += 4;
        if (at + len > n) fail("a parquet value runs past its page", path);
        if (len > Slice::kMaxLength)
            fail("a parquet value is larger than eight megabytes", path);
        out.push_back(Slice::at(base + at, len));
        at += len;
    }
}

}  // namespace

Meta read_meta(const char* data, std::size_t size, const std::string& path) {
    const FileMeta fm = read_file_meta(data, size, path);
    Meta out;
    out.names = fm.names;
    out.rows = fm.rows;
    out.row_groups = fm.groups.size();
    return out;
}

Column read_column(const char* data, std::size_t size, std::size_t which,
                   const std::string& path) {
    const FileMeta fm = read_file_meta(data, size, path);
    if (which >= fm.names.size()) fail("no such column in the parquet file", path);
    const bool optional = fm.optional[which] != 0;

    Column out;
    // Every chunk has to agree about compression, because a Slice is an offset
    // with no room to say what it is an offset *into*. Uncompressed pages are
    // read in place and their offsets are into the mapping; compressed ones are
    // expanded into `owned` and their offsets are into that. One column, one
    // base -- `owned.empty()` says which.
    int codec = -1;
    std::int64_t uncompressed = 0;
    for (const RowGroupMeta& rg : fm.groups) {
        if (which >= rg.columns.size()) fail("a row group is missing a column", path);
        uncompressed += rg.columns[which].total_uncompressed_size;
    }

    // The column starts out held as dictionary indices and stays that way only
    // if every page cooperates. A writer that gives up on the dictionary
    // partway -- DuckDB does, once a column's distinct values outgrow its
    // budget, which at ten million rows is most of them -- forces the whole
    // column into the plain form. Expanding what has been read costs no bytes:
    // a dictionary entry is already a slice, so this copies eight-byte handles,
    // not values.
    // Reused across pages so the per-page work allocates nothing, and sized
    // once from the footer's row count so appending a page never has to move
    // eighty megabytes of what is already decoded.
    std::vector<std::int32_t> defs, idx;
    std::vector<Slice> got;
    out.index.reserve(static_cast<std::size_t>(fm.rows));

    bool dictionary = true;
    const auto degrade = [&] {
        if (!dictionary) return;
        out.values.reserve(static_cast<std::size_t>(fm.rows));
        for (std::int32_t k : out.index)
            out.values.push_back(k >= 0 ? out.dict[static_cast<std::size_t>(k)] : Slice{});
        std::vector<std::int32_t>().swap(out.index);
        dictionary = false;
    };

    for (const RowGroupMeta& rg : fm.groups) {
        const ChunkMeta& c = rg.columns[which];
        if (c.type != kByteArray) fail("only BYTE_ARRAY parquet columns are read here", path);
        if (c.codec != kUncompressed && c.codec != kSnappy)
            fail("only uncompressed and snappy parquet are read here", path);
        if (codec < 0) {
            codec = c.codec;
            if (codec == kSnappy)
                out.owned.reserve(static_cast<std::size_t>(uncompressed));
        } else if (codec != c.codec) {
            fail("a parquet column changes compression between row groups", path);
        }

        std::size_t at = static_cast<std::size_t>(c.dictionary_page_offset > 0
                                                      ? c.dictionary_page_offset
                                                      : c.data_page_offset);
        const std::size_t stop = at + static_cast<std::size_t>(c.total_compressed_size);
        std::int64_t seen = 0;
        const std::size_t dict_base = out.dict.size();

        while (at < stop && at < size && seen < c.num_values) {
            const PageHead h = read_page_head(data, size, at, path);
            const std::size_t body_len = static_cast<std::size_t>(h.compressed);
            if (h.after + body_len > size) fail("a parquet page runs past the file", path);

            const char* page = data + h.after;
            std::size_t page_len = body_len;
            std::uint64_t page_base = h.after;   // offsets are into the mapping
            if (c.codec == kSnappy) {
                const std::size_t was = out.owned.size();
                if (!snappy_append(data + h.after, body_len, out.owned))
                    fail("a snappy page in the parquet file will not decompress", path);
                page = out.owned.data() + was;
                page_len = out.owned.size() - was;
                page_base = was;                 // offsets are into `owned`
            }

            if (h.type == kDictionaryPage) {
                if (h.encoding != kPlain && h.encoding != kPlainDictionary)
                    fail("only PLAIN parquet dictionaries are read here", path);
                plain_slices(page, page_len, page_base, h.num_values, out.dict, path);
            } else if (h.type == kDataPage) {
                const std::size_t n_vals = static_cast<std::size_t>(h.num_values);
                std::size_t vat = 0;
                std::size_t real = n_vals;
                if (optional) {
                    // Definition levels: RLE, four-byte length prefix in v1.
                    if (h.def_encoding != kRle) fail("parquet definition levels are not RLE", path);
                    if (page_len < 4) fail("a parquet page has no definition levels", path);
                    std::uint32_t dl = 0;
                    std::memcpy(&dl, page, 4);
                    if (4 + static_cast<std::size_t>(dl) > page_len)
                        fail("a parquet page ends inside its levels", path);
                    RleReader r(page + 4, dl, 1);
                    defs.resize(n_vals);
                    if (!r.fill(defs.data(), n_vals))
                        fail("a parquet page ran out of definition levels", path);
                    real = 0;
                    for (std::size_t i = 0; i < n_vals; ++i) real += defs[i] ? 1 : 0;
                    vat = 4 + dl;
                }
                if (vat > page_len) fail("a parquet page ends inside its levels", path);

                if (h.encoding == kPlainDictionary || h.encoding == kRleDictionary) {
                    if (out.dict.empty())
                        fail("a parquet data page wants a dictionary there is none of", path);
                    if (vat + 1 > page_len) fail("a parquet page has no bit width", path);
                    const int width = static_cast<int>(static_cast<std::uint8_t>(page[vat]));
                    if (width > 32) fail("a parquet dictionary index is wider than 32 bits", path);
                    RleReader r(page + vat + 1, page_len - vat - 1, width);
                    idx.resize(real);
                    if (!r.fill(idx.data(), real))
                        fail("a parquet page ran out of dictionary indices", path);
                    for (std::size_t i = 0; i < real; ++i) {
                        const std::size_t k = dict_base + static_cast<std::size_t>(idx[i]);
                        if (k >= out.dict.size())
                            fail("a parquet dictionary index is out of range", path);
                        idx[i] = static_cast<std::int32_t>(k);
                    }
                    if (dictionary && real == n_vals) {
                        // Every value present and the column still indices: the
                        // page's indices are the column's, as they are -- one
                        // copy rather than a push_back and two branches per
                        // value. Every page of a REQUIRED column takes this,
                        // and most of an OPTIONAL one.
                        out.index.insert(out.index.end(), idx.begin(),
                                         idx.begin() + static_cast<std::ptrdiff_t>(real));
                    } else {
                        std::size_t k = 0;
                        for (std::size_t i = 0; i < n_vals; ++i) {
                            const bool here = !optional || defs[i];
                            if (dictionary)
                                out.index.push_back(here ? idx[k++] : Column::kNull);
                            else
                                out.values.push_back(
                                    here ? out.dict[static_cast<std::size_t>(idx[k++])] : Slice{});
                        }
                    }
                } else if (h.encoding == kPlain) {
                    degrade();
                    got.clear();
                    got.reserve(real);
                    plain_slices(page + vat, page_len - vat, page_base + vat,
                                 static_cast<std::int32_t>(real), got, path);
                    if (real == n_vals) {
                        out.values.insert(out.values.end(), got.begin(), got.end());
                    } else {
                        std::size_t k = 0;
                        for (std::size_t i = 0; i < n_vals; ++i)
                            out.values.push_back(defs[i] ? got[k++] : Slice{});
                    }
                } else {
                    fail("only PLAIN and dictionary parquet encodings are read here", path);
                }
                seen += h.num_values;
            } else if (h.type != kIndexPage) {
                fail("unknown parquet page type", path);
            }
            at = h.after + body_len;
        }
    }
    out.dictionary = dictionary;
    // A column that never met a page at all is plain and empty, not dictionary.
    if (dictionary && out.index.empty() && out.dict.empty()) out.dictionary = false;
    return out;
}

}  // namespace csvdiff::parquet
