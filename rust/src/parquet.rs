//! A Parquet reader shaped for comparing, not for querying.
//!
//! This is the Rust twin of `cpp/src/parquet.cpp`, and it makes the same
//! bargain: it reads exactly what this job needs and refuses the rest by name.
//! `BYTE_ARRAY` columns and the word-sized types (BOOLEAN, INT32, INT64, FLOAT,
//! DOUBLE, a decimal in up to eight fixed bytes), PLAIN and dictionary
//! encodings, uncompressed, snappy, gzip, zstd and LZ4 pages, v1 data pages.
//! That covers what DuckDB, polars, pyarrow and this project's own generator
//! write for string and numeric columns by default, and it is the shape the
//! comparison actually meets.
//!
//! Two things it does that a general reader would not.
//!
//! It hands back *offsets into the mapped file* rather than strings. A PLAIN
//! byte array is a four-byte length followed by its bytes, already contiguous
//! and already in the mapping, so a value can stay an offset and a length --
//! the same representation the CSV and JSON paths use, and the reason nothing
//! here builds a string per cell.
//!
//! And it keeps dictionary columns encoded. Where a column is dictionary
//! encoded the rows are indices into a small table of distinct values, so two
//! files can be compared by mapping one dictionary onto the other once and then
//! comparing integers -- which is what makes the per-column diff a vector
//! operation instead of a string compare per row.

use crate::alloc;
use crate::engine::turbo::codec::{Codec, decompress};
use crate::error::{Error, Result};

fn fail<T>(what: &str, path: &str) -> Result<T> {
    Err(Error::new(format!("{what}: {path}")))
}

/// The same, for the corners of the format this reader deliberately does not
/// cover. Marked so [`crate::engine`] can hand the pair to `turbo`, which reads
/// more of Parquet than the columnar path does, instead of refusing a file this
/// binary can in fact read.
fn unsupported<T>(what: &str, path: &str) -> Result<T> {
    Err(Error::unsupported(format!("{what}: {path}")))
}

// parquet.thrift, the handful of values this reader meets.
const TYPE_BOOLEAN: i64 = 0;
const TYPE_INT32: i64 = 1;
const TYPE_INT64: i64 = 2;
const TYPE_FLOAT: i64 = 4;
const TYPE_DOUBLE: i64 = 5;
const TYPE_BYTE_ARRAY: i64 = 6;
const TYPE_FIXED_LEN_BYTE_ARRAY: i64 = 7;
const ENC_PLAIN: i64 = 0;
const ENC_PLAIN_DICTIONARY: i64 = 2;
const ENC_RLE: i64 = 3;
const ENC_RLE_DICTIONARY: i64 = 8;
const CODEC_UNCOMPRESSED: i64 = 0;
const PAGE_DATA: i64 = 0;
const PAGE_INDEX: i64 = 1;
const PAGE_DICTIONARY: i64 = 2;
const PAGE_DATA_V2: i64 = 3;

// Thrift compact protocol field types.
const T_STOP: u8 = 0;
const T_TRUE: u8 = 1;
const T_FALSE: u8 = 2;
const T_BYTE: u8 = 3;
const T_I16: u8 = 4;
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_DOUBLE: u8 = 7;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_SET: u8 = 10;
const T_MAP: u8 = 11;
const T_STRUCT: u8 = 12;

/// A value's bytes, as an offset and a length packed into one word: forty bits
/// of offset, twenty-three of length, and one to say the value is null.
///
/// That is the same layout the CSV engine packs a field into, for the same
/// reason -- eight bytes a value rather than sixteen is eighty megabytes less to
/// carry, and eighty megabytes less to stream past the CPU, on a ten-million-row
/// column. Forty bits of offset is a terabyte; twenty-three of length is eight
/// megabytes, and a longer value is refused by name rather than truncated.
///
/// The offset is into the mapped file where the page was stored uncompressed,
/// and into the reader's own buffer where it had to be decompressed --
/// [`Column::owned`] says which.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Slice(u64);

impl Slice {
    pub const MAX_LENGTH: u32 = (1 << 23) - 1;
    const OFFSET_MASK: u64 = (1 << 40) - 1;
    const LENGTH_SHIFT: u32 = 40;
    const NULL_BIT: u64 = 1 << 63;

    pub fn at(offset: u64, length: u32) -> Self {
        Slice((offset & Self::OFFSET_MASK) | ((length as u64) << Self::LENGTH_SHIFT))
    }
    pub fn null() -> Self {
        Slice(Self::NULL_BIT)
    }
    pub fn is_null(self) -> bool {
        self.0 & Self::NULL_BIT != 0
    }
    pub fn offset(self) -> usize {
        (self.0 & Self::OFFSET_MASK) as usize
    }
    pub fn length(self) -> usize {
        ((self.0 >> Self::LENGTH_SHIFT) & Self::MAX_LENGTH as u64) as usize
    }
}

impl Default for Slice {
    fn default() -> Self {
        Slice::null()
    }
}

/// One column of one file, decoded as far as it is useful to decode it.
///
/// A dictionary column keeps `dict` and `index`: `index[row]` selects a value,
/// or is [`Column::NULL`]. A plain column keeps `values`, one slice per row. The
/// comparison reads whichever is populated, and the dictionary form is the one
/// worth having.
#[derive(Default)]
pub struct Column {
    pub dictionary: bool,
    /// Dictionary form: the distinct values.
    pub dict: Vec<Slice>,
    /// Dictionary form: one per row.
    pub index: Vec<i32>,
    /// Plain form: one per row.
    pub values: Vec<Slice>,
    /// Decompressed pages, when the column was compressed. A column is either
    /// wholly compressed or wholly not -- the reader refuses anything else --
    /// so this one field says where every slice in the column points: into
    /// `owned` when it is non-empty, into the mapping when it is not.
    pub owned: Vec<u8>,
    /// Word form, for a column of word-sized values; the fields above are then
    /// empty.
    pub words: Option<Words>,
}

/// A column of word-sized values: one canonical word per row, so that two
/// cells are the same value exactly when their words are equal. A float has
/// two zeros and many NaNs, each of which reads as one text, and each is
/// stored as one word; a null row is zero, with its bit set in `nulls`.
#[derive(Default)]
pub struct Words {
    /// Little-endian: an INT32 sign-extended, an INT64 as it is, a float as its
    /// bits, a boolean as 0 or 1, a fixed-length decimal as its unscaled value
    /// sign-extended.
    pub values: Vec<[u8; 8]>,
    /// One bit per row that is null, or empty when no row is.
    pub nulls: Vec<u64>,
}

