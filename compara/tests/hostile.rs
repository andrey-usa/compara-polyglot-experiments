//! Files built to break a reader, not to be compared.
//!
//! compara decodes Parquet itself, so every length, count and offset in a file
//! is the file's word until checked. Each case here is one way that word was
//! trusted -- found by mutating the fixtures in `formats/` and `fold/`, or
//! written by hand from the format -- and each must end the run with exit 2
//! and a message, quickly: not a panic (101), not an abort, not a hang, and not
//! gigabytes of memory on the way to a refusal.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_compara");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A file of this test's own, removed when it goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, bytes: &[u8]) -> Scratch {
        let path =
            std::env::temp_dir().join(format!("compara-hostile-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write a scratch file");
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs compara and insists on a refusal within `limit`.
fn refused(args: &[&Path], extra: &[&str], limit: Duration) -> String {
    let mut child = Command::new(BIN)
        .args(args)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run compara");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for compara") {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("compara {args:?} {extra:?} was still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut stderr)
        .expect("read stderr");
    assert_eq!(
        status.code(),
        Some(2),
        "compara {args:?} {extra:?}: {status}, stderr: {stderr}"
    );
    assert!(stderr.starts_with("error: "), "no message: {stderr}");
    assert!(
        !stderr.contains("internal:"),
        "a panic, reported as an error: {stderr}"
    );
    stderr
}

/// Every way in that reads rows: the first rows, and both compare paths with
/// the file on either side.
fn refused_reading(file: &Path) {
    let mate = fixture("formats/b_dict_snappy.parquet");
    let limit = Duration::from_secs(20);
    refused(&[Path::new("head"), file], &[], limit);
    for engine in ["auto", "turbo"] {
        refused(
            &[Path::new("compare"), file, &mate],
            &["-k", "id", "--summary", "--engine", engine],
            limit,
        );
        refused(
            &[Path::new("compare"), &mate, file],
            &["-k", "id", "--summary", "--engine", engine],
            limit,
        );
    }
}

/// `refused_reading`, and `columns` too: for a file whose footer is the problem.
fn refused_everywhere(file: &Path) {
    refused(&[Path::new("columns"), file], &[], Duration::from_secs(20));
    refused_reading(file);
}

fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

/// A Parquet file of nothing but this footer.
fn with_footer(meta: &[u8]) -> Vec<u8> {
    let mut out = b"PAR1".to_vec();
    out.extend_from_slice(meta);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(b"PAR1");
    out
}

#[test]
fn a_footer_nested_past_any_writer_does_not_overflow_the_stack() {
    // Field 10 of the file metadata, unknown to the reader, holding a struct
    // in a struct, 200,000 deep. Skipping it recursed once per level.
    let depth = 200_000;
    let mut meta = vec![0xAC];
    meta.extend(std::iter::repeat_n(0x1C, depth));
    meta.extend(std::iter::repeat_n(0x00, depth + 1));
    let file = Scratch::new("deep.parquet", &with_footer(&meta));
    refused_everywhere(&file.0);
}

#[test]
fn a_list_of_bools_cannot_spin_without_reading() {
    // A thousand lists of two billion bools each. A bool in a list takes a
    // byte, but the skip took none, so this looped for hours on a few bytes.
    let mut meta = vec![0xA9, 0xF9];
    meta.extend(varint(1000));
    for _ in 0..1000 {
        meta.push(0xF1);
        meta.extend(varint(1 << 31));
    }
    meta.push(0x00);
    let file = Scratch::new("bools.parquet", &with_footer(&meta));
    refused_everywhere(&file.0);
}

#[test]
fn a_row_group_count_the_footer_cannot_hold_sizes_nothing() {
    // version 1, then field 4 (row groups): a list of 2^32 - 1 structs, in a
    // 14-byte footer. The columnar reader allocated 100 GB for it and aborted.
    let mut meta = vec![0x15, 0x02, 0x39, 0xFC];
    meta.extend(varint(0xFFFF_FFFF));
    meta.push(0x00);
    let file = Scratch::new("groups.parquet", &with_footer(&meta));
    refused_everywhere(&file.0);
}

#[test]
fn a_string_length_that_wraps_the_cursor_is_refused() {
    let mut meta = vec![0xA8];
    meta.extend(varint(u64::MAX - 1));
    meta.push(0x00);
    let file = Scratch::new("wrap.parquet", &with_footer(&meta));
    refused_everywhere(&file.0);
}

#[test]
fn a_negative_page_size_does_not_wrap_the_bounds_check() {
    // A mutant of b_dict_snappy.parquet: a page header whose compressed size
    // is negative, which cast to usize wrapped past the check that the page
    // fits in the file.
    let mate = fixture("formats/a_dict_snappy.parquet");
    let file = fixture("hostile/negative_page_size.parquet");
    refused(
        &[Path::new("compare"), &file, &mate],
        &["-k", "id", "--summary"],
        Duration::from_secs(20),
    );
}

#[test]
fn a_delta_header_claiming_billions_of_values_sizes_nothing() {
    // A mutant of a_delta_v2.parquet whose DELTA_BINARY_PACKED header asks
    // for a miniblock of 10^16 values: the reader reserved it and aborted.
    refused_reading(&fixture("hostile/delta_header.parquet"));
}

#[test]
fn a_zstd_page_cannot_expand_past_its_header() {
    // One page of 36 KB that its header says decompresses to 36 KB, and whose
    // zstd frame decompresses to 256 MB of zeros. The reader decompressed all
    // of it before comparing the length with the header's.
    let stderr = refused(
        &[Path::new("head"), &fixture("hostile/zstd_bomb.parquet")],
        &[],
        Duration::from_secs(20),
    );
    assert!(
        !stderr.contains("268435456"),
        "the whole bomb was decompressed: {stderr}"
    );
}

#[test]
fn a_gzip_bomb_is_refused_before_it_fills_the_disk() {
    // 18 gzip members of 64 MiB of zeros each: 1.1 MiB that expands to 1.1 GiB,
    // a thousand times and more. It was expanded whole before reading.
    use std::io::Write;
    let mut member = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![0u8; 1 << 20];
    for _ in 0..64 {
        member.write_all(&zeros).unwrap();
    }
    let member = member.finish().unwrap();
    let mut bomb = Vec::new();
    for _ in 0..18 {
        bomb.extend_from_slice(&member);
    }
    let file = Scratch::new("bomb.csv.gz", &bomb);
    let stderr = refused(&[Path::new("head"), &file.0], &[], Duration::from_secs(60));
    assert!(stderr.contains("decompression bomb"), "{stderr}");
}

#[test]
fn a_bit_width_wider_than_its_values_is_refused() {
    // Found by fuzzing: a page whose packed values claim 110 bits each. The
    // shift past 64 panicked in a debug build and, in a release one, read
    // garbage; a delta page's widths past 32 also lost their high bits.
    let file = fixture("hostile/bit_width_overflow.parquet");
    refused_reading(&file);
    let stderr = refused(
        &[Path::new("compare"), &file, &file],
        &["-k", "id", "--summary", "--engine", "turbo"],
        Duration::from_secs(20),
    );
    assert!(stderr.contains("bits wide"), "{stderr}");
}
