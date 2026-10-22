//! Column resolution, normalisation, cell equality and key ordering.
//!
//! These rules are what make the engines interchangeable, so they live in one
//! place rather than in each backend.

use std::cmp::Ordering;

use crate::contract::{EngineMeta, Val};
use crate::error::{Error, Result};
use crate::options::Options;

/// Which columns take part, and which exist on only one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub compared: Vec<String>,
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
}

impl Resolved {
    /// Turns the resolution into the contract's [`EngineMeta`].
    pub fn meta(&self, key: &[String], a_cols: usize, b_cols: usize) -> EngineMeta {
        EngineMeta {
            key: key.to_vec(),
            compared: self.compared.clone(),
            only_in_a: self.only_in_a.clone(),
            only_in_b: self.only_in_b.clone(),
            a_cols,
            b_cols,
        }
    }
}

/// Works out the compared columns from the two headers.
pub fn resolve(a_cols: &[String], b_cols: &[String], opt: &Options) -> Result<Resolved> {
    let missing: Vec<&String> = opt
        .key
        .iter()
        .filter(|k| !a_cols.contains(k) || !b_cols.contains(k))
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        return Err(Error::new(format!(
            "key column(s) missing from one of the files: {}",
            names.join(", ")
        )));
    }

    // A name that is in neither file is a typo, and a typo here is the one that
    // does not announce itself: `--key` makes the answer impossible and
    // `--compare` refuses, but a misspelled `--ignore` silently compares the
    // column it was meant to drop and reports it as changed on every row. It
    // cost a benchmark run, and a `-i x,y,z` against a `z2` column sat in this
    // repository's own C suite until the check went in.
    //
    // In *neither* file, not in both: `--ignore` is subtractive, so naming a
    // column that only one side has is a real name doing no harm -- it is not
    // compared either way. Only a name nothing has can be a mistake.
    let unknown: Vec<&str> = opt
        .ignore
        .iter()
        .filter(|c| !a_cols.contains(c) && !b_cols.contains(c))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(Error::new(format!(
            "ignore column(s) present in neither file: {}",
            unknown.join(", ")
        )));
    }

    let common: Vec<String> = a_cols
        .iter()
        .filter(|c| b_cols.contains(c))
        .cloned()
        .collect();

    let requested: Vec<String> = match &opt.compare {
        None => common.clone(),
        Some(wanted) => {
            let bad: Vec<&str> = wanted
                .iter()
                .filter(|c| !common.contains(c))
                .map(String::as_str)
                .collect();
            if !bad.is_empty() {
                return Err(Error::new(format!(
                    "compare column(s) not present in both files: {}",
                    bad.join(", ")
                )));
            }
            wanted.clone()
        }
    };

    let compared: Vec<String> = requested
        .into_iter()
        .filter(|c| !opt.key.contains(c) && !opt.ignore.contains(c))
        .collect();

    Ok(Resolved {
        compared,
        only_in_a: a_cols
            .iter()
            .filter(|c| !b_cols.contains(c))
            .cloned()
            .collect(),
        only_in_b: b_cols
            .iter()
            .filter(|c| !a_cols.contains(c))
            .cloned()
            .collect(),
    })
}

