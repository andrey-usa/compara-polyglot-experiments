//! A Parquet reader shaped for comparing, not for querying.
//!
//! The Zig twin of `cpp/src/parquet.cpp` and `rust/src/parquet.rs`, and it makes
//! the same bargain: it reads exactly what this job needs and refuses the rest
//! by name. `BYTE_ARRAY` columns, PLAIN and dictionary encodings, uncompressed
//! and snappy, v1 data pages. That covers what DuckDB, polars and this
//! project's own generator write for string columns.
//!
//! Two things it does that a general reader would not.
//!
//! It hands back *offsets into the mapped file* rather than strings. A PLAIN
//! byte array is a four-byte length followed by its bytes, already contiguous
//! and already in the mapping, so a value can stay an offset and a length --
//! the same representation the CSV path uses, and the reason nothing here
//! builds a string per cell.
//!
//! And it keeps dictionary columns encoded. Where a column is dictionary
//! encoded the rows are indices into a small table of distinct values, so two
//! files can be compared by mapping one dictionary onto the other once and then
//! comparing integers.
//!
//! Every allocation goes through the caller's allocator, like the rest of this
//! port: a comparison that would exceed `--max-memory` fails at the allocation
//! that would have crossed it.

const std = @import("std");

pub const Error = error{
    NotParquet,
    ParquetTruncated,
    ParquetMalformed,
    ParquetNestedColumn,
    ParquetNoSuchColumn,
    ParquetTypeUnsupported,
    ParquetCodecUnsupported,
    ParquetEncodingUnsupported,
    ParquetPageV2,
    ParquetValueTooLong,
    ParquetMixedCompression,
    ParquetSnappyFailed,
};

// parquet.thrift, the handful of values this reader meets.
const type_byte_array: i64 = 6;
const enc_plain: i64 = 0;
const enc_plain_dictionary: i64 = 2;
const enc_rle: i64 = 3;
const enc_rle_dictionary: i64 = 8;
const codec_uncompressed: i64 = 0;
const codec_snappy: i64 = 1;
const page_data: i64 = 0;
const page_index: i64 = 1;
const page_dictionary: i64 = 2;
const page_data_v2: i64 = 3;

// Thrift compact protocol field types.
const t_stop: u8 = 0;
const t_true: u8 = 1;
const t_false: u8 = 2;
const t_byte: u8 = 3;
const t_i16: u8 = 4;
const t_i32: u8 = 5;
const t_i64: u8 = 6;
const t_double: u8 = 7;
const t_binary: u8 = 8;
const t_list: u8 = 9;
const t_set: u8 = 10;
const t_map: u8 = 11;
const t_struct: u8 = 12;

/// A value's bytes, as an offset and a length packed into one word: forty bits
/// of offset, twenty-three of length, and one to say the value is null.
///
/// The same layout this port already packs a CSV field into, for the same
/// reason -- eight bytes a value rather than sixteen is eighty megabytes less
/// to carry on a ten-million-row column, and eighty megabytes less to stream
/// past the CPU.
pub const Slice = packed struct(u64) {
    bits: u64,

    pub const max_length: u32 = (1 << 23) - 1;
    const offset_mask: u64 = (1 << 40) - 1;
    const length_shift: u6 = 40;
    const null_bit: u64 = 1 << 63;

    pub const none: Slice = .{ .bits = null_bit };

    pub fn at(off: u64, len: u32) Slice {
        return .{ .bits = (off & offset_mask) | (@as(u64, len) << length_shift) };
    }
    pub fn isNull(self: Slice) bool {
        return self.bits & null_bit != 0;
    }
    pub fn offset(self: Slice) usize {
        return @intCast(self.bits & offset_mask);
    }
    pub fn length(self: Slice) usize {
        return @intCast((self.bits >> length_shift) & max_length);
    }
};

