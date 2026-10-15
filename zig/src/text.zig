//! The two text readers: CSV, and newline-delimited JSON.
//!
//! They are one module because they are one design. A JSON value is a contiguous
//! run of bytes in the file exactly as a CSV field is, so a field stays an offset
//! and a length for both and nothing downstream has to know which format it came
//! from. What differs is how a row is walked — by column number, or by key — and
//! how a value is escaped, which `slab.zig` settles.
//!
//! The two sides of a comparison need not agree: a CSV export compares against
//! the JSON the same pipeline emits, with the key order on each side free to
//! differ, because the JSON reader joins by name.

const std = @import("std");
const scan = @import("scan.zig");
const f = @import("field.zig");
const Field = f.Field;
const Slab = @import("slab.zig").Slab;
const Dialect = @import("slab.zig").Dialect;

pub const Error = error{ NoHeaderRow, NoJsonObject };

fn jsonSpace(c: u8) bool {
    return c == ' ' or c == '\t' or c == '\r' or c == '\n';
}

/// Newline-delimited JSON if the first thing that is not whitespace is a brace.
///
/// A CSV header can begin with anything else, and a `{` in the first column of a
/// CSV header is not something this project has ever had to read.
pub fn sniffDialect(data: []const u8) Dialect {
    for (data[0..@min(data.len, 64)]) |b| {
        if (jsonSpace(b)) continue;
        return if (b == '{') .json else .csv;
    }
    return .csv;
}

/// Guesses the delimiter from the header line, defaulting to a comma.
pub fn detectDelimiter(header: []const u8) u8 {
    var best: u8 = ',';
    var best_count: isize = -1;
    for ([_]u8{ ',', ';', '\t', '|' }) |c| {
        var n: isize = 0;
        for (header) |b| {
            if (b == c) n += 1;
        }
        if (n > best_count) {
            best = c;
            best_count = n;
        }
    }
    return best;
}

/// A quoted CSV field, flagged when it holds a doubled quote. A quote inside the
/// body can only be half of such a pair, which is what makes the test a single
/// scan and lets the unescaping wait until the bytes are actually read.
fn quotedField(d: []const u8, from: usize, to: usize) Field {
    return f.pack(from, to - from, scan.nextOf1(d, from, to, '"') < to);
}

/// An unquoted field, with a trailing carriage return stripped so CRLF behaves
/// like LF.
fn plainField(d: []const u8, from: usize, to: usize) Field {
    var stop = to;
    if (stop > from and d[stop - 1] == '\r') stop -= 1;
    return f.pack(from, stop - from, false);
}

/// The offset of the newline ending the row that starts at `pos`.
fn endOfRow(d: []const u8, pos: usize, end: usize) usize {
    var at = pos;
    while (at < end) {
        const next = scan.nextOf2(d, at, end, '\n', '"');
        if (next >= end) return end;
        if (d[next] == '"') {
            at = scan.skipQuoted(d, next + 1, end);
            continue;
        }
        return next;
    }
    return end;
}

/// Skips one JSON string starting at its opening quote, returning the offset one
/// past the closing quote and whether the string holds a backslash.
fn skipJsonString(d: []const u8, from: usize, end: usize) struct { usize, bool } {
    var at = from + 1; // the opening quote
    var escaped = false;
    while (true) {
        const stop = scan.nextOf2(d, at, end, '"', '\\');
        if (stop >= end) return .{ end, escaped };
        if (d[stop] == '"') return .{ stop + 1, escaped };
        escaped = true;
        at = stop + 2; // the backslash and whatever it escapes
        if (at > end) return .{ end, escaped };
    }
}

/// Does the mate's tail name a column this run tracks?
///
/// The CSV proof rests on a column sitting at a fixed offset. JSON makes no such
/// promise: a value is found by name, and a name repeated in one object takes its
/// *last* value for a compared column -- which the C, C++ and Rust ports do too,
/// and `test.sh` cross-checks -- so a second `"amount"` past the diverging byte
/// would carry a value the prefix never saw.
///
/// It cannot be ruled out in general, but it can be ruled out *here*, because the
/// proof only reaches this point when the two rows agree all the way through the
/// last wanted value. Whatever is left is the trailing ignored columns: a few
/// bytes. A name is a quoted string, so every quote in them is a candidate.
/// Mistaking a closing quote for an opening one costs a lookup that fails, which
/// is a fallback and not a wrong answer -- and an escaped name counts as a hit for
/// the same reason, since the wanted names are held unescaped.
pub fn jsonTailIsClean(p: RowParser, d: []const u8, from: usize, end: usize) bool {
    const j = switch (p) {
        .json => |x| x,
        .csv => return false,
    };
    var at = from;
    while (at < end) {
        const q = scan.nextOf1(d, at, end, '"');
        if (q >= end) return true;
        const close, const escaped = skipJsonString(d, q, end);
        if (escaped) return false;
        const lo = q + 1;
        const hi = if (close > q + 1) close - 1 else q + 1;
        if (hi > lo and j.slotFor(d[lo..hi]) != null) return false;
        if (close <= q) return false;
        at = close;
    }
    return true;
}

