//! A Parquet writer for the benchmark payloads.
//!
//! The generator's job is to produce the same twenty columns in whichever format
//! the run needs, and to do it at disk speed. Going through DuckDB or pyarrow for
//! the Parquet half would make the payload depend on a Python install and on
//! whatever those writers decide to do this release; writing it here keeps the
//! generator one binary and the file one shape we control.
//!
//! It writes the subset the comparison actually needs: every column a required
//! UTF-8 byte array, one dictionary page and one data page per column chunk,
//! optionally zstd-compressed. Dictionary encoding is chosen per column chunk,
//! by counting distinct values while the row group fills — which is exactly the
//! decision that makes `status` cost a few bytes a row and `txn_id` cost its
//! full width.
//!
//! What it does not write: definition levels (nothing is null — a blanked cell is
//! an empty string, as it is in the CSV), statistics, bloom filters, or page
//! indexes. A reader that needs those is reading a file this was not asked to
//! make.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::error::Result;

const MAGIC: &[u8; 4] = b"PAR1";

/// Above this many distinct values in one column chunk, the dictionary stops
/// paying for itself and the chunk is written plain instead.
pub const DICTIONARY_LIMIT: usize = 1 << 17;

/// How a page's bytes are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd,
}

impl Compression {
    fn id(self) -> i32 {
        match self {
            Compression::None => 0,
            Compression::Zstd => 6,
        }
    }

    fn apply(self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Compression::None => bytes.to_vec(),
            Compression::Zstd => zstd::stream::encode_all(bytes, 1)?,
        })
    }
}

/// One column's values within the row group being filled: laid end to end, with
/// where each one stops.
#[derive(Default)]
struct Column {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl Column {
    fn push(&mut self, value: &str) {
        self.bytes.extend_from_slice(value.as_bytes());
        self.ends.push(self.bytes.len() as u32);
    }

    fn value(&self, i: usize) -> &[u8] {
        let from = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[from..self.ends[i] as usize]
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }
}

/// What the footer has to say about one column chunk once it is written.
struct ChunkMeta {
    dictionary_page_offset: Option<u64>,
    data_page_offset: u64,
    dictionary: bool,
    uncompressed: u64,
    compressed: u64,
    values: i64,
}

struct RowGroupMeta {
    rows: i64,
    chunks: Vec<ChunkMeta>,
}

pub struct Writer {
    out: BufWriter<File>,
    at: u64,
    names: Vec<String>,
    columns: Vec<Column>,
    compression: Compression,
    rows_per_group: usize,
    dictionary_limit: usize,
    rows_in_group: usize,
    total_rows: i64,
    groups: Vec<RowGroupMeta>,
}

impl Writer {
    pub fn create(
        path: &Path,
        names: &[&str],
        compression: Compression,
        rows_per_group: usize,
    ) -> Result<Writer> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        out.write_all(MAGIC)?;
        Ok(Writer {
            out,
            at: MAGIC.len() as u64,
            names: names.iter().map(|n| (*n).to_string()).collect(),
            columns: (0..names.len()).map(|_| Column::default()).collect(),
            compression,
            rows_per_group: rows_per_group.max(1),
            dictionary_limit: DICTIONARY_LIMIT,
            rows_in_group: 0,
            total_rows: 0,
            groups: Vec::new(),
        })
    }

    /// Distinct values a column chunk may hold before it is written plain
    /// instead of dictionary-encoded (default 131,072).
    pub fn with_dictionary_limit(mut self, limit: usize) -> Writer {
        self.dictionary_limit = limit;
        self
    }

