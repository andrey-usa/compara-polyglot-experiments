//! Reading Parquet into rows, for a comparison whose other side is text.
//!
//! `pqdiff.zig` is the fast path and the one to reach for when both sides are
//! Parquet: it never builds a row at all, joining on key columns and then
//! comparing whole columns as integers. It cannot help when only one side is
//! columnar, which is the case this module exists for -- a CSV export against
//! the Parquet a warehouse emits. Here the pages are decoded into rows, so the
//! text engine can join the two sides on the same field representation.
//!
//! Everything else this engine reads is a row of contiguous bytes, so a field is
//! an offset and a length and nothing is copied. Parquet is columnar and has no
//! row at all: the values of one row live in as many column chunks as there are
//! columns, page-compressed, and often as indices into a dictionary. So the pages
//! are decoded once, into an arena, and a field becomes an offset and a length
//! *into that arena* -- the same word the CSV and JSON readers produce, and the
//! same join over it.
//!
//! The one thing worth doing well is the dictionary. Where a column is
//! dictionary-encoded -- which is most string columns in most files -- the
//! dictionary is decoded once and every row's field points *at the dictionary
//! entry*, so a column of ten million rows over a few thousand distinct values
//! costs eight bytes a row and nothing more. That is what makes this reader
//! smaller in memory than reading the equivalent CSV rather than larger.
//!
//! Columns are decoded in parallel, since they share nothing until they are
//! stitched into rows. Every byte of it comes from the caller's allocator, so a
//! Parquet comparison is bounded by `--max-memory` exactly as a CSV one is.
//!
//! **How a value becomes text.** This tool compares text -- `1.0` and `1` are
//! different unless a tolerance is set -- so a typed Parquet value has to be
//! rendered, and the rule is fixed rather than clever: byte arrays are their
//! bytes, booleans are `true` and `false`, integers and decimals are written out
//! in full, floats take their shortest round-trip form laid out as JavaScript
//! lays it out, dates are `YYYY-MM-DD` and timestamps ISO 8601. The Rust port
//! implements the same rule, so the two agree cell for cell.

const std = @import("std");
const codec = @import("codec.zig");
const encoding = @import("encoding.zig");
const f = @import("field.zig");
const slab_mod = @import("slab.zig");
const thrift = @import("thrift.zig");

const Field = f.Field;
const Slab = slab_mod.Slab;

pub const Error = error{
    NotParquet,
    TruncatedParquetFile,
    BadFooter,
    NestedSchema,
    RepeatedColumn,
    NoColumns,
    RowCountMismatch,
    UnsupportedEncoding,
    UnsupportedType,
    ExternalColumn,
    TruncatedPage,
    DictionaryMissing,
    DictionaryIndexOutOfRange,
};

const MAGIC = "PAR1";

/// Whether a file is Parquet, by its magic rather than by its name. A file called
/// `.parquet` that is really a CSV should be read as a CSV, and one called
/// anything else that is really Parquet should still work.
pub fn looksLikeParquet(data: []const u8) bool {
    return data.len >= 12 and
        std.mem.eql(u8, data[0..4], MAGIC) and
        std.mem.eql(u8, data[data.len - 4 ..], MAGIC);
}

// ---------------------------------------------------------------------------
// What the footer says
// ---------------------------------------------------------------------------

const Physical = enum {
    boolean,
    int32,
    int64,
    int96,
    float,
    double,
    byte_array,
    fixed_len,

    fn fromId(id: i32) !Physical {
        return switch (id) {
            0 => .boolean,
            1 => .int32,
            2 => .int64,
            3 => .int96,
            4 => .float,
            5 => .double,
            6 => .byte_array,
            7 => .fixed_len,
            else => Error.UnsupportedType,
        };
    }
};

const Unit = enum { millis, micros, nanos };

/// How a value of this column is written out as text.
const Render = union(enum) {
    text,
    boolean,
    int,
    float32,
    float64,
    decimal: i32,
    date,
    time: Unit,
    timestamp: struct { unit: Unit, utc: bool },
};

const ColumnInfo = struct {
    name: []const u8,
    physical: Physical,
    render: Render,
    /// Bytes per value for FIXED_LEN_BYTE_ARRAY.
    type_length: usize,
    /// One when the column is optional, so a definition level below it is a null.
    max_def_level: u8,
};

const Chunk = struct {
    codec: codec.Codec,
    num_values: i64,
    data_page_offset: u64,
    dictionary_page_offset: ?u64,
    total_compressed_size: i64,
    /// What the values weigh once decoded, which is what the arena has to hold.
    /// Equal to the compressed size only for an uncompressed file -- which is
    /// why sizing the arena from that one went unnoticed until a codec was read.
    total_uncompressed_size: i64,
};

const RowGroup = struct {
    num_rows: usize,
    chunks: []Chunk,
};

pub const Projection = struct {
    /// Row-major: `fields[row * width + slot]`.
    fields: []Field,
    arena: []u8,
};

