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

use std::mem::MaybeUninit;

use crate::alloc;
use crate::engine::turbo::codec::{Codec, decompress};
use crate::error::{Error, Result};
use crate::parallel;

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
    /// The same value, its offset moved `by` bytes on; a null stays null.
    fn shifted(self, by: u64) -> Self {
        if self.is_null() || by == 0 {
            self
        } else {
            Slice::at(self.offset() as u64 + by, self.length() as u32)
        }
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
    /// What the footer says each column's pages expand to, every row group
    /// together: the bytes a read of it decodes, so a measure of its work.
    pub expanded: Vec<u64>,
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

/// What a remote Parquet object must hold for the columns `want` to be read:
/// their column chunks in every row group, as byte ranges of the file, with
/// the column names in file order. `footer` is the file's footer as it ends
/// the file -- its metadata, the metadata's length and the magic -- which is
/// all of the file this needs.
///
/// A chunk is read from its first page, the dictionary's when there is one,
/// for its stored size. The two readers here disagree only on a dictionary
/// offset past the first data page, which one of them then starts at and the
/// other ignores; the range covers both.
#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
pub(crate) fn chunk_ranges(
    footer: &[u8],
    want: &dyn Fn(&str) -> bool,
) -> Result<(Vec<String>, Vec<std::ops::Range<u64>>)> {
    let mut whole = Vec::with_capacity(footer.len() + 4);
    whole.extend_from_slice(b"PAR1");
    whole.extend_from_slice(footer);
    let fm = read_file_meta(&whole, "the remote parquet footer")?;
    let mut out = Vec::new();
    for g in &fm.groups {
        for (i, name) in fm.names.iter().enumerate() {
            let Some(c) = g.columns.get(i) else {
                continue;
            };
            if !want(name) {
                continue;
            }
            let data = c.data_page_offset.max(0) as u64;
            let dict = (c.dictionary_page_offset > 0).then_some(c.dictionary_page_offset as u64);
            let first = dict.map_or(data, |d| d.min(data));
            let last = dict.map_or(data, |d| d.max(data));
            let size = c.total_compressed_size.max(0) as u64;
            out.push(first..last.saturating_add(size));
        }
    }
    Ok((fm.names, out))
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Several files as one
// ---------------------------------------------------------------------------

/// Thrift compact encoding, for the parts of a footer `merge_footers` writes
/// itself rather than copies.
struct Out {
    b: Vec<u8>,
}

impl Out {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }

    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }

    fn field(&mut self, id: i16, last: &mut i16, ty: u8) {
        let delta = id.wrapping_sub(*last);
        if (1..=15).contains(&delta) {
            self.b.push(((delta as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.zigzag(id as i64);
        }
        *last = id;
    }

    fn list(&mut self, elem: u8, n: usize) {
        if n < 15 {
            self.b.push(((n as u8) << 4) | elem);
        } else {
            self.b.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }

    /// A field whose value is copied as it was: the header written here, the
    /// value's bytes from `t` (none for a bool, which lives in the header).
    fn copy(&mut self, t: &mut Thrift, id: i16, last: &mut i16, ty: u8) -> Result<()> {
        self.field(id, last, ty);
        let from = t.at;
        t.skip(ty)?;
        self.b.extend_from_slice(&t.d[from..t.at]);
        Ok(())
    }

    /// An i64 offset field, moved by `base`. Zero and below mean "none" to
    /// every writer that writes them, and stay as they are.
    fn offset(&mut self, t: &mut Thrift, id: i16, last: &mut i16, ty: u8, base: u64) -> Result<()> {
        if ty != T_I64 {
            return self.copy(t, id, last, ty);
        }
        let v = t.zigzag()?;
        self.field(id, last, T_I64);
        self.zigzag(if v > 0 {
            v.saturating_add(base as i64)
        } else {
            v
        });
        Ok(())
    }
}

/// The schema of one part, for saying how two parts differ: per column its
/// name, whether it may be null, its fixed width and its physical type.
fn part_schema(fm: &FileMeta) -> Vec<(String, bool, usize, i64)> {
    let first = fm.groups.first();
    (0..fm.names.len())
        .map(|i| {
            let ty = first.and_then(|g| g.columns.get(i)).map_or(-1, |c| c.ty);
            (fm.names[i].clone(), fm.optional[i], fm.type_length[i], ty)
        })
        .collect()
}

/// The footer of the file made by writing several Parquet files one after
/// another: the first file's schema and key-value metadata, every file's row
/// groups in order with each offset moved by where its file starts, and the
/// rows of all of them. `parts` holds, per file, where it starts in the whole
/// and its metadata (the bytes before the metadata's length and the closing
/// magic); `names` names each file for an error.
///
/// The files must have the same columns, in the same order, of the same types
/// and nullability -- the pages of one are decoded with the schema of the
/// first -- and each error says which file and which column differ.
/// Columns a table holds in its paths rather than in its files: Hive
/// partitions, `date=2026-10-01/`. `values` has, per part, a value per name.
pub(crate) struct Partitions<'a> {
    pub names: &'a [String],
    pub values: &'a [Vec<String>],
}

/// [`merge_footers`] for parts that carry `partitions` in their paths: each
/// partition column the files do not already have is added to the schema as
/// text, and to every row group as a chunk of one dictionary value -- two
/// pages of a few bytes, handed to `place` to write and say where.
pub(crate) fn merge_footers_with(
    parts: &[(u64, &[u8])],
    names: &[&str],
    partitions: &Partitions,
    place: &mut dyn FnMut(&[u8]) -> Result<u64>,
) -> Result<Vec<u8>> {
    merge(parts, names, Some(partitions), place)
}

pub(crate) fn merge_footers(parts: &[(u64, &[u8])], names: &[&str]) -> Result<Vec<u8>> {
    merge(parts, names, None, &mut |_| {
        unreachable!("nothing to place")
    })
}

fn merge(
    parts: &[(u64, &[u8])],
    names: &[&str],
    partitions: Option<&Partitions>,
    place: &mut dyn FnMut(&[u8]) -> Result<u64>,
) -> Result<Vec<u8>> {
    let Some(&(_, first)) = parts.first() else {
        return fail("no parquet files to read", "");
    };
    let with_magic = |meta: &[u8]| {
        let mut whole = Vec::with_capacity(meta.len() + 12);
        whole.extend_from_slice(b"PAR1");
        whole.extend_from_slice(meta);
        whole.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        whole.extend_from_slice(b"PAR1");
        whole
    };
    let schema0 = part_schema(&read_file_meta(&with_magic(first), names[0])?);
    // The partition columns the files do not have themselves.
    let extra: Vec<usize> = partitions.map_or(Vec::new(), |p| {
        (0..p.names.len())
            .filter(|&i| !schema0.iter().any(|c| c.0 == p.names[i]))
            .collect()
    });
    let mut rows = 0i64;
    let mut groups = Out { b: Vec::new() };
    let mut group_count = 0usize;
    // The first file's fields other than the rows and row groups, as written.
    let mut kept: Vec<(i16, u8, Vec<u8>)> = Vec::new();
    for (p, &(base, meta)) in parts.iter().enumerate() {
        let name = names[p];
        if p > 0 {
            let schema = part_schema(&read_file_meta(&with_magic(meta), name)?);
            if schema != schema0 {
                return Err(schema_difference(&schema0, &schema, names[0], name));
            }
        }
        let mut t = Thrift::new(meta, name);
        let (mut id, mut last) = (0i16, 0i16);
        loop {
            let ty = t.field(&mut id, &mut last)?;
            if ty == T_STOP {
                break;
            }
            match id {
                3 => rows = rows.saturating_add(t.zigzag()?),
                4 => {
                    let (_, n) = t.list()?;
                    for _ in 0..n {
                        let added: Vec<(&str, &str)> = extra
                            .iter()
                            .map(|&i| {
                                let pt = partitions.expect("extra columns come from partitions");
                                (pt.names[i].as_str(), pt.values[p][i].as_str())
                            })
                            .collect();
                        merge_row_group(&mut t, &mut groups, base, name, &added, place)?;
                    }
                    group_count += n;
                }
                8 | 9 => return unsupported("encrypted parquet files are not read here", name),
                // The schema, with the partition columns added after the
                // file's own.
                2 if p == 0 && !extra.is_empty() => {
                    let pt = partitions.expect("extra columns come from partitions");
                    let added: Vec<&str> = extra.iter().map(|&i| pt.names[i].as_str()).collect();
                    kept.push((id, ty, schema_with(&mut t, &added)?));
                }
                // Column orders name every column; with columns added they
                // would not, and nothing here reads them.
                7 if !extra.is_empty() => t.skip(ty)?,
                _ if p == 0 => {
                    let from = t.at;
                    t.skip(ty)?;
                    kept.push((id, ty, t.d[from..t.at].to_vec()));
                }
                _ => t.skip(ty)?,
            }
        }
    }
    let mut out = Out {
        b: Vec::with_capacity(groups.b.len() + 4096),
    };
    let mut last = 0i16;
    let mut wrote_groups = false;
    let groups_now = |out: &mut Out, last: &mut i16| {
        out.field(3, last, T_I64);
        out.zigzag(rows);
        out.field(4, last, T_LIST);
        out.list(T_STRUCT, group_count);
        out.b.extend_from_slice(&groups.b);
    };
    kept.sort_by_key(|k| k.0);
    for (id, ty, value) in &kept {
        if *id > 4 && !wrote_groups {
            groups_now(&mut out, &mut last);
            wrote_groups = true;
        }
        out.field(*id, &mut last, *ty);
        out.b.extend_from_slice(value);
    }
    if !wrote_groups {
        groups_now(&mut out, &mut last);
    }
    out.b.push(T_STOP);
    Ok(out.b)
}

/// A row group's row count, read ahead of the columns that come before it.
fn row_group_rows(t: &Thrift) -> Result<i64> {
    let mut ahead = Thrift {
        d: t.d,
        at: t.at,
        path: t.path,
    };
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = ahead.field(&mut id, &mut last)?;
        if ty == T_STOP {
            return fail("a row group without a row count", t.path);
        }
        if id == 3 && ty == T_I64 {
            return ahead.zigzag();
        }
        ahead.skip(ty)?;
    }
}

/// The schema list with `added` text columns after the file's own, the root's
/// child count raised to match.
fn schema_with(t: &mut Thrift, added: &[&str]) -> Result<Vec<u8>> {
    let mut out = Out { b: Vec::new() };
    let (elem, n) = t.list()?;
    if n == 0 || elem != T_STRUCT {
        return fail("the parquet schema has no root", t.path);
    }
    out.list(T_STRUCT, n + added.len());
    // The root: every field as it was, but the number of children.
    let (mut id, mut last, mut olast) = (0i16, 0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            out.b.push(T_STOP);
            break;
        }
        if id == 5 && ty == T_I32 {
            let children = t.zigzag()?;
            out.field(5, &mut olast, T_I32);
            out.zigzag(children + added.len() as i64);
        } else {
            out.copy(t, id, &mut olast, ty)?;
        }
    }
    let from = t.at;
    for _ in 1..n {
        t.skip(T_STRUCT)?;
    }
    out.b.extend_from_slice(&t.d[from..t.at]);
    for name in added {
        // type BYTE_ARRAY, REQUIRED, the name, converted type UTF8.
        let mut last = 0i16;
        out.field(1, &mut last, T_I32);
        out.zigzag(TYPE_BYTE_ARRAY);
        out.field(3, &mut last, T_I32);
        out.zigzag(0);
        out.field(4, &mut last, T_BINARY);
        out.varint(name.len() as u64);
        out.b.extend_from_slice(name.as_bytes());
        out.field(6, &mut last, T_I32);
        out.zigzag(0);
        out.b.push(T_STOP);
    }
    Ok(out.b)
}

/// One row group's chunk of a partition column: `rows` copies of `value`, as
/// a dictionary of one and a page of zeros pointing at it.
fn constant_chunk(
    out: &mut Out,
    name: &str,
    value: &str,
    rows: i64,
    place: &mut dyn FnMut(&[u8]) -> Result<u64>,
) -> Result<()> {
    let mut dict_body = Out { b: Vec::new() };
    dict_body
        .b
        .extend_from_slice(&(value.len() as u32).to_le_bytes());
    dict_body.b.extend_from_slice(value.as_bytes());
    // Bit width 1, then one RLE run of `rows` zeros.
    let mut data_body = Out { b: vec![1] };
    data_body.varint((rows.max(0) as u64) << 1);
    data_body.b.push(0);
    let page = |kind: i64, body: &[u8], header_id: i16, header: &dyn Fn(&mut Out)| {
        let mut h = Out { b: Vec::new() };
        let mut last = 0i16;
        h.field(1, &mut last, T_I32);
        h.zigzag(kind);
        h.field(2, &mut last, T_I32);
        h.zigzag(body.len() as i64);
        h.field(3, &mut last, T_I32);
        h.zigzag(body.len() as i64);
        h.field(header_id, &mut last, T_STRUCT);
        header(&mut h);
        h.b.push(T_STOP);
        h.b.extend_from_slice(body);
        h.b
    };
    let dict = page(2, &dict_body.b, 7, &|h: &mut Out| {
        let mut last = 0i16;
        h.field(1, &mut last, T_I32);
        h.zigzag(1);
        h.field(2, &mut last, T_I32);
        h.zigzag(ENC_PLAIN_DICTIONARY);
        h.b.push(T_STOP);
    });
    let data = page(0, &data_body.b, 5, &|h: &mut Out| {
        let mut last = 0i16;
        h.field(1, &mut last, T_I32);
        h.zigzag(rows);
        h.field(2, &mut last, T_I32);
        h.zigzag(ENC_PLAIN_DICTIONARY);
        h.field(3, &mut last, T_I32);
        h.zigzag(ENC_RLE);
        h.field(4, &mut last, T_I32);
        h.zigzag(ENC_RLE);
        h.b.push(T_STOP);
    });
    let mut both = dict.clone();
    both.extend_from_slice(&data);
    let at = place(&both)? as i64;
    let size = both.len() as i64;
    // ColumnChunk: file_offset, then the metadata.
    let mut last = 0i16;
    out.field(2, &mut last, T_I64);
    out.zigzag(at);
    out.field(3, &mut last, T_STRUCT);
    let mut m = 0i16;
    out.field(1, &mut m, T_I32);
    out.zigzag(TYPE_BYTE_ARRAY);
    out.field(2, &mut m, T_LIST);
    out.list(T_I32, 3);
    for e in [ENC_PLAIN, ENC_RLE, ENC_PLAIN_DICTIONARY] {
        out.zigzag(e);
    }
    out.field(3, &mut m, T_LIST);
    out.list(T_BINARY, 1);
    out.varint(name.len() as u64);
    out.b.extend_from_slice(name.as_bytes());
    out.field(4, &mut m, T_I32);
    out.zigzag(0);
    out.field(5, &mut m, T_I64);
    out.zigzag(rows);
    out.field(6, &mut m, T_I64);
    out.zigzag(size);
    out.field(7, &mut m, T_I64);
    out.zigzag(size);
    out.field(9, &mut m, T_I64);
    out.zigzag(at + dict.len() as i64);
    out.field(11, &mut m, T_I64);
    out.zigzag(at);
    out.b.push(T_STOP);
    out.b.push(T_STOP);
    Ok(())
}

fn merge_row_group(
    t: &mut Thrift,
    out: &mut Out,
    base: u64,
    name: &str,
    added: &[(&str, &str)],
    place: &mut dyn FnMut(&[u8]) -> Result<u64>,
) -> Result<()> {
    let rows = if added.is_empty() {
        0
    } else {
        row_group_rows(t)?
    };
    let (mut id, mut last, mut olast) = (0i16, 0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            out.b.push(T_STOP);
            return Ok(());
        }
        match id {
            1 if ty == T_LIST => {
                let (elem, n) = t.list()?;
                out.field(1, &mut olast, T_LIST);
                out.list(elem, n + added.len());
                for _ in 0..n {
                    merge_chunk(t, out, base, name)?;
                }
                for (column, value) in added {
                    constant_chunk(out, column, value, rows, place)?;
                }
            }
            5 => out.offset(t, id, &mut olast, ty, base)?,
            // The ordinal numbers a row group within its file, for decryption;
            // it would be wrong in the whole, and nothing here needs it.
            7 => t.skip(ty)?,
            _ => out.copy(t, id, &mut olast, ty)?,
        }
    }
}