    /// One row, as one value per column in schema order.
    pub fn write_row<'a>(&mut self, cells: impl Iterator<Item = &'a str>) -> Result<()> {
        for (column, value) in self.columns.iter_mut().zip(cells) {
            column.push(value);
        }
        self.rows_in_group += 1;
        self.total_rows += 1;
        if self.rows_in_group >= self.rows_per_group {
            self.flush_group()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.flush_group()?;
        let footer = self.footer();
        self.out.write_all(&footer)?;
        self.out.write_all(&(footer.len() as u32).to_le_bytes())?;
        self.out.write_all(MAGIC)?;
        self.out.flush()?;
        Ok(())
    }

    fn flush_group(&mut self) -> Result<()> {
        let rows = self.rows_in_group;
        if rows == 0 {
            return Ok(());
        }
        // Taken out so each column can be read while the file is written to.
        let mut columns = std::mem::take(&mut self.columns);
        let mut chunks = Vec::with_capacity(columns.len());
        for column in &columns {
            chunks.push(self.write_chunk(column, rows)?);
        }
        for column in &mut columns {
            column.clear();
        }
        self.columns = columns;
        self.groups.push(RowGroupMeta {
            rows: rows as i64,
            chunks,
        });
        self.rows_in_group = 0;
        Ok(())
    }

    /// Writes one column chunk, dictionary-encoded where that is smaller.
    fn write_chunk(&mut self, column: &Column, rows: usize) -> Result<ChunkMeta> {
        // The dictionary is built by walking the values once. It is abandoned as
        // soon as it grows past the limit, so a key column costs one pass and no
        // more, and a low-cardinality column costs a few bytes a row.
        let mut index: HashMap<&[u8], u32> = HashMap::new();
        let mut order: Vec<&[u8]> = Vec::new();
        let mut codes: Vec<u32> = Vec::with_capacity(rows);
        let mut dictionary = true;
        let limit = self.dictionary_limit;
        for i in 0..rows {
            let value = column.value(i);
            let next = index.len() as u32;
            match index.get(value) {
                Some(&code) => codes.push(code),
                None => {
                    if index.len() >= limit {
                        dictionary = false;
                        break;
                    }
                    index.insert(value, next);
                    order.push(value);
                    codes.push(next);
                }
            }
        }

        let mut uncompressed = 0u64;
        let mut compressed = 0u64;
        let mut dictionary_page_offset = None;

        if dictionary {
            let mut page = Vec::new();
            for value in &order {
                page.extend_from_slice(&(value.len() as u32).to_le_bytes());
                page.extend_from_slice(value);
            }
            dictionary_page_offset = Some(self.at);
            let (u, c) = self.write_page(&page, PageKind::Dictionary(order.len() as i32))?;
            uncompressed += u;
            compressed += c;
        }

        let data_page_offset = self.at;
        let page = if dictionary {
            let width = bit_width(order.len().saturating_sub(1) as u32);
            let mut body = vec![width];
            pack_bits(&codes, width, &mut body);
            body
        } else {
            let mut body = Vec::with_capacity(column.bytes.len() + rows * 4);
            for i in 0..rows {
                let value = column.value(i);
                body.extend_from_slice(&(value.len() as u32).to_le_bytes());
                body.extend_from_slice(value);
            }
            body
        };
        let (u, c) = self.write_page(&page, PageKind::Data(rows as i32, dictionary))?;
        uncompressed += u;
        compressed += c;

        Ok(ChunkMeta {
            dictionary_page_offset,
            data_page_offset,
            dictionary,
            uncompressed,
            compressed,
            values: rows as i64,
        })
    }

    /// Writes one page header and its body, returning what it cost.
    fn write_page(&mut self, body: &[u8], kind: PageKind) -> Result<(u64, u64)> {
        let squeezed = self.compression.apply(body)?;
        let mut header = Thrift::new();
        header.i32_field(1, kind.type_id());
        header.i32_field(2, body.len() as i32);
        header.i32_field(3, squeezed.len() as i32);
        match kind {
            PageKind::Data(values, dictionary) => {
                header.struct_field(5, |h| {
                    h.i32_field(1, values);
                    // 8 is RLE_DICTIONARY, 0 is PLAIN; the level encodings are
                    // named even though a required column writes no levels.
                    h.i32_field(2, if dictionary { 8 } else { 0 });
                    h.i32_field(3, 3);
                    h.i32_field(4, 3);
                });
            }
            PageKind::Dictionary(values) => {
                header.struct_field(7, |h| {
                    h.i32_field(1, values);
                    h.i32_field(2, 0); // PLAIN
                    h.bool_field(3, false);
                });
            }
        }
        let header = header.finish();
        self.out.write_all(&header)?;
        self.out.write_all(&squeezed)?;
        self.at += (header.len() + squeezed.len()) as u64;
        Ok((
            (header.len() + body.len()) as u64,
            (header.len() + squeezed.len()) as u64,
        ))
    }

    fn footer(&self) -> Vec<u8> {
        let mut t = Thrift::new();
        t.i32_field(1, 1); // version
        t.list_field(2, self.names.len() + 1, KIND_STRUCT, |t| {
            // The root, then one leaf per column: required UTF-8 byte arrays.
            t.write_struct(|t| {
                t.binary_field(4, b"compara");
                t.i32_field(5, self.names.len() as i32);
            });
            for name in &self.names {
                t.write_struct(|t| {
                    t.i32_field(1, 6); // BYTE_ARRAY
                    t.i32_field(3, 0); // REQUIRED
                    t.binary_field(4, name.as_bytes());
                    t.i32_field(6, 0); // UTF8
                    t.struct_field(10, |t| {
                        t.struct_field(1, |_| {}); // logicalType: STRING
                    });
                });
            }
        });
        t.i64_field(3, self.total_rows);
        t.list_field(4, self.groups.len(), KIND_STRUCT, |t| {
            for group in &self.groups {
                t.write_struct(|t| {
                    t.list_field(1, group.chunks.len(), KIND_STRUCT, |t| {
                        for (i, chunk) in group.chunks.iter().enumerate() {
                            t.write_struct(|t| {
                                t.i64_field(
                                    2,
                                    chunk
                                        .dictionary_page_offset
                                        .unwrap_or(chunk.data_page_offset)
                                        as i64,
                                );
                                t.struct_field(3, |t| {
                                    t.i32_field(1, 6); // BYTE_ARRAY
                                    let encodings: &[i32] =
                                        if chunk.dictionary { &[0, 8] } else { &[0] };
                                    t.list_field(2, encodings.len(), KIND_I32, |t| {
                                        for e in encodings {
                                            t.i32_value(*e);
                                        }
                                    });
                                    t.list_field(3, 1, KIND_BINARY, |t| {
                                        t.binary_value(self.names[i].as_bytes());
                                    });
                                    t.i32_field(4, self.compression.id());
                                    t.i64_field(5, chunk.values);
                                    t.i64_field(6, chunk.uncompressed as i64);
                                    t.i64_field(7, chunk.compressed as i64);
                                    t.i64_field(9, chunk.data_page_offset as i64);
                                    if let Some(at) = chunk.dictionary_page_offset {
                                        t.i64_field(11, at as i64);
                                    }
                                });
                            });
                        }
                    });
                    let size: u64 = group.chunks.iter().map(|c| c.compressed).sum();
                    t.i64_field(2, size as i64);
                    t.i64_field(3, group.rows);
                });
            }
        });
        t.binary_field(6, b"compara gen-data");
        t.finish()
    }
}

