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
//! Top-level lines are `set`, `base`, `use`, `flow`, `auth`, `before
//! all|each`, `after each|all` and `scenario`; the indented lines under a
//! block are its steps. A step is a navigera command line (tokenized like a
//! shell, without expansion), an `expect` line, `save NAME = <command>`,
//! `run`, `as`, `optional`, `if` or `handoff`.

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
    /// `run <flow> <args…>`.
    Run {
        flow: String,
        args: Vec<String>,
    },
    /// `optional <step>`: a failure is noted, not fatal (a banner that may
    /// or may not show).
    Optional(Box<Action>),
    /// `if <expect clause>` and the steps indented under it: run them when
    /// the clause holds now (or within its `--timeout-ms`).
    If {
        cond: Expect,
        steps: Vec<Step>,
    },
    /// `as <auth>`: start signed in, from the `auth` block's saved state.
    As(String),
    /// `handoff [until <expect clause>]`: the browser on screen for a
    /// person (a 2FA prompt, a CAPTCHA), until the clause holds or Enter.
    Handoff(Option<Expect>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub line: usize,
    /// The line as written (trimmed), for reports.
    pub text: String,
    pub action: Action,
    /// How far the line is indented: the steps of an `if` are indented
    /// more than the `if`.
    pub indent: usize,
}

