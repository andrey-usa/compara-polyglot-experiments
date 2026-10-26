//! `--trace on-failure|all`: what each step of a scenario saw (where the
//! page was, the console messages and requests it brought) in `trace.jsonl`
//! beside the scenario's screenshot, and the page's ax tree at a failure in
//! `ax.txt`. `primavera trace <dir> [<line>]` reads it back in the terminal.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Trace {
    #[default]
    Off,
    OnFailure,
    All,
}

impl Trace {
    pub fn parse(word: &str) -> Result<Trace, String> {
        match word {
            "off" => Ok(Trace::Off),
            "on-failure" => Ok(Trace::OnFailure),
            "all" | "on" => Ok(Trace::All),
            other => Err(format!("--trace takes on-failure, all or off, not {other:?}")),
        }
    }
}

/// The highest request number in navigera `network` lines (`#12 GET …`).
pub fn last_request(lines: &str) -> u64 {
    lines.lines().filter_map(|l| l.strip_prefix('#')?.split_whitespace().next()?.parse().ok()).max().unwrap_or(0)
}

/// One step's line in `trace.jsonl`.
pub fn step(
    line: usize,
    text: &str,
    ms: f64,
    error: Option<&str>,
    page: (String, String),
    console: Vec<Value>,
    network: Vec<String>,
) -> Value {
    json!({
        "line": line, "text": text, "ms": (ms * 10.0).round() / 10.0, "ok": error.is_none(),
        "error": error, "url": page.0, "title": page.1, "console": console, "network": network,
    })
}

/// The trace file of `dir`, or of each scenario directory under it.
fn find(dir: &Path) -> Vec<PathBuf> {
    let own = dir.join("trace.jsonl");
    if own.is_file() {
        return vec![own];
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path().join("trace.jsonl"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found
}

/// A trace as the terminal shows it: every step, or one in full.
pub fn render(text: &str, only: Option<usize>, dir: &Path) -> String {
    let mut out = String::new();
    let mut lines = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let Some(head) = lines.next() else { return "an empty trace\n".into() };
    let _ = writeln!(
        out,
        "{} {}:{}  {:?}  ({:.0} ms)",
        if head["passed"] == true { "PASS" } else { "FAIL" },
        head["file"].as_str().unwrap_or(""),
        head["line"],
        head["scenario"].as_str().unwrap_or(""),
        head["ms"].as_f64().unwrap_or(0.0)
    );
    let mut place = String::new();
    for s in lines {
        let line = s["line"].as_u64().unwrap_or(0) as usize;
        if only.is_some_and(|o| o != line) {
            continue;
        }
        let ok = s["ok"] == true;
        let _ = writeln!(
            out,
            "{:>5}  {}  {:>7.0} ms  {}",
            line,
            if ok { "ok  " } else { "FAIL" },
            s["ms"].as_f64().unwrap_or(0.0),
            s["text"].as_str().unwrap_or("")
        );
        let here = format!("{} {:?}", s["url"].as_str().unwrap_or(""), s["title"].as_str().unwrap_or(""));
        if here != place || only.is_some() {
            let _ = writeln!(out, "                    at {here}");
            place = here;
        }
        if let Some(e) = s["error"].as_str() {
            for l in e.lines() {
                let _ = writeln!(out, "                    {l}");
            }
        }
        // All of them for the step asked about, else the first few.
        let cap = if only.is_some() { usize::MAX } else { 5 };
        let console = s["console"].as_array().cloned().unwrap_or_default();
        for c in console.iter().take(cap) {
            let _ = writeln!(
                out,
                "                    console {}: {}",
                c["level"].as_str().unwrap_or("log"),
                c["text"].as_str().unwrap_or("")
            );
        }
        let network = s["network"].as_array().cloned().unwrap_or_default();
        for n in network.iter().take(cap) {
            let _ = writeln!(out, "                    net {}", n.as_str().unwrap_or(""));
        }
        let more = console.len().saturating_sub(cap) + network.len().saturating_sub(cap);
        if more > 0 {
            let _ = writeln!(out, "                    … {more} more (primavera trace {} {line})", dir.display());
        }
        if !ok && only.is_some() {
            if let Ok(ax) = std::fs::read_to_string(dir.join("ax.txt")) {
                let _ = writeln!(out, "\nthe page at the failure (ax.txt):\n{ax}");
            }
        }
    }
    out
}

/// `primavera trace <dir> [<line>]`.
pub fn show(dir: &Path, only: Option<usize>) -> Result<String, String> {
    let files = find(dir);
    match files.as_slice() {
        [] => Err(format!(
            "no trace.jsonl in {} or the directories under it (run with --trace on-failure or --trace all)",
            dir.display()
        )),
        [one] => {
            let text = std::fs::read_to_string(one).map_err(|e| format!("{}: {e}", one.display()))?;
            Ok(render(&text, only, one.parent().unwrap_or(Path::new("."))))
        }
        many => {
            let mut out = String::from("traces here (primavera trace <dir> [<line>]):\n");
            for f in many {
                let first = std::fs::read_to_string(f).ok().and_then(|t| t.lines().next().map(str::to_string));
                let head: Value = first.and_then(|l| serde_json::from_str(&l).ok()).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "  {}  {}  {}",
                    if head["passed"] == true { "PASS" } else { "FAIL" },
                    f.parent().unwrap_or(Path::new(".")).display(),
                    head["scenario"].as_str().unwrap_or("")
                );
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_read_back_with_where_they_were_and_what_came() {
        let head = json!({"scenario": "Pays", "file": "shop.pv", "line": 3, "passed": false, "ms": 340.0});
        let console: Vec<Value> = (0..7).map(|k| json!({"level": "error", "text": format!("boom {k}")})).collect();
        let a = step(
            4,
            "goto /cart",
            120.0,
            None,
            ("http://x/cart".into(), "Cart".into()),
            vec![],
            vec!["#1 GET 200 Document /cart 1 kB 30 ms".into()],
        );
        let b = step(
            5,
            "click --text Pay",
            20.0,
            Some("no element"),
            ("http://x/cart".into(), "Cart".into()),
            console,
            vec![],
        );
        let text = [head, a, b].iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n");
        let all = render(&text, None, Path::new("ev/shop"));
        assert!(all.starts_with("FAIL shop.pv:3  \"Pays\""), "{all}");
        assert!(all.contains("    4  ok        120 ms  goto /cart"), "{all}");
        assert!(all.contains("at http://x/cart \"Cart\""), "{all}");
        assert!(all.contains("net #1 GET 200"), "{all}");
        assert_eq!(all.matches("at http://x/cart").count(), 1, "said once while it stays: {all}");
        assert!(all.contains("console error: boom 4") && !all.contains("boom 5"), "{all}");
        assert!(all.contains("… 2 more (primavera trace ev/shop 5)"), "{all}");
        let one = render(&text, Some(5), Path::new("ev/shop"));
        assert!(one.contains("boom 6") && !one.contains("goto /cart"), "{one}");
        assert_eq!(last_request("#1 GET 200 a\n#12 POST 201 b\n"), 12);
        assert_eq!(Trace::parse("on-failure"), Ok(Trace::OnFailure));
        assert!(Trace::parse("sometimes").is_err());
    }
}
