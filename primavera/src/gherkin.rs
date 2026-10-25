//! `.feature` files, run as they are: Gherkin's `Feature`, `Background`,
//! `Scenario` (`Example`), `Scenario Outline` with `Examples`, tags and
//! `Given/When/Then/And/But/*` steps. A step is one of the run's own
//! `step "…"` blocks (in any `.pv` file it loads), else a phrase of the
//! built-in book; Cucumber JSON reports the steps one by one.
//!
//! ```text
//! step "I am signed in as {role}"
//!   as ${role}
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;
use serde_json::{json, Value};

use crate::runner::ScenarioResult;
use crate::suite::{parse_words, Action, Feature, Flow, ParseError, Scenario, Step, Suite};

const KEYWORDS: &[&str] = &["Given", "When", "Then", "And", "But", "*"];

/// The keyword a step line starts with, and the rest.
fn step_line(t: &str) -> Option<(&str, &str)> {
    KEYWORDS.iter().find_map(|k| t.strip_prefix(k).filter(|r| r.starts_with(' ')).map(|r| (*k, r.trim())))
}

/// `Name:` headers: the keyword and the title after it.
fn header<'a>(t: &'a str, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| t.strip_prefix(k).and_then(|r| r.strip_prefix(':')).map(str::trim))
}

fn cells(t: &str) -> Vec<String> {
    t.trim().trim_matches('|').split('|').map(|c| c.trim().to_string()).collect()
}

/// A Gherkin step, kept as written until [`resolve`] finds what it does.
fn pending(line: usize, keyword: &str, text: &str) -> Step {
    Step { line, text: format!("{keyword} {text}"), action: Action::Gherkin(text.to_string()), indent: 0 }
}

/// An `Examples` table: its tags, header and rows.
struct Examples {
    tags: Vec<String>,
    head: Vec<String>,
    rows: Vec<Vec<String>>,
}

struct Outline {
    name: String,
    line: usize,
    tags: Vec<String>,
    steps: Vec<(usize, String, String)>,
    /// One table per `Examples`, with its own tags.
    tables: Vec<Examples>,
    /// Its `Rule`'s `Background`, run first.
    first: Vec<Step>,
}

