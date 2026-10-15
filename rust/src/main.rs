//! The command line.
//!
//! ```text
//! compara compare A.csv B.csv --key id,region [--compare c1,c2] [--out report.html]
//! compara compare A.csv B.csv --profile orders
//! ```
//!
//! Exit codes: 0 identical, 1 differences found, 2 error, 3 duplicate keys (only
//! with `--fail-on-dups`). That makes it a drop-in CI or pipeline gate.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compara::error::{Error, Result};
use compara::{Options, compare, parse_list, profiles, render};

const USAGE: &str = "\
usage: compara <command> [options]

commands:
  compare A B     Compare two tables on a composite key: print the counts, and with -o
                  write an HTML report
  columns FILE    Print one file's column names, one per line
  head FILE       Print the first rows of one file (default 10)
  version         Print the version (also --version)

columns and head read CSV, newline-delimited JSON and Parquet, and neither reads
more of the file than it has to: `columns` on Parquet reads the footer, and
`head` stops at the first page. Both take --delimiter and --encoding.

head options:
  -n, --rows N            Rows to show (default 10)
      --csv               Print as CSV instead of an aligned table

compare options:
  -k, --key COLS          Comma-separated key column(s) (or from --profile)
  -c, --compare COLS      Columns to compare (default: all common non-key columns)
  -i, --ignore COLS       Columns to skip
  -p, --profile NAME      Profile name from compara.toml
      --config PATH       Path to compara.toml
      --trim              Strip whitespace before comparing
      --ignore-case
      --empty-is-null     Treat empty string and null as equal
      --tolerance N       Absolute numeric tolerance
      --max-rows N        Rows embedded per section (default 50000)
      --delimiter D       Force delimiter (default: auto)
      --encoding ENC
      --engine E          auto | turbo | sortmerge | native
      --threads N
      --memory SIZE       Memory to plan for, e.g. 8G (default: what the machine or
                          container has free); decides when large text files are
                          joined in one pass instead of two, and how many columns of
                          two Parquet files are read at once; Parquet that needs
                          more than SIZE one column at a time is refused
      --export-dir DIR    Write full changed/added/removed CSVs here
  -o, --out PATH          Write the HTML report here (no report without it)
      --json PATH         Write a JSON summary (counts + column stats) here
      --no-compress       Report: embed plain JSON instead of gzip (older browsers)
      --summary           Print the counts and write nothing, and refuse any flag that would
                          (the default without -o, --json or --export-dir)
      --fail-on-dups      Exit 3 when either file has duplicate keys

Exit codes: 0 identical, 1 differences found, 2 error, 3 duplicate keys (with --fail-on-dups).
";

/// Replaces the panic hook with one that cannot hang.
///
/// The hook the standard library installs takes a lock, then symbolises the
/// stack, and symbolising allocates. Under a memory budget that allocation can
/// fail, and the allocation error hook reaches for the same lock the panicking
/// thread is already holding: the process stops, in a state no timeout of its
/// own will end. On a benchmark runner a hang is the worst of the three
/// outcomes -- worse than the abort it replaced -- because nothing after it
/// runs. Two threads panicking at once also interleave the default hook's
/// output mid-word, which is how `memory allocation of 800040080004008000400
/// bytes failed` got printed.
///
/// So: one line, one write, no backtrace -- and `RUST_BACKTRACE` does not change
/// that. It used to: this hook printed `Backtrace::force_capture()` whenever
/// the variable was set, on the theory that it did not take the lock. It does.
/// `Backtrace`'s `Display` resolves symbols under the same backtrace lock, the
/// allocation error hook reaches for it, and the thread waits on itself; a
/// second panicking thread then waits on the stderr lock and `main` on `join`.
/// Measured at a 4.5 MB `ulimit -d`: 4 hangs in 30 runs with `RUST_BACKTRACE`
/// set, none in 30 without. And "the caller's risk" was nobody's choice:
/// `setup-rust-toolchain` exports `RUST_BACKTRACE` to every CI job, and plenty
/// of shells set it globally, which is how `tests/out_of_memory.rs` came to
/// fail intermittently on pull requests that touched nothing near it.
///
/// A backtrace is still there for whoever asks for this program's one by name:
/// `COMPARA_BACKTRACE=1`. It can hang under a memory cap, for the reason above.
fn quiet_panics() {
    std::panic::set_hook(Box::new(|info| {
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("a panic with no message");
        let mut err = std::io::stderr().lock();
        match info.location() {
            Some(at) => {
                let _ = writeln!(
                    err,
                    "error: internal: {what} at {}:{}",
                    at.file(),
                    at.line()
                );
            }
            None => {
                let _ = writeln!(err, "error: internal: {what}");
            }
        }
        if std::env::var_os("COMPARA_BACKTRACE").is_some() {
            let _ = writeln!(err, "{}", std::backtrace::Backtrace::force_capture());
        }
        let _ = err.flush();
    }));
}

