//! What a run prints, and the JUnit XML CI systems read.

use std::fmt::Write as _;
use std::path::Path;

use crate::runner::{Failure, ScenarioResult};

fn secs(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

fn rel(p: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| p.display().to_string())
}

fn failure_lines(out: &mut String, f: &Failure, indent: &str) {
    let _ = writeln!(out, "{indent}line {}: {}", f.line, f.text);
    for l in f.message.lines() {
        let _ = writeln!(out, "{indent}{l}");
    }
    if let Some((url, title)) = &f.at {
        let _ = writeln!(out, "{indent}at {url} {title:?}");
    }
    if !f.nearest.is_empty() {
        let _ = writeln!(out, "{indent}nearest on the page:");
        let width = f.nearest.iter().map(|n| n.0.chars().count()).max().unwrap_or(0);
        for (i, (shown, locator, score)) in f.nearest.iter().enumerate() {
            let pad = " ".repeat(width - shown.chars().count());
            let _ = writeln!(out, "{indent}  {}. {shown}{pad}   {locator}   {score:.2}", i + 1);
        }
    }
    if let Some(h) = &f.hint {
        let _ = writeln!(out, "{indent}note: {h}");
    }
    if let Some(s) = &f.screenshot {
        let _ = writeln!(out, "{indent}screenshot {}", rel(s));
    }
}

/// One scenario, as it finishes: a line when it passed, the whole story when
/// it failed, and how the first try failed when it passed on a retry.
pub fn console_line(r: &ScenarioResult) -> String {
    let mut out = String::new();
    let mark = if r.flaky() {
        "FLAKY"
    } else if r.passed() {
        "PASS"
    } else {
        "FAIL"
    };
    let tries = if r.attempt > 1 { format!(", try {}", r.attempt) } else { String::new() };
    let _ = writeln!(out, "{mark:<5} {}:{}  {}  ({}{tries})", rel(&r.file), r.line, r.name, secs(r.ms));
    if let Some(f) = &r.failure {
        failure_lines(&mut out, f, "      ");
    } else if let Some(f) = r.retried.first() {
        let _ = writeln!(out, "      failed first:");
        failure_lines(&mut out, f, "        ");
    }
    out
}

pub fn summary(results: &[ScenarioResult], ms: f64) -> String {
    let failed = results.iter().filter(|r| !r.passed()).count();
    let flaky = results.iter().filter(|r| r.flaky()).count();
    let passed = results.len() - failed - flaky;
    let flaky = if flaky > 0 { format!(", {flaky} flaky") } else { String::new() };
    format!("{passed} passed{flaky}, {failed} failed in {}", secs(ms))
}

fn failure_json(f: &Failure) -> serde_json::Value {
    serde_json::json!({
        "line": f.line,
        "step": f.text,
        "message": f.message,
        "url": f.at.as_ref().map(|a| a.0.clone()),
        "title": f.at.as_ref().map(|a| a.1.clone()),
        "note": f.hint,
        "screenshot": f.screenshot.as_ref().map(|p| p.display().to_string()),
        "nearest": f.nearest.iter().map(|(shown, locator, score)| serde_json::json!({
            "element": shown, "locator": locator, "score": (score * 100.0).round() / 100.0,
        })).collect::<Vec<_>>(),
    })
}

/// JSON lines: one object per finished scenario (`event: scenario`), for an
/// orchestrator or an agent reading the run as it goes.
pub fn jsonl(r: &ScenarioResult) -> String {
    let status = if r.flaky() {
        "flaky"
    } else if r.passed() {
        "passed"
    } else {
        "failed"
    };
    let v = serde_json::json!({
        "event": "scenario",
        "file": rel(&r.file),
        "line": r.line,
        "name": r.name,
        "tags": r.tags,
        "status": status,
        "ms": (r.ms * 10.0).round() / 10.0,
        "attempt": r.attempt,
        "worker": r.worker,
        "failure": r.failure.as_ref().map(failure_json),
        "retried": r.retried.iter().map(failure_json).collect::<Vec<_>>(),
        "saved": r.saved,
        "notes": r.notes,
    });
    format!("{v}\n")
}

pub fn jsonl_summary(results: &[ScenarioResult], ms: f64) -> String {
    let failed = results.iter().filter(|r| !r.passed()).count();
    let flaky = results.iter().filter(|r| r.flaky()).count();
    let v = serde_json::json!({
        "event": "summary",
        "passed": results.len() - failed - flaky,
        "flaky": flaky,
        "failed": failed,
        "ms": ms.round(),
    });
    format!("{v}\n")
}

/// GitHub Actions workflow commands: an error on the failing line, a
/// warning for a flaky scenario.
pub fn github(r: &ScenarioResult) -> String {
    let esc = |s: &str| s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    let prop = |s: &str| esc(s).replace(':', "%3A").replace(',', "%2C");
    let file = prop(&rel(&r.file));
    match (&r.failure, r.retried.first()) {
        (Some(f), _) => {
            let mut msg = format!("{}\n{}", f.text, f.message);
            if let Some((shown, locator, _)) = f.nearest.first() {
                msg.push_str(&format!("\nnearest: {shown}  {locator}"));
            }
            if let Some(h) = &f.hint {
                msg.push_str(&format!("\nnote: {h}"));
            }
            format!("::error file={file},line={},title={}::{}\n", f.line, prop(&r.name), esc(&msg))
        }
        (None, Some(f)) => format!(
            "::warning file={file},line={},title={}::{}\n",
            f.line,
            prop(&format!("flaky: {}", r.name)),
            esc(&format!("failed on try 1, passed on try {}: {}", r.attempt, f.message))
        ),
        (None, None) => String::new(),
    }
}

