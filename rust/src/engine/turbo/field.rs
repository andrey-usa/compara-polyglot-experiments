//! A field packed into one word, and the SWAR scanning that finds one.
//!
//! Both halves are shared by every reader here — CSV, newline-delimited JSON and
//! Parquet — because they all end up describing a value the same way: a run of
//! bytes somewhere, addressed by offset and length rather than copied into a
//! string. What differs between the formats is where those bytes live and how
//! they are escaped, which is [`super::slab`]'s business, not this module's.

/// A field packed into one word: offset, length, and whether it needs unescaping.
///
/// 40 bits of offset addresses a terabyte and 23 bits of length a field of eight
/// megabytes, which is more than a cell has any business being.
pub(super) type Field = u64;

/// A field that is not there at all: a short row, a missing JSON key, a null.
pub(super) const ABSENT: Field = u64::MAX;
/// A field too long for the packed length. Reported rather than truncated: the
/// length is masked into 23 bits, so silently packing an over-long field would
/// corrupt its value instead of failing.
pub(super) const TOO_LONG: Field = u64::MAX - 1;

const OFFSET_MASK: u64 = (1 << 40) - 1;
const LENGTH_SHIFT: u32 = 40;
const LENGTH_MASK: u64 = (1 << 23) - 1;
const ESCAPED: u64 = 1 << 63;

/// The longest field this packing can address.
pub(super) const MAX_FIELD_LEN: u64 = LENGTH_MASK;

pub(super) fn pack(offset: u64, len: u64, escaped: bool) -> Field {
    if len > MAX_FIELD_LEN {
        return TOO_LONG;
    }
    (offset & OFFSET_MASK)
        | ((len & LENGTH_MASK) << LENGTH_SHIFT)
        | if escaped { ESCAPED } else { 0 }
}

pub(super) fn offset_of(f: Field) -> usize {
    (f & OFFSET_MASK) as usize
}

pub(super) fn len_of(f: Field) -> usize {
    ((f >> LENGTH_SHIFT) & LENGTH_MASK) as usize
}

pub(super) fn is_escaped(f: Field) -> bool {
    f & ESCAPED != 0
}

/// Whether the word describes bytes at all, as opposed to one of the sentinels.
pub(super) fn is_real(f: Field) -> bool {
    f != ABSENT && f != TOO_LONG
}

// ---------------------------------------------------------------------------
// Scanning: SWAR, or a vector register where the CPU has one
// ---------------------------------------------------------------------------
//
// Every scan comes in two widths: eight bytes at a time with SWAR, which needs no
// CPU feature at all, and thirty-two (AVX2) or sixty-four (AVX-512BW) with a
// vector compare. Which one runs is a type, `Swar` or `Wide`, carried by the
// functions that scan, so each width is compiled as its own copy and the choice
// is not re-made inside a loop.
//
// A build for a vector unit -- `-C target-cpu=x86-64-v3`, `+avx2` or `+avx512bw`
// -- compiles the vector compare inline and always takes `Wide`. A baseline
// x86-64 build, which is what a release ships and a plain `cargo build` makes,
// asks the CPU once whether it has AVX2 (`Wide::detect`); each parsing entry
// point in `text` then runs a copy of itself compiled for AVX2 (`*_wide`, marked
// `#[target_feature]`), into which every scan below inlines, or its SWAR copy.
// The check is a cached load and a branch per row, where a check and a call per
// scan would land between every field and the next.

/// The width a scan steps in. Implemented by `Swar` and `Wide` only.
pub(super) trait Scan: Copy {
    const WIDE: bool;
    /// One bit per byte of the register at `at` that is `a` or `b`. Only asked
    /// of `Wide`, and only where `at + STEP <= data.len()`.
    fn hits(self, data: &[u8], at: usize, a: u8, b: u8) -> u64;
    /// One bit per byte where `x` at `xi` and `y` at `yi` hold the same byte.
    /// Only asked of `Wide`, and only where a whole register fits in each.
    fn same(self, x: &[u8], xi: usize, y: &[u8], yi: usize) -> u64;
}

/// Eight bytes at a time, on any CPU.
#[derive(Clone, Copy)]
pub(super) struct Swar;