fn merge_chunk(t: &mut Thrift, out: &mut Out, base: u64, name: &str) -> Result<()> {
    let (mut id, mut last, mut olast) = (0i16, 0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            out.b.push(T_STOP);
            return Ok(());
        }
        match id {
            1 => {
                return unsupported(
                    "a parquet file whose column data lives in another file is not read here",
                    name,
                );
            }
            2 | 4 | 6 => out.offset(t, id, &mut olast, ty, base)?,
            3 if ty == T_STRUCT => {
                out.field(3, &mut olast, T_STRUCT);
                let (mut mid, mut mlast, mut molast) = (0i16, 0i16, 0i16);
                loop {
                    let mty = t.field(&mut mid, &mut mlast)?;
                    if mty == T_STOP {
                        out.b.push(T_STOP);
                        break;
                    }
                    match mid {
                        9 | 10 | 11 | 14 => out.offset(t, mid, &mut molast, mty, base)?,
                        _ => out.copy(t, mid, &mut molast, mty)?,
                    }
                }
            }
            8 | 9 => return unsupported("encrypted parquet files are not read here", name),
            _ => out.copy(t, id, &mut olast, ty)?,
        }
    }
}

/// What tells two parts' schemas apart, as an error naming both files.
fn schema_difference(
    a: &[(String, bool, usize, i64)],
    b: &[(String, bool, usize, i64)],
    a_name: &str,
    b_name: &str,
) -> Error {
    let names =
        |s: &[(String, bool, usize, i64)]| s.iter().map(|c| c.0.clone()).collect::<Vec<_>>();
    let (an, bn) = (names(a), names(b));
    let only = |x: &[String], y: &[String]| {
        x.iter()
            .filter(|n| !y.contains(n))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let what = if an != bn {
        let (missing, extra) = (only(&an, &bn), only(&bn, &an));
        match (missing.is_empty(), extra.is_empty()) {
            (true, true) => "the same columns in a different order".to_string(),
            (false, true) => format!("no {missing}"),
            (true, false) => format!("{extra} as well"),
            (false, false) => format!("{extra} instead of {missing}"),
        }
    } else {
        let (x, y) = a
            .iter()
            .zip(b)
            .find(|(x, y)| x != y)
            .expect("schemas differ");
        if x.1 != y.1 {
            format!(
                "{} {} where {a_name} has it {}",
                x.0,
                if y.1 { "nullable" } else { "required" },
                if x.1 { "nullable" } else { "required" }
            )
        } else {
            format!("{} stored as another type", x.0)
        }
    };
    Error::new(format!(
        "{b_name} has {what}; the files of one side are read as one table and must have the \
         columns of the first ({a_name}), in its order and of its types"
    ))
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
    if let Some(refusal) = crate::source::page_bomb(
        out.num_values.max(0) as u64,
        out.uncompressed.max(0) as u64,
        out.compressed.max(0) as u64,
    ) {
        return Err(Error::new(format!("{refusal}: {path}")));
    }
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
/// It appends to `out`, writing into room reserved past the page so that short
/// literals and back-references move as whole 8- or 16-byte words instead of
/// a `memcpy` call each. That matters more than it looks: this is the one loop
/// that touches every byte of a compressed file, and its elements are mostly a
/// few bytes long.
///
/// `Ok(false)` is a malformed stream; `Err` is the one allocation it makes --
/// the room for the expanded page -- refusing. The two are worth keeping apart:
/// the caller reports them differently, and calling a budget a corrupt file
/// sends the reader after the wrong bug.
fn snappy_append(input: &[u8], out: &mut Vec<u8>) -> Result<bool> {
    let Some((want, at)) = snappy_length(input) else {
        return Ok(false);
    };
    let base = out.len();
    // The room past the page lets a short literal or copy move as one or two
    // fixed-width words, spilling up to SNAPPY_SLACK - 1 bytes past its end:
    // what comes next writes over them, and the length set at the end cuts
    // them off. Most elements are a few bytes to a few dozen, and a `memcpy`
    // call per element was the loop's largest cost.
    alloc::grow(
        out,
        want.saturating_add(SNAPPY_SLACK),
        "the expanded snappy page",
    )?;
    // Safety: the reserve above made room for `base + want + SNAPPY_SLACK`
    // bytes, and everything below `base` is the vector's own.
    let (dst, ok) = unsafe { snappy_body(input, at, out.as_mut_ptr(), base, base + want) };
    // Safety: every byte below `dst` was written by `snappy_body`, and `dst`
    // never passes `base + want`, inside the room reserved above.
    unsafe { out.set_len(dst) };
    Ok(ok && dst == base + want)
}

/// The expanded length a snappy stream starts with, and where its elements
/// begin; `None` when the varint is cut short or runs past 64 bits.
fn snappy_length(input: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0usize;
    let mut want = 0u64;
    let mut shift = 0;
    loop {
        let b = *input.get(at)?;
        at += 1;
        want |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((usize::try_from(want).ok()?, at));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Bytes `snappy_body` may write past the end of the page it is expanding.
const SNAPPY_SLACK: usize = 16;

/// The element loop of [`snappy_append`]: expands `input[at..]` to `ptr` from
/// `base` up to `end`, and returns how far it got and whether the stream was
/// well formed.
///
/// # Safety
///
/// `ptr` must be valid for writes of `end + SNAPPY_SLACK` bytes, and nothing
/// else may touch `ptr[base..end + SNAPPY_SLACK]` while it runs. What it reads
/// back through `ptr` is what it has written itself.
unsafe fn snappy_body(
    input: &[u8],
    mut at: usize,
    ptr: *mut u8,
    base: usize,
    end: usize,
) -> (usize, bool) {
    let n = input.len();
    let src = input.as_ptr();
    let mut dst = base;
    while at < n {
        let tag = input[at];
        at += 1;
        if tag & 0x03 == 0 {
            // literal
            let mut len = (tag >> 2) as usize + 1;
            if len > 60 {
                let extra = len - 60;
                if at + extra > n {
                    return (dst, false);
                }
                let mut v = 0usize;
                for i in 0..extra {
                    v |= (input[at + i] as usize) << (8 * i);
                }
                at += extra;
                len = v + 1;
            }
            if len > n - at || len > end - dst {
                return (dst, false);
            }
            // Safety: `at + len <= n` and `dst + len <= end` were checked; the
            // short form reads 16 bytes only where the input has them and
            // writes at most 15 past `end`, inside the slack.
            unsafe {
                if len <= 16 && at + 16 <= n {
                    std::ptr::copy_nonoverlapping(src.add(at), ptr.add(dst), 16);
                } else {
                    std::ptr::copy_nonoverlapping(src.add(at), ptr.add(dst), len);
                }
            }
            dst += len;
            at += len;
            continue;
        }
        let (len, offset) = match tag & 0x03 {
            1 => {
                if at >= n {
                    return (dst, false);
                }
                let o = (((tag >> 5) as usize) << 8) | input[at] as usize;
                at += 1;
                (((tag >> 2) & 0x07) as usize + 4, o)
            }
            2 => {
                if at + 2 > n {
                    return (dst, false);
                }
                let o = input[at] as usize | ((input[at + 1] as usize) << 8);
                at += 2;
                ((tag >> 2) as usize + 1, o)
            }
            _ => {
                if at + 4 > n {
                    return (dst, false);
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
            return (dst, false);
        }
        let from = dst - offset;
        // Safety: the source starts inside what is already written (`offset`
        // was checked against it) and the copy ends by `end` (checked above);
        // the word-wide forms write at most 15 bytes past it, inside the
        // slack. Each word is read only after everything it covers was
        // written: a word of `w` bytes is taken from `w` or more bytes back.
        unsafe {
            if offset >= 16 {
                let mut i = 0;
                while i < len {
                    std::ptr::copy_nonoverlapping(ptr.add(from + i), ptr.add(dst + i), 16);
                    i += 16;
                }
            } else if offset >= 8 {
                let mut i = 0;
                while i < len {
                    std::ptr::copy_nonoverlapping(ptr.add(from + i), ptr.add(dst + i), 8);
                    i += 8;
                }
            } else {
                // A pattern shorter than a word repeats into itself.
                for i in 0..len {
                    *ptr.add(dst + i) = *ptr.add(from + i);
                }
            }
        }
        dst += len;
    }
    (dst, true)
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
    let expanded = (0..fm.names.len())
        .map(|c| {
            fm.groups
                .iter()
                .filter_map(|g| g.columns.get(c))
                .map(|m| m.total_uncompressed_size.max(0) as u64)
                .sum()
        })
        .collect();
    Meta {
        names: fm.names.clone(),
        rows: fm.rows,
        row_groups: fm.groups.len(),
        held,
        expanded,
    }
}

/// Reads one column across every row group. `which` indexes into [`Meta::names`];
/// `fm` is the file's footer, parsed once by the caller.
///
/// It takes up to `ways` threads: the row groups are split into that many runs
/// of about equal size, and each run is decoded on a thread of its own into its
/// own stretch of the column's buffers.
///
/// The answer is the one a single pass gives, value for value; only where an
/// expanded page sits in [`Column::owned`] differs. Anything a run cannot be
/// sure of on its own -- an error, a page that expands past what the footer
/// said its chunk holds, chunks whose counts do not add up -- sends the
/// column back through the single pass, which decides it the way it always
/// has. On a file that is well formed, that never happens.
pub(crate) fn read_column(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    path: &str,
    ways: usize,
) -> Result<Column> {
    if which >= fm.names.len() {
        return fail("no such column in the parquet file", path);
    }
    let ty = fm
        .groups
        .first()
        .and_then(|g| g.columns.get(which))
        .map_or(TYPE_BYTE_ARRAY, |c| c.ty);
    if ty != TYPE_BYTE_ARRAY {
        return read_words(data, fm, which, ty, path, ways);
    }

    let mut uncompressed = 0i64;
    for rg in &fm.groups {
        if which >= rg.columns.len() {
            return fail("a row group is missing a column", path);
        }
        uncompressed += rg.columns[which].total_uncompressed_size;
    }

    if let Some(runs) = runs(fm, which, ways)
        && let Some(col) = bytes_in_runs(data, fm, which, &runs, path)
    {
        return Ok(col);
    }

    let mut owned: Vec<u8> = Vec::new();
    let Some(run) = byte_run(
        data,
        fm,
        which,
        0..fm.groups.len(),
        fm.rows as usize,
        uncompressed as usize,
        &mut owned,
        path,
    )?
    else {
        return fail("a parquet page did not fit the column's buffer", path);
    };
    let mut col = Column {
        dictionary: run.dictionary,
        dict: run.dict,
        index: run.index,
        values: run.values,
        owned,
        words: None,
    };
    // A column that never met a page at all is plain and empty, not dictionary.
    if col.dictionary && col.index.is_empty() && col.dict.is_empty() {
        col.dictionary = false;
    }
    Ok(col)
}

/// A run of consecutive row groups, decoded by one thread when a column is
/// read several ways at once.
struct Run {
    groups: std::ops::Range<usize>,
    /// Values, nulls included, that the run's chunks say they hold: its rows.
    rows: usize,
    /// What the run's chunks say they expand to, page headers included, which
    /// bounds what their pages expand to.
    bytes: usize,
}

/// The row groups cut into up to `ways` runs of about equal expanded size, or
/// `None` when the column is read in one pass: one way asked for, one row
/// group, or chunk counts that do not add up to the file's rows. A run has to
/// know where its rows start, and the counts are what says so.
fn runs(fm: &FileMeta, which: usize, ways: usize) -> Option<Vec<Run>> {
    let ways = ways.min(fm.groups.len());
    if ways < 2 {
        return None;
    }
    let (mut rows, mut bytes) = (0u64, 0u64);
    for g in &fm.groups {
        let c = g.columns.get(which)?;
        rows = rows.checked_add(u64::try_from(c.num_values).ok()?)?;
        bytes = bytes.checked_add(u64::try_from(c.total_uncompressed_size).ok()?)?;
    }
    if rows != u64::try_from(fm.rows).ok()? {
        return None;
    }
    let mut out: Vec<Run> = Vec::with_capacity(ways);
    let mut start = 0usize;
    let (mut run_rows, mut run_bytes, mut seen) = (0usize, 0usize, 0u64);
    for (i, g) in fm.groups.iter().enumerate() {
        let c = &g.columns[which];
        run_rows += c.num_values as usize;
        run_bytes += c.total_uncompressed_size as usize;
        seen += c.total_uncompressed_size as u64;
        // Cut where the bytes so far reach the next share of the whole.
        let share = bytes * (out.len() as u64 + 1) / ways as u64;
        if seen >= share && i + 1 < fm.groups.len() && out.len() + 1 < ways {
            out.push(Run {
                groups: start..i + 1,
                rows: run_rows,
                bytes: run_bytes,
            });
            start = i + 1;
            (run_rows, run_bytes) = (0, 0);
        }
    }
    out.push(Run {
        groups: start..fm.groups.len(),
        rows: run_rows,
        bytes: run_bytes,
    });
    (out.len() > 1).then_some(out)
}

/// Where a run of row groups puts the pages it has to expand. A growable
/// vector for the single pass; a fixed stretch of a shared buffer for a run,
/// which says so when a page does not fit rather than spilling into the next.
trait Arena {
    fn reserve(&mut self, bytes: usize) -> Result<()>;
    fn len(&self) -> usize;
    /// Appends the page, expanded. `Ok(false)` when it does not fit.
    fn expand(&mut self, codec: Codec, stored: &[u8], expected: i64, path: &str) -> Result<bool>;
    fn bytes(&self, from: usize, to: usize) -> &[u8];
}

impl Arena for Vec<u8> {
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        alloc::grow(self, bytes, "the uncompressed column")
    }
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn expand(&mut self, codec: Codec, stored: &[u8], expected: i64, path: &str) -> Result<bool> {
        expand_append(codec, stored, expected, self, path)?;
        Ok(true)
    }
    fn bytes(&self, from: usize, to: usize) -> &[u8] {
        &self[from..to]
    }
}

/// One run's stretch of a column's buffer: what the footer says the run's
/// chunks expand to, and [`SNAPPY_SLACK`] past it.
struct Region<'a> {
    buf: &'a mut [MaybeUninit<u8>],
    len: usize,
}

impl Arena for Region<'_> {
    fn reserve(&mut self, _: usize) -> Result<()> {
        Ok(())
    }
    fn len(&self) -> usize {
        self.len
    }
    fn expand(&mut self, codec: Codec, stored: &[u8], expected: i64, path: &str) -> Result<bool> {
        let room = self.buf.len() - self.len;
        if codec == Codec::Snappy {
            let Some((want, at)) = snappy_length(stored) else {
                return fail(
                    "a snappy page in the parquet file will not decompress",
                    path,
                );
            };
            if want > room.saturating_sub(SNAPPY_SLACK) {
                return Ok(false);
            }
            let (base, end) = (self.len, self.len + want);
            // Safety: `end + SNAPPY_SLACK` is within the region, checked
            // above, and the region is this run's alone.
            let (dst, ok) =
                unsafe { snappy_body(stored, at, self.buf.as_mut_ptr().cast(), base, end) };
            if !ok || dst != end {
                return fail(
                    "a snappy page in the parquet file will not decompress",
                    path,
                );
            }
            self.len = end;
            return Ok(true);
        }
        let Ok(expected) = usize::try_from(expected) else {
            return fail("a parquet page has a negative size", path);
        };
        if expected > room {
            return Ok(false);
        }
        let page =
            decompress(codec, stored, expected).map_err(|e| Error::new(format!("{e}: {path}")))?;
        if page.len() > room {
            return Ok(false);
        }
        // Safety: `page.len()` bytes fit from `self.len`, checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                page.as_ptr(),
                self.buf.as_mut_ptr().add(self.len).cast(),
                page.len(),
            );
        }
        self.len += page.len();
        Ok(true)
    }
    fn bytes(&self, from: usize, to: usize) -> &[u8] {
        assert!(from <= to && to <= self.len);
        // Safety: everything below `self.len` has been written.
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr().add(from).cast(), to - from) }
    }
}

