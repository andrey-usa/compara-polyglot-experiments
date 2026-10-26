//! Several files as one side: a directory, a glob, an object storage prefix.
//!
//! Tables in a data lake are directories of parts -- `part-00000.parquet`,
//! `date=2026-10-01/part-00003.csv.gz` -- and a side of a comparison can be
//! one: a local directory, a glob (`'exports/2026-10-*/*.parquet'`, quoted so
//! the shell leaves it), or an object storage prefix (`s3://bucket/orders/`).
//! The parts are read in path order as one table, so "the first occurrence
//! of a repeated key" means the first in that order.
//!
//! The engines read one file, so the parts become one, and nothing below them
//! changes:
//!
//! - CSV and TSV parts follow each other, every part's header checked against
//!   the first's and then blanked out with newlines -- a blank line is skipped
//!   by every reader here, so a part's header is overwritten in place rather
//!   than the part copied around it;
//! - newline-delimited JSON parts follow each other;
//! - Parquet parts follow each other, and a footer written after them names
//!   every part's row groups, their offsets moved to where each part now
//!   starts ([`crate::parquet::merge_footers`]). From object storage only the
//!   chunks the comparison reads are fetched, as for one file.
//!
//! In a directory or under a prefix, names starting with `.` or `_` are passed
//! over, at any depth (`_SUCCESS`, `_delta_log/`, `.part-0.crc`), and so are
//! empty files. A Delta or Iceberg table is refused: its directory holds
//! files of versions that are no longer the table.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{human, read_up_to, temp_file};
use crate::error::{Error, Result};

