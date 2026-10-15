//! Page decompression: the four codecs a Parquet file in the wild actually uses.
//!
//! Snappy and LZ4 are written out here rather than pulled in, because both are
//! byte-copy loops of about eighty lines and neither is in the standard library.
//! Gzip and zstd are not written out: their decoders are real programs, and Zig
//! ships both.
//!
//! Every one of these writes into a buffer sized from the page header, so a
//! corrupt length is a refusal rather than an allocation the size of the number
//! that happened to be in the file. The buffer comes from the caller's
//! allocator, so `--max-memory` still bounds a Parquet comparison.

const std = @import("std");

pub const Error = error{
    UnsupportedCodec,
    TruncatedPage,
    CorruptPage,
    PageSizeMismatch,
};

/// Parquet's compression codec ids, from its Thrift schema.
pub const Codec = enum {
    none,
    snappy,
    gzip,
    zstd,
    lz4_raw,

    pub fn fromId(id: i32) !Codec {
        return switch (id) {
            0 => .none,
            1 => .snappy,
            2 => .gzip,
            6 => .zstd,
            7 => .lz4_raw,
            else => Error.UnsupportedCodec, // LZO, Brotli, and the deprecated LZ4 framing
        };
    }
};

/// Decompresses one page into a buffer sized from the page header.
/// The sliding window gzip and zstd decode through, kept for as long as a caller
/// has pages to read rather than allocated per page.
///
/// A zstd window is 8.13 MB. Allocating one per page is invisible on the clock --
/// 3% of CPU and nothing on the wall -- but this port runs on a fixed buffer that
/// hands memory back and cannot reuse it, so every window is spent for good. A
/// 1M-row zstd pair needed a `--max-memory` of 4800 MB to hold 291 MB of live
/// data; sharing one window across a column's pages brings that to 1600 MB.
pub const Scratch = struct {
    gpa: std.mem.Allocator,
    window: []u8 = &.{},

    pub fn deinit(self: *Scratch) void {
        self.gpa.free(self.window);
        self.window = &.{};
    }

    /// Grows to the high-water mark and stays there: the codecs below ask for one
    /// of two fixed sizes, so this allocates at most twice in a column's life.
    fn atLeast(self: *Scratch, bytes: usize) ![]u8 {
        if (self.window.len < bytes) {
            self.gpa.free(self.window);
            self.window = &.{};
            self.window = try self.gpa.alloc(u8, bytes);
        }
        return self.window[0..bytes];
    }
};

pub fn decompress(
    gpa: std.mem.Allocator,
    scratch: *Scratch,
    codec: Codec,
    input: []const u8,
    expected: usize,
) ![]u8 {
    const out = try gpa.alloc(u8, expected);
    errdefer gpa.free(out);
    switch (codec) {
        .none => {
            if (input.len < expected) return Error.PageSizeMismatch;
            @memcpy(out, input[0..expected]);
        },
        .snappy => try snappy(input, out),
        .lz4_raw => try lz4(input, out),
        .gzip => try inflate(scratch, input, out),
        .zstd => try unzstd(scratch, input, out),
    }
    return out;
}

/// A cursor that fills `out` and refuses to run past it, which is what turns a
/// corrupt length into an error rather than a wild write.
const Sink = struct {
    out: []u8,
    at: usize = 0,

    fn literal(self: *Sink, bytes: []const u8) !void {
        if (self.at + bytes.len > self.out.len) return Error.PageSizeMismatch;
        @memcpy(self.out[self.at..][0..bytes.len], bytes);
        self.at += bytes.len;
    }

    /// Copies `len` bytes from `distance` back, which may overlap what it is
    /// writing: a run of one byte is encoded as a one-byte match repeated.
    fn copy(self: *Sink, distance: usize, len: usize) !void {
        if (distance == 0 or distance > self.at) return Error.CorruptPage;
        if (self.at + len > self.out.len) return Error.PageSizeMismatch;
        var from = self.at - distance;
        for (0..len) |_| {
            self.out[self.at] = self.out[from];
            self.at += 1;
            from += 1;
        }
    }
};

