//! SWAR scanning: eight bytes per step, using arithmetic rather than comparison.
//!
//! A delimiter is found by broadcasting it across a word and subtracting ones:
//! the borrow crosses a byte only where that byte was zero, and `~diff` cancels
//! the false positives the borrow creates, so what survives marks exactly the
//! matching bytes.

const std = @import("std");
const build_options = @import("build_options");

/// How many bytes a scan step takes. Eight is SWAR -- ordinary 64-bit
/// arithmetic, no CPU feature at all -- and 32 or 64 puts the same question to a
/// vector register, which on x86 is AVX2 or AVX-512. `build.zig` picks 32 when
/// the target has AVX2 and 8 otherwise; `-Dscan` overrides it.
///
/// It is a build option rather than a runtime switch because a benchmark of an
/// instruction set should not be measuring a function pointer, and because each
/// binary is then what you would actually ship for that target.
const width = build_options.scan_width;

/// Whether this build scans with a vector register rather than SWAR. The
/// delimiter cursor below is worth having only here -- see [`Delims`].
pub const wide_scan = build_options.scan_width > 8;
const Vector = @Vector(width, u8);
const Mask = std.meta.Int(.unsigned, width);

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// The bytes of `chunk` equal to `target`, as one bit each.
inline fn matches(chunk: Vector, target: Vector) Mask {
    const equal: @Vector(width, bool) = chunk == target;
    return @bitCast(equal);
}

inline fn loadVector(data: []const u8, at: usize) Vector {
    return @bitCast(data[at..][0..width].*);
}

inline fn broadcast(b: u8) u64 {
    return @as(u64, b) *% ONES;
}

inline fn matchBits(word: u64, target: u64) u64 {
    const diff = word ^ target;
    return (diff -% ONES) & ~diff & HIGH;
}

/// The high bit of every byte of `word` equal to `target`, and of no other.
///
/// [`matchBits`] is the classic zero-byte trick, and it is exact only about the
/// *lowest* matching byte: subtracting ones from the whole word lets a borrow
/// out of a matching byte disturb the bytes above it. Everything that only ever
/// took `@ctz` of the result was therefore right, and a cursor that wants every
/// match in the word is not.
///
/// Forcing each byte's high bit before the subtraction is what stops the
/// borrow: no byte is then less than 0x80, so subtracting one never carries
/// into its neighbour. The result marks *non*-matching bytes, so it is
/// complemented.
inline fn matchBitsAll(word: u64, target: u64) u64 {
    const diff = word ^ target;
    return ~(diff | ((diff | HIGH) -% ONES)) & HIGH;
}

inline fn load64(data: []const u8, at: usize) u64 {
    return std.mem.readInt(u64, data[at..][0..8], .little);
}

/// The offset of the first byte at or after `from` that is `a` or `b`, or `end`.
pub fn nextOf2(data: []const u8, from: usize, end: usize, a: u8, b: u8) usize {
    var at = from;
    if (width > 8) {
        const va: Vector = @splat(a);
        const vb: Vector = @splat(b);
        while (at + width <= end) : (at += width) {
            const chunk = loadVector(data, at);
            const hits = matches(chunk, va) | matches(chunk, vb);
            if (hits != 0) return at + @ctz(hits);
        }
    }
    const ba = broadcast(a);
    const bb = broadcast(b);
    while (at + 8 <= end) : (at += 8) {
        const word = load64(data, at);
        const hits = matchBits(word, ba) | matchBits(word, bb);
        if (hits != 0) return at + (@ctz(hits) >> 3);
    }
    while (at < end) : (at += 1) {
        if (data[at] == a or data[at] == b) return at;
    }
    return end;
}