fn main() -> ExitCode {
    quiet_panics();
    compara::alloc::hold_reserve();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let status = run(&args);
    report_peak_rss();
    ExitCode::from(status)
}

fn run(args: &[String]) -> u8 {
    let Some(command) = args.first() else {
        eprint!("{USAGE}");
        return 2;
    };
    match command.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            0
        }
        "-V" | "--version" | "version" => {
            println!("compara {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "compare" => match cmd_compare(&args[1..]) {
            Ok(status) => status,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        "columns" => match cmd_columns(&args[1..]) {
            Ok(status) => status,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        "head" => match cmd_head(&args[1..]) {
            Ok(status) => status,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        other => {
            eprint!("error: unknown command: {other}\n\n{USAGE}");
            2
        }
    }
}

/// A small GNU-style argument reader: `--name value`, `--name=value`, `-n value`
/// and bare flags, with everything else treated as a file argument.
struct Args {
    values: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

/// The options that take no value. Anything else consumes the token after it,
/// which is how a file path can appear before, between or after the options.
const FLAGS: [&str; 9] = [
    "trim",
    "ignore-case",
    "empty-is-null",
    "no-compress",
    "fail-on-dups",
    "summary",
    // `head --csv`. A value-taking flag left out of this list does not fail
    // quietly -- it eats the token after it -- but it does fail confusingly:
    // `head a.csv --csv` reported "--csv needs a value".
    "csv",
    "help",
    "h",
];

impl Args {
    fn parse(argv: &[String]) -> Result<Self> {
        let mut values = Vec::new();
        let mut positional = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            let token = &argv[i];
            let Some(rest) = token.strip_prefix('-').map(|r| r.trim_start_matches('-')) else {
                positional.push(token.clone());
                i += 1;
                continue;
            };
            if rest.is_empty() {
                positional.push(token.clone());
                i += 1;
                continue;
            }
            match rest.split_once('=') {
                Some((name, value)) => values.push((name.to_string(), Some(value.to_string()))),
                None if FLAGS.contains(&rest) => values.push((rest.to_string(), None)),
                None => {
                    let value = argv
                        .get(i + 1)
                        .ok_or_else(|| Error::new(format!("{token} needs a value")))?;
                    values.push((rest.to_string(), Some(value.clone())));
                    i += 1;
                }
            }
            i += 1;
        }
        Ok(Args { values, positional })
    }

    fn get(&self, long: &str, short: Option<&str>) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| name == long || Some(name.as_str()) == short)
            .and_then(|(_, value)| value.as_deref())
    }

    fn flag(&self, long: &str) -> bool {
        self.values.iter().any(|(name, _)| name == long)
    }

    fn number<T: std::str::FromStr>(&self, long: &str) -> Result<Option<T>> {
        match self.get(long, None) {
            None => Ok(None),
            Some(v) => v
                .parse::<T>()
                .map(Some)
                .map_err(|_| Error::new(format!("--{long} must be a number, got: {v}"))),
        }
    }
}