fn xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

/// JUnit XML: one `<testsuite>` per file, one `<testcase>` per scenario.
pub fn junit(results: &[ScenarioResult], ms: f64) -> String {
    let mut files: Vec<&Path> = Vec::new();
    for r in results {
        if !files.contains(&r.file.as_path()) {
            files.push(&r.file);
        }
    }
    let failures = results.iter().filter(|r| !r.passed()).count();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<testsuites name=\"primavera\" tests=\"{}\" failures=\"{failures}\" time=\"{:.3}\">",
        results.len(),
        ms / 1000.0
    );
    for file in files {
        let rs: Vec<&ScenarioResult> = results.iter().filter(|r| r.file == file).collect();
        let name = xml(&rel(file));
        let _ = writeln!(
            out,
            "  <testsuite name=\"{name}\" tests=\"{}\" failures=\"{}\" time=\"{:.3}\">",
            rs.len(),
            rs.iter().filter(|r| !r.passed()).count(),
            rs.iter().map(|r| r.ms).sum::<f64>() / 1000.0
        );
        for r in rs {
            let _ = write!(
                out,
                "    <testcase classname=\"{name}\" name=\"{}\" file=\"{name}\" line=\"{}\" time=\"{:.3}\"",
                xml(&r.name),
                r.line,
                r.ms / 1000.0
            );
            match &r.failure {
                None if r.flaky() => {
                    let first =
                        r.retried.first().map(|f| format!("line {}: {}", f.line, f.message)).unwrap_or_default();
                    let _ = write!(
                        out,
                        ">\n      <system-out>{}</system-out>\n    </testcase>\n",
                        xml(&format!("flaky: passed on try {} after failing: {first}", r.attempt))
                    );
                }
                None => out.push_str("/>\n"),
                Some(f) => {
                    let mut body = format!("line {}: {}\n{}\n", f.line, f.text, f.message);
                    if let Some((url, title)) = &f.at {
                        let _ = writeln!(body, "at {url} {title:?}");
                    }
                    for (shown, locator, _) in &f.nearest {
                        let _ = writeln!(body, "nearest: {shown}  {locator}");
                    }
                    if let Some(h) = &f.hint {
                        let _ = writeln!(body, "note: {h}");
                    }
                    if let Some(s) = &f.screenshot {
                        let _ = writeln!(body, "[[ATTACHMENT|{}]]", s.display());
                    }
                    let first = f.message.lines().next().unwrap_or("");
                    let _ = write!(
                        out,
                        ">\n      <failure message=\"{}\" type=\"expectation\">{}</failure>\n    </testcase>\n",
                        xml(&format!("line {}: {first}", f.line)),
                        xml(&body)
                    );
                }
            }
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Failure, ScenarioResult};
    use std::path::PathBuf;

    #[test]
    fn junit_escapes_and_counts() {
        let pass = ScenarioResult {
            file: PathBuf::from("/x/shop.pv"),
            name: "Adds <one>".into(),
            line: 3,
            tags: vec![],
            ms: 412.0,
            steps: vec![],
            failure: None,
            attempt: 1,
            retried: vec![],
            saved: Default::default(),
            notes: vec![],
            worker: 0,
        };
        let fail = ScenarioResult {
            name: "Total & tax".into(),
            line: 9,
            failure: Some(Failure {
                line: 12,
                text: "expect --testid total text is \"$36.00\"".into(),
                message: "expected … saw \"$40.00\"".into(),
                at: None,
                hint: None,
                screenshot: None,
                nearest: vec![],
            }),
            ..pass.clone()
        };
        let x = junit(&[pass, fail], 1000.0);
        assert!(x.contains("tests=\"2\" failures=\"1\""), "{x}");
        assert!(x.contains("name=\"Adds &lt;one&gt;\""), "{x}");
        assert!(x.contains("Total &amp; tax"), "{x}");
        assert!(x.contains("<failure message=\"line 12: expected … saw &quot;$40.00&quot;\""), "{x}");
        // GitHub: the failing line, properties escaped, message on one line.
        let g = github(&fail_case());
        assert!(g.starts_with("::error file=/x/shop.pv,line=12,title=Total & tax::expect"), "{g}");
        assert!(g.contains("%0Aexpected"), "{g}");
        let flaky =
            ScenarioResult { failure: None, retried: vec![fail_case().failure.unwrap()], attempt: 2, ..fail_case() };
        assert!(flaky.flaky());
        assert!(github(&flaky).starts_with("::warning "), "{}", github(&flaky));
        let j: serde_json::Value = serde_json::from_str(jsonl(&flaky).trim()).unwrap();
        assert_eq!((j["status"].as_str(), j["attempt"].as_u64()), (Some("flaky"), Some(2)));
        assert!(junit(&[flaky], 1.0).contains("flaky: passed on try 2"));
        assert!(summary(&[fail_case()], 1.0).contains("0 passed, 1 failed"));
    }

    fn fail_case() -> ScenarioResult {
        ScenarioResult {
            file: PathBuf::from("/x/shop.pv"),
            name: "Total & tax".into(),
            line: 9,
            tags: vec![],
            ms: 1.0,
            steps: vec![],
            failure: Some(Failure {
                line: 12,
                text: "expect --testid total text is \"$36.00\"".into(),
                message: "expected … saw \"$40.00\"".into(),
                at: None,
                hint: None,
                screenshot: None,
                nearest: vec![],
            }),
            attempt: 1,
            retried: vec![],
            saved: Default::default(),
            notes: vec![],
            worker: 0,
        }
    }
}