impl Scan for Swar {
    const WIDE: bool = false;
    #[inline(always)]
    fn hits(self, _: &[u8], _: usize, _: u8, _: u8) -> u64 {
        0
    }
    #[inline(always)]
    fn same(self, _: &[u8], _: usize, _: &[u8], _: usize) -> u64 {
        0
    }
}

/// A vector register at a time. Holding one is the proof that the CPU has the
/// instructions: the field is private, and [`Wide::detect`] is the only way to
/// make one.
#[derive(Clone, Copy)]
pub(super) struct Wide(());

impl Wide {
    /// `Some` where this CPU can scan wide. Asked of the CPU once per process
    /// (`std` caches it); every later call is a load and a test.
    #[inline(always)]
    pub(super) fn detect() -> Option<Wide> {
        #[cfg(all(
            target_arch = "x86_64",
            any(target_feature = "avx2", target_feature = "avx512bw")
        ))]
        return Some(Wide(()));
        #[cfg(all(
            target_arch = "x86_64",
            not(any(target_feature = "avx2", target_feature = "avx512bw"))
        ))]
        // POPCNT and BMI1 ride along: every CPU with AVX2 has them, and the
        // wide functions count and find bits with them.
        return (std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("popcnt")
            && std::arch::is_x86_feature_detected!("bmi1"))
        .then_some(Wide(()));
        #[cfg(not(target_arch = "x86_64"))]
        return None;
    }
}

/// The bytes a `Wide` scan takes in one step.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
pub(super) const STEP: usize = 64;
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
pub(super) const STEP: usize = 32;

impl Scan for Wide {
    const WIDE: bool = true;
    #[inline(always)]
    fn hits(self, data: &[u8], at: usize, a: u8, b: u8) -> u64 {
        debug_assert!(at + STEP <= data.len());
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
        // Safety: the caller has checked `at + 64 <= data.len()`, and this build
        // targets a CPU with AVX-512BW.
        return unsafe {
            use std::arch::x86_64::*;
            let chunk = _mm512_loadu_si512(data.as_ptr().add(at) as *const _);
            _mm512_cmpeq_epi8_mask(chunk, _mm512_set1_epi8(a as i8))
                | _mm512_cmpeq_epi8_mask(chunk, _mm512_set1_epi8(b as i8))
        };
        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            not(target_feature = "avx512bw")
        ))]
        // Safety: as above, with AVX2 and thirty-two bytes.
        return unsafe { avx2_hits(data, at, a, b) };
        #[cfg(all(
            target_arch = "x86_64",
            not(any(target_feature = "avx2", target_feature = "avx512bw"))
        ))]
        // Safety: `self` exists, so the CPU has AVX2 (`Wide::detect`), and the
        // caller has checked the bounds. Inside a function compiled for AVX2
        // this inlines to the few instructions it is.
        return unsafe { avx2_hits(data, at, a, b) };
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (data, at, a, b);
            unreachable!("no Wide scan off x86-64")
        }
    }

    #[inline(always)]
    fn same(self, x: &[u8], xi: usize, y: &[u8], yi: usize) -> u64 {
        debug_assert!(xi + STEP <= x.len() && yi + STEP <= y.len());
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
        // Safety: the caller has checked both bounds, and this build targets a
        // CPU with AVX-512BW.
        return unsafe {
            use std::arch::x86_64::*;
            let a = _mm512_loadu_si512(x.as_ptr().add(xi) as *const _);
            let b = _mm512_loadu_si512(y.as_ptr().add(yi) as *const _);
            _mm512_cmpeq_epi8_mask(a, b)
        };
        #[cfg(all(target_arch = "x86_64", not(target_feature = "avx512bw")))]
        // Safety: `self` exists, so the CPU has AVX2, and the caller has
        // checked both bounds.
        return unsafe { avx2_same(x, xi, y, yi) };
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (x, xi, y, yi);
            unreachable!("no Wide scan off x86-64")
        }
    }
}

/// The AVX2 byte-for-byte compare of two registers' worth.
///
/// # Safety
/// The CPU has AVX2, `xi + 32 <= x.len()` and `yi + 32 <= y.len()`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
#[allow(dead_code)] // unused in an AVX-512 build
unsafe fn avx2_same(x: &[u8], xi: usize, y: &[u8], yi: usize) -> u64 {
    use std::arch::x86_64::*;
    // Safety: the bounds are the caller's promise.
    let (a, b) = unsafe {
        (
            _mm256_loadu_si256(x.as_ptr().add(xi) as *const _),
            _mm256_loadu_si256(y.as_ptr().add(yi) as *const _),
        )
    };
    _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, b)) as u32 as u64
}