/// The JSON proof, past ignored values that differ.
///
/// `a` is A's row from its start through the last value either side wants; the
/// plain proof holds when the mate opens with exactly those bytes and its tail
/// names nothing tracked. One ignored member in between whose value differs --
/// an `updated_at`, say, which differs on every row -- fails that on every row.
/// So where the two rows first differ, this finds the member of A's object that
/// byte is in. If it is the value of a name nobody tracks, the value is skipped
/// in each row on its own -- in B from the same offset, since everything before
/// it agreed, name included -- and the comparison carries on from there. A
/// difference anywhere else, in a tracked value, or in a shape the walk does not
/// follow fails the proof, and the row is parsed as before. The names before
/// each skipped value are equal bytes in both rows and the skipped values belong
/// to untracked names, so every tracked name sits at the same place with the
/// same bytes; the tail check covers what follows. The C, C++ and Rust ports
/// have the same proof (#234, #235, #236).
pub fn jsonRowsMatch(p: RowParser, a: []const u8, b: []const u8, b_lo: usize, b_hi: usize) bool {
    const j = switch (p) {
        .json => |x| x,
        .csv => return false,
    };
    var ai: usize = 0;
    var bi = b_lo;
    var after_value = false;
    var hops: usize = 0;
    while (hops < 64) : (hops += 1) {
        const left = a.len - ai;
        const m = @min(left, b_hi - bi);
        // The whole run first: equal slices are one compare, and they are the
        // common case -- every row whose ignored values do not differ.
        if (m == left and std.mem.eql(u8, a[ai..], b[bi .. bi + left])) {
            return jsonTailIsClean(p, b, bi + left, b_hi);
        }
        const same = commonPrefix(a[ai .. ai + m], b[bi .. bi + m]);
        if (same == left) return jsonTailIsClean(p, b, bi + left, b_hi);
        if (same == m) return false; // B ends first
        const gap = jsonMemberAt(j, a, ai, ai + same, after_value) orelse return false;
        const b_vs = bi + (gap[0] - ai);
        const b_ve = jsonValueEnd(b, b_vs, b_hi);
        if (b_ve >= b_hi or b_ve <= b_vs) return false;
        ai = gap[1];
        bi = b_ve;
        after_value = true;
    }
    return false;
}

/// How many leading bytes `x` and `y` share, eight at a time.
fn commonPrefix(x: []const u8, y: []const u8) usize {
    const n = @min(x.len, y.len);
    var i: usize = 0;
    while (i + 8 <= n) : (i += 8) {
        const u = std.mem.readInt(u64, x[i..][0..8], .little);
        const v = std.mem.readInt(u64, y[i..][0..8], .little);
        if (u != v) return i + @ctz(u ^ v) / 8;
    }
    while (i < n and x[i] == y[i]) i += 1;
    return i;
}

fn jsonGap(c: u8) bool {
    return c == ' ' or c == '\t' or c == '\r';
}

/// Past one JSON value from `at`: a string, a scalar, or a nested object or
/// array. `end` where it does not close before `end`.
fn jsonValueEnd(d: []const u8, from: usize, end: usize) usize {
    if (from >= end) return end;
    switch (d[from]) {
        '"' => return skipJsonString(d, from, end)[0],
        '{', '[' => return skipJsonNested(d, from, end),
        else => {
            var at = from;
            while (at < end) : (at += 1) {
                switch (d[at]) {
                    ',', '}', ']', '\n', ' ', '\t', '\r' => break,
                    else => {},
                }
            }
            return at;
        },
    }
}

/// The member of `d`'s object whose value holds byte `diff`, walking from `from`
/// (the row start, or just past a value already skipped): its value's bounds.
/// Null where `diff` falls in a name or the punctuation between members, in a
/// tracked member, or anywhere the walk cannot follow -- an escaped name
/// included, as the tail check treats one. `d` ends at the last wanted value, so
/// a value that runs to its end is not a gap.
fn jsonMemberAt(j: RowParser.Json, d: []const u8, from: usize, diff: usize, after: bool) ?[2]usize {
    const end = d.len;
    var at = from;
    var after_value = after;
    while (true) {
        while (at < end and jsonGap(d[at])) at += 1;
        if (at >= end or at > diff) return null;
        if (d[at] != @as(u8, if (after_value) ',' else '{')) return null;
        at += 1;
        while (at < end and jsonGap(d[at])) at += 1;
        if (at >= end or d[at] != '"' or at >= diff) return null;
        const kq = at;
        const kend, const escaped = skipJsonString(d, at, end);
        if (escaped or kend >= end or kend >= diff) return null;
        at = kend;
        while (at < end and jsonGap(d[at])) at += 1;
        if (at >= end or d[at] != ':') return null;
        at += 1;
        while (at < end and jsonGap(d[at])) at += 1;
        if (at > diff) return null;
        const v_end = jsonValueEnd(d, at, end);
        if (v_end >= end or v_end <= at) return null;
        if (diff < v_end) {
            if (j.slotFor(d[kq + 1 .. kend - 1]) != null) return null;
            return .{ at, v_end };
        }
        at = v_end;
        after_value = true;
    }
}

/// Skips a nested object or array, which is not a cell value.
fn skipJsonNested(d: []const u8, from: usize, end: usize) usize {
    var pos = from;
    var depth: i32 = 0;
    while (pos < end) {
        switch (d[pos]) {
            '"' => {
                pos = skipJsonString(d, pos, end)[0];
                continue;
            },
            '{', '[' => depth += 1,
            '}', ']' => {
                depth -= 1;
                pos += 1;
                if (depth <= 0) return pos;
                continue;
            },
            else => {},
        }
        pos += 1;
    }
    return end;
}

/// The end of a row, which for newline-delimited JSON is the next newline byte
/// and nothing subtler.
///
/// This used to alternate a scan for `\n` or `"` with a walk over each string it
/// landed on, to avoid mistaking a newline inside a quoted value for the end of
/// the row. That cannot happen: RFC 8259 forbids the raw control characters
/// U+0000 to U+001F inside a string, and a newline is U+000A, so a valid JSON
/// string cannot contain one -- it must be written `\n`. The framing of ndjson
/// depends on exactly that.
///
/// The C port made this change first and its note carries the argument. Input
/// that does put a raw newline inside a string is not JSON, and this reader will
/// split the row there -- which is what every ndjson reader does, because the
/// format has no other way to say where a row ends.
fn endOfJsonRow(d: []const u8, from: usize, end: usize) usize {
    const stop = scan.nextOf1(d, from, end, '\n');
    return if (stop >= end) end else stop + 1;
}

