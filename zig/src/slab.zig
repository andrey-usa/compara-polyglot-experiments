//! The bytes a field points into, and the one route every comparison reads them
//! through.
//!
//! A field is an offset and a length, so something has to say what those address
//! and how the bytes are escaped. For CSV and JSON that is the mapped file; for
//! Parquet it is an arena of decoded values, which is the same thing to everyone
//! downstream. The escape rule rides along because it follows the format: CSV
//! doubles a quote, JSON puts a backslash in front of one, and a decoded Parquet
//! value is already literal.
//!
//! Hashing, equality and decoding all read a field through `logical`, so they
//! cannot come to different conclusions about the same value — the property whose
//! absence produced two silently wrong answers in the Java port.

const std = @import("std");
const builtin = @import("builtin");
const f = @import("field.zig");
const Field = f.Field;

pub const Error = error{CannotReadFile};

/// Which escape rule the bytes behind a field follow.
pub const Dialect = enum {
    /// A doubled quote is one quote.
    csv,
    /// A backslash escape, `\uXXXX` included.
    json,
    /// The bytes are the value: nothing to undo. Parquet decodes to this.
    raw,
};

pub const Slab = struct {
    data: []const u8,
    dialect: Dialect = .csv,
    /// Set when the bytes are a mapping, so they can be given back.
    mapping: ?[]align(std.heap.page_size_min) const u8 = null,
    /// Set when the bytes are an arena this process built.
    arena: ?[]u8 = null,
    gpa: ?std.mem.Allocator = null,

    /// Maps a file read-only. The whole file is read once, front to back.
    pub fn map(io: std.Io, path: []const u8) !Slab {
        const file = std.Io.Dir.cwd().openFile(io, path, .{}) catch return Error.CannotReadFile;
        defer file.close(io);
        const size = (file.stat(io) catch return Error.CannotReadFile).size;
        if (size == 0) return Slab{ .data = &[_]u8{} };
        const mapped = mapHandle(file.handle, size) catch return Error.CannotReadFile;
        return Slab{ .data = mapped, .mapping = mapped };
    }

    /// A slab over bytes this process built: the Parquet reader's arena.
    pub fn owned(gpa: std.mem.Allocator, bytes: []u8, dialect: Dialect) Slab {
        return .{ .data = bytes, .dialect = dialect, .arena = bytes, .gpa = gpa };
    }

    pub fn close(self: *Slab) void {
        if (self.mapping) |m| unmap(m);
        if (self.arena) |a| {
            if (self.gpa) |gpa| gpa.free(a);
        }
        self.* = .{ .data = &[_]u8{} };
    }

    /// The field's raw span, still holding whatever escapes it was written with.
    pub fn raw(self: Slab, field: Field) []const u8 {
        if (!f.isReal(field)) return &[_]u8{};
        return self.data[f.offsetOf(field)..][0..f.lenOf(field)];
    }

    /// The field's logical bytes: the value, with its escapes undone.
    pub fn logical(self: Slab, field: Field) Logical {
        const escaped = f.isReal(field) and f.isEscaped(field);
        return .{
            .raw = self.raw(field),
            .dialect = if (escaped) self.dialect else .raw,
        };
    }
};

/// The one call this port makes that the two platforms spell differently, and
/// the only reason there is a switch anywhere in this file.
///
/// The POSIX side is `mmap`. The Windows side is a section object plus a view of
/// it, declared here rather than taken from `std.os.windows`, which carries no
/// binding for either call. Both give back the same thing: the whole file,
/// read-only, private, mapped at offset zero -- which is the only mapping this
/// port ever asks for.
const Mapping = []align(std.heap.page_size_min) const u8;

const PAGE_READONLY: std.os.windows.DWORD = 0x02;
const FILE_MAP_READ: std.os.windows.DWORD = 0x0004;

extern "kernel32" fn CreateFileMappingW(
    hFile: std.os.windows.HANDLE,
    lpAttributes: ?*anyopaque,
    flProtect: std.os.windows.DWORD,
    dwMaximumSizeHigh: std.os.windows.DWORD,
    dwMaximumSizeLow: std.os.windows.DWORD,
    lpName: ?[*:0]const u16,
) callconv(.winapi) ?std.os.windows.HANDLE;

extern "kernel32" fn MapViewOfFile(
    hFileMappingObject: std.os.windows.HANDLE,
    dwDesiredAccess: std.os.windows.DWORD,
    dwFileOffsetHigh: std.os.windows.DWORD,
    dwFileOffsetLow: std.os.windows.DWORD,
    dwNumberOfBytesToMap: usize,
) callconv(.winapi) ?std.os.windows.LPVOID;