pub const Reader = struct {
    gpa: std.mem.Allocator,
    slab: Slab,
    columns: []ColumnInfo,
    row_groups: []RowGroup,
    rows: usize,

    pub fn open(gpa: std.mem.Allocator, io: std.Io, path: []const u8) !Reader {
        var slab = try Slab.map(io, path);
        errdefer slab.close();
        const data = slab.data;
        if (!looksLikeParquet(data)) return Error.NotParquet;
        const end = data.len;
        const footer_len: usize = std.mem.readInt(u32, data[end - 8 ..][0..4], .little);
        if (footer_len + 8 > end) return Error.BadFooter;
        const meta = try readMetadata(gpa, data[end - 8 - footer_len .. end - 8]);
        return .{
            .gpa = gpa,
            .slab = slab,
            .columns = meta.columns,
            .row_groups = meta.row_groups,
            .rows = meta.rows,
        };
    }

    pub fn deinit(self: *Reader) void {
        for (self.columns) |c| self.gpa.free(c.name);
        self.gpa.free(self.columns);
        for (self.row_groups) |g| self.gpa.free(g.chunks);
        self.gpa.free(self.row_groups);
        self.slab.close();
    }

    /// The column names, in schema order: this file's header. The slices belong
    /// to the reader, so a caller that outlives it copies them.
    pub fn columnNames(self: Reader, gpa: std.mem.Allocator) ![][]const u8 {
        const out = try gpa.alloc([]const u8, self.columns.len);
        var made: usize = 0;
        errdefer {
            for (out[0..made]) |n| gpa.free(n);
            gpa.free(out);
        }
        for (self.columns, 0..) |c, i| {
            out[i] = try gpa.dupe(u8, c.name);
            made += 1;
        }
        return out;
    }

    fn leafFor(self: Reader, name: []const u8) ?usize {
        for (self.columns, 0..) |c, i| {
            if (std.mem.eql(u8, c.name, name)) return i;
        }
        return null;
    }

    /// Decodes the wanted columns into row-major field words and the arena they
    /// point into. `wanted[i]` is the name whose values belong in slot `i`; a name
    /// this file does not have leaves that slot absent in every row.
    ///
    /// Columns are decoded a wave at a time — as many at once as there are
    /// threads — and each wave is folded into the output before the next starts.
    /// Decoding all of them first and stitching afterwards holds two copies of
    /// every field word at the peak, which at ten million rows over nineteen
    /// columns is 1.5 GB per file for nothing.
    pub fn project(
        self: *Reader,
        gpa: std.mem.Allocator,
        wanted: []const ?[]const u8,
        threads: usize,
    ) !Projection {
        const width = wanted.len;
        var jobs: std.ArrayList(Job) = .empty;
        defer jobs.deinit(gpa);
        for (wanted, 0..) |name, slot| {
            const leaf = if (name) |n| self.leafFor(n) else null;
            if (leaf) |at| try jobs.append(gpa, .{ .slot = slot, .leaf = at });
        }

        const fields = try gpa.alloc(Field, self.rows * width);
        errdefer gpa.free(fields);
        @memset(fields, f.ABSENT);
        var arena: std.ArrayList(u8) = .empty;
        errdefer arena.deinit(gpa);

        const ways = @max(1, @min(threads, jobs.items.len));
        var wave = Wave{ .reader = self, .gpa = gpa };
        var at: usize = 0;
        while (at < jobs.items.len) {
            const batch = jobs.items[at..@min(at + ways, jobs.items.len)];
            at += batch.len;
            try wave.decode(batch);
            defer wave.release();
            if (wave.failure) |e| return e;

            for (batch, wave.done[0..batch.len]) |job, maybe| {
                const column = maybe orelse continue;
                // The offset is the low bits of the word, so moving a column's
                // bytes into the file's one arena is an addition — and only the
                // two sentinels have to be left alone.
                const base = arena.items.len;
                try arena.appendSlice(gpa, column.arena);
                // Written a block of rows at a time: the destination is
                // row-major, so a whole column in one pass would touch every
                // cache line of the output once per column.
                const BLOCK = 1024;
                var from: usize = 0;
                while (from < self.rows) : (from += BLOCK) {
                    const to = @min(from + BLOCK, self.rows);
                    for (from..to) |row| {
                        fields[row * width + job.slot] = f.shift(column.fields[row], base);
                    }
                }
            }
        }
        return .{ .fields = fields, .arena = try arena.toOwnedSlice(gpa) };
    }
};

/// One wave of columns, decoded at once. A thread per column beyond the first,
/// each writing only its own slot of `done` and `failures`, which is why neither
/// needs a lock: they are read after every thread has been joined.
const Wave = struct {
    reader: *Reader,
    gpa: std.mem.Allocator,
    done: [MAX]?Decoded = @splat(null),
    failures: [MAX]?anyerror = @splat(null),
    jobs: []const Job = &.{},
    failure: ?anyerror = null,

    const MAX = 64;

    fn one(self: *Wave, i: usize) void {
        self.done[i] = decodeColumn(self.reader, self.gpa, self.jobs[i].leaf) catch |e| {
            self.failures[i] = e;
            return;
        };
    }

    fn decode(self: *Wave, jobs: []const Job) !void {
        self.jobs = jobs;
        self.failure = null;
        var workers: [MAX]?std.Thread = @splat(null);
        const spawned = @min(jobs.len - 1, MAX - 1);
        for (0..spawned) |i| {
            workers[i] = std.Thread.spawn(.{}, Wave.one, .{ self, i + 1 }) catch null;
        }
        self.one(0);
        for (0..spawned) |i| {
            if (workers[i]) |w| w.join() else self.one(i + 1);
        }
        for (self.failures[0..jobs.len]) |maybe| {
            if (maybe) |e| self.failure = e;
        }
    }

    fn release(self: *Wave) void {
        for (&self.done) |*maybe| {
            if (maybe.*) |d| d.deinit(self.gpa);
            maybe.* = null;
        }
        @memset(&self.failures, null);
    }
};

const Job = struct { slot: usize, leaf: usize };

/// One column's values: a field per row, and the bytes they point into.
const Decoded = struct {
    fields: []Field,
    arena: []u8,

    fn deinit(self: Decoded, gpa: std.mem.Allocator) void {
        gpa.free(self.fields);
        gpa.free(self.arena);
    }
};


// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

const Metadata = struct {
    columns: []ColumnInfo,
    row_groups: []RowGroup,
    rows: usize,
};

const SchemaElement = struct {
    physical: ?i32 = null,
    type_length: i32 = 0,
    repetition: i32 = 0,
    name: []const u8 = "",
    num_children: i32 = 0,
    converted: ?i32 = null,
    scale: i32 = 0,
    logical: ?Render = null,
};

