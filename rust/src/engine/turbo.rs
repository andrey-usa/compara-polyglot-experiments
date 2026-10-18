//! The byte-level engine: map the file, index it in place, build no string for a
//! cell unless that cell reaches the report.
//!
//! Every other in-memory engine here reads a row into owned `String`s and hashes
//! one of them into a map. On twenty columns and a million rows that is twenty
//! million allocations, and all but the few thousand the report embeds are
//! thrown away. This engine keeps a field as an offset and a length into the
//! mapped bytes; hashing and comparison read those bytes in place, and only the
//! capped report sections are ever decoded.
//!
//! That is the same design as the Java `turbo` engine and the C, C++ and Zig
//! ports, and it exists here for a specific reason: Java `turbo` beats Rust
//! `native` by nearly six times on time and four on memory, which reads as a
//! language result and is not one. With both designs in one language the
//! comparison is honest.
//!
//! Three things are layered on that base, and each is worth a sentence:
//!
//! - **Three input formats.** CSV, newline-delimited JSON and Parquet all reduce
//!   to the same field word, so the two sides of a comparison need not be in the
//!   same format: a CSV export compares against the Parquet a warehouse emits.
//!   See [`text`] and [`parquet`].
//! - **Every core.** Each file is split at row boundaries, parsed and hashed in
//!   parallel, and inserted into its index in file order; the join then splits
//!   over contiguous ranges of A's keys. Ordering is preserved where it is
//!   load-bearing, so the answer does not depend on the thread count.
//! - **Normalisation is delegated rather than reimplemented.** With `--trim`,
//!   `--ignore-case`, `--empty-is-null` or a tolerance in play a field is decoded
//!   and handed to the same [`crate::columns`] functions every other engine uses,
//!   so the answer cannot drift; without them the raw bytes are compared and
//!   hashed directly, which is the path the benchmarks take.
//!
//! Hashing and equality always read the same bytes by the same route — the
//! property whose absence produced two silently wrong answers in the Java port.

pub(crate) mod codec;
mod encoding;
mod field;
pub(super) mod parquet;
mod slab;
mod text;
mod thrift;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use field::{ABSENT, Field, MAX_FIELD_LEN, TOO_LONG, count_byte, next_of1};
use slab::{Dialect, Slab, same_bytes, text_of, text_of_checked};
use text::{
    RowParser, Runs, csv_header, detect_delimiter, guard_span, json_header, json_is_array,
    json_rows_match, shared_tail, sniff_dialect,
};

use crate::alloc;
use crate::columns::{Resolved, Typed, compare_keys, differs, empty_to_null, normalise, resolve};
use crate::contract::{Cell, CellDiff, ColumnStat, Counts, EngineResult, Section, Val};
use crate::error::{Error, Result};
use crate::options::Options;
use crate::parallel;
use crate::phases::Phases;
use crate::rowstore::Joined;
use crate::sections::assemble;

/// Below this there is nothing to divide: finding the chunk boundaries would
/// cost more than the parsing it splits.
const CHUNKING_THRESHOLD: usize = 4 << 20;

/// How many keys make the join worth splitting.
const JOIN_THRESHOLD: usize = 1 << 14;

/// How many rows ahead a table probe is started. Enough misses in flight to
/// cover the latency of one, and not so many that the lines are evicted before
/// the loop reaches them.
const PREFETCH_AHEAD: usize = 32;

/// Keys per join chunk.
///
/// Sized in rows rather than in threads so that the chunk boundaries -- and so
/// which rows a capped section keeps -- are the same at any `--threads`. Small
/// enough that no single chunk can be the last thing four cores are waiting on:
/// at ten million keys this is a couple of hundred chunks of a few hundredths
/// of a second each.
const JOIN_CHUNK: usize = 1 << 16;

// ---------------------------------------------------------------------------
// One file, read into the representation the join works on
// ---------------------------------------------------------------------------

/// How a file's rows are addressed.
enum Rows {
    /// Text: a row is an offset into the mapping, re-parsed on demand. The index
    /// stores where a row starts rather than its fields, because an offset is
    /// eight bytes where the fields would be twenty times that, and re-parsing is
    /// cheap because the parser stops at the last needed column.
    Text {
        parser: RowParser,
        /// The same parser configured with the key columns alone, for the sweep.
        ///
        /// The sweep reads every row of the file and wants two things from each:
        /// its hash, which is computed from the key columns, and where the next
        /// row starts. It has never wanted the other seventeen, and packing them
        /// was most of what it cost -- a parser stops at the last column it was
        /// asked for, and asking for less makes the rest of the row a plain scan
        /// for the newline rather than a field-by-field walk.
        keys: RowParser,
        from: usize,
    },
    /// Parquet: there is no row to re-read, so the fields are materialised once,
    /// row-major, and a row is an index into them.
    Columnar { fields: Vec<Field>, rows: usize },
}

/// One file: the bytes its fields point into, and how to get a row's fields.
struct Side {
    slab: Slab,
    rows: Rows,
    width: usize,
    /// Per field slot, what this file stores (a typed Parquet column renders
    /// one canonical spelling of each value; text is just text).
    typed: Vec<Typed>,
}

impl Side {
    /// The fields of the row addressed by `at` — a byte offset for a text file,
    /// a row number for a columnar one.
    fn fields_at(&self, at: u64, out: &mut [Field]) {
        match &self.rows {
            Rows::Text { parser, .. } => {
                let data = self.slab.data();
                parser.parse(data, at as usize, data.len(), out);
            }
            Rows::Columnar { fields, .. } => {
                let from = at as usize * self.width;
                out.copy_from_slice(&fields[from..from + self.width]);
            }
        }
    }

    /// The row parser, when rows are text and there is one.
    fn parser(&self) -> Option<&RowParser> {
        match &self.rows {
            Rows::Text { parser, .. } => Some(parser),
            Rows::Columnar { .. } => None,
        }
    }

