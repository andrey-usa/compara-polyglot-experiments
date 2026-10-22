//! A deterministic pair of CSV files for benchmarking and CI.
//!
//! Both files have 20 columns and share the composite key
//! `(account_id, txn_id)`. File B is file A with a controlled amount of drift,
//! so every run has a known answer:
//!
//! | Drift | Share of rows |
//! |---|---|
//! | `status` changed | 3.0% |
//! | `amount` changed | 1.5% |
//! | `balance` changed | 1.5% |
//! | `value_date` blanked | 0.3% |
//! | `updated_at` changed | 100% (excluded with `--ignore`) |
//! | rows only in B | 0.10% |
//! | rows only in A | 0.10% |
//! | duplicate keys | 0.01% per file |
//!
//! The recipe and the hash are the same as the Python, TypeScript, Java and Go
//! generators — the CSV files come out byte for byte identical — so a benchmark
//! number from any of them is directly comparable.
//!
//! The same rows are written as newline-delimited JSON and as Parquet on demand,
//! by [`Format`]. That matters for benchmarking the three readers against each
//! other: the payloads have to hold the same values, spelled the same way, or a
//! format comparison is measuring the converter. Writing all three here means no
//! part of the benchmark depends on DuckDB, pyarrow or a Python install.

pub mod parquet;

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::error::{Error, Result};

pub use parquet::Compression;

/// Which format the payload is written in. The values are identical in all
/// four; only the container changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    /// CSV with a tab between fields. No value contains a comma or a tab, so
    /// the rows are the CSV rows with the separator swapped.
    Tsv,
    Ndjson,
    Parquet,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Ndjson => "ndjson",
            Format::Parquet => "parquet",
        }
    }

    pub fn parse(name: &str) -> Result<Format> {
        match name.trim().to_ascii_lowercase().as_str() {
            "csv" => Ok(Format::Csv),
            "tsv" => Ok(Format::Tsv),
            "ndjson" | "json" => Ok(Format::Ndjson),
            "parquet" => Ok(Format::Parquet),
            other => Err(Error::new(format!(
                "unknown format: {other}. Choose one of csv, tsv, ndjson, parquet"
            ))),
        }
    }
}

/// The 20-column schema, in order.
pub const COLUMNS: [&str; 20] = [
    "account_id",
    "txn_id",
    "posting_date",
    "value_date",
    "currency",
    "amount",
    "fee",
    "balance",
    "status",
    "channel",
    "region",
    "branch_code",
    "product_code",
    "counterparty",
    "quantity",
    "rate",
    "category",
    "risk_flag",
    "note",
    "updated_at",
];

const STATUS: [&str; 4] = ["posted", "pending", "settled", "reversed"];
const CHANNEL: [&str; 5] = ["branch", "online", "mobile", "atm", "wire"];
const REGION: [&str; 4] = ["EMEA", "NA", "APAC", "LATAM"];
const CURRENCY: [&str; 4] = ["USD", "EUR", "GBP", "JPY"];
const CATEGORY: [&str; 5] = ["retail", "corporate", "treasury", "cards", "loans"];

// Drift buckets, against a 0..9999 hash bucket per row.
const CHG_STATUS: u64 = 300;
const CHG_AMOUNT: u64 = 150;
const CHG_BALANCE: u64 = 150;
const CHG_VALUE_DATE: u64 = 30;
const REMOVED_MOD: i64 = 1000;
const ADDED_RATIO: i64 = 1000;
const DUP_MOD: i64 = 10_000;