/// Parse one `.feature` file. Its steps stay unresolved until [`resolve`].
pub fn parse(file: &Path, text: &str) -> Result<Suite, ParseError> {
    let err = |line: usize, message: String| ParseError { file: file.to_path_buf(), line, message };
    let mut suite = Suite { file: file.to_path_buf(), ..Suite::default() };
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Background,
        RuleBackground,
        Scenario,
        Outline,
        Examples,
    }
    let mut at = In::Nothing;
    let mut tags: Vec<String> = Vec::new();
    let mut feature_tags: Vec<String> = Vec::new();
    let mut outline: Option<Outline> = None;
    // Inside a `Rule`: its own `Background`, for its scenarios only.
    let mut in_rule = false;
    let mut rule_background: Vec<Step> = Vec::new();
    // Whether the block has steps yet (free text before them is a description).
    let mut stepped = false;
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let finish = |o: Option<Outline>,
                  suite: &mut Suite,
                  names: &mut BTreeMap<String, usize>|
     -> Result<(), ParseError> {
        let Some(o) = o else { return Ok(()) };
        if o.tables.iter().all(|t| t.rows.is_empty()) {
            return Err(err(o.line, "a `Scenario Outline` needs an `Examples` table with a row".into()));
        }
        let mut k = 0;
        for Examples { tags: extra, head, rows } in &o.tables {
            for row in rows {
                k += 1;
                let fill =
                    |s: &str| head.iter().zip(row).fold(s.to_string(), |s, (h, v)| s.replace(&format!("<{h}>"), v));
                let mut name = fill(&o.name);
                if name == o.name {
                    // Named by its values: `Sign in (pw=nope)`.
                    let values: Vec<String> = head.iter().zip(row).map(|(h, v)| format!("{h}={v}")).collect();
                    name = format!("{} ({})", o.name, values.join(", "));
                }
                if names.contains_key(&name) {
                    name = format!("{name} #{k}");
                }
                if let Some(first) = names.insert(name.clone(), o.line) {
                    return Err(err(o.line, format!("two scenarios named {name:?} (lines {first} and {})", o.line)));
                }
                suite.scenarios.push(Scenario {
                    name,
                    tags: o.tags.iter().chain(extra).cloned().collect(),
                    line: o.line,
                    steps: o
                        .first
                        .iter()
                        .cloned()
                        .chain(o.steps.iter().map(|(l, kw, t)| pending(*l, kw, &fill(t))))
                        .collect(),
                    vars: head.iter().cloned().zip(row.iter().cloned()).collect(),
                });
            }
        }
        Ok(())
    };
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('@') {
            tags.extend(t.split_whitespace().filter_map(|w| w.strip_prefix('@')).map(String::from));
            continue;
        }
        if let Some(name) = header(t, &["Feature"]) {
            if suite.feature.is_some() {
                return Err(err(line, "a second `Feature` in one file".into()));
            }
            feature_tags = std::mem::take(&mut tags);
            suite.feature = Some(Feature { name: name.to_string(), line, tags: feature_tags.clone() });
            at = In::Nothing;
            stepped = false;
            continue;
        }
        if suite.feature.is_none() {
            return Err(err(line, "a .feature file starts with `Feature: <name>`".into()));
        }
        if let Some(_rule) = header(t, &["Rule"]) {
            // Rules group scenarios; a `Background` under one is for them.
            finish(outline.take(), &mut suite, &mut names)?;
            tags.clear();
            in_rule = true;
            rule_background.clear();
            at = In::Nothing;
            continue;
        }
        if header(t, &["Background"]).is_some() {
            finish(outline.take(), &mut suite, &mut names)?;
            at = if in_rule { In::RuleBackground } else { In::Background };
            stepped = false;
            continue;
        }
        if let Some(name) = header(t, &["Scenario Outline", "Scenario Template"]) {
            finish(outline.take(), &mut suite, &mut names)?;
            let all = feature_tags.iter().cloned().chain(std::mem::take(&mut tags)).collect();
            outline = Some(Outline {
                name: name.to_string(),
                line,
                tags: all,
                steps: Vec::new(),
                tables: Vec::new(),
                first: rule_background.clone(),
            });
            at = In::Outline;
            stepped = false;
            continue;
        }
        if let Some(name) = header(t, &["Scenario", "Example"]) {
            finish(outline.take(), &mut suite, &mut names)?;
            if name.is_empty() {
                return Err(err(line, "a scenario needs a name".into()));
            }
            if let Some(first) = names.insert(name.to_string(), line) {
                return Err(err(line, format!("two scenarios named {name:?} (lines {first} and {line})")));
            }
            let all = feature_tags.iter().cloned().chain(std::mem::take(&mut tags)).collect();
            suite.scenarios.push(Scenario {
                name: name.to_string(),
                tags: all,
                line,
                steps: rule_background.clone(),
                vars: BTreeMap::new(),
            });
            at = In::Scenario;
            stepped = false;
            continue;
        }
        if header(t, &["Examples", "Scenarios"]).is_some() {
            let o =
                outline.as_mut().ok_or_else(|| err(line, "`Examples` belongs under a `Scenario Outline`".into()))?;
            o.tables.push(Examples { tags: std::mem::take(&mut tags), head: Vec::new(), rows: Vec::new() });
            at = In::Examples;
            continue;
        }
        if t.starts_with('|') {
            if at != In::Examples {
                return Err(err(
                    line,
                    "a data table under a step isn't supported yet (put the values in the step)".into(),
                ));
            }
            let Examples { head, rows, .. } =
                outline.as_mut().and_then(|o| o.tables.last_mut()).expect("an Examples table is open");
            let c = cells(t);
            if head.is_empty() {
                *head = c;
            } else if c.len() != head.len() {
                return Err(err(line, format!("{} cells in this row, {} in the header", c.len(), head.len())));
            } else {
                rows.push(c);
            }
            continue;
        }
        if t.starts_with("\"\"\"") || t.starts_with("```") {
            return Err(err(line, "a doc string under a step isn't supported yet".into()));
        }
        if let Some((kw, text)) = step_line(t) {
            stepped = true;
            match at {
                In::Background => suite.before_each.push(pending(line, kw, text)),
                In::RuleBackground => rule_background.push(pending(line, kw, text)),
                In::Scenario => {
                    suite.scenarios.last_mut().expect("a scenario is open").steps.push(pending(line, kw, text))
                }
                In::Outline => {
                    outline.as_mut().expect("an outline is open").steps.push((line, kw.to_string(), text.to_string()))
                }
                In::Examples => return Err(err(line, "a step after `Examples` (start a new scenario)".into())),
                In::Nothing => return Err(err(line, "a step outside any `Scenario` or `Background`".into())),
            }
            continue;
        }
        // Free text under a header is its description.
        if !stepped && at != In::Examples {
            continue;
        }
        return Err(err(line, format!("not a step: {t:?} (steps start with Given, When, Then, And, But or *)")));
    }
    finish(outline.take(), &mut suite, &mut names)?;
    if suite.feature.is_none() {
        return Err(err(1, "a .feature file starts with `Feature: <name>`".into()));
    }
    Ok(suite)
}

