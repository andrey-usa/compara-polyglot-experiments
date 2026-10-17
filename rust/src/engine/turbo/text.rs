//! The two text readers: CSV, and newline-delimited JSON.
//!
//! They are one module because they are one design. A JSON value is a contiguous
//! run of bytes in the file exactly as a CSV field is, so a field stays an offset
//! and a length for both and nothing downstream has to know which format it came
//! from. What differs is how a row is walked — by column number, or by key — and
//! how a value is escaped, which [`super::slab`] settles.
//!
//! The two sides of a comparison need not agree: a CSV export compares against
//! the JSON the same pipeline emits, and the key order on each side is free to
//! differ, because the JSON reader joins by name.

use super::field::{
    ABSENT, Delims, Field, Scan, Swar, Wide, next_of1_with, next_of2, next_of2_with, next_of3_with,
    pack, skip_quoted, skip_quoted_with,
};
use super::slab::{Dialect, Slab, text_of};
use crate::error::{Error, Result};
use std::path::Path;

/// Newline-delimited JSON if the first thing that is not whitespace is a brace
/// or a bracket: a top-level JSON array is the same rows, wrapped.
///
/// A CSV header can begin with anything else, and a `{` or `[` in the first
/// column of a CSV header is not something this project has ever had to read.
pub(super) fn sniff_dialect(data: &[u8]) -> Dialect {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    for &b in data.iter().take(64) {
        if json_space(b) {
            continue;
        }
        return if b == b'{' || b == b'[' {
            Dialect::Json
        } else {
            Dialect::Csv
        };
    }
    Dialect::Csv
}