/// One column of one file, decoded as far as it is useful to decode it.
///
/// A dictionary column keeps `dict` and `index`: `index[row]` selects a value,
/// or is `null_index`. A plain column keeps `values`, one slice per row.
pub const Column = struct {
    pub const null_index: i32 = -1;

    dictionary: bool = false,
    /// Dictionary form: the distinct values.
    dict: []Slice = &.{},
    /// Dictionary form: one per row.
    index: []i32 = &.{},
    /// Plain form: one per row.
    values: []Slice = &.{},
    /// Decompressed pages, when the column was compressed. A column is either
    /// wholly compressed or wholly not -- the reader refuses anything else --
    /// so this one field says where every slice points: into `owned` when it is
    /// non-empty, into the mapping when it is not.
    owned: []u8 = &.{},

    pub fn rows(self: Column) usize {
        return if (self.dictionary) self.index.len else self.values.len;
    }

    /// The buffer this column's slices count from.
    pub fn base(self: Column, mapping: []const u8) []const u8 {
        return if (self.owned.len == 0) mapping else self.owned;
    }

    pub fn deinit(self: *Column, gpa: std.mem.Allocator) void {
        gpa.free(self.dict);
        gpa.free(self.index);
        gpa.free(self.values);
        gpa.free(self.owned);
        self.* = .{};
    }
};

/// What a file says about itself, before any column is read.
pub const Meta = struct {
    /// Leaf columns, in file order.
    names: [][]const u8 = &.{},
    rows: i64 = 0,
    row_groups: usize = 0,

    pub fn deinit(self: *Meta, gpa: std.mem.Allocator) void {
        for (self.names) |n| gpa.free(n);
        gpa.free(self.names);
        self.* = .{};
    }
};

// ---------------------------------------------------------------------------
// Thrift compact protocol
// ---------------------------------------------------------------------------

const Thrift = struct {
    d: []const u8,
    at: usize = 0,

    fn byte(self: *Thrift) !u8 {
        if (self.at >= self.d.len) return Error.ParquetTruncated;
        const b = self.d[self.at];
        self.at += 1;
        return b;
    }

    fn varint(self: *Thrift) !u64 {
        var out: u64 = 0;
        var shift: u6 = 0;
        while (true) {
            const b = try self.byte();
            out |= @as(u64, b & 0x7F) << shift;
            if (b & 0x80 == 0) return out;
            if (shift >= 57) return Error.ParquetMalformed;
            shift += 7;
        }
    }

    fn zigzag(self: *Thrift) !i64 {
        const v = try self.varint();
        return @as(i64, @bitCast(v >> 1)) ^ -@as(i64, @intCast(v & 1));
    }

    fn binary(self: *Thrift) ![]const u8 {
        const len: usize = @intCast(try self.varint());
        if (self.at + len > self.d.len) return Error.ParquetTruncated;
        const out = self.d[self.at..][0..len];
        self.at += len;
        return out;
    }

    /// Reads a field header. Returns the type, or `t_stop` at the end of a
    /// struct; `id` is set to the field number.
    fn field(self: *Thrift, id: *i16, last: *i16) !u8 {
        const h = try self.byte();
        if (h == 0) return t_stop;
        const ty = h & 0x0F;
        const delta = (h & 0xF0) >> 4;
        id.* = if (delta == 0) @intCast(try self.zigzag()) else last.* + @as(i16, delta);
        last.* = id.*;
        return ty;
    }

    /// Reads a list header, returning the element type and the count.
    fn list(self: *Thrift, count: *u32) !u8 {
        const h = try self.byte();
        count.* = (h & 0xF0) >> 4;
        if (count.* == 15) count.* = @intCast(try self.varint());
        return h & 0x0F;
    }

    /// Steps over a value without interpreting it, so a struct can be read for
    /// the few fields that matter and the rest skipped.
    fn skip(self: *Thrift, ty: u8) anyerror!void {
        switch (ty) {
            t_true, t_false => {},
            t_byte => _ = try self.byte(),
            t_i16, t_i32, t_i64 => _ = try self.zigzag(),
            t_double => {
                var i: usize = 0;
                while (i < 8) : (i += 1) _ = try self.byte();
            },
            t_binary => _ = try self.binary(),
            t_list, t_set => {
                var n: u32 = 0;
                const elem = try self.list(&n);
                var i: u32 = 0;
                while (i < n) : (i += 1) try self.skip(elem);
            },
            t_map => {
                const n = try self.varint();
                if (n > 0) {
                    const kinds = try self.byte();
                    var i: u64 = 0;
                    while (i < n) : (i += 1) {
                        try self.skip(kinds >> 4);
                        try self.skip(kinds & 0x0F);
                    }
                }
            },
            t_struct => {
                var id: i16 = 0;
                var last: i16 = 0;
                while (true) {
                    const t = try self.field(&id, &last);
                    if (t == t_stop) break;
                    try self.skip(t);
                }
            },
            else => return Error.ParquetMalformed,
        }
    }
};

// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

const ChunkMeta = struct {
    ty: i64 = 0,
    codec: i64 = 0,
    num_values: i64 = 0,
    data_page_offset: i64 = 0,
    dictionary_page_offset: i64 = 0,
    total_compressed_size: i64 = 0,
    total_uncompressed_size: i64 = 0,
};

const FileMeta = struct {
    names: [][]const u8,
    /// true where the column may be null.
    optional: []bool,
    /// Row-major: `chunks[group * columns + which]`.
    chunks: []ChunkMeta,
    columns: usize,
    groups: usize,
    rows: i64,

    fn deinit(self: *FileMeta, gpa: std.mem.Allocator) void {
        for (self.names) |n| gpa.free(n);
        gpa.free(self.names);
        gpa.free(self.optional);
        gpa.free(self.chunks);
    }

    fn chunk(self: FileMeta, group: usize, which: usize) ChunkMeta {
        return self.chunks[group * self.columns + which];
    }
};

fn readColumnMeta(t: *Thrift, out: *ChunkMeta) !void {
    var id: i16 = 0;
    var last: i16 = 0;
    while (true) {
        const ty = try t.field(&id, &last);
        if (ty == t_stop) return;
        switch (id) {
            1 => out.ty = try t.zigzag(),
            4 => out.codec = try t.zigzag(),
            5 => out.num_values = try t.zigzag(),
            6 => out.total_uncompressed_size = try t.zigzag(),
            7 => out.total_compressed_size = try t.zigzag(),
            9 => out.data_page_offset = try t.zigzag(),
            11 => out.dictionary_page_offset = try t.zigzag(),
            else => try t.skip(ty),
        }
    }
}

fn readChunk(t: *Thrift, out: *ChunkMeta) !void {
    var id: i16 = 0;
    var last: i16 = 0;
    while (true) {
        const ty = try t.field(&id, &last);
        if (ty == t_stop) return;
        if (id == 3) try readColumnMeta(t, out) else try t.skip(ty);
    }
}

