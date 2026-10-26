//! The result contract.
//!
//! This is the API, and it is shared byte for byte with the Python, TypeScript,
//! Java and Go implementations of this tool: the same JSON, the same report
//! template, so a report from any of them is interchangeable. Add a field rather
//! than reshaping an existing one.
//!
//! A CSV cell is an `Option<String>`: values are read as text with no type
//! inference, and an empty field — quoted or not — is absent rather than a
//! zero-length string.

use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};

use crate::options::Options;

/// One CSV cell. `None` is an absent value.
pub type Val = Option<String>;

/// The row totals. Always exact, even when the embedded row lists are capped by
/// `max_rows`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub a_rows: i64,
    pub b_rows: i64,
    pub a_keys: i64,
    pub b_keys: i64,
    pub matched: i64,
    pub unchanged: i64,
    pub changed: i64,
    pub added: i64,
    pub removed: i64,
    pub a_dup_keys: i64,
    pub a_dup_rows: i64,
    pub b_dup_keys: i64,
    pub b_dup_rows: i64,
}

/// How one compared column fared across the matched rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnStat {
    pub name: String,
    pub changed: i64,
    pub blanked: i64,
    pub filled: i64,
}

/// One differing cell of a changed row.
///
/// It serialises as the array `[column_index, a, b]`; changed rows carry these
/// triples rather than whole rows, and keeping the payload sparse is what keeps
/// a large report small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDiff {
    pub column: usize,
    pub a: Val,
    pub b: Val,
}

impl Serialize for CellDiff {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(3))?;
        seq.serialize_element(&self.column)?;
        seq.serialize_element(&self.a)?;
        seq.serialize_element(&self.b)?;
        seq.end()
    }
}

/// One field of an embedded row.
///
/// Rows are heterogeneous: added, removed and duplicate rows hold cell values,
/// a duplicate row ends with its occurrence count, and a changed row ends with
/// the list of differing cells. Untagged, so each variant serialises as the bare
/// JSON the other implementations write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Cell {
    Value(Val),
    Count(i64),
    Diffs(Vec<CellDiff>),
}

impl Cell {
    /// The cell's value, for the code paths that know a row holds only values.
    pub fn value(&self) -> Option<&str> {
        match self {
            Cell::Value(v) => v.as_deref(),
            _ => None,
        }
    }
}

/// A capped list of rows plus the header for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
}

impl Section {
    /// Caps a row list at `max_rows` and records whether anything was cut.
    pub fn capped(cols: Vec<String>, mut rows: Vec<Vec<Cell>>, max_rows: usize) -> Self {
        let truncated = rows.len() > max_rows;
        rows.truncate(max_rows);
        Section {
            cols,
            rows,
            truncated,
        }
    }

    /// An empty section, for a comparison that produced nothing in this category.
    pub fn empty(cols: Vec<String>) -> Self {
        Section {
            cols,
            rows: Vec::new(),
            truncated: false,
        }
    }
}

/// One of the two compared files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileMeta {
    pub name: String,
    pub path: String,
    pub size: u64,
}

/// What the engine resolved before comparing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMeta {
    pub key: Vec<String>,
    pub compared: Vec<String>,
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
    pub a_cols: usize,
    pub b_cols: usize,
}

/// [`EngineMeta`] completed with the run's own facts.
///
/// Serialised flat, because that is the shape the report template and the other
/// implementations read.
#[derive(Debug, Clone)]
pub struct Meta {
    pub engine_meta: EngineMeta,
    pub a: FileMeta,
    pub b: FileMeta,
    pub engine: String,
    pub seconds: f64,
    pub generated: String,
    pub options: Options,
}

impl Serialize for Meta {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let m = &self.engine_meta;
        let mut st = s.serialize_struct("meta", 12)?;
        st.serialize_field("key", &m.key)?;
        st.serialize_field("compared", &m.compared)?;
        st.serialize_field("only_in_a", &m.only_in_a)?;
        st.serialize_field("only_in_b", &m.only_in_b)?;
        st.serialize_field("a_cols", &m.a_cols)?;
        st.serialize_field("b_cols", &m.b_cols)?;
        st.serialize_field("a", &self.a)?;
        st.serialize_field("b", &self.b)?;
        st.serialize_field("engine", &self.engine)?;
        st.serialize_field("seconds", &self.seconds)?;
        st.serialize_field("generated", &self.generated)?;
        st.serialize_field("options", &self.options)?;
        st.end()
    }
}

/// What an engine produces, before the run's own facts are attached.
#[derive(Debug, Clone)]
pub struct EngineResult {
    pub meta: EngineMeta,
    pub counts: Counts,
    pub columns: Vec<ColumnStat>,
    pub changed: Section,
    pub added: Section,
    pub removed: Section,
    pub dup_a: Section,
    pub dup_b: Section,
}

/// A finished comparison.
#[derive(Debug, Clone, Serialize)]
pub struct CompareResult {
    pub meta: Meta,
    pub counts: Counts,
    pub columns: Vec<ColumnStat>,
    pub changed: Section,
    pub added: Section,
    pub removed: Section,
    pub dup_a: Section,
    pub dup_b: Section,
}

impl CompareResult {
    /// Whether the two files carry the same data under the options given.
    pub fn identical(&self) -> bool {
        self.counts.changed == 0 && self.counts.added == 0 && self.counts.removed == 0
    }

    /// The subset written by `--json`: everything except the embedded rows.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "meta": self.meta,
            "counts": self.counts,
            "columns": self.columns,
        })
    }
}
