//! Runtime parameters and engine names.

use serde::Serialize;
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Which comparison backend to run.
///
/// [`Engine::Auto`] resolves to the first concrete engine that can run here, in
/// declaration order. Every concrete engine must return identical `counts` and
/// `columns` for the same input; the tests and CI both assert that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Picks the first available concrete engine.
    Auto,
    /// Byte-level: maps the file and indexes it in place, building no string
    /// for a cell unless that cell reaches the report.
    Turbo,
    /// External sort then merge join: bounded memory, spills to disk, size
    /// limited by disk rather than RAM.
    SortMerge,
    /// This project, over the `csv` crate: in-memory, row-oriented.
    Native,
}

impl Engine {
    /// The real backends, in `Auto` preference order.
    pub const CONCRETE: [Engine; 3] = [Engine::Turbo, Engine::SortMerge, Engine::Native];

    /// The name used on the command line and in the report.
    pub fn label(self) -> &'static str {
        match self {
            Engine::Auto => "auto",
            Engine::Turbo => "turbo",
            Engine::SortMerge => "sortmerge",
            Engine::Native => "native",
        }
    }
}

impl FromStr for Engine {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Engine::Auto),
            "turbo" => Ok(Engine::Turbo),
            "sortmerge" => Ok(Engine::SortMerge),
            "native" => Ok(Engine::Native),
            other => Err(Error::new(format!(
                "unknown engine: {other}. Choose one of auto, turbo, sortmerge, native"
            ))),
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// How many rows each report section embeds by default. Counts are always exact
/// regardless.
pub const DEFAULT_MAX_ROWS: usize = 50_000;

/// Everything that varies per comparison. Nothing about a specific dataset
/// belongs in the code, so key columns, compared columns and normalisation are
/// all runtime parameters.
///
/// The JSON names are snake_case to match the other implementations, which read
/// and write the same contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Options {
    /// The composite key; at least one column is required.
    #[serde(skip)]
    pub key: Vec<String>,
    /// The columns to diff, or `None` for every common non-key column.
    #[serde(skip)]
    pub compare: Option<Vec<String>>,
    /// Columns to skip entirely.
    #[serde(skip)]
    pub ignore: Vec<String>,

    pub trim: bool,
    pub ignore_case: bool,
    pub empty_is_null: bool,
    pub tolerance: f64,
    pub max_rows: usize,
    pub delimiter: Option<char>,
    pub encoding: String,
    pub engine: String,
    pub threads: Option<usize>,
    pub export_dir: Option<String>,

    /// How much memory the comparison may count on, in bytes, or `None` to
    /// ask the machine (see [`crate::memory`]). For text it decides whether a
    /// large pair is joined after both files are indexed or inside A's sweep.
    /// For two Parquet files it decides how many columns are decoded at once,
    /// and a pair that needs more than a budget set here even one column at a
    /// time is refused.
    #[serde(skip)]
    pub memory: Option<u64>,

    /// Whether the engine materialises the report's rows.
    ///
    /// The counts, the per-column stats and the duplicate-key tallies are the
    /// whole of what a summary needs, and they are exact either way: a section
    /// records how many rows it dropped, so the totals do not depend on any of
    /// them being kept. What this turns off is the rest -- decoding up to
    /// `max_rows` rows per section into `String`s, sorting them, diffing their
    /// cells, and walking every duplicated key to build that section.
    ///
    /// It is `true` by default because a library caller asking for a [`Diff`]
    /// wants the rows in it. The CLI sets it false for `--summary`, which is
    /// the invocation that writes nothing.
    ///
    /// [`Diff`]: crate::contract::Diff
    #[serde(skip)]
    pub row_lists: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            key: Vec::new(),
            compare: None,
            ignore: Vec::new(),
            trim: false,
            ignore_case: false,
            empty_is_null: false,
            tolerance: 0.0,
            max_rows: DEFAULT_MAX_ROWS,
            delimiter: None,
            encoding: "utf-8".to_string(),
            engine: Engine::Auto.label().to_string(),
            threads: None,
            export_dir: None,
            memory: None,
            row_lists: true,
        }
    }
}

impl Options {
    /// Options for a comparison on the given key, everything else defaulted.
    pub fn with_key<I, S>(key: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Options {
            key: key.into_iter().map(Into::into).collect(),
            ..Options::default()
        }
    }

    /// How many threads a comparison may use in total, `--threads` or the
    /// machine.
    ///
    /// Both files are read at once and each is split further, so this is the
    /// width of the whole run rather than of one file. It lives here rather
    /// than in one engine because every path that spreads work has to agree
    /// with the flag, and they did not: the text engine asked this, and the
    /// columnar Parquet reader asked the machine, so `--threads 1` on a Parquet
    /// pair ran on every core -- which is the one thing that flag is for.
    ///
    /// The report's gzip lanes are still the machine's, because [`crate::report::render`]
    /// is handed a result and not the options that produced it. On `--threads 1`
    /// that is the difference between 1.00x cores and the 1.19x measured.
    pub fn thread_budget(&self) -> usize {
        self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
    }

    /// Checks the options are usable and normalises the defaults.
    pub fn validate(&mut self) -> Result<()> {
        if self.key.is_empty() {
            return Err(Error::new("at least one key column is required"));
        }
        if self.tolerance < 0.0 {
            return Err(Error::new(format!(
                "--tolerance must not be negative, got {}",
                self.tolerance
            )));
        }
        if self.max_rows == 0 {
            return Err(Error::new("--max-rows must be positive, got 0"));
        }
        if self.threads == Some(0) {
            return Err(Error::new("--threads must be positive, got 0"));
        }
        if self.memory == Some(0) {
            return Err(Error::new("--memory must be positive, got 0"));
        }
        if self.encoding.is_empty() {
            self.encoding = "utf-8".to_string();
        }
        if self.engine.is_empty() {
            self.engine = Engine::Auto.label().to_string();
        }
        self.engine_name()?;
        Ok(())
    }

    /// The requested engine, parsed.
    pub fn engine_name(&self) -> Result<Engine> {
        self.engine.parse()
    }

    /// The forced delimiter as a byte, or `None` when it should be sniffed.
    ///
    /// The CSV readers here are byte-oriented, so a multi-byte delimiter cannot
    /// be honoured; that is rejected rather than silently truncated.
    pub fn delimiter_byte(&self) -> Result<Option<u8>> {
        match self.delimiter {
            None => Ok(None),
            Some(c) if c.is_ascii() => Ok(Some(c as u8)),
            Some(c) => Err(Error::new(format!(
                "--delimiter must be an ASCII character, got {c:?}"
            ))),
        }
    }
}

/// Splits `"a, b ,c"` into a list; an empty string yields nothing at all.
pub fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
