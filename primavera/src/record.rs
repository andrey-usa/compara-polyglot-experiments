//! `primavera record <url>`: a browser to use by hand; what you do in it
//! becomes a scenario. A script in every page of the browser notes each
//! click, field change, check, select, upload and Enter as it happens,
//! with the locators that find the element (navigera's `locate`, run in
//! the page at that moment, so a click that navigates away is still
//! known), and says so on the console; primavera reads it from there and
//! writes the scenario the way `from-session` does. The file is rewritten
//! after every step, so stopping with Ctrl-C keeps what was done.
//! `--extend <file.pv>` runs a scenario of that file to its end first and
//! puts what is done after it in after its last step.

use std::time::{Duration, Instant};

use navigera::protocol::{Driver, SessionConfig};
use serde_json::{json, Value};

/// What the page says before each note on the console.
const MARK: &str = "__pv_rec ";

/// The script every document gets: the notes, `MARK` + JSON.
fn recorder() -> String {
    format!(
        r#"(() => {{
  if (window.__pvRecorder) return;
  window.__pvRecorder = true;
  const locate = {locate};
  let seq = 0;
  const note = (el, step) => {{
    const n = ++seq, at = Date.now(), url = location.href;
    locate(el).then((r) => {{
      // A password field's value stays out of the console and the file.
      const s = {{ ...step }};
      if (r.secret) {{ s.value = ''; s.secret = true; }}
      console.debug({mark} + JSON.stringify({{ ...s, n, at, url, locators: r.locators || [], xframe: !!r.xframe }}));
    }},
      () => console.debug({mark} + JSON.stringify({{ ...step, n, at, url, locators: [] }})));
  }};
  const target = (e) => {{ let el = e.composedPath ? e.composedPath()[0] : e.target; return el && el.nodeType === 1 ? el : el && el.parentElement; }};
  const FIELD = 'input:not([type=button]):not([type=submit]):not([type=reset]):not([type=image]),textarea,select,option,[contenteditable]';
  const TOGGLE = 'input[type=checkbox],input[type=radio]';
  document.addEventListener('click', (e) => {{
    const el = target(e);
    if (!el) return;
    const act = el.closest('a,button,input,summary,label,[role],[onclick],[tabindex]') || el;
    // Into a field (typing follows) or a toggle (its change is the step):
    // not a click of its own.
    if (act.matches(FIELD) || act.matches(TOGGLE)) return;
    if (act.tagName === 'LABEL' && act.control) return;
    note(act, {{ op: 'click' }});
  }}, true);
  document.addEventListener('change', (e) => {{
    const el = target(e);
    if (!el) return;
    if (el.matches(TOGGLE)) note(el, {{ op: 'set-checked', checked: el.checked }});
    else if (el.tagName === 'SELECT') note(el, {{ op: 'select', value: [...el.selectedOptions].map((o) => (o.label || o.text).trim()) }});
    else if (el.type === 'file') note(el, {{ op: 'upload', files: [...el.files].map((f) => f.name) }});
    else if ('value' in el) note(el, {{ op: 'fill', value: el.value }});
  }}, true);
  document.addEventListener('keydown', (e) => {{
    const el = target(e);
    if (e.key !== 'Enter' || !el || !el.matches('input')) return;
    note(el, {{ op: 'fill', value: el.value }});
    note(el, {{ op: 'press', key: 'Enter' }});
  }}, true);
}})()"#,
        locate = navigera::cdp::locate_js(),
        mark = serde_json::to_string(MARK).unwrap_or_default(),
    )
}