    /// The key columns of that row, and nothing else.
    ///
    /// The whole row is eighteen more fields than a key comparison reads, and a
    /// parser stops at the last column it was asked for -- so asking for the two
    /// key columns turns the rest of the row into one scan for the newline
    /// instead of eighteen field boundaries packed into words nobody looks at.
    /// This is the same saving the sweep already takes, in the other half of the
    /// engine that only ever wanted a key.
    ///
    /// Fills `out[..key_size]` and leaves the rest of the buffer as it was:
    /// every caller reads the key columns alone.
    fn keys_at(&self, at: u64, key_size: usize, out: &mut [Field]) {
        match &self.rows {
            Rows::Text { keys, .. } => {
                let data = self.slab.data();
                keys.parse(data, at as usize, data.len(), out);
            }
            Rows::Columnar { fields, .. } => {
                let from = at as usize * self.width;
                out[..key_size].copy_from_slice(&fields[from..from + key_size]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Values: raw bytes on the fast path, the shared functions on the slow one
// ---------------------------------------------------------------------------

/// Whether the options ask for anything the raw bytes cannot answer.
fn needs_normalising(opt: &Options) -> bool {
    opt.trim || opt.ignore_case || opt.empty_is_null || opt.tolerance > 0.0
}

/// The field as the `Val` every other engine would have built for it.
fn value(slab: &Slab, f: Field, opt: &Options) -> Val {
    if f == ABSENT {
        return None;
    }
    normalise(empty_to_null(&text_of(slab, f)), opt)
}

/// [`value`] that refuses rather than aborting when there is no room.
///
/// The report path uses this one; see `text_of_checked` for why the comparison
/// path does not. `empty_to_null` and `normalise` are re-expressed here rather
/// than called because both take an owned `Val` and hand back another --
/// `s.trim().to_string()` is a second allocation of a string this already owns
/// -- and because they are shared with the other two engines, which this change
/// does not touch.
fn value_checked(slab: &Slab, f: Field, opt: &Options, what: &str) -> Result<Val> {
    if f == ABSENT {
        return Ok(None);
    }
    let s = text_of_checked(slab, f, what)?;
    if s.is_empty() {
        return Ok(None);
    }
    if !needs_normalising(opt) {
        return Ok(Some(s));
    }
    let trimmed = if opt.trim { s.trim() } else { s.as_str() };
    if opt.empty_is_null && trimmed.is_empty() {
        return Ok(None);
    }
    if opt.ignore_case {
        // `to_lowercase` allocates on its own and can grow the string, so it
        // gets its own checked buffer rather than being done in place.
        let lower = trimmed.to_lowercase();
        return alloc::val(Some(lower.as_str()), what);
    }
    if opt.trim {
        return alloc::val(Some(trimmed), what);
    }
    Ok(Some(s))
}

fn is_absent(slab: &Slab, f: Field, opt: &Options) -> bool {
    if f == ABSENT || field::len_of(f) == 0 || f == TOO_LONG {
        return true;
    }
    if !needs_normalising(opt) {
        return false;
    }
    value(slab, f, opt).is_none()
}

fn same(a: &Slab, x: Field, b: &Slab, y: Field, opt: &Options) -> bool {
    if !needs_normalising(opt) {
        // Fast path: `plain_absent` is the field-word check without the
        // redundant `needs_normalising` call that `is_absent` would do.
        let (xa, yb) = (plain_absent(x), plain_absent(y));
        if xa || yb {
            return xa && yb;
        }
        return same_bytes(a, x, b, y);
    }
    value(a, x, opt) == value(b, y, opt)
}

/// Whether a field is absent, when nothing has to be normalised first.
///
/// The general `is_absent` may decode the value; with no normalising it is two
/// bit tests on the field word, and the join asks it three times per compared
/// cell -- once inside `same`, then twice more for the blanked and filled
/// counters, having thrown the first answer away.
#[inline]
fn plain_absent(f: Field) -> bool {
    !field::is_real(f) || field::len_of(f) == 0
}

/// `cell_differs` for the common case, with each side's absence already known.
/// Both absent is not a difference; exactly one is.
#[inline]
fn plain_differs(a: &Slab, x: Field, xa: bool, b: &Slab, y: Field, yb: bool) -> bool {
    if xa || yb {
        return xa != yb;
    }
    !same_bytes(a, x, b, y)
}

/// Whether two present cells, unequal as text, hold the same typed value.
fn same_value(kind: Typed, a: &Slab, x: Field, b: &Slab, y: Field) -> bool {
    fn bytes(slab: &Slab, f: Field) -> std::borrow::Cow<'_, [u8]> {
        if field::is_escaped(f) {
            slab.logical(f).collect::<Vec<u8>>().into()
        } else {
            slab.raw(f).into()
        }
    }
    kind.same_value(&bytes(a, x), &bytes(b, y))
}

fn cell_differs(a: &Slab, x: Field, b: &Slab, y: Field, opt: &Options) -> bool {
    if !needs_normalising(opt) {
        return !same(a, x, b, y, opt);
    }
    differs(&value(a, x, opt), &value(b, y, opt), opt)
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
fn hash_bytes(bytes: &[u8], seed: u64) -> u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    let mut h = seed;
    let mut chunks = bytes.as_chunks::<8>().0.iter();
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(*chunk);
        h = (h ^ word).wrapping_mul(PRIME);
        // The xor-shift is what spreads a whole word into the low bits, which
        // is where the table's slot comes from.
        h ^= h >> 29;
    }
    let rem = bytes.len() % 8;
    if rem != 0 {
        // The Parquet path's `tail_word`: the zero-padded tail from loads that
        // stay inside the bytes, rather than a copy of a run-time length --
        // a `memcpy` call per key column of every row of both files. It is the
        // word the escaped branch of `hash_field` assembles byte by byte.
        h = (h ^ super::pqdiff::tail_word(bytes, rem)).wrapping_mul(PRIME);
        h ^= h >> 29;
    }
    h
}

/// FNV-1a over the bytes equality would compare, so the two cannot disagree.
fn hash_field(slab: &Slab, f: Field, opt: &Options, normalise: bool, seed: u64) -> u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    let mut h = seed;
    // Fast path: no normalization options set, so absence is just the field
    // word. The `needs_normalising` check was done once per sweep, not once
    // per field per row.
    if !normalise {
        if f == ABSENT || field::len_of(f) == 0 || f == TOO_LONG {
            return (h ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(PRIME);
        }
    } else if is_absent(slab, f, opt) {
        return (h ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(PRIME);
    }
    // Hash exactly the bytes equality compares, by the same route, so the two
    // cannot disagree: the Java port shipped two silently wrong answers when a
    // field reached the hash by one path and the comparison by another.
    let mut len = 0u64;
    if normalise {
        let owned = value(slab, f, opt).unwrap_or_default();
        for b in owned.as_bytes() {
            h = (h ^ (*b as u64)).wrapping_mul(PRIME);
            len += 1;
        }
    } else if !field::is_escaped(f) {
        // Nothing to unescape, so the bytes are the value and eight of them can
        // be taken at a time. See `hash_bytes`.
        //
        // The field word is asked directly rather than through `logical()`,
        // which answers the same question by building an iterator over the slab
        // -- the cost `same_bytes` used to pay, in the one function that runs
        // for every key column of every row of both files.
        let raw = slab.raw(f);
        h = hash_bytes(raw, h);
        len = raw.len() as u64;
    } else {
        // Decoded, then folded by the same word-at-a-time loop as the branch
        // above. Folding the decoded bytes one at a time gave a different hash
        // for the same value, so a key a JSON writer escaped (`a\/b`, `\u0041`)
        // never found the same key written literally in the other file: the row
        // came out added and removed instead of matched.
        //
        // The words are assembled from the decoded bytes as they come, which
        // is `hash_bytes` over them without a buffer: a field may be megabytes.
        let (mut word, mut have) = (0u64, 0u32);
        for b in slab.logical(f) {
            word |= (b as u64) << (8 * have);
            have += 1;
            len += 1;
            if have == 8 {
                h = (h ^ word).wrapping_mul(PRIME);
                h ^= h >> 29;
                (word, have) = (0, 0);
            }
        }
        if have > 0 {
            h = (h ^ word).wrapping_mul(PRIME);
            h ^= h >> 29;
        }
    }
    (h ^ len).wrapping_mul(PRIME)
}

/// Each occupied slot's row becomes its key's position: the marks in `words`
/// before the row, which is `before` for the whole words and a popcount for the
/// rest.
///
/// That is one popcount per key, ten million of them at 10M rows, and a baseline
/// x86-64 build has no POPCNT instruction: `count_ones` becomes a dozen
/// instructions instead of one. So, as the row parse does for AVX2, the loop is
/// compiled a second time for POPCNT and chosen once per call where the CPU has
/// it -- every x86-64 CPU since 2008 does. A build for such a CPU compiles the
/// one loop.
fn rows_to_positions(part: &mut [u32], pos_mask: u32, words: &[u64], before: &[u32]) {
    #[cfg(all(target_arch = "x86_64", not(target_feature = "popcnt")))]
    if std::arch::is_x86_feature_detected!("popcnt") {
        // Safety: the CPU has POPCNT, which is all the copy assumes.
        return unsafe { rows_to_positions_popcnt(part, pos_mask, words, before) };
    }
    rows_to_positions_with(part, pos_mask, words, before)
}

/// [`rows_to_positions_with`] compiled for POPCNT.
///
/// # Safety
/// The CPU has POPCNT.
#[cfg(all(target_arch = "x86_64", not(target_feature = "popcnt")))]
#[target_feature(enable = "popcnt")]
unsafe fn rows_to_positions_popcnt(part: &mut [u32], pos_mask: u32, words: &[u64], before: &[u32]) {
    rows_to_positions_with(part, pos_mask, words, before)
}

#[inline(always)]
fn rows_to_positions_with(part: &mut [u32], pos_mask: u32, words: &[u64], before: &[u32]) {
    for word in part.iter_mut() {
        if *word != EMPTY_SLOT {
            let row = ((*word & pos_mask) - 1) as usize;
            let w = row >> 6;
            let below = words[w] & ((1u64 << (row & 63)) - 1);
            let position = (before[w] + below.count_ones()) as usize;
            *word = (*word & !pos_mask) | (position as u32 + 1);
        }
    }
}

fn key_hash(slab: &Slab, fields: &[Field], key_size: usize, opt: &Options) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    // Hoisted from per-field: the options do not change mid-sweep.
    let normalise = needs_normalising(opt);
    for f in &fields[..key_size] {
        h = hash_field(slab, *f, opt, normalise, h);
    }
    match narrow_hash() {
        Some(mask) => {
            let x = ((h & mask) + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            x ^ (x >> 29)
        }
        None => h,
    }
}

/// `COMPARA_NARROW_HASH=<bits>` folds every key's hash into one of `1 << bits`
/// values, still spread over the table, so that different keys share a full
/// hash. It is how the tests reach the code that tells such keys apart, which
/// real 64-bit hashes reach about never. Read once.
fn narrow_hash() -> Option<u64> {
    static MASK: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| {
        let bits: u32 = std::env::var("COMPARA_NARROW_HASH").ok()?.parse().ok()?;
        (bits < 64).then(|| (1u64 << bits) - 1)
    })
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// What a caller offers as a byte proof that a candidate row needs no parsing.
///
/// Both forms carry A's row from its start through the end of the last value
/// either file wants — **including the keys**, which is what lets the proof stand
/// in for the key comparison as well as the column comparison, and therefore run
/// before the mate is parsed at all. Where they differ is what makes the run a
/// whole number of values rather than a truncation of one.
#[derive(Clone, Copy)]
enum Proof<'b> {
    /// A column sits at a fixed offset, so the run has to end on a field
    /// boundary: `delimiter` or a newline. Without that, `12,3` would match a row
    /// opening `12,34`.
    Csv { bytes: &'b [u8], delimiter: u8 },
    /// A value is found by name and a name repeated in one object takes its last
    /// value, so the run ends one byte past the last value — its closing quote —
    /// and the mate's remaining bytes must name nothing this run tracks.
    Json { bytes: &'b [u8] },
}

/// How many failures in a row before the byte proof is only tried every
/// `PROOF_BACKOFF` rows. A power of two: the loop masks against it.
const PROOF_BACKOFF: usize = 64;

/// Where a row ends: the next row's start, or the end of the file.
///
/// Rows are inserted in the order they were swept and the sweep runs the chunks
/// in file order, so `row_at` ascends and row `n + 1` begins where row `n` stops.
fn row_end(ix: &RowIndex, row: i32, size: usize) -> usize {
    let next = row as usize + 1;
    if next < ix.row_at.len() {
        ix.row_at[next] as usize
    } else {
        size
    }
}

/// The fewest slots a region of a parallel insertion is given.
const MIN_REGION: usize = 64;

/// Whether the key index is built on every thread (`RowIndex::insert_parallel`).
/// Serial insertion measured faster than parallel (0.948x wall, -14% CPU at
/// 4 threads on 10M rows, CI A/B median of 7), so it is the default;
/// `COMPARA_PARALLEL_INSERT` set and not "0" opts back into the parallel
/// insertion.
fn parallel_insert(threads: usize) -> bool {
    if threads < 2 {
        return false;
    }
    std::env::var("COMPARA_PARALLEL_INSERT")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// One region's outcome in a parallel insertion: repeated keys' counts and
/// values by their first row, the rows that repeated a key, and whether the
/// region filled past what its probe can be trusted to end in.
#[derive(Default)]
struct Region {
    counts: HashMap<i32, u32>,
    dup_values: Vec<(i32, Vec<Val>)>,
    later: Vec<i32>,
    dup_keys: i64,
    dup_rows: i64,
    overfull: bool,
    /// A pair of rows whose hashes matched turned out to hold different keys:
    /// the region took one for the other, so its insertion is not the answer.
    unproven: bool,
}

/// Starts the fetch of `part[slot]`, as `RowIndex::prefetch` does for a whole
/// table.
#[inline]
fn prefetch_slot(part: &[u32], slot: usize) {
    #[cfg(target_arch = "x86_64")]
    // Safety: the caller masks `slot` into `part`, and a prefetch has no
    // architectural effect in any case.
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(part.as_ptr().add(slot) as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (part, slot);
}

/// Appends `parts` to `out`, in order, copying each on a thread of its own
/// into the room `out` already has for all of them, and releasing each as soon
/// as it is copied. Errs, `out` untouched, if that room is short or a copy
/// cannot finish.
fn append_in_parallel(out: &mut Vec<u64>, parts: Vec<Vec<u64>>) -> Result<()> {
    let adding: usize = parts.iter().map(Vec::len).sum();
    if out.capacity() - out.len() < adding {
        return Err(Error::new("no room to join the sweep's chunks"));
    }
    if parts.len() <= 1 {
        for part in parts {
            out.extend_from_slice(&part);
        }
        return Ok(());
    }
    let len = out.len();
    {
        let mut room = &mut out.spare_capacity_mut()[..adding];
        let mut tasks = Vec::with_capacity(parts.len());
        for part in parts {
            let (into, tail) = room.split_at_mut(part.len());
            tasks.push(std::sync::Mutex::new(Some((into, part))));
            room = tail;
        }
        let done = in_parallel(tasks.len(), |t| {
            let (into, part) = tasks[t]
                .lock()
                .ok()
                .and_then(|mut task| task.take())
                .ok_or_else(|| Error::new("a chunk taken twice"))?;
            for (slot, value) in into.iter_mut().zip(&part) {
                slot.write(*value);
            }
            Ok(())
        });
        for d in done {
            d?;
        }
    }
    // Safety: every one of the `adding` slots after `len` was written above --
    // the parts were laid end to end over exactly that room, and each task
    // wrote all of its part -- and a task that failed returned before here.
    unsafe { out.set_len(len + adding) };
    Ok(())
}

/// An empty table of `cap` slots, with the zeroes written rather than taken from
/// the kernel.
///
/// `vec![EMPTY_SLOT; cap]` compiles to a calloc, and a fresh anonymous mapping is
/// one shared page of zeroes until something writes to it. Nothing reads this
/// table before the inserts start, so each of its thirty-odd thousand pages would
/// be touched first by a random probe -- a page fault landing in the middle of
/// the dependent load chain the prefetch is there to hide. Faulting them in order
/// instead is work the kernel is far better at: at ten million rows it halves the
/// insert, 1.55s to 0.74s.
fn empty_table(cap: usize) -> Result<Vec<u32>> {
    let mut table = alloc::sized(cap, "the key index")?;
    table.resize(cap, EMPTY_SLOT);
    Ok(table)
}

/// A slot holds the top bits of its key's hash above `pos_bits` and the
/// position in `first_row` plus one in the low `pos_bits`, so zero means
/// empty.
///
/// Carrying the tag is what makes a failed probe cheap: the word already loaded
/// settles it. A table of bare positions has to follow each one into
/// `first_row` and then into `row_hash` -- two dependent random loads, over
/// arrays far too big to cache at ten million keys -- only to reject it.
///
/// The width is chosen from the row count rather than fixed, so the slot stays
/// four bytes: at ten million rows the index needs 24 bits and the tag gets
/// the other 8; at fifty million it needs 26 and the tag gets 6. A narrower tag
/// lets more probes through to the key comparison behind it, which is
/// unchanged -- the answer cannot change, only how often the bytes are
/// re-examined. Past four billion keys there are no bits left for a position;
/// `build` refuses that rather than truncating it.
const EMPTY_SLOT: u32 = 0;

/// One chunk's rows, in the order they appear in it.
struct Chunk {
    at: Vec<u64>,
    hash: Vec<u64>,
}

impl Chunk {
    /// Pushes a row's offset and hash with a single capacity check, instead of
    /// two separate fallible pushes. The sweep does this for every row, so the
    /// doubled branch and `Result` handling was measurable.
    fn push(&mut self, at: u64, hash: u64) -> Result<()> {
        if self.at.len() == self.at.capacity() {
            alloc::room(&mut self.at, 1, "one offset per row")?;
            alloc::room(&mut self.hash, 1, "one hash per row")?;
        }
        self.at.push(at);
        self.hash.push(hash);
        Ok(())
    }
}

/// An open-addressing index over one file's rows, keyed on the composite key.
///
/// Everything is a primitive array: where each row is, its key hash, and a table
/// of key numbers masked into a power-of-two slot count. Collisions are resolved
/// by comparing the key bytes, so the hash only has to be fast and spread.
struct RowIndex {
    row_at: Vec<u64>,
    row_hash: Vec<u64>,
    table: Vec<u32>,
    /// Low bits of a slot holding `first_row` positions plus one; the high
    /// bits hold the top of the key's hash as a tag. Chosen from the row count
    /// in `build`, so the slot stays four bytes at any scale this port runs.
    pos_bits: u32,
    pos_mask: u32,
    mask: usize,
    /// The slots a probe walks before it wraps: the whole table, or for an
    /// index whose insertion ran in parallel, the region of the table its
    /// key's home slot is in (`insert_parallel`).
    region_mask: usize,
    /// The row that first carried each distinct key, in first-appearance order.
    first_row: Vec<i32>,
    occurrences: Vec<u32>,
    rows: i64,
    dup_keys: i64,
    dup_rows: i64,
    /// Rows that repeated an already-seen key, in file order. Only kept when
    /// the fused join asked for them: the sweep joins every row as though it
    /// were first, and these are joined again afterwards and subtracted.
    later: Vec<i32>,
    track_later: bool,
    /// Each repeated key's values, by its `first_row` position, decoded when
    /// the insertion found the repeat: the key was just read to prove it, and
    /// the duplicate section would otherwise read it again, from disk once the
    /// file is past page cache. Only kept when the report has row lists.
    dup_key_values: HashMap<u32, Vec<Val>>,
    keep_dup_keys: bool,
}

impl RowIndex {
    /// Packs a slot: the top `32 - pos_bits` bits of the hash above the
    /// position plus one. Zero tag bits (past two billion rows) degrades to
    /// no tag rather than a wrong answer.
    fn slot_for(&self, hash: u64, pos: usize) -> u32 {
        let pos = pos as u32 + 1;
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

    /// The `first_row` position a non-empty slot holds.
    fn pos_of(&self, word: u32) -> usize {
        ((word & self.pos_mask) - 1) as usize
    }

    /// Finds and hashes every row in parallel, then inserts them in file order.
    ///
    /// The split is safe because the two halves need different things: parsing a
    /// row depends on nothing but where it starts, while the table depends on the
    /// order rows arrive — first occurrence of a key wins, and the duplicate
    /// counts follow from that. Handing rows to threads as they come would make
    /// the answer depend on thread scheduling; when the parallel insertion is
    /// opted into, the insertion is split by key instead (`insert_parallel`), each thread taking
    /// every row of its own keys in file order, which leaves the answer the
    /// serial one.
    fn build(
        side: &Side,
        key_size: usize,
        opt: &Options,
        threads: usize,
        tag: &'static str,
        sink: Option<&RowSink<'_>>,
        after_sweep: Option<&(dyn Fn(usize) + Sync)>,
    ) -> Result<(Self, Vec<SweptPart>)> {
        let mut phases = Phases::new(tag);
        let (mut chunks, fparts) = sweep(side, key_size, opt, threads, sink)?;
        phases.mark("sweep (parallel)");

        // Each fused part is its chunk's, and its picks hold row ids within
        // the chunk: the chunks before it are counted only now. One part per
        // chunk, in chunk order, or none at all.
        debug_assert!(fparts.is_empty() || fparts.len() == chunks.len());
        let mut first = 0usize;
        let fparts: Vec<SweptPart> = fparts
            .into_iter()
            .zip(&chunks)
            .map(|(mut part, chunk)| {
                let base = first as i32;
                for pick in part.changed.iter_mut().chain(part.removed.iter_mut()) {
                    pick.row += base;
                }
                for kept in &mut part.kept.a_rows {
                    kept.0 += base;
                }
                first += chunk.at.len();
                SweptPart {
                    part,
                    rows: base as usize..first,
                }
            })
            .collect();

        let total: usize = chunks.iter().map(|c| c.at.len()).sum();
        // Sized once for the rows about to be inserted, at about a two-thirds
        // load. Starting at four thousand and doubling meant twelve rehashes at
        // ten million rows, each one a full random-access pass over a table
        // already too big to cache -- work that grows with the file and is
        // entirely avoidable, since the row count is known before the first
        // insert.
        //
        // Two thirds and not a half: sizing for a half load doubles the table,
        // and at ten million keys the 268 MB that costs per side is worth more
        // than the probes it saves. Measured both ways, the denser table wins the
        // insert by 27% and the join by 6%. `first_row` and `occurrences` are sized the same way: they end
        // up one entry per distinct key, and every key is distinct until proven
        // otherwise.
        let mut cap: usize = 1 << 12;
        while cap * 2 < total * 3 + 16 {
            cap <<= 1;
        }
        // Wide enough to hold every key index plus the +1 that keeps 0 for
        // empty, narrow enough to leave the rest of the 32-bit slot for the
        // hash tag: 24 position bits at ten million rows, 26 at fifty. Past
        // four billion keys no width fits, and that is refused rather than
        // truncated.
        if total as u64 + 2 > 1u64 << 32 {
            return Err(Error::new("too many rows for the 32-bit key index"));
        }
        let mut pos_bits: u32 = 1;
        while pos_bits < 32 && (1u64 << pos_bits) < total as u64 + 2 {
            pos_bits += 1;
        }
        let pos_mask = if pos_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << pos_bits) - 1
        };
        // A chunk's two lists are already `row_at` and `row_hash` for its rows:
        // every row the sweep saw, in file order. So the first chunk's are taken
        // over rather than copied, and the rest appended a slice at a time, each
        // chunk released as soon as it has been. With one chunk that is no copy
        // at all where there used to be two pushes a row into a second pair of
        // lists the size of the file.
        let (mut row_at, mut row_hash) = match chunks.first_mut() {
            Some(c) => (std::mem::take(&mut c.at), std::mem::take(&mut c.hash)),
            None => (Vec::new(), Vec::new()),
        };
        let rest = total - row_at.len();
        alloc::grow(&mut row_at, rest, "one offset per row")?;
        alloc::grow(&mut row_hash, rest, "one hash per row")?;
        // The rest are copied in, each by a thread of its own and released
        // as soon as it has been: at 150M rows that is 1.8 GB, which on one
        // thread was a second or more of every index's build.
        let (ats, hashes): (Vec<Vec<u64>>, Vec<Vec<u64>>) =
            chunks.drain(..).skip(1).map(|c| (c.at, c.hash)).unzip();
        append_in_parallel(&mut row_at, ats)?;
        append_in_parallel(&mut row_hash, hashes)?;
        drop(chunks);
        let mut idx = RowIndex {
            row_at,
            row_hash,
            table: empty_table(cap)?,
            pos_bits,
            pos_mask,
            mask: cap - 1,
            region_mask: cap - 1,
            first_row: Vec::new(),
            occurrences: Vec::new(),
            rows: 0,
            dup_keys: 0,
            dup_rows: 0,
            later: Vec::new(),
            track_later: sink.is_some(),
            dup_key_values: HashMap::new(),
            keep_dup_keys: opt.row_lists,
        };
        // The file has been read and what is left is the insertion. `after_sweep`
        // starts reading the next file ahead, which fills memory with its
        // pages; so it waits until this index has the memory it will write --
        // the lists and the table above, and for the parallel insertion its
        // key arrays too -- or every page the insertion touched would first
        // have to be taken back from that reading. Called before them, B's
        // insertion at 150M rows took 11.1s where it takes 5.9s alone.
        let parallel = parallel_insert(threads);
        let arrays = if parallel {
            Some((
                alloc::filled(0i32, total, "one row per distinct key")?,
                alloc::filled(1u32, total, "one count per distinct key")?,
            ))
        } else {
            None
        };
        if let Some(hook) = after_sweep {
            hook(total);
        }
        if let Some(arrays) = arrays
            && idx.insert_parallel(side, key_size, opt, threads, arrays)?
        {
            phases.mark("index insert (parallel)");
            return Ok((idx, fparts));
        }
        idx.first_row = alloc::sized(total, "one row per distinct key")?;
        idx.occurrences = alloc::sized(total, "one count per distinct key")?;
        let mut probe = vec![ABSENT; side.width];
        let mut mine = vec![ABSENT; side.width];
        for row in 0..total {
            if let Some(&soon) = idx.row_hash.get(row + PREFETCH_AHEAD) {
                idx.prefetch(soon);
            }
            idx.insert(side, row as i32, key_size, opt, &mut probe, &mut mine)?;
        }
        phases.mark("index insert (serial)");
        Ok((idx, fparts))
    }

    #[allow(clippy::too_many_arguments)]
    /// Indexes row `row`, which is already in `row_at` and `row_hash`.
    fn insert(
        &mut self,
        side: &Side,
        row: i32,
        key_size: usize,
        opt: &Options,
        probe: &mut [Field],
        mine: &mut [Field],
    ) -> Result<()> {
        self.rows += 1;
        let at = self.row_at[row as usize];
        let hash = self.row_hash[row as usize];

        let mut slot = self.slot(hash);
        let mut mine_parsed = false;
        loop {
            let word = self.table[slot];
            if word == EMPTY_SLOT {
                self.table[slot] = self.slot_for(hash, self.first_row.len());
                self.first_row.push(row);
                self.occurrences.push(1);
                // Two thirds, which is what `build` sizes the table for. Half was
                // the wrong number in the wrong place: a file whose keys are
                // nearly all distinct crossed it and doubled a table that had
                // been sized precisely so it would not have to.
                if self.first_row.len() * 3 > self.table.len() * 2 {
                    self.rehash()?;
                }
                return Ok(());
            }
            // The tag rejects almost every collision without leaving this word.
            if self.tag_is(word, hash) {
                let key = self.pos_of(word);
                let candidate = self.first_row[key];
                if self.row_hash[candidate as usize] == hash {
                    // This row's fields are re-parsed rather than carried over from
                    // the sweep because the sweep produced ten million of them and
                    // this branch wants one.
                    if !mine_parsed {
                        side.keys_at(at, key_size, mine);
                        mine_parsed = true;
                    }
                    self.keys_of(side, candidate, key_size, probe);
                    if (0..key_size).all(|i| same(&side.slab, probe[i], &side.slab, mine[i], opt)) {
                        self.occurrences[key] += 1;
                        if self.occurrences[key] == 2 {
                            self.dup_keys += 1;
                            self.dup_rows += 1; // the first occurrence counts once the key repeats
                            if self.keep_dup_keys {
                                // A key that does not decode is left to the
                                // section, which reports it.
                                let values: Option<Vec<Val>> = probe[..key_size]
                                    .iter()
                                    .map(|f| {
                                        value_checked(&side.slab, *f, opt, "a duplicated key").ok()
                                    })
                                    .collect();
                                if let Some(values) = values {
                                    self.dup_key_values.insert(key as u32, values);
                                }
                            }
                        }
                        self.dup_rows += 1;
                        // The fused join joined this row as though it were
                        // first; it is joined again afterwards and subtracted.
                        if self.track_later {
                            alloc::push(&mut self.later, row, "a repeated key's row")?;
                        }
                        return Ok(());
                    }
                }
            }
            slot = self.next(slot);
        }
    }

    /// Indexes every row on `threads` threads, each owning one region of the
    /// table: a key's region is fixed by its home slot, so each thread sees
    /// all the rows of its own keys, in file order, and the first occurrence
    /// still wins -- the answer is the serial insertion's, `first_row` and
    /// `occurrences` included. Only the probe changes: it wraps at the end of
    /// its region (`next`).
    ///
    /// During the insertion a slot holds its key's first row rather than a
    /// position in `first_row`, so the threads need no list of their own; the
    /// rows that start a key are marked in a bitmap, and once every thread is
    /// done a key's position is how many marks come before its row -- which
    /// gives `first_row` in file order, and each slot its position.
    ///
    /// Returns `false`, the table emptied again, if a region filled past nine
    /// tenths: the keys' hashes are not spread, and the serial insertion,
    /// which can grow the table, takes over.
    fn insert_parallel(
        &mut self,
        side: &Side,
        key_size: usize,
        opt: &Options,
        threads: usize,
        (mut first_row, mut occurrences): (Vec<i32>, Vec<u32>),
    ) -> Result<bool> {
        let total = self.row_hash.len();
        let cap = self.table.len();
        let mut regions = 1usize;
        while regions * 2 <= threads && cap / (regions * 2) >= MIN_REGION {
            regions *= 2;
        }
        if regions < 2 {
            return Ok(false);
        }
        let size = cap / regions;
        let shift = size.trailing_zeros();
        let firsts: Vec<AtomicU64> = std::iter::repeat_with(|| AtomicU64::new(0))
            .take(total.div_ceil(64))
            .collect();

        let mut table = std::mem::take(&mut self.table);
        let outs = {
            let parts: Vec<std::sync::Mutex<Option<&mut [u32]>>> = table
                .chunks_mut(size)
                .map(|c| std::sync::Mutex::new(Some(c)))
                .collect();
            let this = &*self;
            let firsts = &firsts;
            in_parallel(regions, |r| {
                let part = parts[r]
                    .lock()
                    .ok()
                    .and_then(|mut p| p.take())
                    .ok_or_else(|| Error::new("an index region taken twice"))?;
                this.insert_region(side, key_size, opt, r, regions, shift, part, firsts)
            })
        };
        let mut regions_out = Vec::with_capacity(outs.len());
        for out in outs {
            regions_out.push(out?);
        }
        if regions_out.iter().any(|r| r.overfull || r.unproven) {
            table.fill(EMPTY_SLOT);
            self.table = table;
            return Ok(false);
        }

        // A key's position: the marks before its first row.
        let words: Vec<u64> = firsts.into_iter().map(AtomicU64::into_inner).collect();
        let mut before: Vec<u32> = alloc::sized(words.len() + 1, "the key positions")?;
        let mut keys = 0u32;
        for w in &words {
            before.push(keys);
            keys += w.count_ones();
        }
        before.push(keys);
        let position = |row: usize| -> usize {
            let w = row >> 6;
            let below = words[w] & ((1u64 << (row & 63)) - 1);
            (before[w] + below.count_ones()) as usize
        };

        // `first_row` in file order: each thread fills the positions of a run
        // of bitmap words, which are a run of positions.
        // `first_row` and `occurrences` came in one per row, written; there is a
        // key per row at most.
        first_row.truncate(keys as usize);
        occurrences.truncate(keys as usize);
        {
            let ways = threads.max(1);
            let mut rest: &mut [i32] = &mut first_row;
            let mut runs = Vec::with_capacity(ways);
            let mut lo = 0usize;
            for t in 0..ways {
                let hi = words.len() * (t + 1) / ways;
                let len = (before[hi] - before[lo]) as usize;
                let (run, tail) = rest.split_at_mut(len);
                runs.push(std::sync::Mutex::new(Some((lo, hi, run))));
                rest = tail;
                lo = hi;
            }
            let words = &words;
            let done = in_parallel(ways, |t| {
                let (lo, hi, run) = runs[t]
                    .lock()
                    .ok()
                    .and_then(|mut r| r.take())
                    .ok_or_else(|| Error::new("a run of keys taken twice"))?;
                let mut at = 0usize;
                for (w, &bits) in words[lo..hi].iter().enumerate() {
                    let mut bits = bits;
                    while bits != 0 {
                        let bit = bits.trailing_zeros() as usize;
                        run[at] = (((lo + w) << 6) | bit) as i32;
                        at += 1;
                        bits &= bits - 1;
                    }
                }
                Ok(())
            });
            for d in done {
                d?;
            }
        }

        // Each slot's row becomes its key's position.
        {
            let pos_mask = self.pos_mask;
            let ways = threads.max(1);
            let span = cap.div_ceil(ways);
            let parts: Vec<std::sync::Mutex<Option<&mut [u32]>>> = table
                .chunks_mut(span)
                .map(|c| std::sync::Mutex::new(Some(c)))
                .collect();
            let done = in_parallel(parts.len(), |t| {
                let part = parts[t]
                    .lock()
                    .ok()
                    .and_then(|mut p| p.take())
                    .ok_or_else(|| Error::new("an index region taken twice"))?;
                rows_to_positions(part, pos_mask, &words, &before);
                Ok(())
            });
            for d in done {
                d?;
            }
        }

        let mut later = Vec::new();
        for region in regions_out {
            for (row, n) in region.counts {
                occurrences[position(row as usize)] = n;
            }
            for (row, values) in region.dup_values {
                self.dup_key_values
                    .insert(position(row as usize) as u32, values);
            }
            self.dup_keys += region.dup_keys;
            self.dup_rows += region.dup_rows;
            if later.is_empty() {
                later = region.later;
            } else {
                alloc::grow(&mut later, region.later.len(), "a repeated key's row")?;
                later.extend_from_slice(&region.later);
            }
        }
        later.sort_unstable();

        self.table = table;
        self.region_mask = size - 1;
        self.first_row = first_row;
        self.occurrences = occurrences;
        self.later = later;
        self.rows = total as i64;
        Ok(true)
    }

    /// One region's share of `insert_parallel`: every row whose key's home
    /// slot is in region `r` (of `regions`, each `1 << shift` slots), in file
    /// order, into `part`, that region's slots. A slot holds its key's first
    /// row; `firsts` marks those rows.
    #[allow(clippy::too_many_arguments)]
    fn insert_region(
        &self,
        side: &Side,
        key_size: usize,
        opt: &Options,
        r: usize,
        regions: usize,
        shift: u32,
        part: &mut [u32],
        firsts: &[AtomicU64],
    ) -> Result<Region> {
        let mut out = Region::default();
        let region_mask = part.len() - 1;
        let full = part.len() / 10 * 9;
        let mut keys = 0usize;
        let mut probe = vec![ABSENT; side.width];
        let mut mine = vec![ABSENT; side.width];
        // Rows whose hash matched an earlier key's in full, each with that
        // key's first row, and the first rows whose key has just repeated.
        // Proving a match reads both rows' keys from the file, and a repeated
        // key is anywhere in it -- past memory, a read from the disk each. So
        // their pages are asked for when the match is found and the keys
        // compared after the loop, when those reads have long finished,
        // rather than the region waiting on each in turn: with the next
        // file's read-ahead queued on the disk, 7,500 such waits took B's
        // insertion at 150M rows from 4.6s to 10.2s.
        let mut matched: Vec<(usize, usize)> = Vec::new();
        let mut repeated: Vec<usize> = Vec::new();
        // Only one row in `regions` is this region's, so look that much
        // further ahead for the next line to fetch.
        let ahead = PREFETCH_AHEAD * regions;
        for row in 0..self.row_hash.len() {
            let hash = self.row_hash[row];
            let home = self.slot(hash);
            if let Some(&soon) = self.row_hash.get(row + ahead) {
                let s = self.slot(soon);
                if s >> shift == r {
                    prefetch_slot(part, s & region_mask);
                }
            }
            if home >> shift != r {
                continue;
            }
            let mut slot = home & region_mask;
            loop {
                let word = part[slot];
                if word == EMPTY_SLOT {
                    part[slot] = self.slot_for(hash, row);
                    firsts[row >> 6].fetch_or(1 << (row & 63), Ordering::Relaxed);
                    keys += 1;
                    if keys > full {
                        out.overfull = true;
                        return Ok(out);
                    }
                    break;
                }
                if self.tag_is(word, hash) {
                    let candidate = self.pos_of(word);
                    if self.row_hash[candidate] == hash {
                        // Taken as the same key now, proved below.
                        side.slab.will_need(self.row_at[row] as usize, PAGE);
                        side.slab.will_need(self.row_at[candidate] as usize, PAGE);
                        alloc::push(&mut matched, (row, candidate), "a matched row")?;
                        let n = out.counts.entry(candidate as i32).or_insert(1);
                        *n += 1;
                        if *n == 2 {
                            out.dup_keys += 1;
                            out.dup_rows += 1; // the first occurrence counts once the key repeats
                            if self.keep_dup_keys {
                                alloc::push(&mut repeated, candidate, "a repeated key")?;
                            }
                        }
                        out.dup_rows += 1;
                        if self.track_later {
                            alloc::push(&mut out.later, row as i32, "a repeated key's row")?;
                        }
                        break;
                    }
                }
                slot = (slot + 1) & region_mask;
            }
        }

        // Equal 64-bit hashes over different keys: never seen, but possible,
        // and then the serial insertion, which proves each match before it
        // takes it, has the answer.
        for &(row, candidate) in &matched {
            side.keys_at(self.row_at[row], key_size, &mut mine);
            side.keys_at(self.row_at[candidate], key_size, &mut probe);
            if !(0..key_size).all(|i| same(&side.slab, probe[i], &side.slab, mine[i], opt)) {
                out.unproven = true;
                return Ok(out);
            }
        }
        for candidate in repeated {
            side.keys_at(self.row_at[candidate], key_size, &mut probe);
            let values: Option<Vec<Val>> = probe[..key_size]
                .iter()
                .map(|f| value_checked(&side.slab, *f, opt, "a duplicated key").ok())
                .collect();
            if let Some(values) = values {
                out.dup_values.push((candidate as i32, values));
            }
        }
        Ok(out)
    }

    fn fields_of(&self, side: &Side, row: i32, out: &mut [Field]) {
        side.fields_at(self.row_at[row as usize], out);
    }

    fn keys_of(&self, side: &Side, row: i32, key_size: usize, out: &mut [Field]) {
        side.keys_at(self.row_at[row as usize], key_size, out);
    }

    fn slot(&self, hash: u64) -> usize {
        // The high bits of an FNV hash are the well-mixed ones; fold them down.
        ((hash ^ (hash >> 32)) as usize) & self.mask
    }

    /// The slot a probe tries after `slot`: the next one, wrapping at the end
    /// of `slot`'s region rather than the table's. One region is the table.
    #[inline]
    fn next(&self, slot: usize) -> usize {
        (slot & !self.region_mask) | ((slot + 1) & self.region_mask)
    }

    /// Starts the fetch of the slot `hash` will land in, without waiting for it.
    ///
    /// Every probe of this table is a random access into tens of megabytes, so
    /// it misses to memory, and the row after it needs a different line: the loop
    /// spends most of its time waiting on a load whose address was known long
    /// before it was issued. Asking for the line `PREFETCH_AHEAD` rows early
    /// turns that serial chain of misses into overlapping ones.
    #[inline]
    fn prefetch(&self, hash: u64) {
        #[cfg(target_arch = "x86_64")]
        // Safety: `slot` masks into the table's length, so the pointer is in
        // bounds, and a prefetch has no architectural effect in any case.
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            _mm_prefetch::<_MM_HINT_T0>(self.table.as_ptr().add(self.slot(hash)) as *const i8);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = hash;
    }

    /// Only reached if the row count was underestimated: `build` sizes the table
    /// for the rows it is about to insert, so the common path never grows it.
    fn rehash(&mut self) -> Result<()> {
        let size = self.table.len() * 2;
        self.table = empty_table(size)?;
        self.mask = size - 1;
        self.region_mask = self.mask;
        for key in 0..self.first_row.len() {
            let row = self.first_row[key];
            let hash = self.row_hash[row as usize];
            let mut slot = self.slot(hash);
            while self.table[slot] != EMPTY_SLOT {
                slot = self.next(slot);
            }
            self.table[slot] = self.slot_for(hash, key);
        }
        Ok(())
    }

    /// The row carrying `fields`' key in this index, or `None`. `other` is the
    /// side those fields live in, which is the opposite file when this is a join
    /// probe. `probe` is scratch the caller owns: the join runs several ranges
    /// at once, and a buffer hanging off the index would be shared between them.
    ///
    /// On `Some`, `probe` holds that row's fields — it is what the key columns
    /// were compared against. The join used to parse the row
    /// again on the line after this one returned, which is a second parse of
    /// every matched row in the file.
    ///
    #[allow(clippy::too_many_arguments)]
    fn lookup(
        &self,
        side: &Side,
        other: &Slab,
        fields: Option<&[Field]>,
        hash: u64,
        key_size: usize,
        opt: &Options,
        span: Option<Proof<'_>>,
        probe: &mut [Field],
    ) -> Option<(i32, bool)> {
        let mut slot = self.slot(hash);
        loop {
            let word = self.table[slot];
            if word == EMPTY_SLOT {
                return None;
            }
            if self.tag_is(word, hash) {
                let candidate = self.first_row[self.pos_of(word)];
                if self.row_hash[candidate as usize] == hash {
                    // The bytes first, where the caller offered them. A candidate
                    // whose row opens with the same bytes as far as either file
                    // reads has the same keys and the same columns, and neither
                    // row needs parsing to say so. A candidate the tag let
                    // through with a different key fails this on its first few
                    // bytes, so the cost of being wrong is a handful of them.
                    let proved = match span {
                        Some(Proof::Csv { bytes, delimiter }) => {
                            self.row_matches(side, candidate, bytes, delimiter)
                        }
                        Some(Proof::Json { bytes }) => self.json_matches(side, candidate, bytes),
                        None => false,
                    };
                    if proved {
                        return Some((candidate, true));
                    }
                    // No fields: the caller has the span but has not parsed the
                    // row. A candidate the bytes do not settle is returned
                    // unproven; the caller parses and re-looks-up with the key
                    // check, which also covers the hash collision this skips.
                    let fields = match fields {
                        Some(f) => f,
                        None => return Some((candidate, false)),
                    };
                    self.fields_of(side, candidate, probe);
                    if (0..key_size).all(|i| same(&side.slab, probe[i], other, fields[i], opt)) {
                        return Some((candidate, false));
                    }
                }
            }
            slot = self.next(slot);
        }
    }

    /// Whether `candidate`'s row opens with exactly `bytes` and names nothing this
    /// run tracks in what follows.
    ///
    /// The second half is the whole difference from CSV. A byte-equal prefix says
    /// the two rows hold the same values *written in the prefix*; it cannot say
    /// the mate does not name one of them again further on, and in JSON a repeated
    /// name takes its last value. What is left to scan is the trailing ignored
    /// columns, which is the only reason this is cheaper than parsing the row.
    fn json_matches(&self, side: &Side, candidate: i32, bytes: &[u8]) -> bool {
        let Some(parser) = side.parser() else {
            return false;
        };
        let data = side.slab.data();
        let lo = self.row_at[candidate as usize] as usize;
        let end = row_end(self, candidate, data.len());
        json_rows_match(parser, bytes, data, lo, end)
    }

    /// Whether `candidate`'s row opens with exactly `bytes` and ends that run on
    /// a field boundary.
    ///
    /// The boundary is what makes the run a whole number of columns rather than
    /// a truncation of one: without it `12,3` would match a row opening `12,34`.
    fn row_matches(&self, side: &Side, candidate: i32, bytes: &[u8], delimiter: u8) -> bool {
        let data = side.slab.data();
        let from = self.row_at[candidate as usize] as usize;
        let Some(to) = from.checked_add(bytes.len()) else {
            return false;
        };
        if to > data.len() {
            return false;
        }
        if to < data.len() && data[to] != delimiter && data[to] != b'\n' {
            return false;
        }
        data[from..to] == *bytes
    }

    fn unique_keys(&self) -> i64 {
        self.first_row.len() as i64
    }
}

// ---------------------------------------------------------------------------
// The sweep: finding and hashing every row, in parallel
// ---------------------------------------------------------------------------

fn sweep(
    side: &Side,
    key_size: usize,
    opt: &Options,
    threads: usize,
    sink: Option<&RowSink<'_>>,
) -> Result<(Vec<Chunk>, Vec<Part>)> {
    match &side.rows {
        Rows::Text { parser, keys, from } => {
            sweep_text(side, keys, parser, *from, key_size, opt, threads, sink)
        }
        Rows::Columnar { rows, .. } => {
            debug_assert!(sink.is_none(), "the fused join is text only");
            sweep_columnar(side, *rows, key_size, opt, threads).map(|c| (c, Vec::new()))
        }
    }
}

/// A fused sweep worker's part, with the rows of the chunk it joined.
struct SweptPart {
    part: Part,
    rows: std::ops::Range<usize>,
}

/// What the sweep hands each row to, when the join runs inside it.
///
/// Each sweep worker owns its part's `Part` and `RowScratch`; the sink only
/// carries the shared join state, so `&RowSink` is `Sync` and no row takes a
/// lock. The worker returns its `Part` with its chunk, and the parts are
/// folded after the sweep.
struct RowSink<'a> {
    ctx: &'a JoinCtx<'a>,
    /// B's seen bitmap: rows the sweep matched set their bit here, with a
    /// plain atomic OR — the one shared write, and it is idempotent.
    seen: &'a [AtomicU64],
    nc: usize,
    /// Whether each worker's part copies the rows the report may show.
    keep_rows: bool,
}

impl RowSink<'_> {
    /// A worker's own join state: its part's counts and its row scratch.
    fn worker(&self) -> (Part, RowScratch) {
        let runs_len = self.ctx.runs.as_ref().map_or(0, Runs::len);
        let mut part = Part::blank(self.nc);
        part.kept.on = self.keep_rows;
        (part, RowScratch::new(self.ctx.width, runs_len))
    }

    /// Joins one row into the worker's own part: its index in its chunk
    /// (the pick id until `build` makes it the file's, and the proof backoff's
    /// counter), its byte range, key hash, and the next row's hash for the
    /// lookup prefetch (`None` at a chunk's end).
    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        part: &mut Part,
        scratch: &mut RowScratch,
        j: usize,
        from: u64,
        end: u64,
        hash: u64,
        next_hash: Option<u64>,
    ) {
        if let Some(next) = next_hash {
            self.ctx.bi.prefetch(next);
        }
        join_row(
            self.ctx,
            part,
            scratch,
            j as i32,
            from as usize,
            end as usize,
            hash,
            j,
            self.seen,
        );
    }
}

/// Runs `each` over `0..parts` on that many threads, collecting what they return
/// in order. A thread that cannot be spawned is not a failure: it is the same
/// work, done here.
fn in_parallel<T, F>(parts: usize, each: F) -> Vec<Result<T>>
where
    T: Send,
    F: Fn(usize) -> Result<T> + Sync,
{
    if parts <= 1 {
        return vec![each(0)];
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = (1..parts)
            .map(|i| parallel::spawn_at(scope, &each, i))
            .collect();
        let mut out = vec![each(0)];
        for handle in handles {
            out.push(
                handle
                    .join()
                    .unwrap_or_else(|_| Err(Error::new("a comparison worker panicked"))),
            );
        }
        out
    })
}

/// Maps `items` into an owned `Vec` across `threads` threads, in order.
///
/// The report's rows are the one place this engine turns cells into `String`s,
/// and it does it up to two hundred thousand times — fifty thousand changed
/// rows on both sides, plus the added and removed lists — over twenty columns
/// each. Every row is independent of every other, so the only reason it was
/// serial is that nobody had split it, and it is the whole of the gap between
/// the Rust port's cores-busy ratio and the C++ port's.
///
/// Chunked rather than one thread per list: the four lists are very uneven —
/// fifty thousand against ten — so dealing them out whole would leave two
/// threads idle while the other two did all of it.
fn map_rows<T, U, F>(items: &[T], threads: usize, what: &str, each: F) -> Result<Vec<U>>
where
    T: Sync,
    U: Send,
    F: Fn(&T) -> Result<U> + Sync,
{
    let parts = threads.clamp(1, items.len().div_ceil(1 << 12).max(1));
    if parts <= 1 {
        let mut out: Vec<U> = alloc::sized(items.len(), what)?;
        for item in items {
            out.push(each(item)?);
        }
        return Ok(out);
    }
    let chunk = items.len().div_ceil(parts);
    let slices: Vec<&[T]> = items.chunks(chunk).collect();
    let one = |p: usize| -> Result<Vec<U>> {
        let mut part: Vec<U> = alloc::sized(slices[p].len(), what)?;
        for item in slices[p] {
            part.push(each(item)?);
        }
        Ok(part)
    };
    let mut out: Vec<U> = alloc::sized(items.len(), what)?;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (1..slices.len())
            .map(|p| parallel::spawn_at(scope, &one, p))
            .collect();
        let mut failed: Option<Error> = None;
        match one(0) {
            Ok(part) => out.extend(part),
            Err(e) => failed = Some(e),
        }
        for handle in handles {
            // A panicking worker would mean losing rows silently, which is worse
            // than the panic: a short report that looks complete is the one
            // outcome to avoid. A worker that ran out of room is different --
            // it says so, and the first such refusal is the one reported.
            match handle.join().expect("a report row worker") {
                Ok(part) => out.extend(part),
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(out),
        }
    })
}

/// Parses and hashes every row of a mapped text file, in `threads` chunks.
///
/// A CSV split has to land on a row boundary, and a newline inside a quoted
/// field is not one. `chunk_bounds` can settle that by counting the quotes
/// before each split -- but that count is a pass over the front of the file on
/// one thread before any chunk starts, and at two chunks a file it read half the
/// file: 0.13-0.16s a side on a 4M-row pair, as long as a chunk's own sweep.
///
/// So the split is guessed first -- the next newline, as for JSON -- and
/// checked after. The first chunk starts at a real row, so the row it finishes
/// on ends at the first real row start past its boundary; if that is where the
/// next chunk began, the boundary was real, and the same holds down the line.
/// Only if a guess was wrong -- a quoted newline at the split -- is the sweep
/// run again on counted bounds, which is exactly what it did before. A chunk
/// that started on a wrong guess parsed garbage, so its rows *and its errors*
/// are discarded unread.
#[allow(clippy::too_many_arguments)]
fn sweep_text(
    side: &Side,
    keys: &RowParser,
    whole: &RowParser,
    from: usize,
    key_size: usize,
    opt: &Options,
    threads: usize,
    sink: Option<&RowSink<'_>>,
) -> Result<(Vec<Chunk>, Vec<Part>)> {
    let data = side.slab.data();
    if from >= data.len() {
        return Ok((Vec::new(), Vec::new()));
    }
    let dialect = side.slab.dialect();
    if dialect == Dialect::Csv {
        let bounds = chunk_bounds(data, from, threads, dialect, false);
        let parts = bounds.len() - 1;
        let mut swept = sweep_chunks(side, keys, whole, &bounds, key_size, opt, sink).into_iter();
        let mut chunks = Vec::with_capacity(parts);
        let mut fparts = Vec::with_capacity(parts);
        let mut held = true;
        for i in 0..parts {
            // Chunk i started at a real row: chunk 0 by construction, and every
            // later one because the loop only gets here past the check below.
            let (chunk, end, fpart) = swept.next().expect("one result per chunk")?;
            chunks.push(chunk);
            if let Some(p) = fpart {
                fparts.push(p);
            }
            if i + 2 < bounds.len() && end != bounds[i + 1] {
                held = false;
                break;
            }
        }
        if held {
            return Ok((chunks, fparts));
        }
        // A wrong guess joined garbage rows; the garbage parts are dropped and
        // the counted sweep re-joins the real rows into fresh ones.
    }
    let bounds = chunk_bounds(data, from, threads, dialect, true);
    let mut chunks = Vec::new();
    let mut fparts = Vec::new();
    for r in sweep_chunks(side, keys, whole, &bounds, key_size, opt, sink) {
        let (chunk, _, fpart) = r?;
        chunks.push(chunk);
        if let Some(p) = fpart {
            fparts.push(p);
        }
    }
    Ok((chunks, fparts))
}

/// One chunk per pair of `bounds`, each returned with the offset its sweep
/// stopped at (the start of the first row past its boundary, or the end of the
/// file) and, when the fused join is running, the worker's own joined part.
#[allow(clippy::type_complexity)]
fn sweep_chunks(
    side: &Side,
    keys: &RowParser,
    whole: &RowParser,
    bounds: &[usize],
    key_size: usize,
    opt: &Options,
    sink: Option<&RowSink<'_>>,
) -> Vec<Result<(Chunk, usize, Option<Part>)>> {
    let data = side.slab.data();
    let parts = bounds.len() - 1;
    in_parallel(parts, |i| {
        let (begin, stop) = (bounds[i], bounds[i + 1]);
        let mut chunk = Chunk {
            at: Vec::new(),
            hash: Vec::new(),
        };
        // The fused join's counts live in the worker that joined them: no
        // lock, no sharing, just returned with the chunk.
        let mut fused = sink.map(|s| s.worker());
        let mut fields = vec![ABSENT; key_size.max(1)];
        let mut whole_fields = vec![ABSENT; side.width];
        let mut pos = begin;
        // Rows that *start* in this chunk belong to it; the last one is finished
        // past the boundary rather than cut in half.
        while pos < stop {
            match data[pos] {
                // A line with nothing on it is not a row.
                b'\n' => {
                    pos += 1;
                    continue;
                }
                b'\r' if pos + 1 < data.len() && data[pos + 1] == b'\n' => {
                    pos += 2;
                    continue;
                }
                _ => {}
            }
            let next = keys.parse(data, pos, data.len(), &mut fields);
            // A field cannot be longer than the row that holds it, so only a row
            // over the cap can hide an over-long column the key parser did not
            // look at. That row is re-read in full to find it. At a hundred and
            // eighty bytes a row this is one comparison and never taken; the
            // check it replaces walked twenty fields of every row of the file.
            let over = next.saturating_sub(pos) as u64 > MAX_FIELD_LEN;
            if fields.contains(&TOO_LONG)
                || (over && {
                    whole.parse(data, pos, data.len(), &mut whole_fields);
                    whole_fields.contains(&TOO_LONG)
                })
            {
                return Err(Error::new(format!(
                    "a field larger than {MAX_FIELD_LEN} bytes is more than this engine packs; \
                     use --engine native"
                )));
            }
            chunk.push(pos as u64, key_hash(&side.slab, &fields, key_size, opt))?;
            // The row before this one ends where this one starts, and its
            // pages are still hot: the fused join takes it now, rather than
            // re-reading it after the sweep.
            if let (Some(s), Some((part, scratch))) = (sink, fused.as_mut())
                && chunk.at.len() >= 2
            {
                let j = chunk.at.len() - 2;
                s.row(
                    part,
                    scratch,
                    j,
                    chunk.at[j],
                    pos as u64,
                    chunk.hash[j],
                    Some(chunk.hash[j + 1]),
                );
            }
            if next <= pos {
                break; // no progress: a malformed tail rather than an endless loop
            }
            pos = next;
        }
        // The chunk's last row ends where the sweep stopped.
        if let (Some(s), Some((part, scratch))) = (sink, fused.as_mut())
            && let Some(&lo) = chunk.at.last()
        {
            let j = chunk.at.len() - 1;
            s.row(part, scratch, j, lo, pos as u64, chunk.hash[j], None);
        }
        Ok((chunk, pos, fused.map(|(part, _)| part)))
    })
}

/// Hashes every row of an already-materialised columnar file. There is nothing
/// to find — row `n` is row `n` — so this splits into equal ranges.
fn sweep_columnar(
    side: &Side,
    rows: usize,
    key_size: usize,
    opt: &Options,
    threads: usize,
) -> Result<Vec<Chunk>> {
    let parts = threads.clamp(1, rows.div_ceil(1 << 14).max(1));
    let results = in_parallel(parts, |i| {
        let lo = rows * i / parts;
        let hi = rows * (i + 1) / parts;
        let mut chunk = Chunk {
            at: alloc::sized(hi - lo, "one offset per row")?,
            hash: alloc::sized(hi - lo, "one hash per row")?,
        };
        let mut fields = vec![ABSENT; side.width];
        for row in lo..hi {
            side.fields_at(row as u64, &mut fields);
            chunk.at.push(row as u64);
            chunk
                .hash
                .push(key_hash(&side.slab, &fields, key_size, opt));
        }
        Ok(chunk)
    });
    results.into_iter().collect()
}

/// Where each chunk begins, as offsets of real row starts.
///
/// The nominal split is `size / threads`, walked forward to the next row. Walking
/// forward is the whole difficulty for CSV: a newline inside a quoted field is
/// not a row boundary, and a thread starting mid-file cannot tell whether it is
/// inside one. Parity settles it — every `"` toggles in-quote state, including
/// both halves of a doubled quote, which toggles twice and so correctly leaves
/// the state alone, so the count of quotes before a position says whether that
/// position is inside a field. Counting them is a scan for one byte, far cheaper
/// than parsing, and it splits across the same threads.
///
/// JSON needs none of that: a raw newline inside a string is not valid JSON, so
/// every newline ends a record.
fn chunk_bounds(
    data: &[u8],
    from: usize,
    threads: usize,
    dialect: Dialect,
    counted: bool,
) -> Vec<usize> {
    let end = data.len();
    // Uncounted, a CSV split is walked like a JSON one: to the next newline,
    // whatever the quotes say. That is a guess, and `sweep_text` checks it.
    let quoted = dialect == Dialect::Csv && counted;
    if threads <= 1 || end - from < CHUNKING_THRESHOLD {
        return vec![from, end];
    }
    let nominal: Vec<usize> = (1..threads)
        .map(|i| from + (end - from) * i / threads)
        .collect();

    // Quotes before each split point, counted once per byte rather than once per
    // byte per split point.
    //
    // Asking each task for the count from `from` to its own split reads the
    // first slice of the file in every task, the second in all but one, and so
    // on: `(threads - 1) / 2` passes over the file in total, and the last task
    // alone reads nearly all of it, so the step costs about a whole pass however
    // many threads run it. That grows with the thread count while the work it
    // parallelises shrinks -- at four threads it was a fifth of the sweep, and at
    // sixteen it would dominate it.
    //
    // Counting each slice on its own and running a prefix sum over the results
    // gives the same numbers for one pass split evenly.
    let quotes: Vec<usize> = if !quoted {
        vec![0; nominal.len()]
    } else {
        let mut edges = Vec::with_capacity(nominal.len() + 1);
        edges.push(from);
        edges.extend_from_slice(&nominal);
        let each = in_parallel(nominal.len(), |i| {
            Ok(count_byte(data, edges[i], edges[i + 1], b'"'))
        });
        let mut running = 0usize;
        each.into_iter()
            .map(|r| {
                running += r.unwrap_or(0);
                running
            })
            .collect()
    };

    let mut bounds = vec![from];
    for (i, &start) in nominal.iter().enumerate() {
        let mut in_quotes = quotes[i] % 2 == 1;
        let at = if dialect == Dialect::Json {
            json_row_start(data, start, end)
        } else {
            let mut at = start;
            while at < end {
                match data[at] {
                    b'"' if quoted => in_quotes = !in_quotes,
                    b'\n' if !in_quotes => {
                        at += 1;
                        break;
                    }
                    _ => {}
                }
                at += 1;
            }
            at
        };
        if at > *bounds.last().expect("a first bound") && at < end {
            bounds.push(at);
        }
    }
    bounds.push(end);
    bounds
}

/// The first JSON row start at or after `at`: the next line whose first byte
/// that is not JSON space opens an object. ndjson rows are one to a line, but
/// a pretty-printed top-level array spreads one object over several lines, so
/// only the line that opens the object is a real row start -- anything else,
/// the array's own brackets and the object's inner lines, belongs to the
/// chunk already reading it. A compact array has no newlines at all and so
/// reads as one chunk.
fn json_row_start(data: &[u8], mut at: usize, end: usize) -> usize {
    while at < end {
        let stop = next_of1(data, at, end, b'\n');
        if stop >= end {
            return end;
        }
        let mut first = stop + 1;
        while first < end && matches!(data[first], b' ' | b'\t' | b'\r') {
            first += 1;
        }
        if first < end && data[first] == b'{' {
            return first;
        }
        at = stop + 1;
    }
    end
}

// ---------------------------------------------------------------------------
// The join
// ---------------------------------------------------------------------------

/// A row chosen for a report section, with the row it matched on the other side.
#[derive(Clone, Copy)]
struct Pick {
    row: i32,
    mate: i32,
}

/// A row list that stops growing at the report cap but keeps counting.
struct Capped {
    held: Vec<Pick>,
    cap: usize,
    unbounded: bool,
    total: i64,
    /// Whether to store picks at all. With `--summary` the lists are
    /// discarded (see the `row_lists` check at the end of `join`), so
    /// populating them is pure allocation -- and under a memory cap, the
    /// allocation that aborts instead of refusing.
    keep: bool,
}

impl Capped {
    fn new(cap: usize, unbounded: bool, keep: bool) -> Self {
        Capped {
            held: Vec::new(),
            cap,
            unbounded,
            total: 0,
            keep,
        }
    }

