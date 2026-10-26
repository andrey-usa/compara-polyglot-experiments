//! `expect ax [<target>] matches` and the tree indented under it: the page's
//! (or the target's) accessibility tree, as navigera's `ax` prints it, must
//! hold the template's lines, partially and in order.
//!
//! ```text
//! expect ax --role form --name Checkout matches
//!   - textbox "Email" value=/.+@example\.com/
//!   - combobox "Country" value="Canada"
//!   - button "Place order"
//! ```
//!
//! - A template line matches a tree line whose words it holds in the same
//!   order (`button` matches `button "Place order"`; words of the tree line
//!   may sit between and after them). Refs (`[ref=N]`) are ignored on both.
//! - `/regex/` in place of a name, or after `key=`, matches the quoted or
//!   bare text there (a value copied from the tree, like `url=/cart/`, also
//!   matches as written).
//! - A line indented under another matches somewhere inside the subtree of
//!   the line that one matched; a line after a sibling matches after that
//!   sibling's subtree.

use std::collections::HashMap;

use regex::Regex;

pub struct Node {
    /// The template line as written (trimmed), for messages.
    text: String,
    re: Regex,
    /// The line's first word (its role), to point at a near miss.
    role: String,
    kids: Vec<Node>,
    id: usize,
}

/// The template's lines (indented as written; blank and `#` lines skipped)
/// as a tree of patterns.
pub fn compile(lines: &[String]) -> Result<Vec<Node>, String> {
    let mut flat = Vec::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        flat.push((line.len() - line.trim_start().len(), t.to_string()));
    }
    if flat.is_empty() {
        return Err("`expect ax … matches` needs the tree it expects, indented under it".into());
    }
    let mut next_id = 0;
    let mut i = 0;
    let base = flat[0].0;
    let nodes = level(&flat, &mut i, base, &mut next_id)?;
    match flat.get(i) {
        Some((_, t)) => Err(format!("{t:?} is indented less than the tree's first line")),
        None => Ok(nodes),
    }
}

fn level(flat: &[(usize, String)], i: &mut usize, indent: usize, next_id: &mut usize) -> Result<Vec<Node>, String> {
    let mut out: Vec<Node> = Vec::new();
    while *i < flat.len() {
        let (ind, text) = &flat[*i];
        if *ind < indent {
            break;
        }
        if *ind > indent {
            let parent = out.last_mut().ok_or_else(|| format!("{text:?} is indented more than the line before it"))?;
            parent.kids = level(flat, i, *ind, next_id)?;
            continue;
        }
        *i += 1;
        let content = strip_refs(text.strip_prefix("- ").unwrap_or(text));
        let re = pattern(&content).map_err(|e| format!("in {text:?}: {e}"))?;
        let role = content.split([' ', ':']).next().unwrap_or("").to_string();
        out.push(Node { text: text.clone(), re, role, kids: Vec::new(), id: *next_id });
        *next_id += 1;
    }
    Ok(out)
}