/// Whether `s` holds a glob character.
pub(crate) fn is_pattern(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Whether one path segment matches one pattern segment: `*` any run of
/// characters, `?` one, `[abc]`, `[a-z]` and `[!a]` a class. Neither `*` nor
/// `?` matches a leading `.`, as in a shell.
fn segment_matches(pattern: &[char], name: &[char]) -> bool {
    if name.first() == Some(&'.') && pattern.first() != Some(&'.') {
        return false;
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(2).position(|&c| c == ']').map(|i| i + 2) else {
                    return n.first() == Some(&'[') && go(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else {
                    return false;
                };
                let (negate, class) = match p[1] {
                    '!' | '^' => (true, &p[2..close]),
                    _ => (false, &p[1..close]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < class.len() {
                    if i + 2 < class.len() && class[i + 1] == '-' {
                        hit |= class[i] <= c && c <= class[i + 2];
                        i += 3;
                    } else {
                        hit |= class[i] == c;
                        i += 1;
                    }
                }
                hit != negate && go(&p[close + 1..], &n[1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&p[1..], &n[1..]),
        }
    }
    go(pattern, name)
}

/// Whether `path` matches `pattern`, segment by segment; `**` is any number of
/// segments, none included.
pub(crate) fn glob_matches(pattern: &str, path: &str) -> bool {
    let split = |s: &str| -> Vec<Vec<char>> {
        s.split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().collect())
            .collect()
    };
    let (p, n) = (split(pattern), split(path));
    fn go(p: &[Vec<char>], n: &[Vec<char>]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some(seg) if seg.len() == 2 && seg[0] == '*' && seg[1] == '*' => {
                (0..=n.len()).any(|i| go(&p[1..], &n[i..]))
            }
            Some(seg) => !n.is_empty() && segment_matches(seg, &n[0]) && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

/// The part of a pattern before its first segment with a glob character: the
/// directory, or the object prefix, to list.
pub(crate) fn literal_prefix(pattern: &str) -> &str {
    let first = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    match pattern[..first].rfind('/') {
        Some(slash) => &pattern[..=slash],
        None => "",
    }
}

/// Whether a path under the listed root is passed over in a directory or
/// prefix: any segment starting with `.` or `_`.
pub(crate) fn hidden(relative: &str) -> bool {
    relative
        .split('/')
        .any(|s| s.starts_with('.') || s.starts_with('_'))
}

/// A table format whose directory is not the table: what to say instead of
/// reading every file in it. `names` are the paths under the root.
/// A table format whose directory is not the table, found under the root:
/// the format and where the table is (relative to the root, empty for the
/// root itself). `names` are the paths under the root.
pub(crate) fn table_format<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Option<(&'static str, String)> {
    for n in names {
        if let Some(at) = n
            .find("_delta_log/")
            .filter(|&at| at == 0 || n[..at].ends_with('/'))
        {
            return Some(("Delta", n[..at].trim_end_matches('/').to_string()));
        }
        if n.ends_with(".metadata.json")
            && let Some(at) = n
                .find("metadata/")
                .filter(|&at| at == 0 || n[..at].ends_with('/'))
        {
            return Some(("Iceberg", n[..at].trim_end_matches('/').to_string()));
        }
    }
    None
}

/// The error for a table format found under a directory or prefix: a Delta
/// table further down is read by naming it; an Iceberg table is not read.
pub(crate) fn refuse_table(spec: &str, found: &(&str, String)) -> Error {
    let (format, at) = (found.0, &found.1);
    let sep = if spec.ends_with('/') { "" } else { "/" };
    if format == "Delta" {
        return Error::new(format!(
            "{spec} holds a Delta table at {at}/, whose directory has the files of earlier \
             versions as well as the current one; name the table to read a version of it \
             ({spec}{sep}{at} for the latest, {spec}{sep}{at}@v<n> for version n)"
        ));
    }
    Error::new(format!(
        "{spec} is an Iceberg table (or holds one), which is not read here: its directory \
         holds the files of earlier snapshots as well as the current one. Compare the files \
         of one snapshot (a glob of them, or a copy), or export the table"
    ))
}

/// The Hive partitions in a part's path below the root it was listed from:
/// `date=2026-10-01/region=eu/part-0.parquet` holds `date` and `region`.
/// Values are percent-decoded, as Hive writes them, and Hive's name for a
/// null partition is read as empty.
pub(crate) fn partitions_of(relative: &str) -> Vec<(String, String)> {
    let segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    let dirs = &segments[..segments.len().saturating_sub(1)];
    dirs.iter()
        .filter_map(|s| s.split_once('='))
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| {
            let v = percent_decode(v);
            let v = if v == "__HIVE_DEFAULT_PARTITION__" {
                String::new()
            } else {
                v
            };
            (percent_decode(k), v)
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The partition columns of a table of parts, given each part's path below
/// the root: the names every part's path has, in the same order, and each
/// part's values. Parts that do not agree have no partition columns.
pub(crate) fn table_partitions(relative: &[String]) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let all: Vec<Vec<(String, String)>> = relative.iter().map(|r| partitions_of(r)).collect();
    let names: Vec<String> = all.first()?.iter().map(|(k, _)| k.clone()).collect();
    if names.is_empty()
        || all
            .iter()
            .any(|p| p.len() != names.len() || p.iter().zip(&names).any(|((k, _), n)| k != n))
    {
        return None;
    }
    let values = all
        .into_iter()
        .map(|p| p.into_iter().map(|(_, v)| v).collect())
        .collect();
    Some((names, values))
}

/// The files a local directory or glob names, in path order; `None` for a
/// spec that is neither, read as one file.
/// Local parts with what to say about them: the Delta version they are, or
/// nothing.
pub(super) type LocalParts = (Vec<(String, PathBuf)>, String);

pub(super) fn local_files(spec: &str) -> Result<Option<LocalParts>> {
    let (table, version) = super::delta::split_version(spec);
    let log = Path::new(table).join("_delta_log");
    if log.is_dir() {
        return delta_files(spec, Path::new(table), &log, version).map(Some);
    }
    if version.is_some() {
        return Err(Error::new(format!(
            "{spec}: @v{} names a version of a Delta table, and {table} is not one",
            version.unwrap_or_default()
        )));
    }
    let path = Path::new(spec);
    let (root, pattern) = if path.is_dir() {
        (path.to_path_buf(), None)
    } else if is_pattern(spec) && !path.exists() {
        let prefix = literal_prefix(spec);
        let root = if prefix.is_empty() { "." } else { prefix };
        if !Path::new(root).is_dir() {
            return Err(Error::new(format!("no files match {spec}")));
        }
        (PathBuf::from(root), Some(spec))
    } else {
        return Ok(None);
    };
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    walk(&root, "", &mut found)?;
    if pattern.is_none()
        && let Some(found) = table_format(found.iter().map(|f| f.0.as_str()))
    {
        return Err(refuse_table(spec, &found));
    }
    let base = if pattern.is_some() {
        literal_prefix(spec)
    } else {
        ""
    };
    let mut files: Vec<(String, PathBuf)> = found
        .into_iter()
        .filter(|(relative, full)| {
            let keep = match pattern {
                // Matched against the spec as written, so `./x/*.csv` and
                // `x/*.csv` both work.
                Some(p) => glob_matches(p, &format!("{base}{relative}")),
                None => !hidden(relative),
            };
            keep && std::fs::metadata(full).is_ok_and(|m| m.len() > 0)
        })
        .collect();
    if files.is_empty() {
        return Err(Error::new(match pattern {
            Some(_) => format!("no files match {spec}"),
            None => format!("no files in {spec}"),
        }));
    }
    files.sort();
    Ok(Some((files, String::new())))
}

/// The files of one version of a local Delta table, in path order.
fn delta_files(spec: &str, table: &Path, log: &Path, version: Option<u64>) -> Result<LocalParts> {
    let names: Vec<String> = std::fs::read_dir(log)
        .map_err(|e| Error::new(format!("cannot list {}: {e}", log.display())))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    let plan = super::delta::plan(&names, version, spec)?;
    let mut read = |name: &str| -> Result<Vec<u8>> {
        std::fs::read(log.join(name))
            .map_err(|e| Error::new(format!("{spec}: cannot read the log ({name}): {e}")))
    };
    let files = super::delta::replay(&plan, &mut read, spec)?;
    let files: Result<Vec<(String, PathBuf)>> = files
        .into_iter()
        .map(|relative| {
            let full = table.join(&relative);
            if !full.is_file() {
                return Err(Error::new(format!(
                    "{spec}: version {} names {relative}, which is no longer there (vacuumed?)",
                    plan.version
                )));
            }
            Ok((relative, full))
        })
        .collect();
    Ok((files?, format!(", Delta version {}", plan.version)))
}

/// Every file under `dir`, with its path relative to the walk's root.
fn walk(dir: &Path, relative: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::new(format!("cannot list {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        let path = entry.path();
        // Followed through symlinks: a linked file or directory is read.
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => walk(&path, &rel, out)?,
            Ok(m) if m.is_file() => out.push((rel, path)),
            _ => {}
        }
    }
    Ok(())
}

/// What a part holds, by its first bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Parquet,
    Json,
    Text,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Parquet => "Parquet",
            Kind::Json => "JSON",
            Kind::Text => "CSV",
        }
    }
}

/// The compression a part's first bytes say it has, if any.
pub(crate) fn compression(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x1f, 0x8b]) {
        Some("gzip")
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some("zstd")
    } else if head.starts_with(b"BZh") {
        Some("bzip2")
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Some("xz")
    } else if head.starts_with(b"PK\x03\x04") {
        Some("zip")
    } else {
        None
    }
}

/// The length of the first record of `text`, its line ending included,
/// minding quotes: a quoted header field may hold a newline. `None` when no
/// newline ends it within `text`.
fn first_record(text: &[u8]) -> Option<usize> {
    let mut quoted = false;
    for (i, &b) in text.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// A header as compared between parts: no byte-order mark, no line ending.
fn bare_header(h: &[u8]) -> &[u8] {
    let h = h.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(h);
    let h = h.strip_suffix(b"\n").unwrap_or(h);
    h.strip_suffix(b"\r").unwrap_or(h)
}

/// The one file the parts become, written in part order.
pub(crate) struct Combined {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    /// Where the next appended part goes.
    end: u64,
    kind: Option<Kind>,
    first: String,
    header: Option<Vec<u8>>,
    /// Per Parquet part: where it starts, its metadata, its name, and its
    /// partition values.
    footers: Vec<(u64, Vec<u8>, String, Vec<String>)>,
    pub(crate) parts: usize,
    /// The table's partition columns, from its paths.
    partitions: Vec<String>,
    /// Which of them a text part gets: those its header does not have.
    text_partitions: Option<Vec<usize>>,
}

impl Combined {
    pub(crate) fn new() -> Result<Combined> {
        let (path, file) = temp_file("combined")?;
        Ok(Combined {
            path,
            file,
            end: 0,
            kind: None,
            first: String::new(),
            header: None,
            footers: Vec::new(),
            parts: 0,
            partitions: Vec::new(),
            text_partitions: None,
        })
    }

    /// The partition columns the parts' paths hold, for each part's values to
    /// be added to it as columns.
    pub(crate) fn set_partitions(&mut self, names: Vec<String>) {
        self.partitions = names;
    }

    pub(crate) fn has_partitions(&self) -> bool {
        !self.partitions.is_empty()
    }

    /// Room for `len` bytes at the end, for a part written there by someone
    /// else (a download); where it starts.
    #[cfg_attr(not(feature = "cloud"), allow(dead_code))]
    pub(crate) fn reserve(&mut self, len: u64) -> u64 {
        let at = self.end;
        self.end += len;
        at
    }

    #[cfg_attr(not(feature = "cloud"), allow(dead_code))]
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Appends a local part, decompressing a gzip or zstd one; `values` are
    /// its partition values, one per [`Combined::set_partitions`] name.
    pub(crate) fn append_file(&mut self, name: &str, path: &Path, values: &[String]) -> Result<()> {
        let mut input =
            File::open(path).map_err(|e| Error::new(format!("cannot read {name}: {e}")))?;
        let mut head = [0u8; 6];
        let n = read_up_to(&mut input, &mut head)?;
        if n == 0 {
            return Ok(());
        }
        input.seek(SeekFrom::Start(0))?;
        let base = self.end;
        self.file.seek(SeekFrom::Start(base))?;
        let out = self.file.try_clone()?;
        if self.has_partitions() && !head[..n].starts_with(b"PAR1") {
            // Text with its partition values added to every row as it is
            // copied: the one case that rewrites a part rather than copying it.
            let reader: Box<dyn Read> = match compression(&head[..n]) {
                None => Box::new(input),
                Some("gzip") => Box::new(flate2::read::MultiGzDecoder::new(
                    std::io::BufReader::with_capacity(1 << 20, input),
                )),
                Some("zstd") => Box::new(zstd::stream::read::Decoder::new(input)?),
                Some(other) => {
                    return Err(Error::new(format!(
                        "{name} is {other}-compressed, which is not read here; gzip and zstd are"
                    )));
                }
            };
            let written = self
                .with_partitions(reader, out, base, values)
                .map_err(|e| Error::new(format!("cannot read {name}: {e}")))?;
            self.end = base + written;
            return self.placed(name, base, written, false, values);
        }
        let written = match compression(&head[..n]) {
            None => std::io::copy(&mut input, &mut self.file).inspect(|&n| {
                crate::memory::write_behind(&self.file, base, n, Some((base, n)));
            }),
            Some("gzip") => {
                let mut d = flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(
                    1 << 20,
                    input,
                ));
                copy_buffered(&mut d, out, base)
            }
            Some("zstd") => zstd::stream::read::Decoder::new(input)
                .and_then(|mut d| copy_buffered(&mut d, out, base)),
            Some(other) => {
                return Err(Error::new(format!(
                    "{name} is {other}-compressed, which is not read here; gzip and zstd are"
                )));
            }
        }
        .map_err(|e| Error::new(format!("cannot read {name}: {e}")))?;
        self.end = base + written;
        self.placed(name, base, written, false, values)
    }

    /// Copies a text part to `out` at `base`, each row given the partition
    /// values as fields of its own -- the header the names -- minding quoted
    /// fields that hold newlines; JSON rows get them as their first keys.
    /// Columns the first part already has are left as they are. The bytes
    /// written.
    fn with_partitions(
        &mut self,
        input: Box<dyn Read>,
        out: File,
        base: u64,
        values: &[String],
    ) -> std::io::Result<u64> {
        let mut input = std::io::BufReader::with_capacity(1 << 20, input);
        let mut w =
            std::io::BufWriter::with_capacity(1 << 20, crate::memory::WriteBehind::at(out, base));
        let mut line = Vec::with_capacity(4096);
        let mut written = 0u64;
        let mut first = true;
        let mut quoted = false;
        let mut delimiter = b',';
        let mut json = false;
        let mut take: Vec<usize> = Vec::new();
        let mut put = |w: &mut dyn Write, bytes: &[u8]| -> std::io::Result<()> {
            written += bytes.len() as u64;
            w.write_all(bytes)
        };
        loop {
            line.clear();
            if std::io::BufRead::read_until(&mut input, b'\n', &mut line)? == 0 {
                break;
            }
            let body_end = line.len()
                - usize::from(line.ends_with(b"\n"))
                - usize::from(line.ends_with(b"\r\n"));
            let (body, ending) = line.split_at(body_end);
            let ending: &[u8] = if ending.is_empty() { b"\n" } else { ending };
            if first {
                first = false;
                let text = String::from_utf8_lossy(body);
                let trimmed = text.trim_start_matches('\u{feff}').trim_start();
                json = trimmed.starts_with('{');
                delimiter = crate::columns::detect_delimiter(&text);
                let partitions = &self.partitions;
                let has = |n: &str| {
                    if json {
                        trimmed.contains(&format!("\"{n}\":"))
                            || trimmed.contains(&format!("\"{n}\" :"))
                    } else {
                        text.split(delimiter as char)
                            .any(|c| c.trim().trim_matches('"') == n)
                    }
                };
                take = self
                    .text_partitions
                    .get_or_insert_with(|| {
                        (0..partitions.len())
                            .filter(|&i| !has(&partitions[i]))
                            .collect()
                    })
                    .clone();
                if !json {
                    put(&mut w, body)?;
                    for &i in &take {
                        put(&mut w, &[delimiter])?;
                        put(&mut w, &field(&self.partitions[i], delimiter))?;
                    }
                    put(&mut w, ending)?;
                    continue;
                }
            }
            if json {
                match body.iter().position(|b| !b.is_ascii_whitespace()) {
                    Some(at) if body[at] == b'{' && !take.is_empty() => {
                        put(&mut w, &body[..=at])?;
                        for (k, &i) in take.iter().enumerate() {
                            if k > 0 {
                                put(&mut w, b",")?;
                            }
                            put(&mut w, &json_string(&self.partitions[i]))?;
                            put(&mut w, b":")?;
                            put(&mut w, &json_string(&values[i]))?;
                        }
                        let rest = &body[at + 1..];
                        if rest.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'}') {
                            put(&mut w, b",")?;
                        }
                        put(&mut w, rest)?;
                    }
                    _ => put(&mut w, body)?,
                }
            } else {
                put(&mut w, body)?;
                quoted ^= body.iter().filter(|&&b| b == b'"').count() % 2 == 1;
                // A row ends where a line ends outside quotes; a blank line is
                // not a row and gets nothing.
                if !quoted && !body.is_empty() && body != b"\r" {
                    for &i in &take {
                        put(&mut w, &[delimiter])?;
                        put(&mut w, &field(&values[i], delimiter))?;
                    }
                }
            }
            put(&mut w, ending)?;
        }
        w.flush()?;
        Ok(written)
    }

    /// Takes in a part that is in place at `base`, `len` bytes, the byte after
    /// it reserved for a newline when `spare`: checks it is what the parts
    /// before it were, and for text blanks out its header and ends its last
    /// line. `values` are its partition values.
    pub(crate) fn placed(
        &mut self,
        name: &str,
        base: u64,
        len: u64,
        spare: bool,
        values: &[String],
    ) -> Result<()> {
        if len == 0 {
            if spare {
                write_at(&self.file, b"\n", base)?;
            }
            return Ok(());
        }
        let mut head = vec![0u8; len.min(64 << 10) as usize];
        read_at(&self.file, &mut head, base)?;
        if let Some(c) = compression(&head) {
            return Err(Error::new(format!(
                "{name} is {c}-compressed though its name does not say so; name it .gz or \
                 .zst, or decompress it"
            )));
        }
        let kind = if head.starts_with(b"PAR1") {
            Kind::Parquet
        } else {
            let start = head
                .strip_prefix(b"\xEF\xBB\xBF".as_slice())
                .unwrap_or(&head);
            match start.iter().find(|b| !b.is_ascii_whitespace()) {
                Some(b'{') => Kind::Json,
                Some(b'[') => {
                    return Err(Error::new(format!(
                        "{name} is a JSON array; the parts of one side are read as one table, \
                         and arrays cannot be (newline-delimited JSON can)"
                    )));
                }
                _ => Kind::Text,
            }
        };
        match self.kind {
            None => {
                self.kind = Some(kind);
                self.first = name.to_string();
            }
            Some(k) if k != kind => {
                return Err(Error::new(format!(
                    "{name} is {} but {} is {}; the parts of one side must be of one format",
                    kind.label(),
                    self.first,
                    k.label()
                )));
            }
            _ => {}
        }
        self.parts += 1;
        match kind {
            Kind::Parquet => {
                let mut tail = [0u8; 8];
                if len < 12 {
                    return Err(Error::new(format!("{name} is a truncated parquet file")));
                }
                read_at(&self.file, &mut tail, base + len - 8)?;
                if &tail[4..] != b"PAR1" {
                    return Err(Error::new(format!("{name} is a truncated parquet file")));
                }
                let meta_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
                if meta_len + 12 > len {
                    return Err(Error::new(format!(
                        "{name}: the parquet footer is longer than the file"
                    )));
                }
                let mut meta = vec![0u8; meta_len as usize];
                read_at(&self.file, &mut meta, base + len - 8 - meta_len)?;
                self.footers
                    .push((base, meta, name.to_string(), values.to_vec()));
                if spare {
                    write_at(&self.file, b"\0", base + len)?;
                }
                return Ok(());
            }
            Kind::Json => {}
            Kind::Text => {
                let mut header_len = first_record(&head);
                if header_len.is_none() && (head.len() as u64) < len {
                    // A header longer than the first 64 KB: read on for it.
                    let mut more = vec![0u8; len.min(16 << 20) as usize];
                    read_at(&self.file, &mut more, base)?;
                    header_len = first_record(&more);
                    head = more;
                }
                let header_len = header_len.unwrap_or(head.len().min(len as usize));
                let header = &head[..header_len];
                match &self.header {
                    None => self.header = Some(bare_header(header).to_vec()),
                    Some(first) => {
                        if bare_header(header) != first.as_slice() {
                            let show = |h: &[u8]| {
                                let s = String::from_utf8_lossy(h);
                                if s.chars().count() > 200 {
                                    format!("{}...", s.chars().take(200).collect::<String>())
                                } else {
                                    s.into_owned()
                                }
                            };
                            return Err(Error::new(format!(
                                "{name} has the header `{}` where {} has `{}`; the parts of one \
                                 side are read as one table and must have the same columns",
                                show(bare_header(header)),
                                self.first,
                                show(first)
                            )));
                        }
                        write_at(&self.file, &vec![b'\n'; header_len], base)?;
                    }
                }
            }
        }
        // A part that does not end its last line would run into the next.
        let mut last = [0u8; 1];
        read_at(&self.file, &mut last, base + len - 1)?;
        if spare {
            write_at(&self.file, b"\n", base + len)?;
        } else if last[0] != b'\n' {
            write_at(&self.file, b"\n", base + len)?;
            self.end = self.end.max(base + len + 1);
        }
        Ok(())
    }

    /// The single file, with a Parquet whole given its footer. Its size.
    pub(crate) fn finish(&mut self) -> Result<u64> {
        if self.parts == 0 {
            return Err(Error::new("every file is empty"));
        }
        if self.kind == Some(Kind::Parquet) {
            let footers = std::mem::take(&mut self.footers);
            let parts: Vec<(u64, &[u8])> = footers
                .iter()
                .map(|(b, m, ..)| (*b, m.as_slice()))
                .collect();
            let names: Vec<&str> = footers.iter().map(|f| f.2.as_str()).collect();
            let meta = if self.partitions.is_empty() {
                crate::parquet::merge_footers(&parts, &names)?
            } else {
                let values: Vec<Vec<String>> = footers.iter().map(|f| f.3.clone()).collect();
                let partitions = crate::parquet::Partitions {
                    names: &self.partitions,
                    values: &values,
                };
                let (file, end) = (&self.file, &mut self.end);
                crate::parquet::merge_footers_with(&parts, &names, &partitions, &mut |bytes| {
                    let at = *end;
                    write_at(file, bytes, at)?;
                    *end += bytes.len() as u64;
                    Ok(at)
                })?
            };
            let at = self.end;
            write_at(&self.file, &meta, at)?;
            let mut close = (meta.len() as u32).to_le_bytes().to_vec();
            close.extend_from_slice(b"PAR1");
            write_at(&self.file, &close, at + meta.len() as u64)?;
            self.end = at + meta.len() as u64 + 8;
        }
        self.file.set_len(self.end)?;
        self.file.flush()?;
        Ok(self.end)
    }

    /// One line for stderr: how many parts became how much.
    pub(crate) fn note(&self) -> String {
        let partitions = if self.partitions.is_empty() {
            String::new()
        } else {
            format!(", partitioned by {}", self.partitions.join(", "))
        };
        format!(
            "{} {} files read as one ({}{partitions})",
            self.parts,
            self.kind.map_or("", |k| k.label()),
            human(self.end)
        )
    }
}

/// A value as one delimited field, quoted when it has to be.
fn field(value: &str, delimiter: u8) -> Vec<u8> {
    if value
        .bytes()
        .any(|b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r')
    {
        format!("\"{}\"", value.replace('"', "\"\"")).into_bytes()
    } else {
        value.as_bytes().to_vec()
    }
}

fn json_string(value: &str) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"\"\"".to_vec())
}