    fn push(&mut self, pick: Pick) {
        self.total += 1;
        if !self.keep {
            return;
        }
        // One past the cap, so a section can still report that it was truncated.
        if self.unbounded || self.held.len() <= self.cap {
            self.held.push(pick);
        }
    }
}

/// One chunk of A's keys.
///
/// Each chunk keeps its own counts, column stats and capped lists; because the
/// chunks are contiguous and merged in chunk order, the result is identical to
/// one thread's, including which rows survive the cap.
struct Part {
    matched: i64,
    changed_per: Vec<i64>,
    blanked_per: Vec<i64>,
    filled_per: Vec<i64>,
    changed: Vec<Pick>,
    removed: Vec<Pick>,
    changed_total: i64,
    removed_total: i64,
    /// The rows the report may show, copied while the fused sweep has them in
    /// memory. Empty unless `Kept::on`.
    kept: Kept,
}

/// Rows the report may show, copied as the fused sweep joins them.
///
/// Past page cache the report's rows are otherwise read back after the sweep,
/// one at a time: at 150M rows tens of thousands of reads scattered over both
/// files, 24s of a 287s run, where the sweep had every one of those pages in
/// memory and let them go. So each row a pick list takes is copied as it is
/// joined -- A's row, and for a changed row its mate in B -- and so are the
/// rows of B that the matches skip over, which is where B's added rows are
/// when the two files are in the same order. All of it is bounded by the
/// pick cap, and a row nothing kept is read back as it always was.
#[derive(Default)]
struct Kept {
    /// Whether this part keeps rows: the fused sweep's own parts, when the
    /// report has row lists. Off for the parts that take repeats back, and
    /// switched off for good if memory for a copy cannot be had -- the rows
    /// are then read from the file, as they would have been.
    on: bool,
    /// Row bytes, each row's from its start to the next row's.
    a: Vec<u8>,
    b: Vec<u8>,
    /// A's rows by id -- within the chunk until `build` numbers them for the
    /// file -- and where each starts in `a`.
    a_rows: Vec<(i32, usize)>,
    /// B's rows by id, and where each starts in `b`.
    b_rows: Vec<(i32, usize)>,
    /// The last B row this part matched, and how many skipped rows it kept.
    last_mate: i32,
    skipped: usize,
}

impl Kept {
    /// Appends `bytes` to `arena` and records `row` at its start. On the first
    /// copy the arena is sized for the pick cap at this row's length: grown a
    /// doubling at a time, the arenas left the heap fragmented enough to slow
    /// the insertion that follows the sweep by half (0.21s to 0.3-0.5s on 8M
    /// rows, the copies alone at 70 MB).
    fn copy(
        on: &mut bool,
        arena: &mut Vec<u8>,
        rows: &mut Vec<(i32, usize)>,
        row: i32,
        bytes: &[u8],
        cap: usize,
    ) {
        if arena.capacity() == 0 {
            const MOST: usize = 64 << 20;
            let want = (bytes.len() + 16)
                .saturating_mul(cap.saturating_add(1))
                .saturating_mul(2);
            let _ = arena.try_reserve_exact(want.min(MOST));
        }
        if arena.try_reserve(bytes.len()).is_err() || rows.try_reserve(1).is_err() {
            *on = false;
            return;
        }
        rows.push((row, arena.len()));
        arena.extend_from_slice(bytes);
    }
}

/// Decodes one row's key and compared columns into the owned values the report
/// holds. This is the only place a cell becomes a `String`, and it runs at most
/// `--max-rows` times per section rather than once per row in the file.
fn row_values(side: &Side, idx: &RowIndex, row: i32, opt: &Options) -> Result<Vec<Val>> {
    let mut fields = vec![ABSENT; side.width];
    idx.fields_of(side, row, &mut fields);
    let mut out: Vec<Val> = alloc::sized(side.width, "a report row")?;
    for f in &fields {
        out.push(value_checked(&side.slab, *f, opt, "a report cell")?);
    }
    Ok(out)
}

/// Decodes just one row's key columns.
///
/// [`row_values`] decodes the whole row, which is what the report's own rows
/// need and between twice and ten times what the duplicate-key section needs:
/// that section keeps the key and throws the compared columns away. The index
/// already has a parser that stops at the key, and there is one of these per
/// duplicated key rather than per kept row, so the columns it was decoding and
/// discarding were the largest allocation anywhere in the report path.
fn key_values(side: &Side, idx: &RowIndex, row: i32, opt: &Options) -> Result<Vec<Val>> {
    let key_size = opt.key.len();
    let mut fields = vec![ABSENT; key_size];
    idx.keys_of(side, row, key_size, &mut fields);
    let mut out: Vec<Val> = alloc::sized(key_size, "a duplicated key")?;
    for f in &fields {
        out.push(value_checked(&side.slab, *f, opt, "a duplicated key")?);
    }
    Ok(out)
}

/// Runs `work` on `threads` threads and returns everything they produced.
///
/// Each worker pulls from a queue `work` closes over until it is empty, so the
/// split is by demand rather than by an even division: a chunk that turns out
/// expensive delays one worker rather than the whole round.
fn on_threads<T: Send>(threads: usize, work: impl Fn() -> Vec<T> + Sync) -> Vec<T> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = (1..threads.max(1))
            .map(|_| parallel::spawn(scope, &work))
            .collect();
        let mut all = work();
        for handle in handles {
            all.extend(handle.join().expect("a join worker"));
        }
        all
    })
}

