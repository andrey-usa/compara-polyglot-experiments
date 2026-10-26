//! Reading Parquet without giving up the representation the engine is built on.
//!
//! Everything else this engine reads is a row of contiguous bytes, so a field is
//! an offset and a length and nothing is copied. Parquet is columnar and has no
//! row at all: the values of one row live in as many column chunks as there are
//! columns, page-compressed, and often as indices into a dictionary. So the
//! pages are decoded once, into an arena, and a field becomes an offset and a
//! length *into that arena* — the same word the CSV and JSON readers produce,
//! and the same join over it.
//!
//! The one thing worth doing well is the dictionary. Where a column is
//! dictionary-encoded — which is most string columns in most files — the
//! dictionary is decoded once and every row's field points *at the dictionary
//! entry*, so a column of ten million rows over a few thousand distinct values
//! costs eight bytes a row and nothing more. That is what makes this reader
//! smaller in memory than reading the equivalent CSV rather than larger.
//!
//! Columns are decoded in parallel, since they share nothing until they are
//! stitched into rows.
//!
//! **How a value becomes text.** This tool compares text — `1.0` and `1` are
//! different unless a tolerance is set — so a typed Parquet value has to be
//! rendered, and the rule is fixed rather than clever: byte arrays are their
//! bytes, booleans are `true` and `false`, integers and decimals are written out
//! in full, floats take their shortest round-trip form, dates are `YYYY-MM-DD`
//! and timestamps ISO 8601. A Parquet file compares against a CSV of the same
//! data exactly when the CSV spells its values that way.

use std::path::Path;

use super::codec::{Codec, decompress};
use super::encoding::{Hybrid, bit_width, delta_binary_packed};
use super::field::{ABSENT, Field, is_real, pack};
use super::slab::Slab;
use super::thrift::{self, kind};
use crate::alloc;
use crate::columns::Typed;
use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"PAR1";

// ---------------------------------------------------------------------------
// What the footer says
// ---------------------------------------------------------------------------

/// A physical type, as Parquet stores the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Physical {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    FixedLen,
}

