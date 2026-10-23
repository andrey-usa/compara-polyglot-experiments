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
//! expect ${order} matches "^ORD-[0-9]+$"
//! expect no errors
//! expect not --selector .toast visible   (any expectation can be negated)
//! ```

#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Url,
    Title,
    /// Visible text anywhere on the page.
    Text(String),
    /// A variable (the word as written, `${…}`), interpolated at run time.
    Var(String),
    /// An element (navigera target flags) and what to read from it.
    Target {
        flags: Vec<String>,
        prop: Prop,
    },
    /// Page exceptions and console errors.
    Errors,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    /// visible, hidden, enabled, disabled, checked, unchecked, focused, editable.
    State(String),
    Text,
    Value,
    Attr(String),
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
}

const STATES: &[&str] = &["visible", "hidden", "enabled", "disabled", "checked", "unchecked", "focused", "editable"];

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
    let mut words: Vec<String> = words.to_vec();
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
                })
            }
            Some("text") if words.len() == 2 => Ok(Expect {
                negate,
                subject: Subject::Text(words[1].clone()),
                matcher: Matcher::Holds,
                value: String::new(),
                timeout_ms,
            }),
            _ => Err("after `no`: `errors`, or `text \"<text>\"`".into()),
        };
    }
    let first = words.first().cloned().ok_or("expect what? (url, title, text, a target, ${var}, no errors)")?;
    let (subject, matcher, value) = match first.as_str() {
        "url" => {
            let (m, v) = tail(&words[1..], "url")?;
            (Subject::Url, m, v)
        }
        "title" => {
            let (m, v) = tail(&words[1..], "title")?;
            (Subject::Title, m, v)
        }
        "text" => match words.len() {
            2 => (Subject::Text(words[1].clone()), Matcher::Holds, String::new()),
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
                        "after the target: a state ({}), or text/value/attr <name>/count and a comparison (got {:?})",
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
    Ok(Expect { negate, subject, matcher, value, timeout_ms })
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
        assert_eq!(e.subject, Subject::Text("Thank you".into()));
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
    fn comparisons() {
        assert!(compare("$36.00", Matcher::Is, "$36.00"));
        assert!(compare("3", Matcher::Is, "3.0"));
        assert!(compare("12", Matcher::Ge, "3"));
        assert!(!compare("abc", Matcher::Gt, "3"));
        assert!(compare("ORD-123", Matcher::Matches, "^ORD-[0-9]+$"));
        assert!(compare("http://x/cart", Matcher::EndsWith, "/cart"));
    }
}