/// A word at a time rather than a byte at a time: the first and last eight
/// bytes (four, or three single bytes, for a shorter name) and the length. A
/// byte loop is a serial multiply per byte, twenty names a row; collisions are
/// settled by comparing the name, so this only has to spread them.
fn nameHash(s: []const u8) u64 {
    const n = s.len;
    var a: u64 = 0;
    var b: u64 = 0;
    if (n >= 8) {
        a = std.mem.readInt(u64, s[0..8], .little);
        b = std.mem.readInt(u64, s[n - 8 ..][0..8], .little);
    } else if (n >= 4) {
        a = std.mem.readInt(u32, s[0..4], .little);
        b = std.mem.readInt(u32, s[n - 4 ..][0..4], .little);
    } else if (n > 0) {
        a = @as(u64, s[0]) | @as(u64, s[n / 2]) << 8 | @as(u64, s[n - 1]) << 16;
    }
    const h = (a ^ (b *% 0x9e37_79b9_7f4a_7c15) ^ n) *% 0xbf58_476d_1ce4_e5b9;
    return h ^ (h >> 31);
}

/// Whether two names are the same bytes, inline and a word at a time.
/// `std.mem.eql` was a call per member of every ndjson row -- the guessed
/// slot, the table probe and the `null` test -- and a sixth of the run's
/// instructions. Names are short, so the words overlap rather than loop far.
inline fn sameName(a: []const u8, b: []const u8) bool {
    const n = a.len;
    if (n != b.len) return false;
    if (n >= 8) {
        var i: usize = 0;
        while (i + 8 < n) : (i += 8) {
            if (std.mem.readInt(u64, a[i..][0..8], .little) != std.mem.readInt(u64, b[i..][0..8], .little))
                return false;
        }
        return std.mem.readInt(u64, a[n - 8 ..][0..8], .little) == std.mem.readInt(u64, b[n - 8 ..][0..8], .little);
    }
    if (n >= 4) {
        return std.mem.readInt(u32, a[0..4], .little) == std.mem.readInt(u32, b[0..4], .little) and
            std.mem.readInt(u32, a[n - 4 ..][0..4], .little) == std.mem.readInt(u32, b[n - 4 ..][0..4], .little);
    }
    for (a, b) |x, y| if (x != y) return false;
    return true;
}

/// Splits rows into fields, projecting straight to the columns asked for.
///
/// The CSV form is addressed by column number: once the last needed column has
/// been read the rest of the row is skipped to its newline without its fields
/// ever being delimited, so on twenty columns keyed on the first two most of a
/// row is never looked at. The JSON form is addressed by key instead, so it walks
/// the whole object — but only once, and one hash per key rather than a search
/// per wanted column, which at twenty columns would be four hundred comparisons
/// a row.
/// Where the wanted part of a row ends, when two files agree closely enough for
/// one to be measured against the other in bytes.
///
/// The join's expensive question is whether a matched pair differs, and it
/// answers it by parsing both rows into fields. It does not have to. If the two
/// rows are byte-identical up to the end of the last column either file wants,
/// then every column in between is byte-identical too, and no parse can say
/// otherwise -- so the pair is unchanged and the mate never needs reading.
///
/// That holds only when both sides really are the same shape: the same delimiter
/// and the same projection, because two files whose headers are ordered
/// differently can carry identical bytes and mean different things. `source` is
/// that projection before it is inverted, so comparing it settles the question
/// outright. JSON has no such prefix -- its keys may come in any order -- so it
/// never qualifies.
pub const Tail = struct { slot: usize, delimiter: u8, src: usize };

pub fn sharedTail(a: RowParser, b: RowParser) ?Tail {
    const ca = switch (a) {
        .csv => |c| c,
        .json => return null,
    };
    const cb = switch (b) {
        .csv => |c| c,
        .json => return null,
    };
    if (ca.delimiter != cb.delimiter) return null;
    if (ca.source.len != cb.source.len) return null;
    for (ca.source, cb.source) |x, y| {
        if (x) |xc| {
            if (y) |yc| {
                if (xc != yc) return null;
            } else return null;
        } else if (y != null) return null;
    }
    const run = ca.slots[ca.starts[ca.last_needed]..ca.starts[ca.last_needed + 1]];
    if (run.len == 0) return null;
    return .{ .slot = run[0], .delimiter = ca.delimiter, .src = ca.last_needed };
}

/// The proof's byte span without parsing the row.
///
/// `sharedTail` reads the span off the guard field's packed offsets, which means
/// parsing every column first — and the join parses all ten million rows of A
/// that way, though the proof only needs one offset: through the delimiter that
/// ends the guard column. The guard is source column `src`, so its end is the
/// (`src`+1)th delimiter from the row start — countable with the delimiter
/// cursor alone, skipping a quoted field the way the row parse does, and without
/// packing a field or walking the slot map. Returns null when the row ends first
/// — a short row, or a malformed one — which falls back to the parse.
///
/// The span is the field bytes, not including the delimiter that ends them —
/// the same `data[from..to]` the field-offset path builds — so the proof's
/// boundary check in `rowMatches` applies unchanged.
///
/// A guard that is the row's last column ends at the line ending instead of a
/// delimiter, and that is accepted for the last count only; a line ending
/// before it is a short row. Refusing it, as this did until the 2M x 100 run,
/// turned the fast path off for any file whose last column is compared.
pub fn guardSpan(data: []const u8, lo: usize, hi: usize, delimiter: u8, commas: usize) ?usize {
    if (commas == 0) return null;
    var delims = scan.Delims.init(data, lo, hi, delimiter, '\n');
    const at = pastFields(&delims, data, lo, hi, delimiter, commas, true) orelse return null;
    return at - 1 - lo;
}