enum PageKind {
    /// A data page and whether it is dictionary-encoded.
    Data(i32, bool),
    Dictionary(i32),
}

impl PageKind {
    fn type_id(&self) -> i32 {
        match self {
            PageKind::Data(..) => 0,
            PageKind::Dictionary(_) => 2,
        }
    }
}

/// The bits needed to hold every value up to `max`.
fn bit_width(max: u32) -> u8 {
    (32 - max.leading_zeros()) as u8
}

/// Packs dictionary indices as one bit-packed run: least significant bit first,
/// in groups of eight, which is the layout every Parquet reader expects.
fn pack_bits(values: &[u32], width: u8, out: &mut Vec<u8>) {
    let groups = values.len().div_ceil(8);
    let mut header = ((groups as u64) << 1) | 1;
    loop {
        let byte = (header & 0x7f) as u8;
        header >>= 7;
        if header == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    if width == 0 {
        return;
    }
    let mut buffer = 0u64;
    let mut bits = 0u32;
    for i in 0..groups * 8 {
        let value = values.get(i).copied().unwrap_or(0) as u64;
        buffer |= value << bits;
        bits += width as u32;
        while bits >= 8 {
            out.push((buffer & 0xff) as u8);
            buffer >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push((buffer & 0xff) as u8);
    }
}

// ---------------------------------------------------------------------------
// Thrift's compact protocol, writing side
// ---------------------------------------------------------------------------

const KIND_I32: u8 = 5;
const KIND_BINARY: u8 = 8;
const KIND_STRUCT: u8 = 12;

/// The mirror of the reader's compact-protocol decoder, and no larger: field
/// headers carry the delta from the last id, integers are zigzag varints, and a
/// struct ends with a zero byte.
struct Thrift {
    out: Vec<u8>,
    last_id: i16,
    stack: Vec<i16>,
}

impl Thrift {
    fn new() -> Thrift {
        Thrift {
            out: Vec::new(),
            last_id: 0,
            stack: Vec::new(),
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.out.push(0); // the outermost struct's stop byte
        self.out
    }

    fn varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.out.push(byte);
                return;
            }
            self.out.push(byte | 0x80);
        }
    }

    fn zigzag(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    fn header(&mut self, id: i16, kind: u8) {
        let delta = id - self.last_id;
        if delta > 0 && delta <= 15 {
            self.out.push(((delta as u8) << 4) | kind);
        } else {
            self.out.push(kind);
            self.zigzag(id as i64);
        }
        self.last_id = id;
    }

    fn i32_field(&mut self, id: i16, value: i32) {
        self.header(id, KIND_I32);
        self.zigzag(value as i64);
    }

    fn i64_field(&mut self, id: i16, value: i64) {
        self.header(id, 6);
        self.zigzag(value);
    }

    fn bool_field(&mut self, id: i16, value: bool) {
        self.header(id, if value { 1 } else { 2 });
    }

    fn i32_value(&mut self, value: i32) {
        self.zigzag(value as i64);
    }

    fn binary_value(&mut self, bytes: &[u8]) {
        self.varint(bytes.len() as u64);
        self.out.extend_from_slice(bytes);
    }

    fn binary_field(&mut self, id: i16, bytes: &[u8]) {
        self.header(id, KIND_BINARY);
        self.binary_value(bytes);
    }

    /// A nested struct. Field ids inside are relative to it, so the enclosing
    /// struct's last id is stacked over the call.
    fn write_struct(&mut self, body: impl FnOnce(&mut Thrift)) {
        self.stack.push(self.last_id);
        self.last_id = 0;
        body(self);
        self.out.push(0);
        self.last_id = self.stack.pop().unwrap_or(0);
    }

    fn struct_field(&mut self, id: i16, body: impl FnOnce(&mut Thrift)) {
        self.header(id, KIND_STRUCT);
        self.write_struct(body);
    }

    fn list_field(&mut self, id: i16, len: usize, element: u8, body: impl FnOnce(&mut Thrift)) {
        self.header(id, 9);
        if len < 15 {
            self.out.push(((len as u8) << 4) | element);
        } else {
            self.out.push(0xf0 | element);
            self.varint(len as u64);
        }
        body(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_pack_least_significant_bit_first() {
        let mut out = Vec::new();
        pack_bits(&[0, 1, 2, 3, 4, 5, 6, 7], 3, &mut out);
        assert_eq!(out, vec![0x03, 0b1000_1000, 0b1100_0110, 0b1111_1010]);
    }

    #[test]
    fn a_short_group_is_padded_rather_than_truncated() {
        let mut out = Vec::new();
        pack_bits(&[1, 1, 1], 1, &mut out);
        // One group of eight: three ones and five zeroes of padding.
        assert_eq!(out, vec![0x03, 0b0000_0111]);
    }

    #[test]
    fn field_headers_carry_the_delta_from_the_last_id() {
        let mut t = Thrift::new();
        t.i32_field(1, 7);
        t.binary_field(2, b"hi");
        assert_eq!(t.finish(), vec![0x15, 0x0e, 0x18, 0x02, b'h', b'i', 0x00]);
    }
}
