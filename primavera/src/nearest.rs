//! When a step's element isn't there: what on the page comes closest, as
//! locators the step could use instead. Ranked from navigera's `ax` tree by
//! how alike the names are (letter pairs in common) and whether the role
//! is the one asked for. No model involved.

use serde_json::Value;

/// What a step looked for, from its target flags.
#[derive(Debug, Default, PartialEq)]
pub struct Wanted {
    pub role: Option<String>,
    pub name: Option<String>,
}

/// The `--role/--name/--text/--label/--placeholder/--testid` a step gave.
pub fn wanted(words: &[String]) -> Option<Wanted> {
    let mut w = Wanted::default();
    let mut i = 0;
    while i + 1 < words.len() {
        let v = words[i + 1].clone();
        match words[i].as_str() {
            "--role" => w.role = Some(v.to_lowercase()),
            "--name" | "--text" | "--label" | "--placeholder" | "--testid" if w.name.is_none() => w.name = Some(v),
            _ => {}
        }
        i += 1;
    }
    (w.role.is_some() || w.name.is_some()).then_some(w)
}

const NAMED_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "tab",
    "menuitem",
    "switch",
    "slider",
    "spinbutton",
    "heading",
];

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Dice's coefficient on letter pairs: 1 for the same text, 0 for nothing
/// in common.
fn alike(a: &str, b: &str) -> f64 {
    let (a, b) = (norm(a), norm(b));
    if a == b {
        return 1.0;
    }
    let pairs = |s: &str| -> Vec<(char, char)> {
        let c: Vec<char> = s.chars().collect();
        c.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (pa, mut pb) = (pairs(&a), pairs(&b));
    if pa.is_empty() || pb.is_empty() {
        return 0.0;
    }
    let total = pa.len() + pb.len();
    let mut common = 0;
    for p in &pa {
        if let Some(k) = pb.iter().position(|q| q == p) {
            pb.swap_remove(k);
            common += 1;
        }
    }
    let score = 2.0 * common as f64 / total as f64;
    // One contained in the other is closer than shared letters suggest.
    if a.contains(&b) || b.contains(&a) {
        (score + 0.25).min(0.99)
    } else {
        score
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$"))
}

/// Up to `n` candidates: (`role "name"`, a locator for it, score).
pub fn rank(ax: &Value, want: &Wanted, n: usize) -> Vec<(String, String, f64)> {
    let mut out: Vec<(String, String, f64)> = Vec::new();
    for node in ax.as_array().into_iter().flatten() {
        let role = node.get("role").and_then(Value::as_str).unwrap_or("");
        let name = node.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() || name.len() > 80 || role == "RootWebArea" {
            continue;
        }
        let named = NAMED_ROLES.contains(&role);
        if !named && role != "StaticText" {
            continue;
        }
        let mut score = match &want.name {
            Some(w) => alike(w, name),
            None => 0.5,
        };
        match &want.role {
            Some(r) if r == role => score += 0.15,
            Some(_) => score -= 0.1,
            None => {}
        }
        if score < 0.35 {
            continue;
        }
        let locator =
            if named { format!("--role {role} --name {}", quote(name)) } else { format!("--text {}", quote(name)) };
        if out.iter().any(|(_, l, _)| *l == locator) {
            continue;
        }
        out.push((format!("{role} {}", quote(name)), locator, score.min(1.0)));
    }
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_names_rank_first() {
        let ax = serde_json::json!([
            {"role": "RootWebArea", "name": "Cart"},
            {"role": "button", "name": "Apply code"},
            {"role": "link", "name": "Coupons help"},
            {"role": "button", "name": "Proceed to checkout"},
            {"role": "StaticText", "name": "Your cart is empty."},
        ]);
        let want = wanted(&["--role".into(), "button".into(), "--name".into(), "Apply coupon".into()]).unwrap();
        let r = rank(&ax, &want, 3);
        assert_eq!(r[0].1, "--role button --name \"Apply code\"");
        assert!(r.iter().all(|c| !c.0.contains("RootWebArea")));
        let want = wanted(&["--text".into(), "Continue-shoping".into()]).unwrap();
        let ax =
            serde_json::json!([{"role": "link", "name": "Continue shopping"}, {"role": "StaticText", "name": "0"}]);
        assert_eq!(rank(&ax, &want, 3)[0].1, "--role link --name \"Continue shopping\"");
        assert_eq!(wanted(&["--selector".into(), "#x".into()]), None);
        assert_eq!(quote("Pay $5 \"now\""), "\"Pay \\$5 \\\"now\\\"\"");
    }
}