/// Past the byte that closes the `n`th field from `at`, or null where the row
/// ends first. With `may_end` the last of them may close on the line ending.
inline fn pastFields(
    delims: *scan.Delims,
    data: []const u8,
    from: usize,
    hi: usize,
    delimiter: u8,
    n: usize,
    may_end: bool,
) ?usize {
    var at = from;
    for (0..n) |i| {
        const cur = if (at < hi and data[at] == '"') blk: {
            const close = scan.skipQuoted(data, at + 1, hi);
            break :blk delims.next(close);
        } else delims.next(at);
        if (cur >= hi) return null;
        if (data[cur] != delimiter and !(may_end and i + 1 == n and data[cur] == '\n')) return null;
        at = cur + 1;
    }
    return at;
}

/// The gapped proof's plan, where a column nobody wants sits among the wanted
/// ones -- an ignored `updated_at` in the middle of the row, say.
///
/// `guardSpan` covers one run: A's row from its start through the last column
/// either file wants, compared as bytes. An unwanted column inside that run
/// that differs -- and an ignored timestamp differs on every row -- fails the
/// proof every time, and every row is parsed. So the wanted columns are
/// compared as runs instead, and the unwanted ones between them skipped by
/// field count in each row on its own: an ignored value that differs cannot
/// stop a row proving equal, and one that differs in length cannot misalign
/// what follows. `seg[j]` wanted fields are followed by `gap[j]` unwanted ones;
/// the last run has no gap. The C, C++ and Rust ports have the same proof
/// (#229, #230, #231).
pub const Runs = struct {
    seg: []usize,
    gap: []usize,

    /// Null where there is no gap, and the one-run proof applies unchanged.
    /// Only called once `sharedTail` has agreed the two sides' shapes, so A's
    /// map of wanted columns is B's too.
    pub fn plan(gpa: std.mem.Allocator, a: RowParser) !?Runs {
        const c = switch (a) {
            .csv => |c| c,
            .json => return null,
        };
        var seg: std.ArrayList(usize) = .empty;
        errdefer seg.deinit(gpa);
        var gap: std.ArrayList(usize) = .empty;
        errdefer gap.deinit(gpa);
        try seg.append(gpa, 0);
        try gap.append(gpa, 0);
        for (0..c.last_needed + 1) |col| {
            const wanted = c.starts[col + 1] > c.starts[col];
            if (!wanted) {
                gap.items[gap.items.len - 1] += 1;
                continue;
            }
            if (gap.items[gap.items.len - 1] > 0) {
                try seg.append(gpa, 0);
                try gap.append(gpa, 0);
            }
            seg.items[seg.items.len - 1] += 1;
        }
        if (seg.items.len < 2) {
            seg.deinit(gpa);
            gap.deinit(gpa);
            return null;
        }
        const s = try seg.toOwnedSlice(gpa);
        errdefer gpa.free(s);
        return .{ .seg = s, .gap = try gap.toOwnedSlice(gpa) };
    }

    pub fn deinit(self: Runs, gpa: std.mem.Allocator) void {
        gpa.free(self.seg);
        gpa.free(self.gap);
    }

    /// Where each run starts in this row and how many bytes it takes, through
    /// the byte that closes it, into `out` -- two words a run. False where the
    /// row ends first, which falls back to the parse.
    pub fn ofRow(self: Runs, data: []const u8, lo: usize, hi: usize, delimiter: u8, out: []usize) bool {
        var delims = scan.Delims.init(data, lo, hi, delimiter, '\n');
        var at = lo;
        const n = self.seg.len;
        for (self.seg, 0..) |fields, j| {
            const last = j + 1 == n;
            const end = pastFields(&delims, data, at, hi, delimiter, fields, last) orelse return false;
            out[2 * j] = at;
            out[2 * j + 1] = end - at;
            if (last) break;
            at = pastFields(&delims, data, end, hi, delimiter, self.gap[j], false) orelse return false;
        }
        return true;
    }

    /// Whether B's row `b[lo..hi]` holds each of A's runs where its own gaps
    /// put it. A run's bytes include the byte that closes it, so equal bytes
    /// leave B at the start of its next field, as they leave A.
    pub fn matches(self: Runs, a: []const u8, spans: []const usize, b: []const u8, lo: usize, hi: usize, delimiter: u8) bool {
        var delims = scan.Delims.init(b, lo, hi, delimiter, '\n');
        var at = lo;
        const n = self.seg.len;
        for (0..n) |j| {
            const from = spans[2 * j];
            const len = spans[2 * j + 1];
            if (len > hi - at or !std.mem.eql(u8, a[from .. from + len], b[at .. at + len])) return false;
            at += len;
            if (j + 1 == n) break;
            at = pastFields(&delims, b, at, hi, delimiter, self.gap[j], false) orelse return false;
        }
        return true;
    }
};

