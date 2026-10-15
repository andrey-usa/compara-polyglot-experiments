//! A composite-key comparison, byte-level, with the memory it may use passed in
//! rather than assumed.
//!
//! The design is the one the Java, Rust and C++ ports share: the file is mapped,
//! a field is an offset and a length packed into one word, delimiters are found
//! eight bytes at a time, and nothing becomes a string unless it reaches the
//! report.
//!
//! What Zig adds is the reason this port exists. Every allocation goes through an
//! allocator the caller supplies, so `--max-memory` is not a target the engine
//! tries to respect — it is a `FixedBufferAllocator` that cannot hand out more
//! than it was given. A comparison that would exceed the budget fails with
//! `error.OutOfMemory` at the allocation that would have crossed it, rather than
//! growing until the kernel intervenes. "Bounded memory" stops being a
//! measurement and becomes a property the type system enforces. That still holds
//! for the two things added since: reading Parquet allocates its arena from the
//! same allocator, and every thread takes its chunk from it too.
//!
//! Three input formats — CSV, newline-delimited JSON and Parquet — all reduce to
//! the same field word, so the two sides of a comparison need not be in the same
//! format. See `text.zig` and `parquet.zig`.

const builtin = @import("builtin");
const std = @import("std");
const scan = @import("scan.zig");
const fld = @import("field.zig");
const slab_mod = @import("slab.zig");
const text = @import("text.zig");
const parquet = @import("pqread.zig");
/// The columnar reader's own error set. Its refusals used to reach the user as
/// "the comparison failed", because `message` only knew `pqread.zig`'s.
const columnar = @import("parquet.zig");
const codec = @import("codec.zig");
const encoding = @import("encoding.zig");

const Field = fld.Field;
const ABSENT = fld.ABSENT;
const TOO_LONG = fld.TOO_LONG;
// Re-exported: `pqdiff.zig` maps its two files the same way this engine does.
pub const Slab = slab_mod.Slab;
const Dialect = slab_mod.Dialect;

/// What the last refusal was about -- the column that is missing, the file that
/// would not open -- for `main` to print after the message, as the C++ and Rust
/// ports do. Zig errors carry no payload, so it rides beside the error: set on
/// the line that returns it. The checks run on the calling thread before any
/// worker starts, and the name points at the command line, which outlives the run.
pub threadlocal var subject: ?[]const u8 = null;

pub const Error = error{
    KeyColumnMissing,
    ComparedColumnMissing,
    IgnoredColumnMissing,
    NoHeaderRow,
    FieldTooLong,
    CannotReadFile,
    NonAsciiCaseFold,
};

/// Below this there is nothing to divide: finding the chunk boundaries would cost
/// more than the parsing it splits.
const CHUNKING_THRESHOLD: usize = 4 << 20;

/// How many keys make the join worth splitting.
const JOIN_THRESHOLD: usize = 1 << 14;

/// Phase timings, on stderr, when `CSVDIFF_PHASES` is set — the same switch and
/// the same output shape the other two ports use.
///
/// A comparison has costs that move independently: sweeping and hashing every
/// row, inserting them into the index, and joining. Knowing which one grew is
/// the difference between tuning and guessing, and the cores-busy column says
/// only *that* something is serial, never which thing.
/// Set once from `CSVDIFF_PHASES` before the comparison starts; read by every
/// thread and written by none of them. `main.zig` owns the environment, so it
/// does the reading.
pub var phases_on: bool = false;

/// Set from `CSVDIFF_FUSED_JOIN` by `main.zig`, like `phases_on`: takes the join
/// inside A's sweep at any size, which is how the tests reach it. See `fused` in
/// `compare`.
pub var fused_forced: bool = false;

pub const Phases = struct {
    on: bool,
    tag: []const u8,
    last: i128,

    /// `tag` prefixes every line, because the two files are read on two threads
    /// and their phases would otherwise interleave unattributed.
    ///
    /// The monotonic clock straight from the platform: std's timing now wants an
    /// `Io`, and threading one through the engine for a diagnostic that is off by
    /// default would be a worse trade than this switch.
    ///
    /// The switch is the point. This was one unconditional call to
    /// `std.os.linux.clock_gettime`, which *compiles* on macOS -- `std.os.linux`
    /// is a namespace, not a target check -- and then emits Linux syscall numbers
    /// on a kernel that does not use them. A build error would have been the
    /// kinder failure. `Phases.start` calls this whether or not `CSVDIFF_PHASES`
    /// is set, so every macOS run went through it.
    fn now() i128 {
        return switch (builtin.os.tag) {
            // Linux by way of the kernel, so this port keeps linking no libc.
            .linux => blk: {
                var ts: std.os.linux.timespec = undefined;
                _ = std.os.linux.clock_gettime(.MONOTONIC, &ts);
                break :blk @as(i128, ts.sec) * std.time.ns_per_s + ts.nsec;
            },
            // Darwin always links libSystem, so the C entry point is the one.
            .macos, .ios, .tvos, .watchos, .visionos => blk: {
                var ts: std.c.timespec = undefined;
                _ = std.c.clock_gettime(.MONOTONIC, &ts);
                break :blk @as(i128, ts.sec) * std.time.ns_per_s + ts.nsec;
            },
            .windows => blk: {
                var counter: std.os.windows.LARGE_INTEGER = undefined;
                var frequency: std.os.windows.LARGE_INTEGER = undefined;
                _ = std.os.windows.ntdll.RtlQueryPerformanceCounter(&counter);
                _ = std.os.windows.ntdll.RtlQueryPerformanceFrequency(&frequency);
                if (frequency == 0) break :blk 0;
                break :blk @divTrunc(@as(i128, counter) * std.time.ns_per_s, @as(i128, frequency));
            },
            // Deliberately a build error rather than a plausible-looking number:
            // a phase table is only worth reading if its clock is real.
            else => @compileError(
                "no monotonic clock for this target; add one to Phases.now()",
            ),
        };
    }

    pub fn start(tag: []const u8) Phases {
        return .{
            .on = phases_on,
            .tag = tag,
            .last = now(),
        };
    }

    pub fn mark(self: *Phases, what: []const u8) void {
        const at = now();
        if (self.on) {
            const seconds = @as(f64, @floatFromInt(at - self.last)) / 1e9;
            var buf: [64]u8 = undefined;
            const name = std.fmt.bufPrint(&buf, "{s}{s}", .{ self.tag, what }) catch what;
            std.debug.print("  {s: <26} {d: >7.3}s\n", .{ name, seconds });
        }
        self.last = at;
    }
};

/// How many rows ahead a table probe is started. Enough misses in flight to
/// cover the latency of one, and not so many that the lines are evicted before
/// the loop reaches them.
pub const PREFETCH_AHEAD: usize = 32;

/// Keys per join chunk. Sized in rows rather than in threads so that a chunk is
/// small enough that no single one of them is the last thing four cores are
/// waiting on: at ten million keys this is a couple of hundred chunks of a few
/// hundredths of a second each.
const JOIN_CHUNK: usize = 1 << 16;

/// How many chunks `keys` divides into. One below the threshold, where finding
/// the boundaries would cost more than the join it splits.
fn waysFor(keys: usize) usize {
    if (keys < JOIN_THRESHOLD) return 1;
    return @max(1, std.math.divCeil(usize, keys, JOIN_CHUNK) catch 1);
}

pub const Options = struct {
    key: []const []const u8,
    compare: []const []const u8 = &.{},
    ignore: []const []const u8 = &.{},
    trim: bool = false,
    ignore_case: bool = false,
    empty_is_null: bool = false,
    tolerance: f64 = 0,
    max_rows: usize = 50_000,
    delimiter: ?u8 = null,
    /// How many threads the whole comparison may use. Zero means as many as the
    /// machine has. Both files are read at once and each is split further, so
    /// this is the width of the run rather than of one file.
    threads: usize = 0,
};

pub const Counts = struct {
    a_rows: i64 = 0,
    b_rows: i64 = 0,
    a_keys: i64 = 0,
    b_keys: i64 = 0,
    matched: i64 = 0,
    unchanged: i64 = 0,
    changed: i64 = 0,
    added: i64 = 0,
    removed: i64 = 0,
    a_dup_keys: i64 = 0,
    a_dup_rows: i64 = 0,
    b_dup_keys: i64 = 0,
    b_dup_rows: i64 = 0,
};

pub const ColumnStat = struct {
    name: []const u8,
    changed: i64 = 0,
    blanked: i64 = 0,
    filled: i64 = 0,
};

/// A reader declining a file it does not handle, rather than a file being wrong.
///
/// The columnar path reads a narrow slice of Parquet on purpose -- uncompressed
/// or snappy, BYTE_ARRAY, v1 pages -- while `pqread.zig` beside it reads zstd,
/// gzip, lz4, other column types and v2 pages. A capability refusal is a reason
/// to try that one; a truncated file or a missing column is not. The Rust port
/// draws the line in the same five places, in `Error::is_unsupported`.
pub fn isUnsupported(err: anyerror) bool {
    return switch (err) {
        columnar.Error.ParquetCodecUnsupported,
        columnar.Error.ParquetTypeUnsupported,
        columnar.Error.ParquetEncodingUnsupported,
        columnar.Error.ParquetNestedColumn,
        columnar.Error.ParquetPageV2,
        => true,
        else => false,
    };
}

/// Returns `err` with `name` recorded as what it is about. Only the refusals a
/// name explains get one; any other error passes through untouched.
pub fn named(err: anyerror, name: []const u8) anyerror {
    switch (err) {
        Error.KeyColumnMissing, Error.ComparedColumnMissing, Error.IgnoredColumnMissing, Error.CannotReadFile => subject = name,
        else => {},
    }
    return err;
}