fn json_space(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

/// Whether the file opens with a top-level `[`: the rows then sit inside a
/// JSON array, and row boundaries answer to its brackets and separators.
/// Only asked once per file, where the parser is built.
pub(super) fn json_is_array(data: &[u8]) -> bool {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    data.iter()
        .take(64)
        .find(|&&b| !json_space(b))
        .is_some_and(|&b| b == b'[')
}

/// Guesses the delimiter from the header line, defaulting to a comma.
pub(super) fn detect_delimiter(header: &[u8]) -> u8 {
    let mut best = b',';
    let mut best_count = -1i64;
    for c in *b",;\t|" {
        let n = header.iter().filter(|&&b| b == c).count() as i64;
        if n > best_count {
            best = c;
            best_count = n;
        }
    }
    best
}

/// The CSV header row's names, and where the first data row starts.
pub(super) fn csv_header(slab: &Slab, delimiter: u8, path: &Path) -> Result<(Vec<String>, usize)> {
    let data = slab.data();
    let end = data.len();
    if end == 0 {
        return Err(Error::new(format!(
            "file has no header row: {}",
            path.display()
        )));
    }
    let mut names = Vec::new();
    let mut pos = 0usize;
    loop {
        let (field, next) = if pos < end && data[pos] == b'"' {
            let close = skip_quoted(data, pos + 1, end);
            let body_end = close.saturating_sub(1).max(pos + 1);
            (
                quoted_field(Swar, data, pos + 1, body_end),
                next_of2(data, close, end, delimiter, b'\n'),
            )
        } else {
            let next = next_of2(data, pos, end, delimiter, b'\n');
            (plain_field(data, pos, next), next)
        };
        names.push(text_of(slab, field));
        if next >= end {
            return Ok((names, end));
        }
        if data[next] == b'\n' {
            return Ok((names, next + 1));
        }
        pos = next + 1;
    }
}

/// A JSON file has no header row, so the column names are the keys of the first
/// object, in the order it lists them. A top-level array of objects answers
/// the same way; an empty one has no object to take the names from.
pub(super) fn json_header(slab: &Slab, path: &Path) -> Result<Vec<String>> {
    let data = slab.data();
    let end = data.len();
    let mut names: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos < end && json_space(data[pos]) {
        pos += 1;
    }
    // A top-level array's opening bracket wraps the rows; it is not one.
    if pos < end && data[pos] == b'[' {
        pos += 1;
        while pos < end && json_space(data[pos]) {
            pos += 1;
        }
    }
    if pos >= end || data[pos] != b'{' {
        return Err(Error::new(format!(
            "file has no JSON object to read: {}",
            path.display()
        )));
    }
    pos += 1;
    loop {
        while pos < end && json_space(data[pos]) {
            pos += 1;
        }
        if pos >= end || data[pos] == b'}' {
            break;
        }
        if data[pos] == b',' {
            pos += 1;
            continue;
        }
        if data[pos] != b'"' {
            break;
        }
        let from = pos + 1;
        let (close, escaped) = skip_json_string(Swar, data, pos, end);
        if close <= from {
            break;
        }
        names.push(text_of(
            slab,
            pack(from as u64, (close - 1 - from) as u64, escaped),
        ));
        pos = close;
        while pos < end && json_space(data[pos]) {
            pos += 1;
        }
        if pos >= end || data[pos] != b':' {
            break;
        }
        pos += 1;
        while pos < end && json_space(data[pos]) {
            pos += 1;
        }
        if pos >= end {
            break;
        }
        pos = match data[pos] {
            b'"' => skip_json_string(Swar, data, pos, end).0,
            b'{' | b'[' => skip_json_nested(Swar, data, pos, end),
            _ => {
                let mut at = pos;
                while at < end && data[at] != b',' && data[at] != b'}' && !json_space(data[at]) {
                    at += 1;
                }
                at
            }
        };
    }
    if names.is_empty() {
        return Err(Error::new(format!(
            "the first JSON object has no keys: {}",
            path.display()
        )));
    }
    Ok(names)
}

/// Skips one JSON string starting at its opening quote, returning the offset one
/// past the closing quote and whether the string holds a backslash.
// Inlined: the JSON parse calls it for every key and every string value --
// a hundred million calls on the 2M ndjson pair, each a few dozen
// instructions of which the call was a fair share.
#[inline(always)]
fn skip_json_string<S: Scan>(scan: S, data: &[u8], mut at: usize, end: usize) -> (usize, bool) {
    at += 1; // the opening quote
    let mut escaped = false;
    loop {
        let stop = next_of2_with(scan, data, at, end, b'"', b'\\');
        if stop >= end {
            return (end, escaped);
        }
        if data[stop] == b'"' {
            return (stop + 1, escaped);
        }
        escaped = true;
        at = stop + 2; // the backslash and whatever it escapes
        if at > end {
            return (end, escaped);
        }
    }
}

/// Skips a nested object or array, which is not a cell value.
fn skip_json_nested<S: Scan>(scan: S, data: &[u8], mut pos: usize, end: usize) -> usize {
    let mut depth = 0i32;
    while pos < end {
        match data[pos] {
            b'"' => {
                pos = skip_json_string(scan, data, pos, end).0;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                pos += 1;
                if depth <= 0 {
                    return pos;
                }
                continue;
            }
            _ => {}
        }
        pos += 1;
    }
    end
}

/// A quoted CSV field, flagged when it holds a doubled quote. A quote inside the
/// body can only be half of such a pair, which is what makes the test a single
/// scan and lets the unescaping wait until the bytes are actually read.
#[inline(always)]
fn quoted_field<S: Scan>(scan: S, data: &[u8], from: usize, to: usize) -> Field {
    let escaped = next_of1_with(scan, data, from, to, b'"') < to;
    pack(from as u64, (to - from) as u64, escaped)
}

/// An unquoted field, with a trailing carriage return stripped so CRLF behaves
/// like LF.
fn plain_field(data: &[u8], from: usize, to: usize) -> Field {
    let mut stop = to;
    if stop > from && data[stop - 1] == b'\r' {
        stop -= 1;
    }
    pack(from as u64, (stop - from) as u64, false)
}

/// The offset of the newline ending the row that starts at `pos`.
#[inline(always)]
fn end_of_row<S: Scan>(scan: S, data: &[u8], pos: usize, end: usize) -> usize {
    let mut at = pos;
    while at < end {
        let next = next_of2_with(scan, data, at, end, b'\n', b'"');
        if next >= end {
            return end;
        }
        if data[next] == b'"' {
            at = skip_quoted_with(scan, data, next + 1, end);
            continue;
        }
        return next;
    }
    end
}

/// Splits rows into fields, projecting straight to the columns asked for.
///
/// The CSV form is addressed by column number: once the last needed column has
/// been read the rest of the row is skipped to its newline without its fields
/// ever being delimited, so on twenty columns keyed on the first two most of a
/// row is never looked at. The JSON form is addressed by key instead, so it
/// walks the whole object — but only once, and one hash per key rather than a
/// search per wanted column, which at twenty columns would be four hundred
/// comparisons a row.
pub(super) enum RowParser {
    Csv {
        delimiter: u8,
        last_needed: usize,
        /// Which slots each column of the file feeds. Inverted from the wanted
        /// list once, so storing a field is a lookup rather than a walk.
        slots_for: Vec<Vec<u16>>,
        /// The one slot each column feeds, or a sentinel. Almost every column
        /// feeds exactly one, and the hot path stores through this rather than
        /// chasing a pointer into `slots_for` and looping -- per column, per
        /// parse, and a row is parsed about two and a half times: once by the
        /// sweep, then again for each side of the join and once more when a
        /// probe compares keys.
        single: Vec<i32>,
    },
    Json {
        /// The key whose value belongs in each slot; `None` for a column this
        /// file does not have.
        wanted: Vec<Option<String>>,
        /// Open-addressed name to slot, so a key costs one hash.
        slots: Vec<i32>,
        slot_mask: usize,
        /// Slots below this are key columns: the first value wins, and once
        /// every slot is a filled key column the rest of the object is skipped.
        key_size: usize,
        /// What the table answers for `wanted[i]`: `i` itself unless the same
        /// name also fills an earlier slot, and -1 for a missing column. Names
        /// arrive in the same order row after row, usually slot order, so the
        /// member after slot `i` is tried as `i + 1` first, and a right guess
        /// has to give exactly the slot the table would.
        canon: Vec<i32>,
        /// The file wraps its rows in a top-level array: `[`, `]` and the `,`
        /// separators are container syntax, and an object can span lines.
        /// ndjson keeps the plain one-memchr row end -- it is most of the
        /// JSON this reader ever sees, and the container walk costs it
        /// three quarters again of a sweep.
        array: bool,
    },
}

/// `single[column]` when the column feeds no slot, and when it feeds more
/// than one.
const NONE: i32 = -1;
const MANY: i32 = -2;

/// A word at a time rather than a byte at a time: the first and last eight
/// bytes (four, or three single bytes, for a shorter name) and the length. A
/// byte loop is a serial multiply per byte, twenty names a row; collisions are
/// settled by comparing the name, so this only has to spread them.
fn name_hash(s: &[u8]) -> u64 {
    let n = s.len();
    let (a, b) = if n >= 8 {
        (
            u64::from_le_bytes(s[..8].try_into().unwrap()),
            u64::from_le_bytes(s[n - 8..].try_into().unwrap()),
        )
    } else if n >= 4 {
        (
            u32::from_le_bytes(s[..4].try_into().unwrap()) as u64,
            u32::from_le_bytes(s[n - 4..].try_into().unwrap()) as u64,
        )
    } else if n > 0 {
        (
            s[0] as u64 | (s[n / 2] as u64) << 8 | (s[n - 1] as u64) << 16,
            0,
        )
    } else {
        (0, 0)
    };
    let h =
        (a ^ b.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ n as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^ (h >> 31)
}

/// Where the wanted part of a row ends, when two files agree closely enough for
/// one to be measured against the other in bytes.
///
/// The join's expensive question is whether a matched pair differs, and it
/// answers it by parsing both rows into fields. It does not have to. If the two
/// rows are byte-identical up to the end of the last column either file wants,
/// then every column in between is byte-identical too, and no parse can say
/// otherwise -- so the pair is unchanged and the mate never needs reading.
///
/// That holds only when both sides really are the same shape: the same
/// delimiter and the same column-to-slot mapping. Two files whose headers are
/// ordered differently can carry identical bytes and mean different things,
/// which is what comparing `slots_for` rules out -- the whole mapping, not the
/// one-slot shortcut derived from it, because two columns that each feed several
/// slots can agree on how many without agreeing on which. JSON has no such
/// prefix -- its keys may come in any order -- so it never qualifies.
///
/// Returns the slot holding the last wanted column, the delimiter that ends
/// it, and the source column index of that column (so the caller can count
/// delimiters to it without parsing).
pub(super) fn shared_tail(a: &RowParser, b: &RowParser) -> Option<(usize, u8, usize)> {
    match (a, b) {
        (
            RowParser::Csv {
                delimiter: da,
                last_needed: la,
                slots_for: fa,
                ..
            },
            RowParser::Csv {
                delimiter: db,
                slots_for: fb,
                ..
            },
        ) if da == db && fa == fb => fa[*la].first().map(|slot| (*slot as usize, *da, *la)),
        _ => None,
    }
}

/// The proof's byte span without parsing the row.
///
/// `csv_span` in the join reads it off the guard field's offsets, which means
/// parsing every column first. The guard is source column `guard_src`, so its
/// end is the (`guard_src`+1)th delimiter from the row start -- countable with
/// the delimiter cursor alone, skipping a quoted field the way the row parse
/// does. Returns `None` when the row ends first (short or malformed), which
/// falls back to the parse.
///
/// The span is the field bytes, not including the delimiter that ends them --
/// the same `data[from..to]` the field-offset path builds -- so the proof's
/// boundary check in `row_matches` applies unchanged. A guard that is the
/// row's last column ends at the newline instead of a delimiter.
pub(super) fn guard_span(
    data: &[u8],
    lo: usize,
    hi: usize,
    delimiter: u8,
    commas: usize,
) -> Option<usize> {
    match Wide::detect() {
        // Safety: `w` proves the CPU has AVX2.
        Some(w) => unsafe { guard_span_wide(w, data, lo, hi, delimiter, commas) },
        None => guard_span_with(Swar, data, lo, hi, delimiter, commas),
    }
}

/// [`guard_span_with`], compiled for AVX2 in a baseline build.
///
/// # Safety
/// Holding a `Wide` is the proof the CPU has AVX2; nothing else is required.
#[cfg_attr(
    all(
        target_arch = "x86_64",
        not(any(target_feature = "avx2", target_feature = "avx512bw"))
    ),
    target_feature(enable = "avx2")
)]
unsafe fn guard_span_wide(
    w: Wide,
    data: &[u8],
    lo: usize,
    hi: usize,
    delimiter: u8,
    commas: usize,
) -> Option<usize> {
    guard_span_with(w, data, lo, hi, delimiter, commas)
}

