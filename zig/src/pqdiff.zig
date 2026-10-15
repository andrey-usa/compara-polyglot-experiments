//! Comparing two Parquet files without turning them back into rows.
//!
//! The Zig twin of `cpp/src/pqdiff.cpp` and `rust/src/engine/pqdiff.rs`. The CSV
//! engine beside this one has one shape: map the file, find every row, reduce
//! each row to a handful of packed (offset, length) fields. That shape is right
//! for a text format, where a value's boundaries are only known by scanning for
//! them.
//!
//! Parquet is not that. A value's boundaries are written down, values of one
//! column are contiguous, and -- this is the part worth exploiting -- a
//! low-cardinality column is stored as small integers indexing a dictionary of
//! its distinct values. Reading such a file back into rows in order to compare
//! them row by row throws away the one thing the format gives you.
//!
//! So this path is columnar end to end. It reads the key columns, joins on them
//! once to produce a list of matched (a_row, b_row) pairs, and then walks the
//! compared columns one at a time, releasing each before reading the next. Where
//! both sides of a column are dictionary encoded, the two dictionaries are
//! mapped onto one shared id space once -- a few thousand string comparisons --
//! after which "did this cell change" is `i32 != i32`, and the resulting
//! mismatch mask is scanned eight bytes at a time with the same SWAR trick the
//! CSV scanner uses to find a delimiter.
//!
//! Every allocation still goes through the caller's allocator, so `--max-memory`
//! bounds this path exactly as it bounds the CSV one.

const std = @import("std");
const build_options = @import("build_options");
const csvdiff = @import("csvdiff.zig");
const scan = @import("scan.zig");
const parquet = @import("parquet.zig");

/// Bytes per step of the mismatch scan, from the same `-Dscan` build option the
/// CSV scanner uses: 8 is SWAR, 32 and 64 are a vector register.
const mask_width = build_options.scan_width;
const MaskVector = @Vector(mask_width, u8);
const MaskBits = std.meta.Int(.unsigned, mask_width);

const Options = csvdiff.Options;
const Counts = csvdiff.Counts;
const ColumnStat = csvdiff.ColumnStat;
const Result = csvdiff.Result;
const Phases = csvdiff.Phases;
const PREFETCH_AHEAD = csvdiff.PREFETCH_AHEAD;

pub const Error = error{
    ParquetRowCountMismatch,
    ParquetMixedWithText,
};

/// Four bytes is enough to know. Cheap, so the dispatch can ask before it maps.
pub fn isParquetFile(io: std.Io, path: []const u8) bool {
    const file = std.Io.Dir.cwd().openFile(io, path, .{}) catch return false;
    defer file.close(io);
    var magic: [4]u8 = undefined;
    var reader = file.reader(io, &.{});
    reader.interface.readSliceAll(&magic) catch return false;
    return std.mem.eql(u8, &magic, "PAR1");
}

// ---------------------------------------------------------------------------
// Values: the same rules as the CSV path, on a plain byte span
// ---------------------------------------------------------------------------

/// One cell. Parquet values carry no escaping -- a byte array is its own bytes
/// -- so where the CSV path reads a field through its escape decoder, here a
/// cell is already a span.
const Look = struct {
    bytes: []const u8 = &.{},
    is_null: bool = true,

    const none: Look = .{};
};

fn needsNormalising(o: Options) bool {
    return o.trim or o.ignore_case or o.empty_is_null or o.tolerance > 0;
}

fn isSpace(c: u8) bool {
    return c <= ' ';
}

/// Scratch for the normalising paths. Reused rather than allocated per cell, so
/// after the first value of a run nothing here allocates at all.
const Scratch = struct {
    a: std.ArrayList(u8) = .empty,
    b: std.ArrayList(u8) = .empty,

    fn deinit(self: *Scratch, gpa: std.mem.Allocator) void {
        self.a.deinit(gpa);
        self.b.deinit(gpa);
    }
};

/// The cell's normalised bytes, or null when it is absent. `--ignore-case` is
/// ASCII only and a non-ASCII byte is refused by name, exactly as in the CSV
/// path: folding case partially is worse than not folding it.
fn normalised(
    gpa: std.mem.Allocator,
    v: Look,
    o: Options,
    buf: *std.ArrayList(u8),
) !?[]const u8 {
    if (v.is_null or v.bytes.len == 0) return null;
    var s = v.bytes;
    if (o.trim) {
        while (s.len > 0 and isSpace(s[0])) s = s[1..];
        while (s.len > 0 and isSpace(s[s.len - 1])) s = s[0 .. s.len - 1];
    }
    // A value that trims to nothing is still a value, "", as it is in the CSV
    // path: only --empty-is-null makes it absent. Reading it as absent made
    // `"  "` against an empty cell no change here and a change on the same rows
    // as CSV.
    if (!o.ignore_case) {
        if (s.len == 0 and o.empty_is_null) return null;
        return s;
    }
    buf.clearRetainingCapacity();
    try buf.ensureTotalCapacity(gpa, s.len);
    for (s) |c| {
        if (c >= 0x80) return csvdiff.Error.NonAsciiCaseFold;
        buf.appendAssumeCapacity(std.ascii.toLower(c));
    }
    if (buf.items.len == 0 and o.empty_is_null) return null;
    return buf.items;
}

/// Decided without `--ignore-case`, as in the CSV path: absence never depends on
/// case, and a test that folded refused a non-ASCII value for being there.
fn isAbsent(gpa: std.mem.Allocator, v: Look, o: Options, buf: *std.ArrayList(u8)) !bool {
    if (v.is_null or v.bytes.len == 0) return true;
    if (!needsNormalising(o)) return false;
    return (try normalised(gpa, v, unfolded(o), buf)) == null;
}

/// The options without `--ignore-case`, for the questions case cannot change.
fn unfolded(o: Options) Options {
    var u = o;
    u.ignore_case = false;
    return u;
}

fn ascii(bytes: []const u8) bool {
    for (bytes) |c| if (c >= 0x80) return false;
    return true;
}

fn same(gpa: std.mem.Allocator, x: Look, y: Look, o: Options, s: *Scratch) !bool {
    const xa = try isAbsent(gpa, x, o, &s.a);
    const yb = try isAbsent(gpa, y, o, &s.b);
    if (xa or yb) return xa and yb;
    if (!needsNormalising(o)) return std.mem.eql(u8, x.bytes, y.bytes);
    // Identical bytes normalise identically; asked before the fold, so an
    // unchanged value outside ASCII is not a reason to refuse.
    if (std.mem.eql(u8, x.bytes, y.bytes)) return true;
    const nx = (try normalised(gpa, x, o, &s.a)) orelse "";
    const ny = (try normalised(gpa, y, o, &s.b)) orelse "";
    return std.mem.eql(u8, nx, ny);
}

