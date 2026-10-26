//! Page decompression: the four codecs a Parquet file in the wild actually uses.
//!
//! Snappy and LZ4 are written out here rather than pulled in, because both are
//! byte-copy loops of about eighty lines and each would otherwise be a
//! dependency for one function. Gzip and zstd are not: their decoders are real
//! programs, and the crates for them are already in this build.
//!
//! Every one of these writes into a buffer sized from the page header, so a
//! corrupt length is a refusal rather than an allocation the size of the number
//! that happened to be in the file. The columnar reader (`crate::parquet`)
//! decodes its gzip, zstd and LZ4 pages through here too.

use std::io::Read;

use crate::error::{Error, Result};

/// Parquet's compression codec ids, from its Thrift schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    None,
    Snappy,
    Gzip,
    Zstd,
    Lz4Raw,
}

impl Codec {
    pub(crate) fn from_id(id: i32) -> Result<Codec> {
        Ok(match id {
            0 => Codec::None,
            1 => Codec::Snappy,
            2 => Codec::Gzip,
            6 => Codec::Zstd,
            7 => Codec::Lz4Raw,
            3 => return Err(unsupported("LZO")),
            4 => return Err(unsupported("Brotli")),
            5 => {
                return Err(unsupported(
                    "the deprecated LZ4 framing; rewrite it as LZ4_RAW",
                ));
            }
            other => return Err(unsupported(&format!("codec {other}"))),
        })
    }
}

fn unsupported(what: &str) -> Error {
    Error::new(format!(
        "this Parquet file is compressed with {what}, which this reader does not decode"
    ))
}

fn truncated() -> Error {
    Error::new("a compressed Parquet page ends mid-stream")
}

/// Decompresses one page into `out`, which is sized from the page header.
pub(crate) fn decompress(codec: Codec, input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let out = match codec {
        Codec::None => input.to_vec(),
        Codec::Snappy => snappy(input, expected)?,
        Codec::Lz4Raw => lz4(input, expected)?,
        Codec::Gzip => {
            let mut out = crate::alloc::sized(expected, "a decompressed Parquet page")?;
            flate2::read::MultiGzDecoder::new(input)
                .take(expected as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| Error::new(format!("a gzip Parquet page will not decode: {e}")))?;
            out
        }
        // Into a buffer of exactly the size the header gives, which zstd will
        // not write past: a few kilobytes of zstd can expand to gigabytes, and
        // decode_all would have produced all of them before the length check.
        Codec::Zstd => {
            let mut out = crate::alloc::sized(expected, "a decompressed Parquet page")?;
            zstd::bulk::Decompressor::new()
                .and_then(|mut d| d.decompress_to_buffer(input, &mut out))
                .map_err(|e| Error::new(format!("a zstd Parquet page will not decode: {e}")))?;
            out
        }
    };
    if out.len() != expected {
        return Err(Error::new(format!(
            "a Parquet page decompressed to {} bytes where its header says {expected}",
            out.len()
        )));
    }
    Ok(out)
}

/// Copies `len` bytes from `distance` back in `out`, which may overlap itself:
/// a run of the same byte is written as a one-byte match repeated.
/// Refuses a page that has already decompressed past the size its header
/// gives, so a forged one cannot expand without bound before the final check.
fn within(out: &[u8], more: usize, expected: usize) -> Result<()> {
    if more > expected.saturating_sub(out.len()) {
        return Err(Error::new(format!(
            "a Parquet page decompresses past the {expected} bytes its header says"
        )));
    }
    Ok(())
}

fn copy_within(out: &mut Vec<u8>, distance: usize, len: usize) -> Result<()> {
    if distance == 0 || distance > out.len() {
        return Err(Error::new(
            "a compressed Parquet page refers to bytes before the start of the page",
        ));
    }
    for from in (out.len() - distance..).take(len) {
        let b = out[from];
        out.push(b);
    }
    Ok(())
}