/// How many chunks `keys` divides into. One below the threshold, where finding
/// the boundaries would cost more than the join it splits.
fn ways_for(keys: usize) -> usize {
    if keys < JOIN_THRESHOLD {
        1
    } else {
        keys.div_ceil(JOIN_CHUNK).max(1)
    }
}

fn to_cells(values: &[Val]) -> Result<Vec<Cell>> {
    let mut out: Vec<Cell> = alloc::sized(values.len(), "a report row's cells")?;
    for v in values {
        out.push(Cell::Value(alloc::val(v.as_deref(), "a report cell")?));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
/// Everything one row's join needs beyond the row itself and the part its
/// counts land in. Built once per join and shared by every part; the fused join
/// builds one too, so both paths join a row with the same code.
struct JoinCtx<'a> {
    a: &'a Side,
    b: &'a Side,
    bi: &'a RowIndex,
    opt: &'a Options,
    key_size: usize,
    nc: usize,
    width: usize,
    cap: usize,
    keep_picks: bool,
    exporting: bool,
    plain: bool,
    span_tail: Option<(usize, u8, usize)>,
    guard: Option<(u8, usize)>,
    runs: Option<Runs>,
    json_proof: bool,
    /// Per compared column, whether either file stores it typed: then two
    /// cells that differ as text but hold the same value are the same, because
    /// a typed value has no spelling of its own to compare (`31.40` in a CSV is
    /// the 31.4 a Parquet file stores).
    typed: Vec<Typed>,
}

/// One row-join's scratch: the two rows' fields, the runs' spans, and the proof
/// backoff. The normal join builds one per part; the fused join carries one per
/// part across the sweep.
struct RowScratch {
    fa: Vec<Field>,
    fb: Vec<Field>,
    spans: Vec<(usize, usize)>,
    refused: usize,
}

impl RowScratch {
    fn new(width: usize, runs_len: usize) -> RowScratch {
        RowScratch {
            fa: vec![ABSENT; width],
            fb: vec![ABSENT; width],
            spans: vec![(0usize, 0usize); runs_len],
            refused: 0,
        }
    }
}

impl Part {
    fn blank(nc: usize) -> Part {
        Part {
            matched: 0,
            changed_per: vec![0; nc],
            blanked_per: vec![0; nc],
            filled_per: vec![0; nc],
            changed: Vec::new(),
            removed: Vec::new(),
            changed_total: 0,
            removed_total: 0,
            kept: Kept {
                last_mate: -1,
                ..Kept::default()
            },
        }
    }

    /// Folds another part's counts into this one. Only the counts: the fused
    /// path settles its picks in `fused_picks`, and the ordinary fold in
    /// `join_tail` keeps its own pick merging.
    fn add(&mut self, other: &Part) {
        self.matched += other.matched;
        for i in 0..self.changed_per.len() {
            self.changed_per[i] += other.changed_per[i];
            self.blanked_per[i] += other.blanked_per[i];
            self.filled_per[i] += other.filled_per[i];
        }
        self.changed_total += other.changed_total;
        self.removed_total += other.removed_total;
    }

    /// Takes another part's counts back out: the later-duplicate correction.
    /// A repeated key's first row stays matched, so the caller marks nothing.
    /// The picks are not touched; a repeat's are dropped in `fused_picks`.
    fn sub(&mut self, other: &Part) {
        self.matched -= other.matched;
        for i in 0..self.changed_per.len() {
            self.changed_per[i] -= other.changed_per[i];
            self.blanked_per[i] -= other.blanked_per[i];
            self.filled_per[i] -= other.filled_per[i];
        }
        self.changed_total -= other.changed_total;
        self.removed_total -= other.removed_total;
    }
}

/// Records a matched B row in the seen bitmap. Idempotent: the fused sweep may
/// mark the same row from a duplicate key, and a wrong split guess may mark
/// rows the counted sweep marks again.
fn mark_seen(seen: &[AtomicU64], row: i32) {
    let bit = row as usize;
    seen[bit >> 6].fetch_or(1 << (bit & 63), Ordering::Relaxed);
}

/// Joins one row of A's against B's index: the join's inner loop as a function,
/// shared by the normal join (over each key's first row) and the fused join
/// (over every row as the sweep finds it, then again over the repeated keys,
/// whose results are subtracted).
///
/// `row` is the row's id for the pick lists, `from`/`end` its byte range in
/// A's slab, `hash` its sweep-computed key hash, and `k` its index in the part,
/// for the proof backoff. `seen` records the mate in B's seen bitmap.
#[allow(clippy::too_many_arguments)]
fn join_row(
    ctx: &JoinCtx,
    out: &mut Part,
    scratch: &mut RowScratch,
    row: i32,
    from: usize,
    end: usize,
    hash: u64,
    k: usize,
    seen: &[AtomicU64],
) {
    enum Fast {
        Proved(i32),
        Unproven,
        Missed,
        Skipped,
    }
    // The byte proof is worth its scan where rows repeat themselves; where
    // they really differ every attempt fails and costs a scan, a probe and a
    // compare before the parse that was always coming. So after
    // `PROOF_BACKOFF` failures in a row it is tried once every
    // `PROOF_BACKOFF` rows, and a success resumes it -- as C, C++ and Zig do.
    let attempt = scratch.refused < PROOF_BACKOFF || k & (PROOF_BACKOFF - 1) == 0;
    let fast = if !attempt {
        Fast::Skipped
    } else if let (Some(runs), Some((delimiter, _))) = (&ctx.runs, ctx.guard) {
        let data = ctx.a.slab.data();
        if runs.of_row(data, from, end, delimiter, &mut scratch.spans) {
            // No fields and no span: the first candidate whose whole
            // hash matches, unproven, which the runs then settle.
            match ctx.bi.lookup(
                ctx.b,
                &ctx.a.slab,
                None,
                hash,
                ctx.key_size,
                ctx.opt,
                None,
                &mut scratch.fb,
            ) {
                Some((mate, _)) => {
                    let bd = ctx.b.slab.data();
                    let b_lo = ctx.bi.row_at[mate as usize] as usize;
                    let b_hi = row_end(ctx.bi, mate, bd.len());
                    if runs.matches(data, &scratch.spans, bd, b_lo, b_hi, delimiter) {
                        Fast::Proved(mate)
                    } else {
                        Fast::Unproven
                    }
                }
                None => Fast::Missed,
            }
        } else {
            Fast::Skipped
        }
    } else if let Some((delimiter, commas)) = ctx.guard {
        let data = ctx.a.slab.data();
        match guard_span(data, from, end, delimiter, commas) {
            Some(len) => {
                let span = Some(Proof::Csv {
                    bytes: &data[from..from + len],
                    delimiter,
                });
                match ctx.bi.lookup(
                    ctx.b,
                    &ctx.a.slab,
                    None,
                    hash,
                    ctx.key_size,
                    ctx.opt,
                    span,
                    &mut scratch.fb,
                ) {
                    Some((mate, true)) => Fast::Proved(mate),
                    Some(_) => Fast::Unproven,
                    None => Fast::Missed,
                }
            }
            None => Fast::Skipped,
        }
    } else {
        Fast::Skipped
    };
    match fast {
        Fast::Proved(_) => scratch.refused = 0,
        Fast::Unproven => scratch.refused = (scratch.refused + 1).min(PROOF_BACKOFF),
        Fast::Missed | Fast::Skipped => {}
    }
    // An unproven candidate has had its bytes compared already, over the
    // same span the parsed row would offer: asking again cannot succeed.
    let proof_again = attempt && !matches!(fast, Fast::Unproven);
    match fast {
        Fast::Proved(mate) => {
            out.matched += 1;
            mark_seen(seen, mate);
            if out.kept.on {
                keep_skipped(ctx, out, mate);
            }
            return;
        }
        Fast::Missed => {
            out.removed_total += 1;
            if ctx.keep_picks && (ctx.exporting || out.removed.len() <= ctx.cap) {
                out.removed.push(Pick { row, mate: -1 });
                if out.kept.on {
                    keep_a(ctx, out, row, from, end);
                }
            }
            return;
        }
        Fast::Unproven | Fast::Skipped => {}
    }

    ctx.a.fields_at(from as u64, &mut scratch.fa);
    // `scratch.fb` is the lookup's scratch, and on a hit it already holds the
    // mate's fields: that is what the key columns were matched against.
    // A's row up to the end of the last column either file wants. The
    // end has to be a boundary in A as well: a quoted field ends on its
    // closing quote, and what follows is not part of the run.
    let csv_span = ctx
        .span_tail
        .filter(|_| proof_again)
        .and_then(|(slot, delimiter, _)| {
            let f = scratch.fa[slot];
            if !field::is_real(f) {
                return None;
            }
            let data = ctx.a.slab.data();
            let to = field::offset_of(f) + field::len_of(f);
            if to < from || to > data.len() {
                return None;
            }
            if to < data.len() && data[to] != delimiter && data[to] != b'\n' {
                return None;
            }
            Some(Proof::Csv {
                bytes: &data[from..to],
                delimiter,
            })
        });
    // Through the byte that closes the last value either file wants --
    // whichever it turns out to be, since two objects need not list their
    // names in the same order. `ctx.width` and not `ctx.nc`: the keys have to be
    // inside the run for it to stand in for the key comparison.
    let span = csv_span.or_else(|| {
        if !ctx.json_proof {
            return None;
        }
        if scratch.refused >= PROOF_BACKOFF && k & (PROOF_BACKOFF - 1) != 0 {
            return None;
        }
        let data = ctx.a.slab.data();
        let mut t = from;
        for &f in scratch.fa.iter().take(ctx.width) {
            if !field::is_real(f) {
                continue;
            }
            let e = field::offset_of(f) + field::len_of(f);
            if e > t {
                t = e;
            }
        }
        if t >= end || t < from {
            return None;
        }
        Some(Proof::Json {
            bytes: &data[from..t + 1],
        })
    });
    let Some((mate, same_bytes)) = ctx.bi.lookup(
        ctx.b,
        &ctx.a.slab,
        Some(&scratch.fa),
        hash,
        ctx.key_size,
        ctx.opt,
        span,
        &mut scratch.fb,
    ) else {
        out.removed_total += 1;
        if ctx.keep_picks && (ctx.exporting || out.removed.len() <= ctx.cap) {
            out.removed.push(Pick { row, mate: -1 });
            if out.kept.on {
                keep_a(ctx, out, row, from, end);
            }
        }
        return;
    };
    out.matched += 1;
    mark_seen(seen, mate);
    if out.kept.on {
        keep_skipped(ctx, out, mate);
    }
    // `same_bytes` is the proof's own verdict, so it is what the backoff
    // counts. A run of failures means two files where the rows really do
    // differ, and scanning them is work for nothing.
    if ctx.json_proof && span.is_some() {
        if same_bytes {
            scratch.refused = 0;
        } else if scratch.refused < PROOF_BACKOFF {
            scratch.refused += 1;
        }
    }
    // The two rows carry the same bytes across every column either file
    // wants, so no column differs and the mate was never read.
    if same_bytes {
        return;
    }

    let mut any = false;
    // Two loops rather than one with a flag inside it: `ctx.plain` cannot
    // change between columns or between rows, and this is the innermost
    // loop of the whole comparison.
    if ctx.plain {
        for i in 0..ctx.nc {
            let (x, y) = (scratch.fa[ctx.key_size + i], scratch.fb[ctx.key_size + i]);
            let (xa, yb) = (plain_absent(x), plain_absent(y));
            if plain_differs(&ctx.a.slab, x, xa, &ctx.b.slab, y, yb)
                && !(ctx.typed[i] != Typed::Text
                    && !xa
                    && !yb
                    && same_value(ctx.typed[i], &ctx.a.slab, x, &ctx.b.slab, y))
            {
                any = true;
                out.changed_per[i] += 1;
                // Absence is already known rather than asked for again.
                if yb {
                    out.blanked_per[i] += 1;
                }
                if xa {
                    out.filled_per[i] += 1;
                }
            }
        }
    } else {
        for i in 0..ctx.nc {
            let (x, y) = (scratch.fa[ctx.key_size + i], scratch.fb[ctx.key_size + i]);
            let differ = if ctx.typed[i] != Typed::Text {
                ctx.typed[i].differs(
                    &value(&ctx.a.slab, x, ctx.opt),
                    &value(&ctx.b.slab, y, ctx.opt),
                    ctx.opt,
                )
            } else {
                cell_differs(&ctx.a.slab, x, &ctx.b.slab, y, ctx.opt)
            };
            if differ {
                any = true;
                out.changed_per[i] += 1;
                if is_absent(&ctx.b.slab, y, ctx.opt) {
                    out.blanked_per[i] += 1;
                }
                if is_absent(&ctx.a.slab, x, ctx.opt) {
                    out.filled_per[i] += 1;
                }
            }
        }
    }
    if any {
        out.changed_total += 1;
        if ctx.keep_picks && (ctx.exporting || out.changed.len() <= ctx.cap) {
            out.changed.push(Pick { row, mate });
            if out.kept.on {
                keep_a(ctx, out, row, from, end);
                keep_b(ctx, out, mate);
            }
        }
    }
}

/// Copies A's row `row`, bytes `from..end`, into the part's kept rows.
fn keep_a(ctx: &JoinCtx, out: &mut Part, row: i32, from: usize, end: usize) {
    let kept = &mut out.kept;
    let bytes = &ctx.a.slab.data()[from..end];
    Kept::copy(
        &mut kept.on,
        &mut kept.a,
        &mut kept.a_rows,
        row,
        bytes,
        ctx.cap,
    );
}

/// Copies B's row `row` into the part's kept rows.
fn keep_b(ctx: &JoinCtx, out: &mut Part, row: i32) {
    let data = ctx.b.slab.data();
    let (from, end) = (
        ctx.bi.row_at[row as usize] as usize,
        row_end(ctx.bi, row, data.len()),
    );
    let kept = &mut out.kept;
    Kept::copy(
        &mut kept.on,
        &mut kept.b,
        &mut kept.b_rows,
        row,
        &data[from..end],
        ctx.cap,
    );
}

/// How far apart two consecutive mates may be for the rows between them to be
/// kept. A wider gap is two files in different orders, where what lies between
/// is not where the added rows are.
const SKIPPED_SPAN: i32 = 8;

/// Keeps the rows of B between this part's last mate and `mate`: when both
/// files are in the same order, the rows a run of matches skips are B's added
/// rows (or its repeats), and their pages are the ones the mate was just read
/// from. Bounded like a pick list.
fn keep_skipped(ctx: &JoinCtx, out: &mut Part, mate: i32) {
    let last = out.kept.last_mate;
    out.kept.last_mate = mate;
    if last < 0 || mate <= last + 1 || mate - last > SKIPPED_SPAN {
        return;
    }
    for row in last + 1..mate {
        if out.kept.skipped > ctx.cap || !out.kept.on {
            return;
        }
        out.kept.skipped += 1;
        keep_b(ctx, out, row);
    }
}

/// Everything both join paths build once: the row-join context and B's
/// seen bitmap.
fn join_setup<'a>(
    a: &'a Side,
    b: &'a Side,
    bi: &'a RowIndex,
    opt: &'a Options,
    nc: usize,
    exporting: bool,
) -> (JoinCtx<'a>, Vec<AtomicU64>) {
    let key_size = opt.key.len();
    let width = key_size + nc;
    let cap = opt.max_rows;
    // The pick lists are only read for the report (`row_lists`) or a full
    // export; with `--summary` they are discarded, so do not build them.
    let keep_picks = opt.row_lists || exporting;
    // Nothing to normalise: the cell comparison is two bit tests and a memcmp,
    // and neither the absence checks nor the value decoding are reachable.
    let plain = !needs_normalising(opt);

    // Both sides are chunked into one queue and every thread pulls from it.
    //
    // Giving B a thread of its own looked right -- A reads both rows and every
    // compared column where B only asks whether each of its keys exists in A --
    // but measuring it says otherwise: B's four million probes on one thread took
    // 3.1s while A's three ranges took 1.6s each, so the join was waiting on B
    // with three cores idle. A key costs B about two thirds of what it costs A,
    // and B has just as many of them; nothing about that ratio makes a thread the
    // right unit. Chunks do not care which side they came from.
    // Where a byte comparison may stand in for a parse, when the two files are
    // the same shape; see `shared_tail`. `None` compares every pair column by
    // column, which is what a mixed pair, a columnar side or JSON gets.
    let span_tail = match (a.parser(), b.parser()) {
        (Some(pa), Some(pb)) => shared_tail(pa, pb),
        _ => None,
    };
    // The proof's delimiter and delimiter count, for the fast path in the join
    // below. `None` takes the old parse-first path.
    let guard = span_tail.map(|(_, delimiter, last_needed)| (delimiter, last_needed + 1));
    // The same fast path in runs, where an unwanted column sits among the
    // wanted ones; see `Runs`. `None` where there is no gap.
    let runs = span_tail.and_then(|_| a.parser().and_then(Runs::plan));
    // What JSON gets instead, since `shared_tail` cannot promise it an offset.
    // See `json_values_agree`.
    let json_proof =
        nc > 0 && a.slab.dialect() == Dialect::Json && b.slab.dialect() == Dialect::Json;
    let typed = (0..nc)
        .map(|i| Typed::of_pair(a.typed[key_size + i], b.typed[key_size + i]))
        .collect();
    // Which of B's rows some key of A matched.
    //
    // B's half of the join used to answer "is this key in A?" for every key in
    // B, which is ten million probes to find the ten thousand rows A never had.
    // A's half has already asked the same question of the same pairs, from the
    // other end, and thrown the answer away -- so it keeps it here instead, and
    // B's half becomes a walk over a bitmap: a key nobody matched is a key A
    // never had. One bit per row of B, 1.25 MB at ten million.
    //
    // A's chunks all finish before B's start, or the bitmap would be read while
    // it was still being written; the join is two rounds rather than one queue
    // for that reason alone.
    let seen: Vec<AtomicU64> = std::iter::repeat_with(|| AtomicU64::new(0))
        .take(bi.row_at.len().div_ceil(64))
        .collect();

    let ctx = JoinCtx {
        a,
        b,
        bi,
        opt,
        key_size,
        nc,
        width,
        cap,
        keep_picks,
        exporting,
        plain,
        span_tail,
        guard,
        runs,
        json_proof,
        typed,
    };
    (ctx, seen)
}

