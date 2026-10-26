//! `primavera fix`: put a suggestion from the last run's "nearest on the
//! page" into the step that failed. Every run keeps its failures and their
//! suggestions in `.primavera/last-run.json`; `fix <file>:<line> --use 2`
//! swaps that step's locator flags for the second one, `fix --all` takes
//! each failure's first when it scored `--min` (0.6) or more, and plain
//! `fix` lists what there is.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::runner::ScenarioResult;
use crate::suite::tokenize;

const LAST_RUN: &str = ".primavera/last-run.json";

/// A suggestion that is a step to put before the failing one (an
/// `optional click` on what covers its element), not a locator.
pub const BEFORE: &str = "before: ";

/// The flags that say which element a step acts on (`--frame` stays: it
/// says where to look, not what).
const LOCATOR: &[&str] =
    &["--selector", "--text", "--role", "--name", "--label", "--placeholder", "--testid", "--near", "--nth"];

/// Keep the run's failures and their suggestions for `fix`.
pub fn save(results: &[ScenarioResult]) {
    let failures: Vec<Value> = results
        .iter()
        .filter_map(|r| r.failure.as_ref().map(|f| (r, f)))
        .map(|(r, f)| {
            // A flow's step is fixed where it is written.
            let (file, line, text) = f.source.clone().unwrap_or_else(|| (r.file.clone(), f.line, f.text.clone()));
            json!({
                "file": std::path::absolute(&file).unwrap_or(file),
                "scenario": r.name,
                "line": line,
                "text": text,
                "message": f.message,
                "hint": f.hint,
                "screenshot": f.screenshot,
                "nearest": f.nearest.iter().map(|(e, l, p)| json!({"element": e, "locator": l, "p": p})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let _ = std::fs::create_dir_all(".primavera");
    let _ =
        std::fs::write(LAST_RUN, serde_json::to_string_pretty(&json!({ "failures": failures })).unwrap_or_default());
}

/// A word as the tokenizer reads it back.
pub fn quote(w: &str) -> String {
    if !w.is_empty() && !w.chars().any(|c| c.is_whitespace() || "'\"\\".contains(c)) {
        return w.to_string();
    }
    let breaks = w.contains(['\n', '\r']);
    if w.contains('"') && !w.contains('\'') && !breaks {
        return format!("'{w}'");
    }
    format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

/// The step `line` with its locator flags replaced by `locator`'s.
pub fn rewrite(line: &str, locator: &str) -> Result<String, String> {
    let indent = &line[..line.len() - line.trim_start().len()];
    let words = tokenize(line.trim())?;
    let new = tokenize(locator)?;
    let mut out = Vec::new();
    let mut at = None;
    let mut i = 0;
    while i < words.len() {
        if LOCATOR.contains(&words[i].as_str()) && i + 1 < words.len() {
            at.get_or_insert(out.len());
            i += 2;
            continue;
        }
        out.push(words[i].clone());
        i += 1;
    }
    let at = at.ok_or("the step has no locator to replace (--role/--name, --text, --label, --selector, …)")?;
    out.splice(at..at, new);
    Ok(format!("{indent}{}", out.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")))
}

struct Failure {
    file: PathBuf,
    line: usize,
    text: String,
    nearest: Vec<(String, String, f64)>,
}

/// The last run's failures as it kept them (`primavera_failure`).
pub fn last_run() -> Result<Value, String> {
    let text = std::fs::read_to_string(LAST_RUN).map_err(|_| format!("no {LAST_RUN}: run the tests first"))?;
    serde_json::from_str(&text).map_err(|e| format!("{LAST_RUN}: {e}"))
}

/// The failures that have suggestions.
fn load() -> Result<Vec<Failure>, String> {
    let v = last_run()?;
    Ok(v["failures"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| Failure {
            file: PathBuf::from(f["file"].as_str().unwrap_or("")),
            line: f["line"].as_u64().unwrap_or(0) as usize,
            text: f["text"].as_str().unwrap_or("").to_string(),
            nearest: f["nearest"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|n| {
                    (
                        n["element"].as_str().unwrap_or("").to_string(),
                        n["locator"].as_str().unwrap_or("").to_string(),
                        n["p"].as_f64().unwrap_or(0.0),
                    )
                })
                .collect(),
        })
        .filter(|f| !f.nearest.is_empty())
        .collect())
}

fn shown(p: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| p.display().to_string())
}

/// Apply suggestion `n` (1-based) to one failure's line in its file.
fn apply(f: &Failure, n: usize) -> Result<String, String> {
    if f.file.extension().is_some_and(|e| e == "feature") {
        return Err(format!(
            "{}:{} is a .feature step: change the `step` block it runs, or its phrase",
            shown(&f.file),
            f.line
        ));
    }
    let (_, locator, _) = f
        .nearest
        .get(n.wrapping_sub(1))
        .ok_or_else(|| format!("{}:{} has {} suggestion(s), not {n}", shown(&f.file), f.line, f.nearest.len()))?;
    let text = std::fs::read_to_string(&f.file).map_err(|e| format!("{}: {e}", f.file.display()))?;
    let mut lines: Vec<&str> = text.split('\n').collect();
    let old = lines.get(f.line.wrapping_sub(1)).copied().unwrap_or("");
    if old.trim_end_matches('\r').trim() != f.text {
        return Err(format!(
            "{}:{} isn't `{}` anymore (edited since, or the step is in a file it uses): nothing changed",
            shown(&f.file),
            f.line,
            f.text
        ));
    }
    let cr = if old.ends_with('\r') { "\r" } else { "" };
    if let Some(step) = locator.strip_prefix(BEFORE) {
        let indent = &old[..old.len() - old.trim_start().len()];
        let new = format!("{indent}{step}{cr}");
        let said = format!("{}:{}\n  + {}\n    {}\n", shown(&f.file), f.line, new.trim(), old.trim());
        lines.insert(f.line - 1, &new);
        std::fs::write(&f.file, lines.join("\n")).map_err(|e| format!("{}: {e}", f.file.display()))?;
        return Ok(said);
    }
    let new = rewrite(old.trim_end_matches('\r'), locator)? + cr;
    let said = format!("{}:{}\n  - {}\n  + {}\n", shown(&f.file), f.line, old.trim(), new.trim());
    lines[f.line - 1] = &new;
    std::fs::write(&f.file, lines.join("\n")).map_err(|e| format!("{}: {e}", f.file.display()))?;
    Ok(said)
}

/// `primavera fix [<file>:<line>] [--use <n>] [--all [--min <p>]]`.
pub fn main(args: &[String]) -> Result<String, String> {
    let failures = load()?;
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let n: usize = match flag("--use") {
        Some(v) => v.parse().map_err(|_| format!("--use takes a number, not {v:?}"))?,
        None => 1,
    };
    let min: f64 = match flag("--min") {
        Some(v) => v.parse().map_err(|_| format!("--min takes a number from 0 to 1, not {v:?}"))?,
        None => 0.6,
    };
    let at = args
        .iter()
        .find(|a| !a.starts_with("--") && a.contains(':') && Some(*a) != flag("--use") && Some(*a) != flag("--min"));
    if let Some(at) = at {
        let (file, line) = at.rsplit_once(':').ok_or("expected <file>:<line>")?;
        let line: usize = line.parse().map_err(|_| format!("expected <file>:<line>, not {at:?}"))?;
        let want = std::path::absolute(file).unwrap_or_else(|_| PathBuf::from(file));
        let f = failures
            .iter()
            .find(|f| f.line == line && f.file == want)
            .ok_or_else(|| format!("the last run has no suggestion for {at}"))?;
        return apply(f, n);
    }
    if args.iter().any(|a| a == "--all") {
        let mut out = String::new();
        for f in &failures {
            match f.nearest.first() {
                Some((_, _, p)) if *p >= min => match apply(f, 1) {
                    Ok(s) => out.push_str(&s),
                    Err(e) => out.push_str(&format!("{e}\n")),
                },
                _ => out.push_str(&format!("{}:{} kept: no suggestion scored {min} or more\n", shown(&f.file), f.line)),
            }
        }
        return Ok(if out.is_empty() { "nothing to fix\n".into() } else { out });
    }
    let mut out = String::new();
    for f in &failures {
        out.push_str(&format!("{}:{}  {}\n", shown(&f.file), f.line, f.text));
        for (i, (e, l, p)) in f.nearest.iter().enumerate() {
            out.push_str(&format!("  {}. {e}   {l}   {p:.2}\n", i + 1));
        }
        out.push_str(&format!("  primavera fix {}:{} --use 1\n", shown(&f.file), f.line));
    }
    Ok(if out.is_empty() { "the last run left nothing to fix\n".into() } else { out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_locator_changes() {
        assert_eq!(
            rewrite(
                "  click --role button --name \"Accept everything\" --timeout-ms 500",
                "--role button --name \"Accept all\""
            )
            .unwrap(),
            "  click --role button --name \"Accept all\" --timeout-ms 500"
        );
        assert_eq!(
            rewrite("    expect --text Total visible", "--role heading --name 'Order total'").unwrap(),
            "    expect --role heading --name \"Order total\" visible"
        );
        assert_eq!(
            rewrite("  fill --label Email --frame '#pay' ${email}", "--label E-mail").unwrap(),
            "  fill --label E-mail --frame #pay ${email}"
        );
        assert!(rewrite("  goto /cart", "--text x").is_err());
        // What it writes reads back as the same words.
        for w in
            ["a b", "say \"hi\"", "it's \"x\"", "back\\slash", "", "plain", "two\nlines", "say \"hi\"\r\nthen", "a\\nb"]
        {
            assert_eq!(tokenize(&quote(w)).unwrap(), [w], "{w:?} as {}", quote(w));
        }
    }
}