fn cmd_compare(argv: &[String]) -> Result<u8> {
    let args = Args::parse(argv)?;
    if args.flag("help") || args.flag("h") {
        print!("{USAGE}");
        return Ok(0);
    }
    if args.positional.len() < 2 {
        return Err(Error::new(
            "compare needs two files: compara compare A.csv B.csv --key ...",
        ));
    }
    let a_path = Path::new(&args.positional[0]);
    let b_path = Path::new(&args.positional[1]);

    let mut opt = Options::default();
    if let Some(name) = args.get("profile", Some("p")) {
        let found = profiles::load(args.get("config", None))?;
        let profile = found
            .get(name)
            .ok_or_else(|| Error::new(format!("profile not found: {name}")))?;
        profile.apply_to(&mut opt);
    }

    // Only options actually given override the profile, hence the `if let`s.
    if let Some(v) = args.get("key", Some("k")) {
        opt.key = parse_list(v);
    }
    if let Some(v) = args.get("compare", Some("c")) {
        opt.compare = Some(parse_list(v));
    }
    if let Some(v) = args.get("ignore", Some("i")) {
        opt.ignore = parse_list(v);
    }
    opt.trim |= args.flag("trim");
    opt.ignore_case |= args.flag("ignore-case");
    opt.empty_is_null |= args.flag("empty-is-null");
    if let Some(v) = args.number::<f64>("tolerance")? {
        opt.tolerance = v;
    }
    if let Some(v) = args.number::<usize>("max-rows")? {
        opt.max_rows = v;
    }
    if let Some(v) = args.number::<usize>("threads")? {
        opt.threads = Some(v);
    }
    if let Some(v) = args.get("memory", None) {
        opt.memory = Some(compara::memory::parse_size(v).ok_or_else(|| {
            Error::new(format!(
                "--memory must be a size such as 512M or 8G, got: {v}"
            ))
        })?);
    }
    if let Some(v) = args.get("delimiter", None) {
        opt.delimiter = v.chars().next();
    }
    if let Some(v) = args.get("encoding", None) {
        opt.encoding = v.to_string();
    }
    if let Some(v) = args.get("engine", None) {
        opt.engine = v.to_string();
    }
    if let Some(v) = args.get("export-dir", None) {
        opt.export_dir = Some(v.to_string());
    }
    if opt.key.is_empty() {
        return Err(Error::new("--key (or a profile with key) is required"));
    }

    // Without -o there is no report: the counts line is the answer, and the
    // engine skips collecting the rows a report would embed. `--summary` says
    // so explicitly, and refuses an output flag rather than quietly ignoring
    // one, because "write this here" and "write nothing" cannot both be
    // honoured and guessing which was meant is how a file silently stops
    // appearing.
    let summary_only = args.flag("summary");
    if summary_only {
        for (flag, short) in [("out", Some("o")), ("json", None), ("export-dir", None)] {
            if args.get(flag, short).is_some() {
                return Err(Error::new(format!(
                    "--summary writes nothing; --{flag} asks it to"
                )));
            }
        }
    }
    let report = args.get("out", Some("o")).map(PathBuf::from);
    if report.is_none() && args.flag("no-compress") {
        return Err(Error::new(
            "--no-compress is about the HTML report; add -o <path> to write one",
        ));
    }
    // Only the report embeds rows. An export is every row, which each engine
    // keeps for it whether or not this is set.
    opt.row_lists = report.is_some();

    let result = compare(a_path, b_path, &mut opt)?;

    let mut written: Option<PathBuf> = None;
    if let Some(out_path) = report {
        let html = render(&result, !args.flag("no-compress"))?;
        fs::write(&out_path, html)
            .map_err(|e| Error::new(format!("cannot write {}: {e}", out_path.display())))?;
        written = Some(out_path);
    }
    if let Some(path) = args.get("json", None) {
        let summary = serde_json::to_string_pretty(&result.summary())?;
        fs::write(path, summary).map_err(|e| Error::new(format!("cannot write {path}: {e}")))?;
    }

    let c = &result.counts;
    println!(
        "A {} rows | B {} rows | matched {} (changed {}) | added {} | removed {} | \
         dup keys A {} B {} | {} {}s",
        comma(c.a_rows),
        comma(c.b_rows),
        comma(c.matched),
        comma(c.changed),
        comma(c.added),
        comma(c.removed),
        comma(c.a_dup_keys),
        comma(c.b_dup_keys),
        result.meta.engine,
        result.meta.seconds
    );
    if let Some((out_path, meta)) = written
        .as_ref()
        .and_then(|p| fs::metadata(p).ok().map(|m| (p, m)))
    {
        println!(
            "Report: {} ({:.0} KB)",
            out_path.display(),
            meta.len() as f64 / 1024.0
        );
    }

    if args.flag("fail-on-dups") && (c.a_dup_keys > 0 || c.b_dup_keys > 0) {
        return Ok(3);
    }
    let code = if result.identical() { 0 } else { 1 };
    // The process ends when this returns, and the kernel takes its memory back
    // in one step. Dropping the result first freed the report's rows a string
    // at a time -- several hundred thousand of them at the default row cap,
    // 45ms of a 1.7s run on a 4M-row pair -- for nothing anyone can observe.
    std::mem::forget(result);
    Ok(code)
}