fn readMetadata(gpa: std.mem.Allocator, data: []const u8) !Metadata {
    var r = thrift.Reader.init(data);
    var schema: std.ArrayList(SchemaElement) = .empty;
    defer schema.deinit(gpa);
    var groups: std.ArrayList(RowGroup) = .empty;
    errdefer {
        for (groups.items) |g| gpa.free(g.chunks);
        groups.deinit(gpa);
    }
    var rows: usize = 0;

    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            2 => {
                const n, const kind = try r.listBegin();
                for (0..n) |_| {
                    if (kind != thrift.STRUCT) {
                        try r.skip(kind);
                        continue;
                    }
                    try schema.append(gpa, try readSchemaElement(&r));
                }
            },
            3 => rows = @intCast(@max(0, try r.i64v())),
            4 => {
                const n, const kind = try r.listBegin();
                for (0..n) |_| {
                    if (kind != thrift.STRUCT) {
                        try r.skip(kind);
                        continue;
                    }
                    try groups.append(gpa, try readRowGroup(gpa, &r));
                }
            },
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();

    const columns = try leafColumns(gpa, schema.items);
    errdefer {
        for (columns) |c| gpa.free(c.name);
        gpa.free(columns);
    }
    var counted: usize = 0;
    for (groups.items) |g| {
        if (g.chunks.len != columns.len) return Error.RowCountMismatch;
        counted += g.num_rows;
    }
    // The footer's own row count is the one every reader trusts; a disagreement
    // means the file is not what it says it is.
    if (counted != rows) return Error.RowCountMismatch;
    return .{
        .columns = columns,
        .row_groups = try groups.toOwnedSlice(gpa),
        .rows = rows,
    };
}

fn readSchemaElement(r: *thrift.Reader) !SchemaElement {
    var out = SchemaElement{};
    var decimal_scale: ?i32 = null;
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            1 => out.physical = try r.i32v(),
            2 => out.type_length = try r.i32v(),
            3 => out.repetition = try r.i32v(),
            4 => out.name = try r.binary(),
            5 => out.num_children = try r.i32v(),
            6 => out.converted = try r.i32v(),
            7 => decimal_scale = try r.i32v(),
            10 => out.logical = try readLogicalType(r),
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    out.scale = decimal_scale orelse 0;
    // A DECIMAL written the old way carries its scale in the schema element
    // rather than in the logical type.
    if (out.logical == null and out.converted == 5) out.logical = .{ .decimal = out.scale };
    return out;
}

/// The logical type union, of which this reader needs the ones that change how a
/// value reads as text.
fn readLogicalType(r: *thrift.Reader) !?Render {
    var out: ?Render = null;
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            // STRING, JSON, BSON, UUID: all of them are their bytes.
            1, 12, 13, 14 => {
                try r.skip(fh.kind);
                out = .text;
            },
            5 => {
                var scale: i32 = 0;
                r.structBegin();
                while (try r.field()) |g| {
                    if (g.id == 1) scale = try r.i32v() else try r.skip(g.kind);
                }
                r.structEnd();
                out = .{ .decimal = scale };
            },
            6 => {
                try r.skip(fh.kind);
                out = .date;
            },
            7 => out = .{ .time = (try readTimeType(r)).unit },
            8 => {
                const t = try readTimeType(r);
                out = .{ .timestamp = .{ .unit = t.unit, .utc = t.utc } };
            },
            10 => {
                try r.skip(fh.kind);
                out = .int;
            },
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    return out;
}

/// The `isAdjustedToUTC` flag and the unit shared by TIME and TIMESTAMP.
fn readTimeType(r: *thrift.Reader) !struct { utc: bool, unit: Unit } {
    var utc = false;
    var unit: Unit = .millis;
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            // A compact-protocol bool is in the field header itself.
            1 => {
                if (fh.kind == thrift.TRUE) {
                    utc = true;
                } else if (fh.kind == thrift.FALSE) {
                    utc = false;
                } else {
                    utc = (try r.i32v()) != 0;
                }
            },
            2 => {
                r.structBegin();
                while (try r.field()) |g| {
                    unit = switch (g.id) {
                        1 => .millis,
                        2 => .micros,
                        else => .nanos,
                    };
                    try r.skip(g.kind);
                }
                r.structEnd();
            },
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    return .{ .utc = utc, .unit = unit };
}

/// The schema's leaves, refusing anything that is not a flat table.
fn leafColumns(gpa: std.mem.Allocator, schema: []const SchemaElement) ![]ColumnInfo {
    if (schema.len == 0) return Error.NoColumns;
    var out: std.ArrayList(ColumnInfo) = .empty;
    errdefer {
        for (out.items) |c| gpa.free(c.name);
        out.deinit(gpa);
    }
    for (schema[1..]) |element| {
        if (element.num_children > 0) return Error.NestedSchema;
        if (element.repetition == 2) return Error.RepeatedColumn;
        const physical = try Physical.fromId(element.physical orelse -1);
        try out.append(gpa, .{
            .name = try gpa.dupe(u8, element.name),
            .physical = physical,
            .render = renderFor(physical, element),
            .type_length = @intCast(@max(0, element.type_length)),
            .max_def_level = if (element.repetition == 1) 1 else 0,
        });
    }
    if (out.items.len == 0) return Error.NoColumns;
    return out.toOwnedSlice(gpa);
}

/// What a value of this column reads as: from its logical type where it has one,
/// and from the physical type where it does not.
fn renderFor(physical: Physical, element: SchemaElement) Render {
    if (element.logical) |render| {
        if (render != .int or physical != .byte_array) return render;
    }
    if (element.converted) |converted| {
        switch (converted) {
            0, 4, 19, 20 => return .text, // UTF8, ENUM, JSON, BSON
            6 => return .date,
            7 => return .{ .time = .millis },
            8 => return .{ .time = .micros },
            9 => return .{ .timestamp = .{ .unit = .millis, .utc = true } },
            10 => return .{ .timestamp = .{ .unit = .micros, .utc = true } },
            11, 12, 13, 14 => return .int,
            else => {},
        }
    }
    return switch (physical) {
        .boolean => .boolean,
        .int32, .int64 => .int,
        .int96 => .{ .timestamp = .{ .unit = .nanos, .utc = false } },
        .float => .float32,
        .double => .float64,
        .byte_array, .fixed_len => .text,
    };
}