/// The AVX2 compare.
///
/// # Safety
/// The CPU has AVX2, and `at + 32 <= data.len()`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,popcnt,bmi1")]
#[inline]
#[allow(dead_code)] // unused in an AVX-512 build
unsafe fn avx2_hits(data: &[u8], at: usize, a: u8, b: u8) -> u64 {
    use std::arch::x86_64::*;
    // Safety: the bounds are the caller's promise.
    let chunk = unsafe { _mm256_loadu_si256(data.as_ptr().add(at) as *const _) };
    let equal = _mm256_or_si256(
        _mm256_cmpeq_epi8(chunk, _mm256_set1_epi8(a as i8)),
        _mm256_cmpeq_epi8(chunk, _mm256_set1_epi8(b as i8)),
    );
    _mm256_movemask_epi8(equal) as u32 as u64
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

fn broadcast(b: u8) -> u64 {
    (b as u64) * ONES
}

/// Sets the high bit of every byte of `word` that equals `target`.
///
/// `diff - ONES` borrows across a byte only where that byte was zero, and
/// `!diff` cancels the false positives the borrow creates, so what survives
/// marks exactly the matching bytes.
#[inline(always)]
fn match_bits(word: u64, target: u64) -> u64 {
    let diff = word ^ target;
    (diff.wrapping_sub(ONES)) & !diff & HIGH
}

/// The high bit of every byte of `word` equal to `target`, and of no other.
///
/// [`match_bits`] is the classic zero-byte trick, and it is exact only about
/// the *lowest* matching byte: subtracting ones from the whole word lets a
/// borrow out of a matching byte disturb the byte above it, which shows up
/// exactly when that byte is `target ^ 1` -- after a comma, that is `-`, so
/// every negative number in a CSV triggers it. Everything that only ever takes
/// `trailing_zeros` of the result is therefore right; a cursor that wants every
/// match in the word is not.
///
/// Forcing each byte's high bit before the subtraction stops the borrow: no
/// byte is then below 0x80, so subtracting one never carries into its
/// neighbour. The result marks *non*-matching bytes, so it is complemented.
#[inline(always)]
fn match_bits_all(word: u64, target: u64) -> u64 {
    let diff = word ^ target;
    !(diff | ((diff | HIGH).wrapping_sub(ONES))) & HIGH
}

/// A left-to-right cursor over the delimiters of one row.
///
/// [`next_of2`] begins a fresh scan at every field, which costs twice over: a
/// word straddling a field boundary is loaded once for the field that ends in
/// it and again for the field that starts there, and the two broadcasts are
/// recomputed for each of a row's twenty fields. Fields are found strictly in
/// order, so the scan can keep what it has -- the chunk it loaded and the
/// matches in it that no field has claimed yet -- and read every byte once.
///
/// **Only worth it for a vector step, which is why the row parse uses it only
/// with `Wide`.** Fields here average 9.2 bytes, so an eight-byte step rarely
/// spans a boundary and there is almost nothing to reuse: measured interleaved
/// against the plain scan, SWAR came out 3.8% slower in this port and 0.7%
/// slower in Zig, the bookkeeping costing more than it saves. A thirty-two-byte
/// step covers three and a half fields, so the plain scan loads the same bytes
/// three or four times over, and the cursor is 15.0% faster here and 12.9% in
/// Zig.
///
/// Instruction count says the opposite of all that -- the exact mask above is
/// two more ALU operations per word, and callgrind reports the cursor as a
/// regression. What it removes is *loads*. This one had to be settled by the
/// clock.
///
/// `next` takes the position to resume from rather than assuming the previous
/// result, because a quoted field is walked by `skip_quoted` and the cursor has
/// to be told to seek past its body.
pub(super) struct Delims<'a, S: Scan> {
    scan: S,
    data: &'a [u8],
    a: u8,
    b: u8,
    ba: u64,
    bb: u64,
    /// Where the held chunk begins, and the matches in it not yet returned.
    held: usize,
    bits: u64,
    /// Whether `bits` came from a vector compare (one bit per byte) or from
    /// SWAR (the high bit of each byte).
    wide: bool,
    /// The first byte no chunk has covered yet.
    scanned: usize,
}

