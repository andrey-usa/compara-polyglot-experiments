//! The bytes a field points into, and the one route every comparison reads them
//! through.
//!
//! A field is an offset and a length, so something has to say what those address
//! and how the bytes are escaped. For CSV and JSON that is the mapped file; for
//! Parquet it is an arena of decoded values, which is the same thing to everyone
//! downstream. The escape rule rides along because it follows the format: CSV
//! doubles a quote, JSON puts a backslash in front of one, and a decoded Parquet
//! value is already literal.
//!
//! Hashing, equality and decoding all read a field through [`Slab::logical`], so
//! they cannot come to different conclusions about the same value — the property
//! whose absence produced two silently wrong answers in the Java port.

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use super::field::{Field, is_escaped, is_real, len_of, offset_of};
use crate::error::{Error, Result};

/// Which escape rule the bytes behind a field follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    /// A doubled quote is one quote.
    Csv,
    /// A backslash escape, `\uXXXX` included.
    Json,
    /// The bytes are the value: nothing to undo. Parquet decodes to this.
    Raw,
}

/// Where the bytes came from. A mapping is paged in on demand and evictable; an
/// arena is what a columnar format leaves behind once its pages are decoded.
enum Bytes {
    Mapped { _file: File, map: Mmap },
    Owned(Vec<u8>),
}

pub(super) struct Slab {
    bytes: Bytes,
    dialect: Dialect,
    /// Where the data starts: past a UTF-8 byte order mark, which is not part
    /// of the first header name (or of the first JSON object).
    start: usize,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

impl Slab {
    /// Maps a file read-only, with a hint that it will be read front to back.
    pub(super) fn map(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .map_err(|e| Error::new(format!("cannot read {}: {e}", path.display())))?;
        // Safety: the file is opened read-only and not modified while mapped.
        // A concurrent truncation would be a torn read, which is the documented
        // hazard of every mapping engine here and of DuckDB's reader too.
        let map = unsafe { Mmap::map(&file) }
            .map_err(|e| Error::new(format!("cannot map {}: {e}", path.display())))?;
        // Unix only: memmap2 gates `Advice` and `advise` behind `#[cfg(unix)]`,
        // because it is `madvise` underneath and Windows has no equivalent worth
        // wrapping. The hint is an optimisation the kernel may ignore anyway, so
        // its absence costs nothing but readahead on a platform that does its
        // own. Without the gate this port does not compile on Windows at all.
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let start = if map.starts_with(BOM) { BOM.len() } else { 0 };
        Ok(Slab {
            bytes: Bytes::Mapped { _file: file, map },
            dialect: Dialect::Csv,
            start,
        })
    }

    /// Asks for `len` bytes at `from` to be read in, and returns without
    /// waiting for them. A hint only: bytes already in memory, and bytes not
    /// mapped from a file, make it a no-op.
    pub(super) fn will_need(&self, from: usize, len: usize) {
        #[cfg(unix)]
        if let Bytes::Mapped { map, .. } = &self.bytes {
            let from = (from + self.start).min(map.len());
            let len = len.min(map.len() - from);
            if len > 0 {
                let _ = map.advise_range(memmap2::Advice::WillNeed, from, len);
            }
        }
        #[cfg(not(unix))]
        let _ = (from, len);
    }

    /// A slab over bytes this process built: the Parquet reader's arena.
    pub(super) fn owned(data: Vec<u8>, dialect: Dialect) -> Self {
        Slab {
            bytes: Bytes::Owned(data),
            dialect,
            start: 0,
        }
    }

    pub(super) fn set_dialect(&mut self, dialect: Dialect) {
        self.dialect = dialect;
    }

    pub(super) fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub(super) fn data(&self) -> &[u8] {
        match &self.bytes {
            Bytes::Mapped { map, .. } => &map[self.start..],
            Bytes::Owned(v) => v,
        }
    }

    /// The field's raw span, still holding whatever escapes it was written with.
    pub(super) fn raw(&self, f: Field) -> &[u8] {
        if !is_real(f) {
            return &[];
        }
        let (at, len) = (offset_of(f), len_of(f));
        &self.data()[at..at + len]
    }

    /// The field's logical bytes: the value, with its escapes undone.
    pub(super) fn logical(&self, f: Field) -> LogicalBytes<'_> {
        let escaped = is_real(f) && is_escaped(f);
        LogicalBytes {
            raw: self.raw(f),
            dialect: if escaped { self.dialect } else { Dialect::Raw },
            at: 0,
            pending: [0; 4],
            pending_len: 0,
            pending_at: 0,
        }
    }
}

