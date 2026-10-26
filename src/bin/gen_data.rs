//! Writes a deterministic pair of files for benchmarking and CI.
//!
//! The recipe lives in [`compara::gendata`]; this is only the command line. The
//! same rows go out as CSV, TSV, newline-delimited JSON or Parquet, so a format
//! benchmark compares readers rather than converters.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use compara::gendata::{COLUMNS, Compression, Format, ParquetLayout, generate_with, parse_rows};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

const USAGE: &str = "\
usage: compara-gen-data [options]

  -n, --rows N          rows a side: 10000, 10k, 1m, 2.5m (default 10k)
  -o, --out-dir DIR     where the pair is written, created if missing (default data)
      --prefix P        filename stem (default: the --rows text, so 50m_a.csv)
      --seed N          the recipe's seed; the same seed writes the same bytes (default 7)
  -f, --format F        csv | tsv | ndjson | parquet (default csv)
      --compression C   none | zstd, Parquet only (default none)
      --row-group-size N  rows per Parquet row group (default 1000000)
      --dict-limit N    distinct values a Parquet column chunk may hold before
                        it is written plain (default 131072)
  -h, --help            this

Writes <prefix>_a.<ext> and <prefix>_b.<ext>. Fifty million rows of CSV is
about 9.2 GB a side, so check the disk before asking for one.";

/// One `--flag value` or `--flag=value`, whichever was written.
///
/// This used to be a loop that skipped anything it did not recognise, so
/// `--rows=50m` -- the spelling every GNU tool takes -- was dropped on the
/// floor and the run silently produced the 10k default. It printed
/// `rust: 10000 rows` and nothing about the flag it ignored. A generator that
/// quietly writes a different dataset than the one asked for is the same class
/// of fault as `--ignore` matching nothing in silence: the run completes, the
/// numbers are wrong, and nothing says so. Unknown flags are an error now, and
/// the C generator has always rejected them.
fn run() -> compara::Result<()> {
    let mut rows_arg = "10k".to_string();
    let mut out_dir = "data".to_string();
    let mut prefix: Option<String> = None;
    let mut seed: i64 = 7;
    let mut format = Format::Csv;
    let mut layout = ParquetLayout::default();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].clone();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }

        // Every remaining flag takes a value, from `=` or the next argument.
        let mut take = || -> compara::Result<String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            let v = argv
                .get(i + 1)
                .cloned()
                .ok_or_else(|| compara::Error::new(format!("{flag} needs a value\n\n{USAGE}")))?;
            // A value that is itself a flag means the one before it was left
            // empty. Saying so beats parsing `--out-dir` as a row count, which
            // is what the old loop did.
            if v.starts_with('-')
                && v.len() > 1
                && !v[1..].starts_with(|c: char| c.is_ascii_digit())
            {
                return Err(compara::Error::new(format!(
                    "{flag} needs a value, got the next flag: {v}\n\n{USAGE}"
                )));
            }
            i += 1;
            Ok(v)
        };

        match flag.as_str() {
            "--rows" | "-n" => rows_arg = take()?,
            "--out-dir" | "-o" => out_dir = take()?,
            "--prefix" => prefix = Some(take()?),
            "--seed" => {
                let v = take()?;
                seed = v.parse().map_err(|_| {
                    compara::Error::new(format!("--seed must be a number, got: {v}"))
                })?;
            }
            "--format" | "-f" => format = Format::parse(&take()?)?,
            "--row-group-size" | "--dict-limit" => {
                let v = take()?;
                let n: usize = v.parse().ok().filter(|&n| n > 0).ok_or_else(|| {
                    compara::Error::new(format!("{flag} must be a positive number, got: {v}"))
                })?;
                if flag == "--row-group-size" {
                    layout.row_group_rows = n;
                } else {
                    layout.dictionary_limit = n;
                }
            }
            "--compression" => {
                let v = take()?;
                layout.compression = match v.trim().to_ascii_lowercase().as_str() {
                    "none" | "uncompressed" => Compression::None,
                    "zstd" => Compression::Zstd,
                    other => {
                        return Err(compara::Error::new(format!(
                            "unknown compression: {other}. Choose none or zstd"
                        )));
                    }
                }
            }
            other => {
                return Err(compara::Error::new(format!(
                    "unknown option: {other}\n\n{USAGE}"
                )));
            }
        }
        i += 1;
    }

    let rows = parse_rows(&rows_arg)?;
    let dir = PathBuf::from(&out_dir);
    std::fs::create_dir_all(&dir)?;
    let label = prefix.unwrap_or_else(|| rows_arg.to_lowercase());
    let a = dir.join(format!("{label}_a.{}", format.extension()));
    let b = dir.join(format!("{label}_b.{}", format.extension()));

    let start = Instant::now();
    generate_with(rows, &a, &b, seed, format, layout)?;
    println!(
        "rust: {rows} rows x {} columns in {:.1}s",
        COLUMNS.len(),
        start.elapsed().as_secs_f64()
    );
    for path in [&a, &b] {
        if let Ok(meta) = std::fs::metadata(path) {
            println!("  {}  {:.1} MB", path.display(), meta.len() as f64 / 1e6);
        }
    }
    Ok(())
}
