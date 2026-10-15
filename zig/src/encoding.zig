//! The integer encodings a Parquet page is written in.
//!
//! Three of them, and every one of the reader's value paths ends in one:
//! definition levels and dictionary indices are the RLE / bit-packed hybrid,
//! version-2 integer columns are delta binary packed, and version-2 string
//! columns are lengths in that same delta encoding followed by the bytes.
//!
//! They are here rather than beside the page loop because they are the part
//! worth testing on its own: an off-by-one in a bit-packed group is a wrong
//! value rather than a failure, and a wrong value in a comparison tool is the
//! worst kind of bug there is.

const std = @import("std");

pub const Error = error{ TruncatedValues, BadBlockSize, VarintTooLong };

/// The bits needed to hold every value up to `max`.
pub fn bitWidth(max: u32) u8 {
    return @intCast(32 - @clz(max));
}

fn varint(data: []const u8, at: *usize) !u64 {
    var out: u64 = 0;
    var shift: u6 = 0;
    while (true) {
        if (at.* >= data.len) return Error.TruncatedValues;
        const b = data[at.*];
        at.* += 1;
        out |= @as(u64, b & 0x7f) << shift;
        if (b & 0x80 == 0) return out;
        if (shift >= 63 - 7) return Error.VarintTooLong;
        shift += 7;
    }
}

fn zigzag(n: u64) i64 {
    return @as(i64, @bitCast(n >> 1)) ^ -@as(i64, @intCast(n & 1));
}

/// Reads `count` values of `width` bits each, packed least-significant bit first.
///
/// This is the layout both the hybrid and the delta encodings pack their groups
/// in: values run through the bytes without alignment, low bits first, so a
/// three-bit value can straddle a byte boundary and often does.
fn unpack(data: []const u8, width: u8, count: usize, out: []u32) !void {
    if (width == 0) {
        @memset(out[0..count], 0);
        return;
    }
    var bit: usize = 0;
    for (0..count) |i| {
        var value: u64 = 0;
        var taken: u8 = 0;
        while (taken < width) {
            const byte_at = bit / 8;
            if (byte_at >= data.len) return Error.TruncatedValues;
            const in_byte: u3 = @intCast(bit % 8);
            const available: u8 = 8 - @as(u8, in_byte);
            const want: u8 = @min(width - taken, available);
            const mask: u8 = if (want == 8) 0xff else (@as(u8, 1) << @intCast(want)) - 1;
            value |= @as(u64, (data[byte_at] >> in_byte) & mask) << @intCast(taken);
            taken += want;
            bit += want;
        }
        out[i] = @truncate(value);
    }
}

/// The RLE / bit-packed hybrid: alternating runs of one repeated value and
/// groups of eight packed ones, each introduced by a varint that says which.
pub const Hybrid = struct {
    gpa: std.mem.Allocator,
    data: []const u8,
    at: usize = 0,
    width: u8,
    /// Values decoded from the current run but not yet handed out.
    buffer: std.ArrayList(u32) = .empty,
    taken: usize = 0,

    pub fn init(gpa: std.mem.Allocator, data: []const u8, width: u8) Hybrid {
        return .{ .gpa = gpa, .data = data, .width = width };
    }

    pub fn deinit(self: *Hybrid) void {
        self.buffer.deinit(self.gpa);
    }

    /// The next value, or an error if the page runs out before the rows do.
    pub fn next(self: *Hybrid) !u32 {
        if (self.taken == self.buffer.items.len) {
            self.buffer.clearRetainingCapacity();
            self.taken = 0;
            try self.fill();
            if (self.buffer.items.len == 0) return Error.TruncatedValues;
        }
        const v = self.buffer.items[self.taken];
        self.taken += 1;
        return v;
    }

    fn fill(self: *Hybrid) !void {
        if (self.at >= self.data.len) return;
        const header = try varint(self.data, &self.at);
        if (header & 1 == 1) {
            // A bit-packed run: the header counts groups of eight.
            const groups: usize = @intCast(header >> 1);
            const count = groups * 8;
            const bytes = groups * self.width;
            if (self.at + bytes > self.data.len) return Error.TruncatedValues;
            const slice = self.data[self.at..][0..bytes];
            self.at += bytes;
            try self.buffer.resize(self.gpa, count);
            try unpack(slice, self.width, count, self.buffer.items);
        } else {
            // A run of one repeated value, written in whole bytes.
            const count: usize = @intCast(header >> 1);
            const bytes = (@as(usize, self.width) + 7) / 8;
            if (self.at + bytes > self.data.len) return Error.TruncatedValues;
            var value: u32 = 0;
            for (self.data[self.at..][0..bytes], 0..) |b, i| {
                value |= @as(u32, b) << @intCast(8 * i);
            }
            self.at += bytes;
            try self.buffer.appendNTimes(self.gpa, value, count);
        }
    }
};