/// Treats an empty field as absent, quoted or not.
///
/// This is what DuckDB's reader does, and the other implementations follow it,
/// so every engine here must too or a count would change with the engine.
pub fn empty_to_null(s: &str) -> Val {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Applies `--trim`, `--ignore-case` and `--empty-is-null` to one cell.
pub fn normalise(v: Val, opt: &Options) -> Val {
    let mut s = v?;
    if opt.trim {
        s = s.trim().to_string();
    }
    if opt.ignore_case {
        s = s.to_lowercase();
    }
    if opt.empty_is_null && s.is_empty() {
        return None;
    }
    Some(s)
}

/// Parses a cell as a number, for `--tolerance`.
///
/// Deliberately stricter than [`f64::from_str`]: `inf` and `nan` are ordinary
/// text in a CSV, and treating them as numbers would make two unequal strings
/// compare equal.
fn as_number(v: &Val) -> Option<f64> {
    let s = v.as_deref()?.trim();
    if s.is_empty() {
        return None;
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if !body.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    if !body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// What a column holds, as far as comparing its text goes. A file that stores
/// typed values (Parquet) renders each in one fixed spelling, which another
/// file's text need not share: `31.40` is the 31.4 a Parquet DOUBLE stores,
/// and `2026-09-01 02:15:00` the timestamp it renders `2026-09-01T02:15:00`.
/// Such a column is compared by value wherever both cells parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    Text,
    Number,
    Instant,
}

impl Typed {
    /// The kind a compared column gets when its two files say `a` and `b`.
    pub fn of_pair(a: Typed, b: Typed) -> Typed {
        if a == Typed::Number || b == Typed::Number {
            Typed::Number
        } else if a == Typed::Instant || b == Typed::Instant {
            Typed::Instant
        } else {
            Typed::Text
        }
    }

    /// Whether two present cells' bytes, unequal as text, hold the same value.
    pub fn same_value(self, a: &[u8], b: &[u8]) -> bool {
        let (Ok(a), Ok(b)) = (std::str::from_utf8(a), std::str::from_utf8(b)) else {
            return false;
        };
        match self {
            Typed::Text => false,
            Typed::Number => matches!((decimal(a), decimal(b)), (Some(x), Some(y)) if x == y),
            Typed::Instant => matches!((instant(a), instant(b)), (Some(x), Some(y)) if x == y),
        }
    }

    /// [`differs`] for a column of this kind.
    pub fn differs(self, a: &Val, b: &Val, opt: &Options) -> bool {
        match self {
            Typed::Text => differs(a, b, opt),
            Typed::Number if opt.tolerance > 0.0 => differs(a, b, opt),
            Typed::Number => match (
                a.as_deref().and_then(decimal),
                b.as_deref().and_then(decimal),
            ) {
                (Some(x), Some(y)) => x != y,
                _ => differs(a, b, opt),
            },
            Typed::Instant => match (
                a.as_deref().and_then(instant),
                b.as_deref().and_then(instant),
            ) {
                (Some(x), Some(y)) => x != y,
                _ => differs(a, b, opt),
            },
        }
    }
}

/// A number as its exact decimal value: the sign, the significant digits and
/// the power of ten they are scaled by, so `31.40`, `31.4` and `3.14e1` are one
/// value and `-0` is `0`. Exact rather than an `f64`, because two integers past
/// 2^53 (a 19-digit id) or two DECIMAL(38) values can differ in a digit an
/// `f64` does not keep. `None` for text that is not a number.
fn decimal(s: &str) -> Option<(bool, Vec<u8>, i64)> {
    let (negative, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], Some(&body[at + 1..])),
        None => (body, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |t: &str| t.bytes().all(|c| c.is_ascii_digit());
    if whole.len() + fraction.len() == 0 || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let mut scale: i64 = match exponent {
        Some(e) => {
            let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
            if digits.is_empty() || digits.len() > 9 || !all_digits(digits) {
                return None;
            }
            e.parse().ok()?
        }
        None => 0,
    };
    scale -= fraction.len() as i64;
    let mut digits: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes())
        .skip_while(|&c| c == b'0')
        .collect();
    while digits.last() == Some(&b'0') {
        digits.pop();
        scale += 1;
    }
    if digits.is_empty() {
        return Some((false, digits, 0));
    }
    Some((negative, digits, scale))
}

/// A date, time or timestamp in one spelling: `T` between date and time, no
/// trailing zeros in the fraction, no UTC marker (`Z`, `+00:00`). `None` for
/// text that is not one.
fn instant(s: &str) -> Option<String> {
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    let date = b.len() >= 10
        && digits_in(b, 0..4)
        && b[4] == b'-'
        && digits_in(b, 5..7)
        && b[7] == b'-'
        && digits_in(b, 8..10);
    let (date_part, time_part) = if date && b.len() == 10 {
        (&s[..10], "")
    } else if date && b.len() >= 19 && (b[10] == b' ' || b[10] == b'T') {
        (&s[..10], &s[11..])
    } else if !date {
        ("", s)
    } else {
        return None;
    };
    let mut time = String::new();
    if !time_part.is_empty() {
        let t = time_part.as_bytes();
        if !(t.len() >= 8
            && digits_in(t, 0..2)
            && t[2] == b':'
            && digits_in(t, 3..5)
            && t[5] == b':'
            && digits_in(t, 6..8))
        {
            return None;
        }
        let fraction = &time_part[8..];
        let fraction = match fraction.strip_prefix('.') {
            Some(f) if f.bytes().all(|c| c.is_ascii_digit()) => f.trim_end_matches('0'),
            None if fraction.is_empty() => "",
            _ => return None,
        };
        time.push_str(&time_part[..8]);
        if !fraction.is_empty() {
            time.push('.');
            time.push_str(fraction);
        }
    }
    Some(format!("{date_part}T{time}"))
}

fn digits_in(b: &[u8], r: std::ops::Range<usize>) -> bool {
    b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit))
}

