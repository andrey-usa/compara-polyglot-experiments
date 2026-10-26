//! Named comparison profiles from `compara.toml`, so a recurring comparison does
//! not get retyped.
//!
//! ```toml
//! [profiles.orders]
//! key       = ["order_id", "line_no"]
//! compare   = ["qty", "price", "status"]   # omit for all common non-key columns
//! ignore    = ["updated_at"]
//! trim      = true
//! tolerance = 0.005
//! ```
//!
//! The file format is shared with the other implementations.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::options::Options;

/// One profile. Every field is optional because "absent" and "false" mean
/// different things: only the keys actually present override the defaults.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Profile {
    pub key: Option<Vec<String>>,
    pub compare: Option<Vec<String>>,
    pub ignore: Option<Vec<String>>,
    pub trim: Option<bool>,
    pub ignore_case: Option<bool>,
    pub empty_is_null: Option<bool>,
    pub tolerance: Option<f64>,
    pub max_rows: Option<usize>,
    pub delimiter: Option<char>,
    pub encoding: Option<String>,
    pub engine: Option<String>,
}

impl Profile {
    /// Lays the profile down under the command line, which then overrides it.
    pub fn apply_to(&self, opt: &mut Options) {
        if let Some(v) = &self.key {
            opt.key = v.clone();
        }
        if let Some(v) = &self.compare {
            opt.compare = Some(v.clone());
        }
        if let Some(v) = &self.ignore {
            opt.ignore = v.clone();
        }
        if let Some(v) = self.trim {
            opt.trim = v;
        }
        if let Some(v) = self.ignore_case {
            opt.ignore_case = v;
        }
        if let Some(v) = self.empty_is_null {
            opt.empty_is_null = v;
        }
        if let Some(v) = self.tolerance {
            opt.tolerance = v;
        }
        if let Some(v) = self.max_rows {
            opt.max_rows = v;
        }
        if let Some(v) = self.delimiter {
            opt.delimiter = Some(v);
        }
        if let Some(v) = &self.encoding {
            opt.encoding = v.clone();
        }
        if let Some(v) = &self.engine {
            opt.engine = v.clone();
        }
    }
}

/// Where a config file is looked for when no path is given.
pub fn search_path() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("compara.toml")];
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".config/compara/compara.toml"));
    }
    paths
}

/// Reads the profile tables from a config file, or returns nothing when there is none.
///
/// This reads the subset of TOML the config format actually uses — `[profiles.x]`
/// tables of scalars and string arrays — rather than pulling in a parser for a
/// file that is a dozen lines long.
pub fn load(explicit: Option<&str>) -> Result<HashMap<String, Profile>> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(p) => vec![PathBuf::from(p)],
        None => search_path(),
    };
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| Error::new(format!("cannot read {}: {e}", path.display())))?;
        return parse(&text)
            .map_err(|e| Error::new(format!("cannot parse {}: {e}", path.display())));
    }
    match explicit {
        Some(p) => Err(Error::new(format!("config file not found: {p}"))),
        None => Ok(HashMap::new()),
    }
}

/// Parses the profile tables out of a config file's text.
pub fn parse(text: &str) -> Result<HashMap<String, Profile>> {
    let mut out: HashMap<String, Profile> = HashMap::new();
    let mut current: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name
                .trim()
                .strip_prefix("profiles.")
                .map(|n| n.trim_matches('"').to_string());
            if let Some(name) = &current {
                out.entry(name.clone()).or_default();
            }
            continue;
        }
        let Some(name) = &current else { continue };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let profile = out.entry(name.clone()).or_default();
        assign(profile, key.trim(), strip_comment(value).trim())?;
    }
    Ok(out)
}

fn assign(profile: &mut Profile, key: &str, raw: &str) -> Result<()> {
    let number =
        |what: &str| -> Error { Error::new(format!("{what} must be a number, got: {raw}")) };
    match key {
        "key" => profile.key = Some(toml_array(raw)),
        "compare" => profile.compare = Some(toml_array(raw)),
        "ignore" => profile.ignore = Some(toml_array(raw)),
        "trim" => profile.trim = Some(raw == "true"),
        "ignore_case" => profile.ignore_case = Some(raw == "true"),
        "empty_is_null" => profile.empty_is_null = Some(raw == "true"),
        "tolerance" => profile.tolerance = Some(raw.parse().map_err(|_| number("tolerance"))?),
        "max_rows" => profile.max_rows = Some(raw.parse().map_err(|_| number("max_rows"))?),
        "delimiter" => profile.delimiter = toml_string(raw).chars().next(),
        "encoding" => profile.encoding = Some(toml_string(raw)),
        "engine" => profile.engine = Some(toml_string(raw)),
        _ => {}
    }
    Ok(())
}

/// Removes a trailing `#` comment, which cannot appear inside a value here
/// because every value is a scalar or a string array.
fn strip_comment(value: &str) -> &str {
    let mut in_string = false;
    for (i, c) in value.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => return &value[..i],
            _ => {}
        }
    }
    value
}

fn toml_string(raw: &str) -> String {
    raw.trim().trim_matches(['"', '\'']).to_string()
}

fn toml_array(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(toml_string)
        .filter(|s| !s.is_empty())
        .collect()
}