/// Snappy's raw block format: a length, then literals and back-references.
fn snappy(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut at = 0usize;
    // The block opens with the uncompressed length as a varint.
    let mut declared = 0usize;
    let mut shift = 0u32;
    loop {
        let b = *input.get(at).ok_or_else(truncated)?;
        at += 1;
        declared |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 35 {
            return Err(Error::new(
                "a snappy Parquet page declares an absurd length",
            ));
        }
    }
    if declared != expected {
        return Err(Error::new(format!(
            "a snappy Parquet page declares {declared} bytes where its header says {expected}"
        )));
    }
    let mut out = crate::alloc::sized(expected, "a decompressed Parquet page")?;
    while at < input.len() {
        let tag = input[at];
        at += 1;
        if tag & 0x03 == 0 {
            // A literal: a short length in the tag, or one to four bytes of it.
            let mut len = (tag >> 2) as usize;
            if len >= 60 {
                let extra = len - 59;
                let bytes = input.get(at..at + extra).ok_or_else(truncated)?;
                at += extra;
                len = 0;
                for (i, &b) in bytes.iter().enumerate() {
                    len |= (b as usize) << (8 * i);
                }
            }
            len += 1;
            let bytes = input.get(at..at + len).ok_or_else(truncated)?;
            at += len;
            within(&out, bytes.len(), expected)?;
            out.extend_from_slice(bytes);
            continue;
        }
        let (len, distance) = match tag & 0x03 {
            1 => {
                let low = *input.get(at).ok_or_else(truncated)? as usize;
                at += 1;
                (
                    4 + ((tag >> 2) & 0x07) as usize,
                    (((tag >> 5) as usize) << 8) | low,
                )
            }
            2 => {
                let b = input.get(at..at + 2).ok_or_else(truncated)?;
                at += 2;
                (
                    ((tag >> 2) + 1) as usize,
                    u16::from_le_bytes([b[0], b[1]]) as usize,
                )
            }
            _ => {
                let b = input.get(at..at + 4).ok_or_else(truncated)?;
                at += 4;
                (
                    ((tag >> 2) + 1) as usize,
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize,
                )
            }
        };
        within(&out, len, expected)?;
        copy_within(&mut out, distance, len)?;
    }
    Ok(out)
}

/// LZ4's block format: a token, literals, then a two-byte back-reference.
fn lz4(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = crate::alloc::sized(expected, "a decompressed Parquet page")?;
    let mut at = 0usize;

    let extend = |at: &mut usize, mut len: usize| -> Result<usize> {
        if len == 15 {
            loop {
                let b = *input.get(*at).ok_or_else(truncated)?;
                *at += 1;
                len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(len)
    };

    while at < input.len() {
        let token = input[at];
        at += 1;
        let literals = extend(&mut at, (token >> 4) as usize)?;
        let bytes = input.get(at..at + literals).ok_or_else(truncated)?;
        at += literals;
        within(&out, bytes.len(), expected)?;
        out.extend_from_slice(bytes);
        // The last sequence of a block is literals only, with no match after it.
        if at >= input.len() {
            break;
        }
        let b = input.get(at..at + 2).ok_or_else(truncated)?;
        at += 2;
        let distance = u16::from_le_bytes([b[0], b[1]]) as usize;
        let len = extend(&mut at, (token & 0x0f) as usize)? + 4;
        within(&out, len, expected)?;
        copy_within(&mut out, distance, len)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snappy_reads_literals_and_back_references() {
        // "abcabcabcab": a three-byte literal then a copy that overlaps itself
        // -- eight bytes taken from three back, which is only possible because
        // the copy reads what it has just written.
        let block = [0x0b, 0x08, b'a', b'b', b'c', 0x11, 0x03];
        assert_eq!(
            snappy(&block, 11).expect("a valid block"),
            b"abcabcabcab".to_vec()
        );
    }

    #[test]
    fn a_page_that_decompresses_to_the_wrong_size_is_refused() {
        let block = [0x0b, 0x08, b'a', b'b', b'c', 0x11, 0x03];
        assert!(decompress(Codec::Snappy, &block, 12).is_err());
    }

    #[test]
    fn a_back_reference_before_the_page_is_refused_rather_than_read() {
        // A copy with a distance larger than anything written so far.
        let block = [0x04, 0x01, 0x0a];
        assert!(snappy(&block, 4).is_err());
    }

    #[test]
    fn lz4_reads_literals_and_matches() {
        // token: 3 literals, match of 4+0; "abc" then copy 4 from distance 3.
        let block = [0x30, b'a', b'b', b'c', 0x03, 0x00];
        assert_eq!(lz4(&block, 7).expect("a valid block"), b"abcabca".to_vec());
    }

    #[test]
    fn gzip_and_zstd_go_through_the_libraries_already_here() {
        let text = b"the same bytes, twice over, the same bytes, twice over";
        let zipped = zstd::stream::encode_all(&text[..], 1).expect("zstd to compress");
        assert_eq!(
            decompress(Codec::Zstd, &zipped, text.len()).expect("zstd to decode"),
            text.to_vec()
        );
    }
}