fn readRowGroup(gpa: std.mem.Allocator, r: *thrift.Reader) !RowGroup {
    var chunks: std.ArrayList(Chunk) = .empty;
    errdefer chunks.deinit(gpa);
    var num_rows: usize = 0;
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            1 => {
                const n, const kind = try r.listBegin();
                for (0..n) |_| {
                    if (kind != thrift.STRUCT) {
                        try r.skip(kind);
                        continue;
                    }
                    try chunks.append(gpa, try readColumnChunk(r));
                }
            },
            3 => num_rows = @intCast(@max(0, try r.i64v())),
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    return .{ .num_rows = num_rows, .chunks = try chunks.toOwnedSlice(gpa) };
}

fn readColumnChunk(r: *thrift.Reader) !Chunk {
    var chunk = Chunk{
        .codec = .none,
        .num_values = 0,
        .data_page_offset = 0,
        .dictionary_page_offset = null,
        .total_compressed_size = 0,
        .total_uncompressed_size = 0,
    };
    var external = false;
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            1 => external = (try r.binary()).len > 0,
            3 => {
                r.structBegin();
                while (try r.field()) |g| {
                    switch (g.id) {
                        4 => chunk.codec = try codec.Codec.fromId(try r.i32v()),
                        5 => chunk.num_values = try r.i64v(),
                        6 => chunk.total_uncompressed_size = try r.i64v(),
                        7 => chunk.total_compressed_size = try r.i64v(),
                        9 => chunk.data_page_offset = @intCast(@max(0, try r.i64v())),
                        11 => {
                            const at = try r.i64v();
                            chunk.dictionary_page_offset = if (at > 0) @intCast(at) else null;
                        },
                        else => try r.skip(g.kind),
                    }
                }
                r.structEnd();
            },
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    if (external) return Error.ExternalColumn;
    return chunk;
}

// ---------------------------------------------------------------------------
// The pages
// ---------------------------------------------------------------------------

const PageHeader = struct {
    kind: i32 = 0,
    uncompressed: usize = 0,
    compressed: usize = 0,
    num_values: usize = 0,
    encoding: i32 = 0,
    /// Version 2 keeps its levels outside the compressed part.
    v2_def_bytes: usize = 0,
    v2_rep_bytes: usize = 0,
    v2_compressed: bool = true,
    body_at: usize = 0,
};

fn readPageHeader(data: []const u8, at: usize) !PageHeader {
    var r = thrift.Reader.init(data[at..]);
    var h = PageHeader{};
    r.structBegin();
    while (try r.field()) |fh| {
        switch (fh.id) {
            1 => h.kind = try r.i32v(),
            2 => h.uncompressed = @intCast(@max(0, try r.i32v())),
            3 => h.compressed = @intCast(@max(0, try r.i32v())),
            // Data page v1 and dictionary page: both carry a count and an
            // encoding in the same two field ids.
            5, 7 => {
                r.structBegin();
                while (try r.field()) |g| {
                    switch (g.id) {
                        1 => h.num_values = @intCast(@max(0, try r.i32v())),
                        2 => h.encoding = try r.i32v(),
                        else => try r.skip(g.kind),
                    }
                }
                r.structEnd();
            },
            8 => {
                r.structBegin();
                while (try r.field()) |g| {
                    switch (g.id) {
                        1 => h.num_values = @intCast(@max(0, try r.i32v())),
                        4 => h.encoding = try r.i32v(),
                        5 => h.v2_def_bytes = @intCast(@max(0, try r.i32v())),
                        6 => h.v2_rep_bytes = @intCast(@max(0, try r.i32v())),
                        7 => {
                            if (g.kind == thrift.TRUE) {
                                h.v2_compressed = true;
                            } else if (g.kind == thrift.FALSE) {
                                h.v2_compressed = false;
                            } else {
                                h.v2_compressed = (try r.i32v()) != 0;
                            }
                        },
                        else => try r.skip(g.kind),
                    }
                }
                r.structEnd();
            },
            else => try r.skip(fh.kind),
        }
    }
    r.structEnd();
    h.body_at = at + r.position();
    return h;
}

/// Everything one column's decode needs to hand around: where the values go, and
/// the scratch it formats them through.
const Sink = struct {
    gpa: std.mem.Allocator,
    arena: std.ArrayList(u8) = .empty,
    fields: std.ArrayList(Field) = .empty,
    scratch: std.ArrayList(u8) = .empty,

    fn deinit(self: *Sink) void {
        self.arena.deinit(self.gpa);
        self.fields.deinit(self.gpa);
        self.scratch.deinit(self.gpa);
    }

    fn push(self: *Sink, bytes: []const u8) !Field {
        const at = self.arena.items.len;
        try self.arena.appendSlice(self.gpa, bytes);
        return f.pack(at, bytes.len, false);
    }

    fn pushScratch(self: *Sink) !Field {
        return self.push(self.scratch.items);
    }
};

