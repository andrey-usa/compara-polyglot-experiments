//! `primavera from-session <name>`: what an agent did in a navigera session
//! (`navigera -s <name> …`) as a scenario to keep. navigera's `history`
//! has each action, and for one on a ref the locators that found its
//! element again just before it ran; the most stable of them replaces the
//! ref. A navigation gets a proposed `expect url`, and a value typed into a
//! password field becomes `${secret:NAME}`, named after the field.

use navigera::protocol::{Command, SessionConfig, TargetArgs};
use serde_json::Value;

use crate::fix::quote;

/// The session's history, oldest first.
pub fn history(session: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    navigera::session::client(&SessionConfig::default(), session, &Command::History { clear: false }, &mut out);
    let text = String::from_utf8_lossy(&out);
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| format!("navigera said: {}", text.trim()))?;
    if v["ok"] != true {
        let e = v["error"].as_str().unwrap_or("no answer");
        return Err(if e.contains("unknown") && e.contains("history") {
            format!("{e}\n(this session's navigera has no `history`: start the session with this primavera's navigera)")
        } else {
            e.to_string()
        });
    }
    Ok(v["result"]["history"].as_array().cloned().unwrap_or_default())
}

/// `https://shop.test:8080` of a URL.
fn origin(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find('/').map_or(url.len(), |i| url.len() - rest.len() + i);
    Some(&url[..end])
}

/// The flags of a target as written, or a ref's most stable locator.
fn flags(t: &TargetArgs, locators: &Value) -> Result<Vec<String>, String> {
    if let Some(r) = t.node_ref {
        let best =
            locators.get(0).and_then(|l| l["flags"].as_array()).ok_or(format!("ref {r}: no locator found it again"))?;
        return Ok(best.iter().filter_map(|f| f.as_str().map(String::from)).collect());
    }
    if let Some(f) = &t.find {
        return Err(format!("--find {f:?} was not turned into a ref"));
    }
    let mut out = Vec::new();
    let mut add = |flag: &str, v: &Option<String>| {
        if let Some(v) = v {
            out.extend([flag.to_string(), v.clone()]);
        }
    };
    add("--testid", &t.testid);
    add("--role", &t.role);
    add("--name", &t.name);
    add("--label", &t.label);
    add("--placeholder", &t.placeholder);
    add("--text", &t.text);
    add("--selector", &t.selector);
    add("--near", &t.near);
    add("--frame", &t.frame);
    if let Some(n) = t.nth {
        out.extend(["--nth".into(), n.to_string()]);
    }
    Ok(out)
}

/// The words that name a field: its locators' name, label or placeholder,
/// else the target's own.
fn field_name(t: &TargetArgs, locators: &Value) -> String {
    let from_locators = locators.as_array().into_iter().flatten().find_map(|l| {
        let f: Vec<&str> = l["flags"].as_array()?.iter().filter_map(Value::as_str).collect();
        f.windows(2).find(|p| matches!(p[0], "--name" | "--label" | "--placeholder")).map(|p| p[1].to_string())
    });
    from_locators
        .or_else(|| {
            t.name.clone().or_else(|| t.label.clone()).or_else(|| t.placeholder.clone()).or_else(|| t.testid.clone())
        })
        .unwrap_or_default()
}

/// `${secret:…}`'s name for a field: its name in capitals (`TOKEN`,
/// `CONFIRM_PASSWORD`), `PASSWORD` when it has none.
fn secret_name(t: &TargetArgs, locators: &Value) -> String {
    let mut name = String::new();
    for c in field_name(t, locators).chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_uppercase());
        } else if !name.ends_with('_') && !name.is_empty() {
            name.push('_');
        }
    }
    let name = name.trim_end_matches('_').to_string();
    if name.is_empty() {
        "PASSWORD".into()
    } else {
        name
    }
}

/// Does the target look like a password field (when navigera didn't say)?
fn secret(t: &TargetArgs, locators: &Value) -> bool {
    let mut words: Vec<String> =
        [&t.name, &t.label, &t.placeholder, &t.selector, &t.testid].into_iter().flatten().cloned().collect();
    if let Some(all) = locators.as_array() {
        words.extend(
            all.iter()
                .flat_map(|l| l["flags"].as_array().cloned().unwrap_or_default())
                .filter_map(|f| f.as_str().map(String::from)),
        );
    }
    words.iter().any(|w| {
        let w = w.to_ascii_lowercase();
        w.contains("password") || w.contains("passwd") || w.contains("[type=\"password\"]")
    })
}