/// SQL's `IS DISTINCT FROM`, with the numeric tolerance applied where both sides
/// parse as numbers.
///
/// Two absent values are equal; one absent value differs from any present one.
pub fn differs(a: &Val, b: &Val, opt: &Options) -> bool {
    if a.is_none() && b.is_none() {
        return false;
    }
    if opt.tolerance > 0.0
        && let (Some(x), Some(y)) = (as_number(a), as_number(b))
    {
        return (x - y).abs() > opt.tolerance;
    }
    a != b
}

/// Orders key values the way DuckDB orders `VARCHAR`: ascending, with absent
/// values last.
pub fn compare_keys(x: &[Val], y: &[Val], key_size: usize) -> Ordering {
    for i in 0..key_size {
        match (&x[i], &y[i]) {
            (None, None) => continue,
            (None, Some(_)) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Less,
            (Some(a), Some(b)) => match a.cmp(b) {
                Ordering::Equal => continue,
                other => return other,
            },
        }
    }
    Ordering::Equal
}

/// Flattens a composite key into one string for hashing.
///
/// `\x00` marks an absent value and `\x01` separates columns; neither can appear
/// in a CSV field, so distinct keys cannot collide.
pub fn key_of(row: &[Val], key_size: usize) -> String {
    let mut out = String::new();
    for cell in row.iter().take(key_size) {
        match cell {
            None => out.push('\u{0}'),
            Some(s) => out.push_str(s),
        }
        out.push('\u{1}');
    }
    out
}

