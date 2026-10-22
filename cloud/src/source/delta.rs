//! Delta Lake tables: which of the files under a table make up one version.
//!
//! A Delta table is a directory of Parquet files and a log, `_delta_log/`,
//! that says which of them are the table: commits (`00000000000000000012.json`,
//! one action a line -- `add` a file, `remove` one) and, every so often, a
//! checkpoint (`00000000000000000010.checkpoint.parquet`) holding the state
//! at that version. A version is its last checkpoint at or before it, then
//! the commits after that, replayed. `orders/` is the latest version and
//! `orders@v12` the twelfth, so two versions of one table compare like two
//! tables.
//!
//! What is refused rather than read wrongly: deletion vectors (rows of a
//! file marked deleted outside it), column mapping (columns stored under
//! other names), V2 checkpoints, and a log whose start has been cleaned up
//! with no checkpoint to stand in for it.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};

/// `table@v12` as the table and version 12; anything else as itself.
pub(crate) fn split_version(spec: &str) -> (&str, Option<u64>) {
    if let Some(at) = spec.rfind("@v") {
        let digits = &spec[at + 2..];
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(v) = digits.parse::<u64>()
        {
            return (&spec[..at], Some(v));
        }
    }
    (spec, None)
}

/// Whether paths listed under a root (relative to it) are a Delta table's.
#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
pub(crate) fn is_delta<'a>(mut relative: impl Iterator<Item = &'a str>) -> bool {
    relative.any(|r| r.starts_with("_delta_log/"))
}

/// The log files to read for one version: its checkpoint's parts, if it has
/// one, and the commits after it, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub version: u64,
    pub checkpoint: Vec<String>,
    pub commits: Vec<String>,
}

enum Checkpoint {
    Single(String),
    Parts(u32, BTreeMap<u32, String>),
    V2,
}

/// Which log files make `version` (the latest when `None`), from the names in
/// `_delta_log/` (file names only).
pub(crate) fn plan(log: &[String], version: Option<u64>, spec: &str) -> Result<Plan> {
    let mut commits = BTreeSet::new();
    let mut checkpoints: BTreeMap<u64, Checkpoint> = BTreeMap::new();
    for name in log {
        let Some(v) = name.get(..20).and_then(|d| d.parse::<u64>().ok()) else {
            continue;
        };
        if name.len() < 21 || !name[..20].bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let rest = &name[20..];
        if rest == ".json" {
            commits.insert(v);
        } else if rest == ".checkpoint.parquet" {
            checkpoints.insert(v, Checkpoint::Single(name.clone()));
        } else if let Some(p) = rest
            .strip_prefix(".checkpoint.")
            .and_then(|r| r.strip_suffix(".parquet"))
        {
            let mut it = p.split('.');
            match (it.next(), it.next(), it.next()) {
                (Some(part), Some(of), None)
                    if part.len() == 10
                        && of.len() == 10
                        && part.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    let (part, of) = (part.parse().unwrap_or(0), of.parse().unwrap_or(0));
                    if let Checkpoint::Parts(_, parts) = checkpoints
                        .entry(v)
                        .or_insert_with(|| Checkpoint::Parts(of, BTreeMap::new()))
                    {
                        parts.insert(part, name.clone());
                    }
                }
                _ => {
                    checkpoints.entry(v).or_insert(Checkpoint::V2);
                }
            }
        } else if rest.starts_with(".checkpoint.") {
            checkpoints.entry(v).or_insert(Checkpoint::V2);
        }
    }
    let latest = commits
        .iter()
        .chain(checkpoints.keys())
        .max()
        .copied()
        .ok_or_else(|| Error::new(format!("{spec}: the Delta log has no commits")))?;
    let target = version.unwrap_or(latest);
    if target > latest {
        return Err(Error::new(format!(
            "{spec}: no version {target}; the latest is {latest}"
        )));
    }
    // The newest whole checkpoint at or before the version.
    let mut from = None;
    for (&v, c) in checkpoints.range(..=target).rev() {
        match c {
            Checkpoint::Single(name) => {
                from = Some((v, vec![name.clone()]));
                break;
            }
            Checkpoint::Parts(of, parts) if parts.len() as u32 == *of && *of > 0 => {
                from = Some((v, parts.values().cloned().collect()));
                break;
            }
            Checkpoint::V2 if v == target || !commits.contains(&(v + 1)) => {
                return Err(Error::new(format!(
                    "{spec}: version {v} is in a V2 checkpoint, which is not read here"
                )));
            }
            _ => {}
        }
    }
    let start = from.as_ref().map_or(0, |(v, _)| v + 1);
    let mut wanted = Vec::new();
    for v in start..=target {
        if !commits.contains(&v) {
            return Err(Error::new(if from.is_none() {
                format!(
                    "{spec}: the Delta log no longer holds version {v}, and has no checkpoint \
                     at or before {target} to read from instead"
                )
            } else {
                format!("{spec}: the Delta log is missing version {v}")
            }));
        }
        wanted.push(format!("{v:020}.json"));
    }
    Ok(Plan {
        version: target,
        checkpoint: from.map(|(_, names)| names).unwrap_or_default(),
        commits: wanted,
    })
}