/// One recorded action as a step, or why it isn't one.
fn step(
    c: &Command,
    locators: &Value,
    rel: &dyn Fn(&str) -> String,
    secret: Option<&str>,
) -> Result<Vec<String>, String> {
    let on = |t: &TargetArgs| flags(t, locators);
    let mut w: Vec<String> = Vec::new();
    let mut push = |words: &[&str]| w.extend(words.iter().map(|s| s.to_string()));
    match c {
        Command::Goto { url, .. } | Command::Open { url: Some(url) } => push(&["goto", &rel(url)]),
        Command::TabNew { url } => {
            push(&["tab-new"]);
            if let Some(u) = url {
                push(&[&rel(u)]);
            }
        }
        Command::Back {} => push(&["back"]),
        Command::Forward {} => push(&["forward"]),
        Command::Reload {} => push(&["reload"]),
        Command::Click { target, button, force, .. } => {
            push(&["click"]);
            w.extend(on(target)?);
            if let Some(b) = button.as_deref().filter(|b| *b != "left") {
                w.extend(["--button".into(), b.into()]);
            }
            if *force {
                w.push("--force".into());
            }
        }
        Command::Dblclick { target, .. } => {
            push(&["dblclick"]);
            w.extend(on(target)?);
        }
        Command::Hover { target, .. } => {
            push(&["hover"]);
            w.extend(on(target)?);
        }
        Command::Fill { target, value, .. } | Command::Type { target, value, .. } => {
            push(&[if matches!(c, Command::Fill { .. }) { "fill" } else { "type" }]);
            w.extend(on(target)?);
            w.push(match secret {
                Some(name) => format!("${{secret:{name}}}"),
                None => value.clone(),
            });
        }
        Command::InsertText { target, value, .. } => {
            push(&["insert-text", value]);
            w.extend(on(target)?);
        }
        Command::Press { key, target, .. } => {
            push(&["press", key]);
            w.extend(on(target)?);
        }
        Command::KeyDown { key } => push(&["keydown", key]),
        Command::KeyUp { key } => push(&["keyup", key]),
        Command::Select { target, value, add, .. } => {
            push(&["select"]);
            w.extend(on(target)?);
            w.extend(value.iter().cloned());
            if *add {
                w.push("--add".into());
            }
        }
        Command::SetChecked { checked, target, .. } => {
            push(&[if *checked { "check" } else { "uncheck" }]);
            w.extend(on(target)?);
        }
        Command::Upload { target, files, .. } => {
            push(&["upload"]);
            w.extend(on(target)?);
            w.extend(files.iter().cloned());
        }
        Command::Scroll { target, by, to, .. } => {
            push(&["scroll"]);
            if let Some(b) = by {
                w.extend(["--by".into(), b.to_string()]);
            }
            if let Some(t) = to {
                w.extend(["--to".into(), t.clone()]);
            }
            w.extend(on(target)?);
        }
        Command::Dialog { accept, prompt_text } => {
            push(&["dialog", if accept.unwrap_or(true) { "--accept" } else { "--dismiss" }]);
            if let Some(t) = prompt_text {
                w.extend(["--prompt-text".into(), t.clone()]);
            }
        }
        Command::TabSelect { index, .. } => push(&["tab-select", &index.to_string()]),
        Command::TabClose { index } => {
            push(&["tab-close"]);
            if let Some(i) = index {
                w.push(i.to_string());
            }
        }
        Command::Drag { from, to, .. } => match (&from.selector, &to.selector, from.node_ref.or(to.node_ref)) {
            (Some(a), Some(b), None) => push(&["drag", a, b]),
            _ => return Err("a drag between refs (write it with selectors)".into()),
        },
        Command::Wait { selector, text, url, gone, js, frame, ms, idle, timeout_ms, download } => {
            push(&["wait"]);
            let mut add = |flag: &str, v: &Option<String>| {
                if let Some(v) = v {
                    w.extend([flag.to_string(), v.clone()]);
                }
            };
            add("--selector", selector);
            add("--text", text);
            add("--url", url);
            add("--gone", gone);
            add("--js", js);
            add("--frame", frame);
            if let Some(m) = ms {
                w.extend(["--ms".into(), m.to_string()]);
            }
            if *idle {
                w.push("--idle".into());
            }
            if let Some(t) = timeout_ms {
                w.extend(["--timeout-ms".into(), t.to_string()]);
            }
            if let Some(d) = download {
                w.push("--download".into());
                if !d.is_empty() {
                    w.push(d.clone());
                }
            }
        }
        Command::Mouse { action, .. } => {
            return Err(format!("mouse {action} at coordinates (kept out: they move with the layout)"))
        }
        other => {
            let op = serde_json::to_value(other).ok().and_then(|v| v["op"].as_str().map(String::from));
            return Err(format!("{} isn't an action a test repeats", op.unwrap_or_default()));
        }
    }
    Ok(w)
}

