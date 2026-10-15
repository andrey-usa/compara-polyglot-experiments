#include "pq_write.hpp"

#include <fcntl.h>
#include "../src/win32.hpp"   // O_BINARY: a no-op off Windows
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cstring>
#include <thread>
#include <unordered_map>

namespace pqwrite {
namespace {

// parquet.thrift, the handful of values this writer emits.
enum : int { kTypeByteArray = 6 };
enum : int { kPlain = 0, kPlainDictionary = 2, kRle = 3, kRleDictionary = 8 };
enum : int { kUncompressed = 0, kSnappy = 1 };
enum : int { kDataPage = 0, kDictionaryPage = 2 };
enum : int { kRequired = 0, kOptional = 1 };
enum : int { kUtf8 = 0 };

// Thrift compact protocol field types.
enum : int { kStop = 0, kI32 = 5, kI64 = 6, kBinary = 8, kList = 9, kStruct = 12 };

void put_varint(std::string& out, std::uint64_t v) {
    while (v >= 0x80) {
        out.push_back(static_cast<char>((v & 0x7F) | 0x80));
        v >>= 7;
    }
    out.push_back(static_cast<char>(v));
}

std::uint64_t zigzag(std::int64_t v) {
    return (static_cast<std::uint64_t>(v) << 1) ^ static_cast<std::uint64_t>(v >> 63);
}

// The mirror of the reader's Thrift class. A field header is a delta from the
// last field id in the same struct, so nesting has to save and restore that
// delta -- which is what `Guard` is for.
class Enc {
  public:
    explicit Enc(std::string& out) : out_(out) {}

    void field(int id, int type) {
        const int delta = id - last_;
        if (delta > 0 && delta <= 15) {
            out_.push_back(static_cast<char>((delta << 4) | type));
        } else {
            out_.push_back(static_cast<char>(type));
            put_varint(out_, zigzag(id));
        }
        last_ = id;
    }
    void stop() { out_.push_back(0); }

    void i32(int id, std::int32_t v) { field(id, kI32); put_varint(out_, zigzag(v)); }
    void i64(int id, std::int64_t v) { field(id, kI64); put_varint(out_, zigzag(v)); }
    void str(int id, std::string_view v) {
        field(id, kBinary);
        put_varint(out_, v.size());
        out_.append(v);
    }
    // A list of i32s, which is what every enum list in this metadata is.
    void enums(int id, const std::vector<int>& v) {
        field(id, kList);
        header(v.size(), kI32);
        for (int x : v) put_varint(out_, zigzag(x));
    }
    void strings(int id, const std::vector<std::string>& v) {
        field(id, kList);
        header(v.size(), kBinary);
        for (const auto& s : v) {
            put_varint(out_, s.size());
            out_.append(s);
        }
    }
    // Opens a list of structs whose encoded bodies the caller appends itself.
    void struct_list(int id, std::size_t count) {
        field(id, kList);
        header(count, kStruct);
    }

    // Enters a nested struct: field ids inside start from zero again.
    class Guard {
      public:
        explicit Guard(Enc& e) : e_(e), saved_(e.last_) { e_.last_ = 0; }
        ~Guard() {
            e_.stop();
            e_.last_ = saved_;
        }

      private:
        Enc& e_;
        int saved_;
    };
    Guard nested(int id) {
        field(id, kStruct);
        return Guard(*this);
    }

  private:
    void header(std::size_t count, int elem) {
        if (count < 15) {
            out_.push_back(static_cast<char>((count << 4) | elem));
        } else {
            out_.push_back(static_cast<char>(0xF0 | elem));
            put_varint(out_, count);
        }
    }

