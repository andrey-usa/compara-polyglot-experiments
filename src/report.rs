//! The self-contained HTML report: one file, no network, no fonts, no frameworks.
//!
//! Only differing cells are embedded, and the payload is gzip then base64, which
//! the browser decodes natively with `DecompressionStream`. `--no-compress`
//! writes plain JSON for anything older than about 2023.
//!
//! The template is byte for byte the one the Python, TypeScript, Java and Go
//! implementations use, embedded here so the binary is the only thing to ship.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::write::GzEncoder;
use flate2::{Compress, Compression, Crc, FlushCompress, Status};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::contract::CompareResult;
use crate::error::{Error, Result};

const TEMPLATE: &str = include_str!("report.html");

/// A `Write` that appends to a `Vec<u8>` through the checked allocator.
///
/// `serde_json` and the gzip encoder both write into a buffer that grows, and
/// both grow it with `Vec`, which aborts when there is no room. Neither offers
/// a fallible allocation path -- but both write through `io::Write`, and the
/// buffer on the far side of that is ours.
///
/// The refusal is carried out in `failed` rather than folded into the
/// `io::Error`, because the `io::Error` loses what the port wants to say: which
/// structure did not fit and how big it was. The serialiser's own error is
/// discarded once `failed` is set -- it will be a write error describing the
/// symptom, and the cause is here.
struct Checked<'a> {
    out: &'a mut Vec<u8>,
    what: &'static str,
    failed: Option<Error>,
}

impl<'a> Checked<'a> {
    fn new(out: &'a mut Vec<u8>, what: &'static str) -> Self {
        Checked {
            out,
            what,
            failed: None,
        }
    }
}

impl Write for Checked<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Err(e) = crate::alloc::room(self.out, buf.len(), self.what) {
            self.failed = Some(e);
            return Err(std::io::Error::other("out of memory"));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The result as JSON, in a buffer that can refuse instead of aborting.
fn payload_json(result: &CompareResult) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut writer = Checked::new(&mut buf, "the report payload");
    let wrote = serde_json::to_writer(&mut writer, result);
    let refused = writer.failed.take();
    if let Some(e) = refused {
        return Err(e);
    }
    wrote?;
    Ok(buf)
}

/// Renders the report.
///
/// Pass `compress = false` to embed plain JSON instead of the gzip payload.
pub fn render(result: &CompareResult, compress: bool) -> Result<String> {
    let raw = payload_json(result)?;

    let (payload, mode) = if compress {
        // Level 6, not 9: on the largest report this cap allows, level 9 buys
        // 1.3% of file size for 121% more compression time. The size invariant
        // this project cares about is that only differing cells are embedded,
        // which is what keeps the payload near a megabyte at all.
        (base64(&gzip(&raw)?)?, "gzip")
    } else {
        // The payload sits inside a <script> element, so a literal "</" would end
        // it early.
        let text =
            std::str::from_utf8(&raw).map_err(|_| Error::new("the report payload is not UTF-8"))?;
        (escaped_script_text(text)?, "json")
    };
    drop(raw);

    let title = format!("{} vs {}", result.meta.a.name, result.meta.b.name);
    let shell = TEMPLATE
        .replace("__TITLE__", &escape_html(&title))
        .replace("__MODE__", mode);
    // The payload is the whole of the file's size, so the last substitution is
    // the one that has to be sized rather than discovered: `String::replace`
    // would grow its buffer by doubling and abort on the growth that did not
    // fit.
    let at = shell
        .find("__PAYLOAD__")
        .ok_or_else(|| Error::new("the report template has no payload slot"))?;
    let need = shell.len() - "__PAYLOAD__".len() + payload.len();
    let mut out: Vec<u8> = crate::alloc::sized(need, "the report")?;
    out.extend_from_slice(&shell.as_bytes()[..at]);
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(&shell.as_bytes()[at + "__PAYLOAD__".len()..]);
    // Three `&str` concatenated, so UTF-8 by construction.
    String::from_utf8(out).map_err(|_| Error::new("the report is not UTF-8"))
}

