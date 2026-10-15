//! The command line.
//!
//! `--max-memory` is what this port is for. It is not a target the engine tries
//! to respect: it is a fixed buffer the allocator hands out from, so a
//! comparison that would exceed it fails at the allocation that would have
//! crossed the line, with the budget named. Every other implementation here can
//! only be *measured* and hoped about.
//!
//! Exit codes: 0 identical, 1 differences found, 2 error.

const std = @import("std");
const csvdiff = @import("csvdiff.zig");
const pqdiff = @import("pqdiff.zig");

const usage =
    \\csvdiff - composite-key comparison of CSV, JSON and Parquet, with a memory budget
    \\
    \\usage:
    \\  csvdiff compare A B -k COLS [options]
    \\
    \\options:
    \\  -k, --key COLS        comma-separated key column(s), required
    \\  -c, --compare COLS    columns to compare (default: all common non-key columns)
    \\  -i, --ignore COLS     columns to skip
    \\      --trim            strip whitespace before comparing
    \\      --ignore-case     ASCII only; a non-ASCII byte is refused by name
    \\      --empty-is-null   treat an empty string and an absent value as equal
    \\      --tolerance N     absolute numeric tolerance
    \\      --max-rows N      rows embedded per section (default 50000)
    \\      --delimiter D     force the delimiter (default: sniff it)
    \\      --threads N       how many threads to use (default: as many as cores)
    \\      --max-memory MB   fail rather than exceed this much memory
    \\      --json PATH       write the JSON summary here
    \\
    \\exit codes: 0 identical, 1 differences found, 2 error
    \\
;

fn split(gpa: std.mem.Allocator, s: []const u8) ![][]const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    var it = std.mem.splitScalar(u8, s, ',');
    while (it.next()) |part| {
        if (part.len > 0) try out.append(gpa, part);
    }
    return out.toOwnedSlice(gpa);
}