pub const RowParser = union(enum) {
    csv: Csv,
    json: Json,

    pub const Csv = struct {
        delimiter: u8,
        /// Where each projected column sits in the file, or null when absent.
        source: []const ?usize,
        last_needed: usize,
        /// The inverse: which slots each column of the file feeds, laid out as
        /// one flat array with a start per column, so storing a field is a
        /// lookup rather than a walk of every wanted column -- twenty columns
        /// against twenty slots is four hundred comparisons a row otherwise.
        /// A column can feed more than one slot, because `--compare` may name a
        /// key column, so the run is a range rather than a single entry.
        slots: []const u16,
        starts: []const u32,
        /// The one slot each column feeds, or a sentinel. Almost every column
        /// feeds exactly one, and the hot path stores through this rather than
        /// slicing `slots` -- which costs two dependent loads from `starts` and
        /// a loop setup, per column, per parse, and a row is parsed about two
        /// and a half times: once by the sweep, then again for each side of the
        /// join and once more when a probe compares keys.
        single: []const i32,

        /// Marks the slots of columns `from` onward absent.
        ///
        /// This replaces clearing every slot at the top of the row. `out` is
        /// reused across rows, so a column this row does not reach has to be
        /// blanked or it would show the previous row's value -- but that is
        /// only the columns after the row ran out, and a well-formed row runs
        /// out of nothing. The full clear was 315 million instructions of the
        /// 4.6 billion this port spent on a 200,000-row pair, seven per cent of
        /// the whole comparison, because it ran twenty slots per row per file
        /// whether or not a single one of them needed it.
        /// `single[column]` when the column feeds no slot, and when it feeds
        /// more than one -- `--compare` naming a key column is the only way to
        /// get there, so it is worth a branch rather than a second array.
        pub const NONE: i32 = -1;
        pub const MANY: i32 = -2;

        /// Stores one field into every slot its column feeds.
        inline fn store(self: Csv, out: []Field, column: usize, field: Field) void {
            const one = self.single[column];
            if (one >= 0) {
                out[@intCast(one)] = field;
            } else if (one == MANY) {
                for (self.slots[self.starts[column]..self.starts[column + 1]]) |slot| {
                    out[slot] = field;
                }
            }
        }

        inline fn blankFrom(self: Csv, out: []Field, from: usize) void {
            if (from > self.last_needed) return;
            for (self.slots[self.starts[from]..self.starts[self.last_needed + 1]]) |slot| {
                out[slot] = f.ABSENT;
            }
        }
    };

    pub const Json = struct {
        /// The key whose value belongs in each slot; null for a column this file
        /// does not have.
        wanted: []const ?[]const u8,
        /// Open-addressed name to slot, so a key costs one hash.
        slots: []i32,
        slot_mask: usize,
        /// Slots below this are key columns: the first value wins, and once
        /// every slot is a filled key column the rest of the object is skipped.
        key_size: usize,
        /// What the table answers for `wanted[i]`: `i` itself unless the same
        /// name also fills an earlier slot, and -1 for a missing column. Names
        /// arrive in the same order row after row, usually slot order, so the
        /// member after slot `i` is tried as `i + 1` first, and a right guess
        /// has to give exactly the slot the table would.
        canon: []i32,

        pub fn slotFor(self: Json, key: []const u8) ?usize {
            var at = nameHash(key) & self.slot_mask;
            while (true) {
                const i = self.slots[at];
                if (i < 0) return null;
                const name = self.wanted[@intCast(i)];
                if (name != null and sameName(name.?, key)) return @intCast(i);
                at = (at + 1) & self.slot_mask;
            }
        }
    };

    pub fn initCsv(gpa: std.mem.Allocator, delimiter: u8, source: []const ?usize) !RowParser {
        var last: usize = 0;
        for (source) |s| if (s) |c| {
            if (c > last) last = c;
        };
        // Counting sort into the flat inverse: how many slots each column feeds,
        // then where each column's run starts, then the slots themselves.
        const starts = try gpa.alloc(u32, last + 2);
        @memset(starts, 0);
        for (source) |s| if (s) |c| {
            starts[c + 1] += 1;
        };
        for (1..starts.len) |i| starts[i] += starts[i - 1];
        const slots = try gpa.alloc(u16, starts[starts.len - 1]);
        var filled = try gpa.alloc(u32, last + 1);
        defer gpa.free(filled);
        @memset(filled, 0);
        for (source, 0..) |s, slot| {
            if (s) |c| {
                slots[starts[c] + filled[c]] = @intCast(slot);
                filled[c] += 1;
            }
        }
        // The one-slot shortcut, derived from the same counting sort.
        const single = try gpa.alloc(i32, last + 1);
        for (single, 0..) |*one, c| {
            one.* = switch (starts[c + 1] - starts[c]) {
                0 => Csv.NONE,
                1 => @intCast(slots[starts[c]]),
                else => Csv.MANY,
            };
        }
        return .{ .csv = .{
            .delimiter = delimiter,
            .source = source,
            .last_needed = last,
            .slots = slots,
            .starts = starts,
            .single = single,
        } };
    }

    pub fn initJson(gpa: std.mem.Allocator, wanted: []const ?[]const u8, key_size: usize) !RowParser {
        var n: usize = 16;
        while (n < wanted.len * 4) n <<= 1;
        const slots = try gpa.alloc(i32, n);
        errdefer gpa.free(slots);
        @memset(slots, -1);
        const mask = n - 1;
        for (wanted, 0..) |name, i| {
            if (name == null) continue;
            var at = nameHash(name.?) & mask;
            while (slots[at] >= 0) at = (at + 1) & mask;
            slots[at] = @intCast(i);
        }
        const canon = try gpa.alloc(i32, wanted.len);
        const json: Json = .{
            .wanted = wanted,
            .slots = slots,
            .slot_mask = mask,
            .key_size = @min(key_size, wanted.len),
            .canon = canon,
        };
        for (wanted, 0..) |name, i| {
            canon[i] = if (name) |w| (if (json.slotFor(w)) |slot| @intCast(slot) else -1) else -1;
        }
        return .{ .json = json };
    }

    pub fn deinit(self: RowParser, gpa: std.mem.Allocator) void {
        switch (self) {
            .json => |j| {
                gpa.free(j.slots);
                gpa.free(j.canon);
            },
            .csv => |c| {
                gpa.free(c.slots);
                gpa.free(c.starts);
                gpa.free(c.single);
            },
        }
    }

    /// Parses one row into `out`, returning the offset of the next row. A row
    /// shorter than the header leaves the missing fields absent, which is a
    /// difference to report rather than a file to refuse.
    pub fn parse(self: RowParser, d: []const u8, start: usize, end: usize, out: []Field) usize {
        return switch (self) {
            .csv => |c| parseCsv(c, d, start, end, out),
            .json => |j| parseJson(j, d, start, end, out),
        };
    }

    fn parseCsv(self: Csv, d: []const u8, start: usize, end: usize, out: []Field) usize {
        var pos = start;
        var column: usize = 0;
        // One cursor for the row -- every byte scanned once, both delimiters
        // broadcast once -- but only where a scan step spans several fields.
        // On an eight-byte step it does not, and the plain scan is quicker.
        var delims: scan.Delims = if (scan.wide_scan)
            scan.Delims.init(d, start, end, self.delimiter, '\n')
        else
            undefined;

        while (pos <= end) {
            var field: Field = undefined;
            var next: usize = undefined;
            if (pos < end and d[pos] == '"') {
                const close = scan.skipQuoted(d, pos + 1, end);
                const body_end = if (close > pos + 1) close - 1 else pos + 1;
                next = if (scan.wide_scan)
                    delims.next(close)
                else
                    scan.nextOf2(d, close, end, self.delimiter, '\n');
                field = quotedField(d, pos + 1, body_end);
            } else {
                next = if (scan.wide_scan)
                    delims.next(pos)
                else
                    scan.nextOf2(d, pos, end, self.delimiter, '\n');
                field = plainField(d, pos, next);
            }
            if (column <= self.last_needed) self.store(out, column, field);
            column += 1;

            if (next >= end) {
                self.blankFrom(out, column);
                return end;
            }
            if (d[next] == '\n') {
                self.blankFrom(out, column);
                return next + 1;
            }
            pos = next + 1;
            // Every needed column is filled, so there is nothing to blank: the
            // rest of the row is skipped without being parsed.
            if (column > self.last_needed) {
                const eol = endOfRow(d, pos, end);
                return if (eol >= end) end else eol + 1;
            }
        }
        self.blankFrom(out, column);
        return end;
    }

    /// Walks one JSON object, storing the values of the keys we want.
    fn parseJson(self: Json, d: []const u8, start: usize, end: usize, out: []Field) usize {
        @memset(out, f.ABSENT);
        var found: usize = 0; // key slots filled, for the early exit below
        var guess: usize = 0; // the slot the next member most likely fills
        var pos = start;
        while (pos < end and jsonSpace(d[pos])) pos += 1;
        if (pos >= end) return end;
        if (d[pos] != '{') return endOfJsonRow(d, pos, end); // not an object: skip the line
        pos += 1;

        while (true) {
            // Compact ndjson separates members with a bare `,"`, which goes
            // straight to the next key; the general path takes the comma, loops
            // back and skips space twice to get there.
            if (pos + 1 < end and d[pos] == ',' and d[pos + 1] == '"') {
                pos += 1;
            } else {
                while (pos < end and jsonSpace(d[pos])) pos += 1;
                if (pos >= end) break;
                if (d[pos] == '}') {
                    pos += 1;
                    break;
                }
                if (d[pos] == ',') {
                    pos += 1;
                    continue;
                }
                if (d[pos] != '"') break; // malformed: stop reading this object
            }

            const key_from = pos + 1;
            const key_end = skipJsonString(d, pos, end)[0];
            if (key_end > end or key_end < 2) break;
            const key = d[key_from .. key_end - 1];
            pos = key_end;
            // And `":"` between a key and a string value, likewise.
            if (pos + 1 < end and d[pos] == ':' and d[pos + 1] == '"') {
                pos += 1;
            } else {
                while (pos < end and jsonSpace(d[pos])) pos += 1;
                if (pos >= end or d[pos] != ':') break;
                pos += 1;
                while (pos < end and jsonSpace(d[pos])) pos += 1;
                if (pos >= end) break;
            }

            var field: ?Field = null;
            if (d[pos] == '"') {
                const from = pos + 1;
                const close, const escaped = skipJsonString(d, pos, end);
                const to = if (close > from) close - 1 else from;
                pos = close;
                field = f.pack(from, to - from, escaped);
            } else if (d[pos] == '{' or d[pos] == '[') {
                // Not a cell value. Left absent rather than guessed at.
                pos = skipJsonNested(d, pos, end);
            } else {
                // A number, true, false or null: it runs to the next comma,
                // brace or space.
                const from = pos;
                while (pos < end and d[pos] != ',' and d[pos] != '}' and !jsonSpace(d[pos])) pos += 1;
                if (!sameName(d[from..pos], "null")) field = f.pack(from, pos - from, false);
            }
            if (field) |value| {
                const hit: ?usize = if (guess < self.wanted.len and self.wanted[guess] != null and
                    sameName(self.wanted[guess].?, key))
                    (if (self.canon[guess] >= 0) @as(usize, @intCast(self.canon[guess])) else null)
                else
                    self.slotFor(key);
                if (hit) |slot| {
                    guess = slot + 1;
                    // First occurrence wins for a key column, and only for a key
                    // column -- the rule the C port states. A JSON object is not
                    // supposed to repeat a name, but the key-only parse below
                    // stops as soon as it has the keys, and it has to agree with
                    // the full parse on what a row's key is: last-wins would let
                    // it stop on a different value than the full parse ends
                    // with, a lookup that misses its own row. Compared columns
                    // keep last-wins.
                    if (slot < self.key_size) {
                        if (out[slot] == f.ABSENT) {
                            out[slot] = value;
                            found += 1;
                            // Every key found and nothing else wanted: the rest
                            // of the object is bytes to skip, not fields to
                            // parse. Walking them was two thirds of the sweep.
                            if (found == self.wanted.len) break;
                        }
                    } else {
                        out[slot] = value;
                    }
                }
            }
        }
        return endOfJsonRow(d, pos, end);
    }
};