/// A field's bytes with its escapes undone, one byte at a time and without
/// allocating. A `\uXXXX` escape decodes to as many as four bytes, which is what
/// the small pending buffer is for.
pub(super) struct LogicalBytes<'a> {
    raw: &'a [u8],
    dialect: Dialect,
    at: usize,
    pending: [u8; 4],
    pending_len: u8,
    pending_at: u8,
}

impl LogicalBytes<'_> {
    /// True when the bytes are the value, so a caller may take the slice whole.
    pub(super) fn is_plain(&self) -> bool {
        self.dialect == Dialect::Raw
    }

    /// Encodes one code point, returning its first byte and holding the rest.
    ///
    /// The encoding is done by hand rather than through `char`, which refuses an
    /// unpaired surrogate: a file carrying one is malformed, but the C, C++ and
    /// Zig ports encode it rather than substituting a replacement character, and
    /// two ports that disagree about the bytes of a value would disagree about
    /// its hash.
    fn hold(&mut self, cp: u32) -> u8 {
        let mut buf = [0u8; 4];
        let n = match cp {
            0..=0x7F => {
                buf[0] = cp as u8;
                1
            }
            0x80..=0x7FF => {
                buf[0] = 0xC0 | (cp >> 6) as u8;
                buf[1] = 0x80 | (cp & 0x3F) as u8;
                2
            }
            0x800..=0xFFFF => {
                buf[0] = 0xE0 | (cp >> 12) as u8;
                buf[1] = 0x80 | ((cp >> 6) & 0x3F) as u8;
                buf[2] = 0x80 | (cp & 0x3F) as u8;
                3
            }
            _ => {
                buf[0] = 0xF0 | (cp >> 18) as u8;
                buf[1] = 0x80 | ((cp >> 12) & 0x3F) as u8;
                buf[2] = 0x80 | ((cp >> 6) & 0x3F) as u8;
                buf[3] = 0x80 | (cp & 0x3F) as u8;
                4
            }
        };
        self.pending = buf;
        self.pending_len = n;
        self.pending_at = 1;
        buf[0]
    }
}

impl Iterator for LogicalBytes<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.pending_at < self.pending_len {
            let b = self.pending[self.pending_at as usize];
            self.pending_at += 1;
            return Some(b);
        }
        let b = *self.raw.get(self.at)?;
        self.at += 1;
        match self.dialect {
            Dialect::Raw => Some(b),
            Dialect::Csv => {
                // A quote inside a quoted body can only be half of a doubled
                // pair, so the second one is dropped.
                if b == b'"' && self.raw.get(self.at) == Some(&b'"') {
                    self.at += 1;
                }
                Some(b)
            }
            Dialect::Json => {
                if b != b'\\' {
                    return Some(b);
                }
                let Some(&e) = self.raw.get(self.at) else {
                    return Some(b);
                };
                self.at += 1;
                Some(match e {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'u' => match self.take_escape() {
                        // A code point becomes UTF-8 rather than staying as the
                        // six bytes it was written as: a JSON writer may spell a
                        // character either way, and both spellings have to
                        // compare equal.
                        Some(cp) => self.hold(cp),
                        None => {
                            // Not four hex digits: it was never an escape, so
                            // both bytes are the value.
                            self.pending = [b'u', 0, 0, 0];
                            self.pending_len = 1;
                            self.pending_at = 0;
                            b'\\'
                        }
                    },
                    // Not an escape this dialect defines: both bytes are content.
                    other => {
                        self.pending = [other, 0, 0, 0];
                        self.pending_len = 1;
                        self.pending_at = 0;
                        b'\\'
                    }
                })
            }
        }
    }
}

impl LogicalBytes<'_> {
    /// The code point of the `\u` escape whose digits start at `self.at`, with a
    /// following low surrogate folded in. Leaves `at` past what it consumed.
    fn take_escape(&mut self) -> Option<u32> {
        let hi = self.take_hex4()?;
        if !(0xD800..=0xDBFF).contains(&hi) {
            return Some(hi);
        }
        // A surrogate pair is one code point written as two escapes.
        if self.raw.get(self.at) == Some(&b'\\') && self.raw.get(self.at + 1) == Some(&b'u') {
            let save = self.at;
            self.at += 2;
            match self.take_hex4() {
                Some(lo) if (0xDC00..=0xDFFF).contains(&lo) => {
                    return Some(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                }
                _ => self.at = save,
            }
        }
        Some(hi)
    }

    fn take_hex4(&mut self) -> Option<u32> {
        let digits = self.raw.get(self.at..self.at + 4)?;
        let mut cp = 0u32;
        for &d in digits {
            cp = cp * 16 + (d as char).to_digit(16)?;
        }
        self.at += 4;
        Some(cp)
    }
}