impl Words {
    #[inline]
    pub fn is_null(&self, row: usize) -> bool {
        !self.nulls.is_empty() && self.nulls[row >> 6] >> (row & 63) & 1 != 0
    }
}

impl Column {
    pub const NULL: i32 = -1;

    pub fn rows(&self) -> usize {
        if let Some(words) = &self.words {
            words.values.len()
        } else if self.dictionary {
            self.index.len()
        } else {
            self.values.len()
        }
    }
}

/// What a file says about itself, before any column is read.
#[derive(Default)]
pub struct Meta {
    /// Leaf columns, in file order.
    pub names: Vec<String>,
    pub rows: i64,
    pub row_groups: usize,
    /// About how many bytes [`read_column`] holds for each column while it is
    /// in memory: a word or a handle per row, and for a compressed byte-array
    /// column the expanded pages its handles point into.
    pub held: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Thrift compact protocol
// ---------------------------------------------------------------------------

/// How deep `skip` follows nested structs and lists. Parquet's metadata nests
/// a few levels; a footer nested thousands deep exists only to overflow the
/// stack of a reader that recurses.
const MAX_NESTING: u32 = 64;

struct Thrift<'a> {
    d: &'a [u8],
    at: usize,
    path: &'a str,
}

impl<'a> Thrift<'a> {
    fn new(d: &'a [u8], path: &'a str) -> Self {
        Thrift { d, at: 0, path }
    }

    fn byte(&mut self) -> Result<u8> {
        if self.at >= self.d.len() {
            return fail("parquet metadata ends early", self.path);
        }
        let b = self.d[self.at];
        self.at += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut out = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            out |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift > 63 {
                return fail("a varint in the metadata is malformed", self.path);
            }
        }
    }

    fn zigzag(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn binary(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()?;
        if len > (self.d.len() - self.at) as u64 {
            return fail("a string in the metadata runs past the end", self.path);
        }
        let out = &self.d[self.at..self.at + len as usize];
        self.at += len as usize;
        Ok(out)
    }

    /// A container's element count, refused when the bytes left could not
    /// hold that many elements (every element, even a bool in a list, takes
    /// at least one), so a forged count can neither size an allocation nor
    /// spin a loop.
    fn count(&mut self, n: u64) -> Result<usize> {
        if n > (self.d.len() - self.at) as u64 {
            return fail(
                "a list in the parquet metadata is longer than the metadata",
                self.path,
            );
        }
        Ok(n as usize)
    }

    /// Reads a field header. Returns the type, or `T_STOP` at the end of a
    /// struct; `id` is set to the field number.
    fn field(&mut self, id: &mut i16, last: &mut i16) -> Result<u8> {
        let h = self.byte()?;
        if h == 0 {
            return Ok(T_STOP);
        }
        let ty = h & 0x0F;
        let delta = (h & 0xF0) >> 4;
        *id = if delta == 0 {
            self.zigzag()? as i16
        } else {
            last.wrapping_add(delta as i16)
        };
        *last = *id;
        Ok(ty)
    }

    /// Reads a list header, returning the element type and the count.
    fn list(&mut self) -> Result<(u8, usize)> {
        let h = self.byte()?;
        let mut count = ((h & 0xF0) >> 4) as u64;
        if count == 15 {
            count = self.varint()?;
        }
        Ok((h & 0x0F, self.count(count)?))
    }

    /// Steps over a value without interpreting it, so a struct can be read for
    /// the few fields that matter and the rest skipped.
    fn skip(&mut self, ty: u8) -> Result<()> {
        self.skip_nested(ty, 0, false)
    }

    /// `skip` at a nesting `depth`; an `element` sits in a list, set or map,
    /// where a bool takes a byte of its own instead of living in the field
    /// header.
    fn skip_nested(&mut self, ty: u8, depth: u32, element: bool) -> Result<()> {
        if depth > MAX_NESTING {
            return fail(
                "the parquet metadata nests deeper than any writer produces",
                self.path,
            );
        }
        match ty {
            T_TRUE | T_FALSE if element => {
                self.byte()?;
            }
            T_TRUE | T_FALSE => {}
            T_BYTE => {
                self.byte()?;
            }
            T_I16 | T_I32 | T_I64 => {
                self.zigzag()?;
            }
            T_DOUBLE => {
                for _ in 0..8 {
                    self.byte()?;
                }
            }
            T_BINARY => {
                self.binary()?;
            }
            T_LIST | T_SET => {
                let (elem, n) = self.list()?;
                for _ in 0..n {
                    self.skip_nested(elem, depth + 1, true)?;
                }
            }
            T_MAP => {
                let n = self.varint()?;
                let n = self.count(n)?;
                if n > 0 {
                    let kinds = self.byte()?;
                    for _ in 0..n {
                        self.skip_nested(kinds >> 4, depth + 1, true)?;
                        self.skip_nested(kinds & 0x0F, depth + 1, true)?;
                    }
                }
            }
            T_STRUCT => {
                let (mut id, mut last) = (0i16, 0i16);
                loop {
                    let t = self.field(&mut id, &mut last)?;
                    if t == T_STOP {
                        break;
                    }
                    self.skip_nested(t, depth + 1, false)?;
                }
            }
            _ => return fail("unknown type in the parquet metadata", self.path),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct ChunkMeta {
    ty: i64,
    codec: i64,
    num_values: i64,
    data_page_offset: i64,
    dictionary_page_offset: i64,
    total_compressed_size: i64,
    total_uncompressed_size: i64,
}

#[derive(Default, Clone)]
struct RowGroupMeta {
    columns: Vec<ChunkMeta>,
}

#[derive(Default)]
pub(crate) struct FileMeta {
    names: Vec<String>,
    /// 1 where the column may be null.
    optional: Vec<bool>,
    /// Bytes per value of a FIXED_LEN_BYTE_ARRAY column.
    type_length: Vec<usize>,
    groups: Vec<RowGroupMeta>,
    rows: i64,
}

fn read_column_meta(t: &mut Thrift, out: &mut ChunkMeta) -> Result<()> {
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            return Ok(());
        }
        match id {
            1 => out.ty = t.zigzag()?,
            4 => out.codec = t.zigzag()?,
            5 => out.num_values = t.zigzag()?,
            6 => out.total_uncompressed_size = t.zigzag()?,
            7 => out.total_compressed_size = t.zigzag()?,
            9 => out.data_page_offset = t.zigzag()?,
            11 => out.dictionary_page_offset = t.zigzag()?,
            _ => t.skip(ty)?,
        }
    }
}

fn read_chunk(t: &mut Thrift, out: &mut ChunkMeta) -> Result<()> {
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            return Ok(());
        }
        if id == 3 {
            read_column_meta(t, out)?;
        } else {
            t.skip(ty)?;
        }
    }
}