/// What a run of row groups decoded to; the fields are [`Column`]'s. A slice
/// into an expanded page counts from the start of the run's own arena.
struct ByteRun {
    dictionary: bool,
    dict: Vec<Slice>,
    index: Vec<i32>,
    values: Vec<Slice>,
    codec: Option<Codec>,
}

/// The page walk of a byte-array column over `groups`, expanding compressed
/// pages into `owned`. `rows` sizes the per-row vectors and `uncompressed`
/// the arena, both up front. `Ok(None)` when `owned` is a region a page does
/// not fit.
#[allow(clippy::too_many_arguments)]
fn byte_run<A: Arena>(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    groups: std::ops::Range<usize>,
    rows: usize,
    uncompressed: usize,
    owned: &mut A,
    path: &str,
) -> Result<Option<ByteRun>> {
    let optional = fm.optional[which];
    // The column's parts are held as locals rather than in a `Column`, because
    // a page is read *out of* `owned` while its values are pushed *into* `dict`
    // or `values`, and those are only disjoint borrows if they are separate
    // bindings. They become a `Column` at the end.
    let mut dict: Vec<Slice> = Vec::new();
    let mut index: Vec<i32> = Vec::new();
    let mut values: Vec<Slice> = Vec::new();
    let mut codec: Option<Codec> = None;

    // Reused across pages so the per-page work allocates nothing, and sized
    // once from the footer's row count so appending a page never has to move
    // eighty megabytes of what is already decoded.
    let mut defs: Vec<i32> = Vec::new();
    let mut idx: Vec<i32> = Vec::new();
    let mut got: Vec<Slice> = Vec::new();
    alloc::grow(&mut index, rows, "one dictionary index per row")?;

    // The column starts out held as dictionary indices and stays that way only
    // if every page cooperates. A writer that gives up on the dictionary
    // partway -- DuckDB does, once a column's distinct values outgrow its
    // budget, which at ten million rows is most of them -- forces the whole
    // column into the plain form. Expanding what has been read costs no bytes:
    // a dictionary entry is already a slice.
    let mut dictionary = true;

    for rg in &fm.groups[groups] {
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
                // The slack is what the snappy decoder may spill past the last
                // page; without it, the last page's reserve would move the
                // whole column.
                owned.reserve(uncompressed.saturating_add(SNAPPY_SLACK))?;
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
                if !owned.expand(
                    chunk_codec,
                    &data[h.after..h.after + body_len],
                    h.uncompressed,
                    path,
                )? {
                    return Ok(None);
                }
                (was, owned.len() - was, was as u64, true)
            } else {
                (h.after, body_len, h.after as u64, false)
            };
            let body: &[u8] = if in_owned {
                owned.bytes(page_from, page_from + page_len)
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
                        alloc::grow(&mut values, rows, "one handle per row")?;
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
    Ok(Some(ByteRun {
        dictionary,
        dict,
        index,
        values,
        codec,
    }))
}

/// A byte-array column read as `runs`, one thread each, or `None` for the
/// single pass to read it instead (see [`read_column`]).
fn bytes_in_runs(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    runs: &[Run],
    path: &str,
) -> Option<Column> {
    // One buffer for every run's expanded pages, each run writing its own
    // stretch of it, so the pages are expanded once, where they stay -- not
    // into a buffer per run and then copied together, which would hold the
    // column twice.
    let compressed =
        codec_of(fm.groups[runs[0].groups.start].columns[which].codec, path).ok()? != Codec::None;
    let mut starts = Vec::with_capacity(runs.len());
    let mut total = 0usize;
    for r in runs {
        starts.push(total);
        if compressed {
            total = total.checked_add(r.bytes.checked_add(SNAPPY_SLACK)?)?;
        }
    }
    let mut owned: Vec<u8> = Vec::new();
    owned.try_reserve_exact(total).ok()?;

    let decoded: Vec<Option<(ByteRun, usize)>> = {
        let mut rest = &mut owned.spare_capacity_mut()[..total];
        let mut regions = Vec::with_capacity(runs.len());
        for r in runs {
            let take = if compressed {
                r.bytes + SNAPPY_SLACK
            } else {
                0
            };
            let (mine, after) = rest.split_at_mut(take);
            regions.push(parallel::Once::new(Region { buf: mine, len: 0 }));
            rest = after;
        }
        let one = |k: usize| -> Option<(ByteRun, usize)> {
            let mut region = regions[k].take()?;
            let r = &runs[k];
            let run = byte_run(
                data,
                fm,
                which,
                r.groups.clone(),
                r.rows,
                0,
                &mut region,
                path,
            )
            .ok()??;
            let rows = if run.dictionary {
                run.index.len()
            } else {
                run.values.len()
            };
            (rows == r.rows).then_some((run, region.len))
        };
        std::thread::scope(|scope| {
            let others: Vec<_> = (1..runs.len())
                .map(|k| parallel::spawn_at(scope, &one, k))
                .collect();
            let mut all = vec![one(0)];
            all.extend(others.into_iter().map(|h| h.join().ok().flatten()));
            all
        })
    };
    let decoded: Vec<(ByteRun, usize)> = decoded.into_iter().collect::<Option<_>>()?;
    if decoded
        .iter()
        .any(|(run, _)| run.codec != decoded[0].0.codec)
    {
        return None;
    }

    // Every byte below the buffer's length has to be written: the stretches
    // the runs left unused are zeroed, though nothing points into them.
    if compressed {
        let spare = owned.spare_capacity_mut();
        for (k, (_, used)) in decoded.iter().enumerate() {
            let to = starts.get(k + 1).copied().unwrap_or(total);
            spare[starts[k] + used..to].fill(MaybeUninit::new(0));
        }
        // Safety: each run wrote `starts[k]..starts[k] + used`, and the rest
        // of `..total` was zeroed just above.
        unsafe { owned.set_len(total) };
    }

    // A run counted its slices from its own start, and its dictionary
    // indices from its own first dictionary page.
    let shift = |k: usize| if compressed { starts[k] as u64 } else { 0 };
    let rows: usize = runs.iter().map(|r| r.rows).sum();
    let dictionary = decoded.iter().all(|(run, _)| run.dictionary);
    let mut dict: Vec<Slice> =
        alloc::sized(decoded.iter().map(|(run, _)| run.dict.len()).sum(), "").ok()?;
    let mut dict_at = Vec::with_capacity(decoded.len());
    for (k, (run, _)) in decoded.iter().enumerate() {
        dict_at.push(dict.len());
        dict.extend(run.dict.iter().map(|s| s.shifted(shift(k))));
    }
    let mut index: Vec<i32> = Vec::new();
    let mut values: Vec<Slice> = Vec::new();
    if dictionary {
        index = alloc::sized(rows, "").ok()?;
        for (k, (run, _)) in decoded.into_iter().enumerate() {
            let off = i32::try_from(dict_at[k]).ok()?;
            index.extend(run.index.iter().map(|&v| if v < 0 { v } else { v + off }));
        }
    } else {
        values = alloc::sized(rows, "").ok()?;
        for (k, (run, _)) in decoded.into_iter().enumerate() {
            if run.dictionary {
                values.extend(run.index.iter().map(|&v| {
                    if v < 0 {
                        Slice::null()
                    } else {
                        dict[dict_at[k] + v as usize]
                    }
                }));
            } else {
                values.extend(run.values.iter().map(|s| s.shifted(shift(k))));
            }
        }
    }
    let mut col = Column {
        dictionary,
        dict,
        index,
        values,
        owned,
        words: None,
    };
    if col.dictionary && col.index.is_empty() && col.dict.is_empty() {
        col.dictionary = false;
    }
    Some(col)
}

/// Reads a column of word-sized values across every row group, into
/// [`Column::words`]. The page walk is [`read_column`]'s; what differs is that
/// a value is decoded into its word as it is read, so a dictionary page is
/// decoded once and its entries copied per row.
fn read_words(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    ty: i64,
    path: &str,
    ways: usize,
) -> Result<Column> {
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
    if let Some(runs) = runs(fm, which, ways)
        && let Some(col) = words_in_runs(data, fm, which, ty, width, &runs, path)
    {
        return Ok(col);
    }

    let rows = fm.rows.max(0) as usize;
    let mut values: Vec<[u8; 8]> = alloc::sized(rows, "one value per row")?;
    let (n, nulls) = match words_run(
        data,
        fm,
        which,
        ty,
        width,
        0..fm.groups.len(),
        &mut values.spare_capacity_mut()[..rows],
        true,
        path,
    )? {
        Some(done) => done,
        None => {
            return fail(
                "a parquet column holds more values than the file has rows",
                path,
            );
        }
    };
    // Safety: `words_run` wrote the first `n`.
    unsafe { values.set_len(n) };
    Ok(Column {
        words: Some(Words { values, nulls }),
        ..Column::default()
    })
}

/// A word column read as `runs`, one thread each, or `None` for the single
/// pass to read it instead (see [`read_column`]).
fn words_in_runs(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    ty: i64,
    width: usize,
    runs: &[Run],
    path: &str,
) -> Option<Column> {
    let rows: usize = runs.iter().map(|r| r.rows).sum();
    let mut values: Vec<[u8; 8]> = alloc::sized(rows, "").ok()?;
    // Each run writes its own rows of `values`, where they stay.
    let decoded: Vec<Option<Vec<u64>>> = {
        let mut rest = &mut values.spare_capacity_mut()[..rows];
        let mut slots = Vec::with_capacity(runs.len());
        for r in runs {
            let (mine, after) = rest.split_at_mut(r.rows);
            slots.push(parallel::Once::new(mine));
            rest = after;
        }
        let one = |k: usize| -> Option<Vec<u64>> {
            let out = slots[k].take()?;
            let want = out.len();
            let (n, nulls) = words_run(
                data,
                fm,
                which,
                ty,
                width,
                runs[k].groups.clone(),
                out,
                false,
                path,
            )
            .ok()??;
            (n == want).then_some(nulls)
        };
        std::thread::scope(|scope| {
            let others: Vec<_> = (1..runs.len())
                .map(|k| parallel::spawn_at(scope, &one, k))
                .collect();
            let mut all = vec![one(0)];
            all.extend(others.into_iter().map(|h| h.join().ok().flatten()));
            all
        })
    };
    let each: Vec<Vec<u64>> = decoded.into_iter().collect::<Option<_>>()?;
    // Safety: the runs cover `..rows` between them, and each wrote every one
    // of its rows -- `n == want` above.
    unsafe { values.set_len(rows) };
    let mut nulls: Vec<u64> = Vec::new();
    if each.iter().any(|n| !n.is_empty()) {
        nulls = alloc::filled(0u64, rows.div_ceil(64), "").ok()?;
        let mut at = 0usize;
        for (r, bits) in runs.iter().zip(&each) {
            or_bits(&mut nulls, at, bits);
            at += r.rows;
        }
    }
    Some(Column {
        words: Some(Words { values, nulls }),
        ..Column::default()
    })
}

/// ORs `src`, a bitmap of rows counted from zero, into `dst` from row `at` on.
fn or_bits(dst: &mut [u64], at: usize, src: &[u64]) {
    let shift = at % 64;
    for (i, &w) in src.iter().enumerate() {
        if w == 0 {
            continue;
        }
        let q = at / 64 + i;
        dst[q] |= w << shift;
        if shift != 0 && q + 1 < dst.len() {
            dst[q + 1] |= w >> (64 - shift);
        }
    }
}

/// The page walk of a word column over `groups`, writing a word per row into
/// `out` and returning how many it wrote and a bitmap of the null ones, counted
/// from `out`'s start (empty when none is). More values than `out` holds is an
/// error when `whole` -- `out` is then every row the file has -- and `Ok(None)`
/// for a run, which leaves the call to the single pass.
#[allow(clippy::too_many_arguments)]
fn words_run(
    data: &[u8],
    fm: &FileMeta,
    which: usize,
    ty: i64,
    width: usize,
    groups: std::ops::Range<usize>,
    out: &mut [MaybeUninit<[u8; 8]>],
    whole: bool,
    path: &str,
) -> Result<Option<(usize, Vec<u64>)>> {
    let optional = fm.optional[which];
    let rows = out.len();
    let mut len = 0usize;
    let mut nulls: Vec<u64> = Vec::new();
    let mut dict: Vec<[u8; 8]> = Vec::new();
    let mut page: Vec<u8> = Vec::new();
    let mut defs: Vec<i32> = Vec::new();
    let mut idx: Vec<i32> = Vec::new();
    let mut got: Vec<[u8; 8]> = Vec::new();

    for rg in &fm.groups[groups] {
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
                if n_vals > rows - len {
                    if !whole {
                        return Ok(None);
                    }
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
                    for (slot, &w) in out[len..len + real].iter_mut().zip(&got) {
                        slot.write(w);
                    }
                    len += real;
                } else {
                    if nulls.is_empty() {
                        nulls = alloc::filled(0u64, rows.div_ceil(64), "a null flag per row")?;
                    }
                    let mut k = 0usize;
                    for &d in &defs[..n_vals] {
                        if d != 0 {
                            out[len].write(got[k]);
                            k += 1;
                        } else {
                            nulls[len >> 6] |= 1u64 << (len & 63);
                            out[len].write([0; 8]);
                        }
                        len += 1;
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
    Ok(Some((len, nulls)))
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

// ---------------------------------------------------------------------------
// Nested string leaves, for a Delta Lake checkpoint
// ---------------------------------------------------------------------------

/// A leaf of a nested schema: its path of names from the root, its physical
/// type, and the definition and repetition levels it can reach.
struct NestedLeaf {
    path: Vec<String>,
    max_def: u32,
    max_rep: u32,
}

/// Walks a schema list in its depth-first order, giving each leaf its path
/// and levels. `at` is the next element; `parents` the path so far.
fn nested_leaves(
    elements: &[(String, i64, i64)],
    at: &mut usize,
    parents: &mut Vec<String>,
    def: u32,
    rep: u32,
    out: &mut Vec<NestedLeaf>,
) {
    let Some((name, repetition, children)) = elements.get(*at).cloned() else {
        return;
    };
    *at += 1;
    let def = def + u32::from(repetition == 1 || repetition == 2);
    let rep = rep + u32::from(repetition == 2);
    parents.push(name);
    if children > 0 {
        for _ in 0..children {
            nested_leaves(elements, at, parents, def, rep, out);
        }
    } else {
        out.push(NestedLeaf {
            path: parents.clone(),
            max_def: def,
            max_rep: rep,
        });
    }
    parents.pop();
}

/// A column chunk as the nested reader needs it: codec, values, dictionary
/// offset, data offset, stored size.
type NestedChunk = (i64, i64, i64, i64, i64);

/// The bits a level up to `max` takes.
fn level_width(max: u32) -> u32 {
    32 - max.leading_zeros()
}

/// The values of the string leaves at `paths` -- each a path of names from
/// the root, `["add", "path"]` -- in a Parquet file with nested columns, as a
/// Delta Lake checkpoint is: one per row, `None` where the leaf or a struct
/// above it is null. A leaf under a repeated field (a list, a map) is
/// refused; a path that names no leaf is all `None`.
pub(crate) fn nested_strings(
    data: &[u8],
    paths: &[&[&str]],
    name: &str,
) -> Result<Vec<Vec<Option<String>>>> {
    if data.len() < 12 || &data[..4] != b"PAR1" || &data[data.len() - 4..] != b"PAR1" {
        return fail("not a parquet file", name);
    }
    let meta_len =
        u32::from_le_bytes(data[data.len() - 8..data.len() - 4].try_into().unwrap()) as usize;
    if meta_len + 12 > data.len() {
        return fail("the parquet footer is longer than the file", name);
    }
    let meta = &data[data.len() - 8 - meta_len..data.len() - 8];
    let mut t = Thrift::new(meta, name);
    // (name, repetition, children) per schema element; per row group, per
    // leaf: codec, values, dictionary offset, data offset, stored size.
    let mut elements: Vec<(String, i64, i64)> = Vec::new();
    let mut groups: Vec<Vec<NestedChunk>> = Vec::new();
    let (mut id, mut last) = (0i16, 0i16);
    loop {
        let ty = t.field(&mut id, &mut last)?;
        if ty == T_STOP {
            break;
        }
        match id {
            2 => {
                let (_, n) = t.list()?;
                for _ in 0..n {
                    let (mut sid, mut slast) = (0i16, 0i16);
                    let (mut nm, mut repetition, mut children) = (String::new(), 0i64, 0i64);
                    loop {
                        let st = t.field(&mut sid, &mut slast)?;
                        if st == T_STOP {
                            break;
                        }
                        match sid {
                            3 => repetition = t.zigzag()?,
                            4 => nm = String::from_utf8_lossy(t.binary()?).into_owned(),
                            5 => children = t.zigzag()?,
                            _ => t.skip(st)?,
                        }
                    }
                    elements.push((nm, repetition, children));
                }
            }
            4 => {
                let (_, n) = t.list()?;
                for _ in 0..n {
                    let mut chunks = Vec::new();
                    let (mut gid, mut glast) = (0i16, 0i16);
                    loop {
                        let gt = t.field(&mut gid, &mut glast)?;
                        if gt == T_STOP {
                            break;
                        }
                        if gid != 1 {
                            t.skip(gt)?;
                            continue;
                        }
                        let (_, m) = t.list()?;
                        for _ in 0..m {
                            let mut c = ChunkMeta::default();
                            read_chunk(&mut t, &mut c)?;
                            chunks.push((
                                c.codec,
                                c.num_values,
                                c.dictionary_page_offset,
                                c.data_page_offset,
                                c.total_compressed_size,
                            ));
                        }
                    }
                    groups.push(chunks);
                }
            }
            _ => t.skip(ty)?,
        }
    }
    let mut leaves = Vec::new();
    let (mut at, mut parents) = (1usize, Vec::new());
    let root_children = elements.first().map_or(0, |e| e.2);
    for _ in 0..root_children {
        nested_leaves(&elements, &mut at, &mut parents, 0, 0, &mut leaves);
    }
    let rows: usize = groups
        .iter()
        .map(|g| g.first().map_or(0, |c| c.1.max(0) as usize))
        .sum();
    let mut out = Vec::with_capacity(paths.len());
    for want in paths {
        let Some((index, leaf)) = leaves
            .iter()
            .enumerate()
            .find(|(_, l)| l.path.iter().map(String::as_str).eq(want.iter().copied()))
        else {
            out.push(vec![None; rows]);
            continue;
        };
        if leaf.max_rep > 0 {
            return unsupported("a repeated nested column is not read here", name);
        }
        let mut values = Vec::with_capacity(rows);
        for g in &groups {
            let Some(&(codec, count, dict_at, data_at, size)) = g.get(index) else {
                return fail("a row group is missing a column", name);
            };
            nested_chunk(
                data,
                codec_of(codec, name)?,
                count.max(0) as usize,
                if dict_at > 0 {
                    dict_at.min(data_at)
                } else {
                    data_at
                } as usize,
                size.max(0) as usize,
                leaf.max_def,
                name,
                &mut values,
            )?;
        }
        out.push(values);
    }
    Ok(out)
}

/// One column chunk of a nested string leaf, its values appended to `out`.
#[allow(clippy::too_many_arguments)]
fn nested_chunk(
    data: &[u8],
    codec: Codec,
    count: usize,
    start: usize,
    size: usize,
    max_def: u32,
    name: &str,
    out: &mut Vec<Option<String>>,
) -> Result<()> {
    let end = start.saturating_add(size).min(data.len());
    let mut at = start;
    let mut dictionary: Vec<String> = Vec::new();
    let mut done = 0usize;
    let width = level_width(max_def);
    while done < count && at < end {
        // PageHeader: type 1, uncompressed 2, compressed 3, data page 5,
        // dictionary page 7, data page v2 8.
        let mut t = Thrift::new(&data[..end], name);
        t.at = at;
        let (mut kind, mut raw, mut stored) = (0i64, 0i64, 0i64);
        let (mut n, mut encoding, mut def_len, mut rep_len, mut compressed) =
            (0i64, 0i64, 0i64, 0i64, true);
        let (mut id, mut last) = (0i16, 0i16);
        loop {
            let ty = t.field(&mut id, &mut last)?;
            if ty == T_STOP {
                break;
            }
            match id {
                1 => kind = t.zigzag()?,
                2 => raw = t.zigzag()?,
                3 => stored = t.zigzag()?,
                5 | 7 | 8 => {
                    let (mut hid, mut hlast) = (0i16, 0i16);
                    loop {
                        let ht = t.field(&mut hid, &mut hlast)?;
                        if ht == T_STOP {
                            break;
                        }
                        match (id, hid) {
                            (_, 1) => n = t.zigzag()?,
                            (5 | 7, 2) | (8, 4) => encoding = t.zigzag()?,
                            (8, 5) => def_len = t.zigzag()?,
                            (8, 6) => rep_len = t.zigzag()?,
                            (8, 7) => compressed = ht == T_TRUE,
                            _ => t.skip(ht)?,
                        }
                    }
                }
                _ => t.skip(ty)?,
            }
        }
        let body_at = t.at;
        let body_end = body_at.saturating_add(stored.max(0) as usize);
        if body_end > end {
            return fail("a parquet page runs past its column chunk", name);
        }
        let body = &data[body_at..body_end];
        at = body_end;
        match kind {
            2 => {
                let mut page = Vec::new();
                expand_append(codec, body, raw, &mut page, name)?;
                let mut p = 0usize;
                for _ in 0..n.max(0) {
                    let Some(len) = page.get(p..p + 4) else {
                        return fail("a dictionary page ends early", name);
                    };
                    let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
                    let Some(v) = page.get(p + 4..p + 4 + len) else {
                        return fail("a dictionary page ends early", name);
                    };
                    dictionary.push(String::from_utf8_lossy(v).into_owned());
                    p += 4 + len;
                }
            }
            0 | 3 => {
                let values = n.max(0) as usize;
                // The levels, then the values: v1 has all of it compressed and
                // the levels length-prefixed; v2 has the levels first,
                // uncompressed, with their lengths in the header.
                let (levels, rest): (Vec<i32>, Vec<u8>) = if kind == 0 {
                    let mut page = Vec::new();
                    expand_append(codec, body, raw, &mut page, name)?;
                    let mut levels = vec![max_def as i32; values];
                    let mut p = 0usize;
                    if max_def > 0 {
                        let Some(len) = page.get(0..4) else {
                            return fail("a data page ends early", name);
                        };
                        let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
                        let Some(run) = page.get(4..4 + len) else {
                            return fail("a data page ends early", name);
                        };
                        if !RleReader::new(run, width).fill(&mut levels) {
                            return fail("the definition levels of a page will not decode", name);
                        }
                        p = 4 + len;
                    }
                    (levels, page[p..].to_vec())
                } else {
                    let (rep_len, def_len) = (rep_len.max(0) as usize, def_len.max(0) as usize);
                    let Some(defs) = body.get(rep_len..rep_len + def_len) else {
                        return fail("a data page ends early", name);
                    };
                    let mut levels = vec![max_def as i32; values];
                    if max_def > 0 && !RleReader::new(defs, width).fill(&mut levels) {
                        return fail("the definition levels of a page will not decode", name);
                    }
                    let tail = &body[rep_len + def_len..];
                    let rest = if compressed {
                        let mut page = Vec::new();
                        expand_append(
                            codec,
                            tail,
                            raw - (rep_len + def_len) as i64,
                            &mut page,
                            name,
                        )?;
                        page
                    } else {
                        tail.to_vec()
                    };
                    (levels, rest)
                };
                let present = levels.iter().filter(|&&l| l == max_def as i32).count();
                let mut strings: Vec<Option<String>> = Vec::with_capacity(present);
                match encoding {
                    ENC_PLAIN => {
                        let mut p = 0usize;
                        for _ in 0..present {
                            let Some(len) = rest.get(p..p + 4) else {
                                return fail("a data page ends early", name);
                            };
                            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
                            let Some(v) = rest.get(p + 4..p + 4 + len) else {
                                return fail("a data page ends early", name);
                            };
                            strings.push(Some(String::from_utf8_lossy(v).into_owned()));
                            p += 4 + len;
                        }
                    }
                    ENC_PLAIN_DICTIONARY | ENC_RLE_DICTIONARY => {
                        if present > 0 {
                            let Some(&w) = rest.first() else {
                                return fail("a data page ends early", name);
                            };
                            let mut index = vec![0i32; present];
                            if !RleReader::new(&rest[1..], u32::from(w)).fill(&mut index) {
                                return fail(
                                    "the dictionary indices of a page will not decode",
                                    name,
                                );
                            }
                            for i in index {
                                let Some(v) = dictionary.get(i.max(0) as usize) else {
                                    return fail("a dictionary index past the dictionary", name);
                                };
                                strings.push(Some(v.clone()));
                            }
                        }
                    }
                    _ => return unsupported("this parquet string encoding is not read here", name),
                }
                let mut next = strings.into_iter();
                for l in levels {
                    out.push(if l == max_def as i32 {
                        next.next().flatten()
                    } else {
                        None
                    });
                }
                done += values;
            }
            _ => {}
        }
    }
    Ok(())
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

    /// Every value of a column as the bytes it reads as, `None` for a null,
    /// whichever form the reader kept it in.
    fn cells(data: &[u8], col: &Column) -> Vec<Option<Vec<u8>>> {
        if let Some(w) = &col.words {
            return w
                .values
                .iter()
                .enumerate()
                .map(|(r, v)| (!w.is_null(r)).then(|| v.to_vec()))
                .collect();
        }
        let base: &[u8] = if col.owned.is_empty() {
            data
        } else {
            &col.owned
        };
        let get =
            |s: Slice| (!s.is_null()).then(|| base[s.offset()..s.offset() + s.length()].to_vec());
        if col.dictionary {
            col.index
                .iter()
                .map(|&k| {
                    if k < 0 {
                        None
                    } else {
                        get(col.dict[k as usize])
                    }
                })
                .collect()
        } else {
            col.values.iter().map(|&s| get(s)).collect()
        }
    }

    fn fixtures() -> Vec<(String, Vec<u8>)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats");
        let mut out: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
            .map(|p| (p.display().to_string(), std::fs::read(&p).unwrap()))
            .collect();
        out.sort();
        out
    }

    /// Reads every column of every fixture in one pass and in two to five
    /// runs, and holds the runs to the pass: the same form, the same
    /// dictionary, the same value in every row.
    #[test]
    fn a_column_read_in_runs_is_the_column_read_in_one_pass() {
        let mut split = 0;
        for (name, data) in fixtures() {
            let Ok(fm) = read_file_meta(&data, &name) else {
                continue;
            };
            for which in 0..fm.names.len() {
                let col = &fm.names[which];
                let one = match read_column(&data, &fm, which, &name, 1) {
                    Ok(one) => one,
                    Err(e) => {
                        for ways in 2..=5 {
                            let many = read_column(&data, &fm, which, &name, ways);
                            assert!(many.is_err(), "{name} {col}: {e} in one pass only");
                        }
                        continue;
                    }
                };
                let want = cells(&data, &one);
                let ty = fm.groups[0].columns[which].ty;
                for ways in 2..=5 {
                    let many = read_column(&data, &fm, which, &name, ways).unwrap();
                    assert_eq!(many.dictionary, one.dictionary, "{name} {col} {ways} ways");
                    assert_eq!(cells(&data, &many), want, "{name} {col} {ways} ways");
                    let dict = |c: &Column| -> Vec<Vec<u8>> {
                        let base: &[u8] = if c.owned.is_empty() { &data } else { &c.owned };
                        c.dict
                            .iter()
                            .map(|s| base[s.offset()..s.offset() + s.length()].to_vec())
                            .collect()
                    };
                    assert_eq!(dict(&many), dict(&one), "{name} {col} {ways} ways");
                    // And it was the runs that read it, not the single pass
                    // standing in for them.
                    let Some(runs) = runs(&fm, which, ways) else {
                        continue;
                    };
                    let by_runs = if ty == TYPE_BYTE_ARRAY {
                        bytes_in_runs(&data, &fm, which, &runs, &name)
                    } else {
                        let width = match ty {
                            TYPE_BOOLEAN => 0,
                            TYPE_INT32 | TYPE_FLOAT => 4,
                            TYPE_FIXED_LEN_BYTE_ARRAY => fm.type_length[which],
                            _ => 8,
                        };
                        words_in_runs(&data, &fm, which, ty, width, &runs, &name)
                    };
                    let by_runs = by_runs.unwrap_or_else(|| panic!("{name} {col}: runs gave up"));
                    assert_eq!(cells(&data, &by_runs), want, "{name} {col} {ways} ways");
                    split += 1;
                }
            }
        }
        // The row-group fixtures alone have eleven columns read four ways.
        assert!(split >= 40, "only {split} columns were read in runs");
    }

    #[test]
    fn runs_cover_every_row_group_once_in_order() {
        for (name, data) in fixtures() {
            let fm = read_file_meta(&data, &name).unwrap();
            for ways in 2..=9 {
                let Some(runs) = runs(&fm, 0, ways) else {
                    continue;
                };
                assert!(runs.len() >= 2 && runs.len() <= ways.min(fm.groups.len()));
                assert_eq!(runs[0].groups.start, 0);
                assert_eq!(runs.last().unwrap().groups.end, fm.groups.len());
                for w in runs.windows(2) {
                    assert_eq!(w[0].groups.end, w[1].groups.start);
                }
                assert!(runs.iter().all(|r| !r.groups.is_empty()));
                assert_eq!(runs.iter().map(|r| r.rows).sum::<usize>(), fm.rows as usize);
            }
        }
    }

    /// A footer that undersells its chunks, or whose counts do not add up,
    /// leaves the reading to the single pass, which reads it as before.
    #[test]
    fn a_footer_the_runs_cannot_trust_is_read_in_one_pass() {
        for file in ["rg_snappy.parquet", "rg_zstd.parquet", "rg_none.parquet"] {
            let (name, data) = fixtures()
                .into_iter()
                .find(|(n, _)| n.ends_with(file))
                .unwrap();
            let fm = read_file_meta(&data, &name).unwrap();
            for which in 0..fm.names.len() {
                let want = cells(&data, &read_column(&data, &fm, which, &name, 1).unwrap());

                let mut small = read_file_meta(&data, &name).unwrap();
                for g in small.groups.iter_mut() {
                    g.columns[which].total_uncompressed_size = 1;
                }
                if file != "rg_none.parquet" && fm.groups[0].columns[which].ty == TYPE_BYTE_ARRAY {
                    let runs = runs(&small, which, 4).unwrap();
                    assert!(bytes_in_runs(&data, &small, which, &runs, &name).is_none());
                }
                let got = read_column(&data, &small, which, &name, 4).unwrap();
                assert_eq!(cells(&data, &got), want, "{name} undersold");

                let mut off = read_file_meta(&data, &name).unwrap();
                off.groups[1].columns[which].num_values += 1;
                assert!(runs(&off, which, 4).is_none());
            }
        }
    }

    #[test]
    fn or_bits_lands_each_bit_on_its_row() {
        let mut seed = 7u64;
        for at in [0usize, 1, 63, 64, 65, 100, 128, 191] {
            for n in [1usize, 5, 64, 65, 130] {
                let rows = at + n + 3;
                let mut src = vec![0u64; n.div_ceil(64)];
                let mut want = vec![false; rows];
                for r in 0..n {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    if seed >> 62 == 0 {
                        src[r / 64] |= 1 << (r % 64);
                        want[at + r] = true;
                    }
                }
                let mut dst = vec![0u64; rows.div_ceil(64)];
                or_bits(&mut dst, at, &src);
                let got: Vec<bool> = (0..rows)
                    .map(|r| dst[r / 64] >> (r % 64) & 1 == 1)
                    .collect();
                assert_eq!(got, want, "at {at} n {n}");
            }
        }
    }

    #[test]
    fn snappy_refuses_a_copy_from_before_the_page() {
        let mut out = b"prefix".to_vec();
        assert!(!snappy_append(&stream(b"ab", 3, 4), &mut out).unwrap());
    }

    /// The format read one byte at a time: `None` where [`snappy_append`] must
    /// say `Ok(false)`.
    fn reference(input: &[u8]) -> Option<Vec<u8>> {
        let (mut want, mut shift, mut at) = (0u64, 0, 0);
        loop {
            let b = *input.get(at)?;
            at += 1;
            want |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
        let mut v: Vec<u8> = Vec::new();
        let byte = |at: usize| input.get(at).map(|&b| b as usize);
        while at < input.len() {
            let tag = input[at];
            at += 1;
            let (len, offset) = match tag & 3 {
                0 => {
                    let mut len = (tag >> 2) as usize + 1;
                    if len > 60 {
                        let mut x = 0;
                        for i in 0..len - 60 {
                            x |= byte(at + i)? << (8 * i);
                        }
                        at += len - 60;
                        len = x + 1;
                    }
                    if v.len() + len > want as usize {
                        return None;
                    }
                    v.extend_from_slice(input.get(at..at.checked_add(len)?)?);
                    at += len;
                    continue;
                }
                1 => {
                    let o = (((tag >> 5) as usize) << 8) | byte(at)?;
                    at += 1;
                    (((tag >> 2) & 7) as usize + 4, o)
                }
                2 => {
                    let o = byte(at)? | byte(at + 1)? << 8;
                    at += 2;
                    ((tag >> 2) as usize + 1, o)
                }
                _ => {
                    let mut o = 0;
                    for i in 0..4 {
                        o |= byte(at + i)? << (8 * i);
                    }
                    at += 4;
                    ((tag >> 2) as usize + 1, o)
                }
            };
            if offset == 0 || offset > v.len() || v.len() + len > want as usize {
                return None;
            }
            for _ in 0..len {
                v.push(v[v.len() - offset]);
            }
        }
        (v.len() as u64 == want).then_some(v)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A well-formed stream of every element form: literals short and with one
    /// to four length bytes, and copies with one-, two- and four-byte offsets,
    /// many of them short, close and overlapping, the cases the word-wide
    /// copies cut corners on. Returns the stream and what it expands to.
    fn random_stream(rng: &mut Rng, elements: usize) -> (Vec<u8>, Vec<u8>) {
        let mut body = Vec::new();
        let mut plain: Vec<u8> = Vec::new();
        for _ in 0..elements {
            let pick = rng.below(10);
            if plain.is_empty() || pick < 4 {
                let len = match rng.below(4) {
                    0 => 1 + rng.below(16),
                    1 => 1 + rng.below(60),
                    2 => 61 + rng.below(300),
                    _ => 1 + rng.below(3),
                };
                if len <= 60 {
                    body.push(((len - 1) << 2) as u8);
                } else {
                    // A length form is free to use more bytes than it needs.
                    let extra = 1 + rng.below(4).max(((len - 1) > 255) as usize);
                    body.push(((59 + extra) << 2) as u8);
                    for i in 0..extra {
                        body.push(((len - 1) >> (8 * i)) as u8);
                    }
                }
                let alphabet = 1 + rng.below(4);
                for _ in 0..len {
                    let b = b'a' + rng.below(alphabet) as u8;
                    body.push(b);
                    plain.push(b);
                }
                continue;
            }
            let offset = match rng.below(4) {
                0 => 1 + rng.below(plain.len().min(8)),
                1 => 1 + rng.below(plain.len().min(20)),
                _ => 1 + rng.below(plain.len()),
            };
            let form = rng.below(3);
            let len = match form {
                0 if offset < 2048 => 4 + rng.below(8),
                _ => {
                    let reach = if rng.below(2) == 0 { 20 } else { 64 };
                    1 + rng.below(reach)
                }
            };
            if form == 0 && offset < 2048 {
                body.push((((offset >> 8) << 5) | ((len - 4) << 2) | 1) as u8);
                body.push(offset as u8);
            } else if form != 2 && offset < 65536 {
                body.push((((len - 1) << 2) | 2) as u8);
                body.extend_from_slice(&(offset as u16).to_le_bytes());
            } else {
                body.push((((len - 1) << 2) | 3) as u8);
                body.extend_from_slice(&(offset as u32).to_le_bytes());
            }
            for _ in 0..len {
                plain.push(plain[plain.len() - offset]);
            }
        }
        let mut s = Vec::new();
        let mut n = plain.len();
        while n >= 0x80 {
            s.push((n as u8) | 0x80);
            n >>= 7;
        }
        s.push(n as u8);
        s.extend(body);
        (s, plain)
    }

    #[test]
    fn snappy_expands_random_streams_of_every_element_form() {
        let mut rng = Rng(0x5eed);
        for round in 0..3000 {
            let elements = 1 + rng.below(80);
            let (stream, plain) = random_stream(&mut rng, elements);
            assert_eq!(reference(&stream).as_deref(), Some(&plain[..]));
            // Pages land one after another in a column's buffer: what one
            // spills past its end must not show in the next.
            let mut out = b"previous page".to_vec();
            assert!(snappy_append(&stream, &mut out).unwrap(), "round {round}");
            assert!(snappy_append(&stream, &mut out).unwrap(), "round {round}");
            let mut want = b"previous page".to_vec();
            want.extend_from_slice(&plain);
            want.extend_from_slice(&plain);
            assert_eq!(out, want, "round {round}");
        }
    }

    #[test]
    fn snappy_agrees_with_the_reference_on_damaged_streams() {
        let mut rng = Rng(0xbad5eed);
        for round in 0..20000 {
            let elements = 1 + rng.below(30);
            let (mut stream, _) = random_stream(&mut rng, elements);
            // The length prefix is left alone: a damaged one asks for a page of
            // any size, and the allocation, not the decoder, answers that.
            let head = stream.iter().position(|&b| b & 0x80 == 0).unwrap() + 1;
            match rng.below(3) {
                0 => stream.truncate(head + rng.below(stream.len() - head + 1)),
                1 => {
                    for _ in 0..1 + rng.below(3) {
                        if stream.len() > head {
                            let i = head + rng.below(stream.len() - head);
                            stream[i] = rng.next() as u8;
                        }
                    }
                }
                _ => {
                    let i = head + rng.below(stream.len() - head + 1);
                    stream.insert(i, rng.next() as u8);
                }
            }
            let mut out = b"x".to_vec();
            let got = snappy_append(&stream, &mut out).unwrap();
            match reference(&stream) {
                Some(plain) => {
                    assert!(got, "round {round}");
                    assert_eq!(&out[1..], &plain[..], "round {round}");
                }
                None => assert!(!got, "round {round}"),
            }
        }
    }
}