/// Guesses the delimiter from the header line, defaulting to a comma.
///
/// Ties go to the earliest candidate, so a header with no delimiter at all is a
/// comma — the same choice the other implementations make.
pub fn detect_delimiter(header_line: &str) -> u8 {
    let mut best = b',';
    let mut best_count = 0;
    for candidate in *b",;\t|" {
        let count = header_line.matches(candidate as char).count();
        if count > best_count {
            best = candidate;
            best_count = count;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn opt(key: &[&str], ignore: &[&str]) -> Options {
        Options {
            key: names(key),
            ignore: names(ignore),
            ..Options::default()
        }
    }

    /// The fault this check exists for. `--key` makes the answer impossible and
    /// `--compare` refuses, but a misspelled `--ignore` used to compare the
    /// column it meant to drop and call it changed on every row -- in silence.
    #[test]
    fn an_ignore_name_in_neither_file_is_refused() {
        let a = names(&["x", "y", "id", "z2"]);
        let b = a.clone();
        let err = resolve(&a, &b, &opt(&["id"], &["x", "y", "z"])).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("present in neither file"), "{text}");
        assert!(
            text.contains('z'),
            "the name has to be in the message: {text}"
        );
    }

    /// `--ignore` is subtractive, so a name only one side carries is real and
    /// does no harm: that column is not compared either way. Refusing it would
    /// break the ordinary case of one flag set across files whose schemas
    /// differ at the edges.
    #[test]
    fn an_ignore_name_in_one_file_is_allowed() {
        let a = names(&["id", "shared", "only_a"]);
        let b = names(&["id", "shared", "only_b"]);
        for name in ["only_a", "only_b"] {
            let r = resolve(&a, &b, &opt(&["id"], &[name]));
            assert!(r.is_ok(), "{name} is a real column: {:?}", r.err());
        }
    }

    #[test]
    fn a_real_ignore_name_still_drops_its_column() {
        let a = names(&["id", "keep", "drop"]);
        let b = a.clone();
        let r = resolve(&a, &b, &opt(&["id"], &["drop"])).expect("a real name");
        assert_eq!(r.compared, names(&["keep"]));
    }

    /// The check must not fire on an empty `--ignore`, which is the default.
    #[test]
    fn no_ignore_at_all_is_fine() {
        let a = names(&["id", "v"]);
        let r = resolve(&a, &a.clone(), &opt(&["id"], &[])).expect("no ignore");
        assert_eq!(r.compared, names(&["v"]));
    }

    /// It applies with `--compare` given too: the name is still a typo, even
    /// where the comparison set was named explicitly and `--ignore` is unused.
    #[test]
    fn it_applies_even_when_compare_is_explicit() {
        let a = names(&["id", "v", "w"]);
        let mut o = opt(&["id"], &["nope"]);
        o.compare = Some(names(&["v"]));
        assert!(resolve(&a, &a.clone(), &o).is_err());
    }
    #[test]
    fn an_instant_has_one_spelling() {
        let same = [
            ("2026-09-01 02:15:00", "2026-09-01T02:15:00"),
            ("2026-09-01T02:15:00Z", "2026-09-01 02:15:00"),
            ("2026-09-01 02:15:00+00:00", "2026-09-01T02:15:00"),
            ("2026-09-01 02:15:00.000", "2026-09-01T02:15:00"),
            ("2026-09-01 02:15:00.250", "2026-09-01T02:15:00.25"),
            ("02:15:00.0", "02:15:00"),
        ];
        for (a, b) in same {
            assert!(
                Typed::Instant.same_value(a.as_bytes(), b.as_bytes()),
                "{a} vs {b}"
            );
        }
        let different = [
            ("2026-09-01 02:15:00", "2026-09-01T02:15:01"),
            ("2026-09-01 02:15:00", "2026-08-01T02:15:00"),
            ("2026-09-01 02:15:00.1", "2026-09-01T02:15:00.01"),
            ("2026-09-01 02:15:00+02:00", "2026-09-01T02:15:00"),
            ("2026-09-01", "2026-09-01T00:00:00"),
            ("not a time", "not a time "),
            ("", "Z"),
        ];
        for (a, b) in different {
            assert!(
                !Typed::Instant.same_value(a.as_bytes(), b.as_bytes()),
                "{a} vs {b}"
            );
        }
    }

    #[test]
    fn a_number_is_compared_by_value_only_where_both_cells_parse() {
        let opt = Options::default();
        let v = |s: &str| Some(s.to_string());
        assert!(!Typed::Number.differs(&v("31.40"), &v("31.4"), &opt));
        assert!(!Typed::Number.differs(&v("1e10"), &v("10000000000"), &opt));
        assert!(!Typed::Number.differs(&v("-0.0"), &v("0"), &opt));
        assert!(!Typed::Number.differs(&v("+1.50E-3"), &v("0.0015"), &opt));
        // Past 2^53 an f64 holds both of these as the same number.
        assert!(Typed::Number.differs(&v("9007199254740993"), &v("9007199254740992"), &opt));
        assert!(Typed::Number.differs(
            &v("12345678901234567890.12"),
            &v("12345678901234567890.13"),
            &opt
        ));
        assert!(!Typed::Number.same_value(b"9007199254740993", b"9007199254740992"));
        assert!(Typed::Number.differs(&v(" 1"), &v("1"), &opt));
        assert!(Typed::Number.differs(&v("1e"), &v("1"), &opt));
        assert!(Typed::Number.differs(&v("."), &v("0"), &opt));
        assert!(Typed::Number.differs(&v("31.41"), &v("31.4"), &opt));
        assert!(Typed::Number.differs(&v("n/a"), &v("N/A"), &opt));
        assert!(Typed::Text.differs(&v("31.40"), &v("31.4"), &opt));
    }
}