/// Groups a count with thousands separators, matching the other implementations.
fn comma(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Prints this process's peak resident set size for the benchmark harness, when
/// it asks.
///
/// Rust has no portable API for it, so this reads `VmHWM` from `/proc`; on a
/// platform without it nothing is printed and the harness records no figure
/// rather than a wrong one.
fn report_peak_rss() {
    if std::env::var_os("COMPARA_PRINT_PEAK_RSS").is_none() {
        return;
    }
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: String = rest.chars().filter(char::is_ascii_digit).collect();
            if !kb.is_empty() {
                let _ = writeln!(std::io::stderr(), "PEAK_RSS_KB {kb}");
            }
            return;
        }
    }
}

/// `compara columns FILE` — the column names, one per line.
///
/// One per line rather than a comma list because that is what a shell wants:
/// the whole point of asking is usually to build a `--key` out of the answer,
/// and `paste -sd,` turns this into one.
fn cmd_columns(argv: &[String]) -> Result<u8> {
    let args = Args::parse(argv)?;
    if args.flag("help") || args.flag("h") {
        print!("{USAGE}");
        return Ok(0);
    }
    let Some(first) = args.positional.first() else {
        return Err(Error::new("columns needs a file: compara columns data.csv"));
    };
    let mut opt = Options::default();
    apply_read_options(&args, &mut opt);

    let shape = compara::engine::turbo::shape(Path::new(first), &opt)?;
    for name in &shape.columns {
        println!("{name}");
    }
    // To stderr so the names on stdout stay pipeable on their own.
    match shape.rows {
        Some(rows) => eprintln!(
            "{}: {} columns, {} rows",
            shape.format,
            shape.columns.len(),
            thousands(rows)
        ),
        None => eprintln!(
            "{}: {} columns (row count needs a full read)",
            shape.format,
            shape.columns.len()
        ),
    }
    Ok(0)
}

/// `compara head FILE [-n N]` — the first rows, aligned, or `--csv` for a
/// machine.
fn cmd_head(argv: &[String]) -> Result<u8> {
    let args = Args::parse(argv)?;
    if args.flag("help") || args.flag("h") {
        print!("{USAGE}");
        return Ok(0);
    }
    let Some(first) = args.positional.first() else {
        return Err(Error::new("head needs a file: compara head data.csv"));
    };
    let want = match args.get("rows", Some("n")) {
        Some(text) => text
            .parse::<usize>()
            .map_err(|_| Error::new(format!("--rows must be a whole number, got: {text}")))?,
        None => 10,
    };
    let mut opt = Options::default();
    apply_read_options(&args, &mut opt);

    let (columns, rows) = compara::engine::turbo::head(Path::new(first), want, &opt)?;
    if args.flag("csv") {
        println!("{}", csv_line(&columns));
        for row in &rows {
            println!("{}", csv_line(row));
        }
        return Ok(0);
    }
    print_table(&columns, &rows);
    Ok(0)
}

/// The two flags that change how a file is *read* rather than compared.
fn apply_read_options(args: &Args, opt: &mut Options) {
    if let Some(v) = args.get("delimiter", None) {
        opt.delimiter = v.chars().next();
    }
    if let Some(v) = args.get("encoding", None) {
        opt.encoding = v.to_string();
    }
}

/// One CSV line, quoting only what has to be quoted.
fn csv_line(cells: &[String]) -> String {
    cells
        .iter()
        .map(|c| {
            if c.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// An aligned table, capped so one wide column cannot push the rest off screen.
fn print_table(columns: &[String], rows: &[Vec<String>]) {
    const CAP: usize = 32;
    let clip = |s: &str| -> String {
        let count = s.chars().count();
        if count <= CAP {
            s.to_string()
        } else {
            // A marker rather than a silent cut: a preview that quietly shortens
            // a value teaches the wrong thing about the data.
            format!("{}…", s.chars().take(CAP - 1).collect::<String>())
        }
    };
    let head: Vec<String> = columns.iter().map(|c| clip(c)).collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| clip(c)).collect())
        .collect();

    let mut width: Vec<usize> = head.iter().map(|c| c.chars().count()).collect();
    for row in &body {
        for (i, cell) in row.iter().enumerate() {
            if i < width.len() {
                width[i] = width[i].max(cell.chars().count());
            }
        }
    }

    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let pad = width.get(i).copied().unwrap_or(0);
                format!("{c:<pad$}")
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };

    println!("{}", line(&head));
    println!(
        "{}",
        width
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &body {
        println!("{}", line(row));
    }
}

/// `1234567` as `1,234,567`, the way the compare summary already writes counts.
fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}
