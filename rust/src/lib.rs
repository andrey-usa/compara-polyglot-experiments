//! Parameterised composite-key CSV comparison with a self-contained HTML report.
//!
//! Two CSV files are joined on a composite key and the differing cells are
//! reported: what changed, what was added, what was removed, and which keys are
//! duplicated in either file. Nothing about a specific dataset lives in the
//! code — key columns, compared columns and normalisation are all runtime
//! parameters.
//!
//! ```no_run
//! use compara::{Options, compare, render};
//! use std::path::Path;
//!
//! let mut opt = Options::with_key(["order_id", "line_no"]);
//! opt.ignore = vec!["updated_at".to_string()];
//! let result = compare(Path::new("july.csv"), Path::new("august.csv"), &mut opt)?;
//! std::fs::write("report.html", render(&result, true)?)?;
//! # Ok::<(), compara::Error>(())
//! ```
//!
//! The result contract, the report template and the exit codes are shared byte
//! for byte with the Python, TypeScript, Java and Go implementations of this
//! tool, so a report from any of them is interchangeable.

pub mod alloc;
pub mod columns;
pub mod contract;
pub mod engine;
pub mod error;
pub mod gendata;
pub mod memory;
pub mod options;
pub mod parallel;
pub mod parquet;
pub mod phases;
pub mod profiles;
pub mod report;
pub mod rowstore;
pub mod sections;

pub use contract::{CellDiff, ColumnStat, CompareResult, Counts, Section, Val};
pub use engine::compare;
pub use error::{Error, Result};
pub use options::{DEFAULT_MAX_ROWS, Engine, Options, parse_list};
pub use report::render;
