//! The integer encodings a Parquet page is written in.
//!
//! Three of them, and every one of the reader's value paths ends in one:
//! definition levels and dictionary indices are the RLE / bit-packed hybrid,
//! version-2 integer columns are delta binary packed, and version-2 string
//! columns are lengths in that same delta encoding followed by the bytes.
//!
//! They are here rather than beside the page loop because they are the part
//! worth testing on its own: an off-by-one in a bit-packed group is a wrong
//! value rather than a failure, and a wrong value in a comparison tool is the
//! worst kind of bug there is.

use crate::error::{Error, Result};

fn truncated() -> Error {
    Error::new("a Parquet page ends in the middle of a value")
}

/// The bits needed to hold every value up to `max`.
pub(super) fn bit_width(max: u32) -> u8 {
    (32 - max.leading_zeros()) as u8
}

/// A little-endian varint, and how many bytes it took.
fn varint(data: &[u8], at: &mut usize) -> Result<u64> {
    let mut out = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *data.get(*at).ok_or_else(truncated)?;
        *at += 1;
        out |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(out);
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::new("a Parquet varint is longer than 64 bits"));
        }
    }
}

fn zigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// Reads `count` values of `width` bits each, packed least-significant bit first.
///
/// This is the layout both the hybrid and the delta encodings pack their groups
/// in: values run through the bytes without alignment, low bits first, so a
/// three-bit value can straddle a byte boundary and often does.
fn unpack(
    data: &[u8],
    bit_offset: usize,
    width: u8,
    count: usize,
    out: &mut Vec<u32>,
) -> Result<()> {
    if width == 0 {
        out.extend(std::iter::repeat_n(0, count));
        return Ok(());
    }
    let mut bit = bit_offset;
    for _ in 0..count {
        let mut value = 0u64;
        let mut taken = 0u8;
        while taken < width {
            let byte = *data.get(bit / 8).ok_or_else(truncated)?;
            let in_byte = (bit % 8) as u8;
            let available = 8 - in_byte;
            let want = (width - taken).min(available);
            let mask = if want == 8 { 0xffu8 } else { (1u8 << want) - 1 };
            value |= (((byte >> in_byte) & mask) as u64) << taken;
            taken += want;
            bit += want as usize;
        }
        out.push(value as u32);
    }
    Ok(())
}

/// The RLE / bit-packed hybrid: alternating runs of one repeated value and
/// groups of eight packed ones, each introduced by a varint that says which.
pub(super) struct Hybrid<'a> {
    data: &'a [u8],
    at: usize,
    width: u8,
    /// Values decoded from the current run but not yet handed out.
    buffer: Vec<u32>,
    taken: usize,
    /// A run of one repeated value, handed out without being written down:
    /// its length is the file's word, and one header can claim billions.
    run: u32,
    run_left: usize,
}

impl<'a> Hybrid<'a> {
    pub(super) fn new(data: &'a [u8], width: u8) -> Self {
        Hybrid {
            data,
            at: 0,
            width,
            buffer: Vec::new(),
            taken: 0,
            run: 0,
            run_left: 0,
        }
    }

    /// The next value, or an error if the page runs out before the rows do.
    pub(super) fn next(&mut self) -> Result<u32> {
        if self.run_left == 0 && self.taken == self.buffer.len() {
            self.buffer.clear();
            self.taken = 0;
            self.fill()?;
            if self.run_left == 0 && self.buffer.is_empty() {
                return Err(truncated());
            }
        }
        if self.run_left > 0 {
            self.run_left -= 1;
            return Ok(self.run);
        }
        let v = self.buffer[self.taken];
        self.taken += 1;
        Ok(v)
    }

    fn fill(&mut self) -> Result<()> {
        if self.at >= self.data.len() {
            return Ok(());
        }
        let header = varint(self.data, &mut self.at)?;
        if header & 1 == 1 {
            // A bit-packed run: the header counts groups of eight.
            let groups = (header >> 1) as usize;
            if self.width == 0 {
                // Zero-width values take no bytes: a run of zeros.
                self.run = 0;
                self.run_left = groups.saturating_mul(8);
                return Ok(());
            }
            // The run's bytes must be in the page, which bounds `groups`.
            let bytes = groups
                .checked_mul(self.width as usize)
                .ok_or_else(truncated)?;
            let from = self.at;
            let to = from.checked_add(bytes).ok_or_else(truncated)?;
            let slice = self.data.get(from..to).ok_or_else(truncated)?;
            self.at = to;
            unpack(slice, 0, self.width, groups * 8, &mut self.buffer)?;
        } else {
            // A run of one repeated value, written in whole bytes.
            let count = (header >> 1) as usize;
            let bytes = self.width.div_ceil(8) as usize;
            let slice = self
                .data
                .get(self.at..self.at + bytes)
                .ok_or_else(truncated)?;
            self.at += bytes;
            let mut value = 0u32;
            for (i, &b) in slice.iter().enumerate() {
                value |= (b as u32) << (8 * i);
            }
            self.run = value;
            self.run_left = count;
        }
        Ok(())
    }
}