/// `BASE64.encode` that refuses instead of aborting.
fn base64(data: &[u8]) -> Result<String> {
    let n = base64::encoded_len(data.len(), true)
        .ok_or_else(|| Error::new("the report is too large to encode"))?;
    let mut buf = crate::alloc::filled(0u8, n, "the report payload, base64")?;
    let wrote = BASE64
        .encode_slice(data, &mut buf)
        .map_err(|e| Error::new(format!("cannot encode the report: {e}")))?;
    buf.truncate(wrote);
    // base64 is ASCII by construction.
    String::from_utf8(buf).map_err(|_| Error::new("the base64 payload is not UTF-8"))
}

/// JSON made safe to sit inside a `<script>` element: every `<` written as
/// `\u003c`, which JSON reads back as the same character. Escaping only `</`
/// was not enough: a value holding `<!--<script>` puts the HTML parser in the
/// script element's "double escaped" state, where the element's own `</script>`
/// no longer ends it and the report's code is swallowed as data. In JSON a `<`
/// can only stand inside a string, so the rewrite is always valid. Refuses
/// rather than aborts when the copy does not fit.
fn escaped_script_text(s: &str) -> Result<String> {
    // Five extra bytes per `<`; one reserve for all of it rather than a
    // doubling walk.
    let lt = s.bytes().filter(|&b| b == b'<').count();
    let mut out: Vec<u8> = crate::alloc::sized(s.len() + 5 * lt, "the report payload")?;
    for part in s.as_bytes().split_inclusive(|&b| b == b'<') {
        match part.split_last() {
            Some((b'<', head)) => {
                out.extend_from_slice(head);
                out.extend_from_slice(b"\\u003c");
            }
            _ => out.extend_from_slice(part),
        }
    }
    String::from_utf8(out).map_err(|_| Error::new("the report payload is not UTF-8"))
}

/// Deflate blocks, a quarter of a megabyte of payload each.
///
/// Sized by the payload rather than by the thread count on purpose: the same
/// input then produces the same bytes on any machine, which a block per core
/// would not. A quarter rather than a whole megabyte so that the blocks share
/// out evenly: the six-megabyte payload of a fifty-thousand-row report was six
/// blocks over four cores, two rounds of which the second was half idle --
/// 65ms of gzip where a quarter megabyte takes 45ms, for 1% more bytes.
const BLOCK: usize = 1 << 18;

/// Gzips `raw` on every core.
///
/// This used to be described here as the one part of a run that cannot be spread
/// across cores. It can. A gzip stream is a header, a run of deflate blocks and a
/// trailer, and deflate blocks are independent of each other as long as each
/// starts with an empty window -- so the payload is cut into pieces, each piece
/// is deflated on its own thread ending in a full flush, which byte-aligns the
/// output and resets the window, and the pieces are concatenated under one
/// header. `pigz` has done it this way for years.
///
/// One member, not several concatenated ones. Multi-member gzip is legal and
/// `gunzip` reads it, but this payload is decoded by the browser's
/// `DecompressionStream`, and a single deflate stream is the shape every
/// decoder has always handled.
///
/// The cost is one reset window per megabyte, which is under a tenth of a
/// percent of the file; below one block there is nothing to split and the
/// original single-encoder path is used unchanged.
fn gzip(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() <= BLOCK {
        // The encoder's output buffer is ours, so it grows through `alloc` like
        // everything else the port holds; what the encoder keeps for itself is
        // the window and is bounded.
        let mut out: Vec<u8> = crate::alloc::sized(raw.len() / 2 + 64, "the compressed report")?;
        let mut refused: Option<Error> = None;
        {
            let writer = Checked::new(&mut out, "the compressed report");
            let mut encoder = GzEncoder::new(writer, Compression::new(6));
            let wrote = encoder.write_all(raw).and_then(|()| encoder.try_finish());
            refused = encoder.get_mut().failed.take().or(refused);
            if refused.is_none() {
                wrote?;
            }
        }
        return match refused {
            Some(e) => Err(e),
            None => Ok(out),
        };
    }

    let blocks: Vec<&[u8]> = raw.chunks(BLOCK).collect();
    let last = blocks.len() - 1;
    let next = AtomicUsize::new(0);
    let take = || -> Result<Vec<(usize, Vec<u8>, Crc)>> {
        let mut mine = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i > last {
                return Ok(mine);
            }
            mine.push((i, deflate(blocks[i], i == last)?, crc_of(blocks[i])));
        }
    };

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, blocks.len());
    let mut pieces = std::thread::scope(|scope| -> Result<Vec<(usize, Vec<u8>, Crc)>> {
        let handles: Vec<_> = (1..threads)
            .map(|_| crate::parallel::spawn(scope, &take))
            .collect();
        let mut all = take()?;
        for handle in handles {
            all.extend(
                handle
                    .join()
                    .map_err(|_| Error::new("a gzip worker panicked"))??,
            );
        }
        Ok(all)
    })?;
    pieces.sort_by_key(|(i, _, _)| *i);

    // ID1, ID2, deflate, no flags, no mtime, no extra flags, unknown OS.
    let mut out = crate::alloc::sized(raw.len() / 2 + 64, "the compressed report")?;
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff]);
    let mut crc = Crc::new();
    for (_, data, part) in &pieces {
        out.extend_from_slice(data);
        crc.combine(part);
    }
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    Ok(out)
}