/// Deliberately stricter than a plain float parse: "inf" and "nan" are ordinary
/// text in a table, and treating them as numbers would make two unequal strings
/// compare equal under a tolerance.
fn asNumber(text: []const u8) ?f64 {
    var s = text;
    while (s.len > 0 and isSpace(s[0])) s = s[1..];
    while (s.len > 0 and isSpace(s[s.len - 1])) s = s[0 .. s.len - 1];
    if (s.len == 0) return null;
    var body = s;
    if (body[0] == '+' or body[0] == '-') body = body[1..];
    if (body.len == 0) return null;
    if (!(std.ascii.isDigit(body[0]) or body[0] == '.')) return null;
    for (body) |c| {
        if (!(std.ascii.isDigit(c) or c == '.' or c == 'e' or c == 'E' or c == '+' or c == '-'))
            return null;
    }
    const v = std.fmt.parseFloat(f64, s) catch return null;
    return if (std.math.isFinite(v)) v else null;
}

/// SQL's IS DISTINCT FROM, with the tolerance applied where both sides parse.
fn cellDiffers(gpa: std.mem.Allocator, x: Look, y: Look, o: Options, s: *Scratch) !bool {
    const xa = try isAbsent(gpa, x, o, &s.a);
    const yb = try isAbsent(gpa, y, o, &s.b);
    if (xa and yb) return false;
    if (o.tolerance > 0 and !xa and !yb) {
        // A number reads the same in either case, so no fold here either.
        const tx = (try normalised(gpa, x, unfolded(o), &s.a)) orelse "";
        const nx = asNumber(tx);
        const ty = (try normalised(gpa, y, unfolded(o), &s.b)) orelse "";
        const ny = asNumber(ty);
        if (nx != null and ny != null) return @abs(nx.? - ny.?) > o.tolerance;
    }
    return !(try same(gpa, x, y, o, s));
}

const PRIME: u64 = 0x100_0000_01b3;
const SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, eight bytes at a time.
///
/// A hash here is internal in exactly the sense the CSV engine's is: nothing
/// outside this file sees one, so the only property it owes anyone is that the
/// two places that compute it agree. That is what lets it read a word at a time
/// -- see `csvdiff.hashBytes`, which is the same function for the same reason.
///
/// It matters more than the call sites suggest. Interning a dictionary costs
/// one call per *distinct* value and is nothing; a key column that is not
/// dictionary-encoded -- which a high-cardinality key column usually is not,
/// since a writer gives up on a dictionary that never repeats -- is hashed once
/// per row, on both sides, indexed and probed.
fn foldBytes(seed: u64, v: []const u8) u64 {
    var h = seed;
    var at: usize = 0;
    while (at + 8 <= v.len) : (at += 8) {
        h = (h ^ std.mem.readInt(u64, v[at..][0..8], .little)) *% PRIME;
        h ^= h >> 29; // the xor-shift is what spreads a whole word into the low bits
    }
    if (at < v.len) {
        h = (h ^ scan.tailWord(v, at)) *% PRIME;
        h ^= h >> 29;
    }
    return (h ^ v.len) *% PRIME;
}
fn foldAbsent(h: u64) u64 {
    return (h ^ 0x9e37_79b9_7f4a_7c15) *% PRIME;
}
fn foldId(h: u64, id: i32) u64 {
    return (h ^ @as(u64, @as(u32, @bitCast(id)))) *% PRIME;
}

// ---------------------------------------------------------------------------
// A column, as the comparison sees it
// ---------------------------------------------------------------------------

/// `parquet.Column` hands back offsets; this resolves them against whichever
/// buffer they belong to. Hot loops take `base` once rather than per cell.
const Col = struct {
    c: parquet.Column,
    map: []const u8,

    fn base(self: Col) []const u8 {
        return self.c.base(self.map);
    }
    fn sliceAt(self: Col, row: usize) parquet.Slice {
        if (self.c.dictionary) {
            const k = self.c.index[row];
            if (k < 0) return parquet.Slice.none;
            return self.c.dict[@intCast(k)];
        }
        return self.c.values[row];
    }
    fn at(self: Col, row: usize) Look {
        return look(self.base(), self.sliceAt(row));
    }
    fn dictAt(self: Col, k: usize) Look {
        return look(self.base(), self.c.dict[k]);
    }
    fn rows(self: Col) usize {
        return self.c.rows();
    }
    fn deinit(self: *Col, gpa: std.mem.Allocator) void {
        self.c.deinit(gpa);
    }
};

fn look(base: []const u8, s: parquet.Slice) Look {
    if (s.isNull()) return Look.none;
    return .{ .bytes = base[s.offset()..][0..s.length()], .is_null = false };
}

// ---------------------------------------------------------------------------
// One id space for two dictionaries
// ---------------------------------------------------------------------------

/// The trick the whole columnar path turns on. Two files' dictionaries are
/// interned into one dense id space, which costs one hash per *distinct* value
/// rather than one per row; after that, two cells are equal exactly when their
/// ids are, and a column diff is a comparison of two `i32` arrays.
const Ids = struct {
    map: std.StringHashMapUnmanaged(i32) = .empty,
    /// Copies made where a normalised value had to be built; the raw path
    /// borrows straight from the mapping and copies nothing.
    owned: std.ArrayList([]u8) = .empty,

    fn deinit(self: *Ids, gpa: std.mem.Allocator) void {
        for (self.owned.items) |o| gpa.free(o);
        self.owned.deinit(gpa);
        self.map.deinit(gpa);
    }

    fn of(self: *Ids, gpa: std.mem.Allocator, v: []const u8, copy: bool) !i32 {
        if (self.map.get(v)) |id| return id;
        const key = if (copy) blk: {
            const c = try gpa.dupe(u8, v);
            try self.owned.append(gpa, c);
            break :blk c;
        } else v;
        const id: i32 = @intCast(self.map.count());
        try self.map.put(gpa, key, id);
        return id;
    }
};

/// Whether coding this column's dictionary folds only what can be folded.
fn foldable(col: Col, o: Options) bool {
    if (!o.ignore_case) return true;
    for (0..col.c.dict.len) |k| if (!ascii(col.dictAt(k).bytes)) return false;
    return true;
}