#[inline(always)]
fn guard_span_with<S: Scan>(
    scan: S,
    data: &[u8],
    lo: usize,
    hi: usize,
    delimiter: u8,
    commas: usize,
) -> Option<usize> {
    let mut at = lo;
    let mut delims = Delims::new(scan, data, lo, hi, delimiter, b'\n');
    for i in 0..commas {
        let cur = if at < hi && data[at] == b'"' {
            let close = skip_quoted_with(scan, data, at + 1, hi);
            delims.next(close)
        } else {
            delims.next(at)
        };
        if cur >= hi {
            return None;
        }
        let last = i + 1 == commas;
        if data[cur] == delimiter {
            at = cur + 1;
            if last {
                return Some(cur - lo);
            }
        } else if last && data[cur] == b'\n' {
            return Some(cur - lo);
        } else {
            return None;
        }
    }
    None
}

/// The gapped proof's plan, where a column nobody wants sits among the wanted
/// ones -- an ignored `updated_at` in the middle of the row, say.
///
/// `guard_span` covers one run: A's row from its start through the last column
/// either file wants, compared as bytes. An unwanted column inside that run that
/// differs -- and an ignored timestamp differs on every row -- fails the proof
/// every time, and every row is parsed. So the wanted columns are compared as
/// runs instead, and the unwanted ones between them skipped by field count in
/// each row on its own: an ignored value that differs cannot stop a row proving
/// equal, and one that differs in length cannot misalign what follows. `seg[j]`
/// wanted fields are followed by `gap[j]` unwanted ones; the last run has no
/// gap. The C and C++ ports have the same proof (#229, #230).
pub(super) struct Runs {
    seg: Vec<usize>,
    gap: Vec<usize>,
}

impl Runs {
    /// `None` where there is no gap, and the one-run proof applies unchanged.
    /// Only called once `shared_tail` has agreed the two sides' shapes, so A's
    /// map of wanted columns is B's too.
    pub(super) fn plan(a: &RowParser) -> Option<Runs> {
        let RowParser::Csv {
            last_needed,
            slots_for,
            ..
        } = a
        else {
            return None;
        };
        let (mut seg, mut gap) = (vec![0usize], vec![0usize]);
        for slots in &slots_for[..=*last_needed] {
            if slots.is_empty() {
                *gap.last_mut()? += 1;
                continue;
            }
            if *gap.last()? > 0 {
                seg.push(0);
                gap.push(0);
            }
            *seg.last_mut()? += 1;
        }
        (seg.len() > 1).then_some(Runs { seg, gap })
    }

    pub(super) fn len(&self) -> usize {
        self.seg.len()
    }

    /// Where each run starts in this row and how many bytes it takes, through
    /// the byte that closes it, into `out`. `false` where the row ends first,
    /// which falls back to the parse.
    pub(super) fn of_row(
        &self,
        data: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
        out: &mut [(usize, usize)],
    ) -> bool {
        match Wide::detect() {
            // Safety: `w` proves the CPU has AVX2.
            Some(w) => unsafe { self.of_row_wide(w, data, lo, hi, delimiter, out) },
            None => self.of_row_with(Swar, data, lo, hi, delimiter, out),
        }
    }

    /// [`Runs::of_row_with`], compiled for AVX2 in a baseline build.
    ///
    /// # Safety
    /// Holding a `Wide` is the proof the CPU has AVX2; nothing else is required.
    #[cfg_attr(
        all(
            target_arch = "x86_64",
            not(any(target_feature = "avx2", target_feature = "avx512bw"))
        ),
        target_feature(enable = "avx2")
    )]
    unsafe fn of_row_wide(
        &self,
        w: Wide,
        data: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
        out: &mut [(usize, usize)],
    ) -> bool {
        self.of_row_with(w, data, lo, hi, delimiter, out)
    }

    #[inline(always)]
    fn of_row_with<S: Scan>(
        &self,
        scan: S,
        data: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
        out: &mut [(usize, usize)],
    ) -> bool {
        let mut delims = Delims::new(scan, data, lo, hi, delimiter, b'\n');
        let mut at = lo;
        let n = self.seg.len();
        for (j, (slot, &fields)) in out.iter_mut().zip(&self.seg).enumerate() {
            let last = j + 1 == n;
            let Some(end) = past_fields(scan, &mut delims, data, at, hi, delimiter, fields, last)
            else {
                return false;
            };
            *slot = (at, end - at);
            if last {
                break;
            }
            match past_fields(
                scan,
                &mut delims,
                data,
                end,
                hi,
                delimiter,
                self.gap[j],
                false,
            ) {
                Some(next) => at = next,
                None => return false,
            }
        }
        true
    }

    /// Whether B's row `b[lo..hi]` holds each of A's runs where its own gaps
    /// put it. A run's bytes include the byte that closes it, so equal bytes
    /// leave B at the start of its next field, as they leave A.
    pub(super) fn matches(
        &self,
        a: &[u8],
        spans: &[(usize, usize)],
        b: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
    ) -> bool {
        match Wide::detect() {
            // Safety: `w` proves the CPU has AVX2.
            Some(w) => unsafe { self.matches_wide(w, a, spans, b, lo, hi, delimiter) },
            None => self.matches_with(Swar, a, spans, b, lo, hi, delimiter),
        }
    }

    /// [`Runs::matches_with`], compiled for AVX2 in a baseline build.
    ///
    /// # Safety
    /// Holding a `Wide` is the proof the CPU has AVX2; nothing else is required.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(
        all(
            target_arch = "x86_64",
            not(any(target_feature = "avx2", target_feature = "avx512bw"))
        ),
        target_feature(enable = "avx2")
    )]
    unsafe fn matches_wide(
        &self,
        w: Wide,
        a: &[u8],
        spans: &[(usize, usize)],
        b: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
    ) -> bool {
        self.matches_with(w, a, spans, b, lo, hi, delimiter)
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn matches_with<S: Scan>(
        &self,
        scan: S,
        a: &[u8],
        spans: &[(usize, usize)],
        b: &[u8],
        lo: usize,
        hi: usize,
        delimiter: u8,
    ) -> bool {
        let mut delims = Delims::new(scan, b, lo, hi, delimiter, b'\n');
        let mut at = lo;
        let n = self.seg.len();
        for (j, &(from, len)) in spans.iter().enumerate().take(n) {
            if len > hi - at || a[from..from + len] != b[at..at + len] {
                return false;
            }
            at += len;
            if j + 1 == n {
                break;
            }
            match past_fields(scan, &mut delims, b, at, hi, delimiter, self.gap[j], false) {
                Some(next) => at = next,
                None => return false,
            }
        }
        true
    }
}