fn crc_of(data: &[u8]) -> Crc {
    let mut crc = Crc::new();
    crc.update(data);
    crc
}

/// One block as raw deflate. `last` sets the final-block bit; anything else ends
/// in a full flush, which is what makes the next block independent of this one.
fn deflate(data: &[u8], last: bool) -> Result<Vec<u8>> {
    let mut c = Compress::new(Compression::new(6), false);
    let mut out = crate::alloc::sized(data.len() / 2 + 1024, "one compressed block")?;
    let mut at = 0;
    while at < data.len() {
        if out.len() == out.capacity() {
            crate::alloc::room(&mut out, 1 << 16, "one compressed block")?;
        }
        let before = c.total_in();
        c.compress_vec(&data[at..], &mut out, FlushCompress::None)
            .map_err(|e| Error::new(format!("cannot compress the report: {e}")))?;
        let moved = (c.total_in() - before) as usize;
        if moved == 0 && out.len() < out.capacity() {
            return Err(Error::new("the compressor stopped taking input"));
        }
        at += moved;
    }
    let flush = if last {
        FlushCompress::Finish
    } else {
        FlushCompress::Full
    };
    // Called until it leaves room in the output buffer, which is how zlib says a
    // flush is finished. Not "until it stops producing bytes": a full flush emits
    // its marker every time it is asked, so that test never comes true.
    loop {
        if out.len() == out.capacity() {
            crate::alloc::room(&mut out, 1 << 16, "one compressed block")?;
        }
        let room = out.capacity() - out.len();
        let before = c.total_out();
        let status = c
            .compress_vec(&[], &mut out, flush)
            .map_err(|e| Error::new(format!("cannot compress the report: {e}")))?;
        if status == Status::StreamEnd || (c.total_out() - before) < room as u64 {
            return Ok(out);
        }
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod gzip_tests {
    use super::*;
    use std::io::Read;

    /// Compressible, but not so compressible that the blocks come out empty.
    fn payload(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 16);
        let mut x: u32 = 12345;
        while v.len() < n {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            v.extend_from_slice(format!("{}-{},", x % 1000, v.len() % 7).as_bytes());
        }
        v.truncate(n);
        v
    }

    fn roundtrip(n: usize) {
        let raw = payload(n);
        let gz = gzip(&raw).expect("gzip");
        let mut got = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut got)
            .expect("gunzip");
        assert_eq!(got.len(), raw.len(), "length at {n}");
        assert_eq!(got, raw, "bytes at {n}");
    }

    #[test]
    fn one_block_takes_the_single_encoder_path() {
        roundtrip(1 << 10);
        roundtrip(BLOCK);
    }

    #[test]
    fn several_blocks_make_one_stream_a_plain_decoder_reads() {
        roundtrip(BLOCK + 1);
        roundtrip(BLOCK * 3 + 17);
    }
}