/// splitmix-style hash, matching the other implementations bit for bit.
fn hash(i: i64, salt: i64, seed: i64) -> u64 {
    let mut x = (i.wrapping_mul(31).wrapping_add(salt).wrapping_add(seed)) as u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

fn modulo(i: i64, salt: i64, seed: i64, m: u64) -> u64 {
    hash(i, salt, seed) % m
}

/// The 240 dates the generator draws from, starting at 2026-01-01.
fn days() -> Vec<String> {
    // Only 2026 is covered, so a plain civil-date walk is enough and pulls in no
    // calendar library.
    const MONTH_LENGTHS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut out = Vec::with_capacity(240);
    let (mut month, mut day) = (1u32, 1u32);
    for _ in 0..240 {
        out.push(format!("2026-{month:02}-{day:02}"));
        day += 1;
        if day > MONTH_LENGTHS[(month - 1) as usize] {
            day = 1;
            month += 1;
        }
    }
    out
}

/// One row's twenty values, laid end to end in one buffer with where each stops.
///
/// The values are built once and written out in whichever format was asked for,
/// so the three payloads cannot drift apart: a format that held different bytes
/// would make the format comparison measure the generator instead of the reader.
pub struct Row {
    buf: String,
    ends: Vec<u32>,
}

impl Row {
    fn new() -> Row {
        Row {
            buf: String::with_capacity(256),
            ends: Vec::with_capacity(COLUMNS.len()),
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.ends.clear();
    }

    /// Ends the value being built. What a CSV writer spells as a comma.
    fn end(&mut self) {
        self.ends.push(self.buf.len() as u32);
    }

    fn push(&mut self, text: &str) {
        self.buf.push_str(text);
    }

    fn pad(&mut self, v: i64, width: usize) {
        let _ = write!(self.buf, "{v:0width$}");
    }

    fn number(&mut self, v: u64) {
        let _ = write!(self.buf, "{v}");
    }

    /// An amount held in cents, as a two-decimal number.
    fn money(&mut self, cents: i64) {
        if cents < 0 {
            self.buf.push('-');
        }
        let a = cents.unsigned_abs();
        let _ = write!(self.buf, "{}.{:02}", a / 100, a % 100);
    }

    pub fn cell(&self, i: usize) -> &str {
        let from = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.buf[from..self.ends[i] as usize]
    }

    pub fn cells(&self) -> impl Iterator<Item = &str> {
        (0..self.ends.len()).map(|i| self.cell(i))
    }

    fn write_delimited(&self, delimiter: u8, out: &mut Vec<u8>) {
        for i in 0..self.ends.len() {
            if i > 0 {
                out.push(delimiter);
            }
            out.extend_from_slice(self.cell(i).as_bytes());
        }
        out.push(b'\n');
    }

    /// One JSON object per line, keys in schema order.
    fn write_json(&self, out: &mut Vec<u8>) {
        out.push(b'{');
        for (i, name) in COLUMNS.iter().enumerate().take(self.ends.len()) {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'"');
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b"\":\"");
            write_json_string(self.cell(i), out);
            out.push(b'"');
        }
        out.extend_from_slice(b"}\n");
    }
}

/// Escapes what JSON requires escaping. These values never need it, so the
/// common case is one copy; the branch is here because a generator that writes
/// invalid JSON for an unusual value would be found much later.
fn write_json_string(value: &str, out: &mut Vec<u8>) {
    let plain = value.bytes().all(|b| b >= 0x20 && b != b'"' && b != b'\\');
    if plain {
        out.extend_from_slice(value.as_bytes());
        return;
    }
    for b in value.bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1f => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            other => out.push(other),
        }
    }
}