impl Physical {
    fn from_id(id: i32) -> Result<Physical> {
        Ok(match id {
            0 => Physical::Boolean,
            1 => Physical::Int32,
            2 => Physical::Int64,
            3 => Physical::Int96,
            4 => Physical::Float,
            5 => Physical::Double,
            6 => Physical::ByteArray,
            7 => Physical::FixedLen,
            other => {
                return Err(Error::new(format!(
                    "this Parquet file has a column of physical type {other}, which does not exist"
                )));
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Millis,
    Micros,
    Nanos,
}

/// How a value of this column is written out as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Render {
    /// The bytes are the value: a UTF-8 string, or anything else the file calls
    /// a byte array.
    Text,
    Bool,
    Int {
        signed: bool,
    },
    Float,
    Double,
    Decimal {
        scale: i32,
    },
    Date,
    Time {
        unit: Unit,
    },
    Timestamp {
        unit: Unit,
        utc: bool,
    },
}

/// One leaf column of the schema.
struct ColumnInfo {
    name: String,
    physical: Physical,
    render: Render,
    /// Bytes per value for `FIXED_LEN_BYTE_ARRAY`.
    type_length: usize,
    /// One when the column is optional, so a definition level below it is a null.
    max_def_level: u8,
}

/// One column's bytes within one row group.
#[derive(Clone)]
struct Chunk {
    codec: Codec,
    num_values: i64,
    data_page_offset: u64,
    dictionary_page_offset: Option<u64>,
    total_compressed_size: i64,
}

struct RowGroup {
    num_rows: usize,
    chunks: Vec<Chunk>,
}

/// An open Parquet file: mapped, with its footer read.
pub(super) struct Reader {
    slab: Slab,
    columns: Vec<ColumnInfo>,
    row_groups: Vec<RowGroup>,
    rows: usize,
}

/// Whether a file is Parquet, by its magic rather than by its name. A file
/// called `.parquet` that is really a CSV should be read as a CSV, and one
/// called anything else that is really Parquet should still work.
pub(super) fn looks_like_parquet(data: &[u8]) -> bool {
    data.len() >= 12 && &data[..4] == MAGIC && &data[data.len() - 4..] == MAGIC
}

/// What one column holds, as the columnar path (`pqdiff`) needs to know it: how
/// its values are stored and how each reads as text. A column of the same kind
/// in both files can be compared value by value, without rendering a row, and
/// [`ValueKind::write`] renders the cells a report shows by this reader's rule
/// -- so the columnar path and this one give the same answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValueKind {
    physical: Physical,
    render: Render,
    type_length: usize,
}

/// Every leaf column's kind, in schema order. `data` is the whole file.
pub(crate) fn value_kinds(data: &[u8]) -> Result<Vec<ValueKind>> {
    if !looks_like_parquet(data) {
        return Err(Error::new("not a Parquet file"));
    }
    let end = data.len();
    let footer_len =
        u32::from_le_bytes(data[end - 8..end - 4].try_into().expect("four bytes")) as usize;
    let from = end
        .checked_sub(8 + footer_len)
        .ok_or_else(|| Error::new("the Parquet footer is longer than the file"))?;
    let (columns, _, _) = read_metadata(&data[from..end - 8])?;
    Ok(columns
        .iter()
        .map(|c| ValueKind {
            physical: c.physical,
            render: c.render,
            type_length: c.type_length,
        })
        .collect())
}

impl ValueKind {
    /// A byte array read as text: the value is its bytes.
    pub(crate) fn is_text(self) -> bool {
        self.physical == Physical::ByteArray && self.render == Render::Text
    }

    /// Whether the value fits a word: a boolean, an integer, a float, or a
    /// decimal of up to eight bytes. These are what the columnar path keeps as
    /// one `u64` per row.
    pub(crate) fn is_word(self) -> bool {
        match self.physical {
            Physical::Boolean
            | Physical::Int32
            | Physical::Int64
            | Physical::Float
            | Physical::Double => true,
            Physical::FixedLen => {
                matches!(self.render, Render::Decimal { .. }) && (1..=8).contains(&self.type_length)
            }
            Physical::Int96 | Physical::ByteArray => false,
        }
    }

    /// Writes a word-sized value out as text, as [`plain`] would have. `raw` is
    /// what the columnar reader keeps: an INT32 sign-extended, an INT64 as it
    /// is, a float as its bits, a boolean as 0 or 1, a fixed-length decimal as
    /// its unscaled value sign-extended.
    pub(crate) fn write(self, raw: u64, out: &mut Vec<u8>) {
        match self.physical {
            Physical::Boolean => out.extend_from_slice(if raw != 0 { b"true" } else { b"false" }),
            Physical::Int32 => {
                let v = raw as i64 as i32;
                match self.render {
                    Render::Date => write_date(out, v as i64),
                    Render::Int { signed: false } => {
                        write_number(out, (v as u32) as i64, self.render)
                    }
                    _ => write_number(out, v as i64, self.render),
                }
            }
            Physical::Int64 | Physical::FixedLen => write_number(out, raw as i64, self.render),
            Physical::Float => write_f32(out, f32::from_bits(raw as u32)),
            Physical::Double => write_f64(out, f64::from_bits(raw)),
            Physical::Int96 | Physical::ByteArray => {}
        }
    }
}

impl Reader {
    pub(super) fn open(path: &Path) -> Result<Reader> {
        let slab = Slab::map(path)?;
        let data = slab.data();
        if !looks_like_parquet(data) {
            return Err(Error::new(format!(
                "{} does not begin and end with a Parquet magic number",
                path.display()
            )));
        }
        let end = data.len();
        let footer_len =
            u32::from_le_bytes(data[end - 8..end - 4].try_into().expect("four bytes")) as usize;
        let from = end.checked_sub(8 + footer_len).ok_or_else(|| {
            Error::new(format!(
                "{}'s footer is longer than the file",
                path.display()
            ))
        })?;
        let (columns, row_groups, rows) = read_metadata(&data[from..end - 8])?;
        Ok(Reader {
            slab,
            columns,
            row_groups,
            rows,
        })
    }

    /// The column names, in schema order: this file's header.
    pub(super) fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    pub(super) fn rows(&self) -> usize {
        self.rows
    }

    /// What the column holds, for comparing it with another file's text: its
    /// typed values are rendered in one canonical spelling (see the module
    /// notes), so only their value can be compared.
    pub(super) fn typed(&self, name: &str) -> Typed {
        match self
            .columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.render)
        {
            Some(Render::Int { .. } | Render::Float | Render::Double | Render::Decimal { .. }) => {
                Typed::Number
            }
            Some(Render::Date | Render::Time { .. } | Render::Timestamp { .. }) => Typed::Instant,
            _ => Typed::Text,
        }
    }

    /// Decodes the wanted columns into row-major field words and the arena they
    /// point into. `wanted[i]` is the name whose values belong in slot `i`; a name
    /// this file does not have leaves that slot absent in every row.
    ///
    /// Columns are decoded a wave at a time — as many at once as there are
    /// threads — and each wave is folded into the output before the next starts.
    /// Decoding all of them first and stitching afterwards holds two copies of
    /// every field word at the peak, which at ten million rows over nineteen
    /// columns is 1.5 GB per file for nothing.
    ///
    /// `limit` caps the rows decoded, for a preview that must not pay for the
    /// file: pass `usize::MAX` for the whole thing. Decoding stops on a page
    /// boundary at or past the limit, so a ten-row preview of a four-gigabyte
    /// file reads one page per column rather than all of them.
    pub(super) fn project(
        &self,
        wanted: &[Option<&str>],
        threads: usize,
        limit: usize,
    ) -> Result<(Vec<Field>, Vec<u8>)> {
        let width = wanted.len();
        let rows = self.rows.min(limit);
        let jobs: Vec<(usize, usize)> = wanted
            .iter()
            .enumerate()
            .filter_map(|(slot, name)| {
                let name = (*name)?;
                let leaf = self.columns.iter().position(|c| c.name == name)?;
                Some((slot, leaf))
            })
            .collect();

        let mut fields = crate::alloc::filled(ABSENT, rows * width, "one field per cell")?;
        let mut arena: Vec<u8> = Vec::new();
        let ways = threads.clamp(1, jobs.len().max(1));

        for wave in jobs.chunks(ways) {
            let mut decoded: Vec<Result<Decoded>> = Vec::with_capacity(wave.len());
            let one = |i: usize| self.decode_column(wave[i].1, limit);
            std::thread::scope(|scope| {
                let handles: Vec<_> = (1..wave.len())
                    .map(|i| crate::parallel::spawn_at(scope, &one, i))
                    .collect();
                decoded.push(one(0));
                for handle in handles {
                    decoded.push(
                        handle.join().unwrap_or_else(|_| {
                            Err(Error::new("a Parquet column decoder panicked"))
                        }),
                    );
                }
            });

            for (&(slot, _), column) in wave.iter().zip(decoded) {
                let column = column?;
                // The offset is the low bits of the word, so moving a column's
                // bytes into the file's one arena is an addition — and only the
                // two sentinels have to be left alone.
                let base = arena.len() as u64;
                arena.extend_from_slice(&column.arena);
                // Written a block of rows at a time: the destination is
                // row-major, so a whole column in one pass would touch every
                // cache line of the output once per column.
                const BLOCK: usize = 1024;
                // A limited decode stops on a page boundary, so a column can
                // hold fewer rows than `rows` only if the file itself does.
                let have = rows.min(column.fields.len());
                for from in (0..have).step_by(BLOCK) {
                    let to = (from + BLOCK).min(have);
                    for row in from..to {
                        let f = column.fields[row];
                        fields[row * width + slot] = if is_real(f) { f + base } else { f };
                    }
                }
            }
        }
        Ok((fields, arena))
    }
}

/// One column's values: a field per row, and the bytes they point into.
struct Decoded {
    fields: Vec<Field>,
    arena: Vec<u8>,
}

// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

fn read_metadata(data: &[u8]) -> Result<(Vec<ColumnInfo>, Vec<RowGroup>, usize)> {
    let mut r = thrift::Reader::new(data);
    let mut schema: Vec<SchemaElement> = Vec::new();
    let mut row_groups: Vec<RowGroup> = Vec::new();
    let mut rows = 0usize;

    r.read_struct(|r, f| match f.id {
        2 => {
            r.read_list(|r| {
                schema.push(read_schema_element(r)?);
                Ok(())
            })?;
            Ok(true)
        }
        3 => {
            rows = r.i64()?.max(0) as usize;
            Ok(true)
        }
        4 => {
            r.read_list(|r| {
                row_groups.push(read_row_group(r)?);
                Ok(())
            })?;
            Ok(true)
        }
        _ => Ok(false),
    })?;

    let columns = leaf_columns(&schema)?;
    for group in &row_groups {
        if group.chunks.len() != columns.len() {
            return Err(Error::new(
                "this Parquet file has a row group with a different column count from its schema",
            ));
        }
    }
    let counted: usize = row_groups.iter().map(|g| g.num_rows).sum();
    if counted != rows {
        // The footer's own row count is the one every reader trusts; a
        // disagreement means the file is not what it says it is.
        return Err(Error::new(format!(
            "this Parquet file says it has {rows} rows and its row groups hold {counted}"
        )));
    }
    Ok((columns, row_groups, rows))
}

struct SchemaElement {
    physical: Option<i32>,
    type_length: i32,
    repetition: i32,
    name: String,
    num_children: i32,
    converted: Option<i32>,
    scale: i32,
    logical: Option<Render>,
}

fn read_schema_element(r: &mut thrift::Reader) -> Result<SchemaElement> {
    let mut out = SchemaElement {
        physical: None,
        type_length: 0,
        repetition: 0,
        name: String::new(),
        num_children: 0,
        converted: None,
        scale: 0,
        logical: None,
    };
    let mut decimal_scale = None;
    r.read_struct(|r, f| match f.id {
        1 => {
            out.physical = Some(r.i32()?);
            Ok(true)
        }
        2 => {
            out.type_length = r.i32()?;
            Ok(true)
        }
        3 => {
            out.repetition = r.i32()?;
            Ok(true)
        }
        4 => {
            out.name = r.string()?;
            Ok(true)
        }
        5 => {
            out.num_children = r.i32()?;
            Ok(true)
        }
        6 => {
            out.converted = Some(r.i32()?);
            Ok(true)
        }
        7 => {
            decimal_scale = Some(r.i32()?);
            Ok(true)
        }
        10 => {
            out.logical = read_logical_type(r)?;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    out.scale = decimal_scale.unwrap_or(0);
    // A DECIMAL written the old way carries its scale in the schema element
    // rather than in the logical type.
    if out.logical.is_none() && out.converted == Some(5) {
        out.logical = Some(Render::Decimal { scale: out.scale });
    }
    Ok(out)
}

/// The logical type union, of which this reader needs the six that change how a
/// value reads as text.
fn read_logical_type(r: &mut thrift::Reader) -> Result<Option<Render>> {
    let mut out = None;
    r.read_struct(|r, f| match f.id {
        1 | 12 | 13 | 14 => {
            // STRING, JSON, BSON, UUID: all of them are their bytes.
            r.skip(f.kind)?;
            out = Some(Render::Text);
            Ok(true)
        }
        5 => {
            let mut scale = 0;
            r.read_struct(|r, g| match g.id {
                1 => {
                    scale = r.i32()?;
                    Ok(true)
                }
                _ => Ok(false),
            })?;
            out = Some(Render::Decimal { scale });
            Ok(true)
        }
        6 => {
            r.skip(f.kind)?;
            out = Some(Render::Date);
            Ok(true)
        }
        7 => {
            let (_, unit) = read_time_type(r)?;
            out = Some(Render::Time { unit });
            Ok(true)
        }
        8 => {
            let (utc, unit) = read_time_type(r)?;
            out = Some(Render::Timestamp { unit, utc });
            Ok(true)
        }
        10 => {
            let mut signed = true;
            r.read_struct(|r, g| match g.id {
                2 => {
                    signed = matches!(g.kind, kind::TRUE);
                    if !matches!(g.kind, kind::TRUE | kind::FALSE) {
                        signed = r.i32()? != 0;
                    }
                    Ok(true)
                }
                _ => Ok(false),
            })?;
            out = Some(Render::Int { signed });
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(out)
}

/// The `isAdjustedToUTC` flag and the unit shared by TIME and TIMESTAMP.
fn read_time_type(r: &mut thrift::Reader) -> Result<(bool, Unit)> {
    let mut utc = false;
    let mut unit = Unit::Millis;
    r.read_struct(|r, f| match f.id {
        1 => {
            // A compact-protocol bool is in the field header itself.
            utc = matches!(f.kind, kind::TRUE);
            if !matches!(f.kind, kind::TRUE | kind::FALSE) {
                utc = r.i32()? != 0;
            }
            Ok(true)
        }
        2 => {
            r.read_struct(|r, g| {
                unit = match g.id {
                    1 => Unit::Millis,
                    2 => Unit::Micros,
                    _ => Unit::Nanos,
                };
                r.skip(g.kind)?;
                Ok(true)
            })?;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok((utc, unit))
}

/// The schema's leaves, refusing anything that is not a flat table.
fn leaf_columns(schema: &[SchemaElement]) -> Result<Vec<ColumnInfo>> {
    if schema.is_empty() {
        return Err(Error::new("this Parquet file has no schema"));
    }
    let mut out = Vec::new();
    for element in &schema[1..] {
        if element.num_children > 0 {
            return Err(Error::new(format!(
                "this Parquet file nests a group under {}; a comparison needs a flat table of cells",
                element.name
            )));
        }
        if element.repetition == 2 {
            return Err(Error::new(format!(
                "this Parquet file repeats {}; a comparison needs one value per row",
                element.name
            )));
        }
        let physical = Physical::from_id(element.physical.unwrap_or(-1))?;
        out.push(ColumnInfo {
            name: element.name.clone(),
            physical,
            render: render_for(physical, element),
            type_length: element.type_length.max(0) as usize,
            max_def_level: u8::from(element.repetition == 1),
        });
    }
    if out.is_empty() {
        return Err(Error::new("this Parquet file has no columns"));
    }
    Ok(out)
}

/// What a value of this column reads as, from its logical type where it has one
/// and from the physical type where it does not.
fn render_for(physical: Physical, element: &SchemaElement) -> Render {
    if let Some(render) = element.logical {
        // An INTEGER logical type still renders as an integer; the rest say
        // something the physical type does not.
        if !matches!(render, Render::Int { .. }) || physical != Physical::ByteArray {
            return render;
        }
    }
    match element.converted {
        Some(0) | Some(4) | Some(19) | Some(20) => return Render::Text, // UTF8, ENUM, JSON, BSON
        Some(6) => return Render::Date,
        Some(7) => return Render::Time { unit: Unit::Millis },
        Some(8) => return Render::Time { unit: Unit::Micros },
        Some(9) => {
            return Render::Timestamp {
                unit: Unit::Millis,
                utc: true,
            };
        }
        Some(10) => {
            return Render::Timestamp {
                unit: Unit::Micros,
                utc: true,
            };
        }
        Some(11) | Some(12) | Some(13) | Some(14) => return Render::Int { signed: false },
        _ => {}
    }
    match physical {
        Physical::Boolean => Render::Bool,
        Physical::Int32 | Physical::Int64 => Render::Int { signed: true },
        Physical::Int96 => Render::Timestamp {
            unit: Unit::Nanos,
            utc: false,
        },
        Physical::Float => Render::Float,
        Physical::Double => Render::Double,
        Physical::ByteArray | Physical::FixedLen => Render::Text,
    }
}

fn read_row_group(r: &mut thrift::Reader) -> Result<RowGroup> {
    let mut chunks = Vec::new();
    let mut num_rows = 0usize;
    r.read_struct(|r, f| match f.id {
        1 => {
            r.read_list(|r| {
                chunks.push(read_column_chunk(r)?);
                Ok(())
            })?;
            Ok(true)
        }
        3 => {
            num_rows = r.i64()?.max(0) as usize;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(RowGroup { num_rows, chunks })
}

fn read_column_chunk(r: &mut thrift::Reader) -> Result<Chunk> {
    let mut chunk = Chunk {
        codec: Codec::None,
        num_values: 0,
        data_page_offset: 0,
        dictionary_page_offset: None,
        total_compressed_size: 0,
    };
    let mut external = false;
    r.read_struct(|r, f| match f.id {
        1 => {
            external = !r.string()?.is_empty();
            Ok(true)
        }
        3 => {
            r.read_struct(|r, g| match g.id {
                4 => {
                    chunk.codec = Codec::from_id(r.i32()?)?;
                    Ok(true)
                }
                5 => {
                    chunk.num_values = r.i64()?;
                    Ok(true)
                }
                7 => {
                    chunk.total_compressed_size = r.i64()?;
                    Ok(true)
                }
                9 => {
                    chunk.data_page_offset = r.i64()?.max(0) as u64;
                    Ok(true)
                }
                11 => {
                    let at = r.i64()?;
                    chunk.dictionary_page_offset = (at > 0).then_some(at as u64);
                    Ok(true)
                }
                _ => Ok(false),
            })?;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    if external {
        return Err(Error::new(
            "this Parquet file keeps a column in another file, which this reader does not follow",
        ));
    }
    Ok(chunk)
}

// ---------------------------------------------------------------------------
// The pages
// ---------------------------------------------------------------------------

/// One page's header, and where its bytes begin.
struct PageHeader {
    kind: i32,
    uncompressed: usize,
    compressed: usize,
    num_values: usize,
    encoding: i32,
    /// Version 2 keeps its levels outside the compressed part.
    v2_def_bytes: usize,
    v2_rep_bytes: usize,
    v2_compressed: bool,
    body_at: usize,
}

fn read_page_header(data: &[u8], at: usize) -> Result<PageHeader> {
    let mut r = thrift::Reader::new(&data[at..]);
    let mut h = PageHeader {
        kind: 0,
        uncompressed: 0,
        compressed: 0,
        num_values: 0,
        encoding: 0,
        v2_def_bytes: 0,
        v2_rep_bytes: 0,
        v2_compressed: true,
        body_at: 0,
    };
    r.read_struct(|r, f| match f.id {
        1 => {
            h.kind = r.i32()?;
            Ok(true)
        }
        2 => {
            h.uncompressed = r.i32()?.max(0) as usize;
            Ok(true)
        }
        3 => {
            h.compressed = r.i32()?.max(0) as usize;
            Ok(true)
        }
        5 | 7 => {
            // Data page v1 and dictionary page: both carry a count and an
            // encoding in the same two field ids.
            r.read_struct(|r, g| match g.id {
                1 => {
                    h.num_values = r.i32()?.max(0) as usize;
                    Ok(true)
                }
                2 => {
                    h.encoding = r.i32()?;
                    Ok(true)
                }
                _ => Ok(false),
            })?;
            Ok(true)
        }
        8 => {
            r.read_struct(|r, g| match g.id {
                1 => {
                    h.num_values = r.i32()?.max(0) as usize;
                    Ok(true)
                }
                4 => {
                    h.encoding = r.i32()?;
                    Ok(true)
                }
                5 => {
                    h.v2_def_bytes = r.i32()?.max(0) as usize;
                    Ok(true)
                }
                6 => {
                    h.v2_rep_bytes = r.i32()?.max(0) as usize;
                    Ok(true)
                }
                7 => {
                    h.v2_compressed = matches!(g.kind, kind::TRUE);
                    if !matches!(g.kind, kind::TRUE | kind::FALSE) {
                        h.v2_compressed = r.i32()? != 0;
                    }
                    Ok(true)
                }
                _ => Ok(false),
            })?;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    h.body_at = at + r.position();
    if let Some(refusal) = crate::source::page_bomb(
        h.num_values as u64,
        h.uncompressed as u64,
        h.compressed as u64,
    ) {
        return Err(Error::new(refusal));
    }
    Ok(h)
}

impl Reader {
    /// Decodes one column across every row group into a field per row.
    fn decode_column(&self, leaf: usize, limit: usize) -> Result<Decoded> {
        let info = &self.columns[leaf];
        let data = self.slab.data();
        let mut out = Decoded {
            fields: Vec::with_capacity(self.rows.min(limit)),
            arena: Vec::new(),
        };
        let mut scratch: Vec<u8> = Vec::with_capacity(64);

        for group in &self.row_groups {
            if out.fields.len() >= limit {
                break;
            }
            // Set when the page loop stopped for the limit rather than for the
            // end of the chunk: the row-count check below is only meaningful
            // for a group that was decoded whole.
            let mut truncated = false;
            let chunk = &group.chunks[leaf];
            let start = match chunk.dictionary_page_offset {
                Some(dict) if dict < chunk.data_page_offset => dict,
                _ => chunk.data_page_offset,
            } as usize;
            let end = start
                .saturating_add(chunk.total_compressed_size.max(0) as usize)
                .min(data.len());
            let mut at = start;
            // A dictionary belongs to one column chunk, so it is decoded once
            // per row group and every row of that group points into it.
            let mut dictionary: Vec<Field> = Vec::new();
            let mut seen = 0i64;
            let group_from = out.fields.len();

            while at < end && seen < chunk.num_values {
                let header = read_page_header(data, at)?;
                let body = data
                    .get(header.body_at..header.body_at + header.compressed)
                    .ok_or_else(|| Error::new("a Parquet page runs past the end of the file"))?;
                at = header.body_at + header.compressed;

                match header.kind {
                    2 => {
                        let page = decompress(chunk.codec, body, header.uncompressed)?;
                        dictionary = decode_dictionary(
                            &page,
                            header.num_values,
                            info,
                            &mut out.arena,
                            &mut scratch,
                        )?;
                    }
                    0 | 3 => {
                        seen += header.num_values as i64;
                        self.decode_data_page(
                            &header,
                            body,
                            chunk,
                            info,
                            &dictionary,
                            &mut out,
                            &mut scratch,
                        )?;
                    }
                    // An index page carries no values; anything else is a page
                    // type that did not exist when this was written.
                    _ => {}
                }
                if out.fields.len() >= limit {
                    truncated = true;
                    break;
                }
            }

            let got = out.fields.len() - group_from;
            if !truncated && got != group.num_rows {
                return Err(Error::new(format!(
                    "column {} yielded {got} values for a row group of {} rows",
                    info.name, group.num_rows
                )));
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_data_page(
        &self,
        header: &PageHeader,
        body: &[u8],
        chunk: &Chunk,
        info: &ColumnInfo,
        dictionary: &[Field],
        out: &mut Decoded,
        scratch: &mut Vec<u8>,
    ) -> Result<()> {
        // Version 2 keeps the levels outside the compressed part, so only the
        // values are decompressed; version 1 compresses the whole page.
        let (levels, values) = if header.kind == 3 {
            let level_bytes = header.v2_def_bytes + header.v2_rep_bytes;
            let levels = body
                .get(..level_bytes)
                .ok_or_else(|| Error::new("a Parquet v2 page is shorter than its levels"))?;
            let rest = &body[level_bytes..];
            let values = if header.v2_compressed && chunk.codec != Codec::None {
                let expected = header
                    .uncompressed
                    .checked_sub(level_bytes)
                    .ok_or_else(|| {
                        Error::new("a Parquet v2 page's levels are longer than the whole page")
                    })?;
                decompress(chunk.codec, rest, expected)?
            } else {
                rest.to_vec()
            };
            (levels[header.v2_rep_bytes..].to_vec(), values)
        } else {
            let page = decompress(chunk.codec, body, header.uncompressed)?;
            if info.max_def_level == 0 {
                (Vec::new(), page)
            } else {
                // Version 1 prefixes its level data with a four-byte length.
                let n = u32::from_le_bytes(
                    page.get(..4)
                        .ok_or_else(|| {
                            Error::new("a Parquet page is shorter than its level header")
                        })?
                        .try_into()
                        .expect("four bytes"),
                ) as usize;
                let levels = page
                    .get(4..4 + n)
                    .ok_or_else(|| Error::new("a Parquet page is shorter than its levels"))?
                    .to_vec();
                (levels, page[4 + n..].to_vec())
            }
        };

        // Which rows have a value at all, and how many values the page holds.
        let mut present = alloc::filled(true, header.num_values, "the page's null flags")?;
        let mut wanted = header.num_values;
        if info.max_def_level > 0 {
            let mut hybrid = Hybrid::new(&levels, bit_width(info.max_def_level as u32));
            wanted = 0;
            for slot in present.iter_mut() {
                *slot = hybrid.next()? >= info.max_def_level as u32;
                wanted += usize::from(*slot);
            }
        }

        let mut values = decode_values(
            &values,
            header.encoding,
            wanted,
            info,
            dictionary,
            &mut out.arena,
            scratch,
        )?;
        values.reverse(); // popped from the back, which is the front of the page
        for slot in present {
            out.fields.push(if slot {
                values.pop().ok_or_else(|| {
                    Error::new("a Parquet page holds fewer values than its levels promise")
                })?
            } else {
                ABSENT
            });
        }
        Ok(())
    }
}

/// The dictionary page: plain-encoded values, decoded once into the arena.
fn decode_dictionary(
    page: &[u8],
    count: usize,
    info: &ColumnInfo,
    arena: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<Vec<Field>> {
    plain(page, count, info, arena, scratch)
}

/// One data page's values, in whichever encoding it was written in.
#[allow(clippy::too_many_arguments)]
fn decode_values(
    data: &[u8],
    encoding: i32,
    count: usize,
    info: &ColumnInfo,
    dictionary: &[Field],
    arena: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<Vec<Field>> {
    match encoding {
        0 => plain(data, count, info, arena, scratch),
        2 | 8 => {
            // Dictionary indices: a bit width, then the hybrid encoding.
            if dictionary.is_empty() && count > 0 {
                return Err(Error::new(
                    "a Parquet page is dictionary-encoded but its column chunk has no dictionary",
                ));
            }
            let width = *data
                .first()
                .ok_or_else(|| Error::new("a dictionary-encoded Parquet page is empty"))?;
            let mut hybrid = Hybrid::new(&data[1..], width);
            let mut out = alloc::sized(count, "the values of a Parquet page")?;
            for _ in 0..count {
                let at = hybrid.next()? as usize;
                out.push(*dictionary.get(at).ok_or_else(|| {
                    Error::new("a Parquet page indexes past the end of its dictionary")
                })?);
            }
            Ok(out)
        }
        3 => {
            // RLE, which in a data page means booleans.
            let mut hybrid = Hybrid::new(&data[4.min(data.len())..], 1);
            let mut out = alloc::sized(count, "the values of a Parquet page")?;
            for _ in 0..count {
                out.push(push_bool(arena, hybrid.next()? != 0));
            }
            Ok(out)
        }
        5 => {
            let (ints, _) = delta_binary_packed(data, count)?;
            let mut out = alloc::sized(count, "the values of a Parquet page")?;
            for &v in ints.iter().take(count) {
                out.push(push_number(arena, scratch, v, info));
            }
            Ok(out)
        }
        6 => {
            let (lengths, used) = delta_binary_packed(data, count)?;
            let mut at = used;
            let mut out = alloc::sized(count, "the values of a Parquet page")?;
            for &len in lengths.iter().take(count) {
                let len = len.max(0) as usize;
                let bytes = data
                    .get(at..at + len)
                    .ok_or_else(|| Error::new("a delta-length Parquet page ends mid-value"))?;
                at += len;
                out.push(push_bytes(arena, bytes));
            }
            Ok(out)
        }
        7 => {
            // Prefix lengths, suffix lengths, then the suffixes: each value
            // shares a prefix with the one before it.
            let (prefixes, used) = delta_binary_packed(data, count)?;
            let (suffixes, used2) = delta_binary_packed(&data[used..], count)?;
            let mut at = used + used2;
            let mut previous: Vec<u8> = Vec::new();
            let mut out = alloc::sized(count, "the values of a Parquet page")?;
            for i in 0..count.min(prefixes.len()).min(suffixes.len()) {
                let prefix = prefixes[i].max(0) as usize;
                let len = suffixes[i].max(0) as usize;
                let bytes = data
                    .get(at..at + len)
                    .ok_or_else(|| Error::new("a delta-byte-array Parquet page ends mid-value"))?;
                at += len;
                if prefix > previous.len() {
                    return Err(Error::new(
                        "a delta-byte-array Parquet value shares a prefix longer than the value before it",
                    ));
                }
                previous.truncate(prefix);
                previous.extend_from_slice(bytes);
                out.push(push_bytes(arena, &previous));
            }
            Ok(out)
        }
        other => Err(Error::new(format!(
            "this Parquet file uses encoding {other}, which this reader does not decode; \
             rewrite it with PLAIN or dictionary encoding"
        ))),
    }
}

/// Plain-encoded values: the physical layout, one after another.
fn plain(
    data: &[u8],
    count: usize,
    info: &ColumnInfo,
    arena: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<Vec<Field>> {
    let mut out = alloc::sized(count, "the values of a Parquet page")?;
    let mut at = 0usize;
    let short = || Error::new("a plain-encoded Parquet page ends mid-value");
    for i in 0..count {
        match info.physical {
            Physical::Boolean => {
                let byte = *data.get(i / 8).ok_or_else(short)?;
                out.push(push_bool(arena, byte >> (i % 8) & 1 == 1));
            }
            Physical::ByteArray => {
                let head = data.get(at..at + 4).ok_or_else(short)?;
                let len = u32::from_le_bytes(head.try_into().expect("four bytes")) as usize;
                at += 4;
                let bytes = data.get(at..at + len).ok_or_else(short)?;
                at += len;
                out.push(push_scalar(arena, scratch, bytes, info));
            }
            Physical::FixedLen => {
                let len = info.type_length;
                let bytes = data.get(at..at + len).ok_or_else(short)?;
                at += len;
                out.push(push_scalar(arena, scratch, bytes, info));
            }
            Physical::Int32 => {
                let bytes = data.get(at..at + 4).ok_or_else(short)?;
                at += 4;
                let v = i32::from_le_bytes(bytes.try_into().expect("four bytes"));
                out.push(match info.render {
                    Render::Date => push_date(arena, scratch, v as i64),
                    Render::Int { signed: false } => {
                        push_number(arena, scratch, (v as u32) as i64, info)
                    }
                    _ => push_number(arena, scratch, v as i64, info),
                });
            }
            Physical::Int64 => {
                let bytes = data.get(at..at + 8).ok_or_else(short)?;
                at += 8;
                let v = i64::from_le_bytes(bytes.try_into().expect("eight bytes"));
                out.push(push_number(arena, scratch, v, info));
            }
            Physical::Int96 => {
                // Twelve bytes: nanoseconds within the day, then a Julian day.
                let bytes = data.get(at..at + 12).ok_or_else(short)?;
                at += 12;
                let nanos = u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes")) as i128;
                let julian = u32::from_le_bytes(bytes[8..].try_into().expect("four bytes")) as i128;
                let value = (julian - 2_440_588) * 86_400_000_000_000 + nanos;
                scratch.clear();
                write_timestamp(scratch, value as i64, Unit::Nanos, false);
                out.push(push_bytes(arena, scratch));
            }
            Physical::Float => {
                let bytes = data.get(at..at + 4).ok_or_else(short)?;
                at += 4;
                let v = f32::from_le_bytes(bytes.try_into().expect("four bytes"));
                scratch.clear();
                write_f32(scratch, v);
                out.push(push_bytes(arena, scratch));
            }
            Physical::Double => {
                let bytes = data.get(at..at + 8).ok_or_else(short)?;
                at += 8;
                let v = f64::from_le_bytes(bytes.try_into().expect("eight bytes"));
                scratch.clear();
                write_f64(scratch, v);
                out.push(push_bytes(arena, scratch));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Values as text
// ---------------------------------------------------------------------------

fn push_bytes(arena: &mut Vec<u8>, bytes: &[u8]) -> Field {
    let at = arena.len() as u64;
    arena.extend_from_slice(bytes);
    pack(at, bytes.len() as u64, false)
}

fn push_bool(arena: &mut Vec<u8>, value: bool) -> Field {
    push_bytes(arena, if value { b"true" } else { b"false" })
}

/// A byte-array or fixed-length value, which is its bytes unless the column says
/// those bytes are a decimal.
fn push_scalar(
    arena: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    bytes: &[u8],
    info: &ColumnInfo,
) -> Field {
    match info.render {
        Render::Decimal { scale } => {
            let mut unscaled: i128 = if bytes.first().is_some_and(|&b| b & 0x80 != 0) {
                -1
            } else {
                0
            };
            for &b in bytes.iter().take(16) {
                unscaled = (unscaled << 8) | b as i128;
            }
            scratch.clear();
            write_decimal(scratch, unscaled, scale);
            push_bytes(arena, scratch)
        }
        _ => push_bytes(arena, bytes),
    }
}

/// An integer value, rendered by whatever the column's logical type makes of it.
fn push_number(arena: &mut Vec<u8>, scratch: &mut Vec<u8>, value: i64, info: &ColumnInfo) -> Field {
    scratch.clear();
    write_number(scratch, value, info.render);
    push_bytes(arena, scratch)
}

fn write_number(out: &mut Vec<u8>, value: i64, render: Render) {
    match render {
        Render::Decimal { scale } => write_decimal(out, value as i128, scale),
        Render::Date => write_date(out, value),
        Render::Time { unit } => write_time(out, value, unit),
        Render::Timestamp { unit, utc } => write_timestamp(out, value, unit, utc),
        _ => {
            let mut buffer = itoa(value);
            out.append(&mut buffer);
        }
    }
}

fn push_date(arena: &mut Vec<u8>, scratch: &mut Vec<u8>, days: i64) -> Field {
    scratch.clear();
    write_date(scratch, days);
    push_bytes(arena, scratch)
}

fn itoa(value: i64) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// A float as the shortest digits that read back as the same value, laid out the
/// way JavaScript's number-to-string does it.
///
/// "Shortest round trip" does not say whether `1e-10` is written out in full, and
/// two ports that answer that differently would report a column as changed when
/// it is not. So the layout is stated: plain decimal while the exponent is
/// between -6 and 21, scientific outside it. Every port implements this one rule.
fn write_float(
    out: &mut Vec<u8>,
    negative: bool,
    zero: bool,
    nan: bool,
    infinite: bool,
    sci: &str,
) {
    if nan {
        out.extend_from_slice(b"NaN");
        return;
    }
    if infinite {
        out.extend_from_slice(if negative { b"-Infinity" } else { b"Infinity" });
        return;
    }
    if zero {
        out.push(b'0');
        return;
    }
    // `{:e}` gives the shortest digits as `d.ddde±X`, so the value is those
    // digits with the point after position `n`.
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci, "0"));
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().unwrap_or(0) + 1;
    if negative {
        out.push(b'-');
    }
    if k <= n && n <= 21 {
        out.extend_from_slice(digits.as_bytes());
        out.extend(std::iter::repeat_n(b'0', (n - k) as usize));
    } else if n > 0 && n <= 21 {
        out.extend_from_slice(&digits.as_bytes()[..n as usize]);
        out.push(b'.');
        out.extend_from_slice(&digits.as_bytes()[n as usize..]);
    } else if n > -6 && n <= 0 {
        out.extend_from_slice(b"0.");
        out.extend(std::iter::repeat_n(b'0', (-n) as usize));
        out.extend_from_slice(digits.as_bytes());
    } else {
        out.push(digits.as_bytes()[0]);
        if k > 1 {
            out.push(b'.');
            out.extend_from_slice(&digits.as_bytes()[1..]);
        }
        out.push(b'e');
        out.push(if n > 0 { b'+' } else { b'-' });
        out.extend_from_slice((n - 1).abs().to_string().as_bytes());
    }
}

fn write_f64(out: &mut Vec<u8>, v: f64) {
    write_float(
        out,
        v.is_sign_negative() && v != 0.0,
        v == 0.0,
        v.is_nan(),
        v.is_infinite(),
        &format!("{:e}", v.abs()),
    );
}

/// Formatted from the `f32` rather than from a widened `f64`, because the
/// shortest digits that round-trip a `f32` are not the ones that round-trip the
/// double it widens to: 0.1f32 would otherwise print as 0.10000000149011612.
fn write_f32(out: &mut Vec<u8>, v: f32) {
    write_float(
        out,
        v.is_sign_negative() && v != 0.0,
        v == 0.0,
        v.is_nan(),
        v.is_infinite(),
        &format!("{:e}", v.abs()),
    );
}

/// An unscaled integer and a scale, written out in full: `12345` at scale 2 is
/// `123.45`, which is what a CSV of the same column holds.
fn write_decimal(out: &mut Vec<u8>, unscaled: i128, scale: i32) {
    if scale <= 0 {
        out.extend_from_slice(unscaled.to_string().as_bytes());
        for _ in 0..-scale {
            out.push(b'0');
        }
        return;
    }
    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    if negative {
        out.push(b'-');
    }
    if digits.len() > scale {
        out.extend_from_slice(&digits.as_bytes()[..digits.len() - scale]);
        out.push(b'.');
        out.extend_from_slice(&digits.as_bytes()[digits.len() - scale..]);
    } else {
        out.extend_from_slice(b"0.");
        for _ in 0..scale - digits.len() {
            out.push(b'0');
        }
        out.extend_from_slice(digits.as_bytes());
    }
}

/// The civil date `days` after 1970-01-01, by the shift-the-year-to-March
/// algorithm that makes the leap rule a division rather than a table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

fn two(out: &mut Vec<u8>, value: u32) {
    out.push(b'0' + (value / 10 % 10) as u8);
    out.push(b'0' + (value % 10) as u8);
}

fn write_date(out: &mut Vec<u8>, days: i64) {
    let (year, month, day) = civil_from_days(days);
    out.extend_from_slice(format!("{year:04}").as_bytes());
    out.push(b'-');
    two(out, month);
    out.push(b'-');
    two(out, day);
}

fn per_second(unit: Unit) -> i64 {
    match unit {
        Unit::Millis => 1_000,
        Unit::Micros => 1_000_000,
        Unit::Nanos => 1_000_000_000,
    }
}

/// The fractional part, with the trailing zeros a whole second would leave
/// dropped: `12:00:00` rather than `12:00:00.000`.
fn write_fraction(out: &mut Vec<u8>, fraction: i64, unit: Unit) {
    if fraction == 0 {
        return;
    }
    let digits = match unit {
        Unit::Millis => 3,
        Unit::Micros => 6,
        Unit::Nanos => 9,
    };
    let mut text = format!("{fraction:0digits$}", digits = digits);
    while text.ends_with('0') {
        text.pop();
    }
    out.push(b'.');
    out.extend_from_slice(text.as_bytes());
}

fn write_time(out: &mut Vec<u8>, value: i64, unit: Unit) {
    let scale = per_second(unit);
    let seconds = value.div_euclid(scale);
    let fraction = value.rem_euclid(scale);
    two(out, (seconds / 3600 % 24) as u32);
    out.push(b':');
    two(out, (seconds / 60 % 60) as u32);
    out.push(b':');
    two(out, (seconds % 60) as u32);
    write_fraction(out, fraction, unit);
}

fn write_timestamp(out: &mut Vec<u8>, value: i64, unit: Unit, utc: bool) {
    let scale = per_second(unit);
    let seconds = value.div_euclid(scale);
    let fraction = value.rem_euclid(scale);
    write_date(out, seconds.div_euclid(86_400));
    out.push(b'T');
    let time = seconds.rem_euclid(86_400);
    two(out, (time / 3600) as u32);
    out.push(b':');
    two(out, (time / 60 % 60) as u32);
    out.push(b':');
    two(out, (time % 60) as u32);
    write_fraction(out, fraction, unit);
    if utc {
        out.push(b'Z');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: impl Fn(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).expect("ascii")
    }

    #[test]
    fn dates_are_written_the_way_a_csv_of_the_same_column_would() {
        assert_eq!(text(|o| write_date(o, 0)), "1970-01-01");
        assert_eq!(text(|o| write_date(o, 19_723)), "2024-01-01");
        assert_eq!(text(|o| write_date(o, -1)), "1969-12-31");
        assert_eq!(text(|o| write_date(o, 19_782)), "2024-02-29");
    }

    #[test]
    fn timestamps_drop_a_zero_fraction_and_keep_a_real_one() {
        assert_eq!(
            text(|o| write_timestamp(o, 1_704_067_200_000, Unit::Millis, true)),
            "2024-01-01T00:00:00Z"
        );
        assert_eq!(
            text(|o| write_timestamp(o, 1_704_067_200_500, Unit::Millis, false)),
            "2024-01-01T00:00:00.5"
        );
        assert_eq!(
            text(|o| write_timestamp(o, -1, Unit::Millis, false)),
            "1969-12-31T23:59:59.999"
        );
    }

    #[test]
    fn floats_take_the_layout_every_port_agrees_on() {
        let f = |v: f64| text(|o| write_f64(o, v));
        assert_eq!(f(0.0), "0");
        assert_eq!(f(3.0), "3");
        assert_eq!(f(0.5), "0.5");
        assert_eq!(f(-0.25), "-0.25");
        assert_eq!(f(1e10), "10000000000");
        assert_eq!(f(1e-10), "1e-10");
        assert_eq!(f(1.5e22), "1.5e+22");
        assert_eq!(f(0.000001), "0.000001");
        assert_eq!(f(f64::NAN), "NaN");
        assert_eq!(f(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(text(|o| write_f32(o, 0.1f32)), "0.1");
    }

    #[test]
    fn a_decimal_is_written_out_in_full() {
        assert_eq!(text(|o| write_decimal(o, 12_345, 2)), "123.45");
        assert_eq!(text(|o| write_decimal(o, -5, 3)), "-0.005");
        assert_eq!(text(|o| write_decimal(o, 7, 0)), "7");
        assert_eq!(text(|o| write_decimal(o, 12, -2)), "1200");
    }
}