/// Past the byte that closes the `n`th field from `at`, or `None` where the row
/// ends first. With `may_end` the last of them may close on the line ending: a
/// run that ends in the row's last column has nothing else to close it.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn past_fields<S: Scan>(
    scan: S,
    delims: &mut Delims<'_, S>,
    data: &[u8],
    mut at: usize,
    hi: usize,
    delimiter: u8,
    n: usize,
    may_end: bool,
) -> Option<usize> {
    for i in 0..n {
        let cur = if at < hi && data[at] == b'"' {
            delims.next(skip_quoted_with(scan, data, at + 1, hi))
        } else {
            delims.next(at)
        };
        if cur >= hi {
            return None;
        }
        if data[cur] != delimiter && !(may_end && i + 1 == n && data[cur] == b'\n') {
            return None;
        }
        at = cur + 1;
    }
    Some(at)
}

impl RowParser {
    pub(super) fn csv(delimiter: u8, source: Vec<Option<usize>>) -> Self {
        let last_needed = source.iter().flatten().copied().max().unwrap_or(0);
        // Inverted once, so storing a field is a lookup rather than a walk of
        // every wanted column: twenty columns against twenty slots is four
        // hundred comparisons a row otherwise. A column can feed more than one
        // slot -- `--compare` may name a key column -- so each entry is a list,
        // and it is one element deep in every case but that one.
        let mut slots_for: Vec<Vec<u16>> = vec![Vec::new(); last_needed + 1];
        for (slot, at) in source.iter().enumerate() {
            if let Some(column) = at {
                slots_for[*column].push(slot as u16);
            }
        }
        // The one-slot shortcut, from the same inversion. NONE is a column no
        // slot wants; MANY is `--compare` naming a key column, the only way a
        // column feeds two, and worth a branch rather than a second array.
        let single = slots_for
            .iter()
            .map(|s| match s.len() {
                0 => NONE,
                1 => i32::from(s[0]),
                _ => MANY,
            })
            .collect();
        RowParser::Csv {
            delimiter,
            last_needed,
            slots_for,
            single,
        }
    }

    pub(super) fn json(wanted: Vec<Option<String>>, key_size: usize, array: bool) -> Self {
        let mut n = 16usize;
        while n < wanted.len() * 4 {
            n <<= 1;
        }
        let mut slots = vec![-1i32; n];
        let slot_mask = n - 1;
        for (i, name) in wanted.iter().enumerate() {
            let Some(name) = name else { continue };
            let mut at = (name_hash(name.as_bytes()) as usize) & slot_mask;
            while slots[at] >= 0 {
                at = (at + 1) & slot_mask;
            }
            slots[at] = i as i32;
        }
        let mut parser = RowParser::Json {
            key_size: key_size.min(wanted.len()),
            wanted,
            slots,
            slot_mask,
            canon: Vec::new(),
            array,
        };
        let canon: Vec<i32> = match &parser {
            RowParser::Json { wanted, .. } => wanted
                .iter()
                .map(|w| match w {
                    Some(w) => parser.slot_for(w.as_bytes()).map_or(-1, |s| s as i32),
                    None => -1,
                })
                .collect(),
            RowParser::Csv { .. } => unreachable!(),
        };
        if let RowParser::Json { canon: c, .. } = &mut parser {
            *c = canon;
        }
        parser
    }

    /// Parses one row into `out`, returning the offset of the next row.
    ///
    /// A row shorter than the header leaves the missing fields [`ABSENT`], which
    /// compares as absent — a difference to report, not a file to refuse.
    pub(super) fn parse(&self, data: &[u8], start: usize, end: usize, out: &mut [Field]) -> usize {
        match Wide::detect() {
            // Safety: `w` proves the CPU has AVX2.
            Some(w) => unsafe { self.parse_wide(w, data, start, end, out) },
            None => self.parse_with(Swar, data, start, end, out),
        }
    }