/// Decodes one column across every row group into a field per row.
fn decodeColumn(reader: *Reader, gpa: std.mem.Allocator, leaf: usize) !Decoded {
    const info = reader.columns[leaf];
    const data = reader.slab.data;
    var sink = Sink{ .gpa = gpa };
    errdefer sink.deinit();
    try sink.fields.ensureTotalCapacity(gpa, reader.rows);
    // The arena is sized up front from what the column chunks say they hold.
    // Growing it by doubling would be the largest waste in the run under a
    // FixedBufferAllocator, which cannot reuse what it has handed back.
    //
    // It holds decoded values, so the estimate is the *uncompressed* size. It
    // used to be the compressed one, which is the same number until a codec is
    // in play and then short by the compression ratio -- 17x on this
    // repository's zstd fixture, which is enough doublings to cost more than
    // the decompression it was hiding behind.
    var expected: usize = 0;
    for (reader.row_groups) |group| {
        const chunk = group.chunks[leaf];
        expected += @intCast(@max(
            chunk.total_compressed_size,
            chunk.total_uncompressed_size,
        ));
    }
    try sink.arena.ensureTotalCapacity(gpa, expected);

    var dictionary: std.ArrayList(Field) = .empty;
    defer dictionary.deinit(gpa);

    // One window for every page of this column, released with it. Per page it
    // was 8.13 MB of budget that never came back -- see `codec.Scratch`.
    var scratch = codec.Scratch{ .gpa = gpa };
    defer scratch.deinit();

    for (reader.row_groups) |group| {
        const chunk = group.chunks[leaf];
        const dict_at = chunk.dictionary_page_offset orelse chunk.data_page_offset;
        const start: usize = @intCast(@min(dict_at, chunk.data_page_offset));
        const end = @min(start + @as(usize, @intCast(@max(0, chunk.total_compressed_size))), data.len);
        var at = start;
        // A dictionary belongs to one column chunk, so it is decoded once per row
        // group and every row of that group points into it.
        dictionary.clearRetainingCapacity();
        var seen: i64 = 0;
        const group_from = sink.fields.items.len;

        while (at < end and seen < chunk.num_values) {
            const header = try readPageHeader(data, at);
            if (header.body_at + header.compressed > data.len) return Error.TruncatedPage;
            const body = data[header.body_at..][0..header.compressed];
            at = header.body_at + header.compressed;

            switch (header.kind) {
                2 => {
                    // The dictionary's values go into the arena once; every row
                    // of this row group then points at one of them.
                    const page = try codec.decompress(gpa, &scratch, chunk.codec, body, header.uncompressed);
                    defer gpa.free(page);
                    dictionary.clearRetainingCapacity();
                    try decodeValues(&sink, &dictionary, page, 0, header.num_values, info, &.{});
                },
                0, 3 => {
                    seen += @intCast(header.num_values);
                    try decodeDataPage(&sink, &scratch, &header, body, chunk, info, dictionary.items);
                },
                // An index page carries no values; anything else is a page type
                // that did not exist when this was written.
                else => {},
            }
        }
        if (sink.fields.items.len - group_from != group.num_rows) return Error.RowCountMismatch;
    }

    // The scratch buffer is the sink's own working space and does not leave with
    // the values, so it is released here rather than travelling with them.
    sink.scratch.deinit(gpa);
    return .{
        .fields = try sink.fields.toOwnedSlice(gpa),
        .arena = try sink.arena.toOwnedSlice(gpa),
    };
}

fn decodeDataPage(
    sink: *Sink,
    scratch: *codec.Scratch,
    header: *const PageHeader,
    body: []const u8,
    chunk: Chunk,
    info: ColumnInfo,
    dictionary: []const Field,
) !void {
    const gpa = sink.gpa;
    // Version 2 keeps the levels outside the compressed part, so only the values
    // are decompressed; version 1 compresses the whole page.
    var levels: []const u8 = &.{};
    var values: []const u8 = &.{};
    var owned: ?[]u8 = null;
    defer if (owned) |o| gpa.free(o);

    if (header.kind == 3) {
        const level_bytes = header.v2_def_bytes + header.v2_rep_bytes;
        if (level_bytes > body.len) return Error.TruncatedPage;
        levels = body[header.v2_rep_bytes..level_bytes];
        const rest = body[level_bytes..];
        if (header.v2_compressed and chunk.codec != .none) {
            const page = try codec.decompress(gpa, scratch, chunk.codec, rest, header.uncompressed - level_bytes);
            owned = page;
            values = page;
        } else {
            values = rest;
        }
    } else {
        const page = try codec.decompress(gpa, scratch, chunk.codec, body, header.uncompressed);
        owned = page;
        if (info.max_def_level == 0) {
            values = page;
        } else {
            // Version 1 prefixes its level data with a four-byte length.
            if (page.len < 4) return Error.TruncatedPage;
            const n: usize = std.mem.readInt(u32, page[0..4], .little);
            if (4 + n > page.len) return Error.TruncatedPage;
            levels = page[4 .. 4 + n];
            values = page[4 + n ..];
        }
    }

    // Which rows have a value at all, and how many values the page holds.
    const present = try gpa.alloc(bool, header.num_values);
    defer gpa.free(present);
    var wanted = header.num_values;
    if (info.max_def_level > 0) {
        var hybrid = encoding.Hybrid.init(gpa, levels, encoding.bitWidth(info.max_def_level));
        defer hybrid.deinit();
        wanted = 0;
        for (present) |*slot| {
            slot.* = (try hybrid.next()) >= info.max_def_level;
            if (slot.*) wanted += 1;
        }
    } else {
        @memset(present, true);
    }

    var decoded: std.ArrayList(Field) = .empty;
    defer decoded.deinit(gpa);
    try decodeValues(sink, &decoded, values, header.encoding, wanted, info, dictionary);
    if (decoded.items.len < wanted) return Error.TruncatedPage;

    var taken: usize = 0;
    for (present) |slot| {
        if (slot) {
            try sink.fields.append(gpa, decoded.items[taken]);
            taken += 1;
        } else {
            try sink.fields.append(gpa, f.ABSENT);
        }
    }
}