/// The notes as history entries (`from-session`'s input): in the order
/// they happened, a field filled twice in a row once, each entry's `url`
/// where the page was after it (the next note's, or `now` for the last).
pub fn history(start: &str, mut notes: Vec<Value>, now: &str) -> Vec<Value> {
    notes.sort_by_key(|n| (n["at"].as_u64().unwrap_or(0), n["n"].as_u64().unwrap_or(0)));
    let same = |a: &Value, b: &Value| a["op"] == "fill" && b["op"] == "fill" && a["locators"] == b["locators"];
    let mut kept: Vec<Value> = Vec::new();
    for n in notes {
        match kept.last_mut() {
            Some(last) if same(last, &n) => *last = n,
            _ => kept.push(n),
        }
    }
    let mut out = vec![json!({"command": {"op": "goto", "url": start}, "url": start})];
    for (i, n) in kept.iter().enumerate() {
        let after = kept.get(i + 1).and_then(|m| m["url"].as_str()).unwrap_or(now);
        let mut command = json!({"op": n["op"], "ref": 0});
        for k in ["value", "checked", "files", "key"] {
            if !n[k].is_null() {
                command[k] = n[k].clone();
            }
        }
        // In a frame of another origin the page can't name the frame: left out.
        let locators = if n["xframe"] == true { json!([]) } else { n["locators"].clone() };
        let mut entry = json!({"command": command, "locators": locators, "url": after});
        if n["secret"] == true {
            entry["secret"] = json!(true);
        }
        out.push(entry);
    }
    out
}

/// The notes the page wrote since the last look (`notes` gets them) and
/// where it is now; `None` once the browser is gone.
fn look(driver: &mut Driver, notes: &mut Vec<Value>, here: &mut String) -> Option<bool> {
    let console = run(driver, &["console", "--json", "--clear"]).ok()?;
    let before = notes.len();
    for m in console.as_array().into_iter().flatten() {
        if let Some(json) = m["text"].as_str().and_then(|t| t.strip_prefix(MARK)) {
            if let Ok(n) = serde_json::from_str::<Value>(json) {
                eprintln!("  {} {}", n["op"].as_str().unwrap_or(""), n["locators"][0]["flags"]);
                notes.push(n);
            }
        }
    }
    if let Ok(u) = run(driver, &["url"]) {
        *here = u["url"].as_str().unwrap_or(here).to_string();
    }
    Some(notes.len() != before)
}

fn run(driver: &mut Driver, words: &[&str]) -> Result<Value, String> {
    let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    let cmd = crate::runner::parse(&words)?;
    driver.run(&cmd).map_err(|e| format!("{e:#}"))
}

/// Until the browser closes or `until` passes: `write` after each new note.
fn watch(
    driver: &mut Driver,
    until: Option<Duration>,
    mut write: impl FnMut(&[Value], &str) -> Result<(), String>,
) -> Result<(Vec<Value>, String), String> {
    let started = Instant::now();
    let mut notes: Vec<Value> = Vec::new();
    let mut here = run(driver, &["url"]).ok().and_then(|u| u["url"].as_str().map(String::from)).unwrap_or_default();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        match look(driver, &mut notes, &mut here) {
            None => break,
            Some(true) => write(&notes, &here)?,
            Some(false) => {}
        }
        if until.is_some_and(|d| started.elapsed() >= d) {
            break;
        }
    }
    Ok((notes, here))
}