    /// [`RowParser::parse_with`], compiled for AVX2 in a baseline build, so
    /// every scan of the row inlines into it as a vector compare.
    ///
    /// # Safety
    /// Holding a `Wide` is the proof the CPU has AVX2; nothing else is required.
    #[cfg_attr(
        all(
            target_arch = "x86_64",
            not(any(target_feature = "avx2", target_feature = "avx512bw"))
        ),
        target_feature(enable = "avx2")
    )]
    unsafe fn parse_wide(
        &self,
        w: Wide,
        data: &[u8],
        start: usize,
        end: usize,
        out: &mut [Field],
    ) -> usize {
        self.parse_with(w, data, start, end, out)
    }

    #[inline(always)]
    fn parse_with<S: Scan>(
        &self,
        scan: S,
        data: &[u8],
        start: usize,
        end: usize,
        out: &mut [Field],
    ) -> usize {
        match self {
            RowParser::Csv {
                delimiter,
                last_needed,
                slots_for,
                single,
            } => self.parse_csv(
                scan,
                *delimiter,
                slots_for,
                single,
                *last_needed,
                data,
                start,
                end,
                out,
            ),
            RowParser::Json { .. } => self.parse_json(scan, data, start, end, out),
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn parse_csv<S: Scan>(
        &self,
        scan: S,
        delimiter: u8,
        slots_for: &[Vec<u16>],
        single: &[i32],
        last_needed: usize,
        data: &[u8],
        start: usize,
        end: usize,
        out: &mut [Field],
    ) -> usize {
        // `out` is reused across rows, so a column this row does not reach has
        // to be blanked or it would show the previous row's value -- but that is
        // only the columns after the row ran out, and a well-formed row runs out
        // of nothing. Clearing all of them up front ran twenty stores per row
        // per file to no purpose.
        let blank_from = |out: &mut [Field], from: usize| {
            if from <= last_needed {
                for slots in &slots_for[from..=last_needed] {
                    for slot in slots {
                        out[*slot as usize] = ABSENT;
                    }
                }
            }
        };
        let mut pos = start;
        let mut column = 0usize;
        // One cursor for the row -- every byte scanned once, both delimiters
        // broadcast once -- but only where a scan step spans several fields. On
        // an eight-byte step it does not, and the plain scan is quicker; see
        // `Delims`.
        let mut delims = Delims::new(scan, data, start, end, delimiter, b'\n');

        while pos <= end {
            let (field, next) = if pos < end && data[pos] == b'"' {
                let close = skip_quoted_with(scan, data, pos + 1, end);
                let body_end = close.saturating_sub(1).max(pos + 1);
                (
                    quoted_field(scan, data, pos + 1, body_end),
                    delims.next(close),
                )
            } else {
                let next = if S::WIDE {
                    delims.next(pos)
                } else {
                    next_of2_with(scan, data, pos, end, delimiter, b'\n')
                };
                (plain_field(data, pos, next), next)
            };
            if column <= last_needed {
                let one = single[column];
                if one >= 0 {
                    out[one as usize] = field;
                } else if one == MANY {
                    for slot in &slots_for[column] {
                        out[*slot as usize] = field;
                    }
                }
            }
            column += 1;

            if next >= end {
                blank_from(out, column);
                return end;
            }
            if data[next] == b'\n' {
                blank_from(out, column);
                return next + 1;
            }
            pos = next + 1;
            // Every needed column is filled, so there is nothing to blank: the
            // rest of the row is skipped without being parsed.
            if column > last_needed {
                let eol = end_of_row(scan, data, pos, end);
                return if eol >= end { end } else { eol + 1 };
            }
        }
        blank_from(out, column);
        end
    }

    /// Walks one JSON object, storing the values of the keys we want.
    #[inline(always)]
    fn parse_json<S: Scan>(
        &self,
        scan: S,
        data: &[u8],
        start: usize,
        end: usize,
        out: &mut [Field],
    ) -> usize {
        let RowParser::Json {
            wanted,
            key_size,
            canon,
            array,
            ..
        } = self
        else {
            unreachable!("parse_json on a CSV parser");
        };
        out.fill(ABSENT);
        let mut found = 0; // key slots filled, for the early exit below
        let mut guess = 0; // the slot the next member most likely fills
        let mut open = false; // the early exit left the object unfinished
        let mut pos = start;
        while pos < end && json_space(data[pos]) {
            pos += 1;
        }
        // The opening bracket of a top-level array is container syntax, not a
        // row: step past it and the first object starts the rows.
        if *array && pos < end && data[pos] == b'[' {
            pos += 1;
            while pos < end && json_space(data[pos]) {
                pos += 1;
            }
        }
        if pos >= end {
            return end;
        }
        if data[pos] != b'{' {
            return end_of_json_row(scan, data, pos, end, *array, false); // not an object: skip the line
        }
        pos += 1;

        loop {
            // Compact ndjson separates members with a bare `,"`, which goes
            // straight to the next key; the general path takes the comma, loops
            // back and skips space twice to get there.
            if pos + 1 < end && data[pos] == b',' && data[pos + 1] == b'"' {
                pos += 1;
            } else {
                while pos < end && json_space(data[pos]) {
                    pos += 1;
                }
                if pos >= end {
                    break;
                }
                match data[pos] {
                    b'}' => {
                        pos += 1;
                        break;
                    }
                    b',' => {
                        pos += 1;
                        continue;
                    }
                    b'"' => {}
                    _ => break, // malformed: stop reading this object
                }
            }
            let key_from = pos + 1;
            let key_end = skip_json_string(scan, data, pos, end).0;
            // A row cut off right after a key's opening quote has no key.
            if key_end > end || key_end <= key_from {
                break;
            }
            let key = &data[key_from..key_end - 1];
            pos = key_end;
            // And `":"` between a key and a string value, likewise.
            if pos + 1 < end && data[pos] == b':' && data[pos + 1] == b'"' {
                pos += 1;
            } else {
                while pos < end && json_space(data[pos]) {
                    pos += 1;
                }
                if pos >= end || data[pos] != b':' {
                    break;
                }
                pos += 1;
                while pos < end && json_space(data[pos]) {
                    pos += 1;
                }
                if pos >= end {
                    break;
                }
            }

            let field = if data[pos] == b'"' {
                let from = pos + 1;
                let (close, escaped) = skip_json_string(scan, data, pos, end);
                let to = close.saturating_sub(1).max(from);
                pos = close;
                Some(pack(from as u64, (to - from) as u64, escaped))
            } else if data[pos] == b'{' || data[pos] == b'[' {
                // Not a cell value. Left absent rather than guessed at.
                pos = skip_json_nested(scan, data, pos, end);
                None
            } else {
                // A number, true, false or null: it runs to the next comma,
                // brace or space.
                let from = pos;
                while pos < end && data[pos] != b',' && data[pos] != b'}' && !json_space(data[pos])
                {
                    pos += 1;
                }
                if &data[from..pos] == b"null" {
                    None
                } else {
                    Some(pack(from as u64, (pos - from) as u64, false))
                }
            };
            if let Some(field) = field
                && let Some(slot) = match wanted.get(guess) {
                    Some(Some(w)) if w.as_bytes() == key => usize::try_from(canon[guess]).ok(),
                    _ => self.slot_for(key),
                }
            {
                guess = slot + 1;
                // First occurrence wins for a key column, and only for a key
                // column -- the rule the C port states. A JSON object is not
                // supposed to repeat a name, but the key-only parse stops as
                // soon as it has the keys, and it has to agree with the full
                // parse on what a row's key is: last-wins would let it stop on a
                // different value than the full parse ends with, a lookup that
                // misses its own row. Compared columns keep last-wins.
                if slot < *key_size {
                    if out[slot] == ABSENT {
                        out[slot] = field;
                        found += 1;
                        // Every key found and nothing else wanted: the rest of
                        // the object is bytes to skip, not fields to parse.
                        if found == wanted.len() {
                            open = true;
                            break;
                        }
                    }
                } else {
                    out[slot] = field;
                }
            }
        }
        end_of_json_row(scan, data, pos, end, *array, open)
    }

    pub(super) fn slot_for(&self, key: &[u8]) -> Option<usize> {
        let RowParser::Json {
            wanted,
            slots,
            slot_mask,
            ..
        } = self
        else {
            return None;
        };
        let mut at = (name_hash(key) as usize) & slot_mask;
        loop {
            let i = slots[at];
            if i < 0 {
                return None;
            }
            if wanted[i as usize].as_deref().map(str::as_bytes) == Some(key) {
                return Some(i as usize);
            }
            at = (at + 1) & slot_mask;
        }
    }
}

/// Does the mate's tail name a column this run tracks?
///
/// The CSV proof rests on a column sitting at a fixed offset. JSON makes no such
/// promise: a value is found by name, and a name repeated in one object takes its
/// *last* value for a compared column — which the C and C++ ports do too, and
/// `test.sh` cross-checks — so a second `"amount"` past the diverging byte would
/// carry a value the prefix never saw.
///
/// It cannot be ruled out in general, but it can be ruled out *here*, because the
/// proof only reaches this point when the two rows agree all the way through the
/// last compared value. Whatever is left is the trailing ignored columns: a few
/// bytes. A name is a quoted string, so every quote in them is a candidate.
/// Mistaking a closing quote for an opening one costs a lookup that fails, which
/// is a fallback and not a wrong answer — and an escaped name counts as a hit for
/// the same reason, since the wanted names are held unescaped.
#[inline(always)]
fn json_tail_is_clean_with<S: Scan>(
    scan: S,
    p: &RowParser,
    d: &[u8],
    mut at: usize,
    end: usize,
) -> bool {
    while at < end {
        let q = next_of1_with(scan, d, at, end, b'"');
        if q >= end {
            return true;
        }
        let (close, escaped) = skip_json_string(scan, d, q, end);
        if escaped {
            return false;
        }
        let from = q + 1;
        let to = if close > q + 1 { close - 1 } else { q + 1 };
        if to > from && p.slot_for(&d[from..to]).is_some() {
            return false;
        }
        if close <= q {
            return false;
        }
        at = close;
    }
    true
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
/// same bytes; the tail check covers what follows. The C and C++ ports have the
/// same proof (#234, #235).
pub(super) fn json_rows_match(p: &RowParser, a: &[u8], b: &[u8], b_lo: usize, b_hi: usize) -> bool {
    match Wide::detect() {
        // Safety: `w` proves the CPU has AVX2.
        Some(w) => unsafe { json_rows_match_wide(w, p, a, b, b_lo, b_hi) },
        None => json_rows_match_with(Swar, p, a, b, b_lo, b_hi),
    }
}

/// [`json_rows_match_with`], compiled for AVX2 in a baseline build.
///
/// # Safety
/// Holding a `Wide` is the proof the CPU has AVX2; nothing else is required.
#[cfg_attr(
    all(
        target_arch = "x86_64",
        not(any(target_feature = "avx2", target_feature = "avx512bw"))
    ),
    target_feature(enable = "avx2")
)]
unsafe fn json_rows_match_wide(
    w: Wide,
    p: &RowParser,
    a: &[u8],
    b: &[u8],
    b_lo: usize,
    b_hi: usize,
) -> bool {
    json_rows_match_with(w, p, a, b, b_lo, b_hi)
}