fn read_row_group(t: &mut Thrift, out: &mut RowGroupMeta) -> Result<()> {
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            return Ok(());
        }
        if id == 1 {
            let (_, n) = t.list()?;
            out.columns = vec![ChunkMeta::default(); n];
            for c in out.columns.iter_mut() {
                read_chunk(t, c)?;
            }
        } else {
            t.skip(ty)?;
        }
    }
}

pub(crate) fn read_file_meta(data: &[u8], path: &str) -> Result<FileMeta> {
    if data.len() < 12 || &data[..4] != b"PAR1" || &data[data.len() - 4..] != b"PAR1" {
        return fail("not a parquet file", path);
    }
    let meta_len =
        u32::from_le_bytes(data[data.len() - 8..data.len() - 4].try_into().unwrap()) as usize;
    if meta_len + 8 > data.len() {
        return fail("the parquet footer is longer than the file", path);
    }
    let start = data.len() - 8 - meta_len;
    let mut t = Thrift::new(&data[start..start + meta_len], path);

    let mut out = FileMeta::default();
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            break;
        }
        match id {
            2 => {
                let (_, n) = t.list()?;
                for i in 0..n {
                    // SchemaElement: type_length is 2, repetition_type 3, name 4,
                    // num_children 5.
                    let (mut sid, mut slast) = (0i16, 0i16);
                    let mut name = String::new();
                    let (mut repetition, mut children, mut type_length) = (-1i64, 0i64, 0i64);
                    loop {
                        let st = t.field(&mut sid, &mut slast)?;
                        if st == T_STOP {
                            break;
                        }
                        match sid {
                            2 => type_length = t.zigzag()?,
                            3 => repetition = t.zigzag()?,
                            4 => name = String::from_utf8_lossy(t.binary()?).into_owned(),
                            5 => children = t.zigzag()?,
                            _ => t.skip(st)?,
                        }
                    }
                    // The first element is the root, and anything with children
                    // is a group rather than a column this reader can read.
                    if i == 0 {
                        continue;
                    }
                    if children > 0 {
                        return unsupported("nested parquet columns are not read here", path);
                    }
                    out.names.push(name);
                    out.optional.push(repetition == 1);
                    out.type_length
                        .push(type_length.clamp(0, i32::MAX as i64) as usize);
                }
            }
            3 => out.rows = t.zigzag()?,
            4 => {
                let (_, n) = t.list()?;
                out.groups = vec![RowGroupMeta::default(); n];
                for g in out.groups.iter_mut() {
                    read_row_group(&mut t, g)?;
                }
            }
            _ => t.skip(ty)?,
        }
    }
    if out.names.is_empty() {
        return fail("the parquet schema has no columns", path);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PageHead {
    ty: i64,
    uncompressed: i64,
    compressed: i64,
    num_values: i32,
    encoding: i64,
    def_encoding: i64,
    /// Offset of the page body.
    after: usize,
}

fn read_page_head(data: &[u8], at: usize, path: &str) -> Result<PageHead> {
    let mut t = Thrift::new(data, path);
    t.at = at;
    let mut out = PageHead::default();
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            break;
        }
        match id {
            1 => out.ty = t.zigzag()?,
            2 => out.uncompressed = t.zigzag()?,
            3 => out.compressed = t.zigzag()?,
            5 | 7 => {
                let (mut hid, mut hlast) = (0i16, 0i16);
                loop {
                    let ht = t.field(&mut hid, &mut hlast)?;
                    if ht == T_STOP {
                        break;
                    }
                    match hid {
                        1 => out.num_values = t.zigzag()? as i32,
                        2 => out.encoding = t.zigzag()?,
                        3 if id == 5 => out.def_encoding = t.zigzag()?,
                        _ => t.skip(ht)?,
                    }
                }
            }
            8 => return unsupported("parquet data page v2 is not read here", path),
            _ => t.skip(ty)?,
        }
    }
    out.after = t.at;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// A column chunk's codec. One this reader does not decode is refused as
/// unsupported, which hands the pair to `turbo`: its reader names the codec it
/// cannot decode either, or reads the file.
///
/// Snappy alone used to be read here, and every other codec went to `turbo`,
/// which reads row by row. Polars writes zstd unless told otherwise, so a 2M-row
/// pair of its files took 1.14 s at two threads, and 0.39 s once read here --
/// what the same pair written with snappy takes.
fn codec_of(id: i64, path: &str) -> Result<Codec> {
    match i32::try_from(id).ok().map(Codec::from_id) {
        Some(Ok(codec)) => Ok(codec),
        _ => unsupported("this parquet codec is not read here", path),
    }
}

/// Appends a compressed page to `out`, expanded. Snappy is decoded in place by
/// [`snappy_append`]; gzip, zstd and LZ4 go through the `turbo` reader's
/// decoders, which hold a page to the size its header gives.
fn expand_append(
    codec: Codec,
    stored: &[u8],
    expected: i64,
    out: &mut Vec<u8>,
    path: &str,
) -> Result<()> {
    if codec == Codec::Snappy {
        if !snappy_append(stored, out)? {
            return fail(
                "a snappy page in the parquet file will not decompress",
                path,
            );
        }
        return Ok(());
    }
    let Ok(expected) = usize::try_from(expected) else {
        return fail("a parquet page has a negative size", path);
    };
    let page =
        decompress(codec, stored, expected).map_err(|e| Error::new(format!("{e}: {path}")))?;
    alloc::room(out, page.len(), "the uncompressed column")?;
    out.extend_from_slice(&page);
    Ok(())
}