#[allow(clippy::too_many_arguments)]
fn join(
    a: &Side,
    ai: &RowIndex,
    b: &Side,
    bi: &RowIndex,
    opt: &Options,
    compared: &[String],
    exporting: bool,
    threads: usize,
) -> Result<Joined> {
    let (ctx, seen) = join_setup(a, b, bi, opt, compared.len(), exporting);

    let a_keys = ai.first_row.len();
    let a_ways = ways_for(a_keys);

    let nc = ctx.nc;
    let width = ctx.width;
    let data_len = a.slab.data().len();

    let a_range = |p: usize| -> Part {
        let mut out = Part::blank(nc);
        let mut scratch = RowScratch::new(width, ctx.runs.as_ref().map_or(0, Runs::len));
        let lo = a_keys * p / a_ways;
        let hi = a_keys * (p + 1) / a_ways;
        let keys = &ai.first_row[lo..hi];
        for (i, &row) in keys.iter().enumerate() {
            if let Some(&soon) = keys.get(i + PREFETCH_AHEAD) {
                bi.prefetch(ai.row_hash[soon as usize]);
            }
            // The hash is the one the sweep computed for this row: the same
            // bytes through the same function, so computing it again here would
            // be a second pass over every key in the file for the same number.
            let hash = ai.row_hash[row as usize];
            join_row(
                &ctx,
                &mut out,
                &mut scratch,
                row,
                ai.row_at[row as usize] as usize,
                row_end(ai, row, data_len),
                hash,
                i,
                &seen,
            );
        }
        out
    };

    let next = AtomicUsize::new(0);
    let mut parts = on_threads(threads, || -> Vec<(usize, Part)> {
        let mut mine = Vec::new();
        loop {
            let t = next.fetch_add(1, Ordering::Relaxed);
            if t >= a_ways {
                return mine;
            }
            mine.push((t, a_range(t)));
        }
    });
    parts.sort_by_key(|(t, _)| *t);
    let parts: Vec<Part> = parts.into_iter().map(|(_, part)| part).collect();
    join_tail(
        a, ai, b, bi, opt, compared, exporting, threads, &ctx, &seen, parts, None,
    )
}

