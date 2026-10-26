//! Comparing two Parquet files without turning them back into rows.
//!
//! The Rust twin of `cpp/src/pqdiff.cpp`, and the same design. The `turbo`
//! engine beside this one has one shape: map the file, find every row, reduce
//! each row to a handful of (offset, length) fields. That shape is right for a
//! text format, where a value's boundaries are only known by scanning for them.
//!
//! Parquet is not that. A value's boundaries are written down, values of one
//! column are contiguous, and -- this is the part worth exploiting -- a
//! low-cardinality column is stored as small integers indexing a dictionary of
//! its distinct values. Reading such a file back into rows in order to compare
//! them row by row throws away the one thing the format gives you.
//!
//! So this path is columnar end to end. It reads the key columns, joins on them
//! once to produce a list of matched `(a_row, b_row)` pairs, and then walks the
//! compared columns one at a time, releasing each before reading the next. Where
//! both sides of a column are dictionary encoded, the two dictionaries are
//! mapped onto one shared id space once -- a few thousand string comparisons --
//! after which "did this cell change" is `i32 != i32`, which the compiler
//! vectorises, and the resulting mismatch mask is scanned eight bytes at a time
//! with the same SWAR trick the CSV scanner uses to find a delimiter.
//!
//! A column of numbers, dates or booleans is held as one word per row, in a
//! canonical form (see [`parquet::Words`]), so that "did this cell change" is a
//! `u64` compare there too, and a cell is written out as text only when the
//! report shows it or `--export-dir` lists it -- by the `turbo` reader's own
//! rule, so the two paths cannot disagree about how a value reads.
//!
//! The result is the same `EngineResult`, and therefore the same JSON, as
//! comparing the same data as CSV.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use memmap2::Mmap;

use super::turbo::parquet::{ValueKind, value_kinds};
use crate::alloc;
use crate::columns::{compare_keys, resolve};
use crate::contract::{Cell, CellDiff, ColumnStat, Counts, EngineResult, Section, Val};
use crate::error::{Error, Result};
use crate::memory;
use crate::options::Options;
use crate::parallel;
use crate::parquet;
use crate::phases::Phases;
use crate::rowstore::Joined;
use crate::sections::{create_export_dir, export_file, export_headers, sections};

/// True when both paths are Parquet, so the caller knows to come here.
pub fn is_parquet(path: &Path) -> bool {
    parquet::is_parquet(path)
}

// ---------------------------------------------------------------------------
// Values: the same rules as the CSV path, on a plain byte span
// ---------------------------------------------------------------------------

/// One cell. Parquet values carry no escaping -- a byte array is its own bytes
/// -- so where the CSV path has to read a field through its escape decoder,
/// here a cell is already a span. A word-sized value is its eight bytes, with
/// `kind` saying how they read as text.
#[derive(Clone, Copy)]
struct Look<'a> {
    bytes: &'a [u8],
    null: bool,
    kind: Option<ValueKind>,
}

impl<'a> Look<'a> {
    const NONE: Look<'static> = Look {
        bytes: &[],
        null: true,
        kind: None,
    };
}

/// A present cell as text: its bytes, or its word written out.
fn text_of(c: Look<'_>) -> Cow<'_, str> {
    match c.kind {
        Some(kind) => {
            let mut out = Vec::with_capacity(24);
            kind.write(u64::from_le_bytes(c.bytes.try_into().unwrap()), &mut out);
            Cow::Owned(String::from_utf8(out).unwrap_or_default())
        }
        None => String::from_utf8_lossy(c.bytes),
    }
}

fn needs_normalising(opt: &Options) -> bool {
    opt.trim || opt.ignore_case || opt.empty_is_null || opt.tolerance > 0.0
}

fn value_of(c: Look<'_>, opt: &Options) -> Val {
    if c.null || c.bytes.is_empty() {
        return None;
    }
    let mut text = text_of(c).into_owned();
    if opt.trim {
        text = text.trim().to_string();
    }
    if opt.ignore_case {
        text = text.to_lowercase();
    }
    // A value that trims to nothing is still a value, "", as it is in the CSV
    // engines: only --empty-is-null makes it absent. Reading it as absent made
    // `"  "` against an empty cell no change here and a change on the same rows
    // as CSV.
    if text.is_empty() && opt.empty_is_null {
        return None;
    }
    Some(text)
}

// `absent` and `same` run for every key cell the index and the join touch, and
// were calls: the compiler kept them out of line because the normalising half
// allocates. The common case is a null check and a byte compare, so that is what
// is inlined, and the normalising half is a cold call. On the 2M Parquet pair
// the two functions were 0.49B instructions of their own, and `Col::at` beside
// them another 0.49B.
#[inline]
fn absent(c: Look<'_>, opt: &Options) -> bool {
    if c.null || c.bytes.is_empty() {
        return true;
    }
    // A written-out value is never empty, trimmed or not, so only a null is.
    c.kind.is_none() && needs_normalising(opt) && absent_normalised(c, opt)
}

#[cold]
#[inline(never)]
fn absent_normalised(c: Look<'_>, opt: &Options) -> bool {
    value_of(c, opt).is_none()
}

#[inline]
fn same(x: Look<'_>, y: Look<'_>, opt: &Options) -> bool {
    let (xa, ya) = (absent(x, opt), absent(y, opt));
    if xa || ya {
        return xa && ya;
    }
    if !needs_normalising(opt) {
        return x.bytes == y.bytes;
    }
    same_normalised(x, y, opt)
}

#[cold]
#[inline(never)]
fn same_normalised(x: Look<'_>, y: Look<'_>, opt: &Options) -> bool {
    value_of(x, opt) == value_of(y, opt)
}

/// Deliberately stricter than a bare parse: "inf" and "nan" are ordinary text in
/// a table, and treating them as numbers would make two unequal strings compare
/// equal.
fn as_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let first = body.as_bytes().first()?;
    if !(first.is_ascii_digit() || *first == b'.') {
        return None;
    }
    if !body
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// SQL's IS DISTINCT FROM, with the tolerance applied where both sides parse.
fn cell_differs(x: Look<'_>, y: Look<'_>, opt: &Options) -> bool {
    let (xa, ya) = (absent(x, opt), absent(y, opt));
    if xa && ya {
        return false;
    }
    if opt.tolerance > 0.0 && !xa && !ya {
        let nx = as_number(&text_of(x));
        let ny = as_number(&text_of(y));
        if let (Some(a), Some(b)) = (nx, ny) {
            return (a - b).abs() > opt.tolerance;
        }
    }
    !same(x, y, opt)
}

const PRIME: u64 = 0x100000001b3;
const SEED: u64 = 0xcbf29ce484222325;

/// FNV-1a, eight bytes at a time.
///
/// A hash here is internal in exactly the sense the CSV engine's is: nothing
/// outside this module sees one, so the only property it owes anyone is that
/// the two places that compute it agree. That is what lets it read a word at a
/// time -- see `turbo::hash_bytes`, which is the same function for the same
/// reason.
///
/// It matters more than the call sites suggest. Interning a dictionary costs
/// one call per *distinct* value and is nothing; a key column that is not
/// dictionary-encoded -- which a high-cardinality key column usually is not,
/// since a writer gives up on a dictionary that never repeats -- is hashed once
/// per row, on both sides, indexed and probed.
fn fold_bytes(mut h: u64, v: &[u8]) -> u64 {
    let (words, tail) = v.as_chunks::<8>();
    for chunk in words {
        h = (h ^ u64::from_le_bytes(*chunk)).wrapping_mul(PRIME);
        h ^= h >> 29; // the xor-shift is what spreads a whole word into the low bits
    }
    if !tail.is_empty() {
        h = (h ^ tail_word(v, tail.len())).wrapping_mul(PRIME);
        h ^= h >> 29;
    }
    (h ^ v.len() as u64).wrapping_mul(PRIME)
}

/// The last `rem` bytes of `v`, one to seven of them, as a little-endian word
/// padded with zeros.
///
/// This was a copy into a zeroed buffer, and a copy of a length only known at
/// run time is a call to memcpy, once per key cell per row. A value of eight
/// bytes or more has eight real bytes ending where the tail does, so one load
/// and a shift gives the same word; a shorter one is read as two overlapping
/// halves, or as its first, middle and last byte. The Zig port does the same
/// (`scan.tailWord`).
#[inline]
pub(super) fn tail_word(v: &[u8], rem: usize) -> u64 {
    let n = v.len();
    debug_assert!(rem > 0 && rem < 8 && rem <= n);
    if n >= 8 {
        let last = u64::from_le_bytes(v[n - 8..].try_into().unwrap());
        return last >> (8 * (8 - rem));
    }
    // Shorter than a word: nothing was folded yet, so the tail is all of it.
    if n >= 4 {
        let lo = u32::from_le_bytes(v[..4].try_into().unwrap()) as u64;
        let hi = u32::from_le_bytes(v[n - 4..].try_into().unwrap()) as u64;
        return lo | (hi << (8 * (n - 4)));
    }
    v[0] as u64 | (v[n / 2] as u64) << (8 * (n / 2)) | (v[n - 1] as u64) << (8 * (n - 1))
}
/// [`fold_bytes`] of one eight-byte word, the same value without the loop
/// and the tail test: a word key column is hashed once per row on both sides.
#[inline]
fn fold_word(h: u64, w: &[u8; 8]) -> u64 {
    let mut h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(PRIME);
    h ^= h >> 29;
    (h ^ 8).wrapping_mul(PRIME)
}
fn fold_absent(h: u64) -> u64 {
    (h ^ 0x9e3779b97f4a7c15).wrapping_mul(PRIME)
}
fn fold_id(h: u64, id: i32) -> u64 {
    (h ^ (id as u32) as u64).wrapping_mul(PRIME)
}

// ---------------------------------------------------------------------------
// A column, as the comparison sees it
// ---------------------------------------------------------------------------

/// `parquet::Column` hands back offsets; this resolves them against whichever
/// buffer they belong to and answers "what is in row r".
struct Col<'m> {
    c: parquet::Column,
    map: &'m [u8],
    /// Set for a column held as words.
    kind: Option<ValueKind>,
}

impl<'m> Col<'m> {
    fn new(c: parquet::Column, map: &'m [u8], kind: ValueKind) -> Self {
        let kind = c.words.as_ref().map(|_| kind);
        Col { c, map, kind }
    }
    #[inline]
    fn base(&self) -> &[u8] {
        if self.c.owned.is_empty() {
            self.map
        } else {
            &self.c.owned
        }
    }
    #[inline]
    fn at(&self, row: usize) -> Look<'_> {
        if let Some(w) = &self.c.words {
            return Look {
                bytes: &w.values[row],
                null: w.is_null(row),
                kind: self.kind,
            };
        }
        look(self.base(), self.slice_at(row))
    }
    fn dict_at(&self, k: usize) -> Look<'_> {
        look(self.base(), self.c.dict[k])
    }
    /// The row's slice, without resolving it. Hot loops take this and the base
    /// separately, so neither the "which buffer" branch nor its bounds check
    /// is paid per cell.
    #[inline]
    fn slice_at(&self, row: usize) -> parquet::Slice {
        if self.c.dictionary {
            let k = self.c.index[row];
            if k < 0 {
                return parquet::Slice::null();
            }
            return self.c.dict[k as usize];
        }
        self.c.values[row]
    }
    fn rows(&self) -> usize {
        self.c.rows()
    }
}