/// Steps read in order, nested by indentation under their `if`s.
fn nest(flat: Vec<Step>) -> Result<Vec<Step>, (usize, String)> {
    fn level(items: &[Step], i: &mut usize, indent: usize) -> Result<Vec<Step>, (usize, String)> {
        let mut out = Vec::new();
        while *i < items.len() {
            let st = &items[*i];
            if st.indent < indent {
                break;
            }
            if st.indent > indent {
                return Err((st.line, "indented more than the step before it (only the steps of an `if` are)".into()));
            }
            *i += 1;
            let mut st = st.clone();
            if let Action::If { steps, .. } = &mut st.action {
                match items.get(*i) {
                    Some(next) if next.indent > indent => *steps = level(items, i, next.indent)?,
                    _ => return Err((st.line, "`if` needs the steps it guards indented under it".into())),
                }
            }
            out.push(st);
        }
        Ok(out)
    }
    if flat.is_empty() {
        return Ok(flat);
    }
    let mut i = 0;
    let base = flat[0].indent;
    let out = level(&flat, &mut i, base)?;
    match flat.get(i) {
        Some(st) => Err((st.line, "indented less than the first step of its block".into())),
        None => Ok(out),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub tags: Vec<String>,
    pub line: usize,
    pub steps: Vec<Step>,
    /// A data row's values (`examples`, `for each <file.csv>`).
    pub vars: BTreeMap<String, String>,
}

/// `auth name [ttl 30m]`: steps that sign in, run once (or reused from
/// the last run within the TTL), whose cookies and storage `as name` loads.
#[derive(Debug, Clone, PartialEq)]
pub struct Auth {
    pub name: String,
    /// How long the saved state is reused across runs (seconds); none: once
    /// per run.
    pub ttl_s: Option<u64>,
    pub file: PathBuf,
    pub line: usize,
    pub steps: Vec<Step>,
}

/// `30m`, `1h`, `90s`, `2d`, or plain seconds.
pub fn duration_s(text: &str) -> Option<u64> {
    let (num, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let n: u64 = num.parse().ok()?;
    Some(
        n * match unit {
            "" | "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => return None,
        },
    )
}

/// `flow name(a, b)`: steps a scenario runs with `run name x y`.
#[derive(Debug, Clone, PartialEq)]
pub struct Flow {
    pub name: String,
    pub params: Vec<String>,
    pub file: PathBuf,
    pub line: usize,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Suite {
    pub file: PathBuf,
    pub base: Option<String>,
    pub vars: BTreeMap<String, String>,
    /// Once for the file, before its first scenario; what it saves the
    /// scenarios see.
    pub before_all: Vec<Step>,
    pub before_each: Vec<Step>,
    pub after_each: Vec<Step>,
    /// Once for the file, after its last scenario.
    pub after_all: Vec<Step>,
    pub scenarios: Vec<Scenario>,
    pub flows: BTreeMap<String, Flow>,
    pub auths: BTreeMap<String, Auth>,
    /// `use <file.pv>`: files whose flows and `set`s this one takes.
    pub uses: Vec<(usize, PathBuf)>,
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
            let bare = name.strip_prefix("secret:").unwrap_or(name);
            if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
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

/// Replace the `${NAME}`s that `vars` has, leave the others as written.
fn interpolate_known(word: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = word;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let body = &rest[i + 2..];
        match body.find('}') {
            Some(end) if vars.contains_key(&body[..end]) => {
                out.push_str(&vars[&body[..end]]);
                rest = &body[end + 1..];
            }
            _ => {
                out.push_str("${");
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A table row: `| a | b |` -> ["a", "b"].
fn table_row(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Rows of a CSV file (first line: the column names). Double quotes around
/// a field allow commas in it (`""` for a quote).
pub fn csv_rows(text: &str) -> Result<Vec<BTreeMap<String, String>>, String> {
    let split = |line: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => out.push(std::mem::take(&mut cur).trim().to_string()),
                c => cur.push(c),
            }
        }
        out.push(cur.trim().to_string());
        out
    };
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = split(lines.next().ok_or("the file is empty")?);
    let mut rows = Vec::new();
    for (i, l) in lines.enumerate() {
        let cells = split(l);
        if cells.len() != header.len() {
            return Err(format!("row {} has {} fields, the header {}", i + 2, cells.len(), header.len()));
        }
        rows.push(header.iter().cloned().zip(cells).collect());
    }
    Ok(rows)
}

/// The `${NAME}`s a word uses (not `$${…}`), and whether each has a
/// default (`${NAME:-x}`).
pub fn used_vars(word: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = word;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let Some(end) = body.find('}') else { break };
            let inner = &body[..end];
            match inner.split_once(":-") {
                Some((n, _)) => out.push((n.to_string(), true)),
                None => out.push((inner.to_string(), false)),
            }
            rest = &body[end + 1..];
        } else {
            rest = after;
        }
    }
    out
}

/// Variables a scenario uses that nothing sets: not the file's `set`s or
/// its `use`d files', not `--var`, not its data row, not a `save` before
/// the use, not the environment. (line, name) for each.
pub fn unset_vars(suite: &Suite, sc: &Scenario, run_vars: &[String]) -> Vec<(usize, String)> {
    let mut known: std::collections::BTreeSet<String> = suite.vars.keys().cloned().collect();
    known.extend(run_vars.iter().cloned());
    known.extend(sc.vars.keys().cloned());
    known.extend(["run.id", "scenario.name", "download.path", "download.name"].map(String::from));
    for st in &suite.before_all {
        if let Action::Save { name, .. } = &st.action {
            known.insert(name.clone());
        }
    }
    let mut out = Vec::new();
    fn words(a: &Action) -> Vec<String> {
        match a {
            Action::Command(w) => w.clone(),
            Action::Save { command, .. } => command.clone(),
            Action::Run { args, .. } => args.clone(),
            Action::Optional(inner) => words(inner),
            Action::As(name) => vec![name.clone()],
            Action::Expect(_) | Action::If { .. } | Action::Handoff(_) => Vec::new(),
        }
    }
    for st in suite.before_each.iter().chain(&sc.steps) {
        // An expect's words are in its text.
        let ws = match &st.action {
            Action::Expect(_) => tokenize(&st.text).unwrap_or_default(),
            a => words(a),
        };
        for w in &ws {
            for (name, has_default) in used_vars(w) {
                let env = std::env::var_os(name.strip_prefix("secret:").unwrap_or(&name)).is_some();
                if !has_default && !known.contains(&name) && !env {
                    out.push((st.line, name));
                }
            }
        }
        if let Action::Save { name, .. } = &st.action {
            known.insert(name.clone());
        }
        // What a flow saves is known after it runs.
        if let Action::Run { flow, .. } = &st.action {
            if let Some(f) = suite.flows.get(flow) {
                for fs in &f.steps {
                    if let Action::Save { name, .. } = &fs.action {
                        known.insert(name.clone());
                    }
                }
            }
        }
    }
    out
}

/// Parse one file's text.
pub fn parse(file: &Path, text: &str) -> Result<Suite, ParseError> {
    let err = |line: usize, message: String| ParseError { file: file.to_path_buf(), line, message };
    let mut suite = Suite { file: file.to_path_buf(), ..Suite::default() };
    enum Block {
        None,
        BeforeAll,
        BeforeEach,
        AfterEach,
        AfterAll,
        Scenario,
        Flow(String),
        Auth(String),
    }
    let mut block = Block::None;
    // Data rows of the scenario being read: (line of `examples`, header, rows).
    let mut rows: BTreeMap<usize, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    let mut table: Option<(usize, Option<Vec<String>>)> = None;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent_of(raw) > 0 {
            // `examples` and the table under it.
            if let Some((si, header)) = table.as_mut() {
                if trimmed.starts_with('|') {
                    let cells = table_row(trimmed);
                    match header {
                        None => *header = Some(cells),
                        Some(h) if h.len() == cells.len() => {
                            rows.entry(*si).or_default().push(h.iter().cloned().zip(cells).collect())
                        }
                        Some(h) => {
                            return Err(err(
                                line_no,
                                format!("{} cells in this row, {} in the header", cells.len(), h.len()),
                            ))
                        }
                    }
                    continue;
                }
                table = None;
            }
            if trimmed == "examples" {
                match block {
                    Block::Scenario => {
                        table = Some((suite.scenarios.len() - 1, None));
                        continue;
                    }
                    _ => return Err(err(line_no, "`examples` belongs under a scenario".into())),
                }
            }
            let mut step = parse_step(trimmed).map_err(|m| err(line_no, m))?;
            anchor_inputs(&mut step, file.parent().unwrap_or(Path::new(".")));
            let step = Step { line: line_no, text: trimmed.to_string(), action: step, indent: indent_of(raw) };
            match &block {
                Block::None => {
                    return Err(err(line_no, "a step outside any `scenario`, `flow` or `before each` block".into()))
                }
                Block::BeforeAll => suite.before_all.push(step),
                Block::BeforeEach => suite.before_each.push(step),
                Block::AfterEach => suite.after_each.push(step),
                Block::AfterAll => suite.after_all.push(step),
                Block::Scenario => suite.scenarios.last_mut().expect("a scenario is open").steps.push(step),
                Block::Flow(name) => suite.flows.get_mut(name).expect("the flow is open").steps.push(step),
                Block::Auth(name) => suite.auths.get_mut(name).expect("the auth is open").steps.push(step),
            }
            continue;
        }
        table = None;
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
            Some("use") => {
                let path = words.get(1).ok_or_else(|| err(line_no, "expected `use <file.pv>`".into()))?;
                let dir = file.parent().unwrap_or(Path::new("."));
                suite.uses.push((line_no, dir.join(path)));
                block = Block::None;
            }
            Some(w @ ("before" | "after")) => {
                let which = words.get(1).map(String::as_str);
                if words.len() != 2 || !matches!(which, Some("each" | "all")) {
                    return Err(err(line_no, format!("expected `{w} each` or `{w} all`")));
                }
                let (b, taken) = match (w, which) {
                    ("before", Some("all")) => (Block::BeforeAll, !suite.before_all.is_empty()),
                    ("before", _) => (Block::BeforeEach, !suite.before_each.is_empty()),
                    ("after", Some("all")) => (Block::AfterAll, !suite.after_all.is_empty()),
                    _ => (Block::AfterEach, !suite.after_each.is_empty()),
                };
                if taken {
                    return Err(err(line_no, format!("a second `{trimmed}` block: put its steps in the first")));
                }
                block = b;
            }
            Some("auth") => {
                // auth name [ttl 30m]
                let name = words.get(1).ok_or_else(|| err(line_no, "expected `auth <name> [ttl 30m]`".into()))?.clone();
                if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    return Err(err(line_no, format!("bad auth name {name:?}")));
                }
                let ttl_s = match &words[2..] {
                    [] => None,
                    [t, d] if t == "ttl" => Some(
                        duration_s(d)
                            .ok_or_else(|| err(line_no, format!("ttl takes a duration like 30m, 1h, 90s, 2d (got {d:?})")))?,
                    ),
                    other => return Err(err(line_no, format!("after the auth's name: `ttl <duration>` or nothing (got {other:?})"))),
                };
                if suite.auths.contains_key(&name) {
                    return Err(err(line_no, format!("a second auth named {name:?}")));
                }
                suite.auths.insert(
                    name.clone(),
                    Auth { name: name.clone(), ttl_s, file: file.to_path_buf(), line: line_no, steps: Vec::new() },
                );
                block = Block::Auth(name);
            }
            Some("flow") => {
                // flow name(a, b)   or   flow name
                let rest = trimmed["flow".len()..].trim();
                let (name, params) = match rest.split_once('(') {
                    Some((n, p)) => {
                        let p = p.strip_suffix(')').ok_or_else(|| err(line_no, "expected `flow name(a, b)`".into()))?;
                        (n.trim(), p.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                    }
                    None => (rest, Vec::new()),
                };
                let ok = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if !ok(name) || !params.iter().all(|p: &String| ok(p)) {
                    return Err(err(line_no, format!("bad flow header {rest:?} (expected `flow name(a, b)`)")));
                }
                if suite.flows.contains_key(name) {
                    return Err(err(line_no, format!("a second flow named {name:?}")));
                }
                suite.flows.insert(
                    name.to_string(),
                    Flow { name: name.to_string(), params, file: file.to_path_buf(), line: line_no, steps: Vec::new() },
                );
                block = Block::Flow(name.to_string());
            }
            Some("scenario") => {
                let name = words.get(1).ok_or_else(|| err(line_no, "expected `scenario \"name\"`".into()))?.clone();
                let mut tags = Vec::new();
                let mut k = 2;
                while k < words.len() {
                    let w = &words[k];
                    if w == "for" && words.get(k + 1).map(String::as_str) == Some("each") {
                        let csv = words.get(k + 2).ok_or_else(|| err(line_no, "expected `for each <file.csv>`".into()))?;
                        let path = file.parent().unwrap_or(Path::new(".")).join(csv);
                        let text = std::fs::read_to_string(&path)
                            .map_err(|e| err(line_no, format!("reading {}: {e}", path.display())))?;
                        let data = csv_rows(&text).map_err(|m| err(line_no, format!("{}: {m}", path.display())))?;
                        rows.entry(suite.scenarios.len()).or_default().extend(data);
                        k += 3;
                        continue;
                    }
                    match w.strip_prefix('@') {
                        Some(t) if !t.is_empty() => tags.push(t.to_string()),
                        _ => return Err(err(line_no, format!("after the name, only @tags or `for each <file.csv>`: {w:?}"))),
                    }
                    k += 1;
                }
                if suite.scenarios.iter().any(|s| s.name == name) {
                    return Err(err(line_no, format!("a second scenario named {name:?}")));
                }
                suite.scenarios.push(Scenario { name, tags, line: line_no, steps: Vec::new(), vars: BTreeMap::new() });
                block = Block::Scenario;
            }
            Some(other) => {
                return Err(err(
                    line_no,
                    format!("unknown line {other:?} (top level: set, base, use, flow, auth, before all|each, after each|all, scenario; steps are indented)"),
                ))
            }
            None => {}
        }
    }
    let nested = |steps: Vec<Step>| nest(steps).map_err(|(line, m)| err(line, m));
    suite.before_all = nested(std::mem::take(&mut suite.before_all))?;
    suite.before_each = nested(std::mem::take(&mut suite.before_each))?;
    suite.after_each = nested(std::mem::take(&mut suite.after_each))?;
    suite.after_all = nested(std::mem::take(&mut suite.after_all))?;
    for a in suite.auths.values_mut() {
        a.steps = nested(std::mem::take(&mut a.steps))?;
        if a.steps.is_empty() {
            return Err(err(a.line, format!("auth {:?} has no steps", a.name)));
        }
    }
    for sc in suite.scenarios.iter_mut() {
        sc.steps = nested(std::mem::take(&mut sc.steps))?;
    }
    for f in suite.flows.values_mut() {
        f.steps = nested(std::mem::take(&mut f.steps))?;
    }
    for s in &suite.scenarios {
        if s.steps.is_empty() {
            return Err(err(s.line, format!("scenario {:?} has no steps", s.name)));
        }
    }
    for f in suite.flows.values() {
        if f.steps.is_empty() {
            return Err(err(f.line, format!("flow {:?} has no steps", f.name)));
        }
    }
    // One scenario per data row, named from the row.
    if !rows.is_empty() {
        let mut out = Vec::new();
        for (i, sc) in suite.scenarios.drain(..).enumerate() {
            let Some(data) = rows.get(&i) else {
                out.push(sc);
                continue;
            };
            if data.is_empty() {
                return Err(err(sc.line, format!("scenario {:?}: its data has no rows", sc.name)));
            }
            for (k, row) in data.iter().enumerate() {
                let mut name = interpolate_known(&sc.name, row);
                if name == sc.name {
                    name = format!("{} [{}]", sc.name, k + 1);
                }
                out.push(Scenario { name, vars: row.clone(), ..sc.clone() });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for sc in &out {
            if !seen.insert(sc.name.clone()) {
                return Err(err(
                    sc.line,
                    format!("two data rows name the scenario {:?}: put a column in its name", sc.name),
                ));
            }
        }
        suite.scenarios = out;
    }
    Ok(suite)
}

/// `use` files merged in: their flows and `set`s, then this file's own on
/// top. Checks that every `run` names a flow with the right arguments.
pub fn resolve(suites: &mut [Suite]) -> Result<(), ParseError> {
    for suite in suites.iter_mut() {
        let mut seen = std::collections::BTreeSet::new();
        let mut todo: Vec<(usize, PathBuf)> = suite.uses.clone();
        while let Some((line, path)) = todo.pop() {
            let key = std::fs::canonicalize(&path).unwrap_or(path.clone());
            if !seen.insert(key) {
                continue;
            }
            let err = |m: String| ParseError { file: suite.file.clone(), line, message: m };
            let text = std::fs::read_to_string(&path).map_err(|e| err(format!("use {}: {e}", path.display())))?;
            let used = parse(&path, &text)?;
            for (name, flow) in used.flows {
                suite.flows.entry(name).or_insert(flow);
            }
            for (name, auth) in used.auths {
                suite.auths.entry(name).or_insert(auth);
            }
            for (k, v) in used.vars {
                suite.vars.entry(k).or_insert(v);
            }
            todo.extend(used.uses);
        }
        // Every `run`, wherever it is, names a known flow; every `as` a
        // known auth.
        fn check(steps: &[Step], file: &Path, suite: &Suite) -> Result<(), ParseError> {
            let flows = &suite.flows;
            for st in steps {
                let action = match &st.action {
                    Action::Optional(a) => a.as_ref(),
                    Action::If { steps, .. } => {
                        check(steps, file, suite)?;
                        continue;
                    }
                    a => a,
                };
                if let Action::As(name) = action {
                    if !suite.auths.contains_key(name) {
                        return Err(ParseError {
                            file: file.to_path_buf(),
                            line: st.line,
                            message: format!("no auth named {name:?} (define it with `auth {name}` or `use` its file)"),
                        });
                    }
                }
                if let Action::Run { flow, args } = action {
                    let err = |m: String| ParseError { file: file.to_path_buf(), line: st.line, message: m };
                    let f = flows.get(flow).ok_or_else(|| {
                        err(format!("no flow named {flow:?} (define it with `flow {flow}(…)` or `use` its file)"))
                    })?;
                    if f.params.len() != args.len() {
                        return Err(err(format!(
                            "flow {flow} takes {} argument(s) ({}), not {}",
                            f.params.len(),
                            f.params.join(", "),
                            args.len()
                        )));
                    }
                }
            }
            Ok(())
        }
        let file = suite.file.clone();
        for steps in [&suite.before_all, &suite.before_each, &suite.after_each, &suite.after_all] {
            check(steps, &file, suite)?;
        }
        for sc in &suite.scenarios {
            check(&sc.steps, &file, suite)?;
        }
        for f in suite.flows.values() {
            check(&f.steps, &f.file, suite)?;
        }
        for a in suite.auths.values() {
            check(&a.steps, &a.file, suite)?;
        }
    }
    Ok(())
}

/// Files a step reads (`upload`, `state load`, `eval --file`, `init-script
/// --file`, `route --body-file`) are found next to the `.pv` file, as `use`
/// and `for each` are, so a suite runs the same from any directory. Absolute
/// paths, `-` (stdin) and paths that start with a variable are left alone.
fn anchor_inputs(action: &mut Action, dir: &Path) {
    let words = match action {
        Action::Command(w) | Action::Save { command: w, .. } => w,
        Action::Optional(inner) => return anchor_inputs(inner, dir),
        _ => return,
    };
    let anchor = |w: &mut String| {
        if !w.is_empty() && w != "-" && !w.starts_with("${") && Path::new(w.as_str()).is_relative() {
            *w = dir.join(w.as_str()).to_string_lossy().into_owned();
        }
    };
    let flag_value = |words: &mut [String], flag: &str| {
        let mut i = 1;
        while i + 1 < words.len() {
            if words[i] == flag {
                anchor(&mut words[i + 1]);
                i += 1;
            }
            i += 1;
        }
    };
    match words.first().map(String::as_str) {
        Some("upload") => {
            // `upload [<target>] --file a [--file b] [c d …]`: every flag
            // takes a value; the first bare word is the target when no
            // locator flag names one, the rest are files.
            const TARGET: &[&str] = &[
                "--selector",
                "--ref",
                "--text",
                "--find",
                "--role",
                "--name",
                "--label",
                "--placeholder",
                "--testid",
                "--nth",
                "--frame",
            ];
            let mut target_open = !words.iter().any(|w| TARGET.contains(&w.as_str()));
            let mut i = 1;
            while i < words.len() {
                if words[i] == "--file" {
                    if let Some(v) = words.get_mut(i + 1) {
                        anchor(v);
                    }
                    i += 2;
                } else if words[i].starts_with("--") {
                    i += 2;
                } else if target_open {
                    target_open = false;
                    i += 1;
                } else {
                    anchor(&mut words[i]);
                    i += 1;
                }
            }
        }
        Some("state") if words.get(1).is_some_and(|w| w == "load") => {
            if let Some(path) = words.get_mut(2) {
                anchor(path);
            }
        }
        Some("eval" | "evaluate" | "init-script" | "addinitscript") => flag_value(words, "--file"),
        Some("route") => flag_value(words, "--body-file"),
        _ => {}
    }
}

fn parse_step(line: &str) -> Result<Action, String> {
    let words = tokenize(line)?;
    parse_words(&words)
}

fn parse_words(words: &[String]) -> Result<Action, String> {
    match words.first().map(String::as_str) {
        Some("expect") => Ok(Action::Expect(crate::expect::parse(&words[1..])?)),
        Some("save") => {
            // save NAME = <command…>
            if words.len() < 4 || words[2] != "=" {
                return Err("expected `save NAME = <command>`".into());
            }
            Ok(Action::Save { name: words[1].clone(), command: words[3..].to_vec() })
        }
        Some("run") => {
            let flow = words.get(1).ok_or("expected `run <flow> <args…>`")?.clone();
            Ok(Action::Run { flow, args: words[2..].to_vec() })
        }
        Some("if") => {
            if words.len() < 2 {
                return Err("expected `if <expect clause>` (as after `expect`), then indented steps".into());
            }
            let mut cond = crate::expect::parse(&words[1..])?;
            // A condition looks at the page as it is, unless it says how long.
            cond.timeout_ms = Some(cond.timeout_ms.unwrap_or(0));
            Ok(Action::If { cond, steps: Vec::new() })
        }
        Some("as") => match words {
            [_, name] => Ok(Action::As(name.clone())),
            _ => Err("expected `as <auth name>`".into()),
        },
        Some("handoff") => match words.get(1).map(String::as_str) {
            None => Ok(Action::Handoff(None)),
            Some("until") if words.len() > 2 => {
                let mut cond = crate::expect::parse(&words[2..])?;
                // A person takes a while: five minutes unless it says.
                cond.timeout_ms = Some(cond.timeout_ms.unwrap_or(300_000));
                Ok(Action::Handoff(Some(cond)))
            }
            _ => Err("expected `handoff` or `handoff until <expect clause>`".into()),
        },
        Some("optional") => {
            if words.len() < 2 {
                return Err("expected `optional <step>`".into());
            }
            match parse_words(&words[1..])? {
                Action::Optional(_) => Err("`optional optional`".into()),
                a => Ok(Action::Optional(Box::new(a))),
            }
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
            Ok(Action::Command(words.to_vec()))
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

    #[test]
    fn flows_rows_and_optional_steps() {
        let text = "flow sign-in(email, password)\n  goto /login\n  fill --label Email ${email}\n\nscenario \"Ship to ${country}\"\n  examples\n    | country | fee |\n    | Canada  | $9  |\n    | Mexico  | $12 |\n  run sign-in a@b.c pw\n  optional click --selector #banner\n  expect text ${fee}\n";
        let mut s = parse(Path::new("t.pv"), text).unwrap();
        assert_eq!(s.flows["sign-in"].params, ["email", "password"]);
        assert_eq!(
            s.scenarios.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["Ship to Canada", "Ship to Mexico"]
        );
        assert_eq!(s.scenarios[1].vars["fee"], "$12");
        assert!(
            matches!(&s.scenarios[0].steps[0].action, Action::Run { flow, args } if flow == "sign-in" && args.len() == 2)
        );
        assert!(matches!(&s.scenarios[0].steps[1].action, Action::Optional(_)));
        resolve(std::slice::from_mut(&mut s)).unwrap();
        // A wrong flow call is a file error, with its line.
        let mut bad = parse(Path::new("t.pv"), "flow f(a)\n  goto /\nscenario \"x\"\n  run f\n").unwrap();
        let e = resolve(std::slice::from_mut(&mut bad)).unwrap_err();
        assert_eq!(e.line, 4);
        assert!(e.message.contains("takes 1 argument"), "{e}");
        let mut bad = parse(Path::new("t.pv"), "scenario \"x\"\n  run nowhere\n").unwrap();
        assert!(resolve(std::slice::from_mut(&mut bad)).unwrap_err().message.contains("no flow named"));
        // Rows that would give two scenarios one name.
        let e = parse(
            Path::new("t.pv"),
            "scenario \"Same ${a}\"\n  examples\n    | a | b |\n    | 1 | x |\n    | 1 | y |\n  goto /\n",
        )
        .unwrap_err();
        assert!(e.message.contains("two data rows"), "{e}");
        // A name without a column gets the row's number.
        let s =
            parse(Path::new("t.pv"), "scenario \"Plain\"\n  examples\n    | a |\n    | 1 |\n    | 2 |\n  goto /${a}\n")
                .unwrap();
        assert_eq!(s.scenarios[1].name, "Plain [2]");
    }

    #[test]
    fn files_a_step_reads_are_found_next_to_the_suite() {
        let dir = Path::new("suites").join("shop");
        let text =
            "scenario \"Files\"\n  upload --selector \"#f\" a.txt --file b.txt /abs/c.txt\n  upload #g d.txt\n  \
                    upload --frame iframe e.txt\n  optional state load s.json\n  eval --file x.js\n  \
                    route **/api --body-file body.json\n  upload --file ${download.path}\n  eval -\n  \
                    fill --selector a.txt b.txt\n";
        let suite = parse(&dir.join("t.pv"), text).unwrap();
        let at = |p: &str| dir.join(p).to_string_lossy().into_owned();
        let words: Vec<Vec<String>> = suite.scenarios[0]
            .steps
            .iter()
            .map(|s| match &s.action {
                Action::Command(w) => w.clone(),
                Action::Optional(a) => match a.as_ref() {
                    Action::Command(w) => w.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect();
        let abs = if cfg!(windows) { at("/abs/c.txt") } else { "/abs/c.txt".to_string() };
        assert_eq!(words[0], ["upload", "--selector", "#f", &at("a.txt"), "--file", &at("b.txt"), &abs]);
        assert_eq!(words[1], ["upload", "#g", &at("d.txt")]);
        assert_eq!(words[2], ["upload", "--frame", "iframe", &at("e.txt")]);
        assert_eq!(words[3], ["state", "load", &at("s.json")]);
        assert_eq!(words[4], ["eval", "--file", &at("x.js")]);
        assert_eq!(words[5], ["route", "**/api", "--body-file", &at("body.json")]);
        assert_eq!(words[6], ["upload", "--file", "${download.path}"]);
        assert_eq!(words[7], ["eval", "-"]);
        assert_eq!(words[8], ["fill", "--selector", "a.txt", "b.txt"]);
        // The step's text stays as written, for messages.
        assert!(suite.scenarios[0].steps[0].text.contains(" a.txt "));
    }

    #[test]
    fn csv_and_use_files() {
        let rows = csv_rows("email,password\nada@x.io,\"p,w\"\n\"bo \"\"b\"\"\",z\n").unwrap();
        assert_eq!(rows[0]["password"], "p,w");
        assert_eq!(rows[1]["email"], "bo \"b\"");
        assert!(csv_rows("a,b\n1\n").is_err());
        let dir = std::env::temp_dir().join(format!("pv-use-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("flows")).unwrap();
        std::fs::write(dir.join("flows/auth.pv"), "set who = ada\nflow hello\n  goto /\n").unwrap();
        std::fs::write(dir.join("logins.csv"), "user\nada\nbo\n").unwrap();
        let main = dir.join("main.pv");
        std::fs::write(&main, "use flows/auth.pv\nscenario \"Hi ${user}\" for each logins.csv @data\n  run hello\n")
            .unwrap();
        let mut s = parse(&main, &std::fs::read_to_string(&main).unwrap()).unwrap();
        resolve(std::slice::from_mut(&mut s)).unwrap();
        assert_eq!(s.scenarios.len(), 2);
        assert_eq!(s.scenarios[0].tags, ["data"]);
        assert_eq!(s.vars["who"], "ada");
        assert!(s.flows.contains_key("hello"));
    }

    #[test]
    fn lint_finds_variables_nothing_sets() {
        let text = "set a = 1\nflow f\n  save got = get text --selector h1\nscenario \"x\"\n  goto /${a}\n  goto /${row}\n  run f\n  expect ${got} is ${b:-2}\n  expect ${missing} is 1\n  save later = get title\n  goto /${later}\n";
        let s = parse(Path::new("t.pv"), text).unwrap();
        let un = unset_vars(&s, &s.scenarios[0], &[]);
        assert_eq!(un, [(6, "row".to_string()), (9, "missing".to_string())]);
        assert_eq!(unset_vars(&s, &s.scenarios[0], &["row".into(), "missing".into()]), []);
        assert_eq!(used_vars("$${x}-${y}-${z:-1}"), [("y".to_string(), false), ("z".to_string(), true)]);
    }

    #[test]
    fn if_blocks_nest_by_indentation() {
        let text = "scenario \"x\"\n  goto /\n  if --selector \"#banner\" visible\n    click --selector \"#close\"\n    if text \"More\"\n      click --text More\n  expect title is t\n";
        let s = parse(Path::new("t.pv"), text).unwrap();
        let steps = &s.scenarios[0].steps;
        assert_eq!(steps.len(), 3);
        let Action::If { cond, steps: inner } = &steps[1].action else { panic!("{:?}", steps[1]) };
        assert_eq!(cond.timeout_ms, Some(0));
        assert_eq!(inner.len(), 2);
        assert!(matches!(&inner[1].action, Action::If { steps, .. } if steps.len() == 1));
        let e = parse(Path::new("t.pv"), "scenario \"x\"\n  goto /\n    click --text a\n").unwrap_err();
        assert_eq!(e.line, 3);
        let e = parse(Path::new("t.pv"), "scenario \"x\"\n  if text a\n  goto /\n").unwrap_err();
        assert!(e.message.contains("needs the steps"), "{e}");
    }
}