/// Codes one column's dictionary into `ids`, with -1 for an absent value.
fn codeDict(
    gpa: std.mem.Allocator,
    col: Col,
    ids: *Ids,
    o: Options,
    s: *Scratch,
) ![]i32 {
    const out = try gpa.alloc(i32, col.c.dict.len);
    errdefer gpa.free(out);
    for (out, 0..) |*slot, k| {
        const cell = col.dictAt(k);
        if (try isAbsent(gpa, cell, o, &s.a)) {
            slot.* = -1;
            continue;
        }
        if (needsNormalising(o)) {
            const n = (try normalised(gpa, cell, o, &s.a)) orelse "";
            slot.* = try ids.of(gpa, n, true);
        } else {
            slot.* = try ids.of(gpa, cell.bytes, false);
        }
    }
    return out;
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// One file's key columns. A key column both sides store as a dictionary is
/// reduced to a shared id per row, and from there hashing and equality are
/// integer work; anything else stays bytes and is compared as bytes.
const KeySide = struct {
    col: []Col,
    /// Filled where the column is id-coded on both sides.
    id: [][]i32,
    rows: usize = 0,

    fn deinit(self: *KeySide, gpa: std.mem.Allocator) void {
        for (self.col) |*c| c.deinit(gpa);
        gpa.free(self.col);
        for (self.id) |v| gpa.free(v);
        gpa.free(self.id);
    }
};

fn rowHash(
    gpa: std.mem.Allocator,
    as_id: []const bool,
    s: KeySide,
    row: usize,
    o: Options,
    sc: *Scratch,
) !u64 {
    var h = SEED;
    for (as_id, 0..) |coded, j| {
        if (coded) {
            h = foldId(h, s.id[j][row]);
            continue;
        }
        const c = s.col[j].at(row);
        if (try isAbsent(gpa, c, o, &sc.a)) {
            h = foldAbsent(h);
        } else if (!needsNormalising(o)) {
            h = foldBytes(h, c.bytes);
        } else {
            h = foldBytes(h, (try normalised(gpa, c, o, &sc.a)) orelse "");
        }
    }
    return h;
}

fn rowEq(
    gpa: std.mem.Allocator,
    as_id: []const bool,
    x: KeySide,
    rx: usize,
    y: KeySide,
    ry: usize,
    o: Options,
    sc: *Scratch,
) !bool {
    for (as_id, 0..) |coded, j| {
        if (coded) {
            if (x.id[j][rx] != y.id[j][ry]) return false;
        } else if (!(try same(gpa, x.col[j].at(rx), y.col[j].at(ry), o, sc))) return false;
    }
    return true;
}

/// An open-addressed table over one file's distinct keys, first occurrence wins.
///
/// A slot is one word: the top twenty-four bits of the key's hash, and the
/// position in `firsts` plus one, with zero meaning empty. Carrying the hash
/// *inside* the slot is the point -- a probe that misses is settled by the word
/// it already loaded, where a table of bare positions would have to follow each
/// one into a separate array of hashes and take a second cache miss to reject
/// it. At ten million keys those second misses were the join.
const Index = struct {
    slots: []u32 = &.{},
    mask: u64 = 0,
    /// Low bits of a slot holding `firsts` positions plus one; the high bits
    /// hold the top of the key's hash as a tag. Chosen from the row count at
    /// build time, so the slot stays four bytes at any scale this port runs.
    pos_bits: u6 = 1,
    pos_mask: u32 = 1,
    firsts: std.ArrayList(i32) = .empty,
    counts: std.ArrayList(u32) = .empty,
    /// Per distinct key, for probing the other side.
    hashes: std.ArrayList(u64) = .empty,
    rows: i64 = 0,
    dup_keys: i64 = 0,
    dup_rows: i64 = 0,

    const EMPTY_SLOT: u32 = 0;

    fn slotFor(self: Index, h: u64, pos: usize) u32 {
        const p: u32 = @intCast(pos + 1);
        if (self.pos_bits == 32) return p;
        const pb: u5 = @intCast(self.pos_bits);
        // `h >> (32 + pos_bits)`: the top `32 - pos_bits` bits, now low.
        return (@as(u32, @truncate(h >> (32 + self.pos_bits))) << pb) | p;
    }
    fn tagIs(self: Index, slot: u32, h: u64) bool {
        if (self.pos_bits == 32) return true;
        const pb: u5 = @intCast(self.pos_bits);
        return (slot >> pb) == @as(u32, @truncate(h >> (32 + self.pos_bits)));
    }
    fn posOf(self: Index, slot: u32) usize {
        return @intCast((slot & self.pos_mask) - 1);
    }
    fn unique(self: Index) i64 {
        return @intCast(self.firsts.items.len);
    }
    fn deinit(self: *Index, gpa: std.mem.Allocator) void {
        gpa.free(self.slots);
        self.firsts.deinit(gpa);
        self.counts.deinit(gpa);
        self.hashes.deinit(gpa);
    }
};

/// Hashes are computed first and inserted second, because a row's hash depends
/// on nothing but that row while the table depends on the order rows arrive:
/// first occurrence wins, and duplicate counts follow from that.
fn buildIndex(
    gpa: std.mem.Allocator,
    as_id: []const bool,
    s: KeySide,
    o: Options,
    ways: usize,
) !Index {
    var ix = Index{ .rows = @intCast(s.rows) };
    errdefer ix.deinit(gpa);

    var sc = Scratch{};
    defer sc.deinit(gpa);

    const hs = try gpa.alloc(u64, s.rows);
    defer gpa.free(hs);
    {
        const chunks = @max(1, std.math.divCeil(usize, s.rows, HashRows.rows_per_chunk) catch 1);
        const errs = try gpa.alloc(?anyerror, chunks);
        defer gpa.free(errs);
        @memset(errs, null);
        var work = HashRows{
            .gpa = gpa,
            .as_id = as_id,
            .side = s,
            .opt = o,
            .hs = hs,
            .errs = errs,
        };
        try csvdiff.runOnThreads(&work, HashRows.run, ways);
        for (errs) |e| if (e) |err| return err;
    }

    // Sized to about a two-thirds load: linear probing is still short there, and
    // a smaller table is a smaller working set, which is what this phase is
    // actually limited by.
    var cap: usize = 1 << 12;
    while (cap * 2 < s.rows * 3 + 16) cap <<= 1;
    // Wide enough to hold every key index plus the +1 that keeps 0 for
    // empty, narrow enough to leave the rest of the 32-bit slot for the
    // hash tag. Past four billion keys no width fits, and that is refused
    // rather than truncated.
    if (s.rows + 2 > (@as(u64, 1) << 32)) return error.OutOfMemory;
    var pos_bits: u6 = 1;
    while (pos_bits < 32 and (@as(u64, 1) << pos_bits) < s.rows + 2) pos_bits += 1;
    ix.pos_bits = pos_bits;
    ix.pos_mask = if (pos_bits >= 32) std.math.maxInt(u32) else (@as(u32, 1) << @as(u5, @intCast(pos_bits))) - 1;
    ix.slots = try gpa.alloc(u32, cap);
    @memset(ix.slots, 0);
    ix.mask = cap - 1;
    try ix.firsts.ensureTotalCapacity(gpa, s.rows);
    try ix.counts.ensureTotalCapacity(gpa, s.rows);
    try ix.hashes.ensureTotalCapacity(gpa, s.rows);

    for (hs, 0..) |h, r| {
        // Every insert is a random access into a table of tens of megabytes, and
        // the next one wants a different line: without this the loop is a serial
        // chain of misses, each waiting on an address known long before the load
        // was issued. The hashes are all in hand, so the line can be asked for
        // early. See `PREFETCH_AHEAD`.
        if (r + PREFETCH_AHEAD < hs.len) {
            @prefetch(&ix.slots[hs[r + PREFETCH_AHEAD] & ix.mask], .{ .rw = .read, .locality = 3, .cache = .data });
        }
        var at: usize = @intCast(h & ix.mask);
        while (true) {
            const slot = ix.slots[at];
            if (slot == 0) {
                ix.slots[at] = ix.slotFor(h, ix.firsts.items.len);
                try ix.firsts.append(gpa, @intCast(r));
                try ix.counts.append(gpa, 1);
                try ix.hashes.append(gpa, h);
                break;
            }
            if (ix.tagIs(slot, h)) {
                const pos = ix.posOf(slot);
                if (try rowEq(gpa, as_id, s, @intCast(ix.firsts.items[pos]), s, r, o, &sc)) {
                    ix.counts.items[pos] += 1;
                    if (ix.counts.items[pos] == 2) {
                        ix.dup_keys += 1;
                        ix.dup_rows += 1; // the first occurrence counts once the key repeats
                    }
                    ix.dup_rows += 1;
                    break;
                }
            }
            at = (at + 1) & @as(usize, @intCast(ix.mask));
        }
    }
    return ix;
}

/// One key column of one file, decoded on its own thread.
///
/// Reading a column is the expensive half of the key phase -- pages decoded,
/// dictionaries built, levels expanded -- and the two sides' columns have
/// nothing in common, so there is no reason to do them in turn. What follows
/// them, interning the two dictionaries into one id space, does have to be
/// serial per column: it is one shared id space by construction, and it is a
/// few thousand string comparisons rather than ten million.
const KeyRead = struct {
    /// A ceiling so the jobs and their thread handles live on the stack. A key
    /// of more than sixteen columns reads in place instead.
    const max_jobs = 32;

    gpa: std.mem.Allocator,
    map: []const u8,
    at: usize,
    out: *Col,
    err: ?anyerror = null,

    fn run(self: *KeyRead) void {
        const c = parquet.readColumn(self.gpa, self.map, self.at) catch |e| {
            self.err = e;
            return;
        };
        self.out.* = .{ .c = c, .map = self.map };
    }
};

/// One side's index, built on its own thread. The two sides share nothing --
/// not the mapping, not the key columns, not the table -- so the only reason
/// this is a struct rather than a call is that a thread cannot return an error.
const IndexBuild = struct {
    gpa: std.mem.Allocator,
    as_id: []const bool,
    side: KeySide,
    opt: Options,
    /// How many threads this side's hash pass may use. The two sides build at
    /// once, so this is half the machine each.
    ways: usize = 1,
    index: Index = .{},
    err: ?anyerror = null,

    fn run(self: *IndexBuild) void {
        self.index = buildIndex(self.gpa, self.as_id, self.side, self.opt, self.ways) catch |e| {
            self.err = e;
            return;
        };
    }
};

/// The hash half of the index build, chunked.
///
/// A row's hash depends on nothing but that row, which is the whole reason the
/// build is written as hash-then-insert -- and this half was still walking ten
/// million rows on one thread while the insert it feeds is the part that has to
/// be serial. The Rust port has threaded this since it was written; this one had
/// not.
const HashRows = struct {
    const rows_per_chunk: usize = 1 << 16;

    gpa: std.mem.Allocator,
    as_id: []const bool,
    side: KeySide,
    opt: Options,
    hs: []u64,
    /// One slot per chunk, so a failing chunk writes only its own.
    errs: []?anyerror,
    next: std.atomic.Value(usize) = std.atomic.Value(usize).init(0),

    fn run(self: *HashRows) void {
        var sc = Scratch{};
        defer sc.deinit(self.gpa);
        while (true) {
            const c = self.next.fetchAdd(1, .monotonic);
            if (c >= self.errs.len) return;
            const lo = c * rows_per_chunk;
            const hi = @min(lo + rows_per_chunk, self.hs.len);
            for (lo..hi) |r| {
                self.hs[r] = rowHash(self.gpa, self.as_id, self.side, r, self.opt, &sc) catch |e| {
                    self.errs[c] = e;
                    return;
                };
            }
        }
    }
};

/// One direction of the join: every distinct key on `from`'s side looked up in
/// `into`'s table. The A direction keeps the pairs it finds; the B direction
/// only counts the keys that find nothing, which is why `keep_pairs` exists and
/// why the two can run at once without sharing an output.
/// Keys per chunk of the match sweep. Sized in rows rather than in threads, so a
/// chunk is small enough that no single one of them is the last thing four cores
/// are waiting on.
const SWEEP_CHUNK: usize = 1 << 16;

/// How many chunks `keys` divides into. One below the threshold, where finding
/// the boundaries would cost more than the sweep it splits.
fn sweepWays(keys: usize) usize {
    if (keys < (1 << 14)) return 1;
    return @max(1, std.math.divCeil(usize, keys, SWEEP_CHUNK) catch 1);
}

/// What one chunk of one direction found.
const SweepPart = struct {
    pair_a: std.ArrayList(i32) = .empty,
    pair_b: std.ArrayList(i32) = .empty,
    missing: i64 = 0,
    err: ?anyerror = null,

    fn deinit(self: *SweepPart, gpa: std.mem.Allocator) void {
        self.pair_a.deinit(gpa);
        self.pair_b.deinit(gpa);
    }
};

const MatchSweep = struct {
    gpa: std.mem.Allocator,
    as_id: []const bool,
    into: *const Index,
    into_keys: KeySide,
    from: *const Index,
    from_keys: KeySide,
    opt: Options,
    keep_pairs: bool,
    parts: []SweepPart = &.{},

    /// One chunk of this direction's keys, looked up in the other side's table.
    fn one(self: *MatchSweep, p: usize) void {
        self.chunk(p) catch |e| {
            self.parts[p].err = e;
        };
    }

    fn chunk(self: *MatchSweep, p: usize) !void {
        var sc = Scratch{};
        defer sc.deinit(self.gpa);
        const firsts = self.from.firsts.items;
        const hashes = self.from.hashes.items;
        const lo = firsts.len * p / self.parts.len;
        const hi = firsts.len * (p + 1) / self.parts.len;
        var out = &self.parts[p];
        if (self.keep_pairs) {
            try out.pair_a.ensureTotalCapacity(self.gpa, hi - lo);
            try out.pair_b.ensureTotalCapacity(self.gpa, hi - lo);
        }
        const mine = hashes[lo..hi];
        for (firsts[lo..hi], mine, 0..) |row, h, i| {
            // As in the index build: the probe's address is known well before
            // the load, so it is started early.
            if (i + PREFETCH_AHEAD < mine.len) {
                @prefetch(&self.into.slots[mine[i + PREFETCH_AHEAD] & self.into.mask], .{ .rw = .read, .locality = 3, .cache = .data });
            }
            const mate = try lookup(
                self.gpa,
                self.as_id,
                self.into.*,
                self.into_keys,
                self.from_keys,
                @intCast(row),
                h,
                self.opt,
                &sc,
            );
            if (mate < 0) {
                out.missing += 1;
                continue;
            }
            if (self.keep_pairs) {
                out.pair_a.appendAssumeCapacity(row);
                out.pair_b.appendAssumeCapacity(mate);
            }
        }
    }
};

/// Both directions of the join, chunked into one queue that every thread pulls
/// from.
///
/// A thread per direction looked right -- the two share nothing and write to
/// different places -- and it is what the CSV engine used to do too. Measuring
/// says a direction is the wrong unit: the two are not the same size, so one
/// finishes early and half the machine waits. Chunks do not care which direction
/// they came from.
const Sweeps = struct {
    pairs: *MatchSweep,
    counts: *MatchSweep,
    next: std.atomic.Value(usize) = std.atomic.Value(usize).init(0),

    fn run(self: *Sweeps) void {
        const first = self.pairs.parts.len;
        const total = first + self.counts.parts.len;
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= total) return;
            if (i < first) self.pairs.one(i) else self.counts.one(i - first);
        }
    }
};