extern "kernel32" fn UnmapViewOfFile(lpBaseAddress: ?*const anyopaque) callconv(.winapi) std.os.windows.BOOL;

fn mapHandle(handle: std.Io.File.Handle, size: u64) !Mapping {
    const len: usize = @intCast(size);
    switch (builtin.os.tag) {
        .windows => {
            // A zero maximum size means "as large as the file", which is what
            // the length already says.
            const section = CreateFileMappingW(handle, null, PAGE_READONLY, 0, 0, null) orelse
                return Error.CannotReadFile;
            // The view takes its own reference to the section, so the handle can
            // go now and the mapping still lives until UnmapViewOfFile. Closing
            // it here is what keeps this from leaking one handle per file.
            defer std.os.windows.CloseHandle(section);
            const view = MapViewOfFile(section, FILE_MAP_READ, 0, 0, len) orelse
                return Error.CannotReadFile;
            const bytes: [*]align(std.heap.page_size_min) const u8 = @ptrCast(@alignCast(view));
            return bytes[0..len];
        },
        else => {
            const m = std.posix.mmap(
                null,
                len,
                .{ .READ = true },
                .{ .TYPE = .PRIVATE },
                handle,
                0,
            ) catch return Error.CannotReadFile;
            // Every pass reads the file front to back: the sweep, and the join
            // after it, whose chunks are A's rows in order. Saying so doubles the
            // kernel's readahead and lets it drop pages behind the reader, which
            // is what matters once the pair is larger than memory -- without it
            // the join evicted pages it was about to need and ran at half the
            // disk's rate. The other three ports have always asked. A hint the
            // kernel may ignore, so a refusal costs nothing.
            std.posix.madvise(@constCast(m.ptr), m.len, std.posix.MADV.SEQUENTIAL) catch {};
            return m;
        },
    }
}

fn unmap(m: Mapping) void {
    switch (builtin.os.tag) {
        .windows => _ = UnmapViewOfFile(m.ptr),
        else => std.posix.munmap(m),
    }
}