impl<'a, S: Scan> Delims<'a, S> {
    #[inline(always)]
    pub(super) fn new(scan: S, data: &'a [u8], from: usize, end: usize, a: u8, b: u8) -> Self {
        Delims {
            scan,
            data: &data[..end.min(data.len())],
            a,
            b,
            ba: broadcast(a),
            bb: broadcast(b),
            held: 0,
            bits: 0,
            wide: false,
            scanned: from,
        }
    }

    #[inline(always)]
    fn lowest(&self, bits: u64) -> usize {
        self.held
            + if self.wide {
                bits.trailing_zeros() as usize
            } else {
                (bits.trailing_zeros() >> 3) as usize
            }
    }

    /// The offset of the first byte at or after `from` that is `a` or `b`, or
    /// `end` -- the same answer [`next_of2`] gives, without rescanning.
    ///
    /// The held matches are the common answer -- a 32-byte chunk covers three
    /// or four fields -- so that part is inlined into the parse loop and only a
    /// new chunk is a call. As one function it was not inlined at all: 1.24B of
    /// the CSV run's 7.1B instructions were this function, most of them the
    /// call and the few lines below.
    #[inline(always)]
    pub(super) fn next(&mut self, from: usize) -> usize {
        while self.bits != 0 {
            let at = self.lowest(self.bits);
            self.bits &= self.bits - 1;
            if at >= from {
                return at;
            }
        }
        self.refill(from)
    }

    /// `next` once the held matches are spent: scans on from `from`, or from the
    /// first byte no chunk has covered, whichever is later.
    ///
    /// Inlined, so the cursor's fields can live in registers. As a call it took
    /// `&mut self`, which puts the whole struct on the stack: a `perf` profile
    /// of the 10M CSV run on an EPYC 7763 had the parse loop reloading `held`,
    /// `bits` and `wide` from the stack for every field. C++ measured the same
    /// cursor 10-14% faster once its state was held in locals.
    #[inline(always)]
    fn refill(&mut self, from: usize) -> usize {
        let mut at = from.max(self.scanned);
        if S::WIDE {
            while at + STEP <= self.data.len() {
                let mut hits = self.scan.hits(self.data, at, self.a, self.b);
                self.held = at;
                self.wide = true;
                self.scanned = at + STEP;
                while hits != 0 {
                    let idx = at + hits.trailing_zeros() as usize;
                    hits &= hits - 1;
                    if idx >= from {
                        self.bits = hits;
                        return idx;
                    }
                }
                at += STEP;
            }
        }
        while at + 8 <= self.data.len() {
            let word = word_at(self.data, at);
            let mut hits = match_bits_all(word, self.ba) | match_bits_all(word, self.bb);
            self.held = at;
            self.wide = false;
            self.scanned = at + 8;
            while hits != 0 {
                let idx = at + (hits.trailing_zeros() >> 3) as usize;
                hits &= hits - 1;
                if idx >= from {
                    self.bits = hits;
                    return idx;
                }
            }
            at += 8;
        }
        let mut t = from.max(at);
        while t < self.data.len() {
            if self.data[t] == self.a || self.data[t] == self.b {
                self.scanned = t + 1;
                return t;
            }
            t += 1;
        }
        self.scanned = self.data.len();
        self.data.len()
    }
}

#[inline(always)]
fn word_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("eight bytes"))
}

/// The scan a caller outside the dispatched parse gets: `Wide` in a build for a
/// vector unit, `Swar` otherwise. Only the setup paths use it -- the header,
/// opening a file -- where a row's worth of bytes is all that is scanned.
#[cfg(all(
    target_arch = "x86_64",
    any(target_feature = "avx2", target_feature = "avx512bw")
))]
#[inline(always)]
fn fixed() -> Wide {
    Wide(())
}
#[cfg(not(all(
    target_arch = "x86_64",
    any(target_feature = "avx2", target_feature = "avx512bw")
)))]
#[inline(always)]
fn fixed() -> Swar {
    Swar
}