/// Looks one side's row up in the other side's table.
fn lookup(
    gpa: std.mem.Allocator,
    as_id: []const bool,
    into: Index,
    there: KeySide,
    here: KeySide,
    row: usize,
    h: u64,
    o: Options,
    sc: *Scratch,
) !i32 {
    var at: usize = @intCast(h & into.mask);
    while (true) {
        const slot = into.slots[at];
        if (slot == 0) return -1;
        if (into.tagIs(slot, h)) {
            const first = into.firsts.items[into.posOf(slot)];
            if (try rowEq(gpa, as_id, there, @intCast(first), here, row, o, sc)) return first;
        }
        at = (at + 1) & @as(usize, @intCast(into.mask));
    }
}

// ---------------------------------------------------------------------------
// The mismatch mask
// ---------------------------------------------------------------------------

const block = 4096;

/// Read eight bytes at a time -- the same SWAR idiom the CSV scanner uses to
/// find a delimiter, applied to finding a changed cell. About 0.7% of cells
/// differ per column, so seven bytes in eight are zero and skipping them
/// wholesale is most of the loop.
fn scanMask(neq: []const u8, base: usize, hits: *std.ArrayList(usize), gpa: std.mem.Allocator) !void {
    var i: usize = 0;
    // The same `-Dscan` switch the CSV scanner is built under, asked of the same
    // question in a different shape: there it is "which byte is a delimiter",
    // here "which cell changed".
    //
    // The arithmetic that makes it worth doing: about 0.7% of cells differ per
    // column, so an eight-byte word is all-zero 94.6% of the time, a 32-byte
    // vector 80%, a 64-byte one 64%. Every one of those is a correctly
    // predicted not-taken branch and eight, thirty-two or sixty-four cells
    // retired. The work on a *hit* is identical in all three -- one `@ctz` per
    // changed cell -- so the only thing a wider register buys is fewer trips
    // round the skip path, which is where nearly all the loop is. That is the
    // opposite trade from the CSV scanner, where a wider register also means
    // fewer loads, and it is why both are measured rather than assumed.
    if (mask_width > 8) {
        const zero: MaskVector = @splat(0);
        while (i + mask_width <= neq.len) : (i += mask_width) {
            const chunk: MaskVector = @bitCast(neq[i..][0..mask_width].*);
            const nonzero: @Vector(mask_width, bool) = chunk != zero;
            var bits: MaskBits = @bitCast(nonzero);
            while (bits != 0) {
                try hits.append(gpa, base + i + @ctz(bits));
                bits &= bits - 1;
            }
        }
    }
    while (i + 8 <= neq.len) : (i += 8) {
        var w = std.mem.readInt(u64, neq[i..][0..8], .little);
        while (w != 0) {
            const byte = @ctz(w) >> 3;
            try hits.append(gpa, base + i + byte);
            w &= ~(@as(u64, 0xFF) << @intCast(byte * 8));
        }
    }
    while (i < neq.len) : (i += 1) {
        if (neq[i] != 0) try hits.append(gpa, base + i);
    }
}

