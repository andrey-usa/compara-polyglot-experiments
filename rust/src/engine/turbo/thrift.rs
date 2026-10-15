//! Just enough of Thrift's compact protocol to read a Parquet footer.
//!
//! Parquet's metadata is a Thrift structure, and a Thrift library would bring a
//! code generator and a dependency for what is, at this scale, a few hundred
//! lines: field headers, zigzag varints, and the rule for skipping a field this
//! reader does not care about. Skipping correctly is the part that matters —
//! writers add fields, and a reader that cannot step over an unknown one breaks
//! on files it has never seen.

use crate::error::{Error, Result};

/// Thrift compact type codes, as they appear in the low nibble of a field header.
pub(super) mod kind {
    pub(in super::super) const STOP: u8 = 0;
    pub(in super::super) const TRUE: u8 = 1;
    pub(in super::super) const FALSE: u8 = 2;
    pub(in super::super) const BYTE: u8 = 3;
    pub(in super::super) const I16: u8 = 4;
    pub(in super::super) const I32: u8 = 5;
    pub(in super::super) const I64: u8 = 6;
    pub(in super::super) const DOUBLE: u8 = 7;
    pub(in super::super) const BINARY: u8 = 8;
    pub(in super::super) const LIST: u8 = 9;
    pub(in super::super) const SET: u8 = 10;
    pub(in super::super) const MAP: u8 = 11;
    pub(in super::super) const STRUCT: u8 = 12;
}

/// How deep `skip` follows nested structs and lists. Parquet's metadata nests
/// a few levels; a footer nested thousands deep exists only to overflow the
/// stack of a reader that recurses.
const MAX_NESTING: u32 = 64;

/// A cursor over the bytes of a compact-protocol message.
pub(super) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    /// Field ids are deltas from the previous one within a struct, so the reader
    /// carries the last id seen and stacks it across nested structs.
    last_id: i16,
    stack: Vec<i16>,
}

/// One field header: which field, and what it holds.
pub(super) struct FieldHeader {
    pub(super) id: i16,
    pub(super) kind: u8,
}

fn short(what: &str) -> Error {
    Error::new(format!("the Parquet metadata ends in the middle of {what}"))
}