/// Snappy raw block format: a varint of the uncompressed length, then a stream
/// of literal and copy elements.
///
/// It appends to `out` and writes through indices rather than pushing a byte at
/// a time, and a back-reference is copied eight bytes at a time where the
/// distance allows it. Both matter more than they look: this is the one loop
/// that touches every byte of a compressed file.
///
/// `Ok(false)` is a malformed stream; `Err` is the one allocation it makes --
/// the room for the expanded page -- refusing. The two are worth keeping apart:
/// the caller reports them differently, and calling a budget a corrupt file
/// sends the reader after the wrong bug.
fn snappy_append(input: &[u8], out: &mut Vec<u8>) -> Result<bool> {
    let mut at = 0usize;
    let mut want = 0u64;
    let mut shift = 0;
    loop {
        if at >= input.len() {
            return Ok(false);
        }
        let b = input[at];
        at += 1;
        want |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Ok(false);
        }
    }

    let base = out.len();
    let want = want as usize;
    alloc::grow(out, want, "the expanded snappy page")?;
    // The page is appended rather than written into a zeroed buffer. Zeroing it
    // first touched every expanded byte twice, and on the 2M Parquet pair it was
    // 0.24B instructions of memset. The room is reserved above, so no append
    // below reallocates, and `dst` is always `out.len()`.
    let mut dst = base;
    let end = base + want;

    while at < input.len() {
        let tag = input[at];
        at += 1;
        if tag & 0x03 == 0 {
            // literal
            let mut len = (tag >> 2) as usize + 1;
            if len > 60 {
                let extra = len - 60;
                if at + extra > input.len() {
                    return Ok(false);
                }
                let mut v = 0usize;
                for i in 0..extra {
                    v |= (input[at + i] as usize) << (8 * i);
                }
                at += extra;
                len = v + 1;
            }
            if at + len > input.len() || len > end - dst {
                return Ok(false);
            }
            out.extend_from_slice(&input[at..at + len]);
            dst += len;
            at += len;
            continue;
        }
        let (len, offset) = match tag & 0x03 {
            1 => {
                if at >= input.len() {
                    return Ok(false);
                }
                let o = (((tag >> 5) as usize) << 8) | input[at] as usize;
                at += 1;
                (((tag >> 2) & 0x07) as usize + 4, o)
            }
            2 => {
                if at + 2 > input.len() {
                    return Ok(false);
                }
                let o = input[at] as usize | ((input[at + 1] as usize) << 8);
                at += 2;
                ((tag >> 2) as usize + 1, o)
            }
            _ => {
                if at + 4 > input.len() {
                    return Ok(false);
                }
                let mut o = 0usize;
                for i in 0..4 {
                    o |= (input[at + i] as usize) << (8 * i);
                }
                at += 4;
                ((tag >> 2) as usize + 1, o)
            }
        };
        if offset == 0 || offset > dst - base || len > end - dst {
            return Ok(false);
        }
        let from = dst - offset;
        if offset >= len {
            // The whole source is behind the destination.
            out.extend_from_within(from..from + len);
        } else {
            // The copy repeats a pattern of `offset` bytes that it is still
            // writing. Everything from `from` on is that pattern, starting in
            // phase, and each chunk below ends on a whole number of periods, so
            // the next chunk can take twice as much from the same start.
            let mut left = len;
            while left > 0 {
                let n = left.min(out.len() - from);
                out.extend_from_within(from..from + n);
                left -= n;
            }
        }
        dst += len;
    }
    Ok(dst == end)
}