#[inline(always)]
fn json_rows_match_with<S: Scan>(
    scan: S,
    p: &RowParser,
    a: &[u8],
    b: &[u8],
    b_lo: usize,
    b_hi: usize,
) -> bool {
    let (mut ai, mut bi) = (0usize, b_lo);
    let mut after_value = false;
    for _ in 0..64 {
        let left = a.len() - ai;
        let m = left.min(b_hi - bi);
        // The whole run first: equal slices are one memcmp, and they are the
        // common case -- every row whose ignored values do not differ.
        if m == left && a[ai..] == b[bi..bi + left] {
            return json_tail_is_clean_with(scan, p, b, bi + left, b_hi);
        }
        let same = common_prefix(&a[ai..ai + m], &b[bi..bi + m]);
        if same == left {
            return json_tail_is_clean_with(scan, p, b, bi + left, b_hi);
        }
        if same == m {
            return false; // B ends first
        }
        let Some((vs, ve)) = json_member_at(scan, p, a, ai, ai + same, after_value) else {
            return false;
        };
        let b_vs = bi + (vs - ai);
        let b_ve = json_value_end(scan, b, b_vs, b_hi);
        if b_ve >= b_hi || b_ve <= b_vs {
            return false;
        }
        ai = ve;
        bi = b_ve;
        after_value = true;
    }
    false
}