/// A field's bytes with its escapes undone, one byte at a time and without
/// allocating. A `\uXXXX` escape decodes to as many as four bytes, which is what
/// the small pending buffer is for.
pub const Logical = struct {
    raw: []const u8,
    dialect: Dialect,
    at: usize = 0,
    pending: [4]u8 = undefined,
    pending_len: u8 = 0,
    pending_at: u8 = 0,

    /// True when the bytes are the value, so a caller may take the slice whole.
    pub fn isPlain(self: Logical) bool {
        return self.dialect == .raw;
    }

    pub fn next(self: *Logical) ?u8 {
        if (self.pending_at < self.pending_len) {
            const b = self.pending[self.pending_at];
            self.pending_at += 1;
            return b;
        }
        if (self.at >= self.raw.len) return null;
        const b = self.raw[self.at];
        self.at += 1;
        switch (self.dialect) {
            .raw => return b,
            .csv => {
                // A quote inside a quoted body can only be half of a doubled
                // pair, so the second one is dropped.
                if (b == '"' and self.at < self.raw.len and self.raw[self.at] == '"') self.at += 1;
                return b;
            },
            .json => {
                if (b != '\\' or self.at >= self.raw.len) return b;
                const e = self.raw[self.at];
                self.at += 1;
                return switch (e) {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => 0x08,
                    'f' => 0x0c,
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'u' => blk: {
                        if (self.takeEscape()) |cp| break :blk self.hold(cp);
                        // Not four hex digits: it was never an escape, so both
                        // bytes are the value.
                        break :blk self.holdByte('\\', 'u');
                    },
                    // Not an escape this dialect defines: both bytes are content.
                    else => self.holdByte('\\', e),
                };
            },
        }
    }

    fn holdByte(self: *Logical, first: u8, second: u8) u8 {
        self.pending[0] = second;
        self.pending_len = 1;
        self.pending_at = 0;
        return first;
    }

    /// Encodes one code point, returning its first byte and holding the rest.
    ///
    /// Written out rather than taken from `std.unicode`, which refuses an
    /// unpaired surrogate: a file carrying one is malformed, but the C, C++ and
    /// Rust ports encode it as written rather than substituting a replacement
    /// character, and two ports that disagree about the bytes of a value would
    /// disagree about its hash.
    fn hold(self: *Logical, cp: u21) u8 {
        var buf: [4]u8 = undefined;
        var n: u8 = 0;
        if (cp < 0x80) {
            buf[0] = @intCast(cp);
            n = 1;
        } else if (cp < 0x800) {
            buf[0] = @intCast(0xC0 | (cp >> 6));
            buf[1] = @intCast(0x80 | (cp & 0x3F));
            n = 2;
        } else if (cp < 0x10000) {
            buf[0] = @intCast(0xE0 | (cp >> 12));
            buf[1] = @intCast(0x80 | ((cp >> 6) & 0x3F));
            buf[2] = @intCast(0x80 | (cp & 0x3F));
            n = 3;
        } else {
            buf[0] = @intCast(0xF0 | (cp >> 18));
            buf[1] = @intCast(0x80 | ((cp >> 12) & 0x3F));
            buf[2] = @intCast(0x80 | ((cp >> 6) & 0x3F));
            buf[3] = @intCast(0x80 | (cp & 0x3F));
            n = 4;
        }
        self.pending = buf;
        self.pending_len = n;
        self.pending_at = 1;
        return buf[0];
    }

    /// The code point of the `\u` escape whose digits start at `at`, with a
    /// following low surrogate folded in. Leaves `at` past what it consumed.
    fn takeEscape(self: *Logical) ?u21 {
        const hi = self.takeHex4() orelse return null;
        if (hi < 0xD800 or hi > 0xDBFF) return @intCast(hi);
        // A surrogate pair is one code point written as two escapes.
        if (self.at + 1 < self.raw.len and self.raw[self.at] == '\\' and self.raw[self.at + 1] == 'u') {
            const save = self.at;
            self.at += 2;
            if (self.takeHex4()) |lo| {
                if (lo >= 0xDC00 and lo <= 0xDFFF) {
                    return @intCast(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                }
            }
            self.at = save;
        }
        return @intCast(hi);
    }

    fn takeHex4(self: *Logical) ?u32 {
        if (self.at + 4 > self.raw.len) return null;
        var cp: u32 = 0;
        for (self.raw[self.at..][0..4]) |d| {
            const v: u32 = switch (d) {
                '0'...'9' => d - '0',
                'a'...'f' => d - 'a' + 10,
                'A'...'F' => d - 'A' + 10,
                else => return null,
            };
            cp = cp * 16 + v;
        }
        self.at += 4;
        return cp;
    }
};

/// Whether two fields hold the same logical bytes, without decoding either.
pub fn sameBytes(a: Slab, x: Field, b: Slab, y: Field) bool {
    // Ask the two field words directly rather than building two iterators to ask
    // them. `logical()` already reports a field with no escape as plain, so the
    // memcmp below was always the path a CSV file took -- but reaching it cost
    // two `Logical` values, each slicing the slab and carrying a four-byte
    // pending buffer, to answer a question two bit tests answer. For a
    // nine-byte field that setup was most of the comparison: 0.76 billion
    // instructions of a 4.3 billion run, down to 0.53 billion.
    if (!f.isEscaped(x) and !f.isEscaped(y)) return std.mem.eql(u8, a.raw(x), b.raw(y));
    var lx = a.logical(x);
    var ly = b.logical(y);
    while (true) {
        const cx = lx.next();
        const cy = ly.next();
        if (cx == null and cy == null) return true;
        if (cx == null or cy == null) return false;
        if (cx.? != cy.?) return false;
    }
}

fn collect(text: []const u8, dialect: Dialect, out: []u8) []const u8 {
    const slab = Slab{ .data = text, .dialect = dialect };
    var it = slab.logical(f.pack(0, text.len, true));
    var n: usize = 0;
    while (it.next()) |b| : (n += 1) out[n] = b;
    return out[0..n];
}

test "csv drops the second quote of a pair" {
    var buf: [64]u8 = undefined;
    try std.testing.expectEqualStrings("a\"b", collect("a\"\"b", .csv, &buf));
}

test "json undoes its escapes" {
    var buf: [64]u8 = undefined;
    try std.testing.expectEqualStrings("a\nb\"c\\d", collect("a\\nb\\\"c\\\\d", .json, &buf));
}

test "a unicode escape and the character itself are the same value" {
    var buf: [64]u8 = undefined;
    try std.testing.expectEqualStrings("caf\u{e9}", collect("caf\\u00e9", .json, &buf));
    try std.testing.expectEqualStrings("\u{1f600}", collect("\\ud83d\\ude00", .json, &buf));
    // Not four hex digits, so it was never an escape.
    try std.testing.expectEqualStrings("\\uZZ", collect("\\uZZ", .json, &buf));
    // A lone high surrogate is encoded as written, which is what the other ports
    // do, and must not eat the character after it.
    try std.testing.expectEqualStrings("\xed\xa0\xbdx", collect("\\ud83dx", .json, &buf));
}