    std::string& out_;
    int last_ = 0;
};

// ---------------------------------------------------------------------------
// Snappy
// ---------------------------------------------------------------------------

// A plain LZ77 over a four-byte hash table -- the same shape as the reference
// compressor, without its assembly-level care. It only has to produce a stream
// the reader (and DuckDB, and anything else) accepts, at a ratio close enough
// that a file size in a benchmark table means something.
std::uint32_t load32(const char* p) {
    std::uint32_t v;
    std::memcpy(&v, p, 4);
    return v;
}

void emit_literal(std::string& out, const char* p, std::size_t len) {
    if (len == 0) return;
    const std::size_t n = len - 1;
    if (n < 60) {
        out.push_back(static_cast<char>(n << 2));
    } else {
        int bytes = 1;
        for (std::size_t v = n; v >= 256; v >>= 8) ++bytes;
        out.push_back(static_cast<char>((59 + bytes) << 2));
        for (int i = 0; i < bytes; ++i) out.push_back(static_cast<char>((n >> (8 * i)) & 0xFF));
    }
    out.append(p, len);
}

void emit_copy(std::string& out, std::size_t offset, std::size_t len) {
    while (len > 0) {
        const std::size_t take = std::min<std::size_t>(len, 64);
        if (take >= 4 && take <= 11 && offset < 2048) {
            out.push_back(static_cast<char>(((offset >> 8) << 5) | ((take - 4) << 2) | 1));
            out.push_back(static_cast<char>(offset & 0xFF));
        } else {
            out.push_back(static_cast<char>(((take - 1) << 2) | 2));
            out.push_back(static_cast<char>(offset & 0xFF));
            out.push_back(static_cast<char>((offset >> 8) & 0xFF));
        }
        len -= take;
    }
}

void snappy_compress(std::string_view in, std::string& out) {
    out.clear();
    put_varint(out, in.size());
    const char* d = in.data();
    const std::size_t n = in.size();
    if (n < 16) {
        emit_literal(out, d, n);
        return;
    }
    constexpr int kLog = 15;
    std::vector<std::uint32_t> table(1u << kLog, 0);   // 0 empty, else position + 1
    std::size_t ip = 0, emitted = 0;
    while (ip + 4 <= n) {
        const std::uint32_t h = (load32(d + ip) * 0x1e35a7bdu) >> (32 - kLog);
        const std::uint32_t seen = table[h];
        table[h] = static_cast<std::uint32_t>(ip) + 1;
        if (seen == 0) {
            ++ip;
            continue;
        }
        const std::size_t match = seen - 1;
        const std::size_t offset = ip - match;
        if (offset == 0 || offset > 65535 || load32(d + match) != load32(d + ip)) {
            ++ip;
            continue;
        }
        std::size_t len = 4;
        while (ip + len < n && d[match + len] == d[ip + len]) ++len;
        emit_literal(out, d + emitted, ip - emitted);
        emit_copy(out, offset, len);
        ip += len;
        emitted = ip;
    }
    emit_literal(out, d + emitted, n - emitted);
}

// ---------------------------------------------------------------------------
// RLE / bit-packed hybrid
// ---------------------------------------------------------------------------

// Values are packed least-significant-bit first and end to end, which is how
// the reader picks one out with a single 64-bit load and a shift.
class BitPack {
  public:
    explicit BitPack(std::string& out) : out_(out) {}
    void put(std::uint32_t v, int width) {
        acc_ |= static_cast<std::uint64_t>(v) << bits_;
        bits_ += width;
        while (bits_ >= 8) {
            out_.push_back(static_cast<char>(acc_ & 0xFF));
            acc_ >>= 8;
            bits_ -= 8;
        }
    }
    void flush() {
        if (bits_ > 0) {
            out_.push_back(static_cast<char>(acc_ & 0xFF));
            acc_ = 0;
            bits_ = 0;
        }
    }