/// At least `n` entries in `v`, growing it only when it is shorter. What is
/// already there is left as it is, for a caller that overwrites what it reads.
fn fit(v: &mut Vec<i32>, n: usize, what: &str) -> Result<()> {
    if v.len() < n {
        alloc::grow(v, n - v.len(), what)?;
        v.resize(n, 0);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RLE / bit-packed hybrid
// ---------------------------------------------------------------------------

struct RleReader<'a> {
    d: &'a [u8],
    width: u32,
    mask: u64,
    at: usize,
    left: usize,
    bit: usize,
    packed: bool,
    value: i32,
}

impl<'a> RleReader<'a> {
    fn new(d: &'a [u8], width: u32) -> Self {
        RleReader {
            d,
            width,
            mask: if width >= 64 { !0 } else { (1u64 << width) - 1 },
            at: 0,
            left: 0,
            bit: 0,
            packed: false,
            value: 0,
        }
    }

    fn header(&mut self) -> bool {
        let mut h = 0u64;
        let mut shift = 0;
        loop {
            if self.at >= self.d.len() {
                return false;
            }
            let b = self.d[self.at];
            self.at += 1;
            h |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return false;
            }
        }
        if h & 1 == 0 {
            // RLE run: a count and one value.
            self.packed = false;
            self.left = (h >> 1) as usize;
            let bytes = self.width.div_ceil(8) as usize;
            if self.at + bytes > self.d.len() {
                return false;
            }
            self.value = 0;
            for i in 0..bytes {
                self.value |= (self.d[self.at + i] as i32) << (8 * i);
            }
            self.at += bytes;
        } else {
            // Bit-packed run, in groups of eight. A group is exactly `width`
            // bytes, so the whole run's length is known and `at` can jump
            // straight to the next header while `bit` walks inside it.
            self.packed = true;
            let groups = (h >> 1) as usize;
            self.left = groups.saturating_mul(8);
            self.bit = self.at * 8;
            let run = groups.saturating_mul(self.width as usize);
            self.at = self.at.saturating_add(run).min(self.d.len());
        }
        self.left > 0
    }

    /// Fills `out`. Bulk rather than one at a time on purpose: an RLE run
    /// becomes a fill, and a bit-packed run becomes one 64-bit load, one shift
    /// and one mask per value.
    ///
    /// Values are packed end to end with no padding, so a value straddles bytes
    /// more often than not. Reading eight bytes around it and shifting picks
    /// any of them out without a loop -- the same "look at eight bytes at once"
    /// trick the CSV scanner uses to find a delimiter, here reading rather than
    /// searching. It is exact for every width Parquet allows: seven bits of
    /// misalignment plus thirty-two of value still fits in a word.
    fn fill(&mut self, out: &mut [i32]) -> bool {
        let want = out.len();
        let mut done = 0usize;
        while done < want {
            if self.left == 0 && !self.header() {
                return false;
            }
            let take = (want - done).min(self.left);
            if !self.packed {
                out[done..done + take].fill(self.value);
            } else if self.width == 0 {
                out[done..done + take].fill(0);
            } else {
                // Split once per run rather than tested once per value: the
                // values whose eight-byte window lies inside the buffer, which
                // is all but the last few of a page, and the tail that is read
                // a byte at a time. The position is a local, and the output is
                // walked rather than indexed.
                let (d, width, mask) = (self.d, self.width as usize, self.mask);
                let mut bit = self.bit;
                let dst = &mut out[done..done + take];
                let safe = if d.len() >= 8 {
                    let last = (d.len() - 8) * 8 + 7; // the last bit a whole window starts at
                    if bit > last {
                        0
                    } else {
                        ((last - bit) / width + 1).min(take)
                    }
                } else {
                    0
                };
                let (fast, slow) = dst.split_at_mut(safe);
                for o in fast {
                    let byte = bit >> 3;
                    // Safety: `safe` counted only values whose eight-byte
                    // window starts at or before the last one inside `d`.
                    let w = u64::from_le_bytes(
                        unsafe { d.get_unchecked(byte..byte + 8) }
                            .try_into()
                            .unwrap(),
                    );
                    *o = ((w >> (bit & 7)) & mask) as i32;
                    bit += width;
                }
                for o in slow {
                    let byte = bit >> 3;
                    let mut w = 0u64;
                    for k in 0..8 {
                        if byte + k < d.len() {
                            w |= (d[byte + k] as u64) << (8 * k);
                        }
                    }
                    *o = ((w >> (bit & 7)) & mask) as i32;
                    bit += width;
                }
                self.bit = bit;
            }
            self.left -= take;
            done += take;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// PLAIN byte arrays: a four-byte little-endian length, then that many bytes,
/// repeated. The slices point straight at `base`, so nothing is copied.
fn plain_slices(
    page: &[u8],
    base: u64,
    count: i32,
    out: &mut Vec<Slice>,
    path: &str,
) -> Result<()> {
    // `room` and not `grow`: `got` is cleared before each data page, but `dict`
    // accumulates across row groups, and an *exact* reserve on a vector that is
    // still growing reallocates and copies the whole thing every time. The copying
    // goes as the square of the row-group count, so it is nothing at the two
    // groups a 2M-row file has and about 10 GB of handles at the fifty a 50M-row
    // file has -- which is the size this whole change is about.
    //
    // Not a measured win: A/B'd at 2M with the generator patched down to 100k
    // rows a group, twenty groups, nine rounds, and the two builds could not be
    // told apart. Twenty groups is too few for a quadratic to show. It stays on
    // the scaling argument, which costs nothing to take.
    // Every value takes a four-byte length, so a count the page cannot hold is
    // refused before it sizes anything.
    if count < 0 || count as usize > page.len() / 4 {
        return fail("a parquet page holds fewer values than it says", path);
    }
    alloc::room(out, count as usize, "one handle per value in the page")?;
    let mut at = 0usize;
    for _ in 0..count {
        if at + 4 > page.len() {
            return fail("a parquet page ends inside a value length", path);
        }
        let len = u32::from_le_bytes(page[at..at + 4].try_into().unwrap());
        at += 4;
        if at + len as usize > page.len() {
            return fail("a parquet value runs past its page", path);
        }
        if len > Slice::MAX_LENGTH {
            return fail("a parquet value is larger than eight megabytes", path);
        }
        out.push(Slice::at(base + at as u64, len));
        at += len as usize;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The two entry points
// ---------------------------------------------------------------------------

/// Builds the column table from an already-parsed footer.
pub(crate) fn read_meta(fm: &FileMeta) -> Meta {
    let rows = fm.rows.max(0) as u64;
    let held = (0..fm.names.len())
        .map(|c| {
            let expanded: u64 = fm
                .groups
                .iter()
                .filter_map(|g| g.columns.get(c))
                .filter(|m| m.ty == TYPE_BYTE_ARRAY && m.codec != CODEC_UNCOMPRESSED)
                .map(|m| m.total_uncompressed_size.max(0) as u64)
                .sum();
            rows.saturating_mul(8).saturating_add(expanded)
        })
        .collect();
    Meta {
        names: fm.names.clone(),
        rows: fm.rows,
        row_groups: fm.groups.len(),
        held,
    }
}

/// Reads one column across every row group. `which` indexes into [`Meta::names`];
/// `fm` is the file's footer, parsed once by the caller.
pub(crate) fn read_column(data: &[u8], fm: &FileMeta, which: usize, path: &str) -> Result<Column> {
    if which >= fm.names.len() {
        return fail("no such column in the parquet file", path);
    }
    let optional = fm.optional[which];
    let ty = fm
        .groups
        .first()
        .and_then(|g| g.columns.get(which))
        .map_or(TYPE_BYTE_ARRAY, |c| c.ty);
    if ty != TYPE_BYTE_ARRAY {
        return read_words(data, &fm, which, ty, path);
    }

    // The column's parts are held as locals rather than in a `Column`, because
    // a page is read *out of* `owned` while its values are pushed *into* `dict`
    // or `values`, and those are only disjoint borrows if they are separate
    // bindings. They become a `Column` at the end.
    let mut owned: Vec<u8> = Vec::new();
    let mut dict: Vec<Slice> = Vec::new();
    let mut index: Vec<i32> = Vec::new();
    let mut values: Vec<Slice> = Vec::new();

    let mut codec: Option<Codec> = None;
    let mut uncompressed = 0i64;
    for rg in &fm.groups {
        if which >= rg.columns.len() {
            return fail("a row group is missing a column", path);
        }
        uncompressed += rg.columns[which].total_uncompressed_size;
    }

    // Reused across pages so the per-page work allocates nothing, and sized
    // once from the footer's row count so appending a page never has to move
    // eighty megabytes of what is already decoded.
    let mut defs: Vec<i32> = Vec::new();
    let mut idx: Vec<i32> = Vec::new();
    let mut got: Vec<Slice> = Vec::new();
    alloc::grow(&mut index, fm.rows as usize, "one dictionary index per row")?;

    // The column starts out held as dictionary indices and stays that way only
    // if every page cooperates. A writer that gives up on the dictionary
    // partway -- DuckDB does, once a column's distinct values outgrow its
    // budget, which at ten million rows is most of them -- forces the whole
    // column into the plain form. Expanding what has been read costs no bytes:
    // a dictionary entry is already a slice.
    let mut dictionary = true;

    for rg in &fm.groups {
        let c = &rg.columns[which];
        if c.ty != TYPE_BYTE_ARRAY {
            return unsupported("only BYTE_ARRAY parquet columns are read here", path);
        }
        let chunk_codec = codec_of(c.codec, path)?;
        // Every chunk has to agree about compression, because a slice is an
        // offset with no room to say what it is an offset *into*. Uncompressed
        // pages are read in place and their offsets are into the mapping;
        // compressed ones are expanded into `owned` and their offsets are into
        // that. One column, one base.
        if codec.is_none() {
            codec = Some(chunk_codec);
            if chunk_codec != Codec::None {
                alloc::grow(&mut owned, uncompressed as usize, "the uncompressed column")?;
            }
        } else if codec != Some(chunk_codec) {
            return fail(
                "a parquet column changes compression between row groups",
                path,
            );
        }

        let mut at = if c.dictionary_page_offset > 0 {
            c.dictionary_page_offset as usize
        } else {
            c.data_page_offset as usize
        };
        let stop = at.saturating_add(c.total_compressed_size.max(0) as usize);
        let mut seen = 0i64;
        let dict_base = dict.len();

        while at < stop && at < data.len() && seen < c.num_values {
            let h = read_page_head(data, at, path)?;
            // Both sizes are the file's word, so neither may wrap a bound: a
            // negative one cast to usize would.
            if h.compressed < 0 || h.compressed as u64 > (data.len() - h.after) as u64 {
                return fail("a parquet page runs past the file", path);
            }
            if h.num_values < 0 {
                return fail("a parquet page has a negative number of values", path);
            }
            let body_len = h.compressed as usize;

            // Where the page's bytes live, and what a slice into them counts
            // from. `in_owned` is the whole of the distinction.
            let (page_from, page_len, page_base, in_owned) = if chunk_codec != Codec::None {
                let was = owned.len();
                expand_append(
                    chunk_codec,
                    &data[h.after..h.after + body_len],
                    h.uncompressed,
                    &mut owned,
                    path,
                )?;
                (was, owned.len() - was, was as u64, true)
            } else {
                (h.after, body_len, h.after as u64, false)
            };
            let body: &[u8] = if in_owned {
                &owned[page_from..page_from + page_len]
            } else {
                &data[page_from..page_from + page_len]
            };

            if h.ty == PAGE_DICTIONARY {
                if h.encoding != ENC_PLAIN && h.encoding != ENC_PLAIN_DICTIONARY {
                    return unsupported("only PLAIN parquet dictionaries are read here", path);
                }
                plain_slices(body, page_base, h.num_values, &mut dict, path)?;
            } else if h.ty == PAGE_DATA {
                let n_vals = h.num_values as usize;
                let mut vat = 0usize;
                let mut real = n_vals;
                if optional {
                    // Definition levels: RLE, four-byte length prefix in v1.
                    if h.def_encoding != ENC_RLE {
                        return fail("parquet definition levels are not RLE", path);
                    }
                    if page_len < 4 {
                        return fail("a parquet page has no definition levels", path);
                    }
                    let dl = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
                    if 4 + dl > page_len {
                        return fail("a parquet page ends inside its levels", path);
                    }
                    // Grown when a page is longer than any before it, and not
                    // cleared: `fill` writes every entry it is given, so only
                    // the first `n_vals` are read, and zeroing them first was
                    // most of the 0.24B instructions of memset this function
                    // spent on the 2M pair. The same for `idx` below.
                    fit(&mut defs, n_vals, "definition levels for the page")?;
                    if !RleReader::new(&body[4..4 + dl], 1).fill(&mut defs[..n_vals]) {
                        return fail("a parquet page ran out of definition levels", path);
                    }
                    real = defs[..n_vals].iter().filter(|&&d| d != 0).count();
                    vat = 4 + dl;
                }
                if vat > page_len {
                    return fail("a parquet page ends inside its levels", path);
                }
                let here = |i: usize| !optional || defs[i] != 0;

                if h.encoding == ENC_PLAIN_DICTIONARY || h.encoding == ENC_RLE_DICTIONARY {
                    if dict.is_empty() {
                        return fail(
                            "a parquet data page wants a dictionary there is none of",
                            path,
                        );
                    }
                    if vat + 1 > page_len {
                        return fail("a parquet page has no bit width", path);
                    }
                    let width = body[vat] as u32;
                    if width > 32 {
                        return fail("a parquet dictionary index is wider than 32 bits", path);
                    }
                    fit(&mut idx, real, "dictionary indices for the page")?;
                    let idx = &mut idx[..real];
                    if !RleReader::new(&body[vat + 1..], width).fill(idx) {
                        return fail("a parquet page ran out of dictionary indices", path);
                    }
                    // The OR of the folded indices stays under the
                    // dictionary's length exactly when no index could reach
                    // it, which is every page of a valid file; only a page
                    // that fails the guard is walked again to find the value.
                    let mut or = 0usize;
                    for v in idx.iter_mut() {
                        let k = dict_base + *v as usize;
                        or |= k;
                        *v = k as i32;
                    }
                    if or >= dict.len() && idx.iter().any(|&v| v as usize >= dict.len()) {
                        return fail("a parquet dictionary index is out of range", path);
                    }
                    // A page with no nulls, still in dictionary form, is its
                    // indices as they are: one copy rather than a push, a
                    // capacity check and two branches per value. It is every
                    // page of a REQUIRED column and most of an OPTIONAL one.
                    if dictionary && real == n_vals {
                        index.extend_from_slice(idx);
                    } else {
                        let mut k = 0usize;
                        for i in 0..n_vals {
                            if !here(i) {
                                if dictionary {
                                    index.push(Column::NULL);
                                } else {
                                    values.push(Slice::null());
                                }
                                continue;
                            }
                            let v = idx[k];
                            k += 1;
                            if dictionary {
                                index.push(v);
                            } else {
                                values.push(dict[v as usize]);
                            }
                        }
                    }
                } else if h.encoding == ENC_PLAIN {
                    if dictionary {
                        // Fold what has been read into the plain form. This
                        // copies eight-byte handles, not values.
                        alloc::grow(&mut values, fm.rows as usize, "one handle per row")?;
                        for &k in &index {
                            values.push(if k >= 0 {
                                dict[k as usize]
                            } else {
                                Slice::null()
                            });
                        }
                        index = Vec::new();
                        dictionary = false;
                    }
                    got.clear();
                    plain_slices(
                        &body[vat..],
                        page_base + vat as u64,
                        real as i32,
                        &mut got,
                        path,
                    )?;
                    if real == n_vals {
                        values.extend_from_slice(&got);
                    } else {
                        let mut k = 0usize;
                        for i in 0..n_vals {
                            values.push(if here(i) {
                                let v = got[k];
                                k += 1;
                                v
                            } else {
                                Slice::null()
                            });
                        }
                    }
                } else {
                    return fail(
                        "only PLAIN and dictionary parquet encodings are read here",
                        path,
                    );
                }
                seen += h.num_values as i64;
            } else if h.ty == PAGE_DATA_V2 {
                return unsupported("parquet data page v2 is not read here", path);
            } else if h.ty != PAGE_INDEX {
                return fail("unknown parquet page type", path);
            }
            at = h.after + body_len;
        }
    }

    // A column that never met a page at all is plain and empty, not dictionary.
    if dictionary && index.is_empty() && dict.is_empty() {
        dictionary = false;
    }
    Ok(Column {
        dictionary,
        dict,
        index,
        values,
        owned,
        words: None,
    })
}

/// Reads a column of word-sized values across every row group, into
/// [`Column::words`]. The page walk is [`read_column`]'s; what differs is that
/// a value is decoded into its word as it is read, so a dictionary page is
/// decoded once and its entries copied per row.
fn read_words(data: &[u8], fm: &FileMeta, which: usize, ty: i64, path: &str) -> Result<Column> {
    let width = match ty {
        TYPE_BOOLEAN => 0,
        TYPE_INT32 | TYPE_FLOAT => 4,
        TYPE_INT64 | TYPE_DOUBLE => 8,
        TYPE_FIXED_LEN_BYTE_ARRAY if (1..=8).contains(&fm.type_length[which]) => {
            fm.type_length[which]
        }
        _ => {
            return unsupported(
                "only byte-array and word-sized parquet columns are read here",
                path,
            );
        }
    };
    let optional = fm.optional[which];
    let rows = fm.rows.max(0) as usize;
    let mut values: Vec<[u8; 8]> = alloc::sized(rows, "one value per row")?;
    let mut nulls: Vec<u64> = Vec::new();
    let mut dict: Vec<[u8; 8]> = Vec::new();
    let mut page: Vec<u8> = Vec::new();
    let mut defs: Vec<i32> = Vec::new();
    let mut idx: Vec<i32> = Vec::new();
    let mut got: Vec<[u8; 8]> = Vec::new();

    for rg in &fm.groups {
        let Some(c) = rg.columns.get(which) else {
            return fail("a row group is missing a column", path);
        };
        if c.ty != ty {
            return fail("a parquet column changes type between row groups", path);
        }
        let chunk_codec = codec_of(c.codec, path)?;
        dict.clear();
        let mut at = if c.dictionary_page_offset > 0 {
            c.dictionary_page_offset as usize
        } else {
            c.data_page_offset as usize
        };
        let stop = at.saturating_add(c.total_compressed_size.max(0) as usize);
        let mut seen = 0i64;

        while at < stop && at < data.len() && seen < c.num_values {
            let h = read_page_head(data, at, path)?;
            if h.compressed < 0 || h.compressed as u64 > (data.len() - h.after) as u64 {
                return fail("a parquet page runs past the file", path);
            }
            if h.num_values < 0 {
                return fail("a parquet page has a negative number of values", path);
            }
            let stored = &data[h.after..h.after + h.compressed as usize];
            let body: &[u8] = if chunk_codec != Codec::None {
                page.clear();
                expand_append(chunk_codec, stored, h.uncompressed, &mut page, path)?;
                &page
            } else {
                stored
            };
            let n_vals = h.num_values as usize;

            if h.ty == PAGE_DICTIONARY {
                if h.encoding != ENC_PLAIN && h.encoding != ENC_PLAIN_DICTIONARY {
                    return unsupported("only PLAIN parquet dictionaries are read here", path);
                }
                if !plain_words(body, n_vals, ty, width, &mut dict) {
                    return fail("a parquet page holds fewer values than it says", path);
                }
            } else if h.ty == PAGE_DATA {
                if n_vals > rows - values.len() {
                    return fail(
                        "a parquet column holds more values than the file has rows",
                        path,
                    );
                }
                let mut vat = 0usize;
                let mut real = n_vals;
                if optional {
                    if h.def_encoding != ENC_RLE {
                        return fail("parquet definition levels are not RLE", path);
                    }
                    if body.len() < 4 {
                        return fail("a parquet page has no definition levels", path);
                    }
                    let dl = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
                    if dl > body.len() - 4 {
                        return fail("a parquet page ends inside its levels", path);
                    }
                    fit(&mut defs, n_vals, "definition levels for the page")?;
                    if !RleReader::new(&body[4..4 + dl], 1).fill(&mut defs[..n_vals]) {
                        return fail("a parquet page ran out of definition levels", path);
                    }
                    real = defs[..n_vals].iter().filter(|&&d| d != 0).count();
                    vat = 4 + dl;
                }

                got.clear();
                if h.encoding == ENC_PLAIN {
                    if !plain_words(&body[vat..], real, ty, width, &mut got) {
                        return fail("a parquet page holds fewer values than it says", path);
                    }
                } else if h.encoding == ENC_PLAIN_DICTIONARY || h.encoding == ENC_RLE_DICTIONARY {
                    if dict.is_empty() && real > 0 {
                        return fail(
                            "a parquet data page wants a dictionary there is none of",
                            path,
                        );
                    }
                    let Some(&bits) = body.get(vat) else {
                        return fail("a parquet page has no bit width", path);
                    };
                    if bits > 32 {
                        return fail("a parquet dictionary index is wider than 32 bits", path);
                    }
                    fit(&mut idx, real, "dictionary indices for the page")?;
                    if !RleReader::new(&body[vat + 1..], bits as u32).fill(&mut idx[..real]) {
                        return fail("a parquet page ran out of dictionary indices", path);
                    }
                    alloc::room(&mut got, real, "the values of a parquet page")?;
                    for &k in &idx[..real] {
                        let Some(&word) = dict.get(k as u32 as usize) else {
                            return fail("a parquet dictionary index is out of range", path);
                        };
                        got.push(word);
                    }
                } else {
                    return unsupported(
                        "only PLAIN and dictionary parquet encodings are read here",
                        path,
                    );
                }

                if real == n_vals {
                    values.extend_from_slice(&got);
                } else {
                    if nulls.is_empty() {
                        nulls = alloc::filled(0u64, rows.div_ceil(64), "a null flag per row")?;
                    }
                    let mut k = 0usize;
                    for &d in &defs[..n_vals] {
                        let row = values.len();
                        if d != 0 {
                            values.push(got[k]);
                            k += 1;
                        } else {
                            nulls[row >> 6] |= 1u64 << (row & 63);
                            values.push([0; 8]);
                        }
                    }
                }
                seen += n_vals as i64;
            } else if h.ty == PAGE_DATA_V2 {
                return unsupported("parquet data page v2 is not read here", path);
            } else if h.ty != PAGE_INDEX {
                return fail("unknown parquet page type", path);
            }
            at = h.after + h.compressed as usize;
        }
    }
    Ok(Column {
        words: Some(Words { values, nulls }),
        ..Column::default()
    })
}

/// PLAIN values of a word-sized type, appended to `out` as their words.
/// `false` when the page is too short to hold `count` of them.
fn plain_words(page: &[u8], count: usize, ty: i64, width: usize, out: &mut Vec<[u8; 8]>) -> bool {
    if ty == TYPE_BOOLEAN {
        if count.div_ceil(8) > page.len() {
            return false;
        }
        out.extend((0..count).map(|i| [(page[i / 8] >> (i % 8)) & 1, 0, 0, 0, 0, 0, 0, 0]));
        return true;
    }
    let Some(len) = count.checked_mul(width).filter(|&n| n <= page.len()) else {
        return false;
    };
    out.extend(page[..len].chunks_exact(width).map(|v| word(ty, v)));
    true
}

/// One stored value as its word: see [`Words::values`].
#[inline]
fn word(ty: i64, v: &[u8]) -> [u8; 8] {
    let w = match ty {
        TYPE_INT32 => i32::from_le_bytes(v.try_into().unwrap()) as i64 as u64,
        TYPE_INT64 => u64::from_le_bytes(v.try_into().unwrap()),
        TYPE_FLOAT => {
            let f = f32::from_le_bytes(v.try_into().unwrap());
            let f = if f == 0.0 {
                0.0
            } else if f.is_nan() {
                f32::NAN
            } else {
                f
            };
            f.to_bits() as u64
        }
        TYPE_DOUBLE => {
            let f = f64::from_le_bytes(v.try_into().unwrap());
            let f = if f == 0.0 {
                0.0
            } else if f.is_nan() {
                f64::NAN
            } else {
                f
            };
            f.to_bits()
        }
        // A decimal: big-endian two's complement.
        _ => {
            let mut w: i64 = if v[0] & 0x80 != 0 { -1 } else { 0 };
            for &b in v {
                w = (w << 8) | b as i64;
            }
            w as u64
        }
    };
    w.to_le_bytes()
}

/// True when the file begins with Parquet's `PAR1` magic. Cheap: four bytes.
pub fn is_parquet(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    match std::fs::File::open(path) {
        Ok(mut f) => f.read_exact(&mut magic).is_ok() && &magic == b"PAR1",
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal of `lit`, then one two-byte-offset copy of `len` at `offset`.
    fn stream(lit: &[u8], offset: usize, len: usize) -> Vec<u8> {
        let mut s = vec![(lit.len() + len) as u8]; // fits one varint byte here
        s.push(((lit.len() - 1) << 2) as u8);
        s.extend_from_slice(lit);
        s.push((((len - 1) << 2) | 2) as u8);
        s.push(offset as u8);
        s.push((offset >> 8) as u8);
        s
    }

    fn naive(lit: &[u8], offset: usize, len: usize) -> Vec<u8> {
        let mut v = lit.to_vec();
        for _ in 0..len {
            v.push(v[v.len() - offset]);
        }
        v
    }

    #[test]
    fn snappy_copies_that_overlap_themselves() {
        let lit = b"abcdefghij";
        for offset in 1..=lit.len() {
            for len in 1..=60 {
                let mut out = b"prefix".to_vec();
                assert!(snappy_append(&stream(lit, offset, len), &mut out).unwrap());
                let mut want = b"prefix".to_vec();
                want.extend(naive(lit, offset, len));
                assert_eq!(out, want, "offset {offset} len {len}");
            }
        }
    }

    /// One bit-packed run of `vals` at `width`, as the hybrid encoding writes it.
    fn packed(vals: &[u32], width: u32) -> Vec<u8> {
        let groups = vals.len().div_ceil(8);
        let mut out = vec![((groups << 1) | 1) as u8]; // fits one varint byte here
        let mut bits = vec![0u8; groups * width as usize];
        for (i, &v) in vals.iter().enumerate() {
            for b in 0..width as usize {
                if v >> b & 1 == 1 {
                    let at = i * width as usize + b;
                    bits[at / 8] |= 1 << (at % 8);
                }
            }
        }
        out.extend(bits);
        out
    }

    #[test]
    fn rle_unpacks_every_width_to_the_last_value() {
        let mut seed = 0x9e37_79b9_u64;
        for width in 1..=32u32 {
            for n in [1usize, 7, 8, 9, 63, 64, 120] {
                let mask = if width == 32 {
                    u32::MAX
                } else {
                    (1 << width) - 1
                };
                let vals: Vec<u32> = (0..n)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        (seed >> 32) as u32 & mask
                    })
                    .collect();
                let bytes = packed(&vals, width);
                let mut got = vec![0i32; n];
                assert!(
                    RleReader::new(&bytes, width).fill(&mut got),
                    "width {width} n {n}"
                );
                let want: Vec<i32> = vals.iter().map(|&v| v as i32).collect();
                assert_eq!(got, want, "width {width} n {n}");
            }
        }
    }

    #[test]
    fn snappy_refuses_a_copy_from_before_the_page() {
        let mut out = b"prefix".to_vec();
        assert!(!snappy_append(&stream(b"ab", 3, 4), &mut out).unwrap());
    }
}
