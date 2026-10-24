//! `expect` lines: what to check, how to read it from the page, and how to
//! compare. Every expectation is retried until it holds or its timeout
//! runs out (see `runner`), like Playwright's web-first assertions.
//!
//! ```text
//! expect url ends-with /cart
//! expect title contains Shop
//! expect text "Thank you"              (visible text, frames included)
//! expect no text "Error"
//! expect --role button --name Pay enabled
//! expect --testid total text is "$36.00"
//! expect --label Email value contains @
//! expect --selector .card count >= 3
//! expect --selector #faq attr open is ""
//! expect --testid badge style color is "rgb(0, 128, 0)"
//! expect ${order} matches "^ORD-[0-9]+$"
//! expect request POST /api/orders json $.total is 36
//! expect download report-*.csv
//! expect no errors
//! expect not --selector .toast visible   (any expectation can be negated)
//! expect text "Saved" --soft             (noted, the scenario goes on, fails at the end)
//! ```

#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Url,
    Title,
    /// Visible text anywhere on the page, or inside the `--within` target.
    Text {
        text: String,
        within: Option<Vec<String>>,
    },
    /// A variable (the word as written, `${…}`), interpolated at run time.
    Var(String),
    /// An element (navigera target flags) and what to read from it.
    Target {
        flags: Vec<String>,
        prop: Prop,
    },
    /// Page exceptions and console errors.
    Errors,
    /// A JavaScript expression in the page: truthy, or compared.
    Js(String),
    /// A request the page made (since the scenario started): method (or
    /// `*`), URL glob, an optional status, and what to compare: nothing
    /// (one was made), a value in its JSON response, or how many.
    Request {
        method: String,
        url: String,
        status: Option<String>,
        read: RequestRead,
    },
    /// A cookie of the browser context: present, or its value compared.
    Cookie(String),
    /// `storage local|session <key>`: present, or its value compared.
    Storage {
        area: String,
        key: String,
    },
    /// A finished download whose file name matches the glob.
    Download(String),
    /// A console message (any level) compared: `console contains "x"`.
    Console,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestRead {
    Made,
    /// `json <$.path>`: a value in the last matching response's JSON.
    Json(String),
    Count,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    /// visible, hidden, enabled, disabled, checked, unchecked, focused,
    /// editable, empty.
    State(String),
    Text,
    Value,
    Attr(String),
    /// A computed CSS property.
    Style(String),
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Matcher {
    Is,
    Contains,
    StartsWith,
    EndsWith,
    Matches,
    Gt,
    Ge,
    Lt,
    Le,
    /// A state or the presence of text: no value to compare.
    Holds,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub negate: bool,
    pub subject: Subject,
    pub matcher: Matcher,
    pub value: String,
    pub timeout_ms: Option<u64>,
    /// `--soft`: a failure is noted and the scenario goes on; it fails at
    /// the end.
    pub soft: bool,
}

const STATES: &[&str] =
    &["visible", "hidden", "enabled", "disabled", "checked", "unchecked", "focused", "editable", "empty"];

/// The words of a JSON path: `$`, `.key`, `[0]`, `["key"]`.
pub fn json_path(path: &str) -> Result<Vec<JsonStep>, String> {
    let bad = || format!("bad JSON path {path:?} (like $.items[0].name)");
    let mut rest = path.strip_prefix('$').ok_or_else(bad)?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            if end == 0 {
                return Err(bad());
            }
            out.push(JsonStep::Key(r[..end].to_string()));
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']').ok_or_else(bad)?;
            let inner = r[..end].trim();
            if let Ok(i) = inner.parse::<usize>() {
                out.push(JsonStep::Index(i));
            } else if inner.len() >= 2
                && (inner.starts_with('"') && inner.ends_with('"') || inner.starts_with('\'') && inner.ends_with('\''))
            {
                out.push(JsonStep::Key(inner[1..inner.len() - 1].to_string()));
            } else {
                return Err(bad());
            }
            rest = &r[end + 1..];
        } else {
            return Err(bad());
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonStep {
    Key(String),
    Index(usize),
}

/// The value at a JSON path, as text: strings as they are, the rest as JSON.
pub fn json_at(v: &serde_json::Value, path: &[JsonStep]) -> Option<String> {
    let mut cur = v;
    for st in path {
        cur = match st {
            JsonStep::Key(k) => cur.get(k)?,
            JsonStep::Index(i) => cur.get(*i)?,
        };
    }
    Some(match cur {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn matcher(word: &str) -> Option<Matcher> {
    Some(match word {
        "is" | "==" => Matcher::Is,
        "contains" => Matcher::Contains,
        "starts-with" => Matcher::StartsWith,
        "ends-with" => Matcher::EndsWith,
        "matches" => Matcher::Matches,
        ">" => Matcher::Gt,
        ">=" => Matcher::Ge,
        "<" => Matcher::Lt,
        "<=" => Matcher::Le,
        _ => return None,
    })
}

/// `<matcher> <value>` and nothing after it.
fn tail(words: &[String], what: &str) -> Result<(Matcher, String), String> {
    let m = words.first().and_then(|w| matcher(w)).ok_or_else(|| {
        format!(
            "after {what}: is, contains, starts-with, ends-with, matches, >, >=, <, <= (got {:?})",
            words.first().map(String::as_str).unwrap_or("nothing")
        )
    })?;
    match words.len() {
        2 => {
            if m == Matcher::Matches {
                regex::Regex::new(&words[1]).map_err(|e| format!("bad regular expression {:?}: {e}", words[1]))?;
            }
            Ok((m, words[1].clone()))
        }
        1 => Err(format!("{} needs a value", words[0])),
        _ => Err(format!("one value after {} (quote it if it has spaces): {:?}", words[0], &words[2..])),
    }
}

/// Parse the words after `expect`.
pub fn parse(words: &[String]) -> Result<Expect, String> {
    let mut e = parse_inner(words)?;
    if words.iter().any(|w| w == "--soft") {
        e.soft = true;
    }
    Ok(e)
}

fn parse_inner(words: &[String]) -> Result<Expect, String> {
    let mut words: Vec<String> = words.iter().filter(|w| *w != "--soft").cloned().collect();
    let soft = false;
    let mut timeout_ms = None;
    if let Some(i) = words.iter().position(|w| w == "--timeout-ms") {
        let v = words.get(i + 1).ok_or("--timeout-ms needs a number")?;
        timeout_ms = Some(v.parse::<u64>().map_err(|_| format!("--timeout-ms takes milliseconds, not {v:?}"))?);
        words.drain(i..i + 2);
    }
    let mut negate = false;
    if words.first().map(String::as_str) == Some("not") {
        negate = true;
        words.remove(0);
    }
    if words.first().map(String::as_str) == Some("no") {
        negate = !negate;
        words.remove(0);
        return match words.first().map(String::as_str) {
            Some("errors") if words.len() == 1 => {
                // `no errors`: the error list is empty.
                Ok(Expect {
                    negate: !negate,
                    subject: Subject::Errors,
                    matcher: Matcher::Holds,
                    value: String::new(),
                    timeout_ms,
                    soft,
                })
            }
            Some("text") if words.len() >= 2 => Ok(Expect {
                negate,
                subject: Subject::Text { text: words[1].clone(), within: within(&words[2..])? },
                matcher: Matcher::Holds,
                value: String::new(),
                timeout_ms,
                soft,
            }),
            Some("cookie") if words.len() == 2 => Ok(Expect {
                negate,
                subject: Subject::Cookie(words[1].clone()),
                matcher: Matcher::Holds,
                value: String::new(),
                timeout_ms,
                soft,
            }),
            Some("request") if words.len() == 3 => Ok(Expect {
                negate,
                subject: Subject::Request {
                    method: words[1].to_uppercase(),
                    url: words[2].clone(),
                    status: None,
                    read: RequestRead::Made,
                },
                matcher: Matcher::Holds,
                value: String::new(),
                timeout_ms,
                soft,
            }),
            Some("download") if words.len() == 2 => Ok(Expect {
                negate,
                subject: Subject::Download(words[1].clone()),
                matcher: Matcher::Holds,
                value: String::new(),
                timeout_ms,
                soft,
            }),
            Some("console") if words.len() == 2 => Ok(Expect {
                negate,
                subject: Subject::Console,
                matcher: Matcher::Contains,
                value: words[1].clone(),
                timeout_ms,
                soft,
            }),
            _ => Err("after `no`: `errors`, `text \"<text>\"`, `cookie <name>`, `request <METHOD> <url>`, `download <glob>` or `console \"<text>\"`".into()),
        };
    }
    let first = words.first().cloned().ok_or(
        "expect what? (url, title, text, a target, ${var}, js, request, cookie, storage, download, console, no errors)",
    )?;
    // A subject with an optional comparison after it: present/truthy without
    // one, gone with `absent`.
    let optional_tail = |rest: &[String], what: &str| -> Result<(Matcher, String), String> {
        if rest.is_empty() {
            Ok((Matcher::Holds, String::new()))
        } else {
            tail(rest, what)
        }
    };
    let absent = match first.as_str() {
        "cookie" => words.len() == 3,
        "storage" => words.len() == 4,
        _ => false,
    };
    if absent && words.last().map(String::as_str) == Some("absent") {
        words.pop();
        negate = !negate;
    }
    let (subject, matcher, value) = match first.as_str() {
        // `eval` reads as navigera's own command does.
        "js" | "eval" => {
            let code = words.get(1).ok_or("expect js \"<expression>\"")?.clone();
            let (m, v) = optional_tail(&words[2..], "js")?;
            (Subject::Js(code), m, v)
        }
        "request" => {
            // request <METHOD> <url glob> [status <n>] [json <$.path> | count <comparison> <value>]
            let usage =
                "expect request <METHOD> <url glob> [status <n>] [json <$.path> | count] [<comparison> <value>]";
            let method = words.get(1).ok_or(usage)?.to_uppercase();
            let url = words.get(2).ok_or(usage)?.clone();
            let mut rest = &words[3..];
            let mut status = None;
            if rest.first().map(String::as_str) == Some("status") {
                status = Some(rest.get(1).ok_or("status needs a number")?.clone());
                rest = &rest[2..];
            }
            match rest.first().map(String::as_str) {
                None => {
                    (Subject::Request { method, url, status, read: RequestRead::Made }, Matcher::Holds, String::new())
                }
                Some("json") => {
                    let path = rest.get(1).ok_or("json needs a path, like $.items[0].id")?.clone();
                    json_path(&path)?;
                    let (m, v) = tail(&rest[2..], "the JSON path")?;
                    (Subject::Request { method, url, status, read: RequestRead::Json(path) }, m, v)
                }
                Some("count") => {
                    let (m, v) = tail(&rest[1..], "count")?;
                    v.parse::<f64>().map_err(|_| format!("count compares with a number, not {v:?}"))?;
                    (Subject::Request { method, url, status, read: RequestRead::Count }, m, v)
                }
                Some(other) => {
                    return Err(format!(
                        "after the request: `status <n>`, `json <$.path> …`, `count …` or nothing (got {other:?})"
                    ))
                }
            }
        }
        "download" => match words.len() {
            2 => (Subject::Download(words[1].clone()), Matcher::Holds, String::new()),
            _ => return Err("expect download <file name glob> (like report-*.csv)".into()),
        },
        "console" => {
            let (m, v) = tail(&words[1..], "console")?;
            (Subject::Console, m, v)
        }
        "cookie" => {
            let name = words.get(1).ok_or("expect cookie <name> [<comparison> <value>]")?.clone();
            let (m, v) = optional_tail(&words[2..], "cookie")?;
            (Subject::Cookie(name), m, v)
        }
        "storage" => {
            let usage = "expect storage local|session <key> [<comparison> <value>]";
            let area = words.get(1).filter(|a| *a == "local" || *a == "session").ok_or(usage)?.clone();
            let key = words.get(2).ok_or(usage)?.clone();
            let (m, v) = optional_tail(&words[3..], "storage")?;
            (Subject::Storage { area, key }, m, v)
        }
        "url" => {
            let (m, v) = tail(&words[1..], "url")?;
            (Subject::Url, m, v)
        }
        "title" => {
            let (m, v) = tail(&words[1..], "title")?;
            (Subject::Title, m, v)
        }
        "text" => match words.len() {
            2.. => {
                (Subject::Text { text: words[1].clone(), within: within(&words[2..])? }, Matcher::Holds, String::new())
            }
            _ => return Err("expect text \"<text>\" (quote text with spaces; it takes no comparator)".into()),
        },
        w if w.starts_with("${") => {
            let (m, v) = tail(&words[1..], w)?;
            (Subject::Var(w.to_string()), m, v)
        }
        w if w.starts_with("--") => {
            // Target flags: each takes a value.
            let mut i = 0;
            let mut flags = Vec::new();
            while i < words.len() && words[i].starts_with("--") {
                let v = words.get(i + 1).ok_or_else(|| format!("{} needs a value", words[i]))?;
                flags.push(words[i].clone());
                flags.push(v.clone());
                i += 2;
            }
            let rest = &words[i..];
            match rest.first().map(String::as_str) {
                Some(s) if STATES.contains(&s) && rest.len() == 1 => {
                    (Subject::Target { flags, prop: Prop::State(s.to_string()) }, Matcher::Holds, String::new())
                }
                Some("text") => {
                    let (m, v) = tail(&rest[1..], "text")?;
                    (Subject::Target { flags, prop: Prop::Text }, m, v)
                }
                Some("value") => {
                    let (m, v) = tail(&rest[1..], "value")?;
                    (Subject::Target { flags, prop: Prop::Value }, m, v)
                }
                Some("attr") => {
                    let name = rest.get(1).ok_or("attr needs a name")?.clone();
                    let (m, v) = tail(&rest[2..], "attr")?;
                    (Subject::Target { flags, prop: Prop::Attr(name) }, m, v)
                }
                Some("style") => {
                    let name = rest.get(1).ok_or("style needs a CSS property, like color")?.clone();
                    let (m, v) = tail(&rest[2..], "style")?;
                    (Subject::Target { flags, prop: Prop::Style(name) }, m, v)
                }
                Some("count") => {
                    if !flags.iter().step_by(2).all(|f| f == "--selector") || flags.len() != 2 {
                        return Err("count takes one --selector <css>".into());
                    }
                    let (m, v) = tail(&rest[1..], "count")?;
                    v.parse::<f64>().map_err(|_| format!("count compares with a number, not {v:?}"))?;
                    (Subject::Target { flags, prop: Prop::Count }, m, v)
                }
                other => {
                    return Err(format!(
                        "after the target: a state ({}), or text/value/attr <name>/style <property>/count and a comparison (got {:?})",
                        STATES.join(", "),
                        other.unwrap_or("nothing")
                    ))
                }
            }
        }
        other => {
            return Err(format!(
                "expect what? {other:?} (url, title, text, a target like --role button, ${{var}}, no errors)"
            ))
        }
    };
    Ok(Expect { negate, subject, matcher, value, timeout_ms, soft })
}

/// `--within <target flags>` after `text "<t>"`: where to look.
fn within(rest: &[String]) -> Result<Option<Vec<String>>, String> {
    match rest.first().map(String::as_str) {
        None => Ok(None),
        Some("--within") => {
            let flags = &rest[1..];
            if flags.is_empty()
                || !flags.len().is_multiple_of(2)
                || !flags.iter().step_by(2).all(|f| f.starts_with("--"))
            {
                return Err(
                    "--within takes a target: --within --testid toast, --within --role dialog --name Cart".into()
                );
            }
            Ok(Some(flags.to_vec()))
        }
        Some(other) => {
            Err(format!("after the text: `--within <target>` or nothing (quote text with spaces; got {other:?})"))
        }
    }
}

/// `*` matches any run of characters (`/api/orders/*`); everything else is
/// literal.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(k) => rest = &rest[k + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Does `actual` satisfy `m` against `expected`?
pub fn compare(actual: &str, m: Matcher, expected: &str) -> bool {
    let num = |s: &str| s.trim().parse::<f64>().ok();
    match m {
        Matcher::Is => actual == expected || matches!((num(actual), num(expected)), (Some(a), Some(b)) if a == b),
        Matcher::Contains => actual.contains(expected),
        Matcher::StartsWith => actual.starts_with(expected),
        Matcher::EndsWith => actual.ends_with(expected),
        Matcher::Matches => regex::Regex::new(expected).is_ok_and(|r| r.is_match(actual)),
        Matcher::Gt | Matcher::Ge | Matcher::Lt | Matcher::Le => match (num(actual), num(expected)) {
            (Some(a), Some(b)) => match m {
                Matcher::Gt => a > b,
                Matcher::Ge => a >= b,
                Matcher::Lt => a < b,
                _ => a <= b,
            },
            _ => false,
        },
        Matcher::Holds => true,
    }
}

impl Matcher {
    pub fn word(self) -> &'static str {
        match self {
            Matcher::Is => "is",
            Matcher::Contains => "contains",
            Matcher::StartsWith => "starts-with",
            Matcher::EndsWith => "ends-with",
            Matcher::Matches => "matches",
            Matcher::Gt => ">",
            Matcher::Ge => ">=",
            Matcher::Lt => "<",
            Matcher::Le => "<=",
            Matcher::Holds => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<Expect, String> {
        parse(&crate::suite::tokenize(line).unwrap())
    }

    #[test]
    fn subjects_and_matchers_parse() {
        let e = p("url ends-with /cart").unwrap();
        assert_eq!((e.subject, e.matcher, e.value.as_str()), (Subject::Url, Matcher::EndsWith, "/cart"));
        let e = p("text \"Thank you\"").unwrap();
        assert_eq!(e.subject, Subject::Text { text: "Thank you".into(), within: None });
        let e = p("no text Error").unwrap();
        assert!(e.negate);
        let e = p("--role button --name \"Place order\" enabled").unwrap();
        assert_eq!(
            e.subject,
            Subject::Target {
                flags: vec!["--role".into(), "button".into(), "--name".into(), "Place order".into()],
                prop: Prop::State("enabled".into())
            }
        );
        let e = p("--selector .card count >= 3 --timeout-ms 800").unwrap();
        assert_eq!((e.matcher, e.value.as_str(), e.timeout_ms), (Matcher::Ge, "3", Some(800)));
        let e = p("--selector #faq attr open is \"\"").unwrap();
        assert!(matches!(e.subject, Subject::Target { prop: Prop::Attr(ref a), .. } if a == "open"));
        let e = p("not --selector .toast visible").unwrap();
        assert!(e.negate);
        let e = p("no errors").unwrap();
        assert_eq!((e.subject, e.negate), (Subject::Errors, false));
        let e = p("${order} matches \"^ORD-[0-9]+$\"").unwrap();
        assert_eq!(e.subject, Subject::Var("${order}".into()));
    }

    #[test]
    fn mistakes_say_what_was_expected() {
        assert!(p("url sounds-like x").unwrap_err().contains("after url"));
        assert!(p("title is").unwrap_err().contains("needs a value"));
        assert!(p("--testid total text is $36 dollars").unwrap_err().contains("quote it"));
        assert!(p("--role button pressed").unwrap_err().contains("a state"));
        assert!(p("--role button count is 2").unwrap_err().contains("--selector"));
        assert!(p("url matches \"(\"").unwrap_err().contains("regular expression"));
        assert!(p("text a b").is_err());
    }

    #[test]
    fn page_state_subjects() {
        let e = p("js \"document.forms.length\" >= 1").unwrap();
        assert_eq!((e.subject, e.matcher), (Subject::Js("document.forms.length".into()), Matcher::Ge));
        let e = p("eval \"String(1 + 1)\" is 2").unwrap();
        assert_eq!(e.subject, Subject::Js("String(1 + 1)".into()));
        let e = p("js \"window.ready === true\"").unwrap();
        assert_eq!(e.matcher, Matcher::Holds);
        let e = p("request post /api/orders status 201").unwrap();
        assert_eq!(
            e.subject,
            Subject::Request {
                method: "POST".into(),
                url: "/api/orders".into(),
                status: Some("201".into()),
                read: RequestRead::Made
            }
        );
        let e = p("no request GET /api/tracking*").unwrap();
        assert!(e.negate);
        let e = p("cookie session").unwrap();
        assert_eq!((e.subject, e.matcher), (Subject::Cookie("session".into()), Matcher::Holds));
        let e = p("no cookie session").unwrap();
        assert!(e.negate);
        let e = p("storage local cart contains sku-1").unwrap();
        assert_eq!(e.subject, Subject::Storage { area: "local".into(), key: "cart".into() });
        assert!(p("storage disk x").is_err());
        assert!(p("request POST /x status").is_err());
        assert!(glob("/api/orders/*", "/api/orders/12"));
        assert!(glob("*/cart", "http://x/cart"));
        assert!(glob("/a*c*e", "/abcde"));
        assert!(!glob("/api/orders", "/api/orders/12"));
    }

    #[test]
    fn more_subjects() {
        let e = p("text Saved --within --testid toast --soft").unwrap();
        assert_eq!(
            e.subject,
            Subject::Text { text: "Saved".into(), within: Some(vec!["--testid".into(), "toast".into()]) }
        );
        assert!(e.soft);
        assert!(p("no text Oops --within --role dialog").unwrap().negate);
        assert!(p("text a --within").is_err());
        let e = p("--testid badge style color is \"rgb(0, 128, 0)\"").unwrap();
        assert!(matches!(e.subject, Subject::Target { prop: Prop::Style(ref c), .. } if c == "color"));
        assert!(
            matches!(p("--label Notes empty").unwrap().subject, Subject::Target { prop: Prop::State(ref s), .. } if s == "empty")
        );
        let e = p("request POST /api/orders status 201 json $.items[0].sku is sku-1").unwrap();
        assert!(
            matches!(e.subject, Subject::Request { read: RequestRead::Json(ref j), ref status, .. } if j == "$.items[0].sku" && status.as_deref() == Some("201"))
        );
        assert_eq!((e.matcher, e.value.as_str()), (Matcher::Is, "sku-1"));
        let e = p("request GET /api/items* count >= 2").unwrap();
        assert!(matches!(e.subject, Subject::Request { read: RequestRead::Count, .. }));
        assert!(p("request GET /x json total is 1").unwrap_err().contains("JSON path"));
        assert!(p("request GET /x count is many").is_err());
        let e = p("cookie session absent").unwrap();
        assert!(e.negate && e.matcher == Matcher::Holds);
        assert!(p("storage local cart absent").unwrap().negate);
        assert!(!p("cookie mode is absent").unwrap().negate);
        assert_eq!(p("download report-*.csv").unwrap().subject, Subject::Download("report-*.csv".into()));
        assert!(p("no download *.exe").unwrap().negate);
        let e = p("console contains ready").unwrap();
        assert_eq!((e.subject, e.matcher), (Subject::Console, Matcher::Contains));
        assert!(p("no console Warning").unwrap().negate);
        let path = json_path("$.a[1][\"b c\"].d").unwrap();
        let v = serde_json::json!({"a": [0, {"b c": {"d": 7}}]});
        assert_eq!(json_at(&v, &path).as_deref(), Some("7"));
        assert_eq!(json_at(&v, &json_path("$.a[5]").unwrap()), None);
        assert!(json_path("a.b").is_err());
    }

    #[test]
    fn comparisons() {
        assert!(compare("$36.00", Matcher::Is, "$36.00"));
        assert!(compare("3", Matcher::Is, "3.0"));
        assert!(compare("12", Matcher::Ge, "3"));
        assert!(!compare("abc", Matcher::Gt, "3"));
        assert!(compare("ORD-123", Matcher::Matches, "^ORD-[0-9]+$"));
        assert!(compare("http://x/cart", Matcher::EndsWith, "/cart"));
    }
}