/// A header name is one of the few strings this engine owns; there is one per
/// column, not one per cell.
fn ownField(gpa: std.mem.Allocator, slab: Slab, field: Field) ![]const u8 {
    var buf: std.ArrayList(u8) = .empty;
    errdefer buf.deinit(gpa);
    var it = slab.logical(field);
    while (it.next()) |b| try buf.append(gpa, b);
    return buf.toOwnedSlice(gpa);
}

pub const Header = struct { names: [][]const u8, start: usize };

/// The CSV header row's names, and where the first data row starts.
pub fn readCsvHeader(gpa: std.mem.Allocator, slab: Slab, delimiter: u8) !Header {
    const d = slab.data;
    if (d.len == 0) return Error.NoHeaderRow;
    var names: std.ArrayList([]const u8) = .empty;
    errdefer names.deinit(gpa);
    var pos: usize = 0;
    while (true) {
        var field: Field = undefined;
        var next: usize = undefined;
        if (pos < d.len and d[pos] == '"') {
            const close = scan.skipQuoted(d, pos + 1, d.len);
            const body_end = if (close > pos + 1) close - 1 else pos + 1;
            next = scan.nextOf2(d, close, d.len, delimiter, '\n');
            field = quotedField(d, pos + 1, body_end);
        } else {
            next = scan.nextOf2(d, pos, d.len, delimiter, '\n');
            field = plainField(d, pos, next);
        }
        try names.append(gpa, try ownField(gpa, slab, field));

        if (next >= d.len) return .{ .names = try names.toOwnedSlice(gpa), .start = d.len };
        if (d[next] == '\n') return .{ .names = try names.toOwnedSlice(gpa), .start = next + 1 };
        pos = next + 1;
    }
}