/// Builds one row of one side.
///
/// Money is carried in integer cents and the drift is applied to those integers,
/// never to a float. Every implementation of this generator then produces the
/// same digits without depending on its language's rounding rule — which is
/// what makes the five sets of files byte-identical.
fn row(out: &mut Row, i: i64, b_side: bool, seed: i64, days: &[String]) {
    let bucket = modulo(i, 0, seed, 10_000);
    let mut amount_cents = modulo(i, 21, seed, 900_000_000) as i64 - 100_000_000;
    let mut balance_cents = modulo(i, 31, seed, 2_000_000_000) as i64;
    let mut status = STATUS[modulo(i, 11, seed, STATUS.len() as u64) as usize];
    let mut value_date = days[modulo(i, 41, seed, 240) as usize].as_str();

    if b_side {
        if bucket < CHG_STATUS {
            let n = STATUS.len() as u64;
            status = STATUS[((modulo(i, 11, seed, n) + 1) % n) as usize];
        } else if bucket < CHG_STATUS + CHG_AMOUNT {
            amount_cents += 1234;
        } else if bucket < CHG_STATUS + CHG_AMOUNT + CHG_BALANCE {
            // +1%, rounded half up, in cents.
            balance_cents = (balance_cents * 101 + 50) / 100;
        }
        if bucket < CHG_VALUE_DATE {
            value_date = "";
        }
    }

    out.clear();
    out.push("ACC-");
    out.pad((i * 7919) % 250_000, 8);
    out.end();
    out.push("TXN-");
    out.pad(i, 11);
    out.end();
    out.push(&days[modulo(i, 1, seed, 240) as usize]);
    out.end();
    out.push(value_date);
    out.end();
    out.push(CURRENCY[modulo(i, 51, seed, 4) as usize]);
    out.end();
    out.money(amount_cents);
    out.end();
    out.money(modulo(i, 61, seed, 5000) as i64);
    out.end();
    out.money(balance_cents);
    out.end();
    out.push(status);
    out.end();
    out.push(CHANNEL[modulo(i, 71, seed, 5) as usize]);
    out.end();
    out.push(REGION[modulo(i, 81, seed, 4) as usize]);
    out.end();
    out.push("BR");
    out.pad(modulo(i, 91, seed, 900) as i64 + 100, 4);
    out.end();
    out.push("P");
    out.pad(modulo(i, 101, seed, 5000) as i64, 5);
    out.end();
    out.push("CP-");
    out.pad(modulo(i, 111, seed, 90_000) as i64, 6);
    out.end();
    out.number(modulo(i, 121, seed, 500) + 1);
    out.end();
    out.push("0.");
    out.pad(modulo(i, 131, seed, 1200) as i64, 4);
    out.end();
    out.push(CATEGORY[modulo(i, 141, seed, 5) as usize]);
    out.end();
    out.push(if modulo(i, 151, seed, 20) == 0 {
        "Y"
    } else {
        "N"
    });
    out.end();
    let _ = write!(out.buf, "batch {} line {}", i % 997 + 1, i % 53 + 1);
    out.end();
    out.push(if b_side {
        "2026-09-01 02:15:00"
    } else {
        "2026-08-01 02:15:00"
    });
    out.end();
}

/// Where one side's rows are written, in whichever format was asked for.
enum Sink {
    /// CSV, TSV or newline-delimited JSON: text, buffered a megabyte at a time.
    Text {
        file: BufWriter<File>,
        buf: Vec<u8>,
        /// The field separator, or `None` for JSON.
        delimiter: Option<u8>,
    },
    Parquet(parquet::Writer),
}

/// How the Parquet files are laid out. The defaults are what the benchmarks
/// use; the tests narrow them to reach the reader's less common paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParquetLayout {
    pub compression: Compression,
    /// Rows per row group. A million: large enough that the dictionaries are
    /// worth having and the footer stays small, small enough that the writer
    /// holds a bounded amount of the file at once.
    pub row_group_rows: usize,
    /// Distinct values a column chunk may hold before it is written plain.
    pub dictionary_limit: usize,
}

impl Default for ParquetLayout {
    fn default() -> Self {
        ParquetLayout {
            compression: Compression::None,
            row_group_rows: 1_000_000,
            dictionary_limit: parquet::DICTIONARY_LIMIT,
        }
    }
}