/// One page's values, in whichever encoding it was written in. Appends to `out`,
/// pushing any bytes it has to build into the sink's arena.
fn decodeValues(
    sink: *Sink,
    out: *std.ArrayList(Field),
    data: []const u8,
    page_encoding: i32,
    count: usize,
    info: ColumnInfo,
    dictionary: []const Field,
) !void {
    const gpa = sink.gpa;
    switch (page_encoding) {
        0 => try plain(sink, out, data, count, info),
        2, 8 => {
            // Dictionary indices: a bit width, then the hybrid encoding.
            if (dictionary.len == 0 and count > 0) return Error.DictionaryMissing;
            if (data.len == 0) return Error.TruncatedPage;
            var hybrid = encoding.Hybrid.init(gpa, data[1..], data[0]);
            defer hybrid.deinit();
            for (0..count) |_| {
                const at = try hybrid.next();
                if (at >= dictionary.len) return Error.DictionaryIndexOutOfRange;
                try out.append(gpa, dictionary[at]);
            }
        },
        3 => {
            // RLE, which in a data page means booleans.
            const from: usize = @min(4, data.len);
            var hybrid = encoding.Hybrid.init(gpa, data[from..], 1);
            defer hybrid.deinit();
            for (0..count) |_| {
                const v = (try hybrid.next()) != 0;
                try out.append(gpa, try sink.push(if (v) "true" else "false"));
            }
        },
        5 => {
            const delta = try encoding.deltaBinaryPacked(gpa, data);
            defer gpa.free(delta.values);
            for (delta.values[0..@min(count, delta.values.len)]) |v| {
                try out.append(gpa, try pushNumber(sink, v, info));
            }
        },
        6 => {
            const lengths = try encoding.deltaBinaryPacked(gpa, data);
            defer gpa.free(lengths.values);
            var at = lengths.used;
            for (lengths.values[0..@min(count, lengths.values.len)]) |len_signed| {
                const len: usize = @intCast(@max(0, len_signed));
                if (at + len > data.len) return Error.TruncatedPage;
                try out.append(gpa, try sink.push(data[at..][0..len]));
                at += len;
            }
        },
        7 => {
            // Prefix lengths, suffix lengths, then the suffixes: each value shares
            // a prefix with the one before it.
            const prefixes = try encoding.deltaBinaryPacked(gpa, data);
            defer gpa.free(prefixes.values);
            const suffixes = try encoding.deltaBinaryPacked(gpa, data[prefixes.used..]);
            defer gpa.free(suffixes.values);
            var at = prefixes.used + suffixes.used;
            var previous: std.ArrayList(u8) = .empty;
            defer previous.deinit(gpa);
            const n = @min(count, @min(prefixes.values.len, suffixes.values.len));
            for (0..n) |i| {
                const prefix: usize = @intCast(@max(0, prefixes.values[i]));
                const len: usize = @intCast(@max(0, suffixes.values[i]));
                if (at + len > data.len or prefix > previous.items.len) return Error.TruncatedPage;
                previous.shrinkRetainingCapacity(prefix);
                try previous.appendSlice(gpa, data[at..][0..len]);
                at += len;
                try out.append(gpa, try sink.push(previous.items));
            }
        },
        else => return Error.UnsupportedEncoding,
    }
}