/// Resolves a slice against the buffer it belongs to.
#[inline]
fn look(base: &[u8], s: parquet::Slice) -> Look<'_> {
    if s.is_null() {
        return Look::NONE;
    }
    let at = s.offset();
    Look {
        bytes: &base[at..at + s.length()],
        null: false,
        kind: None,
    }
}

// ---------------------------------------------------------------------------
// One id space for two dictionaries
// ---------------------------------------------------------------------------

/// The trick the whole columnar path turns on. Two files' dictionaries are
/// interned into one dense id space, which costs one hash per *distinct* value
/// rather than one per row; after that, two cells are equal exactly when their
/// ids are, and a column diff is a comparison of two `i32` arrays.
#[derive(Default)]
struct Ids {
    table: HashMap<Vec<u8>, i32, BuildHasherDefault<FoldHasher>>,
}

impl Ids {
    /// Looked up before it is inserted: `entry` wants an owned key, so asking it
    /// allocated a copy of every value, including the ones already interned.
    fn of(&mut self, v: &[u8]) -> i32 {
        if let Some(&id) = self.table.get(v) {
            return id;
        }
        let next = self.table.len() as i32;
        self.table.insert(v.to_vec(), next);
        next
    }
}

/// The key hash this file already uses, as a `Hasher`, for interning. The
/// default SipHash is keyed against collision attacks, which a table of one
/// file's own dictionary values does not need; it was 0.18B instructions on the
/// 2M pair for 1.6M lookups.
#[derive(Default)]
struct FoldHasher(u64);

impl Hasher for FoldHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = fold_bytes(self.0 ^ SEED, bytes);
    }
}

/// Codes one column's dictionary into `ids`, with -1 for an absent value.
fn code(col: &Col<'_>, ids: &mut Ids, opt: &Options) -> Result<Vec<i32>> {
    let mut out = alloc::sized(col.c.dict.len(), "one id per dictionary entry")?;
    for k in 0..col.c.dict.len() {
        let cell = col.dict_at(k);
        if absent(cell, opt) {
            out.push(-1);
            continue;
        }
        if needs_normalising(opt) {
            let v = value_of(cell, opt).unwrap_or_default();
            out.push(ids.of(v.as_bytes()));
        } else {
            out.push(ids.of(cell.bytes));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// One file's key columns. A key column both sides store as a dictionary is
/// reduced to a shared id per row, and from there hashing and equality are
/// integer work; anything else stays bytes and is compared as bytes.
struct KeySide<'m> {
    col: Vec<Col<'m>>,
    /// Filled where the column is id-coded on both sides.
    id: Vec<Vec<i32>>,
    rows: usize,
}

/// The key hash of rows `from..from + out.len()` of `s`, into `out`.
///
/// A column at a time rather than a row at a time: the per-row form paid for
/// its setup -- which columns are ids, whether anything normalises -- on every
/// row, about 140 instructions a row for a key of two integers, where the loop
/// over one column of words is a load, a multiply and a shift. Each row's hash
/// folds its columns in the same order either way, so the value is the same.
fn hash_rows(as_id: &[bool], s: &KeySide<'_>, from: usize, out: &mut [u64], opt: &Options) {
    out.fill(SEED);
    let rows = from..from + out.len();
    for (j, &coded) in as_id.iter().enumerate() {
        if coded {
            for (h, &id) in out.iter_mut().zip(&s.id[j][rows.clone()]) {
                *h = fold_id(*h, id);
            }
            continue;
        }
        // A word is absent only when null and is never normalised away, so
        // this is the general case below with nothing left to decide.
        if let (Some(w), false) = (&s.col[j].c.words, needs_normalising(opt)) {
            let values = &w.values[rows.clone()];
            if w.nulls.is_empty() {
                for (h, v) in out.iter_mut().zip(values) {
                    *h = fold_word(*h, v);
                }
            } else {
                for ((h, v), r) in out.iter_mut().zip(values).zip(rows.clone()) {
                    *h = if w.is_null(r) {
                        fold_absent(*h)
                    } else {
                        fold_word(*h, v)
                    };
                }
            }
            continue;
        }
        if !needs_normalising(opt) {
            let read = KeyRead::of(false, s, j);
            for (h, r) in out.iter_mut().zip(rows.clone()) {
                *h = match read.bytes(r) {
                    None => fold_absent(*h),
                    Some(v) => fold_bytes(*h, v),
                };
            }
            continue;
        }
        for (h, r) in out.iter_mut().zip(rows.clone()) {
            let c = s.col[j].at(r);
            *h = if absent(c, opt) {
                fold_absent(*h)
            } else if !needs_normalising(opt) {
                fold_bytes(*h, c.bytes)
            } else {
                fold_bytes(*h, value_of(c, opt).unwrap_or_default().as_bytes())
            };
        }
    }
}

fn row_eq(
    as_id: &[bool],
    x: &KeySide<'_>,
    rx: usize,
    y: &KeySide<'_>,
    ry: usize,
    opt: &Options,
) -> bool {
    for (j, &coded) in as_id.iter().enumerate() {
        if coded {
            if x.id[j][rx] != y.id[j][ry] {
                return false;
            }
        } else if let (Some(xw), Some(yw), false) =
            (&x.col[j].c.words, &y.col[j].c.words, needs_normalising(opt))
        {
            // `same` on two words: both null, or neither and the same word.
            let null = xw.is_null(rx);
            if null != yw.is_null(ry) || (!null && xw.values[rx] != yw.values[ry]) {
                return false;
            }
        } else if !same(x.col[j].at(rx), y.col[j].at(ry), opt) {
            return false;
        }
    }
    true
}

/// One side's key column as the hot loops read it, settled once rather than
/// asked per cell: which form it is held in, and the buffers that form reads.
/// `row_eq` and `Col::at` asked both on every cell -- whether the column was
/// coded, held as words, a dictionary, which buffer its slices point into --
/// and on the 200k-row Parquet pair that was about 450 instructions per key
/// comparison, most of the join.
#[derive(Clone, Copy)]
enum KeyRead<'a> {
    Id(&'a [i32]),
    Word(&'a parquet::Words),
    Dict {
        base: &'a [u8],
        dict: &'a [parquet::Slice],
        index: &'a [i32],
    },
    Plain {
        base: &'a [u8],
        values: &'a [parquet::Slice],
    },
}

impl<'a> KeyRead<'a> {
    fn of(coded: bool, s: &'a KeySide<'_>, j: usize) -> Self {
        if coded {
            return KeyRead::Id(&s.id[j]);
        }
        let col = &s.col[j];
        if let Some(w) = &col.c.words {
            return KeyRead::Word(w);
        }
        if col.c.dictionary {
            KeyRead::Dict {
                base: col.base(),
                dict: &col.c.dict,
                index: &col.c.index,
            }
        } else {
            KeyRead::Plain {
                base: col.base(),
                values: &col.c.values,
            }
        }
    }

    /// A byte column's cell, or `None` where it is absent -- null or empty, as
    /// [`absent`] reads a cell when nothing normalises.
    #[inline(always)]
    fn bytes(self, row: usize) -> Option<&'a [u8]> {
        let (base, s) = match self {
            KeyRead::Dict { base, dict, index } => {
                let k = index[row];
                if k < 0 {
                    return None;
                }
                (base, dict[k as usize])
            }
            KeyRead::Plain { base, values } => (base, values[row]),
            KeyRead::Id(_) | KeyRead::Word(_) => unreachable!("not a byte column"),
        };
        if s.is_null() || s.length() == 0 {
            return None;
        }
        Some(&base[s.offset()..s.offset() + s.length()])
    }

    /// Starts the fetch of what [`KeyRead::bytes`] and its kin read first
    /// of `row`: the id, word or slice handle.
    #[inline(always)]
    fn prefetch(self, row: usize) {
        let p: *const u8 = match self {
            KeyRead::Id(v) => v.as_ptr().wrapping_add(row).cast(),
            KeyRead::Word(w) => w.values.as_ptr().wrapping_add(row).cast(),
            KeyRead::Dict { index, .. } => index.as_ptr().wrapping_add(row).cast(),
            KeyRead::Plain { values, .. } => values.as_ptr().wrapping_add(row).cast(),
        };
        prefetch_at(p);
    }

    fn is_bytes(self) -> bool {
        matches!(self, KeyRead::Dict { .. } | KeyRead::Plain { .. })
    }
}

/// Two byte strings equal, short ones by a load or two rather than a call to
/// `memcmp`: a key cell is typically an id of a dozen bytes.
#[inline(always)]
fn bytes_eq(p: &[u8], q: &[u8]) -> bool {
    let n = p.len();
    if n != q.len() {
        return false;
    }
    if n > 16 {
        return p == q;
    }
    if n >= 8 {
        let word = |v: &[u8], at: usize| u64::from_le_bytes(v[at..at + 8].try_into().unwrap());
        return word(p, 0) == word(q, 0) && word(p, n - 8) == word(q, n - 8);
    }
    n == 0 || tail_word(p, n) == tail_word(q, n)
}

/// [`row_eq`] between two sides, with each key column read through a
/// [`KeyRead`] decided once. Normalising options, and a column held in
/// different forms on the two sides, keep `row_eq`.
struct KeyEq<'a> {
    cols: Vec<(KeyRead<'a>, KeyRead<'a>)>,
    general: bool,
    as_id: &'a [bool],
    x: &'a KeySide<'a>,
    y: &'a KeySide<'a>,
    opt: &'a Options,
}

impl<'a> KeyEq<'a> {
    fn new(as_id: &'a [bool], x: &'a KeySide<'a>, y: &'a KeySide<'a>, opt: &'a Options) -> Self {
        let cols: Vec<_> = as_id
            .iter()
            .enumerate()
            .map(|(j, &coded)| (KeyRead::of(coded, x, j), KeyRead::of(coded, y, j)))
            .collect();
        let general = needs_normalising(opt)
            || !cols.iter().all(|&(a, b)| match (a, b) {
                (KeyRead::Id(_), KeyRead::Id(_)) | (KeyRead::Word(_), KeyRead::Word(_)) => true,
                _ => a.is_bytes() && b.is_bytes(),
            });
        KeyEq {
            cols,
            general,
            as_id,
            x,
            y,
            opt,
        }
    }

    /// The second stage of the sweep's prefetch: the slot `h` lands in,
    /// fetched [`PREFETCH_AHEAD`] / 2 probes ago, names the first side's row a
    /// hit will compare.
    #[inline(always)]
    fn prefetch_mate(&self, into: &Index, h: u64) {
        let word = into.slots[(h & into.mask) as usize];
        if word != 0 && into.tag_is(word, h) {
            let row = into.row_of(word);
            for &(a, _) in &self.cols {
                a.prefetch(row);
            }
        }
    }

    /// Whether row `rx` of the first side has the key of row `ry` of the second.
    #[inline(always)]
    fn eq(&self, rx: usize, ry: usize) -> bool {
        if self.general {
            return row_eq(self.as_id, self.x, rx, self.y, ry, self.opt);
        }
        for &(a, b) in &self.cols {
            let same = match (a, b) {
                (KeyRead::Id(x), KeyRead::Id(y)) => x[rx] == y[ry],
                (KeyRead::Word(x), KeyRead::Word(y)) => {
                    let null = x.is_null(rx);
                    null == y.is_null(ry) && (null || x.values[rx] == y.values[ry])
                }
                _ => match (a.bytes(rx), b.bytes(ry)) {
                    (None, None) => true,
                    (Some(p), Some(q)) => bytes_eq(p, q),
                    _ => false,
                },
            };
            if !same {
                return false;
            }
        }
        true
    }
}

/// An open-addressed table over one file's distinct keys, first occurrence wins.
///
/// A slot is one word: the top bits of the key's hash, and the row of the
/// key's first occurrence plus one, with zero meaning empty. Carrying the hash
/// *inside* the slot is the point -- a probe that misses is settled by the word
/// it already loaded, where a table of bare positions would have to follow each
/// one into a separate array of hashes and take a second cache miss to reject
/// it. At ten million keys those second misses were the join. Holding the row
/// rather than a position in `firsts` saves the hop a hit took through
/// `firsts` to find it, a miss of its own on every probe that matched.
#[derive(Default)]
struct Index {
    slots: Vec<u32>,
    mask: u64,
    /// Low bits of a slot holding positions plus one; the high bits hold the
    /// top of the key's hash as a tag. Chosen from the row count in `build`,
    /// so the slot stays four bytes at any scale this port runs.
    pos_bits: u32,
    pos_mask: u32,
    /// The first row of each distinct key, in row order.
    firsts: Vec<i32>,
    /// Per row, how many more times the key that first appears there
    /// appears again; empty while no key has.
    extra: Vec<u32>,
    /// Per distinct key, for probing the other side.
    hashes: Vec<u64>,
    rows: i64,
    dup_keys: i64,
    dup_rows: i64,
}

impl Index {
    fn set_pos_bits(&mut self, n: usize) {
        let mut bits = 1u32;
        while bits < 32 && (n + 1) > ((1usize << bits) - 1) {
            bits += 1;
        }
        self.pos_bits = bits;
        self.pos_mask = if bits >= 32 {
            0xFFFF_FFFF
        } else {
            ((1u64 << bits) - 1) as u32
        };
    }

    /// Packs a slot: the top `32 - pos_bits` bits of the hash above the
    /// row plus one. Zero tag bits (past two billion rows) degrades to no tag
    /// rather than a wrong answer.
    fn slot_for(&self, hash: u64, row: usize) -> u32 {
        let pos = row as u32 + 1;
        let tag_bits = 32 - self.pos_bits;
        if tag_bits == 0 {
            pos
        } else {
            (((hash >> (64 - tag_bits)) as u32) << self.pos_bits) | pos
        }
    }

    /// Whether the slot's tag matches the hash's top bits.
    fn tag_is(&self, word: u32, hash: u64) -> bool {
        let tag_bits = 32 - self.pos_bits;
        if tag_bits == 0 {
            return true;
        }
        (word >> self.pos_bits) == (hash >> (64 - tag_bits)) as u32
    }

    /// The first row a non-empty slot holds.
    fn row_of(&self, word: u32) -> usize {
        ((word & self.pos_mask) - 1) as usize
    }

    fn unique(&self) -> i64 {
        self.firsts.len() as i64
    }
}

/// Hashes are computed in parallel because a row's hash depends on nothing but
/// that row; insertion is serial because first-occurrence-wins depends on the
/// order rows arrive, and threading it would make the answer depend on the
/// scheduler. Same split as the CSV path.
/// How many rows ahead a slot probe is started. The same distance the CSV engine
/// uses, and as there it is not delicate.
const PREFETCH_AHEAD: usize = 32;

/// Starts the fetch of the slot `h` will land in, without waiting for it.
///
/// The build loop walks an array of hashes it already holds and the sweep walks
/// another, and both then probe a slot table of tens of megabytes: a serial chain
/// of misses, each waiting on an address known long before the load was issued.
#[inline]
/// A worker that panicked. Nothing in here is expected to, but a panic in a
/// thread must not become a wrong answer in the caller.
fn panicked() -> Error {
    Error::new("a comparison worker panicked")
}

fn prefetch_slot(slots: &[u32], mask: u64, h: u64) {
    #[cfg(target_arch = "x86_64")]
    // Safety: the index is masked into the table's length, so the pointer is in
    // bounds, and a prefetch has no architectural effect in any case.
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(slots.as_ptr().add((h & mask) as usize) as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (slots, mask, h);
    }
}

/// Starts the fetch of the cache line at `p`, which need not be valid: a
/// prefetch has no effect but on the cache.
#[inline]
fn prefetch_at(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    // Safety: a prefetch has no architectural effect, and the address is not
    // dereferenced.
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(p.cast());
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = p;
    }
}

