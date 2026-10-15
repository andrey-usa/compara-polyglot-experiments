//! A field packed into one word: offset, length, and whether it needs unescaping.
//!
//! Every reader here — CSV, newline-delimited JSON and Parquet — ends up
//! describing a value the same way: a run of bytes somewhere, addressed rather
//! than copied. What differs is where those bytes live and how they are escaped,
//! which is `slab.zig`'s business rather than this one's.
//!
//! 40 bits of offset addresses a terabyte and 23 bits of length a field of eight
//! megabytes. An over-long field is reported rather than truncated through the
//! mask.

const std = @import("std");

pub const Field = u64;

/// A field that is not there at all: a short row, a missing JSON key, a null.
pub const ABSENT: Field = std.math.maxInt(u64);
/// A field too long for the packed length, reported rather than truncated.
pub const TOO_LONG: Field = std.math.maxInt(u64) - 1;

const OFFSET_MASK: u64 = (1 << 40) - 1;
const LENGTH_SHIFT: u6 = 40;
const LENGTH_MASK: u64 = (1 << 23) - 1;
const ESCAPED: u64 = 1 << 63;

pub const MAX_FIELD_LEN: u64 = LENGTH_MASK;

pub fn pack(offset: usize, len: usize, escaped: bool) Field {
    if (len > MAX_FIELD_LEN) return TOO_LONG;
    return (@as(u64, offset) & OFFSET_MASK) |
        ((@as(u64, len) & LENGTH_MASK) << LENGTH_SHIFT) |
        (if (escaped) ESCAPED else 0);
}

pub fn offsetOf(f: Field) usize {
    return @intCast(f & OFFSET_MASK);
}

pub fn lenOf(f: Field) usize {
    return @intCast((f >> LENGTH_SHIFT) & LENGTH_MASK);
}

pub fn isEscaped(f: Field) bool {
    return (f & ESCAPED) != 0;
}

/// Whether the word describes bytes at all, as opposed to one of the sentinels.
pub fn isReal(f: Field) bool {
    return f != ABSENT and f != TOO_LONG;
}

/// Moves a field's bytes to a new place in the same arena. The offset is the low
/// bits of the word, so shifting it is an addition — as long as the sentinels
/// are left alone, which is the whole reason this is a function.
pub fn shift(f: Field, by: u64) Field {
    return if (isReal(f)) f + by else f;
}

test "an over-long field is a sentinel rather than a truncation" {
    try std.testing.expectEqual(TOO_LONG, pack(0, MAX_FIELD_LEN + 1, false));
    const f = pack(1 << 30, MAX_FIELD_LEN, true);
    try std.testing.expectEqual(@as(usize, 1 << 30), offsetOf(f));
    try std.testing.expectEqual(@as(usize, MAX_FIELD_LEN), lenOf(f));
    try std.testing.expect(isEscaped(f));
    try std.testing.expectEqual(@as(usize, (1 << 30) + 64), offsetOf(shift(f, 64)));
    try std.testing.expectEqual(ABSENT, shift(ABSENT, 64));
}