/// What went wrong, in words, for any error this engine or its readers can
/// return. A switch here rather than in the command line, because the readers
/// each have their own error set and the caller should not have to know them.
pub fn message(err: anyerror) []const u8 {
    return switch (err) {
        error.OutOfMemory => "out of memory",
        Error.KeyColumnMissing => "key column(s) missing from one of the files",
        Error.ComparedColumnMissing => "compared column missing from one of the files",
        Error.IgnoredColumnMissing => "ignore column(s) present in neither file",
        Error.NoHeaderRow => "file has no header row",
        Error.FieldTooLong => "a field is larger than this engine packs",
        Error.CannotReadFile => "cannot read one of the files",
        Error.NonAsciiCaseFold => "--ignore-case outside ASCII needs Unicode case folding, which " ++
            "this port does not carry; use another implementation for that data",
        text.Error.NoJsonObject => "the JSON file has no object to read the column names from",
        parquet.Error.NotParquet => "that file does not begin and end with a Parquet magic number",
        parquet.Error.BadFooter => "the Parquet footer is longer than the file",
        parquet.Error.TruncatedParquetFile => "that file begins with a Parquet magic number but " ++
            "does not end with one: it is truncated",
        parquet.Error.NestedSchema => "this Parquet file nests a group; a comparison needs a flat " ++
            "table of cells",
        parquet.Error.RepeatedColumn => "this Parquet file repeats a column; a comparison needs " ++
            "one value per row",
        parquet.Error.NoColumns => "this Parquet file has no columns",
        parquet.Error.RowCountMismatch => "this Parquet file's row groups do not hold the number " ++
            "of rows its footer claims",
        parquet.Error.UnsupportedEncoding => "this Parquet file uses an encoding this reader does " ++
            "not decode; rewrite it with PLAIN or dictionary encoding",
        parquet.Error.UnsupportedType => "this Parquet file has a column of a type this reader " ++
            "does not know",
        parquet.Error.ExternalColumn => "this Parquet file keeps a column in another file, which " ++
            "this reader does not follow",
        parquet.Error.TruncatedPage => "a Parquet page ends mid-value",
        parquet.Error.DictionaryMissing => "a Parquet page is dictionary-encoded but its column " ++
            "chunk has no dictionary",
        parquet.Error.DictionaryIndexOutOfRange => "a Parquet page indexes past the end of its " ++
            "dictionary",
        codec.Error.UnsupportedCodec => "this Parquet file is compressed with a codec this reader " ++
            "does not decode",
        codec.Error.CorruptPage => "a compressed Parquet page will not decode",
        codec.Error.PageSizeMismatch => "a Parquet page decompressed to a different size from the " ++
            "one its header states",
        encoding.Error.TruncatedValues => "a Parquet page holds fewer values than its levels promise",
        encoding.Error.BadBlockSize => "a delta-encoded Parquet page has a block size its " ++
            "miniblocks do not divide",
        encoding.Error.VarintTooLong => "a Parquet varint is longer than 64 bits",
        thrift_error.TruncatedMetadata => "the Parquet metadata ends mid-value",
        thrift_error.UnknownThriftType => "the Parquet metadata holds a Thrift type this reader " ++
            "does not know",
        // The columnar reader (`parquet.zig`, behind `pqdiff.zig`). Every one of
        // these reached the user as "the comparison failed" until now -- a zstd
        // Parquet pair said nothing at all about codecs.
        columnar.Error.ParquetTruncated => "that Parquet file is truncated",
        columnar.Error.ParquetMalformed => "that Parquet file is malformed",
        columnar.Error.ParquetNestedColumn => "nested Parquet columns are not read by the columnar " ++
            "path; a comparison needs a flat table of cells",
        columnar.Error.ParquetNoSuchColumn => "a named column is not in that Parquet file",
        columnar.Error.ParquetTypeUnsupported => "only BYTE_ARRAY Parquet columns are read by the " ++
            "columnar path",
        columnar.Error.ParquetCodecUnsupported => "only uncompressed and snappy Parquet are read by " ++
            "the columnar path",
        columnar.Error.ParquetEncodingUnsupported => "only PLAIN Parquet dictionaries are read by the " ++
            "columnar path",
        columnar.Error.ParquetPageV2 => "Parquet data page v2 is not read by the columnar path",
        columnar.Error.ParquetValueTooLong => "a Parquet value is larger than this reader packs",
        columnar.Error.ParquetMixedCompression => "that Parquet file compresses some chunks of a " ++
            "column and not others, which this reader does not follow",
        columnar.Error.ParquetSnappyFailed => "a snappy Parquet page did not decompress",
        else => "the comparison failed",
    };
}

const thrift_error = @import("thrift.zig").Error;

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

fn needsNormalising(o: Options) bool {
    return o.trim or o.ignore_case or o.empty_is_null or o.tolerance > 0;
}

fn isSpace(c: u8) bool {
    return c <= ' ';
}

/// The field's normalised bytes written into `buf`, or null when it is absent.
/// Only the normalising paths call this; the default path never builds anything
/// for a cell at all.
fn normalised(slab: Slab, f: Field, o: Options, buf: []u8) Error!?[]const u8 {
    if (!fld.isReal(f)) return null;
    var n: usize = 0;
    var it = slab.logical(f);
    while (it.next()) |b| {
        if (n >= buf.len) return Error.FieldTooLong;
        buf[n] = b;
        n += 1;
    }
    var s = buf[0..n];
    if (s.len == 0) return null;
    if (o.trim) {
        var start: usize = 0;
        var stop: usize = s.len;
        while (start < stop and isSpace(s[start])) start += 1;
        while (stop > start and isSpace(s[stop - 1])) stop -= 1;
        s = s[start..stop];
    }
    if (o.ignore_case) {
        // Folding case outside ASCII needs a Unicode table this port does not
        // carry, and folding it partially is worse than not folding it at all:
        // CAFE and cafe with an acute would compare equal in the ports that do
        // fold and unequal here, with nothing in the output to say why. Only a
        // value whose fold decides something gets here: absence is decided
        // without folding and identical bytes before it (see `same`).
        for (s) |c| if (c >= 0x80) return Error.NonAsciiCaseFold;
        for (s, 0..) |c, i| s[i] = std.ascii.toLower(c);
    }
    if (o.empty_is_null and s.len == 0) return null;
    return s;
}

/// Scratch space for the two fields a comparison has in hand at once.
const Scratch = struct {
    a: [MAX_INLINE]u8 = undefined,
    b: [MAX_INLINE]u8 = undefined,
    const MAX_INLINE = 4096;
};

/// Absence never depends on case, so it is decided without `--ignore-case`: a
/// test that folded refused a non-ASCII value whose only question was whether it
/// was there.
fn isAbsent(slab: Slab, f: Field, o: Options, buf: []u8) Error!bool {
    if (!fld.isReal(f) or fld.lenOf(f) == 0) return true;
    if (!needsNormalising(o)) return false;
    return (try normalised(slab, f, unfolded(o), buf)) == null;
}

/// The options without `--ignore-case`, for the questions case cannot change.
fn unfolded(o: Options) Options {
    var u = o;
    u.ignore_case = false;
    return u;
}

fn same(a: Slab, x: Field, b: Slab, y: Field, o: Options, s: *Scratch) Error!bool {
    const xa = try isAbsent(a, x, o, &s.a);
    const yb = try isAbsent(b, y, o, &s.b);
    if (xa or yb) return xa and yb;
    if (!needsNormalising(o)) return slab_mod.sameBytes(a, x, b, y);
    // Identical bytes normalise identically, whatever they hold. Asked before
    // the fold, so an unchanged value outside ASCII is not a reason to refuse
    // the run under --ignore-case: the port that folds it gives the same answer.
    if (slab_mod.sameBytes(a, x, b, y)) return true;
    const nx = (try normalised(a, x, o, &s.a)) orelse "";
    const ny = (try normalised(b, y, o, &s.b)) orelse "";
    return std.mem.eql(u8, nx, ny);
}

/// Deliberately stricter than a plain float parse: "inf" and "nan" are ordinary
/// text in a CSV, and treating them as numbers would make two unequal strings
/// compare equal under a tolerance.
fn asNumber(value: []const u8) ?f64 {
    var s = value;
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

/// Whether a field is absent, when nothing has to be normalised first.
///
/// The general `isAbsent` returns `Error!bool` because a normalising build can
/// refuse a non-ASCII byte under `--ignore-case`. With no normalising there is
/// no error to have, and the difference matters: the join asks this four times
/// per compared cell -- twice in `cellDiffers` and twice more inside the `same`
/// it calls -- so ten million rows over seventeen columns propagate an error
/// union nearly seven hundred million times for a question that is two bit
/// tests.
inline fn plainAbsent(f: Field) bool {
    return !fld.isReal(f) or fld.lenOf(f) == 0;
}

/// `cellDiffers` for the common case, computing each side's absence once and
/// returning a plain `bool`. Both absent is not a difference; exactly one is.
inline fn plainDiffers(a: Slab, x: Field, xa: bool, b: Slab, y: Field, yb: bool) bool {
    if (xa or yb) return xa != yb;
    return !slab_mod.sameBytes(a, x, b, y);
}

fn cellDiffers(a: Slab, x: Field, b: Slab, y: Field, o: Options, s: *Scratch) Error!bool {
    const xa = try isAbsent(a, x, o, &s.a);
    const yb = try isAbsent(b, y, o, &s.b);
    if (xa and yb) return false;
    if (o.tolerance > 0 and !xa and !yb) {
        // A number reads the same in either case, so no fold here either.
        const tx = (try normalised(a, x, unfolded(o), &s.a)) orelse "";
        const nx = asNumber(tx);
        const ty = (try normalised(b, y, unfolded(o), &s.b)) orelse "";
        const ny = asNumber(ty);
        if (nx != null and ny != null) return @abs(nx.? - ny.?) > o.tolerance;
    }
    return !(try same(a, x, b, y, o, s));
}

/// FNV-1a over eight bytes at a time.
///
/// A hash is internal — nothing outside this engine can see one — so the only
/// property it owes anyone is that the index build and the join probe compute
/// the same number for the same bytes. That is what lets the common path read a
/// word at a time: a key of twenty-six bytes costs four multiplies instead of
/// twenty-six, and the key hash is computed four times over at every row (both
/// files, indexed then probed), which made byte-at-a-time FNV about a billion
/// dependent multiply-xor steps of a ten-million-row run.
fn hashBytes(bytes: []const u8, seed: u64) u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    var h = seed;
    var at: usize = 0;
    while (at + 8 <= bytes.len) : (at += 8) {
        h = (h ^ std.mem.readInt(u64, bytes[at..][0..8], .little)) *% PRIME;
        // The xor-shift is what spreads a whole word into the low bits, which is
        // where the table's slot comes from.
        h ^= h >> 29;
    }
    if (at < bytes.len) {
        h = (h ^ scan.tailWord(bytes, at)) *% PRIME;
        h ^= h >> 29;
    }
    return h;
}

/// FNV-1a over exactly the bytes equality compares, by the same route.
fn hashField(slab: Slab, f: Field, o: Options, seed: u64, buf: []u8) Error!u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    var h = seed;
    if (try isAbsent(slab, f, o, buf)) return (h ^ 0x9e37_79b9_7f4a_7c15) *% PRIME;
    var len: u64 = 0;
    if (needsNormalising(o)) {
        const v = (try normalised(slab, f, o, buf)) orelse "";
        for (v) |b| {
            h = (h ^ b) *% PRIME;
            len += 1;
        }
    } else {
        if (!fld.isEscaped(f)) {
            // Nothing to unescape, so the bytes are the value and eight of them
            // can be taken at a time. See `hashBytes`.
            //
            // The field word is asked directly rather than through `logical()`,
            // which answers the same question by building an iterator over the
            // slab -- the cost `sameBytes` used to pay, in the one function that
            // runs for every key column of every row of both files.
            const raw = slab.raw(f);
            h = hashBytes(raw, h);
            len = raw.len;
        } else {
            // Decoded, then folded eight bytes at a time like the branch above,
            // with the words assembled as the bytes come: a field may be
            // megabytes. Folding them one at a time gave the same value a
            // different hash, so a key a JSON writer escaped (`a\/b`, `\u0041`)
            // never found the same key written literally in the other file.
            var it = slab.logical(f);
            var word: u64 = 0;
            var have: u6 = 0;
            while (it.next()) |b| {
                word |= @as(u64, b) << (@as(u6, 8) *% have);
                have += 1;
                len += 1;
                if (have == 8) {
                    h = (h ^ word) *% PRIME;
                    h ^= h >> 29;
                    word = 0;
                    have = 0;
                }
            }
            if (have > 0) {
                h = (h ^ word) *% PRIME;
                h ^= h >> 29;
            }
        }
    }
    return (h ^ len) *% PRIME;
}

fn keyHash(slab: Slab, fields: []const Field, key_size: usize, o: Options, buf: []u8) Error!u64 {
    var h: u64 = 0xcbf2_9ce4_8422_2325;
    for (fields[0..key_size]) |f| h = try hashField(slab, f, o, h, buf);
    return h;
}

// ---------------------------------------------------------------------------
// One file, read into the representation the join works on
// ---------------------------------------------------------------------------