pub fn main(init: std.process.Init) !u8 {
    // Zig 0.16 hands main its arena, its general-purpose allocator and its
    // arguments; the budget below is carved out of the latter.
    const boot = init.gpa;
    // Argument lists live as long as the process, so they come from the arena
    // the runtime already cleans up rather than being freed by hand.
    const arena = init.arena.allocator();
    const args = try init.minimal.args.toSlice(arena);
    // The other two ports take this from the environment too, and print the same
    // shape of phase breakdown on stderr.
    // `contains` rather than `getPosix`: the latter is the POSIX-only accessor
    // and reading it on Windows does not compile. Only the presence of the
    // variable matters here, which is exactly what `contains` reports.
    csvdiff.phases_on = init.minimal.environ.contains(arena, "CSVDIFF_PHASES") catch false;
    // Set and not "0", as the C and C++ ports read it.
    csvdiff.fused_forced = if (init.minimal.environ.getAlloc(arena, "CSVDIFF_FUSED_JOIN")) |v|
        v.len > 0 and !std.mem.eql(u8, v, "0")
    else |_|
        false;

    const io = init.io;
    var stdout_buf: [4096]u8 = undefined;
    var stdout = std.Io.File.stdout().writer(io, &stdout_buf);
    var stderr_buf: [4096]u8 = undefined;
    var stderr = std.Io.File.stderr().writer(io, &stderr_buf);

    if (args.len < 2 or std.mem.eql(u8, args[1], "-h") or std.mem.eql(u8, args[1], "--help")) {
        try stdout.interface.writeAll(usage);
        try stdout.interface.flush();
        return if (args.len < 2) 2 else 0;
    }
    if (!std.mem.eql(u8, args[1], "compare")) {
        try stderr.interface.print("error: unknown command: {s}\n", .{args[1]});
        try stderr.interface.flush();
        return 2;
    }

    var opt = csvdiff.Options{ .key = &.{} };
    var files: std.ArrayList([]const u8) = .empty;
    defer files.deinit(boot);
    var json_path: ?[]const u8 = null;
    var max_memory_mb: ?usize = null;

    var i: usize = 2;
    while (i < args.len) : (i += 1) {
        const f = args[i];
        const need = struct {
            fn next(a: []const [:0]const u8, at: *usize) ?[]const u8 {
                if (at.* + 1 >= a.len) return null;
                at.* += 1;
                return a[at.*];
            }
        }.next;

        if (std.mem.eql(u8, f, "-k") or std.mem.eql(u8, f, "--key")) {
            opt.key = try split(arena, need(args, &i) orelse return fail(&stderr, "--key needs a value"));
        } else if (std.mem.eql(u8, f, "-c") or std.mem.eql(u8, f, "--compare")) {
            opt.compare = try split(arena, need(args, &i) orelse return fail(&stderr, "--compare needs a value"));
        } else if (std.mem.eql(u8, f, "-i") or std.mem.eql(u8, f, "--ignore")) {
            opt.ignore = try split(arena, need(args, &i) orelse return fail(&stderr, "--ignore needs a value"));
        } else if (std.mem.eql(u8, f, "--trim")) {
            opt.trim = true;
        } else if (std.mem.eql(u8, f, "--ignore-case")) {
            opt.ignore_case = true;
        } else if (std.mem.eql(u8, f, "--empty-is-null")) {
            opt.empty_is_null = true;
        } else if (std.mem.eql(u8, f, "--tolerance")) {
            opt.tolerance = std.fmt.parseFloat(f64, need(args, &i) orelse "") catch
                return fail(&stderr, "--tolerance needs a number");
        } else if (std.mem.eql(u8, f, "--max-rows")) {
            opt.max_rows = std.fmt.parseInt(usize, need(args, &i) orelse "", 10) catch
                return fail(&stderr, "--max-rows needs a number");
        } else if (std.mem.eql(u8, f, "--threads")) {
            opt.threads = std.fmt.parseInt(usize, need(args, &i) orelse "", 10) catch
                return fail(&stderr, "--threads needs a number");
        } else if (std.mem.eql(u8, f, "--max-memory")) {
            max_memory_mb = std.fmt.parseInt(usize, need(args, &i) orelse "", 10) catch
                return fail(&stderr, "--max-memory needs a number of megabytes");
        } else if (std.mem.eql(u8, f, "--delimiter")) {
            const d = need(args, &i) orelse return fail(&stderr, "--delimiter needs a value");
            opt.delimiter = d[0];
        } else if (std.mem.eql(u8, f, "--json")) {
            json_path = need(args, &i) orelse return fail(&stderr, "--json needs a path");
        } else if (std.mem.eql(u8, f, "--engine") or std.mem.eql(u8, f, "-o") or std.mem.eql(u8, f, "--out")) {
            _ = need(args, &i); // accepted so the flag set is portable; there is one engine and no report
        } else if (f.len > 0 and f[0] == '-') {
            return fail(&stderr, "unknown option");
        } else {
            try files.append(boot, f);
        }
    }

    if (files.items.len != 2) return fail(&stderr, "compare needs exactly two files");
    if (opt.key.len == 0) return fail(&stderr, "--key is required");

    // The budget, if one was given. A FixedBufferAllocator cannot hand out more
    // than it holds, so exceeding it is an error at the allocation rather than a
    // number someone notices afterwards.
    var budget: ?[]u8 = null;
    defer if (budget) |bs| boot.free(bs);
    var fixed: std.heap.FixedBufferAllocator = undefined;
    var gpa = boot;
    if (max_memory_mb) |mb| {
        budget = boot.alloc(u8, mb * 1024 * 1024) catch {
            try stderr.interface.print("error: cannot reserve {d} MB\n", .{mb});
            try stderr.interface.flush();
            return 2;
        };
        fixed = std.heap.FixedBufferAllocator.init(budget.?);
        // The lock-taking variant, because the comparison builds its two indexes
        // on two threads. A bump pointer without a lock would hand both the same
        // bytes; the budget it enforces is unchanged.
        gpa = fixed.threadSafeAllocator();
    }

    // Parquet on both sides goes to the columnar path, which never reconstructs
    // a row: it joins on the key columns and then compares whole columns as
    // integers. Where only one side is Parquet that is not available -- there is
    // no column to compare a byte stream against -- so the text engine reads it
    // into rows instead, which is slower and still answers the question rather
    // than refusing it.
    const both_parquet = pqdiff.isParquetFile(io, files.items[0]) and
        pqdiff.isParquetFile(io, files.items[1]);

    // The columnar path reads a narrow slice of Parquet on purpose -- uncompressed
    // or snappy, BYTE_ARRAY, v1 pages -- and the reader beside it handles the
    // rest: zstd, gzip, lz4, other column types, v2 pages. A refusal on
    // capability grounds, and only that, falls through to that one rather than
    // turning away a file this binary can in fact read. A truncated file or a
    // missing key column still fails, as the columnar path's own error.
    //
    // Without this a zstd Parquet pair was refused by a binary carrying a zstd
    // decoder. The Rust port had the same gap and closed it the same way.
    var columnar_ran = both_parquet;
    var early: ?csvdiff.Result = null;
    if (both_parquet) {
        if (pqdiff.compare(io, gpa, files.items[0], files.items[1], opt)) |r| {
            early = r;
        } else |err| {
            if (!csvdiff.isUnsupported(err)) return failRun(&stderr, err, max_memory_mb);
            // A capability refusal names nothing today, but whatever the next
            // reader fails on must not be printed with this one's subject.
            csvdiff.subject = null;
            columnar_ran = false;
        }
    }

    var result = early orelse (csvdiff.compare(io, gpa, files.items[0], files.items[1], opt) catch |err| {
        return failRun(&stderr, err, max_memory_mb);
    });

    defer result.deinit(gpa);

    const c = result.counts;
    if (json_path) |path| {
        var file = std.Io.Dir.cwd().createFile(io, path, .{}) catch
            return fail(&stderr, "cannot write the JSON summary");
        defer file.close(io);
        var buf: [8192]u8 = undefined;
        var w = file.writer(io, &buf);
        try w.interface.print(
            \\{{"counts":{{"a_rows":{d},"b_rows":{d},"a_keys":{d},"b_keys":{d},"matched":{d},
        ++ "\"unchanged\":{d},\"changed\":{d},\"added\":{d},\"removed\":{d}," ++
            "\"a_dup_keys\":{d},\"a_dup_rows\":{d},\"b_dup_keys\":{d},\"b_dup_rows\":{d}}},\"columns\":[",
            .{ c.a_rows, c.b_rows, c.a_keys, c.b_keys, c.matched, c.unchanged, c.changed, c.added, c.removed, c.a_dup_keys, c.a_dup_rows, c.b_dup_keys, c.b_dup_rows },
        );
        for (result.columns, 0..) |col, n| {
            if (n > 0) try w.interface.writeAll(",");
            try w.interface.print(
                "{{\"name\":\"{s}\",\"changed\":{d},\"blanked\":{d},\"filled\":{d}}}",
                .{ col.name, col.changed, col.blanked, col.filled },
            );
        }
        try w.interface.writeAll("]}");
        try w.interface.flush();
    }

    // Which path ran, not which one was asked for: a Parquet pair never goes
    // through the byte scanner.
    try stdout.interface.print(
        "A {d} rows | B {d} rows | matched {d} (changed {d}) | added {d} | removed {d} | dup keys A {d} B {d} | {s}\n",
        .{
            c.a_rows,                                                  c.b_rows,  c.matched,    c.changed,
            c.added,                                                   c.removed, c.a_dup_keys, c.b_dup_keys,
            if (columnar_ran) @as([]const u8, "parquet") else "turbo",
        },
    );
    try stdout.interface.flush();
    return if (result.identical()) 0 else 1;
}

/// A run that could not finish. A budget that was not enough is the one failure
/// worth naming in full: it is the answer to the question `--max-memory` asked.
fn failRun(stderr: anytype, err: anyerror, max_memory_mb: ?usize) !u8 {
    if (err == error.OutOfMemory) {
        if (max_memory_mb) |mb| {
            try stderr.interface.print(
                "error: the comparison needs more than the {d} MB it was given\n",
                .{mb},
            );
            try stderr.interface.flush();
            return 2;
        }
    }
    if (csvdiff.subject) |name| {
        try stderr.interface.print("error: {s}: {s}\n", .{ csvdiff.message(err), name });
        try stderr.interface.flush();
        return 2;
    }
    return fail(stderr, csvdiff.message(err));
}

fn fail(stderr: anytype, message: []const u8) !u8 {
    try stderr.interface.print("error: {s}\n", .{message});
    try stderr.interface.flush();
    return 2;
}