/// Everything after A's parts are joined: B's walk over the seen bitmap,
/// the fold into counts and columns, and the report/export rows. Shared by the
/// normal join and the fused join, so both produce the same `Joined`.
#[allow(clippy::too_many_arguments)]
fn join_tail(
    a: &Side,
    ai: &RowIndex,
    b: &Side,
    bi: &RowIndex,
    opt: &Options,
    compared: &[String],
    exporting: bool,
    threads: usize,
    ctx: &JoinCtx,
    seen: &[AtomicU64],
    parts: Vec<Part>,
    kept: Option<&KeptRows>,
) -> Result<Joined> {
    let key_size = ctx.key_size;
    let nc = ctx.nc;
    let cap = ctx.cap;
    let keep_picks = ctx.keep_picks;
    let matched_already = |row: i32| {
        let bit = row as usize;
        seen[bit >> 6].load(Ordering::Relaxed) & (1 << (bit & 63)) != 0
    };

    let b_keys = bi.first_row.len();
    let b_ways = ways_for(b_keys);

    let b_range = |p: usize| -> Capped {
        let mut added = Capped::new(cap, exporting, keep_picks);
        let lo = b_keys * p / b_ways;
        let hi = b_keys * (p + 1) / b_ways;
        // `first_row` is in file order, so the bits are read in ascending order
        // and this walk is sequential where the probe it replaces was random.
        for &row in &bi.first_row[lo..hi] {
            if !matched_already(row) {
                added.push(Pick { row, mate: -1 });
            }
        }
        added
    };

    // B's round; A's parts arrived joined. B only reads the bitmap A's
    // writes finished before this call, so the two rounds stay ordered.
    let mut phases = Phases::new("");

    let next = AtomicUsize::new(0);
    let mut chunks = on_threads(threads, || -> Vec<(usize, Capped)> {
        let mut mine = Vec::new();
        loop {
            let t = next.fetch_add(1, Ordering::Relaxed);
            if t >= b_ways {
                return mine;
            }
            mine.push((t, b_range(t)));
        }
    });
    chunks.sort_by_key(|(t, _)| *t);

    let mut added = Capped::new(cap, exporting, keep_picks);
    for (_, chunk) in chunks {
        for pick in &chunk.held {
            added.push(*pick);
        }
        added.total += chunk.total - chunk.held.len() as i64;
    }

    phases.mark("  join chunks (par)");
    // Merged in chunk order, so the rows kept under the cap are the same rows
    // one thread would have kept.
    let mut changed = Capped::new(cap, exporting, keep_picks);
    let mut removed = Capped::new(cap, exporting, keep_picks);
    let mut matched = 0i64;
    let mut changed_per = vec![0i64; nc];
    let mut blanked_per = vec![0i64; nc];
    let mut filled_per = vec![0i64; nc];
    for part in &parts {
        matched += part.matched;
        for i in 0..nc {
            changed_per[i] += part.changed_per[i];
            blanked_per[i] += part.blanked_per[i];
            filled_per[i] += part.filled_per[i];
        }
        for pick in &part.changed {
            changed.push(*pick);
        }
        changed.total += part.changed_total - part.changed.len() as i64;
        for pick in &part.removed {
            removed.push(*pick);
        }
        removed.total += part.removed_total - part.removed.len() as i64;
    }

    let columns: Vec<ColumnStat> = (0..nc)
        .map(|i| ColumnStat {
            name: compared[i].clone(),
            changed: changed_per[i],
            blanked: blanked_per[i],
            filled: filled_per[i],
        })
        .collect();

    // Only now does anything become a String, and only for the rows kept -- but
    // "only" is up to two hundred thousand rows over twenty columns, which was
    // the last serial second of every run that renders a report.
    //
    // Nothing is kept when the caller asked for no rows and no export. The
    // picks were still collected -- they are how each part reports what it
    // dropped, and the totals above are computed from that -- but none of
    // them becomes a string.
    phases.mark("  merge parts (serial)");
    const NONE: &[Pick] = &[];
    let (keep_removed, keep_added, keep_changed) = if opt.row_lists || exporting {
        (
            removed.held.as_slice(),
            added.held.as_slice(),
            changed.held.as_slice(),
        )
    } else {
        (NONE, NONE, NONE)
    };
    // A row the sweep kept is decoded from its copy; anything else is read
    // from the file, which past page cache means from disk.
    let a_row = |row: i32| match kept.and_then(|k| k.values(a, true, row, opt)) {
        Some(values) => Ok(values),
        None => row_values(a, ai, row, opt),
    };
    let b_row = |row: i32| match kept.and_then(|k| k.values(b, false, row, opt)) {
        Some(values) => Ok(values),
        None => row_values(b, bi, row, opt),
    };
    let mut removed_rows = map_rows(keep_removed, threads, "a removed row", |p| a_row(p.row))?;
    let mut added_rows = map_rows(keep_added, threads, "an added row", |p| b_row(p.row))?;
    // A changed row is shown as its key and the cells that differ, and only
    // an export wants the rest of it. So without one, a changed row decodes
    // its key and the columns whose bytes differ, and nothing else: a column
    // the two rows hold byte for byte is the same value whatever the options,
    // and is never a cell. Decoding both whole rows first was two strings
    // per column per kept row -- two million at fifty thousand rows of
    // twenty columns -- nearly all of them thrown away again unread.
    let mut changed_rows = if exporting {
        Vec::new()
    } else {
        map_rows(keep_changed, threads, "a changed row", |p| {
            let build = |kept: Option<&KeptRows>| {
                let (sa, fa) = row_fields(a, ai, kept, true, p.row);
                let (sb, fb) = row_fields(b, bi, kept, false, p.mate);
                changed_row(sa, &fa, sb, &fb, key_size, &ctx.typed, opt)
            };
            match kept {
                // A copy that does not decode leaves the file to answer, as
                // `KeptRows::values` does.
                Some(_) => build(kept).or_else(|_| build(None)),
                None => build(None),
            }
        })?
    };
    let (mut changed_a, mut changed_b) = if exporting {
        (
            map_rows(keep_changed, threads, "a changed row, A side", |p| {
                a_row(p.row)
            })?,
            map_rows(keep_changed, threads, "a changed row, B side", |p| {
                b_row(p.mate)
            })?,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    phases.mark("  row values (par)");
    sort_rows(&mut removed_rows, key_size);
    sort_rows(&mut added_rows, key_size);
    sort_changed_together(&mut changed_a, &mut changed_b, key_size);
    changed_rows.sort_by(|x, y| compare_keys(&x.0, &y.0, key_size));

    phases.mark("  sorts (serial)");
    let mut changed_cells: Vec<Vec<Cell>> = alloc::sized(changed_rows.len(), "the changed rows")?;
    for (keys, cells) in changed_rows {
        let mut row: Vec<Cell> = alloc::sized(key_size + 1, "a changed row's key")?;
        row.extend(keys.into_iter().map(Cell::Value));
        row.push(Cell::Diffs(cells));
        changed_cells.push(row);
    }
    // With an export: zipped by index rather than by iterator so the two
    // sides can be chunked together; they are the same length and in the
    // same order by construction.
    let mut rows: Vec<usize> = alloc::sized(changed_a.len(), "the changed-row index")?;
    rows.extend(0..changed_a.len());
    let exported: Vec<Vec<Cell>> = map_rows(&rows, threads, "a changed row's cells", |&r| {
        let (ar, br) = (&changed_a[r], &changed_b[r]);
        let mut cells: Vec<CellDiff> = Vec::new();
        for i in 0..nc {
            let (x, y) = (&ar[key_size + i], &br[key_size + i]);
            let differ = ctx.typed[i].differs(x, y, opt);
            if differ {
                alloc::push(
                    &mut cells,
                    CellDiff {
                        column: i,
                        a: alloc::val(x.as_deref(), "a changed cell")?,
                        b: alloc::val(y.as_deref(), "a changed cell")?,
                    },
                    "a changed cell",
                )?;
            }
        }
        let mut row: Vec<Cell> = alloc::sized(key_size + 1, "a changed row's key")?;
        for v in &ar[..key_size] {
            row.push(Cell::Value(alloc::val(
                v.as_deref(),
                "a changed row's key",
            )?));
        }
        row.push(Cell::Diffs(cells));
        Ok(row)
    })?;
    changed_cells.extend(exported);

    phases.mark("  changed cells (par)");
    let counts = Counts {
        a_rows: ai.rows,
        b_rows: bi.rows,
        a_keys: ai.unique_keys(),
        b_keys: bi.unique_keys(),
        matched,
        unchanged: matched - changed.total,
        changed: changed.total,
        added: added.total,
        removed: removed.total,
        a_dup_keys: ai.dup_keys,
        a_dup_rows: ai.dup_rows,
        b_dup_keys: bi.dup_keys,
        b_dup_rows: bi.dup_rows,
    };

    Ok(Joined {
        counts,
        columns,
        changed: changed_cells,
        added: map_rows(&added_rows, threads, "an added row's cells", |r| {
            to_cells(r)
        })?,
        removed: map_rows(&removed_rows, threads, "a removed row's cells", |r| {
            to_cells(r)
        })?,
        changed_a,
        changed_b,
    })
}

// ---------------------------------------------------------------------------
// The fused join
// ---------------------------------------------------------------------------

/// Past page cache the join runs inside A's sweep instead of after it.
///
/// Where page cache ends is the machine's to say: [`crate::memory`] reads it
/// (the container's limit included) and draws the line at half of what is
/// available. Below it the join after the sweep is the faster plan -- on a 4M
/// pair in memory, 1.74s against 2.05s for the join inside the sweep.
///
/// Past memory every pass is a read from disk, and the join used to be the
/// fourth: both sweeps, then both files again for the rows to compare -- at
/// 150M rows as long as a cold read of both files. Built first, B's index is
/// all the join needs from B, so A's rows can be joined as A's sweep finds
/// them, while their pages are still hot, and A is read once.
///
/// The join compares a key's first row, and which row is first is only known
/// once the insertion has seen them all. So every row is joined as though it
/// were first, and the rows that turn out to repeat a key -- `later`, kept by
/// the insertion -- are joined again afterwards and subtracted. Counts are
/// sums, so that is exact. The C, C++, and Zig ports do the same (#241-#243).
///
/// The row lists (`--json`, the HTML report) come out the same as well: each
/// sweep part keeps its chunk's first rows, and `fused_picks` drops the
/// repeats among them and fills any place a repeat held.
///
/// Not with the normalisation flags: they refuse from inside a comparison, and
/// a repeat's refusal could not be taken back. Not with an export, which wants
/// every row, repeats taken back from lists that are not capped.
///
/// `COMPARA_FUSED_JOIN=1` takes this path at any size, which is how the tests
/// reach it. The ordinary join then runs too, and the two must agree -- the
/// counts, and with row lists the rows -- or the run fails. With an export the
/// ordinary join's rows are the ones written.
fn fused_active(a_bytes: Option<u64>, b_bytes: Option<u64>, opt: &Options, threads: usize) -> bool {
    if needs_normalising(opt) {
        return false;
    }
    if fused_forced() {
        return true;
    }
    if opt.export_dir.is_some() {
        return false;
    }
    match (a_bytes, b_bytes) {
        (Some(a), Some(b)) => {
            threads > 1 && a.saturating_add(b) > crate::memory::streaming_threshold(opt.memory)
        }
        _ => false,
    }
}

/// Whether `COMPARA_FUSED_JOIN` is set, and not to "0".
fn fused_forced() -> bool {
    std::env::var("COMPARA_FUSED_JOIN")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// What the fused sweep hands on: the folded totals (later duplicates already
/// subtracted), A's index, the row-join context the sweep ran under, and B's
/// seen bitmap, which the tail's B round reads.
struct FusedJoin<'x> {
    totals: Part,
    /// The rows the sweep copied for the report, if it kept any.
    kept: Option<KeptRows>,
    ai: RowIndex,
    ctx: JoinCtx<'x>,
    seen: Vec<AtomicU64>,
}

/// Sweeps A and joins each row as the sweep finds it, while its pages are hot.
///
/// Each sweep worker owns its part's counts and picks, so no row takes a lock;
/// the parts come back with the chunks and are folded.
fn fused_join<'x>(
    a: &'x Side,
    b: &'x Side,
    bi: &'x RowIndex,
    opt: &'x Options,
    nc: usize,
    threads: usize,
) -> Result<FusedJoin<'x>> {
    let key_size = opt.key.len();
    let (ctx, seen) = join_setup(a, b, bi, opt, nc, false);
    let width = ctx.width;
    let runs_len = ctx.runs.as_ref().map_or(0, Runs::len);

    // The report's rows are copied as they are joined, while their pages are
    // in memory, rather than read back from disk afterwards.
    let sink = RowSink {
        ctx: &ctx,
        seen: &seen,
        nc,
        keep_rows: ctx.keep_picks,
    };
    let (ai, parts) = RowIndex::build(a, key_size, opt, threads, "A ", Some(&sink), None)?;

    // The rows that turned out to repeat a key were joined as though first;
    // join them again and subtract. Re-marking the same seen bits changes
    // nothing: the first row with this key is still matched.
    let mut neg = Part::blank(nc);
    {
        let mut scratch = RowScratch::new(width, runs_len);
        let data_len = a.slab.data().len();
        for (k, &row) in ai.later.iter().enumerate() {
            join_row(
                &ctx,
                &mut neg,
                &mut scratch,
                -1,
                ai.row_at[row as usize] as usize,
                row_end(&ai, row, data_len),
                ai.row_hash[row as usize],
                k,
                &seen,
            );
        }
    }
    let mut totals = Part::blank(nc);
    for swept in &parts {
        totals.add(&swept.part);
    }
    totals.sub(&neg);
    if ctx.keep_picks {
        totals.changed = fused_picks(&ctx, &ai, &seen, &parts, PickList::Changed);
        totals.removed = fused_picks(&ctx, &ai, &seen, &parts, PickList::Removed);
    }
    let kept = ctx.keep_picks.then(|| KeptRows::new(a, b, parts));
    Ok(FusedJoin {
        totals,
        ai,
        ctx,
        seen,
        kept,
    })
}