/// A `step "…"` pattern as a regex: `{name}` takes a quoted string or one
/// word. The names, in order.
pub fn step_pattern(pattern: &str) -> Result<(Regex, Vec<String>), String> {
    let mut re = String::from("^");
    let mut names = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}').ok_or_else(|| format!("`{{` without `}}` in {pattern:?}"))? + open;
        let name = &rest[open + 1..close];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("bad placeholder {{{name}}} in {pattern:?}"));
        }
        if names.iter().any(|n| n == name) {
            return Err(format!("{{{name}}} twice in {pattern:?}: name each placeholder once"));
        }
        re.push_str(&regex::escape(&rest[..open]));
        re.push_str(r#"(?:"([^"]*)"|'([^']*)'|(\S+))"#);
        names.push(name.to_string());
        rest = &rest[close + 1..];
    }
    re.push_str(&regex::escape(rest));
    re.push('$');
    Ok((Regex::new(&re).map_err(|e| e.to_string())?, names))
}

/// The built-in phrase book: the navigera words a phrase stands for.
fn phrase(text: &str) -> Option<Vec<String>> {
    static BOOK: std::sync::OnceLock<Vec<(Regex, &'static str)>> = std::sync::OnceLock::new();
    let book = BOOK.get_or_init(|| {
        [
            (r#"^I (?:open|go to|visit|am on|navigate to) "([^"]*)"$"#, "goto $1"),
            (r#"^I click(?: on)? "([^"]*)"$"#, "click --text $1"),
            (r#"^I (?:fill(?: in)?|enter) "([^"]*)" with "([^"]*)"$"#, "fill --label $1 $2"),
            (r#"^I type "([^"]*)" into "([^"]*)"$"#, "fill --label $2 $1"),
            (r#"^I select "([^"]*)" (?:from|in) "([^"]*)"$"#, "select --label $2 --value $1"),
            (r#"^I check "([^"]*)"$"#, "check --label $1"),
            (r#"^I uncheck "([^"]*)"$"#, "uncheck --label $1"),
            (r#"^I press "([^"]*)"$"#, "press $1"),
            (r#"^I wait for "([^"]*)"$"#, "wait --text $1"),
            (r#"^I (?:should )?see "([^"]*)"$"#, "expect text $1"),
            (r#"^I (?:should not|shouldn't|do not|don't) see "([^"]*)"$"#, "expect no text $1"),
            (r#"^the (?:URL|url|page URL) (is|contains|starts with|ends with|matches) "([^"]*)"$"#, "expect url $m $2"),
            (
                r#"^the (?:title|page title) (is|contains|starts with|ends with|matches) "([^"]*)"$"#,
                "expect title $m $2",
            ),
        ]
        .into_iter()
        .map(|(re, words)| (Regex::new(re).expect("a valid phrase"), words))
        .collect()
    });
    for (re, words) in book {
        let Some(c) = re.captures(text) else { continue };
        let group = |k: usize| c.get(k).map(|m| m.as_str().to_string()).unwrap_or_default();
        return Some(
            words
                .split(' ')
                .map(|w| match w {
                    "$1" => group(1),
                    "$2" => group(2),
                    "$m" => group(1).replace(' ', "-"),
                    w => w.to_string(),
                })
                .collect(),
        );
    }
    None
}

/// Every step of a `.feature` suite, found among the run's `step "…"`
/// blocks (`defs`, flows named `step "<pattern>"`), else in the phrase book.
pub fn resolve(suite: &mut Suite, defs: &[Flow]) -> Result<(), ParseError> {
    let compiled: Vec<(&Flow, Regex)> = defs
        .iter()
        .filter_map(|f| {
            let pattern = f.name.strip_prefix("step \"")?.strip_suffix('"')?;
            step_pattern(pattern).ok().map(|(re, _)| (f, re))
        })
        .collect();
    let file = suite.file.clone();
    let mut used: BTreeMap<String, Flow> = BTreeMap::new();
    let mut one = |st: &mut Step| -> Result<(), ParseError> {
        let Action::Gherkin(text) = &st.action else { return Ok(()) };
        let err = |m: String| ParseError { file: file.clone(), line: st.line, message: m };
        let found: Vec<(&Flow, Vec<String>)> = compiled
            .iter()
            .filter_map(|(f, re)| {
                let c = re.captures(text)?;
                let args = (0..f.params.len())
                    .map(|k| (1..=3).find_map(|g| c.get(k * 3 + g)).map(|m| m.as_str().to_string()).unwrap_or_default())
                    .collect();
                Some((*f, args))
            })
            .collect();
        st.action = match found.as_slice() {
            [(f, args)] => {
                used.insert(f.name.clone(), (*f).clone());
                Action::Run { flow: f.name.clone(), args: args.clone() }
            }
            [] => {
                let words = phrase(text).ok_or_else(|| {
                    err(format!(
                        "no step matches {text:?}: write one in a .pv file (`step \"{}\"` and its steps indented under it), or use a built-in phrase",
                        text.replace('"', "'")
                    ))
                })?;
                parse_words(&words).map_err(err)?
            }
            many => {
                let names: Vec<&str> = many.iter().map(|(f, _)| f.name.as_str()).collect();
                return Err(err(format!("{text:?} matches more than one step: {}", names.join(", "))));
            }
        };
        Ok(())
    };
    for st in suite.before_each.iter_mut() {
        one(st)?;
    }
    for sc in suite.scenarios.iter_mut() {
        for st in sc.steps.iter_mut() {
            one(st)?;
        }
    }
    suite.flows.extend(used);
    Ok(())
}

/// Cucumber JSON: one feature per `.feature` file, its scenarios as they
/// ran (each try's last), every step passed, failed or skipped.
pub fn cucumber(results: &[ScenarioResult], suites: &[Suite]) -> String {
    let mut features = Vec::new();
    for suite in suites {
        let Some(feature) = &suite.feature else { continue };
        let uri = suite.file.display().to_string().replace('\\', "/");
        let fid = slug(&feature.name);
        let mut elements = Vec::new();
        for r in results.iter().filter(|r| r.file == suite.file) {
            let Some(sc) = suite.scenarios.iter().find(|s| r.base_name() == s.name) else { continue };
            // Each step as it ran; after a failure the rest are skipped.
            let message = r.failure.as_ref().map(|f| f.message.clone()).unwrap_or_default();
            let mut over = false;
            let mut steps: Vec<Value> = Vec::new();
            for st in suite.before_each.iter().chain(&sc.steps) {
                let (keyword, name) = st.text.split_once(' ').unwrap_or((&st.text, ""));
                let ran = r.steps.iter().find(|s| s.line == st.line);
                let at_failure = r.failure.as_ref().is_some_and(|f| f.line == st.line);
                let result = match ran {
                    _ if over => json!({"status": "skipped"}),
                    Some(s) if s.ok && !at_failure => json!({"status": "passed", "duration": (s.ms * 1e6) as u64}),
                    Some(s) => {
                        over = true;
                        json!({"status": "failed", "duration": (s.ms * 1e6) as u64, "error_message": message})
                    }
                    None if at_failure => {
                        over = true;
                        json!({"status": "failed", "duration": 0, "error_message": message})
                    }
                    None => json!({"status": "skipped"}),
                };
                steps.push(json!({"keyword": format!("{keyword} "), "name": name, "line": st.line, "result": result}));
            }
            // A failure no step owns (inside a `step` block's own lines): the
            // first step that didn't pass.
            if r.failure.is_some() && !over {
                if let Some(first) = steps.iter_mut().find(|s| s["result"]["status"] == "skipped") {
                    first["result"] = json!({"status": "failed", "duration": 0, "error_message": message});
                }
            }
            elements.push(json!({
                "id": format!("{fid};{}", slug(&r.name)),
                "keyword": "Scenario",
                "type": "scenario",
                "name": r.name,
                "line": r.line,
                "tags": r.tags.iter().map(|t| json!({"name": format!("@{t}"), "line": r.line})).collect::<Vec<_>>(),
                "steps": steps,
            }));
        }
        features.push(json!({
            "uri": uri,
            "id": fid,
            "keyword": "Feature",
            "name": feature.name,
            "line": feature.line,
            "tags": feature.tags.iter().map(|t| json!({"name": format!("@{t}"), "line": feature.line})).collect::<Vec<_>>(),
            "elements": elements,
        }));
    }
    serde_json::to_string_pretty(&Value::Array(features)).unwrap_or_default() + "\n"
}

fn slug(s: &str) -> String {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expect::{Expect, Subject};

    const FEATURE: &str = "@shop
Feature: Coupons
  People pay less with a code.

  Background:
    Given I open \"/cart\"

  @smoke
  Scenario: A coupon lowers the total
    When I fill \"Coupon code\" with \"SAVE10\"
    And I click \"Apply coupon\"
    Then I see \"Discount -$4.00\"
    But I don't see \"Invalid code\"
    And the URL ends with \"/cart\"

  Scenario Outline: Ship to <country>
    Given I am signed in as shopper
    When I select \"<country>\" from \"Country\"
    Then I see \"<fee>\"

    @eu
    Examples:
      | country | fee |
      | France  | $9  |
      | Spain   | $12 |
";

    #[test]
    fn a_rules_background_is_for_its_own_scenarios() {
        let text = "Feature: Accounts\n  Background:\n    Given I open \"/\"\n\n  Rule: signed in\n    Background:\n      Given I open \"/login\"\n\n    Scenario: sees orders\n      Then I see \"Orders\"\n\n    Scenario Outline: sees <page>\n      Then I see \"<page>\"\n      Examples:\n        | page |\n        | Cart |\n\n  Rule: guest\n    Scenario: sees the shop\n      Then I see \"Shop\"\n";
        let s = parse(Path::new("r.feature"), text).unwrap();
        assert_eq!(s.before_each.len(), 1, "the feature's Background");
        let lines: Vec<Vec<usize>> = s.scenarios.iter().map(|c| c.steps.iter().map(|st| st.line).collect()).collect();
        assert_eq!(
            lines,
            [vec![7, 10], vec![7, 13], vec![20]],
            "the Rule's Background is line 7, for its scenarios only"
        );
    }

    #[test]
    fn features_scenarios_outlines_and_tags() {
        let s = parse(Path::new("c.feature"), FEATURE).unwrap();
        let f = s.feature.as_ref().unwrap();
        assert_eq!((f.name.as_str(), f.tags.as_slice()), ("Coupons", ["shop".to_string()].as_slice()));
        assert_eq!(s.before_each.len(), 1);
        let names: Vec<&str> = s.scenarios.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["A coupon lowers the total", "Ship to France", "Ship to Spain"]);
        assert_eq!(s.scenarios[0].tags, ["shop", "smoke"]);
        assert_eq!(s.scenarios[2].tags, ["shop", "eu"]);
        assert_eq!(s.scenarios[2].steps[1].text, "When I select \"Spain\" from \"Country\"");
        assert_eq!(s.scenarios[0].steps[3].line, 13);
    }

    #[test]
    fn steps_resolve_to_the_runs_own_then_the_phrase_book() {
        let mut s = parse(Path::new("c.feature"), FEATURE).unwrap();
        let def = Flow {
            name: "step \"I am signed in as {role}\"".into(),
            params: vec!["role".into()],
            file: "steps.pv".into(),
            line: 1,
            steps: vec![],
        };
        resolve(&mut s, std::slice::from_ref(&def)).unwrap();
        assert!(matches!(&s.before_each[0].action, Action::Command(w) if w == &["goto", "/cart"]));
        let steps = &s.scenarios[0].steps;
        assert!(matches!(&steps[0].action, Action::Command(w) if w == &["fill", "--label", "Coupon code", "SAVE10"]));
        let text = |a: &Action| match a {
            Action::Expect(Expect { subject: Subject::Text { text, .. }, negate, .. }) => Some((text.clone(), *negate)),
            _ => None,
        };
        assert_eq!(text(&steps[2].action), Some(("Discount -$4.00".to_string(), false)));
        assert_eq!(text(&steps[3].action), Some(("Invalid code".to_string(), true)));
        assert!(matches!(&steps[4].action, Action::Expect(e) if e.value == "/cart"));
        let outline = &s.scenarios[1].steps;
        assert!(matches!(&outline[0].action, Action::Run { flow, args } if flow == &def.name && args == &["shopper"]));
        assert!(
            matches!(&outline[1].action, Action::Command(w) if w == &["select", "--label", "Country", "--value", "France"])
        );
        assert!(s.flows.contains_key(&def.name));
        // A step nothing matches says how to write one.
        let mut bad = parse(Path::new("c.feature"), "Feature: x\n  Scenario: y\n    When I dance\n").unwrap();
        let e = resolve(&mut bad, &[]).unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("`step \"I dance\"`"), "{e}");
    }

    #[test]
    fn patterns_take_quoted_or_bare_values() {
        let (re, names) = step_pattern("I sign in as {email} with {password}").unwrap();
        assert_eq!(names, ["email", "password"]);
        let c = re.captures("I sign in as \"ada@x.io\" with hunter2").unwrap();
        assert_eq!((c.get(1).unwrap().as_str(), c.get(6).unwrap().as_str()), ("ada@x.io", "hunter2"));
        assert!(step_pattern("a {x} b {x}").is_err());
        assert!(!re.is_match("I sign in as ada"));
    }

    #[test]
    fn bad_features_say_where() {
        let e = parse(Path::new("x.feature"), "Scenario: no feature\n").unwrap_err();
        assert!(e.message.contains("starts with `Feature"), "{e}");
        let e = parse(Path::new("x.feature"), "Feature: f\n  Scenario: s\n    Given x\n      | a |\n").unwrap_err();
        assert_eq!(e.line, 4);
        let e = parse(Path::new("x.feature"), "Feature: f\n  Scenario Outline: o\n    Given <a>\n").unwrap_err();
        assert!(e.message.contains("Examples"), "{e}");
        let e = parse(Path::new("x.feature"), "Feature: f\n  Scenario: s\n    Given x\n    stray words\n").unwrap_err();
        assert!(e.message.contains("not a step"), "{e}");
    }
}