/// A left-to-right cursor over the delimiters of one row.
///
/// `nextOf2` begins a fresh scan at every field, so a chunk straddling a field
/// boundary is loaded once for the field that ends in it and again for the
/// field that starts there. Fields are found strictly in order, so the scan can
/// instead keep what it has -- the chunk it loaded and the matches in it that
/// no field has claimed yet -- and read every byte of the row once.
///
/// **Only worth it for a vector step, which is why `wide_scan` gates its use.**
/// Fields here average 9.2 bytes, so an eight-byte step rarely spans a boundary
/// and there is almost nothing to reuse: measured interleaved against the plain
/// scan, SWAR came out 0.7% slower in this port and 3.8% slower in Rust, the
/// bookkeeping costing more than it saves. A thirty-two-byte step covers three
/// and a half fields, so the plain scan loads the same bytes three or four
/// times over, and the cursor is 12.9% faster here and 15.0% in Rust.
///
/// Instruction count says the opposite of all that -- the exact mask below is
/// two more ALU operations per word, and callgrind reports the cursor as a
/// regression. What it removes is *loads*. This one had to be settled by the
/// clock.
///
/// `next` takes the position to resume from rather than assuming the previous
/// result, because a quoted field is walked by `skipQuoted` and the cursor has
/// to be told to seek past its body.
pub const Delims = struct {
    data: []const u8,
    end: usize,
    a: u8,
    b: u8,
    ba: u64,
    bb: u64,
    va: Vector = @splat(0),
    vb: Vector = @splat(0),
    /// Where the held chunk begins, and the matches in it not yet returned.
    held: usize = 0,
    bits: u64 = 0,
    /// Whether `bits` came from a vector compare (one bit per byte) or from
    /// SWAR (the high bit of each byte). Constant in a SWAR build.
    wide: bool = false,
    /// The first byte no chunk has covered yet.
    scanned: usize,

    pub fn init(data: []const u8, from: usize, end: usize, a: u8, b: u8) Delims {
        var d = Delims{
            .data = data,
            .end = end,
            .a = a,
            .b = b,
            .ba = broadcast(a),
            .bb = broadcast(b),
            .scanned = from,
        };
        if (width > 8) {
            d.va = @splat(a);
            d.vb = @splat(b);
        }
        return d;
    }

    inline fn lowest(self: Delims, bits: u64) usize {
        return self.held + if (self.wide) @ctz(bits) else (@ctz(bits) >> 3);
    }

    /// The offset of the first byte at or after `from` that is `a` or `b`, or
    /// `end` -- the same answer `nextOf2` gives, without rescanning.
    ///
    /// Inline, so the cursor's state can live in registers. As a call taking
    /// `*Delims` it lived on the stack, and a `perf` profile of the 10M CSV run
    /// on an EPYC 7763 put this function at 17% of the time on its own. C++
    /// measured the same cursor 10-14% faster once its state was in locals.
    pub inline fn next(self: *Delims, from: usize) usize {
        // Matches already found and still ahead of the caller.
        while (self.bits != 0) {
            const at = self.lowest(self.bits);
            self.bits &= self.bits - 1;
            if (at >= from) return at;
        }
        var at = if (from > self.scanned) from else self.scanned;
        if (width > 8) {
            while (at + width <= self.end) : (at += width) {
                const chunk = loadVector(self.data, at);
                var hits: u64 = matches(chunk, self.va) | matches(chunk, self.vb);
                self.held = at;
                self.wide = true;
                self.scanned = at + width;
                while (hits != 0) {
                    const idx = at + @ctz(hits);
                    hits &= hits - 1;
                    if (idx >= from) {
                        self.bits = hits;
                        return idx;
                    }
                }
            }
        }
        while (at + 8 <= self.end) : (at += 8) {
            const word = load64(self.data, at);
            var hits = matchBitsAll(word, self.ba) | matchBitsAll(word, self.bb);
            self.held = at;
            self.wide = false;
            self.scanned = at + 8;
            while (hits != 0) {
                const idx = at + (@ctz(hits) >> 3);
                hits &= hits - 1;
                if (idx >= from) {
                    self.bits = hits;
                    return idx;
                }
            }
        }
        var t = if (from > at) from else at;
        while (t < self.end) : (t += 1) {
            if (self.data[t] == self.a or self.data[t] == self.b) {
                self.scanned = t + 1;
                return t;
            }
        }
        self.scanned = self.end;
        return self.end;
    }
};

/// The offset of the first `target` at or after `from`, or `end`.
pub fn nextOf1(data: []const u8, from: usize, end: usize, target: u8) usize {
    var at = from;
    if (width > 8) {
        const vt: Vector = @splat(target);
        while (at + width <= end) : (at += width) {
            const hits = matches(loadVector(data, at), vt);
            if (hits != 0) return at + @ctz(hits);
        }
    }
    const bt = broadcast(target);
    while (at + 8 <= end) : (at += 8) {
        const word = load64(data, at);
        const hits = matchBits(word, bt);
        if (hits != 0) return at + (@ctz(hits) >> 3);
    }
    while (at < end) : (at += 1) {
        if (data[at] == target) return at;
    }
    return end;
}

/// Walks past a quoted field's body. A doubled quote inside it is content.
pub fn skipQuoted(data: []const u8, from: usize, end: usize) usize {
    var at = from;
    while (true) {
        const q = nextOf1(data, at, end, '"');
        if (q >= end) return end;
        if (q + 1 < end and data[q + 1] == '"') {
            at = q + 2;
            continue;
        }
        return q + 1;
    }
}

test "nextOf2 finds either byte, across the word boundary" {
    const d = "aaaaaaaaaaaa,x\nbb";
    try std.testing.expectEqual(@as(usize, 12), nextOf2(d, 0, d.len, ',', '\n'));
    try std.testing.expectEqual(@as(usize, 14), nextOf2(d, 13, d.len, ',', '\n'));
    try std.testing.expectEqual(d.len, nextOf2(d, 15, d.len, ',', '\n'));
}

test "skipQuoted treats a doubled quote as content" {
    const d = "\"a\"\"b\",rest";
    try std.testing.expectEqual(@as(usize, 6), skipQuoted(d, 1, d.len));
}

