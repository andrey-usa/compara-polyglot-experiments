//! Just enough of Thrift's compact protocol to read a Parquet footer.
//!
//! Parquet's metadata is a Thrift structure, and a Thrift library would bring a
//! code generator and a dependency for what is, at this scale, a few hundred
//! lines: field headers, zigzag varints, and the rule for skipping a field this
//! reader does not care about. Skipping correctly is the part that matters --
//! writers add fields, and a reader that cannot step over an unknown one breaks
//! on files it has never seen.

const std = @import("std");

pub const Error = error{ TruncatedMetadata, UnknownThriftType };

/// Thrift compact type codes, as they appear in the low nibble of a field header.
pub const STOP: u8 = 0;
pub const TRUE: u8 = 1;
pub const FALSE: u8 = 2;
pub const BYTE: u8 = 3;
pub const I16: u8 = 4;
pub const I32: u8 = 5;
pub const I64: u8 = 6;
pub const DOUBLE: u8 = 7;
pub const BINARY: u8 = 8;
pub const LIST: u8 = 9;
pub const SET: u8 = 10;
pub const MAP: u8 = 11;
pub const STRUCT: u8 = 12;

/// One field header: which field, and what it holds.
pub const FieldHeader = struct { id: i16, kind: u8 };

/// A cursor over the bytes of a compact-protocol message.
pub const Reader = struct {
    data: []const u8,
    at: usize = 0,
    /// Field ids are deltas from the previous one within a struct, so the reader
    /// carries the last id seen and stacks it across nested structs.
    last_id: i16 = 0,
    stack: [16]i16 = undefined,
    depth: usize = 0,

    pub fn init(data: []const u8) Reader {
        return .{ .data = data };
    }

    fn byte(self: *Reader) !u8 {
        if (self.at >= self.data.len) return Error.TruncatedMetadata;
        const b = self.data[self.at];
        self.at += 1;
        return b;
    }

    fn varint(self: *Reader) !u64 {
        var out: u64 = 0;
        var shift: u6 = 0;
        while (true) {
            const b = try self.byte();
            out |= @as(u64, b & 0x7f) << shift;
            if (b & 0x80 == 0) return out;
            if (shift >= 63 - 7) return Error.TruncatedMetadata;
            shift += 7;
        }
    }

    fn zigzag(self: *Reader) !i64 {
        const n = try self.varint();
        return @as(i64, @bitCast(n >> 1)) ^ -@as(i64, @intCast(n & 1));
    }

    /// Enters a struct. Field ids are relative to the enclosing struct's last id,
    /// so every `structBegin` needs its `structEnd`.
    pub fn structBegin(self: *Reader) void {
        if (self.depth < self.stack.len) self.stack[self.depth] = self.last_id;
        self.depth += 1;
        self.last_id = 0;
    }

    pub fn structEnd(self: *Reader) void {
        if (self.depth > 0) self.depth -= 1;
        self.last_id = if (self.depth < self.stack.len) self.stack[self.depth] else 0;
    }

    /// The next field of the current struct, or null at its stop byte.
    pub fn field(self: *Reader) !?FieldHeader {
        const header = try self.byte();
        if (header == STOP) return null;
        const kind = header & 0x0f;
        const delta: i16 = @intCast(header >> 4);
        const id = if (delta == 0) @as(i16, @intCast(try self.zigzag())) else self.last_id + delta;
        self.last_id = id;
        return .{ .id = id, .kind = kind };
    }

    pub fn i32v(self: *Reader) !i32 {
        return @truncate(try self.zigzag());
    }

    pub fn i64v(self: *Reader) !i64 {
        return self.zigzag();
    }

    pub fn binary(self: *Reader) ![]const u8 {
        const len: usize = @intCast(try self.varint());
        if (self.at + len > self.data.len) return Error.TruncatedMetadata;
        const out = self.data[self.at..][0..len];
        self.at += len;
        return out;
    }

    /// A list header: how many elements, and of what.
    pub fn listBegin(self: *Reader) !struct { usize, u8 } {
        const header = try self.byte();
        const kind = header & 0x0f;
        var size: usize = header >> 4;
        if (size == 15) size = @intCast(try self.varint());
        return .{ size, kind };
    }

    /// Steps over a value of `kind` without interpreting it. This is what lets a
    /// file written by a newer library, carrying fields this reader has never
    /// heard of, still be read.
    pub fn skip(self: *Reader, kind: u8) !void {
        switch (kind) {
            TRUE, FALSE => {},
            BYTE => _ = try self.byte(),
            I16, I32, I64 => _ = try self.zigzag(),
            DOUBLE => {
                if (self.at + 8 > self.data.len) return Error.TruncatedMetadata;
                self.at += 8;
            },
            BINARY => _ = try self.binary(),
            LIST, SET => {
                const n, const element = try self.listBegin();
                for (0..n) |_| try self.skip(element);
            },
            MAP => {
                const n: usize = @intCast(try self.varint());
                if (n == 0) return;
                const kinds = try self.byte();
                for (0..n) |_| {
                    try self.skip(kinds >> 4);
                    try self.skip(kinds & 0x0f);
                }
            },
            STRUCT => {
                self.structBegin();
                while (try self.field()) |fh| try self.skip(fh.kind);
                self.structEnd();
            },
            else => return Error.UnknownThriftType,
        }
    }

    /// How far the cursor has got, which a page header reader needs to know where
    /// the page's own bytes begin.
    pub fn position(self: Reader) usize {
        return self.at;
    }
};

test "fields are read by delta and values unzigzagged" {
    // {1: 7, 2: "hi", 3: [4, 5]} in compact form, then a stop byte.
    const message = [_]u8{ 0x15, 0x0e, 0x18, 0x02, 'h', 'i', 0x19, 0x25, 0x08, 0x0a, 0x00 };
    var r = Reader.init(&message);
    r.structBegin();
    var seen: usize = 0;
    while (try r.field()) |fh| {
        switch (fh.id) {
            1 => try std.testing.expectEqual(@as(i32, 7), try r.i32v()),
            2 => try std.testing.expectEqualStrings("hi", try r.binary()),
            3 => {
                const n, _ = try r.listBegin();
                try std.testing.expectEqual(@as(usize, 2), n);
                try std.testing.expectEqual(@as(i32, 4), try r.i32v());
                try std.testing.expectEqual(@as(i32, 5), try r.i32v());
            },
            else => try r.skip(fh.kind),
        }
        seen += 1;
    }
    r.structEnd();
    try std.testing.expectEqual(@as(usize, 3), seen);
}

test "a field nobody wants is stepped over" {
    const message = [_]u8{ 0x15, 0x0e, 0x18, 0x02, 'h', 'i', 0x19, 0x25, 0x08, 0x0a, 0x00 };
    var r = Reader.init(&message);
    r.structBegin();
    var last: ?i32 = null;
    while (try r.field()) |fh| {
        if (fh.id == 3) {
            _ = try r.listBegin();
            _ = try r.i32v();
            last = try r.i32v();
        } else try r.skip(fh.kind);
    }
    r.structEnd();
    try std.testing.expectEqual(@as(?i32, 5), last);
}