fn build_index(as_id: &[bool], s: &KeySide<'_>, opt: &Options, threads: usize) -> Result<Index> {
    let n = s.rows;
    let mut hs = alloc::filled(0u64, n, "one hash per row")?;
    let ways = if n < (1 << 15) { 1 } else { threads.max(1) };
    if ways == 1 {
        hash_rows(as_id, s, 0, &mut hs, opt);
    } else {
        let per = n.div_ceil(ways);
        // How many chunks got a thread. The OS can refuse one -- a thread wants
        // a stack, and a stack is a private mapping, which is the same budget
        // everything else here comes out of -- and the chunks it refuses are
        // hashed below instead of taking the process down with a panic.
        let mut spawned = 0usize;
        std::thread::scope(|scope| {
            for (p, chunk) in hs.chunks_mut(per).enumerate() {
                let go = move || hash_rows(as_id, s, p * per, chunk, opt);
                if std::thread::Builder::new().spawn_scoped(scope, go).is_err() {
                    break;
                }
                spawned = p + 1;
            }
        });
        let left = (spawned * per).min(n);
        hash_rows(as_id, s, left, &mut hs[left..], opt);
    }

    let mut ix = Index {
        rows: n as i64,
        ..Index::default()
    };
    // Sized to about a two-thirds load: linear probing is still short there,
    // and a smaller table is a smaller working set, which is what this phase is
    // actually limited by.
    let mut cap = 1usize << 12;
    while cap * 2 < n * 3 + 16 {
        cap <<= 1;
    }
    // Written rather than calloc'd, for the reason the CSV engine's `empty_table`
    // gives: a fresh mapping is one shared page of zeroes until something writes
    // to it, and nothing reads this table before the inserts start, so every one
    // of its pages would be first touched by a random probe.
    ix.slots = alloc::sized(cap, "the key index")?;
    ix.slots.resize(cap, 0u32);
    ix.mask = cap as u64 - 1;
    ix.set_pos_bits(n);
    alloc::grow(&mut ix.firsts, n, "one row per distinct key")?;
    alloc::grow(&mut ix.hashes, n, "one hash per distinct key")?;

    let eq = KeyEq::new(as_id, s, s, opt);
    for (r, &h) in hs.iter().enumerate() {
        if let Some(&soon) = hs.get(r + PREFETCH_AHEAD) {
            prefetch_slot(&ix.slots, ix.mask, soon);
        }
        let mut at = (h & ix.mask) as usize;
        loop {
            let slot = ix.slots[at];
            if slot == 0 {
                ix.slots[at] = ix.slot_for(h, r);
                ix.firsts.push(r as i32);
                ix.hashes.push(h);
                break;
            }
            if ix.tag_is(slot, h) {
                let first = ix.row_of(slot);
                if eq.eq(first, r) {
                    if ix.extra.is_empty() {
                        ix.extra = alloc::filled(0u32, n, "a repeat count per row")?;
                    }
                    ix.extra[first] += 1;
                    if ix.extra[first] == 1 {
                        ix.dup_keys += 1;
                        ix.dup_rows += 1; // the first occurrence counts once the key repeats
                    }
                    ix.dup_rows += 1;
                    break;
                }
            }
            at = (at + 1) & ix.mask as usize;
        }
    }
    Ok(ix)
}

/// Looks one side's row up in the other side's table.
/// `eq` compares a row of the table's side (first) with one of the probing
/// side (second).
#[inline(always)]
fn lookup(into: &Index, eq: &KeyEq<'_>, row: usize, h: u64) -> i32 {
    let mut at = (h & into.mask) as usize;
    loop {
        let slot = into.slots[at];
        if slot == 0 {
            return -1;
        }
        if into.tag_is(slot, h) {
            let first = into.row_of(slot);
            if eq.eq(first, row) {
                return first as i32;
            }
        }
        at = (at + 1) & into.mask as usize;
    }
}

// ---------------------------------------------------------------------------
// Row lists, capped but exactly counted
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Capped {
    held: Vec<i32>,
    total: i64,
}

impl Capped {
    fn push(&mut self, row: i32, cap: usize) {
        self.total += 1;
        // One past the cap, so a section can still report that it was truncated.
        if self.held.len() <= cap {
            self.held.push(row);
        }
    }
}

// ---------------------------------------------------------------------------
// The export: every listed row, as text
// ---------------------------------------------------------------------------

/// One column's cells for `--export-dir`, in the order its file lists them:
/// their text in one buffer and an end offset each. The row path holds an
/// exported cell as a `Val`, a heap string per cell; at sixty thousand changed
/// rows of eighteen columns, both sides, that is two million of them.
#[derive(Default)]
struct Cells {
    text: Vec<u8>,
    /// Where each cell ends. The top bit marks an absent cell, which writes as
    /// the same empty field as `""` and differs from it only in key order,
    /// where an absent value sorts last.
    ends: Vec<u64>,
}

const ABSENT_CELL: u64 = 1 << 63;
const EXPORTED: &str = "the exported cells of one column";

impl Cells {
    fn with_room(n: usize) -> Result<Self> {
        Ok(Cells {
            text: Vec::new(),
            ends: alloc::sized(n, EXPORTED)?,
        })
    }