/// [`next_of2_with`] at the build's fixed width.
pub(super) fn next_of2(data: &[u8], from: usize, end: usize, a: u8, b: u8) -> usize {
    next_of2_with(fixed(), data, from, end, a, b)
}

/// [`next_of1_with`] at the build's fixed width.
pub(super) fn next_of1(data: &[u8], from: usize, end: usize, target: u8) -> usize {
    next_of1_with(fixed(), data, from, end, target)
}

/// [`skip_quoted_with`] at the build's fixed width.
pub(super) fn skip_quoted(data: &[u8], from: usize, end: usize) -> usize {
    skip_quoted_with(fixed(), data, from, end)
}

/// The offset of the first byte at or after `from` that is `a` or `b`, or `end`.
///
/// The slice is cut to `end` first, which is not tidiness: it makes `at + 8 <=
/// data.len()` the loop condition, so the compiler can see that the eight-byte
/// read inside cannot be out of range and drops the bounds check it would
/// otherwise emit on every step of a scan over gigabytes.
#[inline(always)]
pub(super) fn next_of2_with<S: Scan>(
    scan: S,
    data: &[u8],
    from: usize,
    end: usize,
    a: u8,
    b: u8,
) -> usize {
    let data = &data[..end.min(data.len())];
    let mut at = from;
    if S::WIDE {
        while at + STEP <= data.len() {
            let hits = scan.hits(data, at, a, b);
            if hits != 0 {
                return at + hits.trailing_zeros() as usize;
            }
            at += STEP;
        }
    }
    let (ba, bb) = (broadcast(a), broadcast(b));
    while at + 8 <= data.len() {
        let word = word_at(data, at);
        let hits = match_bits(word, ba) | match_bits(word, bb);
        if hits != 0 {
            return at + (hits.trailing_zeros() >> 3) as usize;
        }
        at += 8;
    }
    while at < data.len() {
        if data[at] == a || data[at] == b {
            return at;
        }
        at += 1;
    }
    end
}

/// The offset of the first byte at or after `from` that is `a`, `b` or `c`,
/// or `end`.
#[inline(always)]
pub(super) fn next_of3_with<S: Scan>(
    scan: S,
    data: &[u8],
    from: usize,
    end: usize,
    a: u8,
    b: u8,
    c: u8,
) -> usize {
    let data = &data[..end.min(data.len())];
    let mut at = from;
    if S::WIDE {
        while at + STEP <= data.len() {
            let hits = scan.hits(data, at, a, b) | scan.hits(data, at, c, c);
            if hits != 0 {
                return at + hits.trailing_zeros() as usize;
            }
            at += STEP;
        }
    }
    let (ba, bb, bc) = (broadcast(a), broadcast(b), broadcast(c));
    while at + 8 <= data.len() {
        let word = word_at(data, at);
        let hits = match_bits(word, ba) | match_bits(word, bb) | match_bits(word, bc);
        if hits != 0 {
            return at + (hits.trailing_zeros() >> 3) as usize;
        }
        at += 8;
    }
    while at < data.len() {
        if data[at] == a || data[at] == b || data[at] == c {
            return at;
        }
        at += 1;
    }
    end
}

/// The offset of the first `target` at or after `from`, or `end`.
#[inline(always)]
pub(super) fn next_of1_with<S: Scan>(
    scan: S,
    data: &[u8],
    from: usize,
    end: usize,
    target: u8,
) -> usize {
    let data = &data[..end.min(data.len())];
    let mut at = from;
    if S::WIDE {
        while at + STEP <= data.len() {
            let hits = scan.hits(data, at, target, target);
            if hits != 0 {
                return at + hits.trailing_zeros() as usize;
            }
            at += STEP;
        }
    }
    let bt = broadcast(target);
    while at + 8 <= data.len() {
        let hits = match_bits(word_at(data, at), bt);
        if hits != 0 {
            return at + (hits.trailing_zeros() >> 3) as usize;
        }
        at += 8;
    }
    while at < data.len() {
        if data[at] == target {
            return at;
        }
        at += 1;
    }
    end
}

