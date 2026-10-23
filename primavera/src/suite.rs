//! Scenario files (`.pv`): plain text, one step per line.
//!
//! ```text
//! # comment
//! set coupon = SAVE10
//! before each
//!   goto /shop
//! scenario "A coupon lowers the total" @smoke
//!   fill --label "Coupon code" ${coupon}
//!   click --role button --name "Apply coupon"
//!   expect --testid total text is "$36.00"
//! ```
//!
//! Top-level lines are `set`, `base`, `before each`, `after each` and
//! `scenario`; the indented lines under a block are its steps. A step is a
//! navigera command line (tokenized like a shell, without expansion), an
//! `expect` line, or `save NAME = <command>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::expect::Expect;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A navigera command line.
    Command(Vec<String>),
    Expect(Expect),
    /// `save NAME = <command>`: the command's result as a variable.
    Save {
        name: String,
        command: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub line: usize,
    /// The line as written (trimmed), for reports.
    pub text: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub tags: Vec<String>,
    pub line: usize,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Suite {
    pub file: PathBuf,
    pub base: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub before_each: Vec<Step>,
    pub after_each: Vec<Step>,
    pub scenarios: Vec<Scenario>,
}

/// A problem in a file, with where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file.display(), self.line, self.message)
    }
}

/// Split a line the way a shell would, without expanding anything: words
/// separated by blanks, `'…'` taken as is, `"…"` with `\"`, `\\` and `\$`
/// escapes, a backslash outside quotes escaping the next character.
pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("a quote ' is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => cur.push(ch),
                            // `\$` stays escaped for interpolation to see.
                            Some('$') => cur.push_str("$$"),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err("a quote \" is not closed".into()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("a quote \" is not closed".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('$') => cur.push_str("$$"),
                    Some(ch) => cur.push(ch),
                    None => {}
                }
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Replace `${NAME}` and `${NAME:-default}` in a word, looking names up with
/// `lookup`; `$$` is a literal `$`.
pub fn interpolate(word: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let end = body.find('}').ok_or_else(|| format!("`${{` without `}}` in {word:?}"))?;
            let inner = &body[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                return Err(format!("bad variable name {name:?} in {word:?}"));
            }
            match lookup(name).or_else(|| default.map(str::to_string)) {
                Some(v) => out.push_str(&v),
                None => return Err(format!("unknown variable ${{{name}}}")),
            }
            rest = &body[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Parse one file's text.
pub fn parse(file: &Path, text: &str) -> Result<Suite, ParseError> {
    let err = |line: usize, message: String| ParseError { file: file.to_path_buf(), line, message };
    let mut suite = Suite { file: file.to_path_buf(), ..Suite::default() };
    enum Block {
        None,
        BeforeEach,
        AfterEach,
        Scenario,
    }
    let mut block = Block::None;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent_of(raw) > 0 {
            let step = parse_step(trimmed).map_err(|m| err(line_no, m))?;
            let step = Step { line: line_no, text: trimmed.to_string(), action: step };
            match block {
                Block::None => return Err(err(line_no, "a step outside any `scenario` or `before each` block".into())),
                Block::BeforeEach => suite.before_each.push(step),
                Block::AfterEach => suite.after_each.push(step),
                Block::Scenario => suite.scenarios.last_mut().expect("a scenario is open").steps.push(step),
            }
            continue;
        }
        let words = tokenize(trimmed).map_err(|m| err(line_no, m))?;
        match words.first().map(String::as_str) {
            Some("set") => {
                // set NAME = value…
                let rest = trimmed["set".len()..].trim();
                let (name, value) = rest
                    .split_once('=')
                    .ok_or_else(|| err(line_no, "expected `set NAME = value`".into()))?;
                let name = name.trim();
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(err(line_no, format!("bad variable name {name:?}")));
                }
                let value = tokenize(value.trim()).map_err(|m| err(line_no, m))?.join(" ");
                suite.vars.insert(name.to_string(), value);
                block = Block::None;
            }
            Some("base") => {
                let url = words.get(1).ok_or_else(|| err(line_no, "expected `base <url>`".into()))?;
                suite.base = Some(url.clone());
                block = Block::None;
            }
            Some("before") if words.get(1).map(String::as_str) == Some("each") => block = Block::BeforeEach,
            Some("after") if words.get(1).map(String::as_str) == Some("each") => block = Block::AfterEach,
            Some("scenario") => {
                let name = words.get(1).ok_or_else(|| err(line_no, "expected `scenario \"name\"`".into()))?.clone();
                let mut tags = Vec::new();
                for w in &words[2..] {
                    match w.strip_prefix('@') {
                        Some(t) if !t.is_empty() => tags.push(t.to_string()),
                        _ => return Err(err(line_no, format!("after the name, only @tags: {w:?}"))),
                    }
                }
                if suite.scenarios.iter().any(|s| s.name == name) {
                    return Err(err(line_no, format!("a second scenario named {name:?}")));
                }
                suite.scenarios.push(Scenario { name, tags, line: line_no, steps: Vec::new() });
                block = Block::Scenario;
            }
            Some(other) => {
                return Err(err(
                    line_no,
                    format!("unknown line {other:?} (top level: set, base, before each, after each, scenario; steps are indented)"),
                ))
            }
            None => {}
        }
    }
    for s in &suite.scenarios {
        if s.steps.is_empty() {
            return Err(err(s.line, format!("scenario {:?} has no steps", s.name)));
        }
    }
    Ok(suite)
}

fn parse_step(line: &str) -> Result<Action, String> {
    let words = tokenize(line)?;
    match words.first().map(String::as_str) {
        Some("expect") => Ok(Action::Expect(crate::expect::parse(&words[1..])?)),
        Some("save") => {
            // save NAME = <command…>
            if words.len() < 4 || words[2] != "=" {
                return Err("expected `save NAME = <command>`".into());
            }
            Ok(Action::Save { name: words[1].clone(), command: words[3..].to_vec() })
        }
        Some(op) => {
            const TAKES_A_REF: &[&str] =
                &["click", "dblclick", "hover", "fill", "select", "check", "uncheck", "focus", "upload", "drag"];
            if TAKES_A_REF.contains(&op)
                && words.get(1).is_some_and(|w| w.trim_start_matches(['@', 'e']).parse::<u64>().is_ok())
            {
                // `click 12`: a ref from an `ax` of some earlier session.
                return Err(format!(
                    "refs ({}) change between runs: use a locator (--role/--name, --label, --text, --testid, --selector)",
                    words[1]
                ));
            }
            Ok(Action::Command(words))
        }
        None => Err("empty step".into()),
    }
}

/// The `.pv` files under each path (a file is taken as is).
pub fn discover(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().and_then(|e| e.to_str()) == Some("pv") {
                out.push(p);
            }
        }
        Ok(())
    }
    for p in paths {
        if p.is_dir() {
            walk(p, &mut out)?;
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_like_a_shell_without_expansion() {
        assert_eq!(
            tokenize(r#"fill --label "Coupon code" SAVE10"#).unwrap(),
            ["fill", "--label", "Coupon code", "SAVE10"]
        );
        assert_eq!(tokenize(r#"expect text 'It''s'"#).unwrap(), ["expect", "text", "Its"]);
        assert_eq!(tokenize(r#"a "x \"y\" \\ z""#).unwrap(), ["a", r#"x "y" \ z"#]);
        assert_eq!(
            tokenize(r##"click --selector "#cart .item:nth-child(2)""##).unwrap()[2],
            "#cart .item:nth-child(2)"
        );
        assert_eq!(tokenize(r#"expect text "$36.00""#).unwrap()[2], "$36.00");
        assert!(tokenize(r#"a "open"#).is_err());
    }

    #[test]
    fn variables_interpolate_with_defaults_and_escapes() {
        let look = |n: &str| (n == "who").then(|| "Ada Lovelace".to_string());
        assert_eq!(interpolate("hi ${who}!", &look).unwrap(), "hi Ada Lovelace!");
        assert_eq!(interpolate("${nope:-fallback}", &look).unwrap(), "fallback");
        assert_eq!(interpolate("$36.00", &look).unwrap(), "$36.00");
        assert_eq!(interpolate("$${who}", &look).unwrap(), "${who}");
        assert!(interpolate("${nope}", &look).unwrap_err().contains("unknown variable ${nope}"));
        // `\$` in quotes keeps the dollar literal.
        let w = tokenize(r#""\${who}""#).unwrap();
        assert_eq!(interpolate(&w[0], &look).unwrap(), "${who}");
    }

    #[test]
    fn a_file_parses_into_blocks_and_steps() {
        let text = "# shop\nbase http://127.0.0.1:8000\nset coupon = SAVE10\n\nbefore each\n  goto /shop\n\nscenario \"Add one\" @smoke @cart\n  click --text \"Add to cart\"\n  expect --selector #cart-count text is 1\n  save n = get text --selector #cart-count\n\nscenario \"Docs\"\n  goto /docs\n";
        let s = parse(Path::new("t.pv"), text).unwrap();
        assert_eq!(s.base.as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(s.vars["coupon"], "SAVE10");
        assert_eq!(s.before_each.len(), 1);
        assert_eq!(s.scenarios.len(), 2);
        assert_eq!(s.scenarios[0].tags, ["smoke", "cart"]);
        assert_eq!(s.scenarios[0].steps[0].line, 9);
        assert!(matches!(s.scenarios[0].steps[1].action, Action::Expect(_)));
        assert!(matches!(&s.scenarios[0].steps[2].action, Action::Save { name, .. } if name == "n"));
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        let e = parse(Path::new("t.pv"), "scenario \"A\"\n  click 12\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("refs"), "{e}");
        let e = parse(Path::new("t.pv"), "  goto /\n").unwrap_err();
        assert!(e.message.contains("outside"), "{e}");
        let e = parse(Path::new("t.pv"), "scenario \"A\"\n  goto /\nscenario \"A\"\n  goto /\n").unwrap_err();
        assert_eq!(e.line, 3);
        let e = parse(Path::new("t.pv"), "scenario \"Empty\"\n").unwrap_err();
        assert!(e.message.contains("no steps"), "{e}");
        let e = parse(Path::new("t.pv"), "scenario \"A\"\n  expect url sounds-like x\n").unwrap_err();
        assert_eq!(e.line, 2);
    }
}