pub const Delta = struct { values: []i64, used: usize };

/// Delta binary packed integers: a first value, then blocks of miniblocks, each
/// holding its deltas from the block's minimum in as few bits as they fit in.
///
/// Reports how many bytes it consumed, because a `DELTA_LENGTH_BYTE_ARRAY` page
/// has its bytes immediately after them.
pub fn deltaBinaryPacked(gpa: std.mem.Allocator, data: []const u8) !Delta {
    var at: usize = 0;
    const block_size: usize = @intCast(try varint(data, &at));
    const miniblocks: usize = @intCast(try varint(data, &at));
    const total: usize = @intCast(try varint(data, &at));
    const first = zigzag(try varint(data, &at));
    if (miniblocks == 0 or block_size == 0 or block_size % miniblocks != 0) return Error.BadBlockSize;
    const per_miniblock = block_size / miniblocks;

    var out: std.ArrayList(i64) = .empty;
    errdefer out.deinit(gpa);
    try out.ensureTotalCapacity(gpa, total);
    if (total > 0) try out.append(gpa, first);
    var value = first;

    const scratch = try gpa.alloc(u32, per_miniblock);
    defer gpa.free(scratch);
    const widths = try gpa.alloc(u8, miniblocks);
    defer gpa.free(widths);

    while (out.items.len < total) {
        const min_delta = zigzag(try varint(data, &at));
        if (at + miniblocks > data.len) return Error.TruncatedValues;
        @memcpy(widths, data[at..][0..miniblocks]);
        at += miniblocks;
        for (widths) |width| {
            if (out.items.len >= total) break;
            const bytes = per_miniblock * width / 8;
            if (at + bytes > data.len) return Error.TruncatedValues;
            const slice = data[at..][0..bytes];
            at += bytes;
            try unpack(slice, width, per_miniblock, scratch);
            for (scratch) |d| {
                if (out.items.len >= total) break;
                value +%= min_delta +% @as(i64, d);
                try out.append(gpa, value);
            }
        }
    }
    return .{ .values = try out.toOwnedSlice(gpa), .used = at };
}

test "a repeated run and a packed group read the same way" {
    // A run of four 1s, then one group of eight values 0..7 at three bits.
    const data = [_]u8{ 0x08, 0x01, 0x03, 0b1000_1000, 0b1100_0110, 0b1111_1010 };
    var hybrid = Hybrid.init(std.testing.allocator, &data, 3);
    defer hybrid.deinit();
    const want = [_]u32{ 1, 1, 1, 1, 0, 1, 2, 3, 4, 5, 6, 7 };
    for (want) |w| try std.testing.expectEqual(w, try hybrid.next());
}

test "a zero bit width is a run of zeroes rather than a read" {
    const data = [_]u8{ 0x10, 0x00 };
    var hybrid = Hybrid.init(std.testing.allocator, &data, 0);
    defer hybrid.deinit();
    try std.testing.expectEqual(@as(u32, 0), try hybrid.next());
}

test "running off the end is an error rather than a silent zero" {
    const data = [_]u8{ 0x04, 0x01 };
    var hybrid = Hybrid.init(std.testing.allocator, &data, 1);
    defer hybrid.deinit();
    _ = try hybrid.next();
    _ = try hybrid.next();
    try std.testing.expectError(Error.TruncatedValues, hybrid.next());
}

test "bit widths are the bits a value needs" {
    try std.testing.expectEqual(@as(u8, 0), bitWidth(0));
    try std.testing.expectEqual(@as(u8, 1), bitWidth(1));
    try std.testing.expectEqual(@as(u8, 3), bitWidth(7));
    try std.testing.expectEqual(@as(u8, 4), bitWidth(8));
}