/// The table's files at the plan's version, as paths under the table root
/// (decoded, `date=2026-10-01/part-0.parquet`), in path order. `read` gives
/// the bytes of a log file by its name in `_delta_log/`.
pub(crate) fn replay(
    plan: &Plan,
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    spec: &str,
) -> Result<Vec<String>> {
    let mut files: BTreeSet<String> = BTreeSet::new();
    let refuse_dv = || {
        Error::new(format!(
            "{spec}: version {} has deletion vectors -- rows deleted outside their files -- \
             which are not read here; compare an export of the table",
            plan.version
        ))
    };
    let refuse_mapping = || {
        Error::new(format!(
            "{spec}: the table uses column mapping, which stores columns under other names \
             and is not read here; compare an export of the table"
        ))
    };
    for name in &plan.checkpoint {
        let bytes = read(name)?;
        let leaves = crate::parquet::nested_strings(
            &bytes,
            &[
                &["add", "path"],
                &["add", "deletionVector", "storageType"],
                &["metaData", "schemaString"],
            ],
            name,
        )?;
        for (i, path) in leaves[0].iter().enumerate() {
            if let Some(path) = path {
                if leaves[1][i].is_some() {
                    return Err(refuse_dv());
                }
                files.insert(decode_path(path, spec)?);
            }
        }
        if leaves[2]
            .iter()
            .flatten()
            .any(|s| s.contains("delta.columnMapping.physicalName"))
        {
            return Err(refuse_mapping());
        }
    }
    for name in &plan.commits {
        let bytes = read(name)?;
        for line in bytes.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let action: serde_json::Value = serde_json::from_slice(line)
                .map_err(|e| Error::new(format!("{spec}: {name} is not a Delta commit: {e}")))?;
            if let Some(add) = action.get("add") {
                if add.get("deletionVector").is_some_and(|d| !d.is_null()) {
                    return Err(refuse_dv());
                }
                if let Some(path) = add.get("path").and_then(|p| p.as_str()) {
                    files.insert(decode_path(path, spec)?);
                }
            } else if let Some(path) = action
                .get("remove")
                .and_then(|r| r.get("path"))
                .and_then(|p| p.as_str())
            {
                files.remove(&decode_path(path, spec)?);
            } else if let Some(meta) = action.get("metaData") {
                let mode = meta
                    .get("configuration")
                    .and_then(|c| c.get("delta.columnMapping.mode"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("none");
                let physical = meta
                    .get("schemaString")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.contains("delta.columnMapping.physicalName"));
                if mode != "none" || physical {
                    return Err(refuse_mapping());
                }
            }
        }
    }
    if files.is_empty() {
        return Err(Error::new(format!(
            "{spec}: version {} of the table has no files",
            plan.version
        )));
    }
    Ok(files.into_iter().collect())
}