  private:
    std::string& out_;
    std::uint64_t acc_ = 0;
    int bits_ = 0;
};

// A run of eight or more equal values is worth an RLE run; anything shorter is
// bit-packed in groups of eight, padded with zeroes, which the reader never
// looks at because it stops at the value count.
void rle_hybrid(std::string& out, const std::int32_t* v, std::size_t n, int width) {
    const std::size_t bytes = static_cast<std::size_t>((width + 7) / 8);
    std::size_t i = 0;
    while (i < n) {
        std::size_t j = i;
        while (j < n && v[j] == v[i]) ++j;
        if (j - i >= 8) {
            put_varint(out, ((j - i) << 1) | 0);
            for (std::size_t k = 0; k < bytes; ++k)
                out.push_back(static_cast<char>((static_cast<std::uint32_t>(v[i]) >> (8 * k)) & 0xFF));
            i = j;
            continue;
        }
        // Literals up to the next run long enough to be worth encoding -- but a
        // bit-packed run holds a whole number of groups of eight, and a short
        // one anywhere but at the very end would leave the reader taking its
        // padding for data and every value after it shifted. So the run only
        // ends on a group boundary: if a repeat starts off one, just enough of
        // it is taken as literals to land on the next.
        std::size_t k = i;
        while (k < n) {
            std::size_t m = k;
            while (m < n && v[m] == v[k]) ++m;
            if (m - k >= 8) {
                const std::size_t pad = (8 - (k - i) % 8) % 8;
                if (pad == 0) break;
                k = std::min(k + pad, n);
                break;
            }
            k = m;
        }
        const std::size_t count = k - i;
        const std::size_t groups = (count + 7) / 8;
        put_varint(out, (groups << 1) | 1);
        BitPack pack(out);
        for (std::size_t x = 0; x < groups * 8; ++x)
            pack.put(x < count ? static_cast<std::uint32_t>(v[i + x]) : 0, width);
        pack.flush();
        i = k;
    }
}

int width_for(std::size_t distinct) {
    int w = 1;
    while (distinct > (1u << w)) ++w;
    return std::min(w, 32);
}

}  // namespace

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

// One column's values for the row group being built. Values are copied into an
// arena and referred to by offset, so the column costs one allocation that grows
// rather than one per cell.
struct Writer::Column {
    static constexpr std::uint32_t kNull = ~0u;

    std::string arena;
    std::vector<std::pair<std::uint32_t, std::uint32_t>> spans;  // offset, length

    void add(Value v) {
        if (v.null) {
            spans.emplace_back(kNull, 0);
            return;
        }
        spans.emplace_back(static_cast<std::uint32_t>(arena.size()),
                           static_cast<std::uint32_t>(v.text.size()));
        arena.append(v.text);
    }
    void clear() {
        arena.clear();
        spans.clear();
    }
    std::string_view at(std::size_t i) const {
        return {arena.data() + spans[i].first, spans[i].second};
    }
    bool null(std::size_t i) const { return spans[i].first == kNull; }
};

Writer::Writer(const std::string& path, std::vector<std::string> names, Codec codec,
               std::int64_t row_group_rows, std::size_t dict_limit, unsigned threads)
    : path_(path), names_(std::move(names)), codec_(codec), rg_rows_(row_group_rows),
      dict_limit_(dict_limit),
      threads_(threads ? threads : std::max(1u, std::thread::hardware_concurrency())) {
    if (names_.empty()) throw Error("a parquet file needs at least one column");
    if (rg_rows_ <= 0) throw Error("the row group size must be positive");
    fd_ = ::open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, 0644);
    if (fd_ < 0) throw Error("cannot create " + path);
    cols_.resize(names_.size());
    write_all("PAR1");
}

Writer::~Writer() {
    if (fd_ >= 0) ::close(fd_);
}

void Writer::row(const std::vector<Value>& values) {
    if (values.size() != cols_.size())
        throw Error("a row has the wrong number of columns for " + path_);
    for (std::size_t c = 0; c < cols_.size(); ++c) cols_[c].add(values[c]);
    ++held_;
    ++rows_;
    if (held_ >= rg_rows_) flush_row_group();
}

