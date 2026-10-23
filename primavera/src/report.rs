//! What a run prints, and the JUnit XML CI systems read.

use std::fmt::Write as _;
use std::path::Path;

use crate::runner::ScenarioResult;

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

/// One scenario, as it finishes: a line when it passed, the whole story when
/// it failed.
pub fn console_line(r: &ScenarioResult) -> String {
    let mut out = String::new();
    let mark = if r.passed() { "PASS" } else { "FAIL" };
    let _ = writeln!(out, "{mark}  {}:{}  {}  ({})", rel(&r.file), r.line, r.name, secs(r.ms));
    if let Some(f) = &r.failure {
        let _ = writeln!(out, "      line {}: {}", f.line, f.text);
        for l in f.message.lines() {
            let _ = writeln!(out, "      {l}");
        }
        if let Some((url, title)) = &f.at {
            let _ = writeln!(out, "      at {url} {title:?}");
        }
        if let Some(h) = &f.hint {
            let _ = writeln!(out, "      note: {h}");
        }
        if let Some(s) = &f.screenshot {
            let _ = writeln!(out, "      screenshot {}", rel(s));
        }
    }
    out
}

pub fn summary(results: &[ScenarioResult], ms: f64) -> String {
    let failed = results.iter().filter(|r| !r.passed()).count();
    let passed = results.len() - failed;
    format!("{passed} passed, {failed} failed in {}", secs(ms))
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
                None => out.push_str("/>\n"),
                Some(f) => {
                    let mut body = format!("line {}: {}\n{}\n", f.line, f.text, f.message);
                    if let Some((url, title)) = &f.at {
                        let _ = writeln!(body, "at {url} {title:?}");
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
            ms: 412.0,
            steps: vec![],
            failure: None,
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
            }),
            ..pass.clone()
        };
        let x = junit(&[pass, fail], 1000.0);
        assert!(x.contains("tests=\"2\" failures=\"1\""), "{x}");
        assert!(x.contains("name=\"Adds &lt;one&gt;\""), "{x}");
        assert!(x.contains("Total &amp; tax"), "{x}");
        assert!(x.contains("<failure message=\"line 12: expected … saw &quot;$40.00&quot;\""), "{x}");
    }
}
