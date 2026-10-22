//! Capping the report sections and writing the uncapped CSV exports.

use std::fs::{self, File};
use std::path::Path;

use crate::contract::{Cell, EngineMeta, EngineResult, Section, Val};
use crate::error::{Error, Result};
use crate::options::Options;
use crate::rowstore::Joined;

/// Turns a completed join into the capped report sections, and writes the
/// uncapped CSV exports when `--export-dir` is set.
pub fn assemble(
    meta: EngineMeta,
    joined: Joined,
    dup_a: Section,
    dup_b: Section,
    opt: &Options,
    compared: &[String],
) -> Result<EngineResult> {
    if let Some(dir) = &opt.export_dir {
        export(Path::new(dir), &joined, opt, compared)?;
    }
    Ok(sections(meta, joined, dup_a, dup_b, opt, compared))
}

/// The capped report sections alone, for an engine that writes its export
/// itself rather than from a [`Joined`].
pub fn sections(
    meta: EngineMeta,
    joined: Joined,
    dup_a: Section,
    dup_b: Section,
    opt: &Options,
    compared: &[String],
) -> EngineResult {
    let (cols, _) = export_headers(opt, compared);
    EngineResult {
        meta,
        counts: joined.counts,
        columns: joined.columns,
        changed: Section::capped(opt.key.clone(), joined.changed, opt.max_rows),
        added: Section::capped(cols.clone(), joined.added, opt.max_rows),
        removed: Section::capped(cols, joined.removed, opt.max_rows),
        dup_a,
        dup_b,
    }
}

/// The columns of the export files: an added or removed row is the key and
/// the compared columns, and a changed row puts the two sides of each compared
/// column side by side, which the sparse report payload cannot show.
pub fn export_headers(opt: &Options, compared: &[String]) -> (Vec<String>, Vec<String>) {
    let mut rows = opt.key.clone();
    rows.extend_from_slice(compared);
    let mut both = opt.key.clone();
    for c in compared {
        both.push(format!("{c} (A)"));
        both.push(format!("{c} (B)"));
    }
    (rows, both)
}

/// Creates the export directory if it is missing.
pub fn create_export_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .map_err(|e| Error::new(format!("cannot create export-dir {}: {e}", dir.display())))
}

/// Opens one export file and writes its header. Every engine writes through
/// this, so the files are quoted and terminated the same way whichever path
/// produced them.
pub fn export_file(path: &Path, header: &[String]) -> Result<csv::Writer<File>> {
    let mut w = csv::Writer::from_path(path)
        .map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))?;
    w.write_record(header)?;
    Ok(w)
}

fn export(dir: &Path, joined: &Joined, opt: &Options, compared: &[String]) -> Result<()> {
    create_export_dir(dir)?;
    let (cols, both) = export_headers(opt, compared);
    write_cells(&dir.join("added.csv"), &cols, &joined.added)?;
    write_cells(&dir.join("removed.csv"), &cols, &joined.removed)?;

    let key_size = opt.key.len();
    let rows: Vec<Vec<Val>> = joined
        .changed_a
        .iter()
        .zip(&joined.changed_b)
        .map(|(ar, br)| {
            let mut row: Vec<Val> = ar[..key_size].to_vec();
            for i in 0..compared.len() {
                row.push(ar[key_size + i].clone());
                row.push(br[key_size + i].clone());
            }
            row
        })
        .collect();
    write_rows(&dir.join("changed.csv"), &both, &rows)
}

fn write_cells(path: &Path, header: &[String], rows: &[Vec<Cell>]) -> Result<()> {
    let values: Vec<Vec<Val>> = rows
        .iter()
        .map(|row| row.iter().map(|c| c.value().map(str::to_string)).collect())
        .collect();
    write_rows(path, header, &values)
}

/// Writes an uncapped export file from rows of values.
fn write_rows(path: &Path, header: &[String], rows: &[Vec<Val>]) -> Result<()> {
    let mut w = export_file(path, header)?;
    let mut record: Vec<&str> = Vec::with_capacity(header.len());
    for row in rows {
        record.clear();
        for i in 0..header.len() {
            record.push(row.get(i).and_then(Option::as_deref).unwrap_or(""));
        }
        w.write_record(&record)?;
    }
    w.flush()?;
    Ok(())
}