void Writer::flush_row_group() {
    if (held_ == 0) return;
    const std::size_t n = static_cast<std::size_t>(held_);

    // Each column's pages are built into its own buffer, in parallel, because a
    // column depends on nothing outside itself: its dictionary, its definition
    // levels, and its compression are all its own. Only where the bytes land in
    // the file depends on the other columns, and that is known once every
    // buffer's size is -- so the offsets are fixed up afterwards and the
    // buffers written in column order.
    struct Built {
        std::string bytes;
        std::int64_t uncompressed = 0;
        std::int64_t compressed = 0;
        std::int64_t dict_at = -1;   // offset within `bytes`, or -1 for no dictionary
        std::int64_t data_at = 0;
    };
    std::vector<Built> built(cols_.size());
    std::vector<std::exception_ptr> failures(cols_.size());

    const auto build = [&](std::size_t c) {
        Column& col = cols_[c];
        Built& out = built[c];
        std::string body, page, header, defs, idx_bytes;
        std::vector<std::int32_t> def_levels(n), indices, present;

        // Definition levels are the same however the values are encoded: one
        // per row, 1 present and 0 null, RLE, with a four-byte length in front.
        for (std::size_t i = 0; i < n; ++i) def_levels[i] = col.null(i) ? 0 : 1;
        rle_hybrid(defs, def_levels.data(), n, 1);

        // Distinct values in first-seen order, abandoned once the column is too
        // varied for a dictionary to be worth it. Giving up partway is the
        // point: it is what produces a column that is dictionary encoded in one
        // row group and plain in the next, which is what real writers do to a
        // high-cardinality string and what the reader has to fold together.
        std::unordered_map<std::string_view, std::int32_t> seen;
        std::vector<std::string_view> dict;
        bool use_dict = true;
        indices.assign(n, 0);
        for (std::size_t i = 0; i < n; ++i) {
            if (col.null(i)) continue;
            const std::string_view v = col.at(i);
            const auto it = seen.find(v);
            if (it != seen.end()) {
                indices[i] = it->second;
                continue;
            }
            if (dict.size() >= dict_limit_) {
                use_dict = false;
                break;
            }
            const std::int32_t id = static_cast<std::int32_t>(dict.size());
            seen.emplace(v, id);
            dict.push_back(v);
            indices[i] = id;
        }

        // A page is a header and a body; only the body is compressed, and the
        // sizes in the header describe the body alone.
        const auto emit = [&](int type, std::int32_t values, int encoding) {
            const std::size_t raw = body.size();
            if (codec_ == Codec::Snappy) snappy_compress(body, page);
            else page = body;
            header.clear();
            {
                Enc e(header);
                e.i32(1, type);
                e.i32(2, static_cast<std::int32_t>(raw));
                e.i32(3, static_cast<std::int32_t>(page.size()));
                if (type == kDataPage) {
                    Enc::Guard g = e.nested(5);
                    e.i32(1, values);
                    e.i32(2, encoding);
                    e.i32(3, kRle);   // definition levels
                    e.i32(4, kRle);   // repetition levels
                } else {
                    Enc::Guard g = e.nested(7);
                    e.i32(1, values);
                    e.i32(2, encoding);
                }
                e.stop();
            }
            out.bytes.append(header);
            out.bytes.append(page);
            out.uncompressed += static_cast<std::int64_t>(header.size() + raw);
            out.compressed += static_cast<std::int64_t>(header.size() + page.size());
        };

        if (use_dict) {
            out.dict_at = 0;
            body.clear();
            for (std::string_view v : dict) {
                const std::uint32_t len = static_cast<std::uint32_t>(v.size());
                body.append(reinterpret_cast<const char*>(&len), 4);
                body.append(v);
            }
            emit(kDictionaryPage, static_cast<std::int32_t>(dict.size()), kPlain);
        }

        body.clear();
        const std::uint32_t dl = static_cast<std::uint32_t>(defs.size());
        body.append(reinterpret_cast<const char*>(&dl), 4);
        body.append(defs);
        if (use_dict) {
            const int width = width_for(std::max<std::size_t>(dict.size(), 1));
            body.push_back(static_cast<char>(width));
            present.reserve(n);
            for (std::size_t i = 0; i < n; ++i)
                if (!col.null(i)) present.push_back(indices[i]);
            if (!present.empty()) rle_hybrid(idx_bytes, present.data(), present.size(), width);
            body.append(idx_bytes);
        } else {
            for (std::size_t i = 0; i < n; ++i) {
                if (col.null(i)) continue;
                const std::string_view v = col.at(i);
                const std::uint32_t len = static_cast<std::uint32_t>(v.size());
                body.append(reinterpret_cast<const char*>(&len), 4);
                body.append(v);
            }
        }
        out.data_at = static_cast<std::int64_t>(out.bytes.size());
        emit(kDataPage, static_cast<std::int32_t>(n), use_dict ? kRleDictionary : kPlain);
    };

    const unsigned lanes = std::max(1u, std::min<unsigned>(threads_,
                                                           static_cast<unsigned>(cols_.size())));
    std::atomic<std::size_t> next{0};
    const auto work = [&] {
        for (;;) {
            const std::size_t c = next.fetch_add(1);
            if (c >= cols_.size()) return;
            try {
                build(c);
            } catch (...) {
                failures[c] = std::current_exception();
            }
        }
    };
    {
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
    }
    for (const auto& f : failures)
        if (f) std::rethrow_exception(f);

    std::string chunks;
    std::int64_t group_bytes = 0;
    for (std::size_t c = 0; c < cols_.size(); ++c) {
        const Built& b = built[c];
        const std::int64_t base = at_;
        write_all(b.bytes);
        group_bytes += b.uncompressed;

        const std::int64_t data_offset = base + b.data_at;
        const std::int64_t dict_offset = b.dict_at >= 0 ? base + b.dict_at : 0;
        Enc e(chunks);
        e.i64(2, b.dict_at >= 0 ? dict_offset : data_offset);   // file_offset
        {
            Enc::Guard g = e.nested(3);                         // meta_data
            e.i32(1, kTypeByteArray);
            e.enums(2, b.dict_at >= 0 ? std::vector<int>{kPlain, kRle, kRleDictionary}
                                      : std::vector<int>{kPlain, kRle});
            e.strings(3, {names_[c]});
            e.i32(4, codec_ == Codec::Snappy ? kSnappy : kUncompressed);
            e.i64(5, static_cast<std::int64_t>(n));
            e.i64(6, b.uncompressed);
            e.i64(7, b.compressed);
            e.i64(9, data_offset);
            if (b.dict_at >= 0) e.i64(11, dict_offset);
        }
        e.stop();
        cols_[c].clear();
    }

    // The list header, then the chunk structs exactly as they were encoded,
    // then the two remaining fields. Splicing rather than re-encoding is what
    // lets each chunk be built while its column is being compressed.
    groups_.emplace_back();
    {
        std::string& out = groups_.back();
        Enc e(out);
        e.struct_list(1, cols_.size());
        out.append(chunks);
        e.i64(2, group_bytes);
        e.i64(3, static_cast<std::int64_t>(n));
        e.stop();
    }

    held_ = 0;
}