/// Snappy's raw block format: a length, then literals and back-references.
fn snappy(input: []const u8, out: []u8) !void {
    var at: usize = 0;
    var declared: usize = 0;
    var shift: u6 = 0;
    while (true) {
        if (at >= input.len) return Error.TruncatedPage;
        const b = input[at];
        at += 1;
        declared |= @as(usize, b & 0x7f) << shift;
        if (b & 0x80 == 0) break;
        if (shift >= 28) return Error.CorruptPage;
        shift += 7;
    }
    if (declared != out.len) return Error.PageSizeMismatch;

    var sink = Sink{ .out = out };
    while (at < input.len) {
        const tag = input[at];
        at += 1;
        if (tag & 0x03 == 0) {
            // A literal: a short length in the tag, or one to four bytes of it.
            var len: usize = tag >> 2;
            if (len >= 60) {
                const extra = len - 59;
                if (at + extra > input.len) return Error.TruncatedPage;
                len = 0;
                for (input[at..][0..extra], 0..) |b, i| len |= @as(usize, b) << @intCast(8 * i);
                at += extra;
            }
            len += 1;
            if (at + len > input.len) return Error.TruncatedPage;
            try sink.literal(input[at..][0..len]);
            at += len;
            continue;
        }
        var len: usize = 0;
        var distance: usize = 0;
        switch (tag & 0x03) {
            1 => {
                if (at >= input.len) return Error.TruncatedPage;
                len = 4 + ((tag >> 2) & 0x07);
                distance = (@as(usize, tag >> 5) << 8) | input[at];
                at += 1;
            },
            2 => {
                if (at + 2 > input.len) return Error.TruncatedPage;
                len = @as(usize, tag >> 2) + 1;
                distance = std.mem.readInt(u16, input[at..][0..2], .little);
                at += 2;
            },
            else => {
                if (at + 4 > input.len) return Error.TruncatedPage;
                len = @as(usize, tag >> 2) + 1;
                distance = std.mem.readInt(u32, input[at..][0..4], .little);
                at += 4;
            },
        }
        try sink.copy(distance, len);
    }
    if (sink.at != out.len) return Error.PageSizeMismatch;
}

/// LZ4's block format: a token, literals, then a two-byte back-reference.
fn lz4(input: []const u8, out: []u8) !void {
    var sink = Sink{ .out = out };
    var at: usize = 0;
    while (at < input.len) {
        const token = input[at];
        at += 1;
        var literals: usize = token >> 4;
        if (literals == 15) {
            while (true) {
                if (at >= input.len) return Error.TruncatedPage;
                const b = input[at];
                at += 1;
                literals += b;
                if (b != 255) break;
            }
        }
        if (at + literals > input.len) return Error.TruncatedPage;
        try sink.literal(input[at..][0..literals]);
        at += literals;
        // The last sequence of a block is literals only, with no match after it.
        if (at >= input.len) break;
        if (at + 2 > input.len) return Error.TruncatedPage;
        const distance = std.mem.readInt(u16, input[at..][0..2], .little);
        at += 2;
        var len: usize = token & 0x0f;
        if (len == 15) {
            while (true) {
                if (at >= input.len) return Error.TruncatedPage;
                const b = input[at];
                at += 1;
                len += b;
                if (b != 255) break;
            }
        }
        try sink.copy(distance, len + 4);
    }
    if (sink.at != out.len) return Error.PageSizeMismatch;
}

fn inflate(scratch: *Scratch, input: []const u8, out: []u8) !void {
    const window = try scratch.atLeast(std.compress.flate.max_window_len);
    var in = std.Io.Reader.fixed(input);
    var d = std.compress.flate.Decompress.init(&in, .gzip, window);
    var w = std.Io.Writer.fixed(out);
    const n = d.reader.streamRemaining(&w) catch return Error.CorruptPage;
    if (n != out.len) return Error.PageSizeMismatch;
}

fn unzstd(scratch: *Scratch, input: []const u8, out: []u8) !void {
    const window = try scratch.atLeast(
        std.compress.zstd.default_window_len + std.compress.zstd.block_size_max,
    );
    var in = std.Io.Reader.fixed(input);
    var d = std.compress.zstd.Decompress.init(&in, window, .{ .verify_checksum = false });
    var w = std.Io.Writer.fixed(out);
    const n = d.reader.streamRemaining(&w) catch return Error.CorruptPage;
    if (n != out.len) return Error.PageSizeMismatch;
}

