//! What a run prints, and the JUnit XML CI systems read.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

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

fn failure_lines(out: &mut String, f: &Failure, indent: &str, file: &Path) {
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
        // Where the failing step is written (a flow's, maybe in a used file).
        let (file, line) = f.source.as_ref().map_or((file, f.line), |(p, l, _)| (p.as_path(), *l));
        if file.extension().is_none_or(|e| e != "feature") {
            let _ = writeln!(out, "{indent}fix: primavera fix {}:{line} --use 1", rel(file));
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
        failure_lines(&mut out, f, "      ", &r.file);
    } else if let Some(f) = r.retried.first() {
        let _ = writeln!(out, "      failed first:");
        failure_lines(&mut out, f, "        ", &r.file);
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

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Screenshots up to this size go into the report itself; bigger ones are
/// linked, so a run with many failures still opens quickly.
const EMBED_MAX: u64 = 2 * 1024 * 1024;
/// The most a report embeds in all (as base64); the screenshots after that
/// are links, so a run of many failures still opens fast.
const REPORT_BUDGET: usize = 10 * 1024 * 1024;

/// What a report has embedded: which screenshots, and how many bytes.
#[derive(Default)]
struct Embeds {
    seen: HashSet<PathBuf>,
    bytes: usize,
}

fn failure_html(out: &mut String, f: &Failure, title: &str) {
    let _ = write!(
        out,
        "<div class=failure><div class=what>{} line {}: <code>{}</code></div>",
        esc(title),
        f.line,
        esc(&f.text)
    );
    let _ = write!(out, "<pre>{}</pre>", esc(&f.message));
    if let Some((url, page)) = &f.at {
        let _ = write!(out, "<div>at <code>{}</code> {}</div>", esc(url), esc(&format!("{page:?}")));
    }
    if !f.nearest.is_empty() {
        out.push_str("<div>nearest on the page:</div><ol>");
        for (shown, locator, score) in &f.nearest {
            let _ = write!(out, "<li>{} <code>{}</code> {score:.2}</li>", esc(shown), esc(locator));
        }
        out.push_str("</ol>");
    }
    if let Some(h) = &f.hint {
        let _ = write!(out, "<div class=note>note: {}</div>", esc(h));
    }
    out.push_str("</div>");
}

/// A failure's screenshot: in the page (once per file: a `before all`
/// failure shared by every scenario of its file is embedded the first
/// time), or a link relative to the report when it is too big.
fn screenshot_html(out: &mut String, f: &Failure, report_dir: &Path, embedded: &mut Embeds) {
    let Some(s) = &f.screenshot else { return };
    if embedded.seen.contains(s) {
        out.push_str("<div class=dim>screenshot: the same one as above</div>");
        return;
    }
    let size = std::fs::metadata(s).map(|m| m.len()).ok();
    match (size, std::fs::read(s)) {
        (Some(n), Ok(png)) if n <= EMBED_MAX && embedded.bytes + (n as usize).div_ceil(3) * 4 <= REPORT_BUDGET => {
            embedded.seen.insert(s.clone());
            let b64 = base64(&png);
            embedded.bytes += b64.len();
            let _ = write!(out, "<img alt=\"screenshot at the failure\" src=\"data:image/png;base64,{b64}\">");
        }
        (Some(n), _) => {
            let why =
                if n > EMBED_MAX { "too big to put in the page" } else { "the page's 10 MB of screenshots is spent" };
            let _ = write!(
                out,
                "<div><a href=\"{}\">screenshot</a> <span class=dim>({:.1} MB, {why})</span></div>",
                esc(&href(s, report_dir)),
                n as f64 / 1_048_576.0
            );
        }
        (None, _) => {
            let _ = write!(out, "<div class=dim>screenshot <code>{}</code> (gone)</div>", esc(&rel(s)));
        }
    }
}

/// `target` as a link from a page in `base`: a path relative to it when
/// they share a root (a report and its evidence uploaded together), else a
/// file URL.
fn href(target: &Path, base: &Path) -> String {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let (t, b) = (abs(target), abs(base));
    let tc: Vec<_> = t.components().collect();
    let bc: Vec<_> = b.components().collect();
    let common = tc.iter().zip(&bc).take_while(|(x, y)| x == y).count();
    let raw = if common == 0 {
        let p = t.display().to_string().replace('\\', "/");
        if p.starts_with('/') {
            format!("file://{p}")
        } else {
            format!("file:///{p}")
        }
    } else {
        let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), bc.len() - common).collect();
        parts.extend(tc[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
        parts.join("/")
    };
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// How often each scenario passed across its runs (`--repeat`): its file
/// and name, passes, runs.
pub fn tally(results: &[ScenarioResult]) -> Vec<(String, u32, u32)> {
    let mut t: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for r in results {
        let e = t.entry(format!("{}  {}", r.file.display(), r.base_name())).or_default();
        e.1 += 1;
        if r.passed() {
            e.0 += 1;
        }
    }
    t.into_iter().map(|(k, (ok, n))| (k, ok, n)).collect()
}

/// The whole run as one HTML file that needs nothing else to open: a
/// summary, then each scenario with its steps and times, failures (and
/// how a flaky one failed first) open, with their screenshot in the page.
pub fn html(results: &[ScenarioResult], ms: f64, report_dir: &Path) -> String {
    let mut out = String::new();
    let mut embedded = Embeds::default();
    let sum = summary(results, ms);
    let _ = write!(
        out,
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>primavera: {}</title><style>{CSS}</style></head><body><h1>{}</h1>",
        esc(&sum),
        esc(&sum)
    );
    let counts = tally(results);
    if counts.iter().any(|(_, _, n)| *n > 1) {
        out.push_str("<table class=tally><tr><th>passed</th><th>scenario</th></tr>");
        for (name, ok, n) in &counts {
            let class = if ok == n { "ok" } else { "bad" };
            let _ = write!(out, "<tr class={class}><td>{ok}/{n}</td><td>{}</td></tr>", esc(name));
        }
        out.push_str("</table>");
    }
    // Failed first, then flaky, then passed; within each, the order they ran.
    let rank = |r: &ScenarioResult| {
        if !r.passed() {
            0
        } else if r.flaky() {
            1
        } else {
            2
        }
    };
    let mut order: Vec<&ScenarioResult> = results.iter().collect();
    order.sort_by_key(|r| rank(r));
    for r in order {
        let (class, mark) = match rank(r) {
            0 => ("fail", "FAIL"),
            1 => ("flaky", "FLAKY"),
            _ => ("pass", "PASS"),
        };
        let open = if rank(r) < 2 { " open" } else { "" };
        let tries = if r.attempt > 1 { format!(", try {}", r.attempt) } else { String::new() };
        let tags: String = r.tags.iter().map(|t| format!(" <span class=tag>@{}</span>", esc(t))).collect();
        let _ = write!(
            out,
            "<details class={class}{open}><summary><b>{mark}</b> {} <span class=dim>{}:{} · {}{tries} · worker {}</span>{tags}</summary>",
            esc(&r.name),
            esc(&rel(&r.file)),
            r.line,
            secs(r.ms),
            r.worker
        );
        if let Some(f) = &r.failure {
            failure_html(&mut out, f, "failed at");
        }
        for (i, f) in r.retried.iter().enumerate() {
            failure_html(&mut out, f, &format!("try {} failed at", i + 1));
            screenshot_html(&mut out, f, report_dir, &mut embedded);
        }
        if !r.steps.is_empty() {
            out.push_str("<table><tr><th>line</th><th>step</th><th>time</th></tr>");
            for s in &r.steps {
                let _ = write!(
                    out,
                    "<tr class={}><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
                    if s.ok { "ok" } else { "bad" },
                    s.line,
                    esc(&s.text),
                    secs(s.ms)
                );
            }
            out.push_str("</table>");
        }
        if let Some(f) = &r.failure {
            screenshot_html(&mut out, f, report_dir, &mut embedded);
        }
        for n in &r.notes {
            let _ = write!(out, "<div class=note>{}</div>", esc(n));
        }
        if !r.saved.is_empty() {
            out.push_str("<div>saved:</div><ul>");
            for (k, v) in &r.saved {
                let _ = write!(out, "<li><code>{}</code> = <code>{}</code></li>", esc(k), esc(v));
            }
            out.push_str("</ul>");
        }
        out.push_str("</details>");
    }
    out.push_str("</body></html>\n");
    out
}

const CSS: &str = "body{font:14px/1.4 system-ui,sans-serif;margin:16px;color:#1b1b1b;background:#fff}\
h1{font-size:18px}details{border:1px solid #ccc;border-radius:4px;margin:6px 0;padding:4px 8px}\
details.fail{border-left:4px solid #c62828}details.flaky{border-left:4px solid #e0a000}details.pass{border-left:4px solid #2e7d32}\
summary{cursor:pointer}.dim{color:#666}.tag{color:#555;font-size:12px}code,pre{font:12px ui-monospace,monospace}\
pre{white-space:pre-wrap;background:#f6f6f6;padding:6px}table{border-collapse:collapse;margin:6px 0}\
td,th{border-bottom:1px solid #eee;padding:2px 8px;text-align:left;vertical-align:top}tr.bad td{background:#fdecea}\
.failure{margin:6px 0}.note{color:#555}img{max-width:min(100%,640px);border:1px solid #ccc;margin-top:6px}\
@media (prefers-color-scheme:dark){body{color:#e6e6e6;background:#121212}pre{background:#1e1e1e}\
details{border-color:#444}td,th{border-color:#333}tr.bad td{background:#3a1d1d}.dim,.tag,.note{color:#aaa}}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Failure, ScenarioResult};
    use std::path::PathBuf;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x01]), "//4AAQ==");
    }

    #[test]
    fn html_puts_failures_first_escapes_and_needs_nothing_else() {
        let dir = std::env::temp_dir().join(format!("pv-html-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("shot.png");
        std::fs::write(&shot, b"\x89PNG fake").unwrap();
        let pass = ScenarioResult {
            file: PathBuf::from("/x/shop.pv"),
            name: "Adds <one>".into(),
            line: 3,
            tags: vec!["smoke".into()],
            ms: 412.0,
            steps: vec![crate::runner::StepResult { line: 4, text: "goto /cart".into(), ms: 30.0, ok: true }],
            failure: None,
            attempt: 1,
            retried: vec![],
            saved: Default::default(),
            notes: vec![],
            worker: 0,
            rep: 0,
            started: 0.0,
        };
        let fail = ScenarioResult {
            name: "Total & tax".into(),
            failure: Some(Failure {
                line: 12,
                text: "expect text \"<b>\"".into(),
                message: "saw \"$40.00\"".into(),
                at: Some(("http://shop/cart".into(), "Cart".into())),
                hint: None,
                screenshot: Some(shot.clone()),
                nearest: vec![("button \"Pay\"".into(), "--role button --name Pay".into(), 0.8)],
                source: None,
            }),
            ..pass.clone()
        };
        let h = html(&[pass.clone(), fail.clone()], 1000.0, &dir);
        assert!(h.find("Total &amp; tax").unwrap() < h.find("Adds &lt;one&gt;").unwrap(), "failed first");
        assert!(h.contains("expect text &quot;&lt;b&gt;&quot;"), "{h}");
        assert!(h.contains(&format!("data:image/png;base64,{}", base64(b"\x89PNG fake"))), "{h}");
        assert!(h.contains("<details class=fail open>") && h.contains("<details class=pass>"), "{h}");
        assert!(h.contains("@smoke") && h.contains("goto /cart"), "{h}");
        assert!(!h.contains("http-equiv") && !h.contains("<script") && !h.contains("src=\"http"), "{h}");
        assert!(!h.contains("class=tally"), "one run each: no tally");
        // A flaky scenario shows its first try's screenshot; a screenshot
        // two scenarios share (a `before all`) is in the page once.
        let flaky =
            ScenarioResult { failure: None, retried: vec![fail.failure.clone().unwrap()], attempt: 2, ..pass.clone() };
        let h = html(&[fail.clone(), flaky], 1000.0, &dir);
        assert_eq!(h.matches("data:image/png").count(), 1, "{h}");
        assert!(h.contains("screenshot: the same one as above"), "{h}");
        // A big one is a link from where the report is.
        std::fs::write(&shot, vec![0u8; (EMBED_MAX + 1) as usize]).unwrap();
        let h = html(std::slice::from_ref(&fail), 1000.0, &dir.join("reports"));
        assert!(h.contains("<a href=\"../shot.png\">screenshot</a>"), "{h}");
        // Repeats: a pass count per scenario.
        let again = ScenarioResult { rep: 1, ..pass.clone() };
        let h = html(&[pass.clone(), again, fail], 1000.0, &dir);
        assert!(h.contains("<tr class=ok><td>2/2</td><td>/x/shop.pv  Adds &lt;one&gt;</td></tr>"), "{h}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_report_of_many_failures_stays_under_its_budget() {
        let dir = std::env::temp_dir().join(format!("pv-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut results = Vec::new();
        for k in 0..12 {
            // 1.5 MB each: embeddable alone, not all together.
            let shot = dir.join(format!("s{k}.png"));
            std::fs::write(&shot, vec![k as u8; 1_500_000]).unwrap();
            let mut r = fail_case();
            r.name = format!("Fails {k}");
            r.failure.as_mut().unwrap().screenshot = Some(shot);
            results.push(r);
        }
        let h = html(&results, 1.0, &dir);
        assert!(h.len() <= REPORT_BUDGET + 200_000, "{} bytes", h.len());
        assert_eq!(h.matches("data:image/png").count(), 5);
        assert_eq!(h.matches("10 MB of screenshots is spent").count(), 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn links_are_relative_or_file_urls() {
        assert_eq!(href(Path::new("/a/runs/x y/s.png"), Path::new("/a/reports")), "../runs/x%20y/s.png");
        assert_eq!(href(Path::new("/a/s.png"), Path::new("/a")), "s.png");
    }

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
            rep: 0,
            started: 0.0,
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
                source: None,
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
                source: None,
            }),
            attempt: 1,
            retried: vec![],
            saved: Default::default(),
            notes: vec![],
            worker: 0,
            rep: 0,
            started: 0.0,
        }
    }
}