/// A file's path in the log -- relative and percent-encoded -- as a path
/// under the table root. An absolute one (a shallow clone's) is refused.
fn decode_path(path: &str, spec: &str) -> Result<String> {
    if path.contains("://") || path.starts_with('/') {
        return Err(Error::new(format!(
            "{spec}: the table names a file outside it ({path}), as a clone does; compare an \
             export of the table"
        )));
    }
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn versions_split_off_the_spec() {
        assert_eq!(
            split_version("s3://b/orders@v12"),
            ("s3://b/orders", Some(12))
        );
        assert_eq!(split_version("orders/@v0"), ("orders/", Some(0)));
        assert_eq!(
            split_version("abfss://c@acct.dfs.core.windows.net/t"),
            ("abfss://c@acct.dfs.core.windows.net/t", None)
        );
        assert_eq!(split_version("x@vnext"), ("x@vnext", None));
    }

    #[test]
    fn a_version_is_its_checkpoint_and_the_commits_after() {
        let log = names(&[
            "00000000000000000000.json",
            "00000000000000000001.json",
            "00000000000000000002.json",
            "00000000000000000002.checkpoint.parquet",
            "00000000000000000003.json",
            "00000000000000000004.json",
            "_last_checkpoint",
            "00000000000000000004.crc",
        ]);
        let p = plan(&log, None, "t").unwrap();
        assert_eq!(p.version, 4);
        assert_eq!(
            p.checkpoint,
            names(&["00000000000000000002.checkpoint.parquet"])
        );
        assert_eq!(
            p.commits,
            names(&["00000000000000000003.json", "00000000000000000004.json"])
        );
        let p = plan(&log, Some(1), "t").unwrap();
        assert!(p.checkpoint.is_empty());
        assert_eq!(p.commits.len(), 2);
        assert!(
            plan(&log, Some(9), "t")
                .unwrap_err()
                .to_string()
                .contains("latest is 4")
        );
        // The log's start cleaned up: version 1 is no longer there to read.
        let trimmed = names(&[
            "00000000000000000002.checkpoint.parquet",
            "00000000000000000003.json",
        ]);
        assert!(plan(&trimmed, Some(1), "t").is_err());
        assert_eq!(plan(&trimmed, None, "t").unwrap().version, 3);
        // A checkpoint in parts counts only when every part is there.
        let parts = names(&[
            "00000000000000000000.json",
            "00000000000000000001.json",
            "00000000000000000001.checkpoint.0000000001.0000000002.parquet",
        ]);
        assert!(plan(&parts, None, "t").unwrap().checkpoint.is_empty());
    }

    #[test]
    fn commits_add_and_remove_files() {
        let log = names(&["00000000000000000000.json", "00000000000000000001.json"]);
        let p = plan(&log, None, "t").unwrap();
        let mut read = |name: &str| -> Result<Vec<u8>> {
            Ok(match name {
                "00000000000000000000.json" => concat!(
                    "{\"protocol\":{\"minReaderVersion\":1,\"minWriterVersion\":2}}\n",
                    "{\"metaData\":{\"id\":\"x\",\"configuration\":{}}}\n",
                    "{\"add\":{\"path\":\"date=2026-10-01/part-0.parquet\",\"size\":1}}\n",
                    "{\"add\":{\"path\":\"date=2026-10-01/part%201.parquet\",\"size\":1}}\n",
                )
                .into(),
                _ => concat!(
                    "{\"remove\":{\"path\":\"date=2026-10-01/part-0.parquet\"}}\n",
                    "{\"add\":{\"path\":\"date=2026-10-02/part-2.parquet\",\"deletionVector\":null}}\n",
                )
                .into(),
            })
        };
        let files = replay(&p, &mut read, "t").unwrap();
        assert_eq!(
            files,
            names(&[
                "date=2026-10-01/part 1.parquet",
                "date=2026-10-02/part-2.parquet"
            ])
        );
        let mut dv = |_: &str| -> Result<Vec<u8>> {
            Ok(
                "{\"add\":{\"path\":\"a.parquet\",\"deletionVector\":{\"storageType\":\"u\"}}}\n"
                    .into(),
            )
        };
        assert!(
            replay(&p, &mut dv, "t")
                .unwrap_err()
                .to_string()
                .contains("deletion vectors")
        );
        let mut mapped = |_: &str| -> Result<Vec<u8>> {
            Ok(
                "{\"metaData\":{\"configuration\":{\"delta.columnMapping.mode\":\"name\"}}}\n"
                    .into(),
            )
        };
        assert!(
            replay(&p, &mut mapped, "t")
                .unwrap_err()
                .to_string()
                .contains("column mapping")
        );
    }
}