fn copy_buffered(input: &mut impl Read, out: File, at: u64) -> std::io::Result<u64> {
    let mut w = std::io::BufWriter::with_capacity(1 << 20, crate::memory::WriteBehind::at(out, at));
    let n = std::io::copy(input, &mut w)?;
    w.flush()?;
    Ok(n)
}

#[cfg(unix)]
pub(crate) fn write_at(file: &File, bytes: &[u8], at: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(file.write_all_at(bytes, at)?)
}

#[cfg(unix)]
pub(crate) fn read_at(file: &File, buf: &mut [u8], at: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_exact_at(buf, at)?)
}

#[cfg(windows)]
pub(crate) fn write_at(file: &File, mut bytes: &[u8], mut at: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let n = file.seek_write(bytes, at)?;
        bytes = &bytes[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn read_at(file: &File, mut buf: &mut [u8], mut at: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, at)?;
        if n == 0 {
            return Err(Error::new("unexpected end of a temporary file"));
        }
        buf = &mut buf[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_a_shell() {
        assert!(glob_matches("data/*.csv", "data/a.csv"));
        assert!(!glob_matches("data/*.csv", "data/x/a.csv"));
        assert!(glob_matches("data/**/*.csv", "data/a.csv"));
        assert!(glob_matches("data/**/*.csv", "data/x/y/a.csv"));
        assert!(glob_matches(
            "data/date=2026-1[0-2]-*/part-?.parquet",
            "data/date=2026-10-01/part-3.parquet"
        ));
        assert!(!glob_matches(
            "data/date=2026-1[0-2]-*/part-?.parquet",
            "data/date=2026-09-01/part-3.parquet"
        ));
        assert!(glob_matches("x/[!a]*", "x/b1"));
        assert!(!glob_matches("x/[!a]*", "x/a1"));
        assert!(!glob_matches("x/*", "x/.hidden"));
        assert!(glob_matches("x/.*", "x/.hidden"));
        assert!(glob_matches("bucket/pre*/**", "bucket/prefix/a/b"));
    }

    #[test]
    fn prefixes_and_hidden_names() {
        assert_eq!(literal_prefix("s3://b/t/date=*/x.parquet"), "s3://b/t/");
        assert_eq!(literal_prefix("*.csv"), "");
        assert_eq!(literal_prefix("a/b/c.csv"), "a/b/");
        assert!(hidden("_SUCCESS"));
        assert!(hidden("date=1/.part-0.crc"));
        assert!(hidden("_temporary/0/part-1"));
        assert!(!hidden("date=1/part-0.parquet"));
        assert_eq!(
            table_format(["part-0.parquet", "_delta_log/00000.json"]).map(|f| f.0),
            Some("Delta")
        );
        assert_eq!(
            table_format(["data/a.parquet", "metadata/v1.metadata.json"]).map(|f| f.0),
            Some("Iceberg")
        );
        assert_eq!(table_format(["a/part-0.parquet"]), None);
    }

    #[test]
    fn partitions_come_from_key_value_directories() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            partitions_of("date=2026-10-01/region=eu%2Fwest/part-0.parquet"),
            vec![pair("date", "2026-10-01"), pair("region", "eu/west")]
        );
        assert_eq!(partitions_of("part-0.parquet"), vec![]);
        assert_eq!(partitions_of("d=__HIVE_DEFAULT_PARTITION__/x.csv")[0].1, "");
        let rel = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (names, values) =
            table_partitions(&rel(&["d=1/a.csv", "d=2/b.csv"])).expect("partitioned");
        assert_eq!(names, vec!["d".to_string()]);
        assert_eq!(values, vec![vec!["1".to_string()], vec!["2".to_string()]]);
        assert!(table_partitions(&rel(&["d=1/a.csv", "e=2/b.csv"])).is_none());
        assert!(table_partitions(&rel(&["d=1/a.csv", "b.csv"])).is_none());
    }

    #[test]
    fn headers_are_found_past_quoted_newlines() {
        assert_eq!(first_record(b"a,b\n1,2\n"), Some(4));
        assert_eq!(first_record(b"\"a\nx\",b\r\n1,2"), Some(9));
        assert_eq!(first_record(b"a,b"), None);
        assert_eq!(bare_header(b"\xEF\xBB\xBFa,b\r\n"), b"a,b");
    }
}