    fn push(&mut self, c: Look<'_>, opt: &Options) -> Result<()> {
        let present = render(c, opt, &mut self.text)?;
        let end = self.text.len() as u64;
        alloc::push(
            &mut self.ends,
            if present { end } else { end | ABSENT_CELL },
            EXPORTED,
        )
    }

    fn get(&self, i: usize) -> Option<&[u8]> {
        let end = self.ends[i];
        if end & ABSENT_CELL != 0 {
            return None;
        }
        let start = match i {
            0 => 0,
            _ => self.ends[i - 1] & !ABSENT_CELL,
        };
        Some(&self.text[start as usize..end as usize])
    }

    fn field(&self, i: usize) -> &[u8] {
        self.get(i).unwrap_or_default()
    }

    /// The same cells, cell `order[i]` as cell `i`.
    fn permuted(&self, order: &[u32]) -> Result<Cells> {
        let mut out = Cells {
            text: alloc::sized(self.text.len(), EXPORTED)?,
            ends: alloc::sized(order.len(), EXPORTED)?,
        };
        for &at in order {
            let at = at as usize;
            let end = self.ends[at];
            let start = match at {
                0 => 0,
                _ => self.ends[at - 1] & !ABSENT_CELL,
            };
            out.text
                .extend_from_slice(&self.text[start as usize..(end & !ABSENT_CELL) as usize]);
            out.ends.push(out.text.len() as u64 | (end & ABSENT_CELL));
        }
        Ok(out)
    }
}

/// One compared column's cells of the changed pairs: A's, then B's.
type Sides = (Cells, Cells);

/// Appends a cell as the text its [`value_of`] reads as, without the string,
/// and says whether it is present.
fn render(c: Look<'_>, opt: &Options, out: &mut Vec<u8>) -> Result<bool> {
    if c.null || c.bytes.is_empty() {
        return Ok(false);
    }
    if needs_normalising(opt) {
        let Some(v) = value_of(c, opt) else {
            return Ok(false);
        };
        alloc::room(out, v.len(), EXPORTED)?;
        out.extend_from_slice(v.as_bytes());
        return Ok(true);
    }
    match c.kind {
        Some(kind) => {
            // Every word writes out in fewer than forty-eight bytes.
            alloc::room(out, 48, EXPORTED)?;
            kind.write(u64::from_le_bytes(c.bytes.try_into().unwrap()), out);
        }
        // As `text_of` reads it: the bytes, unless they are not UTF-8.
        None => {
            let text = String::from_utf8_lossy(c.bytes);
            alloc::room(out, text.len(), EXPORTED)?;
            out.extend_from_slice(text.as_bytes());
        }
    }
    Ok(true)
}

/// A column's cells at `rows`, in that order.
fn cells_at(
    col: &Col<'_>,
    rows: impl ExactSizeIterator<Item = usize>,
    opt: &Options,
) -> Result<Cells> {
    let mut cells = Cells::with_room(rows.len())?;
    for r in rows {
        cells.push(col.at(r), opt)?;
    }
    Ok(cells)
}

/// The rows one export file lists, in key order.
///
/// A column's cells are read in the order the rows were found, which is the
/// order of the file they come from, and put into key order afterwards. Reading them in key
/// order instead jumps through the whole column once per cell, and at three
/// hundred thousand changed rows of five million that second pass took twice
/// as long as the first; reordering a column's few megabytes of written-out
/// text while they are still in cache costs a fraction of it.
struct Listed {
    /// Rows of one side, or for the changed file the matched pairs, in the
    /// order they were found.
    found: Vec<usize>,
    /// The file's row `i` is the one found at `order[i]`.
    order: Vec<u32>,
    /// The key cells, in the file's order.
    keys: Vec<Cells>,
}

impl Listed {
    /// Sorted the way [`compare_keys`] sorts every engine's export: by each key
    /// column's text, an absent value last. Text compares as its UTF-8 bytes,
    /// which is how `String` orders, and the cells were made valid UTF-8 when
    /// they were written out.
    fn new(
        found: Vec<usize>,
        key_side: &KeySide<'_>,
        row_of: impl Fn(usize) -> usize,
        opt: &Options,
    ) -> Result<Self> {
        let mut keys = Vec::with_capacity(key_side.col.len());
        for col in &key_side.col {
            keys.push(cells_at(col, found.iter().map(|&id| row_of(id)), opt)?);
        }
        let mut order: Vec<u32> = alloc::sized(found.len(), "the export's row order")?;
        order.extend(0..found.len() as u32);
        order.sort_by(|&x, &y| {
            for k in &keys {
                match (k.get(x as usize), k.get(y as usize)) {
                    (None, None) => continue,
                    (None, Some(_)) => return Ordering::Greater,
                    (Some(_), None) => return Ordering::Less,
                    (Some(a), Some(b)) => match a.cmp(b) {
                        Ordering::Equal => continue,
                        other => return other,
                    },
                }
            }
            Ordering::Equal
        });
        let keys = keys
            .iter()
            .map(|k| k.permuted(&order))
            .collect::<Result<Vec<_>>>()?;
        Ok(Listed { found, order, keys })
    }

    /// A column's cells at these rows, in the file's order; `row_of` turns
    /// what was found into a row of the column's file.
    fn cells(
        &self,
        col: &Col<'_>,
        row_of: impl Fn(usize) -> usize,
        opt: &Options,
    ) -> Result<Cells> {
        cells_at(col, self.found.iter().map(|&id| row_of(id)), opt)?.permuted(&self.order)
    }

    /// Writes the file: each row's key, then `cols` in turn.
    ///
    /// Quoting a field means scanning it, and at sixty thousand rows of
    /// thirty-eight fields that was a serial eighty milliseconds, as long as
    /// the second column pass. So rows are encoded in blocks on every lane, by
    /// the same writer with the same settings the header went through, and
    /// the blocks are written in order a wave at a time, which bounds what is
    /// held to one block per lane.
    fn write(&self, path: &Path, header: &[String], cols: &[&Cells], threads: usize) -> Result<()> {
        const ROWS: usize = 8192;
        let mut w = export_file(path, header)?;
        w.flush()?;
        let mut file = w
            .into_inner()
            .map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))?;
        let encode = |block: usize| -> Result<Vec<u8>> {
            let lo = block * ROWS;
            let hi = (lo + ROWS).min(self.order.len());
            let mut enc = csv::Writer::from_writer(Vec::new());
            let mut record: Vec<&[u8]> = Vec::with_capacity(header.len());
            for i in lo..hi {
                record.clear();
                record.extend(self.keys.iter().map(|k| k.field(i)));
                record.extend(cols.iter().map(|c| c.field(i)));
                enc.write_record(&record)?;
            }
            enc.into_inner()
                .map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))
        };
        let blocks = self.order.len().div_ceil(ROWS);
        let wave = threads.max(1);
        let mut first = 0;
        while first < blocks {
            let last = (first + wave).min(blocks);
            let encoded: Vec<Result<Vec<u8>>> = std::thread::scope(|scope| {
                let rest: Vec<_> = (first + 1..last)
                    .map(|b| parallel::spawn_at(scope, &encode, b))
                    .collect();
                let mut all = vec![encode(first)];
                all.extend(
                    rest.into_iter()
                        .map(|h| h.join().unwrap_or_else(|_| Err(panicked()))),
                );
                all
            });
            for block in encoded {
                file.write_all(&block?)
                    .map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))?;
            }
            first = last;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// What one compared column produced
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ColOut {
    changed: i64,
    blanked: i64,
    filled: i64,
    /// One bit per matched pair.
    bits: Vec<u64>,
    /// The first cap+1 pairs where this column differs, with their two values.
    /// Every pair that reaches the report is among these -- a row only reaches
    /// the report by being one of the first cap+1 *changed* pairs, and the
    /// pairs where this column differs are a subset of those, in the same order.
    held: Vec<(usize, Val, Val)>,
    added_vals: Vec<Val>,
    removed_vals: Vec<Val>,
    /// With `--export-dir`, every added and removed row's cell, in the order
    /// the files list them.
    export_added: Cells,
    export_removed: Cells,
    /// For `COMPARA_PHASES`: when the column's read began (seconds into the
    /// phase), how long it took for both files, and how long the comparison.
    began: f64,
    read: f64,
    compare: f64,
}

const BLOCK: usize = 4096;

/// The bytes of the thirty-two-or-sixty-four at `at` that are not zero, one bit
/// each. Compiled only where the build targets a vector unit, exactly as
/// [`turbo::field`] does it, so neither binary pays for a runtime check.
#[cfg(all(
    target_arch = "x86_64",
    any(target_feature = "avx2", target_feature = "avx512bw")
))]
#[inline(always)]
fn vector_nonzero(neq: &[u8], at: usize) -> u64 {
    #[cfg(target_feature = "avx512bw")]
    // Safety: the caller has checked `at + 64 <= neq.len()`, and the build
    // targets a CPU with AVX-512BW or this function is not compiled at all.
    unsafe {
        use std::arch::x86_64::*;
        let chunk = _mm512_loadu_si512(neq.as_ptr().add(at) as *const _);
        _mm512_test_epi8_mask(chunk, chunk)
    }
    #[cfg(not(target_feature = "avx512bw"))]
    // Safety: as above, with AVX2 and thirty-two bytes.
    unsafe {
        use std::arch::x86_64::*;
        let chunk = _mm256_loadu_si256(neq.as_ptr().add(at) as *const _);
        let zero = _mm256_cmpeq_epi8(chunk, _mm256_setzero_si256());
        !(_mm256_movemask_epi8(zero) as u32) as u64
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
const MASK_WIDTH: usize = 64;
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    not(target_feature = "avx512bw")
))]
const MASK_WIDTH: usize = 32;