fn readFileMeta(gpa: std.mem.Allocator, data: []const u8) !FileMeta {
    if (data.len < 12 or !std.mem.eql(u8, data[0..4], "PAR1") or
        !std.mem.eql(u8, data[data.len - 4 ..], "PAR1")) return Error.NotParquet;
    const meta_len = std.mem.readInt(u32, data[data.len - 8 ..][0..4], .little);
    if (@as(usize, meta_len) + 8 > data.len) return Error.ParquetTruncated;
    const start = data.len - 8 - meta_len;
    var t = Thrift{ .d = data[start..][0..meta_len] };

    var names: std.ArrayList([]const u8) = .empty;
    errdefer {
        for (names.items) |n| gpa.free(n);
        names.deinit(gpa);
    }
    var optional: std.ArrayList(bool) = .empty;
    errdefer optional.deinit(gpa);
    var chunks: std.ArrayList(ChunkMeta) = .empty;
    errdefer chunks.deinit(gpa);
    var rows: i64 = 0;
    var groups: usize = 0;

    var id: i16 = 0;
    var last: i16 = 0;
    while (true) {
        const ty = try t.field(&id, &last);
        if (ty == t_stop) break;
        switch (id) {
            2 => {
                var n: u32 = 0;
                _ = try t.list(&n);
                var i: u32 = 0;
                while (i < n) : (i += 1) {
                    // SchemaElement: repetition_type is 3, name 4, num_children 5.
                    var sid: i16 = 0;
                    var slast: i16 = 0;
                    var name: []const u8 = &.{};
                    var repetition: i64 = -1;
                    var children: i64 = 0;
                    while (true) {
                        const st = try t.field(&sid, &slast);
                        if (st == t_stop) break;
                        switch (sid) {
                            3 => repetition = try t.zigzag(),
                            4 => name = try t.binary(),
                            5 => children = try t.zigzag(),
                            else => try t.skip(st),
                        }
                    }
                    // The first element is the root, and anything with children
                    // is a group rather than a column this reader can read.
                    if (i == 0) continue;
                    if (children > 0) return Error.ParquetNestedColumn;
                    try names.append(gpa, try gpa.dupe(u8, name));
                    try optional.append(gpa, repetition == 1);
                }
            },
            3 => rows = try t.zigzag(),
            4 => {
                var n: u32 = 0;
                _ = try t.list(&n);
                groups = n;
                var g: u32 = 0;
                while (g < n) : (g += 1) {
                    var gid: i16 = 0;
                    var glast: i16 = 0;
                    while (true) {
                        const gt = try t.field(&gid, &glast);
                        if (gt == t_stop) break;
                        if (gid == 1) {
                            var cn: u32 = 0;
                            _ = try t.list(&cn);
                            var c: u32 = 0;
                            while (c < cn) : (c += 1) {
                                var meta: ChunkMeta = .{};
                                try readChunk(&t, &meta);
                                try chunks.append(gpa, meta);
                            }
                        } else try t.skip(gt);
                    }
                }
            },
            else => try t.skip(ty),
        }
    }
    if (names.items.len == 0) return Error.ParquetMalformed;
    if (groups > 0 and chunks.items.len != groups * names.items.len) return Error.ParquetMalformed;

    // Taken before the slices are, because `toOwnedSlice` empties the list it
    // took from and struct fields are evaluated in source order.
    const columns = names.items.len;
    return FileMeta{
        .names = try names.toOwnedSlice(gpa),
        .optional = try optional.toOwnedSlice(gpa),
        .chunks = try chunks.toOwnedSlice(gpa),
        .columns = columns,
        .groups = groups,
        .rows = rows,
    };
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

const PageHead = struct {
    ty: i64 = 0,
    compressed: i64 = 0,
    num_values: i32 = 0,
    encoding: i64 = 0,
    def_encoding: i64 = 0,
    /// Offset of the page body.
    after: usize = 0,
};

fn readPageHead(data: []const u8, at: usize) !PageHead {
    var t = Thrift{ .d = data, .at = at };
    var out: PageHead = .{};
    var id: i16 = 0;
    var last: i16 = 0;
    while (true) {
        const ty = try t.field(&id, &last);
        if (ty == t_stop) break;
        switch (id) {
            1 => out.ty = try t.zigzag(),
            3 => out.compressed = try t.zigzag(),
            5, 7 => {
                const is_data = id == 5;
                var hid: i16 = 0;
                var hlast: i16 = 0;
                while (true) {
                    const ht = try t.field(&hid, &hlast);
                    if (ht == t_stop) break;
                    if (hid == 1) {
                        out.num_values = @intCast(try t.zigzag());
                    } else if (hid == 2) {
                        out.encoding = try t.zigzag();
                    } else if (hid == 3 and is_data) {
                        out.def_encoding = try t.zigzag();
                    } else try t.skip(ht);
                }
            },
            8 => return Error.ParquetPageV2,
            else => try t.skip(ty),
        }
    }
    out.after = t.at;
    return out;
}

// ---------------------------------------------------------------------------
// Snappy
// ---------------------------------------------------------------------------

/// Snappy raw block format: a varint of the uncompressed length, then a stream
/// of literal and copy elements.
///
/// It appends to `out` and copies a back-reference eight bytes at a time where
/// the distance allows it. Both matter more than they look: this is the one loop
/// that touches every byte of a compressed file.
fn snappyAppend(gpa: std.mem.Allocator, input: []const u8, out: *std.ArrayList(u8)) !void {
    var at: usize = 0;
    var want: u64 = 0;
    var shift: u6 = 0;
    while (true) {
        if (at >= input.len) return Error.ParquetSnappyFailed;
        const b = input[at];
        at += 1;
        want |= @as(u64, b & 0x7F) << shift;
        if (b & 0x80 == 0) break;
        if (shift >= 57) return Error.ParquetSnappyFailed;
        shift += 7;
    }

    const base = out.items.len;
    try out.resize(gpa, base + @as(usize, @intCast(want)));
    const buf = out.items;
    var dst = base;
    const end = base + @as(usize, @intCast(want));

    while (at < input.len) {
        const tag = input[at];
        at += 1;
        if (tag & 0x03 == 0) { // literal
            var len: usize = (tag >> 2) + 1;
            if (len > 60) {
                const extra = len - 60;
                if (at + extra > input.len) return Error.ParquetSnappyFailed;
                var v: usize = 0;
                var i: usize = 0;
                while (i < extra) : (i += 1) v |= @as(usize, input[at + i]) << @intCast(8 * i);
                at += extra;
                len = v + 1;
            }
            if (at + len > input.len or len > end - dst) return Error.ParquetSnappyFailed;
            @memcpy(buf[dst..][0..len], input[at..][0..len]);
            dst += len;
            at += len;
            continue;
        }
        var len: usize = 0;
        var offset: usize = 0;
        switch (tag & 0x03) {
            1 => {
                if (at >= input.len) return Error.ParquetSnappyFailed;
                offset = (@as(usize, tag >> 5) << 8) | input[at];
                at += 1;
                len = @as(usize, (tag >> 2) & 0x07) + 4;
            },
            2 => {
                if (at + 2 > input.len) return Error.ParquetSnappyFailed;
                offset = @as(usize, input[at]) | (@as(usize, input[at + 1]) << 8);
                at += 2;
                len = @as(usize, tag >> 2) + 1;
            },
            else => {
                if (at + 4 > input.len) return Error.ParquetSnappyFailed;
                var o: usize = 0;
                var i: usize = 0;
                while (i < 4) : (i += 1) o |= @as(usize, input[at + i]) << @intCast(8 * i);
                at += 4;
                offset = o;
                len = @as(usize, tag >> 2) + 1;
            },
        }
        if (offset == 0 or offset > dst - base or len > end - dst) return Error.ParquetSnappyFailed;
        const from = dst - offset;
        if (offset >= 8) {
            // The source of the next eight bytes is entirely behind the
            // destination, so whole words can be moved.
            var i: usize = 0;
            while (i + 8 <= len) : (i += 8) {
                const w = std.mem.readInt(u64, buf[from + i ..][0..8], .little);
                std.mem.writeInt(u64, buf[dst + i ..][0..8], w, .little);
            }
            while (i < len) : (i += 1) buf[dst + i] = buf[from + i];
        } else {
            // A short distance means the copy repeats a pattern it is still
            // writing, so it has to go a byte at a time.
            var i: usize = 0;
            while (i < len) : (i += 1) buf[dst + i] = buf[from + i];
        }
        dst += len;
    }
    if (dst != end) return Error.ParquetSnappyFailed;
}

// ---------------------------------------------------------------------------
// RLE / bit-packed hybrid
// ---------------------------------------------------------------------------

const RleReader = struct {
    d: []const u8,
    width: u32,
    mask: u64,
    at: usize = 0,
    left: usize = 0,
    bit: usize = 0,
    packed_run: bool = false,
    value: i32 = 0,

    fn init(d: []const u8, width: u32) RleReader {
        return .{
            .d = d,
            .width = width,
            .mask = if (width >= 64) ~@as(u64, 0) else (@as(u64, 1) << @intCast(width)) - 1,
        };
    }

    fn header(self: *RleReader) bool {
        var h: u64 = 0;
        var shift: u6 = 0;
        while (true) {
            if (self.at >= self.d.len) return false;
            const b = self.d[self.at];
            self.at += 1;
            h |= @as(u64, b & 0x7F) << shift;
            if (b & 0x80 == 0) break;
            if (shift >= 57) return false;
            shift += 7;
        }
        if (h & 1 == 0) {
            // RLE run: a count and one value.
            self.packed_run = false;
            self.left = @intCast(h >> 1);
            const bytes: usize = (self.width + 7) / 8;
            if (self.at + bytes > self.d.len) return false;
            self.value = 0;
            var i: usize = 0;
            while (i < bytes) : (i += 1) {
                self.value |= @as(i32, self.d[self.at + i]) << @intCast(8 * i);
            }
            self.at += bytes;
        } else {
            // Bit-packed run, in groups of eight. A group is exactly `width`
            // bytes, so the whole run's length is known and `at` can jump
            // straight to the next header while `bit` walks inside it.
            self.packed_run = true;
            const groups: usize = @intCast(h >> 1);
            self.left = groups * 8;
            self.bit = self.at * 8;
            const run = groups * @as(usize, self.width);
            self.at = @min(self.at + run, self.d.len);
        }
        return self.left > 0;
    }

    /// Fills `out`. Bulk rather than one at a time on purpose: an RLE run
    /// becomes a fill, and a bit-packed run becomes one 64-bit load, one shift
    /// and one mask per value.
    ///
    /// Values are packed end to end with no padding, so a value straddles bytes
    /// more often than not. Reading eight bytes around it and shifting picks any
    /// of them out without a loop -- the same "look at eight bytes at once"
    /// trick the CSV scanner uses to find a delimiter, here reading rather than
    /// searching. It is exact for every width Parquet allows: seven bits of
    /// misalignment plus thirty-two of value still fits in a word.
    fn fill(self: *RleReader, out: []i32) bool {
        var done: usize = 0;
        while (done < out.len) {
            if (self.left == 0 and !self.header()) return false;
            const take = @min(out.len - done, self.left);
            if (!self.packed_run) {
                @memset(out[done..][0..take], self.value);
            } else if (self.width == 0) {
                @memset(out[done..][0..take], 0);
            } else {
                // The eight-byte read needs eight bytes to read, and asking that
                // per value is a branch on every one of ten million. Where the
                // last value of a run starts is arithmetic, so the count that
                // cannot run off the end is solved for once and those values are
                // taken without a check -- the same reason the CSV scanner cuts
                // its slice before looping rather than testing inside it. Only a
                // run ending within eight bytes of the page reaches the general
                // path below.
                var bit = self.bit;
                const width = self.width;
                const mask = self.mask;
                var i: usize = 0;
                if (self.d.len >= 8) {
                    const last = (self.d.len - 8) * 8 + 7;
                    const safe = if (bit > last) 0 else (last - bit) / width + 1;
                    const n = @min(take, safe);
                    while (i < n) : (i += 1) {
                        const w = std.mem.readInt(u64, self.d[bit >> 3 ..][0..8], .little);
                        out[done + i] = @intCast((w >> @intCast(bit & 7)) & mask);
                        bit += width;
                    }
                }
                while (i < take) : (i += 1) {
                    const byte = bit >> 3;
                    var w: u64 = 0;
                    var k: usize = 0;
                    while (k < 8 and byte + k < self.d.len) : (k += 1) {
                        w |= @as(u64, self.d[byte + k]) << @intCast(8 * k);
                    }
                    out[done + i] = @intCast((w >> @intCast(bit & 7)) & mask);
                    bit += width;
                }
                self.bit = bit;
            }
            self.left -= take;
            done += take;
        }
        return true;
    }
};

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// PLAIN byte arrays: a four-byte little-endian length, then that many bytes,
/// repeated. The slices point straight at `base`, so nothing is copied.
fn plainSlices(
    gpa: std.mem.Allocator,
    page: []const u8,
    base: u64,
    count: i32,
    out: *std.ArrayList(Slice),
) !void {
    // A count the page cannot hold is refused below, value by value; reserving
    // for it first would let a corrupt header ask for any amount of memory, so
    // the reservation is capped by what four-byte length prefixes could fit.
    const possible = @min(@as(usize, @intCast(@max(count, 0))), page.len / 4);
    try out.ensureUnusedCapacity(gpa, possible);
    var at: usize = 0;
    var i: i32 = 0;
    while (i < count) : (i += 1) {
        if (at + 4 > page.len) return Error.ParquetTruncated;
        const len = std.mem.readInt(u32, page[at..][0..4], .little);
        at += 4;
        if (at + len > page.len) return Error.ParquetTruncated;
        if (len > Slice.max_length) return Error.ParquetValueTooLong;
        out.appendAssumeCapacity(Slice.at(base + at, len));
        at += len;
    }
}

// ---------------------------------------------------------------------------
// The two entry points
// ---------------------------------------------------------------------------

/// True when the file begins with Parquet's `PAR1` magic. Cheap: four bytes.
pub fn isParquet(data: []const u8) bool {
    return data.len >= 4 and std.mem.eql(u8, data[0..4], "PAR1");
}

/// Reads the footer.
pub fn readMeta(gpa: std.mem.Allocator, data: []const u8) !Meta {
    const fm = try readFileMeta(gpa, data);
    // The names are handed on; the rest of the footer is not.
    gpa.free(fm.optional);
    gpa.free(fm.chunks);
    return Meta{ .names = fm.names, .rows = fm.rows, .row_groups = fm.groups };
}

/// Reads one column across every row group. `which` indexes into `Meta.names`.
pub fn readColumn(gpa: std.mem.Allocator, data: []const u8, which: usize) !Column {
    var fm = try readFileMeta(gpa, data);
    defer fm.deinit(gpa);
    if (which >= fm.columns) return Error.ParquetNoSuchColumn;
    const optional = fm.optional[which];

    var owned: std.ArrayList(u8) = .empty;
    errdefer owned.deinit(gpa);
    var dict: std.ArrayList(Slice) = .empty;
    errdefer dict.deinit(gpa);
    var index: std.ArrayList(i32) = .empty;
    errdefer index.deinit(gpa);
    var values: std.ArrayList(Slice) = .empty;
    errdefer values.deinit(gpa);

    // Reused across pages so the per-page work allocates nothing, and sized once
    // from the footer's row count so appending a page never has to move eighty
    // megabytes of what is already decoded.
    var defs: std.ArrayList(i32) = .empty;
    defer defs.deinit(gpa);
    var idx: std.ArrayList(i32) = .empty;
    defer idx.deinit(gpa);
    var got: std.ArrayList(Slice) = .empty;
    defer got.deinit(gpa);
    try index.ensureTotalCapacity(gpa, @intCast(fm.rows));

    var codec: i64 = -1;
    var uncompressed: i64 = 0;
    {
        var g: usize = 0;
        while (g < fm.groups) : (g += 1) uncompressed += fm.chunk(g, which).total_uncompressed_size;
    }

    // The column starts out held as dictionary indices and stays that way only
    // if every page cooperates. A writer that gives up on the dictionary partway
    // -- DuckDB does, once a column's distinct values outgrow its budget, which
    // at ten million rows is most of them -- forces the whole column into the
    // plain form. Expanding what has been read costs no bytes: a dictionary
    // entry is already a slice.
    var dictionary = true;

    var g: usize = 0;
    while (g < fm.groups) : (g += 1) {
        const c = fm.chunk(g, which);
        if (c.ty != type_byte_array) return Error.ParquetTypeUnsupported;
        if (c.codec != codec_uncompressed and c.codec != codec_snappy) return Error.ParquetCodecUnsupported;
        // Every chunk has to agree about compression, because a slice is an
        // offset with no room to say what it is an offset *into*.
        if (codec < 0) {
            codec = c.codec;
            if (codec == codec_snappy) try owned.ensureTotalCapacity(gpa, @intCast(uncompressed));
        } else if (codec != c.codec) return Error.ParquetMixedCompression;

        var at: usize = @intCast(if (c.dictionary_page_offset > 0) c.dictionary_page_offset else c.data_page_offset);
        const stop = at + @as(usize, @intCast(c.total_compressed_size));
        var seen: i64 = 0;
        const dict_base = dict.items.len;

        while (at < stop and at < data.len and seen < c.num_values) {
            const h = try readPageHead(data, at);
            const body_len: usize = @intCast(h.compressed);
            if (h.after + body_len > data.len) return Error.ParquetTruncated;

            // Where the page's bytes live, and what a slice into them counts
            // from. `in_owned` is the whole of the distinction.
            var page_from: usize = h.after;
            var page_len: usize = body_len;
            var page_base: u64 = h.after;
            const in_owned = c.codec == codec_snappy;
            if (in_owned) {
                const was = owned.items.len;
                try snappyAppend(gpa, data[h.after..][0..body_len], &owned);
                page_from = was;
                page_len = owned.items.len - was;
                page_base = was;
            }
            const body: []const u8 = if (in_owned) owned.items[page_from..][0..page_len] else data[page_from..][0..page_len];

            if (h.ty == page_dictionary) {
                if (h.encoding != enc_plain and h.encoding != enc_plain_dictionary) {
                    return Error.ParquetEncodingUnsupported;
                }
                try plainSlices(gpa, body, page_base, h.num_values, &dict);
            } else if (h.ty == page_data) {
                const n_vals: usize = @intCast(h.num_values);
                var vat: usize = 0;
                var real: usize = n_vals;
                if (optional) {
                    // Definition levels: RLE, four-byte length prefix in v1.
                    if (h.def_encoding != enc_rle) return Error.ParquetEncodingUnsupported;
                    if (page_len < 4) return Error.ParquetTruncated;
                    const dl: usize = std.mem.readInt(u32, body[0..4], .little);
                    if (4 + dl > page_len) return Error.ParquetTruncated;
                    defs.clearRetainingCapacity();
                    try defs.resize(gpa, n_vals);
                    var r = RleReader.init(body[4..][0..dl], 1);
                    if (!r.fill(defs.items)) return Error.ParquetMalformed;
                    real = 0;
                    for (defs.items) |d| real += @intFromBool(d != 0);
                    vat = 4 + dl;
                }
                if (vat > page_len) return Error.ParquetTruncated;

                if (h.encoding == enc_plain_dictionary or h.encoding == enc_rle_dictionary) {
                    if (dict.items.len == 0) return Error.ParquetMalformed;
                    if (vat + 1 > page_len) return Error.ParquetTruncated;
                    const width: u32 = body[vat];
                    if (width > 32) return Error.ParquetEncodingUnsupported;
                    idx.clearRetainingCapacity();
                    try idx.resize(gpa, real);
                    var r = RleReader.init(body[vat + 1 ..], width);
                    if (!r.fill(idx.items)) return Error.ParquetMalformed;
                    for (idx.items) |*v| {
                        const k = dict_base + @as(usize, @intCast(v.*));
                        if (k >= dict.items.len) return Error.ParquetMalformed;
                        v.* = @intCast(k);
                    }
                    // Room for the whole page first, then appends that cannot
                    // fail: `append` checks capacity per value and does not
                    // inline, and at ten million values a column that call
                    // was more of the decode than the decoding.
                    if (dictionary) try index.ensureUnusedCapacity(gpa, n_vals) else try values.ensureUnusedCapacity(gpa, n_vals);
                    if (dictionary and real == n_vals) {
                        // Every value present and the column still indices: the
                        // page's indices are the column's, as they are. That is
                        // every page of a REQUIRED column and most of an
                        // OPTIONAL one.
                        index.appendSliceAssumeCapacity(idx.items);
                    } else {
                        var k: usize = 0;
                        var i: usize = 0;
                        while (i < n_vals) : (i += 1) {
                            const here = !optional or defs.items[i] != 0;
                            if (!here) {
                                if (dictionary) index.appendAssumeCapacity(Column.null_index) else values.appendAssumeCapacity(Slice.none);
                                continue;
                            }
                            const v = idx.items[k];
                            k += 1;
                            if (dictionary) {
                                index.appendAssumeCapacity(v);
                            } else {
                                values.appendAssumeCapacity(dict.items[@intCast(v)]);
                            }
                        }
                    }
                } else if (h.encoding == enc_plain) {
                    if (dictionary) {
                        // Fold what has been read into the plain form. This
                        // copies eight-byte handles, not values.
                        try values.ensureTotalCapacity(gpa, @max(@as(usize, @intCast(fm.rows)), index.items.len));
                        for (index.items) |k| {
                            values.appendAssumeCapacity(if (k >= 0) dict.items[@intCast(k)] else Slice.none);
                        }
                        index.clearAndFree(gpa);
                        dictionary = false;
                    }
                    got.clearRetainingCapacity();
                    try plainSlices(gpa, body[vat..], page_base + vat, @intCast(real), &got);
                    try values.ensureUnusedCapacity(gpa, n_vals);
                    if (real == n_vals) {
                        values.appendSliceAssumeCapacity(got.items);
                    } else {
                        var k: usize = 0;
                        var i: usize = 0;
                        while (i < n_vals) : (i += 1) {
                            const here = defs.items[i] != 0;
                            if (here) {
                                values.appendAssumeCapacity(got.items[k]);
                                k += 1;
                            } else values.appendAssumeCapacity(Slice.none);
                        }
                    }
                } else return Error.ParquetEncodingUnsupported;
                seen += h.num_values;
            } else if (h.ty == page_data_v2) {
                return Error.ParquetPageV2;
            } else if (h.ty != page_index) {
                return Error.ParquetMalformed;
            }
            at = h.after + body_len;
        }
    }

    // A column that never met a page at all is plain and empty, not dictionary.
    if (dictionary and index.items.len == 0 and dict.items.len == 0) dictionary = false;

    return Column{
        .dictionary = dictionary,
        .dict = try dict.toOwnedSlice(gpa),
        .index = try index.toOwnedSlice(gpa),
        .values = try values.toOwnedSlice(gpa),
        .owned = try owned.toOwnedSlice(gpa),
    };
}