test "snappy reads literals and back-references" {
    // "abcabcabcab": a three-byte literal then a copy that overlaps itself.
    const block = [_]u8{ 0x0b, 0x08, 'a', 'b', 'c', 0x11, 0x03 };
    var out: [11]u8 = undefined;
    try snappy(&block, &out);
    try std.testing.expectEqualStrings("abcabcabcab", &out);
}

test "a page that decompresses to the wrong size is refused" {
    const block = [_]u8{ 0x0b, 0x08, 'a', 'b', 'c', 0x11, 0x03 };
    var out: [12]u8 = undefined;
    try std.testing.expectError(Error.PageSizeMismatch, snappy(&block, &out));
}

test "a back-reference before the page is refused rather than read" {
    const block = [_]u8{ 0x04, 0x01, 0x0a };
    var out: [4]u8 = undefined;
    try std.testing.expectError(Error.CorruptPage, snappy(&block, &out));
}

test "lz4 reads literals and matches" {
    const block = [_]u8{ 0x30, 'a', 'b', 'c', 0x03, 0x00 };
    var out: [7]u8 = undefined;
    try lz4(&block, &out);
    try std.testing.expectEqualStrings("abcabca", &out);
}

test "one window serves a column's pages, rather than one each" {
    // The port runs on a fixed buffer that cannot reuse what it hands back, so a
    // window allocated per page is spent for good. Twenty-four pages at flate's
    // 64 KB window is 1.5 MB of it; through one `Scratch` it is 64 KB, and this
    // budget holds only the latter.
    const page = [_]u8{
        0x1f, 0x8b, 0x08, 0x00, 0x9f, 0x22, 0xa2, 0x6a, 0x02, 0xff, 0xd5, 0xcc,
        0xd1, 0x0d, 0x80, 0x20, 0x0c, 0x45, 0xd1, 0x55, 0xde, 0x00, 0xc6, 0x9d,
        0x4a, 0x79, 0x18, 0x22, 0xa6, 0xa5, 0xa0, 0x89, 0xdb, 0xcb, 0x1a, 0xfe,
        0xdc, 0xf3, 0x77, 0x75, 0x3c, 0xb9, 0x96, 0x02, 0x97, 0xe8, 0x37, 0xe7,
        0xf2, 0xe0, 0xca, 0xdb, 0x4c, 0xf2, 0x86, 0xa0, 0x53, 0x26, 0x33, 0x8a,
        0x05, 0x04, 0x6a, 0x97, 0x07, 0xc7, 0xa8, 0xa9, 0x11, 0xa9, 0x99, 0x9e,
        0x3b, 0xf4, 0xff, 0x83, 0x0f, 0x8d, 0xf0, 0x29, 0x58, 0x04, 0x01, 0x00,
        0x00,
    };
    const raw = "csvdiff parquet page payload, repeated for a compressible block. " ** 4;

    var buffer: [512 * 1024]u8 = undefined;
    var fixed = std.heap.FixedBufferAllocator.init(&buffer);
    const gpa = fixed.allocator();

    var scratch = Scratch{ .gpa = gpa };
    defer scratch.deinit();
    for (0..24) |_| {
        const out = try decompress(gpa, &scratch, .gzip, &page, raw.len);
        defer gpa.free(out);
        try std.testing.expectEqualStrings(raw, out);
    }
}

test "a scratch grows to the largest window asked of it and stops" {
    var scratch = Scratch{ .gpa = std.testing.allocator };
    defer scratch.deinit();
    const small = try scratch.atLeast(1024);
    try std.testing.expectEqual(@as(usize, 1024), small.len);
    const big = try scratch.atLeast(4096);
    try std.testing.expectEqual(@as(usize, 4096), big.len);
    // Asking for less than it holds reuses the buffer rather than shrinking it.
    const again = try scratch.atLeast(1024);
    try std.testing.expectEqual(@as(usize, 1024), again.len);
    try std.testing.expectEqual(@as(usize, 4096), scratch.window.len);
}