/// How many leading bytes `x` and `y` share, eight at a time.
fn common_prefix(x: &[u8], y: &[u8]) -> usize {
    let n = x.len().min(y.len());
    let mut i = 0;
    while i + 8 <= n {
        let a = u64::from_le_bytes(x[i..i + 8].try_into().unwrap_or([0; 8]));
        let b = u64::from_le_bytes(y[i..i + 8].try_into().unwrap_or([0; 8]));
        if a != b {
            return i + ((a ^ b).trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && x[i] == y[i] {
        i += 1;
    }
    i
}

fn json_gap(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r'
}

/// Past one JSON value from `at`: a string, a scalar, or a nested object or
/// array. `end` where it does not close before `end`.
#[inline(always)]
fn json_value_end<S: Scan>(scan: S, d: &[u8], mut at: usize, end: usize) -> usize {
    if at >= end {
        return end;
    }
    match d[at] {
        b'"' => skip_json_string(scan, d, at, end).0,
        b'{' | b'[' => skip_json_nested(scan, d, at, end),
        _ => {
            while at < end && !matches!(d[at], b',' | b'}' | b']' | b'\n') && !json_gap(d[at]) {
                at += 1;
            }
            at
        }
    }
}

/// The member of `d`'s object whose value holds byte `diff`, walking from `at`
/// (the row start, or just past a value already skipped): its value's bounds.
/// `None` where `diff` falls in a name or the punctuation between members, in a
/// tracked member, or anywhere the walk cannot follow -- an escaped name
/// included, as the tail check treats one. `d` ends at the last wanted value, so
/// a value that runs to its end is not a gap.
#[inline(always)]
fn json_member_at<S: Scan>(
    scan: S,
    p: &RowParser,
    d: &[u8],
    mut at: usize,
    diff: usize,
    mut after_value: bool,
) -> Option<(usize, usize)> {
    let end = d.len();
    loop {
        while at < end && json_gap(d[at]) {
            at += 1;
        }
        if at >= end || at > diff || d[at] != if after_value { b',' } else { b'{' } {
            return None;
        }
        at += 1;
        while at < end && json_gap(d[at]) {
            at += 1;
        }
        if at >= end || d[at] != b'"' || at >= diff {
            return None;
        }
        let kq = at;
        let (kend, escaped) = skip_json_string(scan, d, at, end);
        if escaped || kend >= end || kend >= diff {
            return None;
        }
        at = kend;
        while at < end && json_gap(d[at]) {
            at += 1;
        }
        if at >= end || d[at] != b':' {
            return None;
        }
        at += 1;
        while at < end && json_gap(d[at]) {
            at += 1;
        }
        if at > diff {
            return None;
        }
        let v_end = json_value_end(scan, d, at, end);
        if v_end >= end || v_end <= at {
            return None;
        }
        if diff < v_end {
            if p.slot_for(&d[kq + 1..kend - 1]).is_some() {
                return None;
            }
            return Some((at, v_end));
        }
        at = v_end;
        after_value = true;
    }
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
/// `array` selects the walk: ndjson rows end at the newline, one memchr
/// exactly as ever, and everything below is the top-level array's syntax.
///
/// With `open`, the row's object was left unfinished -- the key-only parse
/// stops as soon as it has the keys -- and its remaining bytes run to the
/// closing brace first. Braces alone keep the depth: a `}` closes an object
/// wherever it sits, and a string hides its own braces behind `skip_json_string`.
#[inline(always)]
fn end_of_json_row<S: Scan>(
    scan: S,
    data: &[u8],
    pos: usize,
    end: usize,
    array: bool,
    open: bool,
) -> usize {
    if !array {
        let stop = next_of1_with(scan, data, pos, end, b'\n');
        return if stop >= end { end } else { stop + 1 };
    }
    end_of_array_row(scan, data, pos, end, open)
}

/// The array half of [`end_of_json_row`], kept out of its body: the ndjson
/// half is two instructions and inlines into every row's parse, and carrying
/// the container walk there cost the sweep real time.
fn end_of_array_row<S: Scan>(scan: S, data: &[u8], pos: usize, end: usize, open: bool) -> usize {
    let mut at = pos;
    if open {
        let mut depth = 1i32;
        while at < end {
            let b = next_of3_with(scan, data, at, end, b'"', b'{', b'}');
            if b >= end {
                return end;
            }
            match data[b] {
                b'"' => at = skip_json_string(scan, data, b, end).0,
                b'{' => {
                    depth += 1;
                    at = b + 1;
                }
                _ => {
                    depth -= 1;
                    at = b + 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        if depth > 0 {
            return end;
        }
    }
    // Between rows a top-level array puts its own bytes: the `,` separator,
    // which the next object follows on the same line or a later one, and the
    // closing `]`, after which there are no more rows. Anything else ends at
    // the next newline, exactly as a bare ndjson row always has.
    let mut cur = at;
    while cur < end && json_space(data[cur]) {
        cur += 1;
    }
    if cur < end && data[cur] == b',' {
        cur += 1;
        while cur < end && json_space(data[cur]) {
            cur += 1;
        }
        if cur >= end || data[cur] == b']' {
            return end;
        }
        if data[cur] == b'{' {
            return cur;
        }
    } else if cur < end && data[cur] == b']' {
        return end;
    }
    let stop = next_of1_with(scan, data, at, end, b'\n');
    if stop >= end { end } else { stop + 1 }
}

#[cfg(test)]
mod tests {
    use super::super::slab::Slab;
    use super::*;

    fn json_slab(text: &str) -> Slab {
        let mut s = Slab::owned(text.as_bytes().to_vec(), Dialect::Json);
        s.set_dialect(Dialect::Json);
        s
    }

    #[test]
    fn a_brace_is_json_and_anything_else_is_csv() {
        assert_eq!(sniff_dialect(b"  {\"a\":1}"), Dialect::Json);
        assert_eq!(sniff_dialect(b"a,b,c\n"), Dialect::Csv);
        assert_eq!(sniff_dialect(b""), Dialect::Csv);
    }

    #[test]
    fn a_bracket_is_json_too() {
        assert_eq!(sniff_dialect(b"[{\"a\":1}]"), Dialect::Json);
        assert_eq!(sniff_dialect(b"\xEF\xBB\xBF [ {\"a\":1} ]"), Dialect::Json);
        assert_eq!(sniff_dialect(b"[]"), Dialect::Json);
    }

    #[test]
    fn an_array_s_header_is_its_first_object_s_keys() {
        let slab = json_slab("[{\"b\":1,\"a\":\"x\"}]\n");
        let names = json_header(&slab, Path::new("x")).expect("a header");
        assert_eq!(names, ["b", "a"]);
        // An empty array has no object to take the names from.
        let empty = json_slab("[]\n");
        assert!(json_header(&empty, Path::new("x")).is_err());
    }

    /// Every row's key, walked as the sweep walks: one parse call per row, a
    /// missing key marked -- a bracket read as a row would show up here.
    fn array_keys(text: &str) -> Vec<String> {
        let slab = json_slab(text);
        let parser = RowParser::json(vec![Some("k".into())], 1, true);
        let mut out = vec![ABSENT; 1];
        let mut rows = Vec::new();
        let mut at = 0;
        while at < text.len() {
            let next = parser.parse(slab.data(), at, text.len(), &mut out);
            assert!(next > at, "a row parse always advances");
            at = next;
            rows.push(if out[0] == ABSENT {
                "<none>".to_string()
            } else {
                String::from_utf8(slab.raw(out[0]).to_vec()).expect("a key")
            });
        }
        rows
    }

    #[test]
    fn an_array_of_objects_one_per_line_reads_like_ndjson() {
        let ndjson = "{\"k\":\"a\",\"v\":1}\n{\"k\":\"b\",\"v\":2}\n";
        let array = "[\n{\"k\":\"a\",\"v\":1},\n{\"k\":\"b\",\"v\":2}\n]\n";
        assert_eq!(array_keys(ndjson), ["a", "b"]);
        assert_eq!(array_keys(array), array_keys(ndjson));
    }

    #[test]
    fn a_compact_array_splits_on_the_object_separator() {
        let array = "[{\"k\":\"a\"},{\"k\":\"b\"}, {\"k\":\"c\"}]";
        assert_eq!(array_keys(array), ["a", "b", "c"]);
    }

    #[test]
    fn a_pretty_printed_array_reads_one_object_across_lines() {
        let pretty =
            "[\n  {\n    \"k\": \"a\",\n    \"v\": 1\n  },\n  {\n    \"k\": \"b\"\n  }\n]\n";
        assert_eq!(array_keys(pretty), ["a", "b"]);
    }

    #[test]
    fn an_array_with_nested_values_and_escaped_braces_reads_alike() {
        // Nested values are not cells, exactly as in ndjson, and braces inside
        // a string are not the object's end.
        let ndjson =
            "{\"k\":\"a } {\",\"v\":{\"n\":[1,{\"x\":\"}\\\"\"}]}}\n{\"k\":\"b\",\"v\":2}\n";
        let array =
            "[{\"k\":\"a } {\",\"v\":{\"n\":[1,{\"x\":\"}\\\"\"}]}},\n{\"k\":\"b\",\"v\":2}]";
        assert_eq!(array_keys(ndjson), ["a } {", "b"]);
        assert_eq!(array_keys(array), array_keys(ndjson));
    }

    #[test]
    fn the_key_only_and_full_parses_agree_across_an_array() {
        // The early exit leaves the object unfinished; both parses must still
        // find the same rows and the same keys.
        let array = "[\n  {\n    \"k\": \"a\",\n    \"v\": \"w x\",\n    \"n\": {\"deep\": [1, 2]}\n  },\n  {\n    \"k\": \"b\"\n  }\n]\n";
        let slab = json_slab(array);
        let full = RowParser::json(vec![Some("k".into()), Some("v".into())], 1, true);
        let keys = RowParser::json(vec![Some("k".into())], 1, true);
        let mut out = vec![ABSENT; 2];
        let next = full.parse(slab.data(), 0, array.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"a");
        assert_eq!(slab.raw(out[1]), b"w x");
        let mut key = vec![ABSENT; 1];
        assert_eq!(keys.parse(slab.data(), 0, array.len(), &mut key), next);
        assert_eq!(slab.raw(key[0]), b"a");
        let last = keys.parse(slab.data(), next, array.len(), &mut key);
        assert_eq!(slab.raw(key[0]), b"b");
        assert_eq!(last, array.len());
    }

    #[test]
    fn the_header_is_the_first_object_s_keys_in_order() {
        let slab = json_slab("{\"b\":1,\"a\":\"x\",\"n\":null}\n");
        let names = json_header(&slab, Path::new("x")).expect("a header");
        assert_eq!(names, ["b", "a", "n"]);
    }

    #[test]
    fn values_are_read_by_key_whatever_order_they_come_in() {
        let text = "{\"k\":\"1\",\"v\":\"a\"}\n{\"v\":\"b\",\"k\":\"2\"}\n";
        let slab = json_slab(text);
        let parser = RowParser::json(vec![Some("k".into()), Some("v".into())], 0, false);
        let mut out = vec![ABSENT; 2];
        let next = parser.parse(slab.data(), 0, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"1");
        assert_eq!(slab.raw(out[1]), b"a");
        parser.parse(slab.data(), next, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"2");
        assert_eq!(slab.raw(out[1]), b"b");
    }

    #[test]
    fn null_a_nested_value_and_a_missing_key_are_all_absent() {
        let text = "{\"k\":\"1\",\"v\":null,\"w\":{\"deep\":1},\"x\":[1,2]}\n";
        let slab = json_slab(text);
        let parser = RowParser::json(
            vec![
                Some("k".into()),
                Some("v".into()),
                Some("w".into()),
                Some("x".into()),
                Some("missing".into()),
            ],
            0,
            false,
        );
        let mut out = vec![0; 5];
        let next = parser.parse(slab.data(), 0, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"1");
        assert_eq!(out[1..], [ABSENT; 4]);
        assert_eq!(next, text.len());
    }

    #[test]
    fn a_newline_inside_a_string_does_not_end_the_row() {
        let text = "{\"k\":\"a\\nb\",\"v\":\"1\"}\n{\"k\":\"c\",\"v\":\"2\"}\n";
        let slab = json_slab(text);
        let parser = RowParser::json(vec![Some("k".into()), Some("v".into())], 0, false);
        let mut out = vec![ABSENT; 2];
        let next = parser.parse(slab.data(), 0, text.len(), &mut out);
        assert_eq!(slab.logical(out[0]).collect::<Vec<u8>>(), b"a\nb");
        parser.parse(slab.data(), next, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"c");
    }

    #[test]
    fn a_repeated_key_the_key_only_and_full_parses_agree() {
        // The first `k` is the row's key in both parses -- the key-only one
        // stops there -- while a repeated compared column keeps its last value.
        let text = "{\"k\":\"1\",\"v\":\"a\",\"k\":\"2\",\"v\":\"b\"}\n{\"k\":\"3\"}\n";
        let slab = json_slab(text);
        let full = RowParser::json(vec![Some("k".into()), Some("v".into())], 1, false);
        let keys = RowParser::json(vec![Some("k".into())], 1, false);
        let mut out = vec![ABSENT; 2];
        let next = full.parse(slab.data(), 0, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"1");
        assert_eq!(slab.raw(out[1]), b"b");
        let mut key = vec![ABSENT; 1];
        assert_eq!(keys.parse(slab.data(), 0, text.len(), &mut key), next);
        assert_eq!(slab.raw(key[0]), b"1");
        keys.parse(slab.data(), next, text.len(), &mut key);
        assert_eq!(slab.raw(key[0]), b"3");
    }

    /// A row parses to the same fields at either scan width. Only runs where
    /// the CPU has the vector width.
    #[test]
    fn both_widths_parse_alike() {
        let Some(w) = Wide::detect() else { return };
        let long = "x".repeat(70);
        let mut csv = format!(
            "id,name,note,amount\n1,alpha,\"a, \"\"quoted\"\" note\",10.5\n\
             2,{long},plain,7\n3,,\"\",\n4,\"{long},\n{long}\",z,1\n5,a,b\n"
        );
        // Field lengths that walk a delimiter through every byte of a vector
        // step, with a quoted field now and then.
        for i in 0..200usize {
            let field = |n: usize| "y".repeat(n % 41);
            let note = if i % 5 == 0 {
                format!("\"{},\"\"{}\"", field(i * 3), field(i))
            } else {
                field(i * 3)
            };
            csv += &format!("{i},{},{note},{}\n", field(i * 7), field(i * 13));
        }
        let json = format!(
            "{{\"id\":\"1\",\"name\":\"a\\\"b\",\"skip\":{{\"n\":[1,{{\"m\":\"}}\"}}]}},\"amount\":2}}\n\
             {{\"amount\":null,\"name\":\"{long}\",\"id\":\"2\",\"id\":\"3\"}}\n\
             {{\"id\":\"4\",\"note\":\"{long}\\n{long}\"}}\n"
        );
        let parsers = [
            (
                RowParser::csv(b',', vec![Some(0), Some(1), Some(2), Some(3)]),
                csv.as_bytes(),
            ),
            (
                RowParser::csv(b',', vec![Some(0), None, Some(2), Some(0)]),
                csv.as_bytes(),
            ),
            (
                RowParser::json(
                    vec![
                        Some("id".into()),
                        Some("name".into()),
                        Some("amount".into()),
                        Some("note".into()),
                    ],
                    1,
                    false,
                ),
                json.as_bytes(),
            ),
        ];
        for (parser, data) in &parsers {
            let n = 4;
            let mut at = 0;
            while at < data.len() {
                let mut a = vec![ABSENT; n];
                let mut b = vec![ABSENT; n];
                let next = parser.parse_with(Swar, data, at, data.len(), &mut a);
                assert_eq!(parser.parse_with(w, data, at, data.len(), &mut b), next);
                assert_eq!(a, b, "the row at {at}");
                assert!(next > at);
                at = next;
            }
        }
    }

    #[test]
    fn a_name_in_two_slots_is_found_where_the_table_would_find_it() {
        // `k` fills slot 0 and slot 2, as when a key column is also named as a
        // compared one. After `v` fills slot 1 the next member is guessed as
        // slot 2, and a right guess has to answer what the table does: slot 0.
        let text = "{\"k\":\"1\",\"v\":\"a\",\"k\":\"2\"}\n";
        let slab = json_slab(text);
        let parser = RowParser::json(
            vec![Some("k".into()), Some("v".into()), Some("k".into())],
            1,
            false,
        );
        let mut out = vec![ABSENT; 3];
        parser.parse(slab.data(), 0, text.len(), &mut out);
        assert_eq!(slab.raw(out[0]), b"1");
        assert_eq!(slab.raw(out[1]), b"a");
        assert_eq!(out[2], ABSENT);
    }
}