void Writer::write_all(const std::string& bytes) {
    std::size_t left = bytes.size();
    const char* from = bytes.data();
    while (left > 0) {
        const ssize_t wrote = ::write(fd_, from, left);
        if (wrote <= 0) throw Error("cannot write " + path_);
        from += wrote;
        left -= static_cast<std::size_t>(wrote);
    }
    at_ += static_cast<std::int64_t>(bytes.size());
}

void Writer::close() {
    if (closed_) return;
    closed_ = true;
    flush_row_group();

    std::string meta;
    {
        Enc e(meta);
        e.i32(1, 1);   // version

        // The schema is a root group with one child per column, all of them
        // optional UTF8 byte arrays -- the shape every reader here agrees on.
        e.struct_list(2, names_.size() + 1);
        {
            std::string root;
            Enc r(root);
            r.i32(3, kRequired);
            r.str(4, "csvdiff");
            r.i32(5, static_cast<std::int32_t>(names_.size()));
            r.stop();
            meta.append(root);
        }
        for (const auto& name : names_) {
            std::string leaf;
            Enc l(leaf);
            l.i32(1, kTypeByteArray);
            l.i32(3, kOptional);
            l.str(4, name);
            l.i32(6, kUtf8);
            l.stop();
            meta.append(leaf);
        }

        e.i64(3, rows_);
        e.struct_list(4, groups_.size());
        for (const auto& g : groups_) meta.append(g);
        e.str(6, "csvdiff gen-data");
        e.stop();
    }

    const std::uint32_t len = static_cast<std::uint32_t>(meta.size());
    meta.append(reinterpret_cast<const char*>(&len), 4);
    meta.append("PAR1", 4);
    write_all(meta);
    ::close(fd_);
    fd_ = -1;
}

}  // namespace pqwrite