/// Whether two fields hold the same logical bytes, without decoding either.
pub(super) fn same_bytes(a: &Slab, x: Field, b: &Slab, y: Field) -> bool {
    // Ask the two field words directly rather than building two iterators to ask
    // them. `logical()` already reports a field with no escape as plain, so the
    // memcmp below was always the path a CSV file took -- but reaching it cost
    // two `LogicalBytes` values, each slicing the slab and carrying a four-byte
    // pending buffer, to answer a question two bit tests answer. For a
    // nine-byte field that setup was most of the comparison.
    if !is_escaped(x) && !is_escaped(y) {
        return a.raw(x) == b.raw(y);
    }
    a.logical(x).eq(b.logical(y))
}

/// The field decoded. Only the report sections and the normalising paths call
/// this; the fast path never builds a string for a cell at all.
pub(super) fn text_of(slab: &Slab, f: Field) -> String {
    let logical = slab.logical(f);
    if logical.is_plain() {
        return String::from_utf8_lossy(slab.raw(f)).into_owned();
    }
    let bytes: Vec<u8> = logical.collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// [`text_of`] that refuses rather than aborting when there is no room.
///
/// The report path uses this one. The comparison path keeps [`text_of`]: it
/// decodes a cell only when `--trim` or `--ignore-case` is on, throws the string
/// away immediately, and is the hottest loop in the port -- a `Result` there
/// would be paid ten million times to report a failure that cannot happen,
/// because nothing of that size is being held.
pub(super) fn text_of_checked(slab: &Slab, f: Field, what: &str) -> crate::Result<String> {
    let logical = slab.logical(f);
    if logical.is_plain() {
        return match std::str::from_utf8(slab.raw(f)) {
            Ok(s) => crate::alloc::string(s, what),
            // Lossy replacement allocates its own string; it is reached only by
            // input that is not UTF-8, which is not the case a budget is tight
            // for.
            Err(_) => Ok(String::from_utf8_lossy(slab.raw(f)).into_owned()),
        };
    }
    let mut bytes: Vec<u8> = crate::alloc::sized(f_len_hint(f), what)?;
    bytes.extend(logical);
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Ok(String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

/// An upper bound on a field's decoded length: decoding only ever removes
/// bytes (a doubled quote becomes one), so the raw length is enough.
fn f_len_hint(f: Field) -> usize {
    super::field::len_of(f)
}

#[cfg(test)]
mod tests {
    use super::super::field::pack;
    use super::*;

    fn bytes(text: &[u8], dialect: Dialect) -> Vec<u8> {
        let slab = Slab::owned(text.to_vec(), dialect);
        slab.logical(pack(0, text.len() as u64, true)).collect()
    }

    fn decode(text: &[u8], dialect: Dialect) -> String {
        String::from_utf8(bytes(text, dialect)).expect("valid utf-8")
    }

    #[test]
    fn csv_drops_the_second_quote_of_a_pair() {
        assert_eq!(decode(b"a\"\"b", Dialect::Csv), "a\"b");
    }

    #[test]
    fn json_undoes_its_escapes() {
        assert_eq!(decode(br#"a\nb\"c\\d"#, Dialect::Json), "a\nb\"c\\d");
    }

    #[test]
    fn a_unicode_escape_and_the_character_itself_are_the_same_value() {
        // A writer may spell a character either way, so both spellings have to
        // decode to the same bytes.
        assert_eq!(decode(br"caf\u00e9", Dialect::Json), "caf\u{e9}");
        assert_eq!(decode(br"\ud83d\ude00", Dialect::Json), "\u{1f600}");
        // Not four hex digits, so it was never an escape.
        assert_eq!(decode(br"\uZZ", Dialect::Json), r"\uZZ");
        // A lone high surrogate has no pair to fold in and must not eat what
        // follows it. It is encoded as written, which is what the other ports
        // do; substituting a replacement character here would make the same
        // file hash differently in two ports.
        assert_eq!(bytes(br"\ud83dx", Dialect::Json), b"\xed\xa0\xbdx");
    }
}