/// Every part's kept rows, found by row id.
/// Row `row` of `side`'s fields and the slab they point into: the sweep's
/// copy where one was kept, the file otherwise.
fn row_fields<'s>(
    side: &'s Side,
    idx: &RowIndex,
    kept: Option<&'s KeptRows>,
    is_a: bool,
    row: i32,
) -> (&'s Slab, Vec<Field>) {
    if let Some(found) = kept.and_then(|k| k.fields(side, is_a, row)) {
        return found;
    }
    let mut fields = vec![ABSENT; side.width];
    idx.fields_of(side, row, &mut fields);
    (&side.slab, fields)
}

/// A changed row's key values and its differing cells, decoded from the two
/// rows' fields. A column whose two fields hold the same bytes is skipped
/// undecoded: equal bytes are equal values under every option, so `differs`
/// could only have said no. Every other column is decoded and asked, exactly
/// as it was when the whole rows were.
fn changed_row(
    sa: &Slab,
    fa: &[Field],
    sb: &Slab,
    fb: &[Field],
    key_size: usize,
    typed: &[Typed],
    opt: &Options,
) -> Result<(Vec<Val>, Vec<CellDiff>)> {
    let mut keys: Vec<Val> = alloc::sized(key_size, "a changed row's key")?;
    for f in &fa[..key_size] {
        keys.push(value_checked(sa, *f, opt, "a changed row's key")?);
    }
    let mut cells: Vec<CellDiff> = Vec::new();
    for (i, &kind) in typed.iter().enumerate() {
        let (x, y) = (fa[key_size + i], fb[key_size + i]);
        if field::is_real(x) && field::is_real(y) && same_bytes(sa, x, sb, y) {
            continue;
        }
        let xv = value_checked(sa, x, opt, "a changed cell")?;
        let yv = value_checked(sb, y, opt, "a changed cell")?;
        let differ = kind.differs(&xv, &yv, opt);
        if differ {
            alloc::push(
                &mut cells,
                CellDiff {
                    column: i,
                    a: xv,
                    b: yv,
                },
                "a changed cell",
            )?;
        }
    }
    Ok((keys, cells))
}

struct KeptRows {
    a: Vec<Slab>,
    b: Vec<Slab>,
    a_rows: HashMap<i32, (usize, usize)>,
    b_rows: HashMap<i32, (usize, usize)>,
}

impl KeptRows {
    fn new(a: &Side, b: &Side, parts: Vec<SweptPart>) -> KeptRows {
        let mut out = KeptRows {
            a: Vec::with_capacity(parts.len()),
            b: Vec::with_capacity(parts.len()),
            a_rows: HashMap::new(),
            b_rows: HashMap::new(),
        };
        for (i, swept) in parts.into_iter().enumerate() {
            let kept = swept.part.kept;
            for (row, at) in kept.a_rows {
                out.a_rows.entry(row).or_insert((i, at));
            }
            for (row, at) in kept.b_rows {
                out.b_rows.entry(row).or_insert((i, at));
            }
            out.a.push(Slab::owned(kept.a, a.slab.dialect()));
            out.b.push(Slab::owned(kept.b, b.slab.dialect()));
        }
        out
    }

    /// Row `row` of `side`'s fields in its copy, and the copy they point into.
    fn fields(&self, side: &Side, is_a: bool, row: i32) -> Option<(&Slab, Vec<Field>)> {
        let (slabs, rows) = if is_a {
            (&self.a, &self.a_rows)
        } else {
            (&self.b, &self.b_rows)
        };
        let &(part, at) = rows.get(&row)?;
        let slab = &slabs[part];
        let parser = side.parser()?;
        let data = slab.data();
        let mut fields = vec![ABSENT; side.width];
        parser.parse(data, at, data.len(), &mut fields);
        Some((slab, fields))
    }

    /// Row `row` of `side`'s values, from its copy, as `row_values` would
    /// decode them from the file. `None` when no copy was kept, or when it does
    /// not decode -- the file then gives the answer, error included.
    fn values(&self, side: &Side, is_a: bool, row: i32, opt: &Options) -> Option<Vec<Val>> {
        let (slab, fields) = self.fields(side, is_a, row)?;
        let mut out: Vec<Val> = alloc::sized(side.width, "a report row").ok()?;
        for f in &fields {
            out.push(value_checked(slab, *f, opt, "a report cell").ok()?);
        }
        Some(out)
    }
}

/// Which of a part's two pick lists.
#[derive(Clone, Copy)]
enum PickList {
    Changed,
    Removed,
}

impl PickList {
    fn of(self, part: &Part) -> (&[Pick], i64) {
        match self {
            PickList::Changed => (&part.changed, part.changed_total),
            PickList::Removed => (&part.removed, part.removed_total),
        }
    }

    fn take(self, part: &mut Part) -> Vec<Pick> {
        match self {
            PickList::Changed => std::mem::take(&mut part.changed),
            PickList::Removed => std::mem::take(&mut part.removed),
        }
    }
}

/// The rows one list of the report keeps, from the fused sweep's parts.
///
/// The ordinary join's list is the first `cap + 1` rows to land in it, in file
/// order, among the rows that are their key's first. Each sweep part kept its
/// chunk's first `cap + 1`, repeats included, so dropping the repeats leaves
/// every first row up to the last one the part held -- and if the part stopped
/// keeping, the places the repeats held may belong to rows after that one.
/// Those rows are joined again, from there until the list is full: a little of
/// A read twice, against all of it.
fn fused_picks(
    ctx: &JoinCtx,
    ai: &RowIndex,
    seen: &[AtomicU64],
    parts: &[SweptPart],
    list: PickList,
) -> Vec<Pick> {
    let want = ctx.cap + 1;
    // `later` is in file order: the insertion walks the rows in it.
    let repeat = |row: usize| ai.later.binary_search(&(row as i32)).is_ok();
    let data_len = ctx.a.slab.data().len();
    let runs_len = ctx.runs.as_ref().map_or(0, Runs::len);
    let mut out = Vec::new();
    for swept in parts {
        if out.len() >= want {
            break;
        }
        let (held, total) = list.of(&swept.part);
        for pick in held {
            if out.len() >= want {
                break;
            }
            if !repeat(pick.row as usize) {
                out.push(*pick);
            }
        }
        if out.len() >= want || total <= held.len() as i64 {
            continue;
        }
        let from = held.last().map_or(swept.rows.start, |p| p.row as usize + 1);
        let mut part = Part::blank(ctx.nc);
        let mut scratch = RowScratch::new(ctx.width, runs_len);
        for row in from..swept.rows.end {
            if out.len() >= want {
                break;
            }
            if repeat(row) {
                continue;
            }
            join_row(
                ctx,
                &mut part,
                &mut scratch,
                row as i32,
                ai.row_at[row] as usize,
                row_end(ai, row as i32, data_len),
                ai.row_hash[row],
                row - swept.rows.start,
                seen,
            );
            out.extend(list.take(&mut part));
        }
    }
    out
}

/// Bytes of A to read ahead per row of B while B's rows are inserted: the
/// insertion takes about 50ns a row at 150M rows, and the runners' disks read
/// 300-400 MB/s, so about 16 bytes a row is what the disk reads in the time
/// the insertion takes.
const READ_AHEAD_PER_ROW: usize = 16;

/// A page, the unit the read-ahead touches A in.
const PAGE: usize = 4096;

/// Reads the first bytes of each of A's sweep chunks into memory, as many as
/// the disk reads while `rows` rows are inserted, never more than half the
/// file, and nothing more once `stop` is set. The chunks are where
/// `chunk_bounds` will start them, near enough: the guessed split is the next
/// newline after each even division.
///
/// One thread per chunk touches a byte of every page, in order, as the sweep
/// would: each waits on its own faults, so a few readahead windows are in
/// flight rather than the whole budget. Asking for it all with
/// madvise(MADV_WILLNEED) (in 128 KB calls -- larger ones read only the
/// readahead window each) did shorten the 150M A sweep, 131.3s to 125.4s, but
/// queued 2.4 GB at once ahead of the insertion's own reads -- the repeated
/// keys it proves against the file -- and the insertion went from 7.5s to
/// 12.4s.
fn read_ahead(a: &Side, rows: usize, threads: usize, stop: &AtomicBool) {
    let Rows::Text { from, .. } = &a.rows else {
        return;
    };
    let size = a.slab.data().len();
    if *from >= size {
        return;
    }
    let ways = threads.max(1);
    let span = size - from;
    let budget = rows.saturating_mul(READ_AHEAD_PER_ROW).min(span / 2);
    let each = budget / ways;
    let data = a.slab.data();
    std::thread::scope(|scope| {
        for i in 0..ways {
            let start = from + span * i / ways;
            let end = (start + each).min(size);
            scope.spawn(move || {
                let mut seen = 0u8;
                let mut at = start;
                while at < end && !stop.load(Ordering::Relaxed) {
                    seen ^= data[at];
                    at += PAGE;
                }
                std::hint::black_box(seen);
            });
        }
    });
}