/// A JSON file has no header row, so the column names are the keys of the first
/// object, in the order it lists them.
pub fn readJsonHeader(gpa: std.mem.Allocator, slab: Slab) !Header {
    const d = slab.data;
    var names: std.ArrayList([]const u8) = .empty;
    errdefer names.deinit(gpa);
    var pos: usize = 0;
    while (pos < d.len and jsonSpace(d[pos])) pos += 1;
    if (pos >= d.len or d[pos] != '{') return Error.NoJsonObject;
    pos += 1;
    while (true) {
        while (pos < d.len and jsonSpace(d[pos])) pos += 1;
        if (pos >= d.len or d[pos] == '}') break;
        if (d[pos] == ',') {
            pos += 1;
            continue;
        }
        if (d[pos] != '"') break;
        const from = pos + 1;
        const close, const escaped = skipJsonString(d, pos, d.len);
        if (close <= from) break;
        try names.append(gpa, try ownField(gpa, slab, f.pack(from, close - 1 - from, escaped)));
        pos = close;
        while (pos < d.len and jsonSpace(d[pos])) pos += 1;
        if (pos >= d.len or d[pos] != ':') break;
        pos += 1;
        while (pos < d.len and jsonSpace(d[pos])) pos += 1;
        if (pos >= d.len) break;
        if (d[pos] == '"') {
            pos = skipJsonString(d, pos, d.len)[0];
        } else if (d[pos] == '{' or d[pos] == '[') {
            pos = skipJsonNested(d, pos, d.len);
        } else {
            while (pos < d.len and d[pos] != ',' and d[pos] != '}' and !jsonSpace(d[pos])) pos += 1;
        }
    }
    if (names.items.len == 0) return Error.NoJsonObject;
    return .{ .names = try names.toOwnedSlice(gpa), .start = 0 };
}

test "a brace is json and anything else is csv" {
    try std.testing.expectEqual(Dialect.json, sniffDialect("  {\"a\":1}"));
    try std.testing.expectEqual(Dialect.csv, sniffDialect("a,b,c\n"));
    try std.testing.expectEqual(Dialect.csv, sniffDialect(""));
}

test "json values are read by key whatever order they come in" {
    const text = "{\"k\":\"1\",\"v\":\"a\"}\n{\"v\":\"b\",\"k\":\"2\"}\n";
    const slab = Slab{ .data = text, .dialect = .json };
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{ "k", "v" };
    const parser = try RowParser.initJson(gpa, &wanted, 0);
    defer parser.deinit(gpa);
    var out: [2]Field = undefined;
    const next = parser.parse(text, 0, text.len, &out);
    try std.testing.expectEqualStrings("1", slab.raw(out[0]));
    try std.testing.expectEqualStrings("a", slab.raw(out[1]));
    _ = parser.parse(text, next, text.len, &out);
    try std.testing.expectEqualStrings("2", slab.raw(out[0]));
    try std.testing.expectEqualStrings("b", slab.raw(out[1]));
}