/// How many `target` bytes there are in `data[from..end]`.
///
/// Only the chunked index build needs this, and only for the quote character:
/// the number of quotes before a position is what says whether that position is
/// inside a quoted field. Counting whole words at a time keeps it far cheaper
/// than the parsing it makes parallel.
pub(super) fn count_byte(data: &[u8], from: usize, end: usize, target: u8) -> usize {
    let data = &data[..end.min(data.len())];
    let bt = broadcast(target);
    let mut n = 0usize;
    let mut at = from;
    while at + 8 <= data.len() {
        n += match_bits(word_at(data, at), bt).count_ones() as usize;
        at += 8;
    }
    while at < data.len() {
        if data[at] == target {
            n += 1;
        }
        at += 1;
    }
    n
}

/// Walks past a quoted field's body, returning the offset after its closing quote.
/// A doubled quote inside is content, not the end.
#[inline(always)]
pub(super) fn skip_quoted_with<S: Scan>(scan: S, data: &[u8], from: usize, end: usize) -> usize {
    let mut at = from;
    loop {
        let q = next_of1_with(scan, data, at, end, b'"');
        if q >= end {
            return end;
        }
        if q + 1 < end && data[q + 1] == b'"' {
            at = q + 2;
            continue;
        }
        return q + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_crosses_the_word_boundary() {
        let d = b"aaaaaaaaaaaa,x\nbb";
        assert_eq!(next_of2(d, 0, d.len(), b',', b'\n'), 12);
        assert_eq!(next_of2(d, 13, d.len(), b',', b'\n'), 14);
        assert_eq!(next_of2(d, 15, d.len(), b',', b'\n'), d.len());
        assert_eq!(next_of1(d, 0, d.len(), b'x'), 13);
    }

    #[test]
    fn counting_matches_a_byte_at_a_time() {
        let d = b"a\"b\"\"c\"d\"e\"\"\"f";
        for from in 0..d.len() {
            let want = d[from..].iter().filter(|&&b| b == b'"').count();
            assert_eq!(count_byte(d, from, d.len(), b'"'), want, "from {from}");
        }
    }

    #[test]
    fn a_doubled_quote_is_content() {
        let d = b"\"a\"\"b\",rest";
        assert_eq!(skip_quoted(d, 1, d.len()), 6);
    }

    /// The widths have to agree wherever a vector step could start, end or
    /// straddle a match. Only runs where the CPU has the vector width: elsewhere
    /// there is one width and nothing to compare.
    #[test]
    fn both_widths_find_the_same_bytes() {
        let Some(w) = Wide::detect() else { return };
        // Plain bytes with the searched-for ones mixed in, over several vector
        // steps so every alignment is crossed.
        let mut d = Vec::new();
        let mut x = 0x2545_f491_u32;
        for _ in 0..300 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            d.push(match x % 11 {
                0 => b',',
                1 => b'\n',
                2 => b'"',
                _ => b'a' + (x % 26) as u8,
            });
        }
        for end in [d.len(), d.len() - 1, 100, 64, 33, 7] {
            let mut wide = Delims::new(w, &d, 0, end, b',', b'\n');
            let mut swar = Delims::new(Swar, &d, 0, end, b',', b'\n');
            let mut at = 0;
            loop {
                let want = next_of2_with(Swar, &d, at, end, b',', b'\n');
                assert_eq!(next_of2_with(w, &d, at, end, b',', b'\n'), want);
                assert_eq!(wide.next(at), want, "wide cursor from {at} to {end}");
                assert_eq!(swar.next(at), want, "SWAR cursor from {at} to {end}");
                if want >= end {
                    break;
                }
                at = want + 1;
            }
            for from in 0..end {
                assert_eq!(
                    next_of1_with(w, &d, from, end, b'"'),
                    next_of1_with(Swar, &d, from, end, b'"'),
                );
                assert_eq!(
                    skip_quoted_with(w, &d, from, end),
                    skip_quoted_with(Swar, &d, from, end),
                );
            }
        }
    }

    #[test]
    fn an_over_long_field_is_a_sentinel_rather_than_a_truncation() {
        assert_eq!(pack(0, MAX_FIELD_LEN + 1, false), TOO_LONG);
        let f = pack(1 << 30, MAX_FIELD_LEN, true);
        assert_eq!(offset_of(f), 1 << 30);
        assert_eq!(len_of(f), MAX_FIELD_LEN as usize);
        assert!(is_escaped(f));
    }
}