impl Sink {
    fn create(path: &Path, format: Format, layout: ParquetLayout) -> Result<Sink> {
        Ok(match format {
            Format::Parquet => Sink::Parquet(
                parquet::Writer::create(path, &COLUMNS, layout.compression, layout.row_group_rows)?
                    .with_dictionary_limit(layout.dictionary_limit),
            ),
            Format::Csv | Format::Tsv | Format::Ndjson => {
                let mut file = BufWriter::with_capacity(1 << 20, File::create(path)?);
                let delimiter = match format {
                    Format::Csv => Some(b','),
                    Format::Tsv => Some(b'\t'),
                    _ => None,
                };
                // JSON carries its names on every record, so it has no header.
                if let Some(d) = delimiter {
                    writeln!(file, "{}", COLUMNS.join(&(d as char).to_string()))?;
                }
                Sink::Text {
                    file,
                    buf: Vec::with_capacity(1 << 20),
                    delimiter,
                }
            }
        })
    }

    fn row(&mut self, row: &Row) -> Result<()> {
        match self {
            Sink::Text {
                file,
                buf,
                delimiter,
            } => {
                buf.clear();
                match delimiter {
                    Some(d) => row.write_delimited(*d, buf),
                    None => row.write_json(buf),
                }
                file.write_all(buf)?;
                Ok(())
            }
            Sink::Parquet(writer) => writer.write_row(row.cells()),
        }
    }

    fn finish(self) -> Result<()> {
        match self {
            Sink::Text { mut file, .. } => {
                file.flush()?;
                Ok(())
            }
            Sink::Parquet(writer) => writer.finish(),
        }
    }
}

/// Writes both files in one pass, in the given format.
pub fn generate_as(
    rows: i64,
    a_path: &Path,
    b_path: &Path,
    seed: i64,
    format: Format,
    compression: Compression,
) -> Result<()> {
    let layout = ParquetLayout {
        compression,
        ..ParquetLayout::default()
    };
    generate_with(rows, a_path, b_path, seed, format, layout)
}

/// [`generate_as`] with every Parquet layout setting exposed.
pub fn generate_with(
    rows: i64,
    a_path: &Path,
    b_path: &Path,
    seed: i64,
    format: Format,
    layout: ParquetLayout,
) -> Result<()> {
    let days = days();
    let mut fa = Sink::create(a_path, format, layout)?;
    let mut fb = Sink::create(b_path, format, layout)?;

    let dup_extra = (rows / DUP_MOD).max(1);
    let added = (rows / ADDED_RATIO).max(1);

    let mut line = Row::new();
    for i in 0..rows {
        row(&mut line, i, false, seed, &days);
        fa.row(&line)?;

        if i % REMOVED_MOD != 7 {
            row(&mut line, i, true, seed, &days);
            fb.row(&line)?;
        }
        if i % DUP_MOD == 3 && i < rows / 2 {
            row(&mut line, i, true, seed, &days);
            fb.row(&line)?;
        }
    }
    for i in 0..dup_extra {
        row(&mut line, i, false, seed, &days);
        fa.row(&line)?;
    }
    for i in rows..rows + added {
        row(&mut line, i, true, seed, &days);
        fb.row(&line)?;
    }
    fa.finish()?;
    fb.finish()?;
    Ok(())
}

/// Writes both files as CSV, which is what every other generator here emits and
/// what the parity check compares byte for byte.
pub fn generate(rows: i64, a_path: &Path, b_path: &Path, seed: i64) -> Result<()> {
    generate_as(rows, a_path, b_path, seed, Format::Csv, Compression::None)
}

/// Parses `10000`, `10k`, `1m`, `2.5M`.
pub fn parse_rows(s: &str) -> Result<i64> {
    let t: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| *c != '_' && *c != ',')
        .collect();
    let bad = || Error::new(format!("--rows must be a number, got: {s}"));
    let last = t.chars().last().ok_or_else(bad)?;
    let multiplier: i64 = match last {
        'k' => 1_000,
        'm' => 1_000_000,
        'g' => 1_000_000_000,
        _ => return t.parse().map_err(|_| bad()),
    };
    let value: f64 = t[..t.len() - 1].parse().map_err(|_| bad())?;
    Ok((value * multiplier as f64).round() as i64)
}