impl<'a> Reader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Reader {
            data,
            at: 0,
            last_id: 0,
            stack: Vec::new(),
        }
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self.data.get(self.at).ok_or_else(|| short("a value"))?;
        self.at += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut out = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            out |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::new(
                    "a Parquet metadata varint is longer than 64 bits",
                ));
            }
        }
    }

    fn zigzag(&mut self) -> Result<i64> {
        let n = self.varint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    /// Enters a struct. Every `struct_begin` needs its `struct_end`, because
    /// field ids are relative to the enclosing struct's last id.
    pub(super) fn struct_begin(&mut self) {
        self.stack.push(self.last_id);
        self.last_id = 0;
    }

    pub(super) fn struct_end(&mut self) {
        self.last_id = self.stack.pop().unwrap_or(0);
    }

    /// The next field of the current struct, or `None` at its stop byte.
    pub(super) fn field(&mut self) -> Result<Option<FieldHeader>> {
        let header = self.byte()?;
        if header == kind::STOP {
            return Ok(None);
        }
        let kind = header & 0x0f;
        let delta = (header >> 4) as i16;
        let id = if delta == 0 {
            self.zigzag()? as i16
        } else {
            self.last_id.wrapping_add(delta)
        };
        self.last_id = id;
        Ok(Some(FieldHeader { id, kind }))
    }

    pub(super) fn i32(&mut self) -> Result<i32> {
        Ok(self.zigzag()? as i32)
    }

    pub(super) fn i64(&mut self) -> Result<i64> {
        self.zigzag()
    }

    pub(super) fn binary(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let from = self.at;
        let to = from
            .checked_add(len)
            .filter(|&to| to <= self.data.len())
            .ok_or_else(|| short("a string"))?;
        self.at = to;
        Ok(&self.data[from..to])
    }

    pub(super) fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.binary()?).into_owned())
    }

    /// A list header: how many elements, and of what.
    pub(super) fn list_begin(&mut self) -> Result<(usize, u8)> {
        let header = self.byte()?;
        let kind = header & 0x0f;
        let size = (header >> 4) as u64;
        let size = if size == 15 { self.varint()? } else { size };
        Ok((self.count(size)?, kind))
    }

    /// A container's element count, refused when the bytes left could not
    /// hold that many elements (every element, even a bool in a list, takes
    /// at least one), so a forged count can neither size an allocation nor
    /// spin a loop.
    fn count(&self, n: u64) -> Result<usize> {
        if n > (self.data.len() - self.at) as u64 {
            return Err(short("a list"));
        }
        Ok(n as usize)
    }

    /// Steps over a value of `kind` without interpreting it. This is what lets a
    /// file written by a newer library, carrying fields this reader has never
    /// heard of, still be read.
    pub(super) fn skip(&mut self, kind: u8) -> Result<()> {
        self.skip_nested(kind, 0, false)
    }

    /// `skip` at a nesting `depth`; an `element` sits in a list, set or map,
    /// where a bool takes a byte of its own instead of living in the field
    /// header.
    fn skip_nested(&mut self, kind: u8, depth: u32, element: bool) -> Result<()> {
        if depth > MAX_NESTING {
            return Err(Error::new(
                "the Parquet metadata nests deeper than any writer produces",
            ));
        }
        match kind {
            kind::TRUE | kind::FALSE if element => self.byte().map(|_| ()),
            kind::TRUE | kind::FALSE => Ok(()),
            kind::BYTE => self.byte().map(|_| ()),
            kind::I16 | kind::I32 | kind::I64 => self.zigzag().map(|_| ()),
            kind::DOUBLE => {
                self.at = self
                    .at
                    .checked_add(8)
                    .filter(|&to| to <= self.data.len())
                    .ok_or_else(|| short("a double"))?;
                Ok(())
            }
            kind::BINARY => self.binary().map(|_| ()),
            kind::LIST | kind::SET => {
                let (n, element) = self.list_begin()?;
                for _ in 0..n {
                    self.skip_nested(element, depth + 1, true)?;
                }
                Ok(())
            }
            kind::MAP => {
                let n = self.varint()?;
                let n = self.count(n)?;
                if n == 0 {
                    return Ok(());
                }
                let kinds = self.byte()?;
                let (key, value) = (kinds >> 4, kinds & 0x0f);
                for _ in 0..n {
                    self.skip_nested(key, depth + 1, true)?;
                    self.skip_nested(value, depth + 1, true)?;
                }
                Ok(())
            }
            kind::STRUCT => {
                self.struct_begin();
                while let Some(f) = self.field()? {
                    self.skip_nested(f.kind, depth + 1, false)?;
                }
                self.struct_end();
                Ok(())
            }
            other => Err(Error::new(format!(
                "the Parquet metadata holds a Thrift type this reader does not know: {other}"
            ))),
        }
    }

    /// Reads a whole struct, handing each field to `on_field`, which returns
    /// `false` for a field it did not consume so this reader can skip it.
    pub(super) fn read_struct<F>(&mut self, mut on_field: F) -> Result<()>
    where
        F: FnMut(&mut Self, &FieldHeader) -> Result<bool>,
    {
        self.struct_begin();
        while let Some(f) = self.field()? {
            if !on_field(self, &f)? {
                self.skip(f.kind)?;
            }
        }
        self.struct_end();
        Ok(())
    }

    /// Reads a list of structs, handing each to `each`.
    pub(super) fn read_list<F>(&mut self, mut each: F) -> Result<()>
    where
        F: FnMut(&mut Self) -> Result<()>,
    {
        let (n, element) = self.list_begin()?;
        for _ in 0..n {
            if element == kind::STRUCT {
                each(self)?;
            } else {
                self.skip_nested(element, 1, true)?;
            }
        }
        Ok(())
    }

    /// How far in the cursor has got, which a page header reader needs to know
    /// where the page's own bytes begin.
    pub(super) fn position(&self) -> usize {
        self.at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{1: 7, 2: "hi", 3: [4, 5]}` in compact form, then a stop byte.
    fn message() -> Vec<u8> {
        vec![
            0x15, 0x0e, // field 1 (delta 1), i32, zigzag 14 -> 7
            0x18, 0x02, b'h', b'i', // field 2, binary, length 2
            0x19, 0x25, 0x08, 0x0a, // field 3, list of two i32: 4 and 5
            0x00,
        ]
    }

    #[test]
    fn fields_are_read_by_delta_and_values_unzigzagged() {
        let bytes = message();
        let mut r = Reader::new(&bytes);
        let mut seen = Vec::new();
        r.read_struct(|r, f| {
            match f.id {
                1 => seen.push(format!("i32 {}", r.i32()?)),
                2 => seen.push(format!("str {}", r.string()?)),
                3 => {
                    let (n, _) = r.list_begin()?;
                    let mut all = Vec::new();
                    for _ in 0..n {
                        all.push(r.i32()?);
                    }
                    seen.push(format!("list {all:?}"));
                }
                _ => return Ok(false),
            }
            Ok(true)
        })
        .expect("a well-formed message");
        assert_eq!(seen, ["i32 7", "str hi", "list [4, 5]"]);
    }

    #[test]
    fn a_field_nobody_wants_is_stepped_over() {
        let bytes = message();
        let mut r = Reader::new(&bytes);
        let mut last = None;
        r.read_struct(|r, f| {
            if f.id == 3 {
                let (n, _) = r.list_begin()?;
                let mut all = Vec::new();
                for _ in 0..n {
                    all.push(r.i32()?);
                }
                last = Some(all);
                return Ok(true);
            }
            Ok(false) // skipped, whatever it was
        })
        .expect("skipping to work");
        assert_eq!(last, Some(vec![4, 5]));
    }
}