/// How a file's rows are addressed.
const Rows = union(enum) {
    /// Text: a row is an offset into the mapping, re-parsed on demand. The index
    /// stores where a row starts rather than its fields, because an offset is
    /// eight bytes where the fields would be twenty times that, and re-parsing is
    /// cheap because the parser stops at the last needed column.
    text: struct {
        parser: text.RowParser,
        /// The same parser configured with the key columns alone, for the sweep.
        ///
        /// The sweep reads every row of the file and wants two things from each:
        /// its hash, which comes from the key columns, and where the next row
        /// starts. It has never wanted the other seventeen, and packing them was
        /// most of what it cost -- a parser stops at the last column it was asked
        /// for, so asking for less makes the rest of the row a plain scan for the
        /// newline rather than a field-by-field walk.
        keys: text.RowParser,
        from: usize,
    },
    /// Parquet: there is no row to re-read, so the fields are materialised once,
    /// row-major, and a row is an index into them.
    columnar: struct { fields: []Field, rows: usize },
};

/// How much of a candidate row a lookup has to parse.
///
/// The two directions of the join want different things from the row they land
/// on. A's direction compares every column against its mate, so it needs the
/// whole row and keeps it. B's direction is asking one question -- is this key in
/// A? -- and discards the row it matched against; parsing the eighteen columns it
/// will not look at is the largest thing that side was doing.
const Want = enum { whole, keys };

/// A run of one row's bytes, and the delimiter that has to end it in the other
/// file for that run to be a whole number of columns.
/// What a caller offers as a byte proof that a candidate row needs no parsing.
///
/// Both forms carry A's row from its start through the end of the last value
/// either file wants -- **including the keys**, which is what lets the proof
/// stand in for the key comparison as well as the column comparison, and so run
/// before the mate is parsed at all. Where they differ is what makes the run a
/// whole number of values rather than a truncation of one.
const Span = union(enum) {
    /// A column sits at a fixed offset, so the run has to end on a field
    /// boundary: `delimiter` or a newline. Without it `12,3` would match a row
    /// opening `12,34`.
    csv: struct { bytes: []const u8, delimiter: u8 },
    /// A value is found by name and a name repeated in one object takes its last
    /// value, so the run ends one byte past the last value -- its closing quote --
    /// and the mate's remaining bytes must name nothing this run tracks.
    json: struct { bytes: []const u8 },
};

/// How many failures in a row before the byte proof is only tried every
/// `PROOF_BACKOFF` rows. A power of two: the loop masks against it.
const PROOF_BACKOFF: usize = 64;

/// What a probe found: the row, and whether the bytes settled it outright.
const Hit = struct { row: i32, same_bytes: bool };

const Side = struct {
    gpa: std.mem.Allocator,
    slab: Slab,
    rows: Rows,
    width: usize,
    /// Held so the JSON parser's key names outlive it.
    wanted_names: ?[]?[]const u8 = null,
    wanted_source: ?[]?usize = null,

    /// The fields of the row addressed by `at` — a byte offset for a text file,
    /// a row number for a columnar one.
    fn fieldsAt(self: Side, at: u64, out: []Field) void {
        switch (self.rows) {
            .text => |t| _ = t.parser.parse(self.slab.data, @intCast(at), self.slab.data.len, out),
            .columnar => |c| {
                const from = @as(usize, @intCast(at)) * self.width;
                @memcpy(out, c.fields[from..][0..self.width]);
            },
        }
    }

    /// The row parser, when rows are text and there is one.
    fn parser(self: Side) ?text.RowParser {
        return switch (self.rows) {
            .text => |t| t.parser,
            .columnar => null,
        };
    }

    /// The key columns of that row, and nothing else.
    ///
    /// The whole row is eighteen more fields than a key comparison reads, and a
    /// parser stops at the last column it was asked for -- so asking for the two
    /// key columns turns the rest of the row into one scan for the newline
    /// instead of eighteen field boundaries packed into words nobody looks at.
    /// This is the saving the sweep already takes, in the half of the engine
    /// that only ever wanted a key.
    ///
    /// Fills `out[0..key_size]` and leaves the rest of the buffer as it was:
    /// every caller reads the key columns alone.
    fn keysAt(self: Side, at: u64, key_size: usize, out: []Field) void {
        switch (self.rows) {
            .text => |t| _ = t.keys.parse(self.slab.data, @intCast(at), self.slab.data.len, out),
            .columnar => |c| {
                const from = @as(usize, @intCast(at)) * self.width;
                @memcpy(out[0..key_size], c.fields[from..][0..key_size]);
            },
        }
    }

    fn deinit(self: *Side) void {
        switch (self.rows) {
            .text => |t| {
                t.parser.deinit(self.gpa);
                t.keys.deinit(self.gpa);
            },
            .columnar => |c| self.gpa.free(c.fields),
        }
        if (self.wanted_names) |n| self.gpa.free(n);
        if (self.wanted_source) |s| self.gpa.free(s);
        self.slab.close();
    }
};

/// A file after its header has been read but before the comparison knows which
/// columns it wants. The two phases are separate because a Parquet file should
/// decode the columns being compared and no others, and that list is not known
/// until both headers have been resolved against each other.
const Input = union(enum) {
    text: struct { slab: Slab, delimiter: u8, from: usize, names: [][]const u8 },
    parquet: struct { reader: parquet.Reader, names: [][]const u8 },

    fn open(gpa: std.mem.Allocator, io: std.Io, path: []const u8, opt: Options) !Input {
        // The format is decided by what is in the file, not by its name: a file
        // called `.parquet` that is really a CSV is read as a CSV.
        var slab = try Slab.map(io, path);
        errdefer slab.close();
        if (parquet.looksLikeParquet(slab.data)) {
            slab.close();
            var reader = try parquet.Reader.open(gpa, io, path);
            errdefer reader.deinit();
            return .{ .parquet = .{ .reader = reader, .names = try reader.columnNames(gpa) } };
        }
        // A file that begins with the magic and does not end with it is a
        // truncated Parquet file. Reading it as a CSV would report a missing key
        // column, which sends the reader looking in the wrong place entirely.
        if (slab.data.len >= 4 and std.mem.eql(u8, slab.data[0..4], "PAR1"))
            return parquet.Error.TruncatedParquetFile;
        slab.dialect = text.sniffDialect(slab.data);
        if (slab.dialect == .json) {
            const head = try text.readJsonHeader(gpa, slab);
            return .{ .text = .{ .slab = slab, .delimiter = ',', .from = 0, .names = head.names } };
        }
        const delimiter = opt.delimiter orelse
            text.detectDelimiter(slab.data[0..scan.nextOf1(slab.data, 0, slab.data.len, '\n')]);
        const head = try text.readCsvHeader(gpa, slab, delimiter);
        return .{ .text = .{
            .slab = slab,
            .delimiter = delimiter,
            .from = head.start,
            .names = head.names,
        } };
    }

    fn names(self: Input) [][]const u8 {
        return switch (self) {
            .text => |t| t.names,
            .parquet => |p| p.names,
        };
    }

    fn freeNames(self: Input, gpa: std.mem.Allocator) void {
        for (self.names()) |n| gpa.free(n);
        gpa.free(self.names());
    }

    /// Reads the file into the join's representation, keeping only `wanted`.
    /// Takes ownership: on return the input is spent either way.
    fn project(
        self: Input,
        gpa: std.mem.Allocator,
        wanted: []const []const u8,
        key_size: usize,
        threads: usize,
    ) !Side {
        const width = wanted.len;
        switch (self) {
            .text => |t| {
                var side = Side{ .gpa = gpa, .slab = t.slab, .width = width, .rows = undefined };
                errdefer side.slab.close();
                if (t.slab.dialect == .json) {
                    const keys = try gpa.alloc(?[]const u8, width);
                    errdefer gpa.free(keys);
                    for (wanted, 0..) |n, i| keys[i] = if (has(t.names, n)) n else null;
                    side.wanted_names = keys;
                    side.rows = .{ .text = .{
                        .parser = try text.RowParser.initJson(gpa, keys, key_size),
                        .keys = try text.RowParser.initJson(gpa, keys[0..@min(key_size, width)], key_size),
                        .from = t.from,
                    } };
                } else {
                    const source = try gpa.alloc(?usize, width);
                    errdefer gpa.free(source);
                    for (wanted, 0..) |n, i| source[i] = indexOf(t.names, n);
                    side.wanted_source = source;
                    side.rows = .{ .text = .{
                        .parser = try text.RowParser.initCsv(gpa, t.delimiter, source),
                        .keys = try text.RowParser.initCsv(
                            gpa,
                            t.delimiter,
                            source[0..@min(key_size, width)],
                        ),
                        .from = t.from,
                    } };
                }
                return side;
            },
            .parquet => |p| {
                var reader = p.reader;
                defer reader.deinit(); // the mapping is finished with once decoded
                const keys = try gpa.alloc(?[]const u8, width);
                defer gpa.free(keys);
                for (wanted, 0..) |n, i| keys[i] = if (has(p.names, n)) n else null;
                const rows = reader.rows;
                const projected = try reader.project(gpa, keys, threads);
                return .{
                    .gpa = gpa,
                    .slab = Slab.owned(gpa, projected.arena, .raw),
                    .rows = .{ .columnar = .{ .fields = projected.fields, .rows = rows } },
                    .width = width,
                };
            },
        }
    }
};

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// A slot holds the top bits of its key's hash above `pos_bits` and the
/// position in `first_row` plus one in the low `pos_bits`, so zero means
/// empty.
///
/// Carrying the tag is what makes a failed probe cheap: the word already loaded
/// settles it. A table of bare positions has to follow each one into `first_row`
/// and then into `row_hash` — two dependent random loads, over arrays far too big
/// to cache at ten million keys — only to reject it.
///
/// The width is chosen from the row count rather than fixed, so the slot stays
/// four bytes: at ten million rows the index needs 24 bits and the tag gets
/// the other 8; at fifty million it needs 26 and the tag gets 6. A narrower tag
/// lets more probes through to the key comparison behind it, which is
/// unchanged — the answer cannot change, only how often the bytes are
/// re-examined.
const EMPTY_SLOT: u32 = 0;

/// One chunk's rows, in the order they appear in it.
const Chunk = struct {
    at: std.ArrayList(u64) = .empty,
    hash: std.ArrayList(u64) = .empty,
    /// Written only by the thread that owns this chunk, read only once every
    /// thread has been joined -- which is why no lock is needed for it.
    failure: ?anyerror = null,
    /// Where a text chunk's sweep left off: the first row start at or past its
    /// end. The next chunk's guessed start is checked against it.
    stopped: usize = 0,

    fn deinit(self: *Chunk, gpa: std.mem.Allocator) void {
        self.at.deinit(gpa);
        self.hash.deinit(gpa);
    }
};