/// How many `target` bytes there are in `data[from..end]`.
///
/// Only the chunked index build needs this, and only for the quote character:
/// the number of quotes before a position is what says whether that position is
/// inside a quoted field. Counting whole words at a time keeps it far cheaper
/// than the parsing it makes parallel.
pub fn countByte(data: []const u8, from: usize, end: usize, target: u8) usize {
    var n: usize = 0;
    var at = from;
    if (width > 8) {
        const vt: Vector = @splat(target);
        while (at + width <= end) : (at += width) {
            n += @popCount(matches(loadVector(data, at), vt));
        }
    }
    const bt = broadcast(target);
    while (at + 8 <= end) : (at += 8) {
        // Every match in the word, not just the lowest -- see `matchBitsAll`.
        n += @popCount(matchBitsAll(load64(data, at), bt));
    }
    while (at < end) : (at += 1) {
        if (data[at] == target) n += 1;
    }
    return n;
}

test "countByte counts the same as one byte at a time" {
    const d = "a\"b\"\"c\"d\"e\"\"\"f";
    var from: usize = 0;
    while (from < d.len) : (from += 1) {
        var want: usize = 0;
        for (d[from..]) |b| {
            if (b == '"') want += 1;
        }
        try std.testing.expectEqual(want, countByte(d, from, d.len, '"'));
    }
}

test "a match followed by the next code point does not count twice" {
    // The borrow out of a matching byte lands on the byte above it, and shows
    // up exactly when that byte is `target ^ 1`. For a quote that is '#'; for a
    // comma it is '-', which is every negative number in a CSV. The classic
    // mask reports both bytes; this is the case that catches it.
    const cases = [_]struct { d: []const u8, t: u8, want: usize }{
        .{ .d = "\"#\"#\"#\"#abcdefgh", .t = '"', .want = 4 },
        .{ .d = "12,-45.67,-8,-9abcd", .t = ',', .want = 3 },
        .{ .d = ",-,-,-,-,-,-,-,-", .t = ',', .want = 8 },
    };
    for (cases) |c| {
        var want: usize = 0;
        for (c.d) |b| {
            if (b == c.t) want += 1;
        }
        try std.testing.expectEqual(c.want, want);
        try std.testing.expectEqual(want, countByte(c.d, 0, c.d.len, c.t));
    }
}

test "the delimiter cursor gives the same answers as a fresh scan" {
    // Deliberately full of `,-`, which is what makes a borrow-corrupted mask
    // report a delimiter one byte late as well as on time.
    const rows = [_][]const u8{
        "a,b,c\n",
        "12,-45.67,-8\n",
        ",,,\n",
        ",-,-,-,-,-,-,-,-,-,-\n",
        "one,two,three,four,five,six,seven,eight,nine,ten\n",
        "x\n",
    };
    for (rows) |d| {
        var cursor = Delims.init(d, 0, d.len, ',', '\n');
        var from: usize = 0;
        while (from <= d.len) {
            const want = nextOf2(d, from, d.len, ',', '\n');
            try std.testing.expectEqual(want, cursor.next(from));
            if (want >= d.len) break;
            from = want + 1;
        }
    }
}

/// The bytes from `at` to the end of `bytes`, fewer than eight, as a little-endian
/// word padded with zeros -- without copying them into a padded buffer first.
///
/// The copy was a variable-length `@memcpy`, which is a call into compiler_rt's
/// memcpy, once per key field of every row of both files. A field of eight bytes
/// or more has eight real bytes ending where the tail ends, so one load of those
/// and a shift that drops the ones already hashed gives the same word. A shorter
/// field is read as two overlapping halves, or as its first, middle and last
/// byte, which between them cover it for every length from one to seven.
pub fn tailWord(bytes: []const u8, at: usize) u64 {
    const n = bytes.len;
    const rem = n - at;
    std.debug.assert(rem > 0 and rem < 8);
    if (n >= 8) {
        const last = std.mem.readInt(u64, bytes[n - 8 ..][0..8], .little);
        return last >> @intCast(8 * (8 - rem));
    }
    // n < 8 means nothing was hashed yet, so the tail is the whole field.
    if (n >= 4) {
        const lo: u64 = std.mem.readInt(u32, bytes[0..4], .little);
        const hi: u64 = std.mem.readInt(u32, bytes[n - 4 ..][0..4], .little);
        return lo | (hi << @intCast(8 * (n - 4)));
    }
    return @as(u64, bytes[0]) |
        (@as(u64, bytes[n / 2]) << @intCast(8 * (n / 2))) |
        (@as(u64, bytes[n - 1]) << @intCast(8 * (n - 1)));
}

test "tailWord is the zero-padded tail for every length and offset" {
    var buf: [40]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @intCast(0x21 + i * 7 % 200);
    for (1..buf.len + 1) |n| {
        const bytes = buf[0..n];
        const at = n - n % 8;
        if (at == n) continue;
        var tail: [8]u8 = @splat(0);
        @memcpy(tail[0 .. n - at], bytes[at..]);
        try std.testing.expectEqual(std.mem.readInt(u64, &tail, .little), tailWord(bytes, at));
    }
}