/// Delta binary packed integers: a first value, then blocks of miniblocks, each
/// holding its deltas from the block's minimum in as few bits as they fit in.
///
/// Returns the values and how many bytes of `data` they took, because a
/// `DELTA_LENGTH_BYTE_ARRAY` page has its bytes immediately after them.
pub(super) fn delta_binary_packed(data: &[u8], want: usize) -> Result<(Vec<i64>, usize)> {
    let mut at = 0usize;
    let block_size = varint(data, &mut at)? as usize;
    let miniblocks = varint(data, &mut at)? as usize;
    let total = varint(data, &mut at)? as usize;
    let first = zigzag(varint(data, &mut at)?);
    if miniblocks == 0 || block_size == 0 || !block_size.is_multiple_of(miniblocks) {
        return Err(Error::new(
            "a delta-encoded Parquet page has a block size its miniblocks do not divide",
        ));
    }
    let per_miniblock = block_size / miniblocks;
    // Every block lists one width byte per miniblock, and a page holds no more
    // values than its header gives: two bounds on what the header may claim.
    if miniblocks > data.len() - at {
        return Err(truncated());
    }
    if total > want.max(1) {
        return Err(Error::new(
            "a delta-encoded Parquet page holds more values than its page says",
        ));
    }

    let mut out = crate::alloc::sized(total.max(1), "the values of a delta-encoded page")?;
    out.push(first);
    let mut value = first;
    let mut widths = vec![0u8; miniblocks];
    let mut scratch: Vec<u32> = Vec::new();

    while out.len() < total {
        let min_delta = zigzag(varint(data, &mut at)?);
        let header = data
            .get(at..at.saturating_add(miniblocks))
            .ok_or_else(truncated)?;
        widths.copy_from_slice(header);
        at += miniblocks;
        for &width in &widths {
            if out.len() >= total {
                break;
            }
            let bytes = per_miniblock
                .checked_mul(width as usize)
                .ok_or_else(truncated)?
                / 8;
            let slice = data
                .get(at..at.saturating_add(bytes))
                .ok_or_else(truncated)?;
            at += bytes;
            scratch.clear();
            // Only the values still wanted: a zero-width miniblock takes no
            // bytes however many values it claims.
            let need = per_miniblock.min(total - out.len());
            unpack(slice, 0, width, need, &mut scratch)?;
            for &d in &scratch {
                if out.len() >= total {
                    break;
                }
                value = value.wrapping_add(min_delta).wrapping_add(d as i64);
                out.push(value);
            }
        }
    }
    Ok((out, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_run_and_a_packed_group_read_the_same_way() {
        // A run of four 1s, then one group of eight values 0..7 at three bits.
        let mut data = vec![0x08, 0x01];
        data.push(0x03); // one group, bit-packed
        data.extend_from_slice(&[0b1000_1000, 0b1100_0110, 0b1111_1010]);
        let mut hybrid = Hybrid::new(&data, 3);
        let mut got = Vec::new();
        for _ in 0..12 {
            got.push(hybrid.next().expect("twelve values"));
        }
        assert_eq!(got, [1, 1, 1, 1, 0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_zero_bit_width_is_a_run_of_zeroes_rather_than_a_read() {
        let data = [0x10, 0x00];
        let mut hybrid = Hybrid::new(&data, 0);
        assert_eq!(hybrid.next().expect("a value"), 0);
    }

    #[test]
    fn running_off_the_end_is_an_error_rather_than_a_silent_zero() {
        let data = [0x04, 0x01];
        let mut hybrid = Hybrid::new(&data, 1);
        for _ in 0..2 {
            hybrid.next().expect("the two values that are there");
        }
        assert!(hybrid.next().is_err());
    }

    #[test]
    fn bit_widths_are_the_bits_a_value_needs() {
        assert_eq!(bit_width(0), 0);
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(7), 3);
        assert_eq!(bit_width(8), 4);
    }
}