/// The mismatch mask, read a register at a time -- the same scanner the CSV
/// path uses to find a delimiter, applied to finding a changed cell.
///
/// The arithmetic that makes this worth doing: about 0.7% of cells differ per
/// column, so an eight-byte word is all-zero 94.6% of the time, a 32-byte
/// vector 80%, a 64-byte one 64%. Every one of those is a correctly predicted
/// not-taken branch and eight, thirty-two or sixty-four cells retired. The work
/// on a *hit* is identical in all three -- one `trailing_zeros` per changed
/// cell -- so the only thing a wider register buys is fewer trips round the
/// skip path, which is where nearly all the loop is. That is the opposite trade
/// from the CSV scanner, where a wider register also means fewer loads, and it
/// is why both are measured rather than assumed.
fn scan_mask(neq: &[u8], base: usize, mut hit: impl FnMut(usize)) {
    let m = neq.len();
    let mut i = 0;
    #[cfg(all(
        target_arch = "x86_64",
        any(target_feature = "avx2", target_feature = "avx512bw")
    ))]
    while i + MASK_WIDTH <= m {
        let mut bits = vector_nonzero(neq, i);
        while bits != 0 {
            hit(base + i + bits.trailing_zeros() as usize);
            bits &= bits - 1;
        }
        i += MASK_WIDTH;
    }
    while i + 8 <= m {
        let mut w = u64::from_le_bytes(neq[i..i + 8].try_into().unwrap());
        while w != 0 {
            let byte = (w.trailing_zeros() >> 3) as usize;
            hit(base + i + byte);
            w &= !(0xFFu64 << (byte * 8));
        }
        i += 8;
    }
    while i < m {
        if neq[i] != 0 {
            hit(base + i);
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn map(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(|e| Error::new(format!("cannot read {path:?}: {e}")))?;
    // SAFETY: the file is opened read-only and not modified while mapped, which
    // is the same contract the `turbo` engine's Slab makes.
    unsafe { Mmap::map(&file) }.map_err(|e| Error::new(format!("cannot map {path:?}: {e}")))
}

pub fn compare(a_path: &Path, b_path: &Path, opt: &Options) -> Result<EngineResult> {
    let mut phases = Phases::new("");
    let a_map = map(a_path)?;
    let b_map = map(b_path)?;
    let a_name = a_path.display().to_string();
    let b_name = b_path.display().to_string();
    // The footer is parsed once per file; every column read below reuses it.
    let a_fm = parquet::read_file_meta(&a_map, &a_name)?;
    let b_fm = parquet::read_file_meta(&b_map, &b_name)?;
    let a_meta = parquet::read_meta(&a_fm);
    let b_meta = parquet::read_meta(&b_fm);

    let resolved = resolve(&a_meta.names, &b_meta.names, opt)?;
    let key_size = opt.key.len();
    let nc = resolved.compared.len();
    let cap = opt.max_rows;
    let exporting = opt.export_dir.is_some();

    let slot_of = |names: &[String], n: &str| names.iter().position(|c| c == n).unwrap();

    // What each column holds. Two columns are compared here only when both
    // files store them the same way -- as text, or as words of one kind -- since
    // a word's eight bytes and another file's text, or a FLOAT and a DOUBLE, are
    // not comparable by their bytes. Anything else goes to `turbo`, which
    // compares every value as the text it reads as.
    let kinds = |data: &[u8], names: &[String]| -> Result<Vec<ValueKind>> {
        match value_kinds(data) {
            Ok(k) if k.len() == names.len() => Ok(k),
            _ => Err(Error::unsupported(
                "the parquet schema is not one this path reads",
            )),
        }
    };
    let (a_kinds, b_kinds) = (kinds(&a_map, &a_meta.names)?, kinds(&b_map, &b_meta.names)?);
    let a_kind = |n: &str| a_kinds[slot_of(&a_meta.names, n)];
    let b_kind = |n: &str| b_kinds[slot_of(&b_meta.names, n)];
    for name in opt.key.iter().chain(&resolved.compared) {
        let (ka, kb) = (a_kind(name), b_kind(name));
        let same_text = ka.is_text() && kb.is_text();
        if !same_text && !(ka == kb && ka.is_word()) {
            return Err(Error::unsupported(format!(
                "{name} is not stored as text or as one word-sized type in both files"
            )));
        }
    }

    let threads = opt.thread_budget();

    // --- how many columns at once -----------------------------------------
    //
    // This path holds the key columns of both files, an index over each and
    // the matched pairs throughout, and then each compared column of both files
    // while it is in flight. Only the second part can be traded for time, so
    // the memory budget (`--memory`, `COMPARA_MEMORY`, or what the machine has
    // free) decides how many columns are decoded at once, down to one. A budget
    // someone stated that cannot hold even one is refused before anything is
    // read, rather than taken as a suggestion.
    let (a_rows, b_rows) = (a_meta.rows.max(0) as u64, b_meta.rows.max(0) as u64);
    let held = |meta: &parquet::Meta, n: &str| meta.held[slot_of(&meta.names, n)];
    let keys: u64 = opt
        .key
        .iter()
        .map(|k| held(&a_meta, k) + held(&b_meta, k))
        .sum();
    // Per row, the index's slot, first row, count and hash, and the hash of
    // every row before it is inserted; per matched pair, its two rows while the
    // ranges gather them and again once they are merged.
    let floor = keys + (a_rows + b_rows) * 36 + a_rows.min(b_rows) * 16;
    let per_column = resolved
        .compared
        .iter()
        .map(|n| held(&a_meta, n) + held(&b_meta, n))
        .max()
        .unwrap_or(0);
    if let Some(budget) = memory::stated(opt.memory)
        && floor + per_column > budget
    {
        return Err(Error::new(format!(
            "comparing these files needs about {} MB here (the key columns and their \
             index, and one column of each file at a time), more than the {} MB budget",
            (floor + per_column) >> 20,
            budget >> 20
        )));
    }
    let lanes = match per_column {
        0 => threads,
        per => (memory::available(opt.memory).saturating_sub(floor) / per)
            .clamp(1, threads.max(1) as u64) as usize,
    };

    // --- keys -------------------------------------------------------------
    //
    // Every key column of both files at once, one job each, then any column
    // both sides store as a dictionary is reduced to one shared id per row.
    //
    // One job per column and not one per side. These reads decode pages for
    // every row in the file and share nothing -- different files, different
    // columns, separate outputs -- so a side at a time is two jobs however wide
    // the key is, and the composite key this tool exists for is rarely one
    // column. Two jobs on a four-core machine is half of it idle for the whole
    // phase, and this phase reads as many bytes as any.
    //
    // Taken from a queue rather than one thread per job, the same way the
    // compared columns below are: the jobs are not the same size -- a
    // dictionary column and a plain one decode at different rates -- and
    // `--threads` has to mean something here too.
    let jobs: Vec<(bool, usize)> = (0..key_size)
        .flat_map(|j| [(false, j), (true, j)])
        .collect();
    let next = AtomicUsize::new(0);
    // Fewer key columns than threads leaves threads over, and each read
    // splits its row groups among its share of them.
    let key_ways = (threads / jobs.len().max(1)).max(1);
    let read_keys = || -> Vec<(usize, Result<parquet::Column>)> {
        let mut mine = Vec::new();
        loop {
            let at = next.fetch_add(1, AtomicOrdering::Relaxed);
            if at >= jobs.len() {
                return mine;
            }
            let (from_b, j) = jobs[at];
            let (data, fm, meta, name) = if from_b {
                (&b_map, &b_fm, &b_meta, &b_name)
            } else {
                (&a_map, &a_fm, &a_meta, &a_name)
            };
            mine.push((
                at,
                parquet::read_column(data, fm, slot_of(&meta.names, &opt.key[j]), name, key_ways),
            ));
        }
    };
    let mut read: Vec<Option<Result<parquet::Column>>> = (0..jobs.len()).map(|_| None).collect();
    {
        // Two lanes even on one thread: the two files are read at once, as
        // the text path sweeps them and the index build below builds them --
        // a side to a thread is the floor `--threads 1` gets everywhere, the
        // C port's reading of it too. One lane read the four key columns of a
        // 2M pair one after another, 0.14 s against the C port's 0.04 s.
        let lanes = threads.max(2).clamp(1, jobs.len().max(1));
        let gathered: Vec<Vec<(usize, Result<parquet::Column>)>> = std::thread::scope(|scope| {
            // One lane runs here rather than waiting on a thread of its own.
            let rest: Vec<_> = (1..lanes)
                .map(|_| parallel::spawn(scope, &read_keys))
                .collect();
            let mut all = vec![read_keys()];
            // A lane that panicked returns nothing, which leaves its jobs
            // `None` below -- so a panic is raised as an error rather than
            // silently dropping a column.
            all.extend(rest.into_iter().map(|h| h.join().unwrap_or_default()));
            all
        });
        for got in gathered {
            for (at, col) in got {
                read[at] = Some(col);
            }
        }
    }
    let mut a_key_cols: Vec<parquet::Column> = Vec::with_capacity(key_size);
    let mut b_key_cols: Vec<parquet::Column> = Vec::with_capacity(key_size);
    for (&(from_b, _), got) in jobs.iter().zip(read) {
        let col = got.unwrap_or_else(|| Err(panicked()))?;
        if from_b {
            b_key_cols.push(col);
        } else {
            a_key_cols.push(col);
        }
    }

    phases.mark("key columns (par)");

    let mut a_keys = KeySide {
        col: a_key_cols
            .into_iter()
            .zip(&opt.key)
            .map(|(c, name)| Col::new(c, &a_map, a_kind(name)))
            .collect(),
        id: vec![Vec::new(); key_size],
        rows: 0,
    };
    let mut b_keys = KeySide {
        col: b_key_cols
            .into_iter()
            .zip(&opt.key)
            .map(|(c, name)| Col::new(c, &b_map, b_kind(name)))
            .collect(),
        id: vec![Vec::new(); key_size],
        rows: 0,
    };
    a_keys.rows = a_keys.col.first().map(|c| c.rows()).unwrap_or(0);
    b_keys.rows = b_keys.col.first().map(|c| c.rows()).unwrap_or(0);
    for j in 0..key_size {
        if a_keys.col[j].rows() != a_keys.rows || b_keys.col[j].rows() != b_keys.rows {
            return Err(Error::new(
                "parquet columns disagree about how many rows the file has",
            ));
        }
    }

    let mut as_id = vec![false; key_size];
    for (j, coded) in as_id.iter_mut().enumerate() {
        if !a_keys.col[j].c.dictionary || !b_keys.col[j].c.dictionary {
            continue;
        }
        *coded = true;
        let mut ids = Ids::default();
        let a_map_j = code(&a_keys.col[j], &mut ids, opt)?;
        let b_map_j = code(&b_keys.col[j], &mut ids, opt)?;
        let apply = |col: &Col<'_>, m: &[i32]| -> Result<Vec<i32>> {
            let mut out = alloc::sized(col.c.index.len(), "one key id per row")?;
            out.extend(
                col.c
                    .index
                    .iter()
                    .map(|&k| if k < 0 { -1 } else { m[k as usize] }),
            );
            Ok(out)
        };
        a_keys.id[j] = apply(&a_keys.col[j], &a_map_j)?;
        b_keys.id[j] = apply(&b_keys.col[j], &b_map_j)?;
    }

    phases.mark("intern dicts (serial)");

    // --- the join ---------------------------------------------------------
    let per_side = (threads / 2).max(1);
    let build_b = || build_index(&as_id, &b_keys, opt, per_side);
    let (ai, bi) = std::thread::scope(|scope| {
        let bh = parallel::spawn(scope, &build_b);
        let a = build_index(&as_id, &a_keys, opt, per_side);
        (a, bh.join().unwrap_or_else(|_| Err(panicked())))
    });
    let (ai, bi) = (ai?, bi?);
    phases.mark("index build (2 ways)");

    // Both directions split over contiguous ranges of one side's distinct keys.
    // Each range accumulates into its own lists, and the ranges merge in order,
    // so which rows survive the report cap is the same as one thread would have
    // kept.
    #[derive(Default)]
    struct Part {
        pa: Vec<i32>,
        pb: Vec<i32>,
        held: Vec<i32>,
        total: i64,
        /// Every unmatched row, not only the first `cap + 1`, for the export.
        all: Vec<i32>,
    }
    let split = |keys_here: usize| {
        if keys_here < (1 << 14) {
            1
        } else {
            threads.max(1)
        }
    };
    let a_ways = split(ai.firsts.len());
    // The B pass is a second full random-probed pass over A's table, and it is
    // not what the counts come from: a key matches from either side or from
    // neither, so the number of B's keys with an A counterpart is exactly the
    // pair count the A pass produces, and `added` is B's distinct keys minus
    // it. What the pass is for is the *sample* -- the report's added section
    // names rows -- so it runs when something will read them and not otherwise.
    //
    // `compara.cpp` has followed that rule since it was measured there, and
    // `pqdiff.cpp` since #106. This path never asked. On a 2M Parquet pair it
    // is the whole of the join's gap against the C port: 0.108s against 0.047s
    // for the same answer.
    let b_ways = if opt.row_lists || exporting {
        split(bi.firsts.len())
    } else {
        0
    };
    // The two sweeps run concurrently in one scope, so a_ways + b_ways is the
    // live thread count; uncapped that is twice the budget whenever both
    // passes are active. Share the budget between the passes in proportion
    // to their key counts, keeping one way per active pass.
    let budget = threads.max(1);
    let (a_ways, b_ways) = if budget < 2 || a_ways + b_ways <= budget {
        (a_ways, b_ways)
    } else {
        let total = (ai.firsts.len() + bi.firsts.len()).max(1);
        let a = ((ai.firsts.len() * budget / total).max(1)).min(budget - 1);
        (a, budget - a)
    };

    // B's rows against A's, and A's against B's: the table's side first.
    let a_probes_b = KeyEq::new(&as_id, &b_keys, &a_keys, opt);
    let b_probes_a = KeyEq::new(&as_id, &a_keys, &b_keys, opt);
    let a_range = |p: usize| -> Result<Part> {
        let mut out = Part::default();
        let lo = ai.firsts.len() * p / a_ways;
        let hi = ai.firsts.len() * (p + 1) / a_ways;
        alloc::grow(&mut out.pa, hi - lo, "one matched row per key, A side")?;
        alloc::grow(&mut out.pb, hi - lo, "one matched row per key, B side")?;
        for at in lo..hi {
            if let Some(&soon) = ai.hashes.get(at + PREFETCH_AHEAD) {
                prefetch_slot(&bi.slots, bi.mask, soon);
            }
            if let Some(&near) = ai.hashes.get(at + PREFETCH_AHEAD / 2) {
                a_probes_b.prefetch_mate(&bi, near);
            }
            let row = ai.firsts[at];
            let mate = lookup(&bi, &a_probes_b, row as usize, ai.hashes[at]);
            if mate < 0 {
                out.total += 1;
                if out.held.len() <= cap {
                    out.held.push(row);
                }
                if exporting {
                    alloc::push(&mut out.all, row, "the removed rows")?;
                }
                continue;
            }
            out.pa.push(row);
            out.pb.push(mate);
        }
        Ok(out)
    };
    let b_range = |p: usize| -> Result<Part> {
        let mut out = Part::default();
        let lo = bi.firsts.len() * p / b_ways;
        let hi = bi.firsts.len() * (p + 1) / b_ways;
        for at in lo..hi {
            if let Some(&soon) = bi.hashes.get(at + PREFETCH_AHEAD) {
                prefetch_slot(&ai.slots, ai.mask, soon);
            }
            if let Some(&near) = bi.hashes.get(at + PREFETCH_AHEAD / 2) {
                b_probes_a.prefetch_mate(&ai, near);
            }
            let row = bi.firsts[at];
            if lookup(&ai, &b_probes_a, row as usize, bi.hashes[at]) >= 0 {
                continue;
            }
            out.total += 1;
            if out.held.len() <= cap {
                out.held.push(row);
            }
            if exporting {
                alloc::push(&mut out.all, row, "the added rows")?;
            }
        }
        Ok(out)
    };

    let (a_parts, b_parts) = std::thread::scope(|scope| {
        let ah: Vec<_> = (0..a_ways)
            .map(|p| parallel::spawn_at(scope, &a_range, p))
            .collect();
        let bh: Vec<_> = (0..b_ways)
            .map(|p| parallel::spawn_at(scope, &b_range, p))
            .collect();
        (
            ah.into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(panicked())))
                .collect::<Result<Vec<_>>>(),
            bh.into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(panicked())))
                .collect::<Result<Vec<_>>>(),
        )
    });
    let (a_parts, b_parts) = (a_parts?, b_parts?);

    phases.mark("match sweep (par)");

    let mut added = Capped::default();
    let mut added_found: Vec<usize> = Vec::new();
    for p in &b_parts {
        for &row in &p.held {
            added.push(row, cap);
        }
        added.total += p.total - p.held.len() as i64;
        alloc::room(&mut added_found, p.all.len(), "the added rows")?;
        added_found.extend(p.all.iter().map(|&r| r as usize));
    }

    let mut pair_a: Vec<i32> = Vec::new();
    let mut pair_b: Vec<i32> = Vec::new();
    let mut removed = Capped::default();
    let mut removed_found: Vec<usize> = Vec::new();
    {
        let total: usize = a_parts.iter().map(|p| p.pa.len()).sum();
        alloc::grow(&mut pair_a, total, "one matched pair per key, A side")?;
        alloc::grow(&mut pair_b, total, "one matched pair per key, B side")?;
        for p in &a_parts {
            pair_a.extend_from_slice(&p.pa);
            pair_b.extend_from_slice(&p.pb);
            for &row in &p.held {
                removed.push(row, cap);
            }
            removed.total += p.total - p.held.len() as i64;
            alloc::room(&mut removed_found, p.all.len(), "the removed rows")?;
            removed_found.extend(p.all.iter().map(|&r| r as usize));
        }
    }
    drop(a_parts);
    drop(b_parts);

    // Derived rather than counted when B was not walked. `held` stays empty,
    // which is what `row_lists` being false means: the count is exact either
    // way, and nothing will print the sample.
    if b_ways == 0 {
        added.total = bi.unique() - pair_a.len() as i64;
    }

    // The added and removed rows an export lists are known from the join, so
    // their cells are written out by the column pass below while each column
    // is in memory for its comparison anyway.
    let export_lists = if exporting {
        Some((
            Listed::new(added_found, &b_keys, |r| r, opt)?,
            Listed::new(removed_found, &a_keys, |r| r, opt)?,
        ))
    } else {
        None
    };

    let npairs = pair_a.len();
    let words = npairs.div_ceil(64);

    // --- the compared columns, one at a time ------------------------------
    //
    // Each worker owns whole columns, so nothing is shared but the pair arrays
    // and the two mappings, which are read-only from here on. How many are in
    // flight is a memory choice, not a parallelism one: a column of ten million
    // values costs a couple of hundred megabytes on each side while it is being
    // read, and it is released before the next is asked for. `lanes` is that
    // choice, made above.
    //
    // Fewer columns than threads would leave threads idle, with the largest
    // column -- read one side and then the other -- as the whole of the
    // phase. Each column in flight gets `per` threads instead: both sides read
    // at once, and each side's row groups split among its half. That can mean
    // fewer columns in flight than the budget allows, never more, and the
    // largest columns are taken first so that the last to finish is a small
    // one.
    let fit = lanes.clamp(1, nc.max(1));
    // On one thread, a column's two sides are read at once, by the same floor
    // as the key columns above. With two threads or more, two columns at a
    // time read a side after the other measured faster than one column with
    // its sides at once.
    let per = if threads <= 1 {
        2
    } else {
        threads.div_ceil(fit)
    };
    let lanes = fit.min(threads.max(1).div_ceil(per)).max(1);
    // The work of a column is the bytes its pages expand to, not what it
    // holds: a dictionary column holds a handle per row and decodes almost
    // nothing.
    let expanded = |meta: &parquet::Meta, n: &str| meta.expanded[slot_of(&meta.names, n)];
    let mut order: Vec<usize> = (0..nc).collect();
    order.sort_by_key(|&c| {
        let n = &resolved.compared[c];
        std::cmp::Reverse(expanded(&a_meta, n) + expanded(&b_meta, n))
    });
    let next = AtomicUsize::new(0);
    let compared = &resolved.compared;
    let (pa, pb) = (&pair_a, &pair_b);
    let (added_held, removed_held) = (&added.held, &removed.held);
    // The export's changed rows need every compared column's cell at the pairs
    // that changed, which are known only after every column is compared. Two
    // ways to have them then: read each column a second time, or keep the
    // decoded columns from the first pass. The second costs roughly rows × 8
    // bytes × 2 sides per word column while it is held, so it is taken only
    // when the memory budget leaves room for it with margin; otherwise the
    // export falls back to the second read.
    let stash_bytes: u64 = resolved
        .compared
        .iter()
        .map(|n| held(&a_meta, n) + held(&b_meta, n))
        .sum();
    let stash = exporting
        && memory::available(opt.memory).saturating_sub(floor) >= stash_bytes + stash_bytes / 4;
    let stashed: Vec<Mutex<Option<(Col<'_>, Col<'_>)>>> =
        (0..nc).map(|_| Mutex::new(None)).collect();
    let read_a = |c: usize, ways: usize| {
        parquet::read_column(
            &a_map,
            &a_fm,
            slot_of(&a_meta.names, &compared[c]),
            &a_name,
            ways,
        )
    };
    let read_b = |c: usize, ways: usize| {
        parquet::read_column(
            &b_map,
            &b_fm,
            slot_of(&b_meta.names, &compared[c]),
            &b_name,
            ways,
        )
    };
    let read_pair = |c: usize| -> Result<(Col<'_>, Col<'_>)> {
        let (a, b) = if per < 2 {
            (read_a(c, 1), read_b(c, 1))
        } else {
            let b_side = || read_b(c, per / 2);
            std::thread::scope(|scope| {
                let b = parallel::spawn(scope, &b_side);
                let a = read_a(c, per - per / 2);
                (a, b.join().unwrap_or_else(|_| Err(panicked())))
            })
        };
        let a_col = Col::new(a?, &a_map, a_kind(&compared[c]));
        let b_col = Col::new(b?, &b_map, b_kind(&compared[c]));
        if a_col.rows() != a_keys.rows || b_col.rows() != b_keys.rows {
            return Err(Error::new(
                "parquet columns disagree about how many rows the file has",
            ));
        }
        Ok((a_col, b_col))
    };

    let phase_start = std::time::Instant::now();
    let work = || -> Result<Vec<(usize, ColOut)>> {
        let mut mine = Vec::new();
        loop {
            let at = next.fetch_add(1, AtomicOrdering::Relaxed);
            if at >= nc {
                return Ok(mine);
            }
            let c = order[at];
            let mut out = ColOut {
                bits: alloc::filled(0u64, words, "one bit per pair in the column")?,
                ..ColOut::default()
            };
            let began = std::time::Instant::now();
            let (a_col, b_col) = read_pair(c)?;
            let read_done = std::time::Instant::now();

            // How a pair's two cells are told apart, decided once per column.
            enum Way<'c> {
                // Both sides dictionary encoded: the two dictionaries go into
                // one id space, and the per-row work is two gathers and an
                // integer compare.
                Coded {
                    amap: Vec<i32>,
                    bmap: Vec<i32>,
                },
                // Words of one kind on both sides: two cells read the same
                // exactly when their words are equal, so no cell is written
                // out to find that it did not change.
                Words {
                    aw: &'c parquet::Words,
                    bw: &'c parquet::Words,
                },
                // Words under --trim, --ignore-case, --empty-is-null or
                // --tolerance: each cell is compared as the text it reads as,
                // the way the CSV path compares it.
                WordsAsText,
                // Byte arrays, with the two buffers found once rather than per
                // cell: which one a slice belongs to is a property of the
                // column, and asking per access costs a branch and a bounds
                // check on every one of ten million rows.
                Bytes {
                    ab: &'c [u8],
                    bb: &'c [u8],
                },
                BytesNormalised {
                    ab: &'c [u8],
                    bb: &'c [u8],
                },
            }
            let way = if a_col.c.dictionary && b_col.c.dictionary && opt.tolerance == 0.0 {
                let mut ids = Ids::default();
                let amap = code(&a_col, &mut ids, opt)?;
                let bmap = code(&b_col, &mut ids, opt)?;
                Way::Coded { amap, bmap }
            } else if let (Some(aw), Some(bw), false) =
                (&a_col.c.words, &b_col.c.words, needs_normalising(opt))
            {
                Way::Words { aw, bw }
            } else if a_col.c.words.is_some() || b_col.c.words.is_some() {
                Way::WordsAsText
            } else if !needs_normalising(opt) {
                Way::Bytes {
                    ab: a_col.base(),
                    bb: b_col.base(),
                }
            } else {
                Way::BytesNormalised {
                    ab: a_col.base(),
                    bb: b_col.base(),
                }
            };

            // One block of pairs from `base`: a byte per pair, set where the
            // column differs.
            let fill = |base: usize, neq: &mut [u8]| {
                let m = neq.len();
                let (xs, ys) = (&pa[base..base + m], &pb[base..base + m]);
                match &way {
                    Way::Coded { amap, bmap } => {
                        let (aix, bix) = (&a_col.c.index, &b_col.c.index);
                        for (i, ne) in neq.iter_mut().enumerate() {
                            // Safety: every pair row is a row of its side's
                            // column, every non-negative index was validated
                            // against its dictionary at decode, and a map
                            // holds one id per entry.
                            let (ka, kb) = unsafe {
                                (
                                    *aix.get_unchecked(*xs.get_unchecked(i) as usize),
                                    *bix.get_unchecked(*ys.get_unchecked(i) as usize),
                                )
                            };
                            // Safety: see above.
                            let xa = if ka < 0 {
                                -1
                            } else {
                                unsafe { *amap.get_unchecked(ka as usize) }
                            };
                            // Safety: see above.
                            let xb = if kb < 0 {
                                -1
                            } else {
                                unsafe { *bmap.get_unchecked(kb as usize) }
                            };
                            *ne = (xa != xb) as u8;
                        }
                    }
                    Way::Words { aw, bw } => {
                        for i in 0..m {
                            if let Some(&soon) = ys.get(i + PREFETCH_AHEAD) {
                                prefetch_at(bw.values.as_ptr().wrapping_add(soon as usize).cast());
                            }
                            neq[i] = (aw.values[xs[i] as usize] != bw.values[ys[i] as usize]) as u8;
                        }
                        if !aw.nulls.is_empty() || !bw.nulls.is_empty() {
                            for i in 0..m {
                                neq[i] |= (aw.is_null(xs[i] as usize) != bw.is_null(ys[i] as usize))
                                    as u8;
                            }
                        }
                    }
                    Way::WordsAsText => {
                        for i in 0..m {
                            neq[i] = cell_differs(
                                a_col.at(xs[i] as usize),
                                b_col.at(ys[i] as usize),
                                opt,
                            ) as u8;
                        }
                    }
                    Way::Bytes { ab, bb } => {
                        // B's rows come in no order, and each costs two
                        // misses: its handle, then its bytes. Both are
                        // fetched ahead, the bytes a stage behind the handle,
                        // by when the handle is in cache.
                        let handle = |row: usize| -> *const u8 {
                            if b_col.c.dictionary {
                                b_col.c.index.as_ptr().wrapping_add(row).cast()
                            } else {
                                b_col.c.values.as_ptr().wrapping_add(row).cast()
                            }
                        };
                        for i in 0..m {
                            if let Some(&soon) = ys.get(i + PREFETCH_AHEAD) {
                                prefetch_at(handle(soon as usize));
                            }
                            if let Some(&near) = ys.get(i + PREFETCH_AHEAD / 2) {
                                let y = b_col.slice_at(near as usize);
                                if !y.is_null() {
                                    prefetch_at(bb.as_ptr().wrapping_add(y.offset()));
                                }
                            }
                            let x = a_col.slice_at(xs[i] as usize);
                            let y = b_col.slice_at(ys[i] as usize);
                            let (xa, ya) = (
                                x.is_null() || x.length() == 0,
                                y.is_null() || y.length() == 0,
                            );
                            neq[i] = ((xa != ya)
                                || (!xa
                                    && !ya
                                    && ab[x.offset()..x.offset() + x.length()]
                                        != bb[y.offset()..y.offset() + y.length()]))
                                as u8;
                        }
                    }
                    Way::BytesNormalised { ab, bb } => {
                        for i in 0..m {
                            neq[i] = cell_differs(
                                look(ab, a_col.slice_at(xs[i] as usize)),
                                look(bb, b_col.slice_at(ys[i] as usize)),
                                opt,
                            ) as u8;
                        }
                    }
                }
            };
            // The pairs in `lo..hi`, a block at a time: where the column
            // differs, in pair order.
            let scan = |lo: usize, hi: usize| -> Result<Vec<usize>> {
                let mut hits: Vec<usize> = Vec::new();
                let mut neq = vec![0u8; BLOCK];
                let mut base = lo;
                while base < hi {
                    let m = BLOCK.min(hi - base);
                    fill(base, &mut neq[..m]);
                    alloc::room(&mut hits, m, "the changed cells in one column")?;
                    scan_mask(&neq[..m], base, |p| hits.push(p));
                    base += m;
                }
                Ok(hits)
            };
            // The column's threads, which read its two sides, compare it too,
            // each a run of whole blocks; the runs' hits are joined in order,
            // so the result is the single pass's.
            let ways = if npairs >= 1 << 16 { per.max(1) } else { 1 };
            let hits = if ways == 1 {
                scan(0, npairs)?
            } else {
                let step = npairs.div_ceil(ways).div_ceil(BLOCK) * BLOCK;
                let part = |k: usize| scan((k * step).min(npairs), ((k + 1) * step).min(npairs));
                let parts: Vec<Result<Vec<usize>>> = std::thread::scope(|scope| {
                    let others: Vec<_> = (1..ways)
                        .map(|k| parallel::spawn_at(scope, &part, k))
                        .collect();
                    let mut all = vec![part(0)];
                    all.extend(
                        others
                            .into_iter()
                            .map(|h| h.join().unwrap_or_else(|_| Err(panicked()))),
                    );
                    all
                });
                let mut hits = Vec::new();
                for p in parts {
                    let p = p?;
                    alloc::room(&mut hits, p.len(), "the changed cells in one column")?;
                    hits.extend(p);
                }
                hits
            };

            for p in hits {
                let x = a_col.at(pa[p] as usize);
                let y = b_col.at(pb[p] as usize);
                out.changed += 1;
                if absent(y, opt) {
                    out.blanked += 1;
                }
                if absent(x, opt) {
                    out.filled += 1;
                }
                out.bits[p >> 6] |= 1u64 << (p & 63);
                if out.held.len() <= cap {
                    out.held.push((p, value_of(x, opt), value_of(y, opt)));
                }
            }

            // The rows that reach the report as added or removed need every
            // column's value, and this is the only time this column is in
            // memory.
            out.added_vals = added_held
                .iter()
                .map(|&r| value_of(b_col.at(r as usize), opt))
                .collect();
            out.removed_vals = removed_held
                .iter()
                .map(|&r| value_of(a_col.at(r as usize), opt))
                .collect();
            if let Some((added_list, removed_list)) = &export_lists {
                out.export_added = added_list.cells(&b_col, |r| r, opt)?;
                out.export_removed = removed_list.cells(&a_col, |r| r, opt)?;
            }
            // Kept for the export's gather below instead of dropped: with the
            // changed pairs known, their cells come out of these columns with
            // no second read of the file.
            if stash {
                *stashed[c].lock().unwrap_or_else(|e| e.into_inner()) = Some((a_col, b_col));
            }
            out.began = (began - phase_start).as_secs_f64();
            out.read = (read_done - began).as_secs_f64();
            out.compare = read_done.elapsed().as_secs_f64();
            mine.push((c, out));
        }
    };

    let mut cols: Vec<ColOut> = (0..nc).map(|_| ColOut::default()).collect();
    {
        let gathered: Vec<Result<Vec<(usize, ColOut)>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..lanes).map(|_| parallel::spawn(scope, &work)).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(panicked())))
                .collect()
        });
        for got in gathered {
            for (c, out) in got? {
                cols[c] = out;
            }
        }
    }

    phases.mark("compared columns (par)");
    if phases.on() {
        phases.detail(&format!("{lanes} at once, {per} thread(s) each"));
        for &c in &order {
            let o = &cols[c];
            phases.detail(&format!(
                "{:<20} from {:.3}s  read {:.3}s  compare {:.3}s",
                compared[c], o.began, o.read, o.compare
            ));
        }
    }

    let columns: Vec<ColumnStat> = compared
        .iter()
        .zip(&cols)
        .map(|(name, c)| ColumnStat {
            name: name.clone(),
            changed: c.changed,
            blanked: c.blanked,
            filled: c.filled,
        })
        .collect();

    // --- which pairs changed ---------------------------------------------
    let mut any = alloc::filled(0u64, words, "one bit per pair")?;
    for c in &cols {
        for (w, v) in any.iter_mut().zip(&c.bits) {
            *w |= *v;
        }
    }
    let mut changed_total = 0i64;
    let mut keep: Vec<usize> = Vec::new(); // the first cap+1 changed pairs, in pair order
    for (w, &word) in any.iter().enumerate() {
        changed_total += word.count_ones() as i64;
        let mut bits = word;
        while bits != 0 && keep.len() <= cap {
            let bit = bits.trailing_zeros() as usize;
            keep.push(w * 64 + bit);
            bits &= bits - 1;
        }
    }

    // --- the export -------------------------------------------------------
    if let Some((added_list, removed_list)) = &export_lists {
        // Which pairs changed is known only now, after every column.
        let mut changed_found: Vec<usize> =
            alloc::sized(changed_total as usize, "the changed pairs")?;
        for (w, &word) in any.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                changed_found.push(w * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
        let changed_list = Listed::new(changed_found, &a_keys, |p| pair_a[p] as usize, opt)?;
        let mut sides: Vec<Sides> = (0..nc).map(|_| Default::default()).collect();
        if !changed_list.found.is_empty() {
            if stash {
                // The columns stayed in memory: each changed pair's cells come
                // out of them directly, and each column is released as it is
                // gathered.
                for (sides_c, slot) in sides.iter_mut().zip(stashed.iter()) {
                    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                    let Some((a_col, b_col)) = guard.take() else {
                        return Err(Error::new("a stashed column went missing"));
                    };
                    drop(guard);
                    let mut a = Cells::with_room(changed_list.found.len())?;
                    let mut b = Cells::with_room(changed_list.found.len())?;
                    for &p in &changed_list.found {
                        a.push(a_col.at(pa[p] as usize), opt)?;
                        b.push(b_col.at(pb[p] as usize), opt)?;
                    }
                    // As `Listed::cells` does: the file lists rows in key
                    // order, not pair order.
                    let a = a.permuted(&changed_list.order)?;
                    let b = b.permuted(&changed_list.order)?;
                    *sides_c = (a, b);
                }
                phases.mark("export cells (stash)");
            } else {
                // No room to keep the columns: each is read again, on as many
                // lanes as the first pass had, and written out at the changed
                // pairs alone.
                let next = AtomicUsize::new(0);
                let again = || -> Result<Vec<(usize, Sides)>> {
                    let mut mine = Vec::new();
                    loop {
                        let c = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if c >= nc {
                            return Ok(mine);
                        }
                        let (a_col, b_col) = read_pair(c)?;
                        let a = changed_list.cells(&a_col, |p| pa[p] as usize, opt)?;
                        let b = changed_list.cells(&b_col, |p| pb[p] as usize, opt)?;
                        mine.push((c, (a, b)));
                    }
                };
                let gathered: Vec<Result<Vec<(usize, Sides)>>> = std::thread::scope(|scope| {
                    let handles: Vec<_> =
                        (0..lanes).map(|_| parallel::spawn(scope, &again)).collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap_or_else(|_| Err(panicked())))
                        .collect()
                });
                for got in gathered {
                    for (c, pair) in got? {
                        sides[c] = pair;
                    }
                }
                phases.mark("export cells (par)");
            }
        }

        let dir = Path::new(opt.export_dir.as_deref().unwrap_or_default());
        create_export_dir(dir)?;
        let (plain, both) = export_headers(opt, compared);
        let added_cols: Vec<&Cells> = cols.iter().map(|c| &c.export_added).collect();
        added_list.write(&dir.join("added.csv"), &plain, &added_cols, threads)?;
        let removed_cols: Vec<&Cells> = cols.iter().map(|c| &c.export_removed).collect();
        removed_list.write(&dir.join("removed.csv"), &plain, &removed_cols, threads)?;
        let changed_cols: Vec<&Cells> = sides.iter().flat_map(|(a, b)| [a, b]).collect();
        changed_list.write(&dir.join("changed.csv"), &both, &changed_cols, threads)?;
        phases.mark("export write");
    }
    drop(export_lists);
    for c in &mut cols {
        c.export_added = Cells::default();
        c.export_removed = Cells::default();
    }

    // --- the report -------------------------------------------------------
    let key_values = |s: &KeySide<'_>, row: i32| -> Vec<Val> {
        (0..key_size)
            .map(|j| value_of(s.col[j].at(row as usize), opt))
            .collect()
    };

    // Nothing is decoded when the caller asked for no rows. The counts above are
    // already final -- `changed_total` is the population count of the mismatch
    // mask, not the length of anything kept -- so this only drops what a report
    // would have shown.
    let keep: &[usize] = if opt.row_lists { &keep } else { &[] };
    let mut rows: Vec<(Vec<Val>, Vec<CellDiff>)> = keep
        .iter()
        .map(|&p| (key_values(&a_keys, pair_a[p]), Vec::new()))
        .collect();
    for (c, out) in cols.iter_mut().enumerate() {
        for (p, x, y) in std::mem::take(&mut out.held) {
            if let Ok(i) = keep.binary_search(&p) {
                rows[i].1.push(CellDiff {
                    column: c,
                    a: x,
                    b: y,
                });
            }
        }
    }
    rows.sort_by(|x, y| compare_keys(&x.0, &y.0, key_size));
    // Not truncated here: `Section::capped` takes the cap and reads the one
    // extra row as the signal that there were more.
    let changed_rows: Vec<Vec<Cell>> = rows
        .into_iter()
        .map(|(key, diffs)| {
            let mut row: Vec<Cell> = key.into_iter().map(Cell::Value).collect();
            row.push(Cell::Diffs(diffs));
            row
        })
        .collect();

    let full_row = |s: &KeySide<'_>, row: i32, at: usize, from_b: bool| -> Vec<Val> {
        let mut out = key_values(s, row);
        out.reserve(nc);
        for c in &cols {
            out.push(if from_b {
                c.added_vals[at].clone()
            } else {
                c.removed_vals[at].clone()
            });
        }
        out
    };
    const NONE: &[i32] = &[];
    let (keep_added, keep_removed) = if opt.row_lists {
        (added.held.as_slice(), removed.held.as_slice())
    } else {
        (NONE, NONE)
    };
    let mut added_rows: Vec<Vec<Val>> = keep_added
        .iter()
        .enumerate()
        .map(|(i, &r)| full_row(&b_keys, r, i, true))
        .collect();
    let mut removed_rows: Vec<Vec<Val>> = keep_removed
        .iter()
        .enumerate()
        .map(|(i, &r)| full_row(&a_keys, r, i, false))
        .collect();
    added_rows.sort_by(|x, y| compare_keys(x, y, key_size));
    removed_rows.sort_by(|x, y| compare_keys(x, y, key_size));

    let to_cells = |r: Vec<Val>| -> Vec<Cell> { r.into_iter().map(Cell::Value).collect() };

    let dup_section = |s: &KeySide<'_>, ix: &Index| -> Section {
        let mut all: Vec<(Vec<Val>, i64)> = if ix.extra.is_empty() {
            Vec::new()
        } else {
            ix.firsts
                .iter()
                .filter(|&&r| ix.extra[r as usize] > 0)
                .map(|&r| (key_values(s, r), 1 + ix.extra[r as usize] as i64))
                .collect()
        };
        all.sort_by(|x, y| {
            y.1.cmp(&x.1)
                .then_with(|| compare_keys(&x.0, &y.0, key_size))
        });
        let total = all.len();
        all.truncate(cap.min(total));
        let mut cols_out = opt.key.clone();
        cols_out.push("count".to_string());
        Section {
            cols: cols_out,
            rows: all
                .into_iter()
                .map(|(k, n)| {
                    let mut row: Vec<Cell> = k.into_iter().map(Cell::Value).collect();
                    row.push(Cell::Count(n));
                    row
                })
                .collect(),
            truncated: total > cap,
        }
    };
    let empty_dups = || {
        let mut cols_out = opt.key.clone();
        cols_out.push("count".to_string());
        Section::capped(cols_out, Vec::new(), cap)
    };
    let (dup_a, dup_b) = if opt.row_lists {
        (dup_section(&a_keys, &ai), dup_section(&b_keys, &bi))
    } else {
        (empty_dups(), empty_dups())
    };

    let matched = npairs as i64;
    let counts = Counts {
        a_rows: ai.rows,
        b_rows: bi.rows,
        a_keys: ai.unique(),
        b_keys: bi.unique(),
        matched,
        unchanged: matched - changed_total,
        changed: changed_total,
        added: added.total,
        removed: removed.total,
        a_dup_keys: ai.dup_keys,
        a_dup_rows: ai.dup_rows,
        b_dup_keys: bi.dup_keys,
        b_dup_rows: bi.dup_rows,
    };

    let joined = Joined {
        counts,
        columns,
        changed: changed_rows,
        added: added_rows.into_iter().map(to_cells).collect(),
        removed: removed_rows.into_iter().map(to_cells).collect(),
        changed_a: Vec::new(),
        changed_b: Vec::new(),
    };
    phases.mark("assemble");
    let meta = resolved.meta(&opt.key, a_meta.names.len(), b_meta.names.len());
    Ok(sections(
        meta,
        joined,
        dup_a,
        dup_b,
        opt,
        &resolved.compared,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_word_is_fold_bytes_of_the_word() {
        for (h, w) in [
            (SEED, [0u8; 8]),
            (1, [1, 2, 3, 4, 5, 6, 7, 8]),
            (u64::MAX, [0xff; 8]),
        ] {
            assert_eq!(fold_word(h, &w), fold_bytes(h, &w));
        }
    }

    #[test]
    fn tail_word_is_the_zero_padded_tail() {
        let buf: Vec<u8> = (0..40u32).map(|i| (0x21 + i * 7 % 200) as u8).collect();
        for n in 1..=buf.len() {
            let v = &buf[..n];
            let rem = n % 8;
            if rem == 0 {
                continue;
            }
            let mut padded = [0u8; 8];
            padded[..rem].copy_from_slice(&v[n - rem..]);
            assert_eq!(tail_word(v, rem), u64::from_le_bytes(padded), "length {n}");
        }
    }

    /// The word-at-a-time key compare agrees with slice equality at every
    /// length, for equal pairs and pairs differing in any one byte.
    #[test]
    fn bytes_eq_is_slice_equality() {
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for n in 0..40usize {
            for _ in 0..200 {
                let p: Vec<u8> = (0..n).map(|_| (next() % 4) as u8 + b'a').collect();
                let mut q = p.clone();
                assert!(bytes_eq(&p, &q), "{p:?}");
                if n > 0 {
                    let at = (next() % n as u64) as usize;
                    q[at] = (next() % 4) as u8 + b'a';
                    assert_eq!(bytes_eq(&p, &q), p == q, "{p:?} {q:?}");
                }
                let longer = [p.as_slice(), b"a"].concat();
                assert!(!bytes_eq(&p, &longer));
            }
        }
    }
}