fn strip_refs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("[ref=") {
        let Some(end) = rest[i..].find(']') else { break };
        out.push_str(rest[..i].trim_end_matches(' '));
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Where a `/regex/` that starts at `from` (its opening `/`) ends: the
/// index just past an unescaped `/` followed by a space or the end.
fn regex_end(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '/' if i > from + 1 && chars.get(i + 1).is_none_or(|c| c.is_whitespace()) => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Words split on spaces outside double quotes and `/regex/`s; a quoted
/// word keeps its quotes (and `\"` inside it).
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let all: Vec<char> = line.chars().collect();
    let mut at = 0;
    while at < all.len() {
        let c = all[at];
        at += 1;
        // A regex as a word or a value (`/a b/`, `value=/x y/`) keeps its spaces.
        if c == '/' && !quoted && (cur.is_empty() || cur.ends_with('=')) {
            if let Some(end) = regex_end(&all, at - 1) {
                cur.extend(&all[at - 1..end]);
                at = end;
                continue;
            }
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                if let Some(&n) = all.get(at) {
                    cur.push(n);
                    at += 1;
                }
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `/re/` (at least one character between the slashes).
fn regex_of(word: &str) -> Option<&str> {
    word.strip_prefix('/').and_then(|w| w.strip_suffix('/')).filter(|w| !w.is_empty())
}

fn word_pattern(word: &str) -> Result<String, String> {
    let literal = regex::escape(word);
    let (key, value) = match word.find('=') {
        Some(eq) if regex_of(&word[eq + 1..]).is_some() => (&word[..=eq], &word[eq + 1..]),
        _ => ("", word),
    };
    Ok(match regex_of(value) {
        Some(re) => {
            Regex::new(re).map_err(|e| format!("bad regular expression /{re}/: {e}"))?;
            format!("(?:{literal}|{}(?:\"(?:{re})\"|(?:{re})))", regex::escape(key))
        }
        None => literal,
    })
}

fn pattern(content: &str) -> Result<Regex, String> {
    let ws = words(content);
    let mut re = String::from("^");
    for (k, w) in ws.iter().enumerate() {
        if k > 0 {
            // Words of the tree line may sit between the template's.
            re.push_str(r"(?:\s+|\s.*?\s)");
        }
        re.push_str(&word_pattern(w)?);
    }
    re.push_str(r"(?:$|[\s:,|])");
    Regex::new(&re).map_err(|e| e.to_string())
}

/// The tree's node lines (`- …`): indentation and text without refs.
fn tree_lines(page: &str) -> Vec<(usize, String)> {
    page.lines()
        .filter_map(|l| {
            let t = l.trim_start();
            let content = t.strip_prefix("- ")?;
            Some((l.len() - t.len(), strip_refs(content)))
        })
        .collect()
}

fn subtree_end(tree: &[(usize, String)], i: usize) -> usize {
    let indent = tree[i].0;
    (i + 1..tree.len()).find(|&j| tree[j].0 <= indent).unwrap_or(tree.len())
}

struct Matcher<'a> {
    tree: &'a [(usize, String)],
    memo: HashMap<(usize, usize, usize), bool>,
}

impl Matcher<'_> {
    /// `nodes`, in order, inside `tree[from..to]`, each after the subtree
    /// of the one before it.
    fn siblings(&mut self, nodes: &[Node], from: usize, to: usize) -> bool {
        let Some((first, rest)) = nodes.split_first() else { return true };
        let key = (first.id, from, to);
        if let Some(&known) = self.memo.get(&key) {
            return known;
        }
        let mut found = false;
        for i in from..to {
            if !first.re.is_match(&self.tree[i].1) {
                continue;
            }
            let end = subtree_end(self.tree, i);
            if self.siblings(&first.kids, i + 1, end) && self.siblings(rest, end, to) {
                found = true;
                break;
            }
        }
        self.memo.insert(key, found);
        found
    }
}

/// Whether `page` (navigera `ax` text) holds `template`; else which line
/// of the template could not be placed, and the nearest line of its role.
pub fn matches(template: &[Node], page: &str) -> Result<(), String> {
    let tree = tree_lines(page);
    let mut m = Matcher { tree: &tree, memo: HashMap::new() };
    if m.siblings(template, 0, tree.len()) {
        return Ok(());
    }
    Err(miss(template, &tree, 0, tree.len(), None))
}

/// The first template line a greedy walk can't place, said with where it
/// was looked for and what came closest.
fn miss(nodes: &[Node], tree: &[(usize, String)], mut from: usize, to: usize, parent: Option<&str>) -> String {
    for node in nodes {
        match (from..to).find(|&i| node.re.is_match(&tree[i].1)) {
            Some(i) => {
                let end = subtree_end(tree, i);
                let mut m = Matcher { tree, memo: HashMap::new() };
                if !m.siblings(&node.kids, i + 1, end) {
                    return miss(&node.kids, tree, i + 1, end, Some(&tree[i].1));
                }
                from = end;
            }
            None => {
                let mut msg = format!("no line matches `{}`", node.text);
                if let Some(p) = parent {
                    msg.push_str(&format!(" under `- {}`", clip(p)));
                }
                if let Some(near) =
                    (from..to).map(|i| &tree[i].1).find(|l| !node.role.is_empty() && l.starts_with(&node.role))
                {
                    msg.push_str(&format!("; nearest: `- {}`", clip(near)));
                } else if tree.is_empty() {
                    msg.push_str("; the tree is empty");
                }
                return msg;
            }
        }
    }
    // Each line has a place on its own, but not all of them in this order.
    "the lines are there, but not in this order or nesting".into()
}

fn clip(s: &str) -> String {
    if s.chars().count() > 150 {
        format!("{}…", s.chars().take(150).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(text: &str) -> Vec<Node> {
        compile(&text.lines().map(String::from).collect::<Vec<_>>()).unwrap()
    }

    const PAGE: &str = "page \"Checkout\" http://shop.test/checkout
- navigation:
  - link \"Cart\" [ref=3] url=/cart/
- main:
  - heading \"Checkout\" [level=1]
  - form \"Checkout\":
    - textbox \"Email\" [ref=9] [required] value=\"ada@example.com\"
    - combobox \"Country\" [ref=11] value=\"Canada\"
    - button \"Place order\" [ref=14]
  - table:
    - row: \"Brass Sprocket\" | spinbutton \"Qty\" [ref=20] value=\"2\" | \"$7.00\"";

    #[test]
    fn partial_in_order_nested_and_refs_ignored() {
        let t = tpl("- form \"Checkout\"\n  - textbox \"Email\" value=/.+@example\\.com/\n  - button \"Place order\"");
        assert_eq!(matches(&t, PAGE), Ok(()));
        // A role alone; words between; a ref in the template.
        assert_eq!(matches(&tpl("- main\n  - combobox value=\"Canada\"\n  - button [ref=1]"), PAGE), Ok(()));
        assert_eq!(matches(&tpl("- row: \"Brass Sprocket\" | spinbutton \"Qty\" value=\"2\""), PAGE), Ok(()));
        // A url copied from the tree is literal, though it looks like /re/.
        assert_eq!(matches(&tpl("- link \"Cart\" url=/cart/"), PAGE), Ok(()));
        assert_eq!(matches(&tpl("- heading /Check.*/ [level=1]"), PAGE), Ok(()));
        // A regex with spaces is one word (T-0057).
        assert_eq!(matches(&tpl("- button /Place or.er/"), PAGE), Ok(()));
        assert_eq!(matches(&tpl("- button /Place \\w+/"), PAGE), Ok(()));
        assert_eq!(matches(&tpl("- textbox value=/ada@example\\.com/ [ref=1]"), PAGE), Ok(()));
        assert_eq!(words("- row: /a b/ | x"), ["-", "row:", "/a b/", "|", "x"]);
        // A path that isn't a regex still splits.
        assert_eq!(words("link url=/cart value=\"x\""), ["link", "url=/cart", "value=\"x\""]);
    }

    #[test]
    fn misses_say_which_line_and_what_came_closest() {
        // Out of order.
        let e = matches(&tpl("- button \"Place order\"\n- textbox \"Email\""), PAGE).unwrap_err();
        assert!(e.contains("`- textbox \"Email\"`"), "{e}");
        // Not under its parent: the navigation holds no button.
        let e = matches(&tpl("- navigation\n  - button \"Place order\""), PAGE).unwrap_err();
        assert!(e.contains("under `- navigation:`"), "{e}");
        // A wrong value names the near line.
        let e = matches(&tpl("- form\n  - combobox \"Country\" value=\"Mexico\""), PAGE).unwrap_err();
        assert!(e.contains("nearest: `- combobox \"Country\" value=\"Canada\"`"), "{e}");
        // `button` doesn't match `buttons`, a whole word only.
        assert!(matches(&tpl("- butto"), PAGE).is_err());
    }

    #[test]
    fn bad_templates_are_refused() {
        assert!(compile(&["- button /(/".to_string()]).is_err());
        assert!(compile(&["  - a".to_string(), "- b".to_string()]).is_err());
        assert!(compile(&[String::new()]).is_err());
    }

    #[test]
    fn many_rows_dont_blow_up() {
        let mut page = String::from("- table:\n");
        for k in 0..300 {
            page.push_str(&format!("  - row: \"r{k}\"\n"));
        }
        let t = tpl(&format!("{}- button \"Next\"", "- row\n".repeat(6)));
        let started = std::time::Instant::now();
        assert!(matches(&t, &page).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