/// `primavera record <url> [--out <file.pv>] [--scenario <name>] [--headless] [--for <s>]`,
/// or `primavera record --extend <file.pv> [--scenario <name>] [--base-url <url>] [--secrets <file>] …`.
pub fn main(args: &[String]) -> Result<String, String> {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let until = match flag("--for") {
        Some(s) => Some(Duration::from_secs_f64(s.parse().map_err(|_| format!("--for takes seconds, not {s:?}"))?)),
        None => None,
    };
    let headless = args.iter().any(|a| a == "--headless");
    if let Some(file) = flag("--extend") {
        return extend(file, flag("--scenario"), flag("--base-url"), flag("--secrets"), headless, until);
    }
    let values: Vec<&String> = [flag("--out"), flag("--scenario"), flag("--for")].into_iter().flatten().collect();
    let url = args
        .iter()
        .find(|a| !a.starts_with("--") && !values.contains(a))
        .ok_or("usage: primavera record <url> [--out <file.pv>] [--scenario <name>] [--headless] [--for <seconds>]\n       primavera record --extend <file.pv> [--scenario <name>] [--base-url <url>] [--secrets <file>]")?;
    let url = navigera::protocol::normalize_url(url);
    let out = flag("--out");
    let scenario = flag("--scenario").cloned().unwrap_or_else(|| "Recorded".to_string());
    let config = SessionConfig { headed: !headless, ..SessionConfig::default() };
    let mut driver = Driver::launch(&config).map_err(|e| format!("the browser didn't start: {e:#}"))?;
    run(&mut driver, &["init-script", &recorder()])?;
    run(&mut driver, &["goto", &url])?;
    eprintln!("recording {url}: use the page; {}", how_to_stop(until, out.map(String::as_str)));
    let render = |notes: &[Value], here: &str| {
        crate::from_session::render(&history(&url, notes.to_vec(), here), "recorded", &scenario)
    };
    if let Some(path) = out {
        std::fs::write(path, render(&[], &url)).map_err(|e| format!("{path}: {e}"))?;
    }
    let (notes, here) = watch(&mut driver, until, |notes, here| match out {
        Some(path) => std::fs::write(path, render(notes, here)).map_err(|e| format!("{path}: {e}")),
        None => Ok(()),
    })?;
    let text = render(&notes, &here);
    driver.session().close();
    match out {
        Some(path) => {
            std::fs::write(path, &text).map_err(|e| format!("{path}: {e}"))?;
            Ok(format!("wrote {path}; check it with `primavera lint {path}`, then run it\n"))
        }
        None => Ok(text),
    }
}

fn how_to_stop(until: Option<Duration>, out: Option<&str>) -> String {
    match (until, out) {
        (Some(d), _) => format!("stops in {} s", d.as_secs_f64()),
        (None, Some(o)) => format!("close the browser or press Ctrl-C to stop ({o} is kept up to date)"),
        (None, None) => "close the browser or press Ctrl-C to stop".into(),
    }
}

/// `record --extend`: run the scenario (the file's last, or `name`) to its
/// end, then record from there; what is done goes in after its last step.
fn extend(
    path: &str,
    name: Option<&String>,
    base: Option<&String>,
    secrets: Option<&String>,
    headless: bool,
    until: Option<Duration>,
) -> Result<String, String> {
    if !path.ends_with(".pv") {
        return Err(format!("{path}: --extend takes a .pv file (a .feature's steps are in its step blocks)"));
    }
    let suites = crate::load(&[std::path::PathBuf::from(path)])?;
    let mut suite = suites.into_iter().next().ok_or_else(|| format!("{path}: nothing to extend"))?;
    let scenario = match name {
        Some(n) => suite.scenarios.iter().rev().find(|s| &s.name == n),
        None => suite.scenarios.last(),
    }
    .cloned()
    .ok_or_else(|| match name {
        Some(n) => format!("{path}: no scenario {n:?}"),
        None => format!("{path}: no scenario to extend"),
    })?;
    // Recording goes on where the scenario ends, not after `after each`.
    suite.after_each.clear();
    let stamp =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let mut opts = crate::runner::Options::new(stamp);
    opts.headed = !headless;
    if let Some(b) = base {
        opts.base = Some(b.clone());
    }
    if let Some(p) = secrets {
        let text = std::fs::read_to_string(p).map_err(|e| format!("--secrets {p}: {e}"))?;
        opts.secrets = crate::secrets_file(&text).map_err(|e| format!("--secrets {p}: {e}"))?;
    }
    if opts.base.is_none() {
        opts.base = suite.base.clone();
    }
    let base = opts.base.clone();
    let mut r = crate::runner::Runner::launch(opts).map_err(|e| format!("the browser didn't start: {e:#}"))?;
    let vars = if suite.before_all.is_empty() {
        Default::default()
    } else {
        r.hook(&suite, &suite.before_all, "before all", &Default::default())
            .map_err(|f| format!("{path}:{}: before all: {}", f.line, f.message))?
    };
    eprintln!("running {path}:{} {:?} to its end …", scenario.line, scenario.name);
    let result = r.run(&suite, &scenario, 1, &vars);
    if let Some(f) = &result.failure {
        return Err(format!(
            "{path}:{}: {:?} fails there, before its end: {}\nfix it first (primavera run {path}), then extend it",
            f.line, scenario.name, f.message
        ));
    }
    let driver = r.driver();
    run(driver, &["init-script", &recorder()])?;
    run(driver, &["eval", &format!("({}, 1)", recorder())])?;
    run(driver, &["console", "--clear"])?;
    let start = run(driver, &["url"])?["url"].as_str().unwrap_or("").to_string();
    eprintln!("recording from {start}: go on; {}", how_to_stop(until, Some(path)));
    let original = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let lines: Vec<&str> = original.lines().collect();
    let (after, indent) = end_of(&lines, scenario.line);
    let text = |notes: &[Value], here: &str| {
        let h = history(&start, notes.to_vec(), here);
        let (steps, _) = crate::from_session::steps(&h[1..], base.as_deref(), Some(start.clone()));
        let added: Vec<String> = steps.lines().map(|l| format!("{indent}{}", l.trim_start())).collect();
        let mut out: Vec<String> = lines[..after].iter().map(|l| l.to_string()).collect();
        out.extend(added);
        out.extend(lines[after..].iter().map(|l| l.to_string()));
        (out.join("\n") + if original.ends_with('\n') { "\n" } else { "" }, steps.lines().count())
    };
    let (notes, here) = watch(driver, until, |notes, here| {
        std::fs::write(path, text(notes, here).0).map_err(|e| format!("{path}: {e}"))
    })?;
    let (out, added) = text(&notes, &here);
    std::fs::write(path, out).map_err(|e| format!("{path}: {e}"))?;
    Ok(format!("{path}: {added} line(s) added to {:?}; check them, then run it\n", scenario.name))
}