/// Plain-encoded values: the physical layout, one after another.
fn plain(sink: *Sink, out: *std.ArrayList(Field), data: []const u8, count: usize, info: ColumnInfo) !void {
    const gpa = sink.gpa;
    var at: usize = 0;
    for (0..count) |i| {
        switch (info.physical) {
            .boolean => {
                if (i / 8 >= data.len) return Error.TruncatedPage;
                const bit = (data[i / 8] >> @intCast(i % 8)) & 1;
                try out.append(gpa, try sink.push(if (bit == 1) "true" else "false"));
            },
            .byte_array => {
                if (at + 4 > data.len) return Error.TruncatedPage;
                const len: usize = std.mem.readInt(u32, data[at..][0..4], .little);
                at += 4;
                if (at + len > data.len) return Error.TruncatedPage;
                try out.append(gpa, try pushScalar(sink, data[at..][0..len], info));
                at += len;
            },
            .fixed_len => {
                const len = info.type_length;
                if (at + len > data.len) return Error.TruncatedPage;
                try out.append(gpa, try pushScalar(sink, data[at..][0..len], info));
                at += len;
            },
            .int32 => {
                if (at + 4 > data.len) return Error.TruncatedPage;
                const v = std.mem.readInt(i32, data[at..][0..4], .little);
                at += 4;
                try out.append(gpa, try pushNumber(sink, v, info));
            },
            .int64 => {
                if (at + 8 > data.len) return Error.TruncatedPage;
                const v = std.mem.readInt(i64, data[at..][0..8], .little);
                at += 8;
                try out.append(gpa, try pushNumber(sink, v, info));
            },
            .int96 => {
                // Twelve bytes: nanoseconds within the day, then a Julian day.
                if (at + 12 > data.len) return Error.TruncatedPage;
                const nanos: i128 = std.mem.readInt(u64, data[at..][0..8], .little);
                const julian: i128 = std.mem.readInt(u32, data[at + 8 ..][0..4], .little);
                at += 12;
                const value = (julian - 2_440_588) * 86_400_000_000_000 + nanos;
                sink.scratch.clearRetainingCapacity();
                try writeTimestamp(&sink.scratch, gpa, @intCast(value), .nanos, false);
                try out.append(gpa, try sink.pushScratch());
            },
            .float => {
                if (at + 4 > data.len) return Error.TruncatedPage;
                const v: f32 = @bitCast(std.mem.readInt(u32, data[at..][0..4], .little));
                at += 4;
                sink.scratch.clearRetainingCapacity();
                try writeFloat(&sink.scratch, gpa, v);
                try out.append(gpa, try sink.pushScratch());
            },
            .double => {
                if (at + 8 > data.len) return Error.TruncatedPage;
                const v: f64 = @bitCast(std.mem.readInt(u64, data[at..][0..8], .little));
                at += 8;
                sink.scratch.clearRetainingCapacity();
                try writeFloat(&sink.scratch, gpa, v);
                try out.append(gpa, try sink.pushScratch());
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Values as text
// ---------------------------------------------------------------------------

/// A byte-array or fixed-length value, which is its bytes unless the column says
/// those bytes are a decimal.
fn pushScalar(sink: *Sink, bytes: []const u8, info: ColumnInfo) !Field {
    switch (info.render) {
        .decimal => |scale| {
            var unscaled: i128 = if (bytes.len > 0 and bytes[0] & 0x80 != 0) -1 else 0;
            for (bytes[0..@min(bytes.len, 16)]) |b| unscaled = (unscaled << 8) | b;
            sink.scratch.clearRetainingCapacity();
            try writeDecimal(&sink.scratch, sink.gpa, unscaled, scale);
            return sink.pushScratch();
        },
        else => return sink.push(bytes),
    }
}

/// An integer value, rendered by whatever the column's logical type makes of it.
fn pushNumber(sink: *Sink, value: i64, info: ColumnInfo) !Field {
    const gpa = sink.gpa;
    sink.scratch.clearRetainingCapacity();
    switch (info.render) {
        .decimal => |scale| try writeDecimal(&sink.scratch, gpa, value, scale),
        .date => try writeDate(&sink.scratch, gpa, value),
        .time => |unit| try writeTime(&sink.scratch, gpa, value, unit),
        .timestamp => |t| try writeTimestamp(&sink.scratch, gpa, value, t.unit, t.utc),
        else => try sink.scratch.print(gpa, "{d}", .{value}),
    }
    return sink.pushScratch();
}

/// A float as the shortest digits that read back as the same value, laid out the
/// way JavaScript's number-to-string does it.
///
/// "Shortest round trip" does not say whether `1e-10` is written out in full, and
/// two ports that answer that differently would report a column as changed when
/// it is not. So the layout is stated: plain decimal while the exponent is
/// between -6 and 21, scientific outside it. The Rust port implements the same
/// rule.
fn writeFloat(out: *std.ArrayList(u8), gpa: std.mem.Allocator, value: anytype) !void {
    if (std.math.isNan(value)) return out.appendSlice(gpa, "NaN");
    if (std.math.isInf(value)) {
        return out.appendSlice(gpa, if (value < 0) "-Infinity" else "Infinity");
    }
    if (value == 0) return out.append(gpa, '0');

    // `{e}` gives the shortest digits as `d.ddde±X`, so the value is those digits
    // with the point after position `n`.
    var buf: [64]u8 = undefined;
    const sci = try std.fmt.bufPrint(&buf, "{e}", .{@abs(value)});
    const split = std.mem.indexOfScalar(u8, sci, 'e') orelse sci.len;
    var digits_buf: [32]u8 = undefined;
    var k: usize = 0;
    for (sci[0..split]) |c| {
        if (std.ascii.isDigit(c)) {
            digits_buf[k] = c;
            k += 1;
        }
    }
    while (k > 1 and digits_buf[k - 1] == '0') k -= 1;
    const digits = digits_buf[0..k];
    const exponent: i32 = if (split < sci.len)
        std.fmt.parseInt(i32, sci[split + 1 ..], 10) catch 0
    else
        0;
    const n: i32 = exponent + 1;
    const width: i32 = @intCast(k);

    if (value < 0) try out.append(gpa, '-');
    if (width <= n and n <= 21) {
        try out.appendSlice(gpa, digits);
        try out.appendNTimes(gpa, '0', @intCast(n - width));
    } else if (n > 0 and n <= 21) {
        const point: usize = @intCast(n);
        try out.appendSlice(gpa, digits[0..point]);
        try out.append(gpa, '.');
        try out.appendSlice(gpa, digits[point..]);
    } else if (n > -6 and n <= 0) {
        try out.appendSlice(gpa, "0.");
        try out.appendNTimes(gpa, '0', @intCast(-n));
        try out.appendSlice(gpa, digits);
    } else {
        try out.append(gpa, digits[0]);
        if (k > 1) {
            try out.append(gpa, '.');
            try out.appendSlice(gpa, digits[1..]);
        }
        try out.append(gpa, 'e');
        try out.append(gpa, if (n > 0) '+' else '-');
        try out.print(gpa, "{d}", .{@abs(n - 1)});
    }
}

/// An unscaled integer and a scale, written out in full: `12345` at scale 2 is
/// `123.45`, which is what a CSV of the same column holds.
fn writeDecimal(out: *std.ArrayList(u8), gpa: std.mem.Allocator, unscaled: i128, scale: i32) !void {
    if (scale <= 0) {
        try out.print(gpa, "{d}", .{unscaled});
        try out.appendNTimes(gpa, '0', @intCast(-scale));
        return;
    }
    var buf: [48]u8 = undefined;
    const magnitude: u128 = @intCast(if (unscaled < 0) -unscaled else unscaled);
    const digits = try std.fmt.bufPrint(&buf, "{d}", .{magnitude});
    if (unscaled < 0) try out.append(gpa, '-');
    const places: usize = @intCast(scale);
    if (digits.len > places) {
        try out.appendSlice(gpa, digits[0 .. digits.len - places]);
        try out.append(gpa, '.');
        try out.appendSlice(gpa, digits[digits.len - places ..]);
    } else {
        try out.appendSlice(gpa, "0.");
        try out.appendNTimes(gpa, '0', places - digits.len);
        try out.appendSlice(gpa, digits);
    }
}

/// The civil date `days` after 1970-01-01, by the shift-the-year-to-March
/// algorithm that makes the leap rule a division rather than a table.
fn civilFromDays(days: i64) struct { year: i64, month: u32, day: u32 } {
    const z = days + 719_468;
    const era = @divFloor(z, 146_097);
    const doe = @mod(z, 146_097);
    const yoe = @divTrunc(doe - @divTrunc(doe, 1460) + @divTrunc(doe, 36_524) - @divTrunc(doe, 146_096), 365);
    const year = yoe + era * 400;
    const doy = doe - (365 * yoe + @divTrunc(yoe, 4) - @divTrunc(yoe, 100));
    const mp = @divTrunc(5 * doy + 2, 153);
    const day: u32 = @intCast(doy - @divTrunc(153 * mp + 2, 5) + 1);
    const month: u32 = @intCast(if (mp < 10) mp + 3 else mp - 9);
    return .{ .year = year + @as(i64, if (month <= 2) 1 else 0), .month = month, .day = day };
}

fn two(out: *std.ArrayList(u8), gpa: std.mem.Allocator, value: u32) !void {
    try out.append(gpa, '0' + @as(u8, @intCast(value / 10 % 10)));
    try out.append(gpa, '0' + @as(u8, @intCast(value % 10)));
}

fn writeDate(out: *std.ArrayList(u8), gpa: std.mem.Allocator, days: i64) !void {
    const date = civilFromDays(days);
    // The year is padded by hand: a width on a signed integer writes its sign,
    // and a year before the common era is not what a CSV of this column holds.
    if (date.year < 0) try out.append(gpa, '-');
    var buf: [24]u8 = undefined;
    const year = try std.fmt.bufPrint(&buf, "{d}", .{@abs(date.year)});
    if (year.len < 4) try out.appendNTimes(gpa, '0', 4 - year.len);
    try out.appendSlice(gpa, year);
    try out.append(gpa, '-');
    try two(out, gpa, date.month);
    try out.append(gpa, '-');
    try two(out, gpa, date.day);
}

fn perSecond(unit: Unit) i64 {
    return switch (unit) {
        .millis => 1_000,
        .micros => 1_000_000,
        .nanos => 1_000_000_000,
    };
}

/// The fractional part, with the trailing zeros a whole second would leave
/// dropped: `12:00:00` rather than `12:00:00.000`.
fn writeFraction(out: *std.ArrayList(u8), gpa: std.mem.Allocator, fraction: i64, unit: Unit) !void {
    if (fraction == 0) return;
    const digits: usize = switch (unit) {
        .millis => 3,
        .micros => 6,
        .nanos => 9,
    };
    var buf: [16]u8 = undefined;
    const text = try std.fmt.bufPrint(&buf, "{d}", .{fraction});
    var end = text.len;
    while (end > 0 and text[end - 1] == '0') end -= 1;
    try out.append(gpa, '.');
    try out.appendNTimes(gpa, '0', digits - text.len);
    try out.appendSlice(gpa, text[0..end]);
}

fn writeTime(out: *std.ArrayList(u8), gpa: std.mem.Allocator, value: i64, unit: Unit) !void {
    const scale = perSecond(unit);
    const seconds = @divFloor(value, scale);
    const fraction = @mod(value, scale);
    try two(out, gpa, @intCast(@mod(@divTrunc(seconds, 3600), 24)));
    try out.append(gpa, ':');
    try two(out, gpa, @intCast(@mod(@divTrunc(seconds, 60), 60)));
    try out.append(gpa, ':');
    try two(out, gpa, @intCast(@mod(seconds, 60)));
    try writeFraction(out, gpa, fraction, unit);
}

fn writeTimestamp(
    out: *std.ArrayList(u8),
    gpa: std.mem.Allocator,
    value: i64,
    unit: Unit,
    utc: bool,
) !void {
    const scale = perSecond(unit);
    const seconds = @divFloor(value, scale);
    const fraction = @mod(value, scale);
    try writeDate(out, gpa, @divFloor(seconds, 86_400));
    try out.append(gpa, 'T');
    const time = @mod(seconds, 86_400);
    try two(out, gpa, @intCast(@divTrunc(time, 3600)));
    try out.append(gpa, ':');
    try two(out, gpa, @intCast(@mod(@divTrunc(time, 60), 60)));
    try out.append(gpa, ':');
    try two(out, gpa, @intCast(@mod(time, 60)));
    try writeFraction(out, gpa, fraction, unit);
    if (utc) try out.append(gpa, 'Z');
}

fn rendered(gpa: std.mem.Allocator, comptime write: anytype, args: anytype) ![]u8 {
    var out: std.ArrayList(u8) = .empty;
    try @call(.auto, write, .{ &out, gpa } ++ args);
    return out.toOwnedSlice(gpa);
}

test "dates are written the way a csv of the same column would" {
    const gpa = std.testing.allocator;
    for ([_]struct { i64, []const u8 }{
        .{ 0, "1970-01-01" },
        .{ 19_723, "2024-01-01" },
        .{ -1, "1969-12-31" },
        .{ 19_782, "2024-02-29" },
    }) |case| {
        const got = try rendered(gpa, writeDate, .{case[0]});
        defer gpa.free(got);
        try std.testing.expectEqualStrings(case[1], got);
    }
}

test "timestamps drop a zero fraction and keep a real one" {
    const gpa = std.testing.allocator;
    const a = try rendered(gpa, writeTimestamp, .{ @as(i64, 1_704_067_200_000), Unit.millis, true });
    defer gpa.free(a);
    try std.testing.expectEqualStrings("2024-01-01T00:00:00Z", a);
    const b = try rendered(gpa, writeTimestamp, .{ @as(i64, 1_704_067_200_500), Unit.millis, false });
    defer gpa.free(b);
    try std.testing.expectEqualStrings("2024-01-01T00:00:00.5", b);
    const c = try rendered(gpa, writeTimestamp, .{ @as(i64, -1), Unit.millis, false });
    defer gpa.free(c);
    try std.testing.expectEqualStrings("1969-12-31T23:59:59.999", c);
}

test "a decimal is written out in full" {
    const gpa = std.testing.allocator;
    for ([_]struct { i128, i32, []const u8 }{
        .{ 12345, 2, "123.45" },
        .{ -5, 3, "-0.005" },
        .{ 7, 0, "7" },
        .{ 12, -2, "1200" },
    }) |case| {
        const got = try rendered(gpa, writeDecimal, .{ case[0], case[1] });
        defer gpa.free(got);
        try std.testing.expectEqualStrings(case[2], got);
    }
}

test "floats take the layout every port agrees on" {
    const gpa = std.testing.allocator;
    for ([_]struct { f64, []const u8 }{
        .{ 0.0, "0" },
        .{ 3.0, "3" },
        .{ 0.5, "0.5" },
        .{ -0.25, "-0.25" },
        .{ 1e10, "10000000000" },
        .{ 1e-10, "1e-10" },
        .{ 1.5e22, "1.5e+22" },
        .{ 0.000001, "0.000001" },
        .{ 2.25, "2.25" },
    }) |case| {
        const got = try rendered(gpa, writeFloat, .{case[0]});
        defer gpa.free(got);
        try std.testing.expectEqualStrings(case[1], got);
    }
}