/// The fused compare: B's index is built first on the full thread budget, then
/// A's sweep joins each row as it finds it.
#[allow(clippy::too_many_arguments)]
fn compare_fused(
    a_input: Input,
    b_input: Input,
    opt: &Options,
    resolved: &Resolved,
    key_size: usize,
    a_cols: usize,
    b_cols: usize,
    wanted: &[&String],
    total: usize,
) -> Result<EngineResult> {
    let nc = resolved.compared.len();
    let exporting = opt.export_dir.is_some();

    // B first, on the full budget: its index is all the join needs from B.
    // A projected on the full budget too; its sweep joins as it goes.
    let b = b_input.project(wanted, key_size, total)?;
    let a = a_input.project(wanted, key_size, total)?;
    // While B's rows are inserted, on one thread, nothing reads the disk: A's
    // sweep cannot start until B's index is whole. So the reads A's sweep will
    // begin with are started then, by a thread that only asks for them.
    let stop = AtomicBool::new(false);
    let (bi, _) = std::thread::scope(|scope| {
        let (go, wait) = std::sync::mpsc::channel::<usize>();
        let (a, stop) = (&a, &stop);
        scope.spawn(move || {
            if let Ok(rows) = wait.recv() {
                read_ahead(a, rows, total, stop);
            }
        });
        let start = move |rows: usize| {
            let _ = go.send(rows);
        };
        let built = RowIndex::build(&b, key_size, opt, total, "B ", None, Some(&start));
        // A's sweep starts now and reads for itself; the read-ahead stops at
        // its next step rather than queue reads the sweep is already making.
        stop.store(true, Ordering::Relaxed);
        built
    })?;
    let fused = fused_join(&a, &b, &bi, opt, nc, total)?;

    let mut phases = Phases::new("");
    let joined = if exporting {
        // Only forced (`fused_active`): the ordinary join supplies the export,
        // and the fused totals must agree with it.
        let check = join(
            &a,
            &fused.ai,
            &b,
            &bi,
            opt,
            &resolved.compared,
            exporting,
            total,
        )?;
        let same = fused.totals.matched == check.counts.matched
            && fused.totals.changed_total == check.counts.changed
            && fused.totals.removed_total == check.counts.removed
            && (0..nc).all(|i| {
                fused.totals.changed_per[i] == check.columns[i].changed
                    && fused.totals.blanked_per[i] == check.columns[i].blanked
                    && fused.totals.filled_per[i] == check.columns[i].filled
            });
        if !same {
            return Err(Error::new(
                "COMPARA_FUSED_JOIN: the join inside the sweep disagrees with the join after it",
            ));
        }
        check
    } else {
        // The fused totals and picks are the join's A round.
        let joined = join_tail(
            &a,
            &fused.ai,
            &b,
            &bi,
            opt,
            &resolved.compared,
            exporting,
            total,
            &fused.ctx,
            &fused.seen,
            vec![fused.totals],
            fused.kept.as_ref(),
        )?;
        if fused_forced() {
            // How the tests reach this path: the ordinary join must agree,
            // row lists included.
            let check = join(
                &a,
                &fused.ai,
                &b,
                &bi,
                opt,
                &resolved.compared,
                exporting,
                total,
            )?;
            if joined.counts != check.counts
                || joined.columns != check.columns
                || joined.changed != check.changed
                || joined.added != check.added
                || joined.removed != check.removed
            {
                return Err(Error::new(
                    "COMPARA_FUSED_JOIN: the join inside the sweep disagrees with the join after it",
                ));
            }
        }
        joined
    };
    phases.mark("fused join");

    let (dup_a, dup_b) = if opt.row_lists {
        (
            duplicate_section(&a, &fused.ai, opt)?,
            duplicate_section(&b, &bi, opt)?,
        )
    } else {
        (empty_section(opt), empty_section(opt))
    };
    phases.mark("duplicate sections");
    let meta = resolved.meta(&opt.key, a_cols, b_cols);
    let out = assemble(meta, joined, dup_a, dup_b, opt, &resolved.compared);
    phases.mark("assemble");
    out
}

fn sort_rows(rows: &mut [Vec<Val>], key_size: usize) {
    rows.sort_by(|x, y| compare_keys(x, y, key_size));
}

/// Sorts the changed rows by key while keeping the parallel A and B lists in step.
///
/// The permutation moves rows rather than copying them. Cloning instead meant
/// deep-copying every `String` in both lists -- fifty thousand rows over twenty
/// columns, twice -- which was almost the whole of this engine's serial sort
/// time, and none of it was the comparisons.
fn sort_changed_together(a: &mut Vec<Vec<Val>>, b: &mut Vec<Vec<Val>>, key_size: usize) {
    let mut order: Vec<usize> = (0..a.len()).collect();
    order.sort_by(|&p, &q| compare_keys(&a[p], &a[q], key_size));
    permute(a, &order);
    permute(b, &order);
}

/// Reorders `v` so that `v[i]` becomes what `v[order[i]]` was, moving each
/// element exactly once. `order` must be a permutation of `0..v.len()`.
fn permute<T: Default>(v: &mut Vec<T>, order: &[usize]) {
    let mut src = std::mem::take(v);
    *v = order.iter().map(|&i| std::mem::take(&mut src[i])).collect();
}

/// The duplicate-key section a `--summary` run reports instead of building one.
///
/// Empty rather than absent: the section's columns are part of the contract and
/// a reader that walks the sections should find the same shape either way.
fn empty_section(opt: &Options) -> Section {
    let mut cols = opt.key.clone();
    cols.push("count".to_string());
    Section::capped(cols, Vec::new(), opt.max_rows)
}

/// The duplicate-key section: most duplicated first, then by key.
fn duplicate_section(side: &Side, idx: &RowIndex, opt: &Options) -> Result<Section> {
    let key_size = opt.key.len();
    let mut entries: Vec<(Vec<Val>, i64)> = Vec::new();
    for (at, (row, n)) in idx.first_row.iter().zip(&idx.occurrences).enumerate() {
        if *n <= 1 {
            continue;
        }
        let key = match idx.dup_key_values.get(&(at as u32)) {
            Some(values) => values.clone(),
            None => key_values(side, idx, *row, opt)?,
        };
        alloc::push(&mut entries, (key, *n as i64), "a duplicated key")?;
    }
    entries.sort_by(|x, y| {
        y.1.cmp(&x.1)
            .then_with(|| compare_keys(&x.0, &y.0, key_size))
    });

    let total = entries.len();
    entries.truncate(opt.max_rows);
    let mut rows: Vec<Vec<Cell>> = alloc::sized(entries.len(), "a duplicate-section row")?;
    for (key, count) in entries {
        let mut row: Vec<Cell> = alloc::sized(key.len() + 1, "a duplicate-section row")?;
        for v in key {
            row.push(Cell::Value(v));
        }
        row.push(Cell::Count(count));
        rows.push(row);
    }
    let mut cols = opt.key.clone();
    cols.push("count".to_string());
    Ok(Section {
        cols,
        rows,
        truncated: total > opt.max_rows,
    })
}

// ---------------------------------------------------------------------------
// Opening a file, whatever format it is in
// ---------------------------------------------------------------------------

/// A file after its header has been read but before the comparison knows which
/// columns it wants. The two phases are separate because a Parquet file should
/// decode the columns being compared and no others, and that list is not known
/// until both headers have been resolved against each other.
enum Input {
    Text {
        slab: Slab,
        delimiter: u8,
        from: usize,
        header: Vec<String>,
    },
    Parquet {
        reader: parquet::Reader,
        header: Vec<String>,
    },
}

impl Input {
    fn open(path: &Path, opt: &Options) -> Result<Input> {
        // The format is decided by what is in the file, not by its name: a file
        // called `.parquet` that is really a CSV is read as a CSV.
        let probe = Slab::map(path)?;
        if parquet::looks_like_parquet(probe.data()) {
            drop(probe);
            let reader = parquet::Reader::open(path)?;
            let header = reader.column_names();
            return Ok(Input::Parquet { reader, header });
        }
        // A file that begins with the magic and does not end with it is a
        // truncated Parquet file. Reading it as a CSV would report a missing key
        // column, which sends the reader looking in the wrong place entirely.
        if probe.data().starts_with(b"PAR1") {
            return Err(Error::new(format!(
                "{} begins with a Parquet magic number but does not end with one: \
                 the file is truncated",
                path.display()
            )));
        }
        let mut slab = probe;
        let dialect = sniff_dialect(slab.data());
        slab.set_dialect(dialect);
        if dialect == Dialect::Json {
            let header = json_header(&slab, path)?;
            return Ok(Input::Text {
                slab,
                delimiter: b',',
                from: 0,
                header,
            });
        }
        let delimiter = match opt.delimiter_byte()? {
            Some(d) => d,
            None => {
                let data = slab.data();
                detect_delimiter(&data[..next_of1(data, 0, data.len(), b'\n')])
            }
        };
        let (header, from) = csv_header(&slab, delimiter, path)?;
        Ok(Input::Text {
            slab,
            delimiter,
            from,
            header,
        })
    }

    fn header(&self) -> &[String] {
        match self {
            Input::Text { header, .. } | Input::Parquet { header, .. } => header,
        }
    }

    /// The mapped size, for the fused join's size gate. `None` for Parquet:
    /// the fused join is text only.
    fn text_bytes(&self) -> Option<u64> {
        match self {
            Input::Text { slab, .. } => Some(slab.data().len() as u64),
            Input::Parquet { .. } => None,
        }
    }

    /// Reads the file into the join's representation, keeping only `wanted`.
    fn project(self, wanted: &[&String], key_size: usize, threads: usize) -> Result<Side> {
        let width = wanted.len();
        match self {
            Input::Text {
                slab,
                delimiter,
                from,
                header,
            } => {
                let has = |n: &String| header.iter().any(|c| c == n);
                let json = slab.dialect() == Dialect::Json;
                let array = json && json_is_array(slab.data());
                let build = |names: &[&String]| -> RowParser {
                    if json {
                        RowParser::json(
                            names.iter().map(|n| has(n).then(|| (*n).clone())).collect(),
                            key_size,
                            array,
                        )
                    } else {
                        RowParser::csv(
                            delimiter,
                            names
                                .iter()
                                .map(|n| header.iter().position(|c| &c == n))
                                .collect(),
                        )
                    }
                };
                let parser = build(wanted);
                let keys = build(&wanted[..key_size.min(wanted.len())]);
                Ok(Side {
                    slab,
                    rows: Rows::Text { parser, keys, from },
                    width,
                    typed: vec![Typed::Text; width],
                })
            }
            Input::Parquet { reader, header } => {
                let names: Vec<Option<&str>> = wanted
                    .iter()
                    .map(|n| header.iter().any(|c| c == *n).then_some(n.as_str()))
                    .collect();
                let rows = reader.rows();
                let typed = wanted.iter().map(|n| reader.typed(n)).collect();
                let (fields, arena) = reader.project(&names, threads, usize::MAX)?;
                // The mapping is finished with: everything the join reads now
                // lives in the arena, so the file's pages can be given back.
                drop(reader);
                Ok(Side {
                    slab: Slab::owned(arena, Dialect::Raw),
                    rows: Rows::Columnar { fields, rows },
                    width,
                    typed,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn compare(a_path: &Path, b_path: &Path, opt: &Options) -> Result<EngineResult> {
    let total = opt.thread_budget();
    let a_input = Input::open(a_path, opt)?;
    let b_input = Input::open(b_path, opt)?;
    let resolved = resolve(a_input.header(), b_input.header(), opt)?;

    let key_size = opt.key.len();
    let a_cols = a_input.header().len();
    let b_cols = b_input.header().len();
    let wanted: Vec<&String> = opt.key.iter().chain(&resolved.compared).collect();

    if fused_active(a_input.text_bytes(), b_input.text_bytes(), opt, total) {
        return compare_fused(
            a_input, b_input, opt, &resolved, key_size, a_cols, b_cols, &wanted, total,
        );
    }

    // The two files share nothing until the join, so they are read at the same
    // time, and each is split further: two files across four cores is two chunks
    // each, so the whole machine is busy rather than half of it.
    let per_file = (total / 2).max(1);
    let prepare = |input: Input, tag: &'static str| -> Result<(Side, RowIndex)> {
        let side = input.project(&wanted, key_size, per_file)?;
        let (index, _) = RowIndex::build(&side, key_size, opt, per_file, tag, None, None)?;
        Ok((side, index))
    };
    let b_held = parallel::Once::new(b_input);
    let read_b = || -> Result<(Side, RowIndex)> {
        let input = b_held
            .take()
            .ok_or_else(|| Error::new("the B side was read twice"))?;
        prepare(input, "B ")
    };
    let (from_a, from_b) = std::thread::scope(|scope| {
        let worker = parallel::spawn(scope, &read_b);
        let mine = prepare(a_input, "A ");
        let theirs = worker
            .join()
            .unwrap_or_else(|_| Err(Error::new("a file reader panicked")));
        (mine, theirs)
    });
    let (a, ai) = from_a?;
    let (b, bi) = from_b?;
    let mut phases = Phases::new("");

    // Both sections are empty when the caller asked for no rows: the duplicate
    // tallies the summary prints come off the index, not from here, and this
    // walks every duplicated key to decode one row for each of them.
    let (dup_a, dup_b) = if opt.row_lists {
        (
            duplicate_section(&a, &ai, opt)?,
            duplicate_section(&b, &bi, opt)?,
        )
    } else {
        (empty_section(opt), empty_section(opt))
    };
    phases.mark("duplicate sections");
    let joined = join(
        &a,
        &ai,
        &b,
        &bi,
        opt,
        &resolved.compared,
        opt.export_dir.is_some(),
        total,
    )?;

    phases.mark("join");
    let meta = resolved.meta(&opt.key, a_cols, b_cols);
    let out = assemble(meta, joined, dup_a, dup_b, opt, &resolved.compared);
    phases.mark("assemble");
    out
}

/// The turbo engine has no optional dependency.
pub fn available() -> bool {
    true
}

/// Whether this file is in a format only this engine reads.
///
/// The other engines are CSV readers, so a Parquet or JSON input has exactly one
/// backend that can answer for it — and `--engine auto` should choose that one
/// rather than hand the file to DuckDB's CSV reader and report the parse error
/// it makes of a binary footer.
pub fn only_this_engine_reads(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut head = [0u8; 64];
    let read = file.read(&mut head).unwrap_or(0);
    if read >= 4 && &head[..4] == b"PAR1" {
        // The magic is at both ends, so a CSV that happens to start with it is
        // not mistaken for Parquet.
        let mut tail = [0u8; 4];
        if file.seek(SeekFrom::End(-4)).is_ok() && file.read_exact(&mut tail).is_ok() {
            return &tail == b"PAR1";
        }
    }
    sniff_dialect(&head[..read]) == Dialect::Json
}

// ---------------------------------------------------------------------------
// Looking at one file
// ---------------------------------------------------------------------------

/// What a single file says about itself: its columns, and its rows where they
/// are free to know.
pub struct Shape {
    pub format: &'static str,
    pub columns: Vec<String>,
    /// Parquet carries a row count in its footer. A text file does not, and
    /// counting would mean reading all of it — which is not what a reader asking
    /// "what is in here?" is paying for.
    pub rows: Option<i64>,
}

/// The columns of one file, and its row count where the format states one.
///
/// This is the format detection the comparison itself uses -- the bytes decide,
/// not the extension -- so a file this reports on is a file `compare` will read
/// the same way.
pub fn shape(path: &Path, opt: &Options) -> Result<Shape> {
    let input = Input::open(path, opt)?;
    Ok(match input {
        Input::Parquet { reader, header } => Shape {
            format: "parquet",
            rows: Some(reader.rows() as i64),
            columns: header,
        },
        Input::Text { slab, header, .. } => Shape {
            format: if slab.dialect() == Dialect::Json {
                "ndjson"
            } else {
                "csv"
            },
            rows: None,
            columns: header,
        },
    })
}

/// The first `want` rows of a file, as text, with its column names.
///
/// A preview must not pay for the file. Parquet goes through `Reader::project`
/// with a row limit, which stops decoding on a page boundary at or past it, so
/// ten rows of a four-gigabyte file read one page per column rather than all of
/// them. Text stops after `want` rows of the parser's own walk.
///
/// The values come back through the engine's parsers, not a second copy of
/// them: a quoted CSV field is unquoted here exactly as the comparison would
/// unquote it, and a `\u` escape in ndjson is decoded the same way. A preview
/// that disagreed with the comparison would be worse than none.
pub fn head(path: &Path, want: usize, opt: &Options) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let input = Input::open(path, opt)?;
    let header: Vec<String> = input.header().to_vec();
    let width = header.len();
    if want == 0 || width == 0 {
        return Ok((header, Vec::new()));
    }

    match input {
        Input::Parquet { reader, .. } => {
            let rows = reader.rows().min(want);
            if rows == 0 {
                return Ok((header, Vec::new()));
            }
            let names: Vec<Option<&str>> = header.iter().map(|n| Some(n.as_str())).collect();
            let (fields, arena) = reader.project(&names, opt.thread_budget(), want)?;
            drop(reader);
            let slab = Slab::owned(arena, Dialect::Raw);
            let mut out = Vec::with_capacity(rows);
            for r in 0..rows {
                out.push(render_row(&slab, &fields[r * width..(r + 1) * width]));
            }
            Ok((header, out))
        }
        Input::Text {
            slab,
            delimiter,
            from,
            ..
        } => {
            let json = slab.dialect() == Dialect::Json;
            let parser = if json {
                RowParser::json(
                    header.iter().cloned().map(Some).collect(),
                    0,
                    json_is_array(slab.data()),
                )
            } else {
                RowParser::csv(delimiter, (0..width).map(Some).collect())
            };
            let data = slab.data();
            let end = data.len();
            let mut at = from;
            let mut fields = vec![ABSENT; width];
            let mut out = Vec::with_capacity(want);
            while at < end && out.len() < want {
                at = parser.parse(data, at, end, &mut fields);
                out.push(render_row(&slab, &fields));
                fields.iter_mut().for_each(|f| *f = ABSENT);
            }
            Ok((header, out))
        }
    }
}

/// One row of field words as text. An absent or over-long field prints empty:
/// a preview says what is there, and a short row is itself worth seeing.
fn render_row(slab: &Slab, fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .map(|&f| {
            if field::is_real(f) {
                text_of(slab, f)
            } else {
                String::new()
            }
        })
        .collect()
}