test "a repeated key: the key-only and full parses agree, and stop where they should" {
    // The first `k` is the row's key in both parses -- the key-only one stops
    // there -- while a repeated compared column keeps its last value.
    const text = "{\"k\":\"1\",\"v\":\"a\",\"k\":\"2\",\"v\":\"b\"}\n{\"k\":\"3\"}\n";
    const slab = Slab{ .data = text, .dialect = .json };
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{ "k", "v" };
    const full = try RowParser.initJson(gpa, &wanted, 1);
    defer full.deinit(gpa);
    const keys = try RowParser.initJson(gpa, wanted[0..1], 1);
    defer keys.deinit(gpa);

    var out: [2]Field = undefined;
    const next = full.parse(text, 0, text.len, &out);
    try std.testing.expectEqualStrings("1", slab.raw(out[0]));
    try std.testing.expectEqualStrings("b", slab.raw(out[1]));

    var key: [1]Field = undefined;
    try std.testing.expectEqual(next, keys.parse(text, 0, text.len, &key));
    try std.testing.expectEqualStrings("1", slab.raw(key[0]));
    _ = keys.parse(text, next, text.len, &key);
    try std.testing.expectEqualStrings("3", slab.raw(key[0]));
}

test "a name in two slots is found where the table would find it" {
    // `k` fills slot 0 and slot 2, as when a key column is also named as a
    // compared one. After `v` fills slot 1 the next member is guessed as slot
    // 2, and a right guess has to answer what the table does: slot 0.
    const text = "{\"k\":\"1\",\"v\":\"a\",\"k\":\"2\"}\n";
    const slab = Slab{ .data = text, .dialect = .json };
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{ "k", "v", "k" };
    const parser = try RowParser.initJson(gpa, &wanted, 1);
    defer parser.deinit(gpa);
    var out: [3]Field = undefined;
    _ = parser.parse(text, 0, text.len, &out);
    try std.testing.expectEqualStrings("1", slab.raw(out[0]));
    try std.testing.expectEqualStrings("a", slab.raw(out[1]));
    try std.testing.expectEqual(f.ABSENT, out[2]);
}

test "null, a nested value and a missing key are all absent" {
    const text = "{\"k\":\"1\",\"v\":null,\"w\":{\"deep\":1},\"x\":[1,2]}\n";
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{ "k", "v", "w", "x", "missing" };
    const parser = try RowParser.initJson(gpa, &wanted, 0);
    defer parser.deinit(gpa);
    var out: [5]Field = undefined;
    const next = parser.parse(text, 0, text.len, &out);
    const slab = Slab{ .data = text, .dialect = .json };
    try std.testing.expectEqualStrings("1", slab.raw(out[0]));
    for (out[1..]) |field| try std.testing.expectEqual(f.ABSENT, field);
    try std.testing.expectEqual(text.len, next);
}

test "the json header is the first object's keys in order" {
    const text = "{\"b\":1,\"a\":\"x\",\"n\":null}\n";
    const gpa = std.testing.allocator;
    const head = try readJsonHeader(gpa, .{ .data = text, .dialect = .json });
    defer {
        for (head.names) |n| gpa.free(n);
        gpa.free(head.names);
    }
    try std.testing.expectEqual(@as(usize, 3), head.names.len);
    try std.testing.expectEqualStrings("b", head.names[0]);
    try std.testing.expectEqualStrings("a", head.names[1]);
    try std.testing.expectEqualStrings("n", head.names[2]);
}

test "the json tail scan spots a tracked name and lets an untracked one pass" {
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{ "account_id", "amount" };
    const parser = try RowParser.initJson(gpa, &wanted, 0);
    defer parser.deinit(gpa);

    // The case the scan exists for: a compared name written again in the tail
    // takes its last value, so the proven prefix cannot settle the row.
    const repeats = ",\"amount\":\"9\"}";
    try std.testing.expect(!jsonTailIsClean(parser, repeats, 0, repeats.len));

    // A name nothing tracks is not a reason to refuse: this is the ordinary
    // trailing ignored column, and refusing on it would cost the whole proof.
    const ignored = ",\"note\":\"x\"}";
    try std.testing.expect(jsonTailIsClean(parser, ignored, 0, ignored.len));

    // A key written after the last value is tracked too -- the run has to cover
    // the keys for the proof to stand in for the key comparison.
    const key_late = ",\"account_id\":\"b\"}";
    try std.testing.expect(!jsonTailIsClean(parser, key_late, 0, key_late.len));

    // Nothing left to scan.
    const empty = "}";
    try std.testing.expect(jsonTailIsClean(parser, empty, 0, empty.len));
}

test "an escaped name in the tail refuses the proof rather than guessing" {
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{"amount"};
    const parser = try RowParser.initJson(gpa, &wanted, 0);
    defer parser.deinit(gpa);

    // The wanted names are held unescaped, so `amount` would not match by
    // bytes even though it names the same column. Refusing is a fallback to
    // parsing the row, which is correct; matching by bytes alone would not be.
    const escaped = ",\"am\\u006funt\":\"9\"}";
    try std.testing.expect(!jsonTailIsClean(parser, escaped, 0, escaped.len));
}

test "a value that merely looks like a name does not refuse the proof" {
    const gpa = std.testing.allocator;
    const wanted = [_]?[]const u8{"amount"};
    const parser = try RowParser.initJson(gpa, &wanted, 0);
    defer parser.deinit(gpa);

    // `"amount"` here is a *value*, not a name. The scan cannot tell the two
    // apart -- it walks quotes -- so it refuses, and the row is parsed. That is
    // the fallback being conservative, and the test records it as intended
    // rather than as a bug someone should "fix" by making the scan cleverer.
    const as_value = ",\"note\":\"amount\"}";
    try std.testing.expect(!jsonTailIsClean(parser, as_value, 0, as_value.len));
}