/// Where a scenario's steps end (the index after its last indented line;
/// `line` is its 1-based header line) and how they are indented.
fn end_of(lines: &[&str], line: usize) -> (usize, String) {
    let mut after = line;
    let mut indent = String::new();
    for (i, l) in lines.iter().enumerate().skip(line) {
        if l.trim().is_empty() {
            continue;
        }
        if !l.starts_with([' ', '\t']) {
            break;
        }
        after = i + 1;
        let own = &l[..l.len() - l.trim_start().len()];
        if indent.is_empty() || own.len() < indent.len() {
            indent = own.to_string();
        }
    }
    (after, if indent.is_empty() { "  ".into() } else { indent })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_become_steps_in_order_with_where_each_left_the_page() {
        let loc = |n: &str| json!([{"kind": "role", "flags": ["--role", "textbox", "--name", n]}]);
        let notes = vec![
            json!({"op": "click", "n": 3, "at": 30, "url": "http://x/login", "locators": [{"kind": "role", "flags": ["--role", "button", "--name", "Sign in"]}]}),
            json!({"op": "fill", "value": "a", "n": 1, "at": 10, "url": "http://x/login", "locators": loc("Email")}),
            json!({"op": "fill", "value": "ada", "n": 2, "at": 20, "url": "http://x/login", "locators": loc("Email")}),
            json!({"op": "set-checked", "checked": true, "n": 4, "at": 40, "url": "http://x/home", "locators": [{"kind": "label", "flags": ["--label", "Remember"]}]}),
        ];
        let h = history("http://x/login", notes, "http://x/home");
        let pv = crate::from_session::render(&h, "recorded", "Signs in");
        assert!(
            pv.ends_with(
                "scenario \"Signs in\"\n  goto /login\n  fill --role textbox --name Email ada\n  \
                 click --role button --name \"Sign in\"\n  expect url ends-with /home\n  check --label Remember\n"
            ),
            "{pv}"
        );
        assert!(recorder().contains("const locate = async function (el"), "the page gets navigera's locate");
    }
}