// ---------------------------------------------------------------------------
// The compared columns
// ---------------------------------------------------------------------------

/// Everything a worker reads and never writes. One of these is shared by all of
/// them; `columns` is the only thing written through it, and each worker touches
/// only the entries for the columns it claimed.
const Job = struct {
    a_map: []const u8,
    b_map: []const u8,
    a_names: []const []const u8,
    b_names: []const []const u8,
    compared: []const []const u8,
    pair_a: []const i32,
    pair_b: []const i32,
    a_rows: usize,
    b_rows: usize,
    opt: Options,
    columns: []ColumnStat,
};

/// One lane of the column pass, with its own scratch and its own bit of the
/// changed-pairs mask. Columns are claimed from a shared counter rather than
/// dealt out in advance, because they are not the same size: a dictionary
/// column is decoded in a fraction of the time a plain one takes.
const Worker = struct {
    /// A fixed ceiling so the thread handles can live on the stack.
    const max_lanes = 32;

    gpa: std.mem.Allocator,
    job: *const Job,
    next: *std.atomic.Value(usize),
    any: []u64,
    neq: []u8,
    xa: []i32,
    xb: []i32,
    hits: std.ArrayList(usize) = .empty,
    sc: Scratch = .{},
    err: ?anyerror = null,

    fn init(gpa: std.mem.Allocator, words: usize, job: *const Job, next: *std.atomic.Value(usize)) !Worker {
        const any = try gpa.alloc(u64, words);
        @memset(any, 0);
        return .{
            .gpa = gpa,
            .job = job,
            .next = next,
            .any = any,
            .neq = try gpa.alloc(u8, block),
            .xa = try gpa.alloc(i32, block),
            .xb = try gpa.alloc(i32, block),
        };
    }

    fn deinit(self: *Worker, gpa: std.mem.Allocator) void {
        gpa.free(self.any);
        gpa.free(self.neq);
        gpa.free(self.xa);
        gpa.free(self.xb);
        self.hits.deinit(gpa);
        self.sc.deinit(gpa);
    }

    fn run(self: *Worker) void {
        while (true) {
            const c = self.next.fetchAdd(1, .monotonic);
            if (c >= self.job.compared.len) return;
            self.one(c) catch |e| {
                if (self.err == null) self.err = e;
                return;
            };
        }
    }

    fn one(self: *Worker, c: usize) !void {
        const gpa = self.gpa;
        const j = self.job;
        const opt = j.opt;
        const name = j.compared[c];
        const npairs = j.pair_a.len;

        var a_col = Col{
            .c = try parquet.readColumn(gpa, j.a_map, indexOf(j.a_names, name)),
            .map = j.a_map,
        };
        defer a_col.deinit(gpa);
        var b_col = Col{
            .c = try parquet.readColumn(gpa, j.b_map, indexOf(j.b_names, name)),
            .map = j.b_map,
        };
        defer b_col.deinit(gpa);
        if (a_col.rows() != j.a_rows or b_col.rows() != j.b_rows) return Error.ParquetRowCountMismatch;

        self.hits.clearRetainingCapacity();
        // The fast path: both sides dictionary encoded, so the two dictionaries
        // go into one id space and the per-row work is two gathers and an
        // integer compare.
        //
        // Not under --ignore-case with a dictionary entry outside ASCII: coding
        // folds every entry, including ones only an unchanged or unmatched row
        // holds, and this port refuses to fold those. The row path folds only a
        // cell that differs byte for byte, which is where the refusal belongs.
        const coded = a_col.c.dictionary and b_col.c.dictionary and opt.tolerance == 0 and
            foldable(a_col, opt) and foldable(b_col, opt);
        if (coded) {
            var ids = Ids{};
            defer ids.deinit(gpa);
            const amap = try codeDict(gpa, a_col, &ids, opt, &self.sc);
            defer gpa.free(amap);
            const bmap = try codeDict(gpa, b_col, &ids, opt, &self.sc);
            defer gpa.free(bmap);
            const aix = a_col.c.index;
            const bix = b_col.c.index;
            var base: usize = 0;
            while (base < npairs) : (base += block) {
                const m = @min(block, npairs - base);
                for (0..m) |i| {
                    const k = aix[@intCast(j.pair_a[base + i])];
                    self.xa[i] = if (k < 0) -1 else amap[@intCast(k)];
                }
                for (0..m) |i| {
                    const k = bix[@intCast(j.pair_b[base + i])];
                    self.xb[i] = if (k < 0) -1 else bmap[@intCast(k)];
                }
                for (0..m) |i| self.neq[i] = @intFromBool(self.xa[i] != self.xb[i]);
                try scanMask(self.neq[0..m], base, &self.hits, gpa);
            }
        } else {
            // The two buffers are found once rather than per cell: which one a
            // slice belongs to is a property of the column, not of the row.
            const ab = a_col.base();
            const bb = b_col.base();
            var base: usize = 0;
            while (base < npairs) : (base += block) {
                const m = @min(block, npairs - base);
                for (0..m) |i| {
                    const x = look(ab, a_col.sliceAt(@intCast(j.pair_a[base + i])));
                    const y = look(bb, b_col.sliceAt(@intCast(j.pair_b[base + i])));
                    self.neq[i] = @intFromBool(try cellDiffers(gpa, x, y, opt, &self.sc));
                }
                try scanMask(self.neq[0..m], base, &self.hits, gpa);
            }
        }

        for (self.hits.items) |p| {
            const x = a_col.at(@intCast(j.pair_a[p]));
            const y = b_col.at(@intCast(j.pair_b[p]));
            j.columns[c].changed += 1;
            if (try isAbsent(gpa, y, opt, &self.sc.b)) j.columns[c].blanked += 1;
            if (try isAbsent(gpa, x, opt, &self.sc.a)) j.columns[c].filled += 1;
            self.any[p >> 6] |= @as(u64, 1) << @intCast(p & 63);
        }
    }
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn has(list: []const []const u8, name: []const u8) bool {
    for (list) |c| if (std.mem.eql(u8, c, name)) return true;
    return false;
}

fn indexOf(list: []const []const u8, name: []const u8) usize {
    for (list, 0..) |c, i| if (std.mem.eql(u8, c, name)) return i;
    unreachable;
}

pub fn compare(
    io: std.Io,
    gpa: std.mem.Allocator,
    a_path: []const u8,
    b_path: []const u8,
    opt: Options,
) !Result {
    var phases = Phases.start("");
    var a_slab = csvdiff.Slab.map(io, a_path) catch |e| return csvdiff.named(e, a_path);
    defer a_slab.close();
    var b_slab = csvdiff.Slab.map(io, b_path) catch |e| return csvdiff.named(e, b_path);
    defer b_slab.close();
    const a_map = a_slab.data;
    const b_map = b_slab.data;

    var a_meta = try parquet.readMeta(gpa, a_map);
    defer a_meta.deinit(gpa);
    var b_meta = try parquet.readMeta(gpa, b_map);
    defer b_meta.deinit(gpa);

    for (opt.key) |k| {
        if (!has(a_meta.names, k) or !has(b_meta.names, k)) return csvdiff.named(csvdiff.Error.KeyColumnMissing, k);
    }
    // The text path's rule, which this path had not carried: a name in neither
    // file is a misspelling, and a misspelled --ignore silently compares the
    // column it meant to drop and calls it changed on every row.
    for (opt.ignore) |c| {
        if (!has(a_meta.names, c) and !has(b_meta.names, c)) return csvdiff.named(csvdiff.Error.IgnoredColumnMissing, c);
    }

    var compared: std.ArrayList([]const u8) = .empty;
    defer compared.deinit(gpa);
    if (opt.compare.len > 0) {
        for (opt.compare) |c| {
            if (!has(a_meta.names, c) or !has(b_meta.names, c)) return csvdiff.named(csvdiff.Error.ComparedColumnMissing, c);
            if (has(opt.key, c) or has(opt.ignore, c)) continue; // as on the text path
            try compared.append(gpa, c);
        }
    } else {
        for (a_meta.names) |c| {
            if (has(b_meta.names, c) and !has(opt.key, c) and !has(opt.ignore, c)) {
                try compared.append(gpa, c);
            }
        }
    }
    const nc = compared.items.len;
    const key_size = opt.key.len;

    // --- keys -------------------------------------------------------------
    var a_keys = KeySide{
        .col = try gpa.alloc(Col, key_size),
        .id = try gpa.alloc([]i32, key_size),
    };
    var b_keys = KeySide{
        .col = try gpa.alloc(Col, key_size),
        .id = try gpa.alloc([]i32, key_size),
    };
    // `alloc` hands back uninitialized memory and the `defer` below deinits
    // every entry, so a read that fails partway -- an unsupported codec, a
    // truncated file -- would free whatever bytes happened to be there. `id`
    // was already being cleared for exactly this reason; `col` was not, and a
    // zstd pair reached `gpa.free` on a garbage pointer.
    for (a_keys.col) |*c| c.* = .{ .c = .{}, .map = &.{} };
    for (b_keys.col) |*c| c.* = .{ .c = .{}, .map = &.{} };
    for (a_keys.id) |*v| v.* = &.{};
    for (b_keys.id) |*v| v.* = &.{};
    defer a_keys.deinit(gpa);
    defer b_keys.deinit(gpa);

    // `--threads` bounds every pass on this path, as it does in the other three
    // ports: the key reads here, the index build and match sweep below, and the
    // column pass after them. The key reads and the column pass used to take
    // the machine whatever `--threads` said, so a one-thread run of this port
    // used a second and a half of CPU in two thirds of a second of wall, and
    // measured nothing about one thread.
    const machine = @max(1, if (opt.threads != 0) opt.threads else (std.Thread.getCpuCount() catch 1));

    // Every key column, both sides, at once. These reads are the phase that
    // decodes pages for ten million rows and they share nothing -- different
    // files, different columns, separate outputs -- but they used to run one
    // after another, which is where this path's cores-busy ratio went. There
    // are `2 * key_size` of them and usually four.
    {
        var jobs: [KeyRead.max_jobs]KeyRead = undefined;
        var n: usize = 0;
        for (opt.key, 0..) |k, j| {
            if (n + 2 > KeyRead.max_jobs) break;
            jobs[n] = .{ .gpa = gpa, .map = a_map, .at = indexOf(a_meta.names, k), .out = &a_keys.col[j] };
            jobs[n + 1] = .{ .gpa = gpa, .map = b_map, .at = indexOf(b_meta.names, k), .out = &b_keys.col[j] };
            n += 2;
        }
        // A key wider than the job table falls back to reading in place, which
        // is the old behaviour and still right.
        for (opt.key[n / 2 ..], n / 2..) |k, j| {
            a_keys.col[j] = .{ .c = try parquet.readColumn(gpa, a_map, indexOf(a_meta.names, k)), .map = a_map };
            b_keys.col[j] = .{ .c = try parquet.readColumn(gpa, b_map, indexOf(b_meta.names, k)), .map = b_map };
        }

        var threads: [KeyRead.max_jobs]?std.Thread = @splat(null);
        // Up to `machine` at once: jobs past that run on this thread after its
        // own, which is the same work in the same order as a narrower machine.
        const spawned = @min(n, machine);
        for (1..spawned) |i| threads[i] = std.Thread.spawn(.{}, KeyRead.run, .{&jobs[i]}) catch null;
        if (n > 0) jobs[0].run();
        for (spawned..n) |i| jobs[i].run();
        // A job whose spawn failed runs here instead; one past `spawned` has
        // already run above and must not run twice.
        for (1..spawned) |i| if (threads[i]) |t| t.join() else jobs[i].run();
        for (jobs[0..n]) |job| if (job.err) |e| return e;
    }
    phases.mark("key columns (par)");
    a_keys.rows = if (key_size > 0) a_keys.col[0].rows() else 0;
    b_keys.rows = if (key_size > 0) b_keys.col[0].rows() else 0;
    for (0..key_size) |j| {
        if (a_keys.col[j].rows() != a_keys.rows or b_keys.col[j].rows() != b_keys.rows) {
            return Error.ParquetRowCountMismatch;
        }
    }

    const as_id = try gpa.alloc(bool, key_size);
    defer gpa.free(as_id);
    @memset(as_id, false);
    {
        var sc = Scratch{};
        defer sc.deinit(gpa);
        for (0..key_size) |j| {
            if (!a_keys.col[j].c.dictionary or !b_keys.col[j].c.dictionary) continue;
            as_id[j] = true;
            var ids = Ids{};
            defer ids.deinit(gpa);
            const a_code = try codeDict(gpa, a_keys.col[j], &ids, opt, &sc);
            defer gpa.free(a_code);
            const b_code = try codeDict(gpa, b_keys.col[j], &ids, opt, &sc);
            defer gpa.free(b_code);
            a_keys.id[j] = try gpa.alloc(i32, a_keys.rows);
            b_keys.id[j] = try gpa.alloc(i32, b_keys.rows);
            for (a_keys.col[j].c.index, 0..) |k, i| a_keys.id[j][i] = if (k < 0) -1 else a_code[@intCast(k)];
            for (b_keys.col[j].c.index, 0..) |k, i| b_keys.id[j][i] = if (k < 0) -1 else b_code[@intCast(k)];
        }
    }

    phases.mark("intern dicts (serial)");
    // --- the join ---------------------------------------------------------
    //
    // Two phases, and each is two independent halves, so each runs on two
    // threads: the indexes share nothing, and once both exist the two
    // directions of the join read them and write to different places. This is
    // the same structure the CSV engine has had since it was threaded at all --
    // one thread per file, one per direction -- and it is what this path was
    // missing: it was building both indexes and walking both directions on one
    // thread while the column pass below it used every core.
    // The same budget the sweep and the column pass take, halved: the two sides
    // build at the same time, so each gets half the machine for its hash pass.
    const per_side = @max(1, machine / 2);
    var build_a = IndexBuild{ .gpa = gpa, .as_id = as_id, .side = a_keys, .opt = opt, .ways = per_side };
    var build_b = IndexBuild{ .gpa = gpa, .as_id = as_id, .side = b_keys, .opt = opt, .ways = per_side };
    // A thread that cannot be spawned is not a reason to fail: the work runs
    // here instead, which is slower and still right.
    const build_thread = std.Thread.spawn(.{}, IndexBuild.run, .{&build_b}) catch null;
    build_a.run();
    if (build_thread) |t| t.join() else build_b.run();

    phases.mark("index build (2 ways)");
    var ai = build_a.index;
    defer ai.deinit(gpa);
    var bi = build_b.index;
    defer bi.deinit(gpa);
    if (build_a.err) |e| return e;
    if (build_b.err) |e| return e;

    // One direction, not two. A key matches from either side or from neither,
    // so the number of B's distinct keys with a mate in A *is* the number of
    // pairs the A pass finds, and `added` is B's distinct keys minus it. The
    // pass over B existed to count exactly that: a second randomly-probed walk
    // of A's table to learn a number already in hand. The Rust port found the
    // same (#121), and C and C++ run it only when a report samples added rows,
    // which this port never prints.
    const a_ways = sweepWays(ai.firsts.items.len);
    const b_ways: usize = 0;
    const parts = try gpa.alloc(SweepPart, a_ways + b_ways);
    defer {
        for (parts) |*part| part.deinit(gpa);
        gpa.free(parts);
    }
    for (parts) |*part| part.* = .{};

    var pairs = MatchSweep{
        .gpa = gpa,
        .as_id = as_id,
        .into = &bi,
        .into_keys = b_keys,
        .from = &ai,
        .from_keys = a_keys,
        .opt = opt,
        .keep_pairs = true,
        .parts = parts[0..a_ways],
    };
    // The other direction only has to count the keys that find no mate, so it
    // shares nothing with the first -- not even an output array.
    var unmatched_b = MatchSweep{
        .gpa = gpa,
        .as_id = as_id,
        .into = &ai,
        .into_keys = a_keys,
        .from = &bi,
        .from_keys = b_keys,
        .opt = opt,
        .keep_pairs = false,
        .parts = parts[a_ways..],
    };
    var sweeps = Sweeps{ .pairs = &pairs, .counts = &unmatched_b };
    // The same budget the column pass below uses: this path takes the machine
    // rather than `--threads`, which bounds the CSV engine's chunking instead.
    try csvdiff.runOnThreads(&sweeps, Sweeps.run, machine);
    for (parts) |part| {
        if (part.err) |e| return e;
    }

    phases.mark("match sweep (par)");
    // Merged in chunk order, so the pairing does not depend on which thread took
    // which chunk.
    var pair_a: std.ArrayList(i32) = .empty;
    defer pair_a.deinit(gpa);
    var pair_b: std.ArrayList(i32) = .empty;
    defer pair_b.deinit(gpa);
    var removed_total: i64 = 0;
    var added_total: i64 = 0;
    {
        var kept: usize = 0;
        for (parts[0..a_ways]) |part| kept += part.pair_a.items.len;
        try pair_a.ensureTotalCapacity(gpa, kept);
        try pair_b.ensureTotalCapacity(gpa, kept);
        for (parts[0..a_ways]) |*part| {
            pair_a.appendSliceAssumeCapacity(part.pair_a.items);
            pair_b.appendSliceAssumeCapacity(part.pair_b.items);
            removed_total += part.missing;
            // Released as it is copied rather than at the end of the function:
            // holding both the chunks and the merged lists is eighty megabytes
            // of the same pairs, live across the column pass that follows.
            // Emptied as well as freed, because the deferred cleanup runs too.
            part.pair_a.deinit(gpa);
            part.pair_b.deinit(gpa);
            part.pair_a = .empty;
            part.pair_b = .empty;
        }
    }
    added_total = bi.unique() - @as(i64, @intCast(pair_a.items.len));

    const npairs = pair_a.items.len;
    const words = (npairs + 63) / 64;

    // --- the compared columns, one at a time ------------------------------
    const any = try gpa.alloc(u64, words);
    defer gpa.free(any);
    @memset(any, 0);

    // The names are owned too, so an error from here on -- a refusal from a
    // column worker, say -- frees each one it made; freeing the array alone
    // leaked them, one per compared column.
    const columns = try gpa.alloc(ColumnStat, nc);
    var named: usize = 0;
    errdefer {
        for (columns[0..named]) |c| gpa.free(c.name);
        gpa.free(columns);
    }
    for (columns, compared.items) |*col, name| {
        col.* = .{ .name = try gpa.dupe(u8, name) };
        named += 1;
    }

    {
        // Each worker owns whole columns, so nothing is shared but the pair
        // arrays and the two mappings, which are read-only from here on. How
        // many run at once is a memory choice as much as a parallelism one: a
        // column of ten million values costs a couple of hundred megabytes on
        // each side while it is being read, and is released before the next.
        const lanes = @max(1, @min(Worker.max_lanes, @min(machine, nc)));
        const job = Job{
            .a_map = a_map,
            .b_map = b_map,
            .a_names = a_meta.names,
            .b_names = b_meta.names,
            .compared = compared.items,
            .pair_a = pair_a.items,
            .pair_b = pair_b.items,
            .a_rows = a_keys.rows,
            .b_rows = b_keys.rows,
            .opt = opt,
            .columns = columns,
        };
        var next = std.atomic.Value(usize).init(0);

        const workers = try gpa.alloc(Worker, lanes);
        defer gpa.free(workers);
        var made: usize = 0;
        defer for (workers[0..made]) |*w| w.deinit(gpa);
        for (workers) |*w| {
            w.* = try Worker.init(gpa, words, &job, &next);
            made += 1;
        }

        // A thread that cannot be spawned is not a reason to fail: it is the
        // same work, done on this one.
        var threads: [Worker.max_lanes]?std.Thread = @splat(null);
        for (workers[1..], 1..) |w, i| {
            _ = w;
            threads[i] = std.Thread.spawn(.{}, Worker.run, .{&workers[i]}) catch null;
            if (threads[i] == null) workers[i].run();
        }
        workers[0].run();
        for (threads[1..lanes]) |t| if (t) |th| th.join();

        for (workers) |*w| {
            if (w.err) |e| return e;
            for (any, w.any) |*dst, src| dst.* |= src;
        }
    }

    var changed_total: i64 = 0;
    for (any) |w| changed_total += @popCount(w);

    const matched: i64 = @intCast(npairs);
    phases.mark("compared columns (par)");
    return Result{
        .counts = .{
            .a_rows = ai.rows,
            .b_rows = bi.rows,
            .a_keys = ai.unique(),
            .b_keys = bi.unique(),
            .matched = matched,
            .unchanged = matched - changed_total,
            .changed = changed_total,
            .added = added_total,
            .removed = removed_total,
            .a_dup_keys = ai.dup_keys,
            .a_dup_rows = ai.dup_rows,
            .b_dup_keys = bi.dup_keys,
            .b_dup_rows = bi.dup_rows,
        },
        .columns = columns,
    };
}

test "a value that trims to nothing is present under --trim, absent under --empty-is-null" {
    const gpa = std.testing.allocator;
    var buf: std.ArrayList(u8) = .empty;
    defer buf.deinit(gpa);
    const blank: Look = .{ .bytes = "  \t", .is_null = false };
    const padded: Look = .{ .bytes = " b ", .is_null = false };

    const trim: Options = .{ .key = &.{}, .trim = true };
    try std.testing.expectEqualStrings("", (try normalised(gpa, blank, trim, &buf)) orelse return error.TrimmedToNothingReadAsAbsent);
    try std.testing.expect(!try isAbsent(gpa, blank, trim, &buf));
    try std.testing.expectEqualStrings("b", (try normalised(gpa, padded, trim, &buf)) orelse return error.TrimmedValueReadAsAbsent);

    const folded: Options = .{ .key = &.{}, .trim = true, .ignore_case = true };
    try std.testing.expectEqualStrings("", (try normalised(gpa, blank, folded, &buf)) orelse return error.TrimmedToNothingReadAsAbsent);

    inline for (.{
        Options{ .key = &.{}, .trim = true, .empty_is_null = true },
        Options{ .key = &.{}, .trim = true, .ignore_case = true, .empty_is_null = true },
    }) |o| {
        try std.testing.expect((try normalised(gpa, blank, o, &buf)) == null);
        try std.testing.expect(try isAbsent(gpa, blank, o, &buf));
    }
    // Null and the empty string are absent whatever is asked.
    try std.testing.expect(try isAbsent(gpa, .{}, trim, &buf));
    try std.testing.expect(try isAbsent(gpa, .{ .bytes = "", .is_null = false }, trim, &buf));
}