/// Open addressing over one file's rows. Everything is a primitive array taken
/// from the caller's allocator, so an index that would not fit in the budget
/// fails here rather than growing until the kernel intervenes.
const RowIndex = struct {
    gpa: std.mem.Allocator,
    side: *const Side,
    key_size: usize,
    opt: Options,
    row_at: std.ArrayList(u64),
    row_hash: std.ArrayList(u64),
    table: []u32,
    /// Low bits of a slot holding `first_row` positions plus one; the high
    /// bits hold the top of the key's hash as a tag. Chosen from the row count
    /// in `build`, so the slot stays four bytes at any scale this port runs.
    pos_bits: u6,
    pos_mask: u32,
    mask: usize,
    first_row: std.ArrayList(i32),
    occurrences: std.ArrayList(u32),
    /// Rows that repeat a key already seen, in file order -- kept only when the
    /// build was given a sink, which joined them as though each were first.
    later: std.ArrayList(i32) = .empty,
    track_later: bool = false,
    probe: []Field,
    mine: []Field,
    rows: i64 = 0,
    dup_keys: i64 = 0,
    dup_rows: i64 = 0,

    /// Packs a slot: the top `32 - pos_bits` bits of the hash above the
    /// position plus one. Zero tag bits (past two billion rows) degrades to
    /// no tag rather than a wrong answer.
    fn slotFor(self: *const RowIndex, hash: u64, pos: usize) u32 {
        const p: u32 = @intCast(pos + 1);
        if (self.pos_bits == 32) return p;
        const pb: u5 = @intCast(self.pos_bits);
        // `hash >> (32 + pos_bits)`: the top `32 - pos_bits` bits, now low.
        // The u64 shift takes u6, so self.pos_bits stays; only the u32 shift needs u5.
        return (@as(u32, @truncate(hash >> (32 + self.pos_bits))) << pb) | p;
    }

    /// Whether the slot's tag matches the hash's top bits.
    fn tagIs(self: *const RowIndex, slot: u32, hash: u64) bool {
        if (self.pos_bits == 32) return true;
        const pb: u5 = @intCast(self.pos_bits);
        return (slot >> pb) == @as(u32, @truncate(hash >> (32 + self.pos_bits)));
    }

    /// The `first_row` position a non-empty slot holds.
    fn posOf(self: *const RowIndex, slot: u32) usize {
        return @intCast((slot & self.pos_mask) - 1);
    }

    /// Finds and hashes every row in parallel, then inserts them on one thread in
    /// file order.
    ///
    /// The split is safe because the two halves need different things: parsing a
    /// row depends on nothing but where it starts, while the table depends on the
    /// order rows arrive — first occurrence of a key wins, and the duplicate
    /// counts follow from that. Doing the second half in parallel would make the
    /// answer depend on thread scheduling.
    fn build(
        gpa: std.mem.Allocator,
        side: *const Side,
        key_size: usize,
        opt: Options,
        threads: usize,
        tag: []const u8,
        sink: ?*FusedSink,
    ) !RowIndex {
        var phases = Phases.start(tag);
        const chunks = try sweep(gpa, side, key_size, opt, threads, sink);
        phases.mark("sweep (parallel)");
        defer {
            for (chunks) |*c| c.deinit(gpa);
            gpa.free(chunks);
        }

        var total: usize = 0;
        for (chunks) |c| total += c.at.items.len;
        // Sized once for the rows about to be inserted, at about a two-thirds
        // load. Starting at four thousand and doubling meant twelve rehashes at
        // ten million rows, each one a full random-access pass over a table
        // already too big to cache — work that grows with the file and is
        // entirely avoidable, since the row count is known before the first
        // insert.
        //
        // Two thirds and not a half: sizing for a half load also removes the
        // rehash, but the table it leaves is twice as big, and at ten million
        // keys the misses that costs are worth more than the probes it saves.
        var cap: usize = 1 << 12;
        while (cap * 2 < total * 3 + 16) cap <<= 1;
        // Wide enough to hold every key index plus the +1 that keeps 0 for
        // empty, narrow enough to leave the rest of the 32-bit slot for the
        // hash tag: 24 position bits at ten million rows, 26 at fifty. Past
        // four billion keys no width fits, and that is refused rather than
        // truncated.
        if (total + 2 > (@as(u64, 1) << 32)) return error.OutOfMemory;
        var pos_bits: u6 = 1;
        while (pos_bits < 32 and (@as(u64, 1) << pos_bits) < total + 2) pos_bits += 1;
        const pos_mask: u32 = if (pos_bits >= 32) std.math.maxInt(u32) else (@as(u32, 1) << @as(u5, @intCast(pos_bits))) - 1;
        const table = try gpa.alloc(u32, cap);
        @memset(table, EMPTY_SLOT);
        var self = RowIndex{
            .gpa = gpa,
            .side = side,
            .key_size = key_size,
            .opt = opt,
            .row_at = .empty,
            .row_hash = .empty,
            .table = table,
            .pos_bits = pos_bits,
            .pos_mask = pos_mask,
            .mask = table.len - 1,
            .first_row = .empty,
            .occurrences = .empty,
            .probe = try gpa.alloc(Field, side.width),
            .mine = try gpa.alloc(Field, side.width),
            .track_later = sink != null,
        };
        errdefer self.deinit();

        // The four lists are *not* reserved to `total`, and used to be. Sizing
        // the table once is measured and stays; sizing these was assumed from
        // it, by an analogy that does not hold.
        //
        // A rehash is a full random-access pass over a table too big to cache,
        // which is why the one above is worth avoiding. Growing an `ArrayList`
        // is a linear copy, which is not the same thing at all -- and the
        // copies happen *while the chunks below are being freed*, so the list
        // grows into pages the loop has just given back instead of reserving
        // fresh ones beside them. Removing all four reservations measured 12%
        // less CPU and 22% less wall on a 6M pair, and 20% off peak `VmData`,
        // which is the number that decides how many rows this port can do at
        // all. Keeping the two row lists reserved and dropping only the other
        // two is worse than dropping all four by a further 17% of CPU.
        //
        // Reserving only `first_row` and `occurrences` was tried again after
        // the row lists came to be taken over from the chunks (#213, reverted):
        // 7% more peak `VmData` on a 5M pair, 402 MB to 430 MB on every run,
        // for CPU time the A/B harness could not tell apart from this.
        //
        // `total` is still what sizes the table, a few lines up.

        var s = Scratch{};
        // Each chunk is released as soon as it has been inserted. Holding all of
        // them to the end would keep two copies of every row's address and hash
        // alive at once, which is sixteen bytes a row of pure duplication.
        //
        // A chunk's two lists are already `row_at` and `row_hash` for its rows:
        // every row the sweep saw, in file order. So the first chunk's lists are
        // taken over rather than copied, and a later chunk's are appended a
        // slice at a time. With one chunk a file, which is what the text sweep
        // makes, this is no copy at all where it used to be two appends a row
        // into lists growing to the size of the file.
        for (chunks) |*chunk| {
            const first = self.row_at.items.len;
            if (first == 0) {
                self.row_at.deinit(gpa);
                self.row_hash.deinit(gpa);
                self.row_at = chunk.at;
                self.row_hash = chunk.hash;
            } else {
                try self.row_at.appendSlice(gpa, chunk.at.items);
                try self.row_hash.appendSlice(gpa, chunk.hash.items);
                chunk.deinit(gpa);
            }
            // Emptied rather than only released, because the caller frees the
            // chunks too and a list deinitialised twice frees a pointer it no
            // longer owns -- or, taken over, one this index owns now.
            chunk.* = .{};

            const hashes = self.row_hash.items[first..];
            for (self.row_at.items[first..], hashes, 0..) |at, hash, i| {
                if (i + PREFETCH_AHEAD < hashes.len) self.prefetch(hashes[i + PREFETCH_AHEAD]);
                try self.insert(@intCast(first + i), at, hash, &s);
            }
        }
        phases.mark("index insert (serial)");
        return self;
    }

    fn deinit(self: *RowIndex) void {
        self.row_at.deinit(self.gpa);
        self.row_hash.deinit(self.gpa);
        self.first_row.deinit(self.gpa);
        self.occurrences.deinit(self.gpa);
        self.later.deinit(self.gpa);
        self.gpa.free(self.table);
        self.gpa.free(self.probe);
        self.gpa.free(self.mine);
    }

    /// Indexes row `row`, which is already in `row_at` and `row_hash`.
    fn insert(self: *RowIndex, row: i32, at: u64, hash: u64, s: *Scratch) !void {
        self.rows += 1;

        var slot = self.slotOf(hash);
        var mine_parsed = false;
        while (true) {
            const word = self.table[slot];
            if (word == EMPTY_SLOT) {
                self.table[slot] = self.slotFor(hash, self.first_row.items.len);
                try self.first_row.append(self.gpa, row);
                try self.occurrences.append(self.gpa, 1);
                // Two thirds, which is what `build` sizes the table for.
                if (self.first_row.items.len * 3 > self.table.len * 2) try self.rehash();
                return;
            }
            // The tag rejects almost every collision without leaving this word.
            if (self.tagIs(word, hash)) {
                const key = self.posOf(word);
                const candidate = self.first_row.items[key];
                if (self.row_hash.items[@intCast(candidate)] == hash) {
                    // This row's fields are re-read rather than carried over from
                    // the sweep because the sweep produced ten million of them and
                    // this branch wants one.
                    if (!mine_parsed) {
                        self.side.keysAt(at, self.key_size, self.mine);
                        mine_parsed = true;
                    }
                    self.keysOf(candidate, self.probe);
                    var ok = true;
                    for (0..self.key_size) |i| {
                        if (!(try same(self.side.slab, self.probe[i], self.side.slab, self.mine[i], self.opt, s))) {
                            ok = false;
                            break;
                        }
                    }
                    if (ok) {
                        self.occurrences.items[key] += 1;
                        if (self.occurrences.items[key] == 2) {
                            self.dup_keys += 1;
                            self.dup_rows += 1; // the first occurrence counts once the key repeats
                        }
                        self.dup_rows += 1;
                        if (self.track_later) try self.later.append(self.gpa, row);
                        return;
                    }
                }
            }
            slot = (slot + 1) & self.mask;
        }
    }

    fn fieldsOf(self: RowIndex, row: i32, out: []Field) void {
        self.side.fieldsAt(self.row_at.items[@intCast(row)], out);
    }

    fn keysOf(self: RowIndex, row: i32, out: []Field) void {
        self.side.keysAt(self.row_at.items[@intCast(row)], self.key_size, out);
    }

    /// The high bits of an FNV hash are the well-mixed ones; fold them down.
    fn slotOf(self: RowIndex, hash: u64) usize {
        return @as(usize, @intCast((hash ^ (hash >> 32)) & 0xffff_ffff)) & self.mask;
    }

    /// Starts the fetch of the slot `hash` will land in, without waiting for it.
    ///
    /// Every probe of this table is a random access into tens of megabytes, so it
    /// misses to memory, and the row after it needs a different line: the loop
    /// spends most of its time waiting on a load whose address was known long
    /// before it was issued. Asking for the line `PREFETCH_AHEAD` rows early turns
    /// that serial chain of misses into overlapping ones.
    fn prefetch(self: *const RowIndex, hash: u64) void {
        @prefetch(&self.table[self.slotOf(hash)], .{ .rw = .read, .locality = 3, .cache = .data });
    }

    /// Only reached if the row count was underestimated: `build` sizes the table
    /// for the rows it is about to insert, so the common path never grows it.
    fn rehash(self: *RowIndex) !void {
        const table = try self.gpa.alloc(u32, self.table.len * 2);
        @memset(table, EMPTY_SLOT);
        self.gpa.free(self.table);
        self.table = table;
        self.mask = table.len - 1;
        for (self.first_row.items, 0..) |row, key| {
            const hash = self.row_hash.items[@intCast(row)];
            var slot = self.slotOf(hash);
            while (self.table[slot] != EMPTY_SLOT) slot = (slot + 1) & self.mask;
            self.table[slot] = self.slotFor(hash, key);
        }
    }

    /// The row carrying `fields`' key, or null. `other` is the slab those fields
    /// live in, which is the opposite file when this is a join probe. `probe` is
    /// scratch the caller owns: the join runs several chunks at once, and a buffer
    /// hanging off the index would be shared between them.
    ///
    /// On a hit, `probe` holds as much of that row as `want` asked for — it is
    /// what the key columns were compared against. The join used to read the row
    /// again on the line after this one returned, which is a second parse of
    /// every matched row in the file.
    ///
    /// `want` is `.whole` for the side that goes on to compare the columns, and
    /// `.keys` for the side that only asks whether the key exists at all and
    /// throws the answer away.
    fn lookup(
        self: *const RowIndex,
        other: Slab,
        fields: ?[]const Field,
        hash: u64,
        want: Want,
        span: ?Span,
        s: *Scratch,
        probe: []Field,
    ) !?Hit {
        var slot = self.slotOf(hash);
        while (true) {
            const word = self.table[slot];
            if (word == EMPTY_SLOT) return null;
            // The tag rejects almost every collision without leaving this word.
            if (self.tagIs(word, hash)) {
                const candidate = self.first_row.items[self.posOf(word)];
                if (self.row_hash.items[@intCast(candidate)] == hash) {
                    // The bytes first, where the caller offered them. A candidate
                    // whose row opens with the same bytes as far as either file
                    // reads has the same keys and the same columns, and neither
                    // row needs parsing to say so. A candidate the tag let
                    // through with a different key fails this on its first few
                    // bytes, so the cost of being wrong is a handful of them.
                    if (span) |sp| {
                        const proved = switch (sp) {
                            .csv => self.rowMatches(candidate, sp.csv.bytes, sp.csv.delimiter),
                            .json => self.jsonMatches(candidate, sp.json.bytes),
                        };
                        if (proved) return .{ .row = candidate, .same_bytes = true };
                    }
                    // No parsed fields: the caller has the span but has not parsed
                    // the row. A candidate the bytes did not settle is returned
                    // unproven, and the caller parses and re-looks-up with the key
                    // check, which also covers the hash collision this skipped.
                    const fa = fields orelse return .{ .row = candidate, .same_bytes = false };
                    switch (want) {
                        .whole => self.fieldsOf(candidate, probe),
                        .keys => self.keysOf(candidate, probe),
                    }
                    var ok = true;
                    for (0..self.key_size) |i| {
                        if (!(try same(self.side.slab, probe[i], other, fa[i], self.opt, s))) {
                            ok = false;
                            break;
                        }
                    }
                    if (ok) return .{ .row = candidate, .same_bytes = false };
                }
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// Whether `candidate`'s row opens with exactly `sp.bytes` and ends that run
    /// on a field boundary.
    ///
    /// The boundary is what makes the run a whole number of columns rather than
    /// a truncation of one: without it `12,3` would match a row opening `12,34`.
    fn rowMatches(self: *const RowIndex, candidate: i32, bytes: []const u8, delimiter: u8) bool {
        const data = self.side.slab.data;
        const from: usize = @intCast(self.row_at.items[@intCast(candidate)]);
        // Both tests, and in this order: `data.len - bytes.len` underflows for
        // a run longer than this whole file, which a release build would wrap
        // rather than trap. A's rows and B's bytes come from different files and
        // nothing bounds one by the other.
        if (bytes.len > data.len or from > data.len - bytes.len) return false;
        const to = from + bytes.len;
        if (to < data.len and data[to] != delimiter and data[to] != '\n') return false;
        return std.mem.eql(u8, data[from..to], bytes);
    }

    /// Where `row` ends: the next row's start, or the end of the file.
    ///
    /// Rows are inserted in the order they were swept and the sweep runs its
    /// chunks in file order, so `row_at` ascends and row `n + 1` begins where
    /// row `n` stops.
    fn rowEnd(self: *const RowIndex, row: i32) usize {
        const next: usize = @as(usize, @intCast(row)) + 1;
        if (next < self.row_at.items.len) return @intCast(self.row_at.items[next]);
        return self.side.slab.data.len;
    }

    /// Whether `candidate`'s row opens with exactly `bytes` and names nothing
    /// this run tracks in what follows.
    ///
    /// The second half is the whole difference from CSV. A byte-equal prefix says
    /// the two rows hold the same values *written in the prefix*; it cannot say
    /// the mate does not name one of them again further on, and in JSON a
    /// repeated name takes its last value. What is left to scan is the trailing
    /// ignored columns, which is the only reason this is cheaper than parsing.
    fn jsonMatches(self: *const RowIndex, candidate: i32, bytes: []const u8) bool {
        const parser = self.side.parser() orelse return false;
        const data = self.side.slab.data;
        const from: usize = @intCast(self.row_at.items[@intCast(candidate)]);
        const end = self.rowEnd(candidate);
        return text.jsonRowsMatch(parser, bytes, data, from, end);
    }

    fn uniqueKeys(self: RowIndex) i64 {
        return @intCast(self.first_row.items.len);
    }
};

// ---------------------------------------------------------------------------
// The sweep: finding and hashing every row, in parallel
// ---------------------------------------------------------------------------

/// Where each chunk begins, as offsets of real row starts.
///
/// The nominal split is `size / threads`, walked forward to the next row. Walking
/// forward is the whole difficulty for CSV: a newline inside a quoted field is not
/// a row boundary, and a thread starting mid-file cannot tell whether it is inside
/// one. Parity settles it — every `"` toggles in-quote state, including both halves
/// of a doubled quote, which toggles twice and so correctly leaves the state alone,
/// so the count of quotes before a position says whether that position is inside a
/// field. Counting them is a scan for one byte, far cheaper than parsing.
///
/// JSON needs none of that: a raw newline inside a string is not valid JSON, so
/// every newline ends a record.
fn chunkBounds(
    gpa: std.mem.Allocator,
    data: []const u8,
    from: usize,
    threads: usize,
    dialect: Dialect,
    count_quotes: bool,
) ![]usize {
    // Without the count a CSV split is taken at the next newline, as JSON's is:
    // a guess, which `sweep` checks.
    const quoted = dialect == .csv and count_quotes;
    var bounds: std.ArrayList(usize) = .empty;
    errdefer bounds.deinit(gpa);
    const end = data.len;
    if (threads <= 1 or end - from < CHUNKING_THRESHOLD) {
        try bounds.appendSlice(gpa, &.{ from, end });
        return bounds.toOwnedSlice(gpa);
    }

    try bounds.append(gpa, from);
    // Quotes before each split point, counted once per byte rather than once per
    // byte per split point.
    //
    // Asking for the count from `from` to each split in turn reads the first
    // slice of the file for every split, the second for all but one, and so on:
    // `(threads - 1) / 2` passes over the file, and all of them on this thread,
    // to prepare a sweep whose whole purpose is to use the others. Counting each
    // slice on its own and running a prefix sum over the results gives the same
    // numbers for one pass.
    var before = try gpa.alloc(usize, threads);
    defer gpa.free(before);
    if (quoted) {
        var running: usize = 0;
        var prev = from;
        for (1..threads) |i| {
            const nominal = from + (end - from) * i / threads;
            running += scan.countByte(data, prev, nominal, '"');
            before[i] = running;
            prev = nominal;
        }
    } else {
        @memset(before, 0);
    }
    for (1..threads) |i| {
        const nominal = from + (end - from) * i / threads;
        var in_quotes = quoted and before[i] % 2 == 1;
        var at = nominal;
        while (at < end) : (at += 1) {
            const c = data[at];
            if (c == '"' and quoted) {
                in_quotes = !in_quotes;
            } else if (c == '\n' and !in_quotes) {
                at += 1;
                break;
            }
        }
        if (at > bounds.items[bounds.items.len - 1] and at < end) try bounds.append(gpa, at);
    }
    try bounds.append(gpa, end);
    return bounds.toOwnedSlice(gpa);
}

/// The shared state of a parallel sweep: a range each, and the first failure.
const Sweep = struct {
    gpa: std.mem.Allocator,
    side: *const Side,
    key_size: usize,
    opt: Options,
    bounds: []const usize,
    /// Set for a columnar file, where a chunk is a range of row numbers rather
    /// than a range of bytes.
    columnar_rows: usize,
    chunks: []Chunk,
    next: std.atomic.Value(usize),
    /// When the join runs inside the sweep, where each row goes once the next
    /// row's start says where it ends.
    sink: ?*FusedSink = null,

    fn run(self: *Sweep) void {
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= self.chunks.len) return;
            self.one(i) catch |e| {
                self.chunks[i].failure = e;
                return;
            };
        }
    }

    fn one(self: *Sweep, i: usize) !void {
        const gpa = self.gpa;
        const fields_line = try privateAlloc(gpa, Field, self.side.width);
        defer gpa.free(fields_line);
        const fields = fields_line[0..self.side.width];
        // The sweep parses with the key-only parser, so it needs room for the
        // key columns; the full-width buffer above is what a row over the field
        // cap is re-read into, and what the columnar branch fills.
        const keys_line = try privateAlloc(gpa, Field, @max(1, self.key_size));
        defer gpa.free(keys_line);
        const keys = keys_line[0..@max(1, self.key_size)];
        var s = Scratch{};
        var chunk = &self.chunks[i];

        switch (self.side.rows) {
            .columnar => {
                const lo = self.columnar_rows * i / self.chunks.len;
                const hi = self.columnar_rows * (i + 1) / self.chunks.len;
                try chunk.at.ensureTotalCapacity(gpa, hi - lo);
                try chunk.hash.ensureTotalCapacity(gpa, hi - lo);
                for (lo..hi) |row| {
                    self.side.fieldsAt(row, fields);
                    chunk.at.appendAssumeCapacity(row);
                    chunk.hash.appendAssumeCapacity(
                        try keyHash(self.side.slab, fields, self.key_size, self.opt, &s.a),
                    );
                }
            },
            .text => |t| {
                const data = self.side.slab.data;
                var pos = self.bounds[i];
                const stop = self.bounds[i + 1];
                // Rows that *start* in this chunk belong to it; the last one is
                // finished past the boundary rather than cut in half.
                while (pos < stop) {
                    // A line with nothing on it is not a row.
                    if (data[pos] == '\n') {
                        pos += 1;
                        continue;
                    }
                    if (data[pos] == '\r' and pos + 1 < data.len and data[pos + 1] == '\n') {
                        pos += 2;
                        continue;
                    }
                    const next = t.keys.parse(data, pos, data.len, keys);
                    // A field cannot be longer than the row that holds it, so
                    // only a row over the cap can hide an over-long column the
                    // key parser never looked at. That row, and only that row, is
                    // re-read in full to find it. At a hundred and eighty bytes a
                    // row this is one comparison and never taken; the check it
                    // replaces walked twenty fields of every row of the file.
                    for (keys) |field| if (field == TOO_LONG) return Error.FieldTooLong;
                    if (next -| pos > fld.MAX_FIELD_LEN) {
                        _ = t.parser.parse(data, pos, data.len, fields);
                        for (fields) |field| if (field == TOO_LONG) return Error.FieldTooLong;
                    }
                    try push(u64, &chunk.at, gpa, pos);
                    try push(u64, &chunk.hash, gpa, try keyHash(self.side.slab, keys, self.key_size, self.opt, &s.a));
                    // The row before this one ends where this one starts, which
                    // is what `rowEnd` will say; its pages are still in memory.
                    if (self.sink) |sink| {
                        const n = chunk.at.items.len;
                        if (n >= 2) try sink.row(i, @intCast(chunk.at.items[n - 2]), pos, chunk.hash.items[n - 2]);
                    }
                    if (next <= pos) break; // no progress: a malformed tail, not a loop
                    pos = next;
                }
                chunk.stopped = pos;
            },
        }
    }
};

/// `append`, with the capacity check inline: two of these run per row of both
/// files, and `append` makes each one a call to `ensureTotalCapacity` that
/// almost always finds room. Only a full list pays for the call now.
inline fn push(comptime T: type, list: *std.ArrayList(T), gpa: std.mem.Allocator, value: T) !void {
    if (list.items.len == list.capacity) try list.ensureUnusedCapacity(gpa, 1);
    list.appendAssumeCapacity(value);
}

fn sweep(
    gpa: std.mem.Allocator,
    side: *const Side,
    key_size: usize,
    opt: Options,
    threads: usize,
    sink: ?*FusedSink,
) ![]Chunk {
    const t = switch (side.rows) {
        .text => |t| t,
        .columnar => return sweepOnce(gpa, side, key_size, opt, threads, &.{}, sink),
    };
    const data = side.slab.data;
    if (t.from >= data.len) return gpa.alloc(Chunk, 0);

    // The CSV splits are guessed, then checked, as the C and C++ ports do.
    //
    // Counted splits cost a pass over most of the file on one thread before any
    // chunk starts. With one chunk a file that was never paid; the join inside
    // the sweep takes the whole budget, and on the 150M pair the count read
    // most of each 26 GB file from disk a second time. A guess lands on the
    // next newline; chunk i's sweep stops at the first row start past its end,
    // and if that is where chunk i + 1 began, chunk i + 1 started on a real
    // row. Only a guess inside a quoted field breaks that, and then the sweep
    // runs again on counted splits. A chunk that started on a wrong guess
    // parsed garbage, so its rows, its failure and what it handed the sink are
    // all thrown away.
    var bounds = try chunkBounds(gpa, data, t.from, threads, side.slab.dialect, false);
    defer gpa.free(bounds);
    var chunks = try sweepOnce(gpa, side, key_size, opt, threads, bounds, sink);
    errdefer freeChunks(gpa, chunks);
    var held = true;
    for (chunks[0..chunks.len -| 1], 0..) |c, i| {
        if (c.failure) |e| return e; // its start is proven
        if (c.stopped != bounds[i + 1]) {
            held = false;
            break;
        }
    }
    if (!held) {
        freeChunks(gpa, chunks);
        chunks = &.{};
        if (sink) |k| k.reset();
        gpa.free(bounds);
        bounds = &.{};
        bounds = try chunkBounds(gpa, data, t.from, threads, side.slab.dialect, true);
        chunks = try sweepOnce(gpa, side, key_size, opt, threads, bounds, sink);
    }
    for (chunks) |c| {
        if (c.failure) |e| return e;
    }
    // Each chunk's last row ends where the next chunk's first row starts, which
    // only the next chunk knows, so those rows are handed over now.
    if (sink) |k| {
        for (chunks, 0..) |c, i| {
            if (c.at.items.len == 0) continue;
            var hi: usize = data.len;
            for (chunks[i + 1 ..]) |later| {
                if (later.at.items.len > 0) {
                    hi = @intCast(later.at.items[0]);
                    break;
                }
            }
            try k.row(i, @intCast(c.at.items[c.at.items.len - 1]), hi, c.hash.items[c.hash.items.len - 1]);
        }
    }
    return chunks;
}

fn freeChunks(gpa: std.mem.Allocator, chunks: []Chunk) void {
    for (chunks) |*c| c.deinit(gpa);
    gpa.free(chunks);
}

/// One sweep over `bounds` (text) or over the rows split `threads` ways
/// (columnar). A text chunk's failure is left in the chunk for the caller,
/// which knows whether the chunk's start was a real row.
fn sweepOnce(
    gpa: std.mem.Allocator,
    side: *const Side,
    key_size: usize,
    opt: Options,
    threads: usize,
    bounds: []const usize,
    sink: ?*FusedSink,
) ![]Chunk {
    const parts: usize = switch (side.rows) {
        .text => bounds.len - 1,
        .columnar => |c| @max(1, @min(threads, (c.rows + (1 << 14) - 1) / (1 << 14))),
    };

    const chunks = try gpa.alloc(Chunk, parts);
    errdefer {
        for (chunks) |*c| c.deinit(gpa);
        gpa.free(chunks);
    }
    for (chunks) |*c| c.* = .{};

    var work = Sweep{
        .gpa = gpa,
        .side = side,
        .key_size = key_size,
        .opt = opt,
        .bounds = bounds,
        .columnar_rows = switch (side.rows) {
            .columnar => |c| c.rows,
            .text => 0,
        },
        .chunks = chunks,
        .next = std.atomic.Value(usize).init(0),
        .sink = sink,
    };
    try runOnThreads(&work, Sweep.run, parts);
    if (side.rows == .columnar) {
        for (chunks) |c| {
            if (c.failure) |e| return e;
        }
    }
    return chunks;
}

/// Scratch a thread writes on every row, on cache lines no other thread's
/// allocation can share.
///
/// `smp_allocator` starts every thread on the same slot and moves one only when
/// that slot is contended, and it aligns a small allocation only to its own
/// size. So the key buffer A's sweep parses into on every row -- sixteen bytes
/// for a two-column key -- and B's, allocated a moment apart on two threads,
/// come out thirty-two bytes apart on one cache line in most runs, and the line
/// moves between the two cores once a row. Rounding the length up to whole
/// lines and aligning to one makes the buffer the only thing on its lines.
/// Free the slice this returns, not a narrower one: the allocator finds the
/// size class from the length.
fn privateAlloc(gpa: std.mem.Allocator, comptime T: type, n: usize) ![]align(std.atomic.cache_line) T {
    const per_line = @max(1, std.atomic.cache_line / @sizeOf(T));
    const len = std.mem.alignForward(usize, @max(n, 1), per_line);
    return gpa.alignedAlloc(T, .fromByteUnits(std.atomic.cache_line), len);
}

/// Runs `entry` on `ways` threads, or on this one where a thread cannot be had:
/// a thread that will not spawn is not a reason to fail, it is the same work.
pub fn runOnThreads(state: anytype, comptime entry: anytype, ways: usize) !void {
    var workers: [64]?std.Thread = undefined;
    const spawned = @min(if (ways > 0) ways - 1 else 0, workers.len);
    for (0..spawned) |i| workers[i] = std.Thread.spawn(.{}, entry, .{state}) catch null;
    entry(state);
    for (0..spawned) |i| {
        if (workers[i]) |w| w.join() else entry(state);
    }
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

fn has(haystack: []const []const u8, needle: []const u8) bool {
    for (haystack) |h| {
        if (std.mem.eql(u8, h, needle)) return true;
    }
    return false;
}

fn indexOf(haystack: []const []const u8, needle: []const u8) ?usize {
    for (haystack, 0..) |h, i| {
        if (std.mem.eql(u8, h, needle)) return i;
    }
    return null;
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

pub const Result = struct {
    counts: Counts,
    columns: []ColumnStat,

    pub fn identical(self: Result) bool {
        return self.counts.changed == 0 and self.counts.added == 0 and self.counts.removed == 0;
    }

    /// The column names are owned copies, so the header they were read from can be
    /// released as soon as the comparison is done with it.
    pub fn deinit(self: *Result, gpa: std.mem.Allocator) void {
        for (self.columns) |c| gpa.free(c.name);
        gpa.free(self.columns);
    }
};

/// One chunk of A's keys: the matched, changed and removed counts and the column
/// stats. The merge is a sum, so the answer does not depend on how many chunks
/// there are or on which thread took which.
///
/// There are no chunks of B. `added` is not counted by a pass any more; see the
/// join's caller.
const Part = struct {
    matched: i64 = 0,
    changed: i64 = 0,
    removed: i64 = 0,
    columns: []ColumnStat,
    /// Written only by the thread that owns this range, read only once every
    /// thread has been joined.
    failure: ?anyerror = null,
};

const Join = struct {
    a: *const Side,
    b: *const Side,
    ai: *const RowIndex,
    bi: *const RowIndex,
    opt: Options,
    key_size: usize,
    nc: usize,
    width: usize,
    parts: []Part,
    /// Where a byte comparison may stand in for a parse, when the two files are
    /// the same shape; see `text.sharedTail`. Null compares every pair column by
    /// column, which is what a mixed pair, a columnar side or JSON gets.
    span_tail: ?text.Tail,
    /// The same fast path in runs, where an unwanted column sits among the
    /// wanted ones; see `text.Runs`. Null where there is no gap.
    runs: ?text.Runs = null,
    /// What JSON gets instead: no fixed offset to promise, so the run is built
    /// per row and the mate's tail is scanned. See `RowIndex.jsonMatches`.
    json_proof: bool,
    next: std.atomic.Value(usize),

    fn run(self: *Join) void {
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= self.parts.len) return;
            const work = self.range(i);
            work catch |e| {
                self.parts[i].failure = e;
                return;
            };
        }
    }

    fn range(self: *Join, p: usize) !void {
        var rs = try RowScratch.init(self);
        defer rs.deinit(self.a.gpa);
        const out = &self.parts[p];

        const keys = self.ai.first_row.items;
        const lo = keys.len * p / self.parts.len;
        const hi = keys.len * (p + 1) / self.parts.len;
        const mine = keys[lo..hi];
        // A proof that keeps failing is a scan for nothing: two files where every
        // row really has changed would pay for it on every row. After
        // `PROOF_BACKOFF` failures in a row it is tried every `PROOF_BACKOFF`
        // rows instead, until one succeeds and it is on again.
        var refused: usize = 0;
        for (mine, 0..) |row, at| {
            if (at + PREFETCH_AHEAD < mine.len) {
                self.bi.prefetch(self.ai.row_hash.items[@intCast(mine[at + PREFETCH_AHEAD])]);
            }
            // The hash is the one the sweep computed for this row: the same
            // bytes through the same function, so computing it again here would
            // be a second pass over every key in the file for the same number.
            const row_lo: usize = @intCast(self.ai.row_at.items[@intCast(row)]);
            try self.joinRow(out, &rs, row_lo, self.ai.rowEnd(row), self.ai.row_hash.items[@intCast(row)], at, &refused);
        }
    }

    /// One row of A against its mate in B, counted into `out`: the row's bytes
    /// from `row_lo` to `row_hi`, the next row's start, which is what `rowEnd`
    /// gives -- by offsets rather than by row number, so the join can run while
    /// A's index is still being built (see `fused` in `compare`). `at` and
    /// `refused` are the proof's backoff; they decide how the answer is
    /// reached, never what it is.
    fn joinRow(
        self: *const Join,
        out: *Part,
        rs: *RowScratch,
        row_lo: usize,
        row_hi: usize,
        hash: u64,
        at: usize,
        refused: *usize,
    ) !void {
        // Nothing to normalise and no tolerance: the cell comparison cannot
        // fail, so it need not be asked through an error union.
        const plain = !needsNormalising(self.opt);
        // The proof's span without the parse: the guard's delimiter sits a
        // fixed count from the row start, countable with the delimiter
        // cursor alone. Most rows prove out here and never pay for fields;
        // a row the bytes do not settle falls through to the parse below.
        const guard = self.span_tail;
        const attempt = guard != null and (refused.* < PROOF_BACKOFF or at & (PROOF_BACKOFF - 1) == 0);
        if (attempt) {
            const t = guard.?;
            const data = self.a.slab.data;
            const from = row_lo;
            const end = row_hi;
            if (self.runs) |runs| gapped: {
                if (!runs.ofRow(data, from, end, t.delimiter, rs.spans)) break :gapped;
                // No fields and no span: the first candidate whose whole
                // hash matches, unproven, which the runs then settle.
                if (try self.bi.lookup(self.a.slab, null, hash, .whole, null, &rs.s, rs.fb)) |hit| {
                    const bd = self.b.slab.data;
                    const b_lo: usize = @intCast(self.bi.row_at.items[@intCast(hit.row)]);
                    if (runs.matches(data, rs.spans, bd, b_lo, self.bi.rowEnd(hit.row), t.delimiter)) {
                        out.matched += 1;
                        refused.* = 0;
                        return;
                    }
                    if (refused.* < PROOF_BACKOFF) refused.* += 1;
                } else {
                    out.removed += 1;
                    return;
                }
            } else if (text.guardSpan(data, from, end, t.delimiter, t.src + 1)) |need| {
                const span: Span = .{ .csv = .{ .bytes = data[from .. from + need], .delimiter = t.delimiter } };
                if (try self.bi.lookup(self.a.slab, null, hash, .whole, span, &rs.s, rs.fb)) |hit| {
                    if (hit.same_bytes) {
                        out.matched += 1;
                        refused.* = 0;
                        return;
                    }
                    // A candidate the bytes did not settle: fall through to
                    // the parse and the key-checking lookup below, which also
                    // covers the hash collision this skipped past.
                    if (refused.* < PROOF_BACKOFF) refused.* += 1;
                } else {
                    out.removed += 1;
                    return;
                }
            }
        }
        self.a.fieldsAt(row_lo, rs.fa);
        // A's row up to the end of the last column either file wants. The
        // end has to be a boundary in A as well: a quoted field ends on its
        // closing quote, and what follows is not part of the run.
        const span: ?Span = blk: {
            const tail = self.span_tail orelse break :blk null;
            const f = rs.fa[tail.slot];
            if (!fld.isReal(f)) break :blk null;
            const data = self.a.slab.data;
            const from = row_lo;
            const to = fld.offsetOf(f) + fld.lenOf(f);
            if (to < from or to > data.len) break :blk null;
            if (to < data.len and data[to] != tail.delimiter and data[to] != '\n') {
                break :blk null;
            }
            break :blk .{ .csv = .{ .bytes = data[from..to], .delimiter = tail.delimiter } };
        };
        // What JSON gets instead, since `sharedTail` cannot promise it an
        // offset. Through the byte that closes the last value either file
        // wants -- whichever it turns out to be, since two objects need not
        // list their names in the same order. Every wanted field and not only
        // the compared ones: the keys have to be inside the run for it to
        // stand in for the key comparison, which is what lets it run before
        // the mate is parsed.
        const proof: ?Span = span orelse blk: {
            if (!self.json_proof) break :blk null;
            if (refused.* >= PROOF_BACKOFF and at & (PROOF_BACKOFF - 1) != 0) break :blk null;
            const data = self.a.slab.data;
            const from = row_lo;
            const end = row_hi;
            var t = from;
            for (rs.fa[0..self.width]) |f| {
                if (!fld.isReal(f)) continue;
                const e = fld.offsetOf(f) + fld.lenOf(f);
                if (e > t) t = e;
            }
            if (t >= end or t < from) break :blk null;
            break :blk .{ .json = .{ .bytes = data[from .. t + 1] } };
        };
        const hit = (try self.bi.lookup(self.a.slab, rs.fa, hash, .whole, proof, &rs.s, rs.fb)) orelse {
            out.removed += 1;
            return;
        };
        out.matched += 1;
        // `same_bytes` is the proof's own verdict, so it is what the backoff
        // counts. A run of failures means two files whose rows really do
        // differ, and scanning them is work for nothing.
        if (self.json_proof and proof != null) {
            if (hit.same_bytes) {
                refused.* = 0;
            } else if (refused.* < PROOF_BACKOFF) {
                refused.* += 1;
            }
        }
        // The two rows carry the same bytes across every column either file
        // wants, so no column differs and the mate was never read.
        if (hit.same_bytes) return;
        var any = false;
        // Two loops rather than one with a flag inside it: `plain` cannot
        // change between columns or between rows, and this is the innermost
        // loop of the whole comparison -- seventeen columns of ten million
        // matched rows.
        if (plain) {
            for (0..self.nc) |i| {
                const x = rs.fa[self.key_size + i];
                const y = rs.fb[self.key_size + i];
                const xa = plainAbsent(x);
                const yb = plainAbsent(y);
                if (plainDiffers(self.a.slab, x, xa, self.b.slab, y, yb)) {
                    any = true;
                    out.columns[i].changed += 1;
                    // Absence is already known: the general path asks for it
                    // twice more, having thrown away the answer.
                    if (yb) out.columns[i].blanked += 1;
                    if (xa) out.columns[i].filled += 1;
                }
            }
        } else {
            for (0..self.nc) |i| {
                const x = rs.fa[self.key_size + i];
                const y = rs.fb[self.key_size + i];
                if (try cellDiffers(self.a.slab, x, self.b.slab, y, self.opt, &rs.s)) {
                    any = true;
                    out.columns[i].changed += 1;
                    if (try isAbsent(self.b.slab, y, self.opt, &rs.s.b)) out.columns[i].blanked += 1;
                    if (try isAbsent(self.a.slab, x, self.opt, &rs.s.a)) out.columns[i].filled += 1;
                }
            }
        }
        if (any) out.changed += 1;
    }
};

/// One join worker's buffers: A's fields; the lookup's scratch, which on a hit
/// already holds the mate's fields; two words a run for the gapped proof; and
/// the normalising scratch.
const RowScratch = struct {
    fa: []Field,
    fb: []Field,
    spans: []usize,
    s: Scratch = .{},

    fn init(join: *const Join) !RowScratch {
        const gpa = join.a.gpa;
        const fa = try gpa.alloc(Field, join.width);
        errdefer gpa.free(fa);
        const fb = try gpa.alloc(Field, join.width);
        errdefer gpa.free(fb);
        const spans = try gpa.alloc(usize, if (join.runs) |r| 2 * r.seg.len else 0);
        return .{ .fa = fa, .fb = fb, .spans = spans };
    }

    fn deinit(self: *RowScratch, gpa: std.mem.Allocator) void {
        gpa.free(self.fa);
        gpa.free(self.fb);
        gpa.free(self.spans);
    }
};

/// The join inside A's sweep: each sweep chunk joins its rows into its own part
/// as it finds them. See `fused` in `compare`.
const FusedSink = struct {
    join: *const Join,
    parts: []Part,
    scratch: []RowScratch,
    seen: []usize,
    refused: []usize,

    fn init(gpa: std.mem.Allocator, join: *const Join, parts: []Part) !FusedSink {
        const scratch = try gpa.alloc(RowScratch, parts.len);
        var made: usize = 0;
        errdefer {
            for (scratch[0..made]) |*x| x.deinit(gpa);
            gpa.free(scratch);
        }
        for (scratch) |*x| {
            x.* = try RowScratch.init(join);
            made += 1;
        }
        const seen = try gpa.alloc(usize, parts.len);
        errdefer gpa.free(seen);
        @memset(seen, 0);
        const refused = try gpa.alloc(usize, parts.len);
        @memset(refused, 0);
        return .{ .join = join, .parts = parts, .scratch = scratch, .seen = seen, .refused = refused };
    }

    fn deinit(self: *FusedSink, gpa: std.mem.Allocator) void {
        for (self.scratch) |*x| x.deinit(gpa);
        gpa.free(self.scratch);
        gpa.free(self.seen);
        gpa.free(self.refused);
    }

    /// Everything the sink was handed came from a sweep that started over.
    fn reset(self: *FusedSink) void {
        for (self.parts) |*p| {
            p.matched = 0;
            p.changed = 0;
            p.removed = 0;
            for (p.columns) |*c| {
                c.changed = 0;
                c.blanked = 0;
                c.filled = 0;
            }
        }
        @memset(self.seen, 0);
        @memset(self.refused, 0);
    }

    fn row(self: *FusedSink, i: usize, lo: usize, hi: usize, hash: u64) !void {
        try self.join.joinRow(&self.parts[i], &self.scratch[i], lo, hi, hash, self.seen[i], &self.refused[i]);
        self.seen[i] += 1;
    }
};

/// The rows that repeat a key, joined again into parts of their own after a
/// join inside the sweep, to be subtracted: the same bytes against the same
/// index give the same counts.
const TakeBack = struct {
    join: *const Join,
    rows: []const i32,
    parts: []Part,
    next: std.atomic.Value(usize),

    fn run(self: *TakeBack) void {
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= self.parts.len) return;
            self.one(i) catch |e| {
                self.parts[i].failure = e;
                return;
            };
        }
    }

    fn one(self: *TakeBack, p: usize) !void {
        var rs = try RowScratch.init(self.join);
        defer rs.deinit(self.join.a.gpa);
        const ai = self.join.ai;
        const lo = self.rows.len * p / self.parts.len;
        const hi = self.rows.len * (p + 1) / self.parts.len;
        var refused: usize = 0;
        for (self.rows[lo..hi], lo..) |row, at| {
            const row_lo: usize = @intCast(ai.row_at.items[@intCast(row)]);
            try self.join.joinRow(&self.parts[p], &rs, row_lo, ai.rowEnd(row), ai.row_hash.items[@intCast(row)], at, &refused);
        }
    }
};