/// What a page says that the one before it didn't: its heading if that
/// changed, and status or alert messages that are new (not ones still
/// loading, `…`).
fn proposals(before: &Value, after: &Value) -> Vec<String> {
    let strs = |v: &Value| -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
    };
    let mut out = Vec::new();
    let h1 = after["h1"].as_str().unwrap_or("");
    if !h1.is_empty() && before["h1"].as_str() != Some(h1) {
        out.push(h1.to_string());
    }
    let old = strs(&before["status"]);
    for m in strs(&after["status"]) {
        let loading = m.ends_with('…') || m.ends_with("...");
        if !loading && !old.contains(&m) && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// The history as a `.pv` file with one scenario.
pub fn render(history: &[Value], session: &str, scenario: &str) -> String {
    let first = history.iter().find_map(|e| match serde_json::from_value::<Command>(e["command"].clone()) {
        Ok(Command::Goto { url, .. } | Command::Open { url: Some(url) }) => Some(url),
        _ => None,
    });
    let base = first.as_deref().and_then(origin).map(String::from);
    let mut out = format!(
        "# From navigera session {session}: what it did, refs turned into locators.\n\
         # Read it through: check the locators, keep the expects that matter, add what\n\
         # the test is for.\n"
    );
    let (steps, secrets) = steps(history, base.as_deref(), None);
    if !secrets.is_empty() {
        let names: Vec<String> = secrets.iter().map(|n| format!("${{secret:{n}}}")).collect();
        out.push_str(&format!(
            "# Secrets went in as {}: give them with --secrets <file> or the environment.\n",
            names.join(", ")
        ));
    }
    out.push('\n');
    if let Some(b) = &base {
        out.push_str(&format!("base {b}\n\n"));
    }
    out.push_str(&format!("scenario {}\n", quote_always(scenario)));
    if steps.is_empty() {
        out.push_str("  # the session's history is empty: act in it first (navigera -s <name> goto …)\n");
    }
    out.push_str(&steps);
    out
}

/// The scenario lines for `history`, URLs relative to `base`, an `expect
/// url` where an action took the page away from where it was (`at` to
/// start with); and the `${secret:…}` names they use.
pub fn steps(history: &[Value], base: Option<&str>, mut at: Option<String>) -> (String, Vec<String>) {
    let rel = |u: &str| match base {
        Some(b) if u.starts_with(b) && u[b.len()..].starts_with('/') => u[b.len()..].to_string(),
        Some(b) if u == b => "/".into(),
        _ => u.to_string(),
    };
    let mut steps = String::new();
    let mut secrets: Vec<String> = Vec::new();
    let mut said = &Value::Null;
    for e in history {
        let Ok(c) = serde_json::from_value::<Command>(e["command"].clone()) else {
            steps.push_str(&format!("  # (an action this primavera can't read: {})\n", e["command"]));
            continue;
        };
        let url = e["url"].as_str().map(String::from);
        // A password field (navigera says so; an older one, by its name).
        let secret = match &c {
            Command::Fill { target, .. } | Command::Type { target, .. } => {
                let yes = match e.get("secret") {
                    Some(v) => v == true,
                    None => secret(target, &e["locators"]),
                };
                yes.then(|| secret_name(target, &e["locators"]))
            }
            _ => None,
        };
        match step(&c, &e["locators"], &rel, secret.as_deref()) {
            Ok(words) => {
                if let Some(n) = secret.filter(|n| !secrets.contains(n)) {
                    secrets.push(n);
                }
                steps.push_str(&format!("  {}\n", words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")));
                // Where it went, unless the step said so itself.
                let went = !matches!(c, Command::Goto { .. } | Command::Open { .. } | Command::TabNew { .. });
                if let (true, Some(u)) = (went, &url) {
                    if at.as_ref().is_some_and(|a| a != u) {
                        let r = rel(u);
                        let (how, v) = if r.starts_with('/') { ("ends-with", r) } else { ("is", r) };
                        steps.push_str(&format!("  expect url {how} {}\n", quote(&v)));
                    }
                }
            }
            Err(why) => steps.push_str(&format!("  # left out: {why}\n")),
        }
        // What the page said after an action that a `goto` didn't start:
        // a heading that changed, a message that appeared.
        if !e["page"].is_null() {
            if !matches!(c, Command::Goto { .. } | Command::Open { .. } | Command::TabNew { .. }) {
                for text in proposals(said, &e["page"]) {
                    // Numbers (an order id, a count) may differ next run.
                    let line = format!("expect text {}", quote(&text));
                    if text.chars().any(|c| c.is_ascii_digit()) {
                        steps.push_str(&format!("  # {line}   (has numbers: may change between runs)\n"));
                    } else {
                        steps.push_str(&format!("  {line}\n"));
                    }
                }
            }
            said = &e["page"];
        }
        if url.is_some() {
            at = url;
        }
    }
    (steps, secrets)
}

fn quote_always(s: &str) -> String {
    let q = quote(s);
    if q.starts_with('"') || q.starts_with('\'') {
        q
    } else {
        format!("\"{q}\"")
    }
}

/// `primavera from-session <name> [--scenario <name>] [--out <file.pv>]`.
pub fn main(args: &[String]) -> Result<String, String> {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let values: Vec<&String> = [flag("--scenario"), flag("--out")].into_iter().flatten().collect();
    let session = args
        .iter()
        .find(|a| !a.starts_with("--") && !values.contains(a))
        .ok_or("usage: primavera from-session <navigera session> [--scenario <name>] [--out <file.pv>]")?;
    let scenario = flag("--scenario").cloned().unwrap_or_else(|| format!("What session {session} did"));
    let text = render(&history(session)?, session, &scenario);
    match flag("--out") {
        Some(path) => {
            std::fs::write(path, &text).map_err(|e| format!("{path}: {e}"))?;
            Ok(format!("wrote {path}; check it with `primavera lint {path}`, then run it\n"))
        }
        None => Ok(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refs_become_locators_and_navigations_get_an_expect() {
        let role = |r: &str, n: &str| json!([{"kind": "role", "flags": ["--role", r, "--name", n]}]);
        let h = vec![
            json!({"command": {"op": "goto", "url": "http://127.0.0.1:8000/login"}, "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "click", "ref": 39}, "locators": role("button", "Accept all"), "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "fill", "ref": 29, "value": "ada@example.com"}, "locators": role("textbox", "Email"), "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "fill", "label": "Password", "value": "hunter2"}, "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "fill", "ref": 30, "value": "s3cret"}, "secret": true, "locators": role("textbox", "Token Gamma"), "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "fill", "ref": 31, "value": "plain"}, "secret": false, "locators": role("textbox", "Password hint"), "url": "http://127.0.0.1:8000/login"}),
            json!({"command": {"op": "click", "ref": 35}, "locators": role("button", "Sign in"), "url": "http://127.0.0.1:8000/account?tab=1",
                   "page": {"h1": "Your account", "status": ["Signed in", "3 new orders", "Loading…"]}}),
            json!({"command": {"op": "click", "ref": 50}, "locators": [], "url": "http://127.0.0.1:8000/account?tab=1"}),
            json!({"command": {"op": "mouse", "action": "click", "x": 10.0, "y": 20.0}, "url": "http://127.0.0.1:8000/account?tab=1"}),
            json!({"command": {"op": "goto", "url": "https://other.test/x"}, "url": "https://other.test/x"}),
            json!({"command": {"op": "wait", "download": "*.csv", "timeout_ms": 9000.0}, "url": "https://other.test/x"}),
        ];
        let pv = render(&h, "work", "Signs in");
        let want = "base http://127.0.0.1:8000\n\nscenario \"Signs in\"\n  goto /login\n  \
                    click --role button --name \"Accept all\"\n  fill --role textbox --name Email ada@example.com\n  \
                    fill --label Password ${secret:PASSWORD}\n  fill --role textbox --name \"Token Gamma\" ${secret:TOKEN_GAMMA}\n  \
                    fill --role textbox --name \"Password hint\" plain\n  click --role button --name \"Sign in\"\n  \
                    expect url ends-with /account?tab=1\n  expect text \"Your account\"\n  expect text \"Signed in\"\n  \
                    # expect text \"3 new orders\"   (has numbers: may change between runs)\n  # left out: ref 50: no locator found it again\n  \
                    # left out: mouse click at coordinates (kept out: they move with the layout)\n  \
                    goto https://other.test/x\n  wait --timeout-ms 9000 --download *.csv\n";
        assert!(pv.ends_with(want), "{pv}");
        assert!(pv.contains("${secret:PASSWORD}, ${secret:TOKEN_GAMMA}: give them with --secrets"), "{pv}");
        assert!(!pv.contains("s3cret") && !pv.contains("hunter2"), "{pv}");
        // What it writes reads back as a scenario.
        let s = crate::suite::parse(std::path::Path::new("t.pv"), &pv);
        assert!(s.is_ok(), "{pv}\n{:?}", s.err());
        assert_eq!(origin("https://a.test:81/p?q"), Some("https://a.test:81"));
        assert!(render(&[], "s", "x").contains("history is empty"));
    }
}