/// One file's whole preparation — decode or map it, then index it — so that both
/// files can be done at once.
const Prepare = struct {
    gpa: std.mem.Allocator,
    input: Input,
    wanted: []const []const u8,
    key_size: usize,
    opt: Options,
    threads: usize,
    tag: []const u8,
    side: ?Side = null,
    index: ?RowIndex = null,
    failure: ?anyerror = null,

    fn run(self: *Prepare) void {
        self.go() catch |e| {
            self.failure = e;
        };
    }

    fn go(self: *Prepare) !void {
        // Near zero for text, where the fields are parsed later in the sweep,
        // and the largest phase of a compressed Parquet run, where every page is
        // decompressed and materialised here. It was outside the table until a
        // zstd run showed 0.19s of phases against a 1.0s wall.
        var phases = Phases.start(self.tag);
        self.side = try self.input.project(self.gpa, self.wanted, self.key_size, self.threads);
        phases.mark("read columns (par)");
        self.index = try RowIndex.build(
            self.gpa,
            &self.side.?,
            self.key_size,
            self.opt,
            self.threads,
            self.tag,
            null,
        );
    }

    fn deinit(self: *Prepare) void {
        if (self.index) |*i| i.deinit();
        if (self.side) |*s| s.deinit();
    }
};

/// Compares two files, taking every byte it needs from `gpa`.
///
/// Hand it a `FixedBufferAllocator` and the comparison cannot exceed that buffer:
/// it returns `error.OutOfMemory` at the allocation that would have crossed the
/// line. That is the whole reason this port is in Zig.
pub fn compare(
    io: std.Io,
    gpa: std.mem.Allocator,
    a_path: []const u8,
    b_path: []const u8,
    opt: Options,
) !Result {
    const total = if (opt.threads > 0) opt.threads else @max(1, std.Thread.getCpuCount() catch 1);

    const a_input = Input.open(gpa, io, a_path, opt) catch |e| return named(e, a_path);
    defer a_input.freeNames(gpa);
    const b_input = Input.open(gpa, io, b_path, opt) catch |e| return named(e, b_path);
    defer b_input.freeNames(gpa);
    const a_names = a_input.names();
    const b_names = b_input.names();

    for (opt.key) |k| {
        if (!has(a_names, k) or !has(b_names, k)) return named(Error.KeyColumnMissing, k);
    }

    // A name in neither file is a typo, and this is the typo that hides. `--key`
    // makes the answer impossible and `--compare` refuses; a misspelled
    // `--ignore` silently compares the column it meant to drop and calls it
    // changed on every row.
    //
    // In *neither* file rather than in both: ignore is subtractive, so a name
    // only one side carries is real and does no harm -- that column is not
    // compared either way. Only a name nothing has can be a mistake.
    for (opt.ignore) |c| {
        if (!has(a_names, c) and !has(b_names, c)) return named(Error.IgnoredColumnMissing, c);
    }

    var compared: std.ArrayList([]const u8) = .empty;
    defer compared.deinit(gpa);
    if (opt.compare.len > 0) {
        // Named explicitly: the order is the caller's, and a name not in both files
        // is an error. A key or an ignored column is dropped rather than compared
        // -- `-k id -c id,a` compares `a`, `-c a,b -i b` compares `a` -- the rule
        // the C and Rust ports apply. This port used to compare them, and with
        // `-i` that miscounted: every row whose ignored column differed was changed.
        for (opt.compare) |c| {
            if (!has(a_names, c) or !has(b_names, c)) return named(Error.ComparedColumnMissing, c);
            if (has(opt.key, c) or has(opt.ignore, c)) continue;
            try compared.append(gpa, c);
        }
    } else {
        for (a_names) |c| {
            if (has(b_names, c) and !has(opt.key, c) and !has(opt.ignore, c))
                try compared.append(gpa, c);
        }
    }

    const key_size = opt.key.len;
    const nc = compared.items.len;
    const width = key_size + nc;

    var wanted: std.ArrayList([]const u8) = .empty;
    defer wanted.deinit(gpa);
    for (opt.key) |k| try wanted.append(gpa, k);
    for (compared.items) |c| try wanted.append(gpa, c);

    // The two files share nothing until the join, so they are read at the same
    // time -- one thread each.
    //
    // Each file used to be split further, `total / 2` ways, on the reasoning that
    // two files across four cores is two chunks each and the whole machine busy.
    // Measured, it was the opposite. On the 4M CSV pair the sweep took 0.35s a
    // side unsplit and 0.46-0.58s split two ways: *slower* with twice the
    // threads, and the whole run 1.50x wall and 2.06x CPU better without it.
    //
    // That was blamed on the allocator: the same code linked against libc and
    // given `c_allocator` scaled, and handed `smp_allocator` it did not. The
    // allocator was where the cause was hiding rather than the cause.
    // `smp_allocator` starts every thread on the same slot and aligns a small
    // allocation only to its own size, so the sixteen-byte buffer A's sweep
    // parses its key into on every row and B's came out on one cache line in
    // most runs, and the line moved between the two cores once a row. glibc's
    // per-thread arenas happened to keep them apart. `privateAlloc` now gives
    // that scratch lines of its own: on the 2M pairs at four threads the sweep
    // went 0.349s to 0.208s for CSV and 0.565s to 0.329s for JSON, and the
    // whole run 1.22x and 1.26x on less CPU. See BENCHMARKS.md.
    //
    // The split was tried again on CI before the key buffer was found, with the
    // chunks' own lists unshared, and did not pay: on an EPYC 9V45 it took the
    // 10M CSV sweep from 0.49s to 0.62s, the serial quote count in front of it
    // included. That run still had the shared key buffer in it, so it is not
    // the last word on splitting; it is why this change does not also split.
    //
    // `gpa` still has to be thread-safe, since A and B run at once. Under
    // --max-memory it is a FixedBufferAllocator -- a bump pointer with no lock,
    // which would hand two threads the same bytes -- so main.zig passes its
    // lock-taking variant. The budget it enforces is unchanged.
    const per_file: usize = 1;

    // Past page cache the join runs inside A's sweep instead of after it.
    //
    // Past memory every pass is a read from disk, and the join used to be the
    // fourth: both sweeps, then both files again for the rows to compare -- at
    // 150M rows as long as a cold read of both files. Built first, B's index is
    // all the join needs from B, so A's rows can be joined as A's sweep finds
    // them, while their pages are in memory, and A is read once.
    //
    // The join compares a key's first row, and which row is first is only known
    // once the insertion has seen them all. So every row is joined as though it
    // were first, and the rows that turn out to repeat a key -- `later`, kept by
    // the insertion -- are joined again afterwards and subtracted. Counts are
    // sums, so that is exact. The C and C++ ports do the same.
    //
    // Text on both sides only, and not with the normalisation flags:
    // --ignore-case refuses from inside a comparison, and a repeat's refusal
    // could not be taken back. CSVDIFF_FUSED_JOIN=1 takes this path at any size,
    // which is how the tests reach it.
    const fused = !needsNormalising(opt) and a_input == .text and b_input == .text and
        (fused_forced or (total > 1 and
            a_input.text.slab.data.len + b_input.text.slab.data.len > (@as(usize, 8) << 30)));

    var prepare_a = Prepare{
        .gpa = gpa,
        .input = a_input,
        .wanted = wanted.items,
        .key_size = key_size,
        .opt = opt,
        .threads = if (fused) total else per_file,
        .tag = "A ",
    };
    var prepare_b = Prepare{
        .gpa = gpa,
        .input = b_input,
        .wanted = wanted.items,
        .key_size = key_size,
        .opt = opt,
        .threads = if (fused) total else per_file,
        .tag = "B ",
    };
    defer prepare_a.deinit();
    defer prepare_b.deinit();
    if (fused) {
        // B whole, on the whole budget; A only read, since its index is built
        // with the join inside it below.
        prepare_b.run();
        if (prepare_b.failure) |e| return e;
        prepare_a.side = try prepare_a.input.project(gpa, wanted.items, key_size, total);
    } else {
        const reader: ?std.Thread = std.Thread.spawn(.{}, Prepare.run, .{&prepare_b}) catch null;
        if (reader == null) prepare_b.run();
        prepare_a.run();
        if (reader) |w| w.join();
        if (prepare_a.failure) |e| return e;
        if (prepare_b.failure) |e| return e;
    }

    const a = &prepare_a.side.?;
    const b = &prepare_b.side.?;
    const bi = &prepare_b.index.?;

    // Owned copies: the names point into the header, which is released when this
    // function returns, and the result outlives it.
    const columns = try gpa.alloc(ColumnStat, nc);
    var made: usize = 0;
    errdefer {
        for (columns[0..made]) |c| gpa.free(c.name);
        gpa.free(columns);
    }
    for (compared.items, 0..) |name, i| {
        columns[i] = .{ .name = try gpa.dupe(u8, name) };
        made += 1;
    }

    // A's keys, chunked into one queue every thread pulls from. B has no pass of
    // its own any more: see `counts.added` below.
    // Fused, A's keys are not counted yet: one part a sweep chunk.
    const a_ways = if (fused) total else waysFor(prepare_a.index.?.first_row.items.len);
    const parts = try gpa.alloc(Part, a_ways);
    // One release, of the parts that were built, on every path. This was a
    // `defer` over all of them beside an `errdefer` over the built ones, so any
    // error from here on freed each part's columns twice -- and an allocation
    // failing part way freed the unbuilt ones, which were never set. It stayed
    // hidden while no error came after this point: the allocator the release
    // build uses on Linux let the second free pass, and macOS's trapped (exit
    // 133) once --ignore-case began refusing in the join rather than before it.
    var built: usize = 0;
    defer {
        for (parts[0..built]) |p| gpa.free(p.columns);
        gpa.free(parts);
    }
    for (parts) |*p| {
        const stats = try gpa.alloc(ColumnStat, nc);
        for (stats, compared.items) |*stat, name| stat.* = .{ .name = name };
        p.* = .{ .columns = stats };
        built += 1;
    }

    var join = Join{
        .a = a,
        .b = b,
        .ai = undefined, // set once A's index exists: see below
        .bi = bi,
        .opt = opt,
        .key_size = key_size,
        .nc = nc,
        .width = width,
        .parts = parts,
        .span_tail = if (a.parser()) |pa| (if (b.parser()) |pb| text.sharedTail(pa, pb) else null) else null,
        .json_proof = nc > 0 and a.slab.dialect == .json and b.slab.dialect == .json,
        .next = std.atomic.Value(usize).init(0),
    };
    if (join.span_tail != null) join.runs = try text.Runs.plan(gpa, a.parser().?);
    defer if (join.runs) |r| r.deinit(gpa);
    var phases = Phases.start("");
    // Fused, the rows that repeat a key, joined again to be subtracted.
    var back: []Part = &.{};
    var back_built: usize = 0;
    defer {
        for (back[0..back_built]) |p| gpa.free(p.columns);
        if (back.len > 0) gpa.free(back);
    }
    if (fused) {
        var sink = try FusedSink.init(gpa, &join, parts);
        defer sink.deinit(gpa);
        prepare_a.index = try RowIndex.build(gpa, a, key_size, opt, total, "A ", &sink);
        join.ai = &prepare_a.index.?;
        phases.mark("A index, joined as it was read");
        const later = join.ai.later.items;
        back = try gpa.alloc(Part, if (later.len < (1 << 14)) 1 else total);
        for (back) |*p| {
            const stats = try gpa.alloc(ColumnStat, nc);
            for (stats, compared.items) |*stat, name| stat.* = .{ .name = name };
            p.* = .{ .columns = stats };
            back_built += 1;
        }
        var take = TakeBack{ .join = &join, .rows = later, .parts = back, .next = std.atomic.Value(usize).init(0) };
        try runOnThreads(&take, TakeBack.run, back.len);
        phases.mark("repeated keys taken back");
    } else {
        join.ai = &prepare_a.index.?;
        try runOnThreads(&join, Join.run, total);
        phases.mark("join chunks (par)");
    }
    const ai = join.ai;
    for (parts) |part| {
        if (part.failure) |e| return e;
    }
    for (back) |part| {
        if (part.failure) |e| return e;
    }

    var counts = Counts{};
    for (parts) |part| {
        counts.matched += part.matched;
        counts.changed += part.changed;
        counts.removed += part.removed;
        for (columns, part.columns) |*into, from| {
            into.changed += from.changed;
            into.blanked += from.blanked;
            into.filled += from.filled;
        }
    }
    for (back) |part| {
        counts.matched -= part.matched;
        counts.changed -= part.changed;
        counts.removed -= part.removed;
        for (columns, part.columns) |*into, from| {
            into.changed -= from.changed;
            into.blanked -= from.blanked;
            into.filled -= from.filled;
        }
    }

    counts.a_rows = ai.rows;
    counts.b_rows = bi.rows;
    counts.a_keys = ai.uniqueKeys();
    counts.b_keys = bi.uniqueKeys();
    // `added` is arithmetic, not a pass.
    //
    // B's half of the join used to ask "is this key in A?" for every key in B --
    // ten million probes to find the ten thousand rows A never had -- while A's
    // half had already asked the same question of the same pairs from the other
    // end. It does not need asking twice: every distinct key of A finds at most
    // one distinct key of B, two distinct keys of A cannot find the same key of
    // B, and the key comparison is symmetric, so the keys of B that nothing
    // matched are simply the ones the A pass did not account for.
    //
    // This port can stop there because it reports counts and column stats and
    // never names an added row. The Rust port renders those rows, so it keeps a
    // bitmap of B's matched rows instead and walks that for the sample; the C++
    // port takes this same subtraction and runs a pass only for `--json`.
    counts.added = counts.b_keys - counts.matched;
    counts.unchanged = counts.matched - counts.changed;
    counts.a_dup_keys = ai.dup_keys;
    counts.a_dup_rows = ai.dup_rows;
    counts.b_dup_keys = bi.dup_keys;
    counts.b_dup_rows = bi.dup_rows;

    return .{ .counts = counts, .columns = columns };
}
