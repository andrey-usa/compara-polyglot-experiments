//! Accessibility snapshot rendering: Chrome's `Accessibility.getFullAXTree`
//! nodes in, an indented text tree for agents out.
//!
//! ```text
//! page "Acme — Cart" http://127.0.0.1:8000/cart
//! - banner:
//!   - link "Acme Supply" [ref=12] url=/
//!   - navigation "Main":
//!     - link "Products" [ref=15] url=/products
//! - heading "Your cart" [level=1]
//! - list:
//!   - listitem:
//!     - text: Brass Sprocket × 2
//!     - button "Remove" [ref=48]
//! - combobox "Country" [ref=51] value="Canada" options: "United States", "Canada", "Mexico"
//! ```
//!
//! Why a tree and not a flat list: in a product grid every "Add to cart"
//! button has the same name; only the nesting says which product it belongs
//! to. Wrapper nodes (`generic`, `none`, label boxes) are collapsed into
//! their children, text that repeats an ancestor's name is dropped, and refs
//! (`backendNodeId`s, usable with `click`/`fill`/`select`/… `--ref`) are
//! only printed on nodes an agent can act on, so a 500-card listing stays a
//! fraction of the raw tree's size.

use std::collections::HashMap;

use serde_json::Value;

/// Roles an agent can act on; always get a ref.
pub const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "switch",
    "slider",
    "spinbutton",
    "treeitem",
    "DisclosureTriangle",
];

/// Roles that only wrap other nodes: collapsed (children promoted) unless
/// they are focusable or carry a name of their own.
const WRAPPERS: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "LabelText",
    "LayoutTable",
    "LayoutTableRow",
    "LayoutTableCell",
    "Section",
    "Div",
    "Pre",
];

/// Roles never printed (their text is carried by the parent).
const SILENT: &[&str] = &["InlineTextBox", "LineBreak"];

/// Roles whose accessible name may come from their content (ARIA "name
/// from content", plus Chrome's text and disclosure roles). Any other role
/// is named by its author (aria-label, a caption, a legend) or not at all.
const NAME_FROM_CONTENT: &[&str] = &[
    "button",
    "cell",
    "checkbox",
    "columnheader",
    "gridcell",
    "heading",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "row",
    "rowheader",
    "sectionhead",
    "switch",
    "tab",
    "tooltip",
    "treeitem",
    "term",
    "StaticText",
    "LabelText",
    "DisclosureTriangle",
];

/// Rendering options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Print a ref on every node with a DOM node, not just actionable ones.
    pub all_refs: bool,
    /// Stop after this many lines (0 = unlimited).
    pub limit: usize,
    /// Render only the subtree of the node with this backend DOM node id.
    pub root_backend_id: Option<u64>,
    /// Page origin (`http://host:port`) for shortening same-origin link urls.
    pub origin: String,
    /// What the page makes clickable beyond the tree's actionable roles.
    pub clicks: Clickables,
}

/// Elements a user could click though the tree gives them no actionable
/// role (a `<div>` row with a click handler, an icon `<span>`), by backend
/// node id; and `#id` / `.class` per element, printed for actionable nodes
/// with no name or text, so `[ref=7]` and `[ref=8]` aren't two blanks.
#[derive(Clone, Debug, Default)]
pub struct Clickables {
    pub nodes: std::collections::HashSet<u64>,
    pub hints: HashMap<u64, String>,
}

/// One frame's AX nodes, spliced under the AX node of its owner element
/// (`<iframe>` backend node id).
pub struct Frame {
    pub owner_backend_id: u64,
    pub nodes: Vec<Value>,
}

pub struct Rendered {
    pub text: String,
    /// Lines emitted before truncation.
    pub lines: usize,
    /// Lines dropped by `limit`.
    pub truncated: usize,
}

fn field<'a>(node: &'a Value, key: &str) -> &'a Value {
    node.get(key).and_then(|v| v.get("value")).unwrap_or(&Value::Null)
}

fn text_of(node: &Value, key: &str) -> String {
    match field(node, key) {
        Value::String(s) => squash(s),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Collapse runs of whitespace (AX names keep the page's newlines/indent).
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

pub(crate) fn prop<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    node.get("properties")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|p| p.get("value"))
        .and_then(|v| v.get("value"))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

struct Tree<'a> {
    by_id: HashMap<&'a str, &'a Value>,
    frame_root_ids: std::collections::HashSet<&'a str>,
    frames: HashMap<u64, Vec<&'a Value>>,
    opts: &'a Options,
    lines: Vec<String>,
}

impl<'a> Tree<'a> {
    fn children(&self, node: &'a Value) -> Vec<&'a Value> {
        let mut out: Vec<&'a Value> = node
            .get("childIds")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(|id| id.as_str().and_then(|id| self.by_id.get(id).copied())).collect())
            .unwrap_or_default();
        // An <iframe>'s document lives in another frame's tree.
        if let Some(backend) = node.get("backendDOMNodeId").and_then(Value::as_u64) {
            if let Some(roots) = self.frames.get(&backend) {
                out.extend(roots.iter().copied());
            }
        }
        out
    }

    fn role(node: &Value) -> &str {
        field(node, "role").as_str().unwrap_or("")
    }

    /// The node's name as the agent should read it. Some engines (Lightpanda)
    /// name a table, list or `<b>` with all of its text: that is content,
    /// not a label, and as a name it would hide the very text it repeats.
    fn name(&self, node: &Value) -> String {
        let name = text_of(node, "name");
        let role = Self::role(node);
        if name.is_empty() || NAME_FROM_CONTENT.contains(&role) || INTERACTIVE.contains(&role) {
            return name;
        }
        let mut text = Vec::new();
        self.descendant_text(node, &mut text, 0);
        if !text.is_empty() && squash(&text.join(" ")) == name {
            return String::new();
        }
        name
    }

    fn descendant_text(&self, node: &'a Value, out: &mut Vec<String>, depth: usize) {
        if depth > 64 {
            return;
        }
        for child in self.children(node) {
            if Self::role(child) == "StaticText" {
                let t = text_of(child, "name");
                if !t.is_empty() {
                    out.push(t);
                }
            } else {
                self.descendant_text(child, out, depth + 1);
            }
        }
    }

    /// A wrapper the page made clickable, that holds nothing else to act on
    /// (a listener on a container is the page listening for its children).
    fn is_clickable(&self, node: &'a Value) -> bool {
        let Some(id) = node.get("backendDOMNodeId").and_then(Value::as_u64) else { return false };
        self.opts.clicks.nodes.contains(&id)
            && !INTERACTIVE.contains(&Self::role(node))
            && Self::role(node) != "LabelText"
            && !self.holds_actionable(node, 0)
    }

    fn holds_actionable(&self, node: &'a Value, depth: usize) -> bool {
        depth < 64
            && self.children(node).into_iter().any(|child| {
                let role = Self::role(child);
                INTERACTIVE.contains(&role)
                    || child
                        .get("backendDOMNodeId")
                        .and_then(Value::as_u64)
                        .is_some_and(|id| self.opts.clicks.nodes.contains(&id))
                    || self.holds_actionable(child, depth + 1)
            })
    }

    fn is_transparent(&self, node: &'a Value) -> bool {
        if self.is_clickable(node) {
            return false;
        }
        if node.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
            return true;
        }
        let role = Self::role(node);
        if role == "RootWebArea" && !self.is_frame_root(node) {
            return true;
        }
        WRAPPERS.contains(&role) && self.name(node).is_empty() && !truthy(prop(node, "focusable"))
    }

    fn is_frame_root(&self, node: &Value) -> bool {
        // Child-frame documents are rendered as `- document "Title":`.
        node.get("nodeId").and_then(Value::as_str).is_some_and(|id| self.frame_root_ids.contains(id))
    }

    /// Children with transparent wrappers expanded in place.
    fn effective_children(&self, node: &'a Value, out: &mut Vec<&'a Value>) {
        for child in self.children(node) {
            if SILENT.contains(&Self::role(child)) {
                continue;
            }
            if self.is_transparent(child) {
                self.effective_children(child, out);
            } else {
                out.push(child);
            }
        }
    }

    fn wants_ref(&self, node: &'a Value, role: &str) -> bool {
        self.opts.all_refs || INTERACTIVE.contains(&role) || truthy(prop(node, "focusable")) || self.is_clickable(node)
    }

    fn states(node: &Value, role: &str) -> Vec<String> {
        let mut out = Vec::new();
        match prop(node, "checked") {
            Some(Value::String(s)) if s == "true" => out.push("checked".into()),
            Some(Value::String(s)) if s == "mixed" => out.push("checked=mixed".into()),
            Some(Value::Bool(true)) => out.push("checked".into()),
            _ => {}
        }
        for (name, label) in [
            ("pressed", "pressed"),
            ("disabled", "disabled"),
            ("required", "required"),
            ("readonly", "readonly"),
            ("modal", "modal"),
        ] {
            if truthy(prop(node, name)) {
                out.push(label.into());
            }
        }
        if role != "option" && truthy(prop(node, "selected")) {
            out.push("selected".into());
        }
        match prop(node, "expanded") {
            Some(v) if truthy(Some(v)) => out.push("expanded".into()),
            Some(_) if role != "combobox" => out.push("collapsed".into()),
            _ => {}
        }
        if let Some(Value::String(s)) = prop(node, "invalid") {
            if s != "false" {
                out.push("invalid".into());
            }
        }
        if let Some(level) = prop(node, "level").and_then(Value::as_u64) {
            out.push(format!("level={level}"));
        }
        out
    }

    fn short_url(&self, url: &str) -> String {
        if !self.opts.origin.is_empty() {
            if let Some(rest) = url.strip_prefix(&self.opts.origin) {
                if rest.starts_with('/') {
                    return rest.to_string();
                }
            }
        }
        url.to_string()
    }

    fn walk(&mut self, node: &'a Value, depth: usize, covered: &str) {
        let role = Self::role(node).to_string();
        if SILENT.contains(&role.as_str()) {
            return;
        }
        if self.is_transparent(node) {
            for child in self.children(node) {
                self.walk(child, depth, covered);
            }
            return;
        }
        let indent = "  ".repeat(depth);
        let name = self.name(node);

        if role == "StaticText" {
            if name.is_empty() || (!covered.is_empty() && covered.contains(&name)) {
                return;
            }
            self.lines.push(format!("{indent}- text: {}", clip(&name, 300)));
            return;
        }

        // A <select>'s options go on its line. Lightpanda names a select
        // inside a <label> with the label's whole text, options included
        // ("Currency USD EUR GBP …"): those words come off the name.
        let mut options = Vec::new();
        if role == "combobox" || role == "listbox" {
            collect_options(self, node, &mut options);
        }
        let name = without_options(name, &options);

        let clickable = self.is_clickable(node);
        let shown_role = if self.is_frame_root(node) {
            "document"
        } else if clickable && (WRAPPERS.contains(&role.as_str()) || node.get("ignored") == Some(&Value::Bool(true))) {
            "clickable"
        } else {
            role.as_str()
        };
        let mut line = format!("{indent}- {shown_role}");
        if !name.is_empty() {
            line.push(' ');
            line.push_str(&quote(&clip(&name, 150)));
        }
        let mut tags = Self::states(node, &role);
        if let Some(id) = node.get("backendDOMNodeId").and_then(Value::as_u64) {
            if self.wants_ref(node, &role) {
                // Nothing to read it by (an icon button, an empty `<span>`
                // with a handler): say which element it is.
                if name.is_empty() && text_of(node, "value").is_empty() {
                    let mut text = Vec::new();
                    self.descendant_text(node, &mut text, 0);
                    if let (true, Some(hint)) =
                        (text.iter().all(|t| t.trim().is_empty()), self.opts.clicks.hints.get(&id))
                    {
                        line.push(' ');
                        line.push_str(hint);
                    }
                }
                tags.push(format!("ref={id}"));
            }
        }
        if !tags.is_empty() {
            line.push_str(&format!(" [{}]", tags.join(", ")));
        }
        // A checkbox's or radio's state is [checked]; its value attribute
        // ("on" unless set, which Lightpanda reports) reads like a state.
        let value = if CHECKABLE.contains(&role.as_str()) { String::new() } else { text_of(node, "value") };
        if !value.is_empty() && value != name {
            line.push_str(&format!(" value={}", quote(&clip(&value, 150))));
        }
        if role == "link" {
            if let Some(Value::String(url)) = prop(node, "url") {
                if !url.is_empty() && !url.starts_with("javascript:") {
                    line.push_str(&format!(" url={}", self.short_url(url)));
                }
            }
        }

        let mut kids = Vec::new();
        self.effective_children(node, &mut kids);

        // <select>: list the options on the combobox line, not as a subtree.
        if (role == "combobox" || role == "listbox") && !options.is_empty() {
            let shown: Vec<String> = options.iter().take(25).map(|o| quote(&clip(o, 60))).collect();
            line.push_str(&format!(" options: {}", shown.join(", ")));
            if options.len() > 25 {
                line.push_str(&format!(", …(+{})", options.len() - 25));
            }
            self.lines.push(line);
            return;
        }

        // Unnamed node holding only text: inline it (`- paragraph: …`).
        let child_cover = if name.is_empty() { covered.to_string() } else { name.clone() };
        if !kids.is_empty() && kids.iter().all(|k| Self::role(k) == "StaticText") {
            let text: Vec<String> = kids
                .iter()
                .map(|k| text_of(k, "name"))
                .filter(|t| !t.is_empty() && !child_cover.contains(t.as_str()) && *t != value)
                .collect();
            if !text.is_empty() {
                line.push_str(&format!(": {}", clip(&text.join(" "), 300)));
            }
            self.lines.push(line);
            return;
        }
        // A named wrapper around one same-named actionable child (a heading
        // holding its link): one line, name printed once.
        if kids.len() == 1 && !name.is_empty() && !INTERACTIVE.contains(&role.as_str()) {
            let only = kids[0];
            let only_role = Self::role(only).to_string();
            if text_of(only, "name") == name && INTERACTIVE.contains(&only_role.as_str()) {
                let mut grand = Vec::new();
                self.effective_children(only, &mut grand);
                let leafish = grand.iter().all(|k| Self::role(k) == "StaticText");
                if leafish {
                    let inner = self.describe(only, &only_role);
                    line.push_str(&format!(": {inner}"));
                    self.lines.push(line);
                    return;
                }
            }
        }
        // An unnamed table cell or list item around one control: one line
        // (Lightpanda leaves cells unnamed, so each checkbox in a table
        // took two lines where Chrome's tree takes one).
        let controls: Vec<&Value> = kids.iter().copied().filter(|k| Self::role(k) != "StaticText").collect();
        let echoes_only = controls.len() == 1 && {
            let control_name = self.name(controls[0]);
            kids.iter()
                .filter(|k| Self::role(k) == "StaticText")
                .all(|k| text_of(k, "name").trim().trim_end_matches(':') == control_name.trim())
        };
        if echoes_only && name.is_empty() && matches!(role.as_str(), "cell" | "gridcell" | "listitem") {
            let only = controls[0];
            let only_role = Self::role(only).to_string();
            let mut grand = Vec::new();
            self.effective_children(only, &mut grand);
            if INTERACTIVE.contains(&only_role.as_str()) && grand.iter().all(|k| Self::role(k) == "StaticText") {
                let only_name = self.name(only);
                let mut inner = self.describe(only, &only_role);
                if !only_name.is_empty() {
                    inner.insert_str(only_role.len(), &format!(" {}", quote(&clip(&only_name, 150))));
                }
                line.push_str(&format!(": {inner}"));
                self.lines.push(line);
                return;
            }
        }
        let has_ref = line.contains("ref=");
        if kids.is_empty()
            && name.is_empty()
            && !has_ref
            && value.is_empty()
            && !INTERACTIVE.contains(&role.as_str())
            && role != "Iframe"
        {
            return; // an empty live region / paragraph says nothing
        }
        let at = self.lines.len();
        self.lines.push(line);
        self.walk_kids(kids, depth + 1, &child_cover);
        if self.lines.len() > at + 1 {
            self.lines[at].push(':');
        } else if name.is_empty() && !has_ref && value.is_empty() && !INTERACTIVE.contains(&role.as_str()) {
            self.lines.truncate(at); // all its children were redundant
        }
    }

    /// Walk sibling nodes, dropping label text that sits next to the control
    /// it names (`<label>Email <input>`): it is already the control's name.
    fn walk_kids(&mut self, kids: Vec<&'a Value>, depth: usize, cover: &str) {
        let bare = |t: &str| t.trim_end_matches([':', '*', ' ']).to_string();
        let sibling_names: Vec<String> = kids
            .iter()
            .filter(|k| Self::role(k) != "StaticText")
            .map(|k| {
                let mut options = Vec::new();
                if matches!(Self::role(k), "combobox" | "listbox") {
                    collect_options(self, k, &mut options);
                }
                bare(&without_options(text_of(k, "name"), &options))
            })
            .filter(|n| !n.is_empty())
            .collect();
        for child in kids {
            if Self::role(child) == "StaticText" {
                let t = bare(&text_of(child, "name"));
                if !t.is_empty() && sibling_names.contains(&t) {
                    continue;
                }
            }
            self.walk(child, depth, cover);
        }
    }

    /// `role [tags] url=…` for a child inlined after its same-named parent.
    fn describe(&self, node: &Value, role: &str) -> String {
        let mut line = role.to_string();
        let mut tags = Self::states(node, role);
        if let Some(id) = node.get("backendDOMNodeId").and_then(Value::as_u64) {
            if self.wants_ref(node, role) {
                tags.push(format!("ref={id}"));
            }
        }
        if !tags.is_empty() {
            line.push_str(&format!(" [{}]", tags.join(", ")));
        }
        if role == "link" {
            if let Some(Value::String(url)) = prop(node, "url") {
                if !url.is_empty() && !url.starts_with("javascript:") {
                    line.push_str(&format!(" url={}", self.short_url(url)));
                }
            }
        }
        line
    }
}

/// `name` without the option texts a `<select>`'s name can end with:
/// Lightpanda names a select inside a `<label>` with all of the label's
/// text, options included ("Currency USD EUR GBP …").
fn without_options(name: String, options: &[String]) -> String {
    let words: Vec<&str> = options.iter().map(|o| o.strip_suffix(" (selected)").unwrap_or(o).trim()).collect();
    let tail = words.join(" ");
    match name.strip_suffix(tail.as_str()) {
        Some(head) if !tail.is_empty() => head.trim_end().to_string(),
        _ => name,
    }
}

/// Roles whose state is checked or not, shown as `[checked]`.
const CHECKABLE: &[&str] = &["checkbox", "radio", "switch", "menuitemcheckbox", "menuitemradio"];

fn collect_options(tree: &Tree<'_>, node: &Value, out: &mut Vec<String>) {
    for child in tree.children(node) {
        if Tree::role(child) == "option" {
            let mut label = text_of(child, "name");
            if truthy(prop(child, "selected")) {
                label.push_str(" (selected)");
            }
            out.push(label);
        } else {
            collect_options(tree, child, out);
        }
    }
}

/// Render the main-frame `nodes` (plus child `frames`) as the text tree.
pub fn render(header: &str, nodes: &[Value], frames: &[Frame], opts: &Options) -> Rendered {
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    for n in nodes.iter().chain(frames.iter().flat_map(|f| f.nodes.iter())) {
        if let Some(id) = n.get("nodeId").and_then(Value::as_str) {
            by_id.insert(id, n);
        }
    }
    let mut frame_roots: HashMap<u64, Vec<&Value>> = HashMap::new();
    let mut frame_root_ids = std::collections::HashSet::new();
    for f in frames {
        if let Some(root) = f.nodes.iter().find(|n| n.get("parentId").is_none()) {
            frame_roots.entry(f.owner_backend_id).or_default().push(root);
            if let Some(id) = root.get("nodeId").and_then(Value::as_str) {
                frame_root_ids.insert(id);
            }
        }
    }
    let mut tree = Tree { by_id, frame_root_ids, frames: frame_roots, opts, lines: Vec::new() };
    if !header.is_empty() {
        tree.lines.push(header.to_string());
    }
    let start: Option<&Value> = match opts.root_backend_id {
        Some(backend) => nodes
            .iter()
            .chain(frames.iter().flat_map(|f| f.nodes.iter()))
            .find(|n| n.get("backendDOMNodeId").and_then(Value::as_u64) == Some(backend)),
        None => nodes.iter().find(|n| n.get("parentId").is_none()),
    };
    if let Some(root) = start {
        if tree.is_transparent(root) {
            let mut kids = Vec::new();
            tree.effective_children(root, &mut kids);
            tree.walk_kids(kids, 0, "");
        } else {
            tree.walk(root, 0, "");
        }
    }
    let total = tree.lines.len();
    let mut lines = tree.lines;
    let mut truncated = 0;
    if opts.limit > 0 && total > opts.limit {
        truncated = total - opts.limit;
        lines.truncate(opts.limit);
        lines.push(format!("… {truncated} more lines (scope with `ax --selector <css>`, or raise `--limit`)"));
    }
    Rendered { lines: lines.len(), text: lines.join("\n"), truncated }
}

/// Lines for what [`crate::cdp::page`]'s `SHADOW_AX` found inside shadow
/// roots, appended to a Lightpanda snapshot: one heading per root, then its
/// elements as the tree would show them, but with no refs -- the engine gives
/// those nodes no id -- and the targeting that does reach them said once.
pub fn render_shadow(found: &[Value]) -> String {
    let mut out = String::new();
    let mut host = 0;
    for e in found {
        let h = e.get("host").and_then(Value::as_u64).unwrap_or(0);
        let role = e.get("role").and_then(Value::as_str).unwrap_or("generic");
        let name = e.get("name").and_then(Value::as_str).unwrap_or("");
        if h != host {
            host = h;
            let tag = e.get("tag").and_then(Value::as_str).unwrap_or("element");
            out.push_str(&format!(
                "- shadow root of <{tag}> (Lightpanda leaves it out of the tree and gives it no refs; \
                 target it with --text \"<name>\"):\n"
            ));
        }
        let mut line = format!("  - {role}");
        if !name.is_empty() {
            line.push_str(&format!(" {}", serde_json::to_string(name).unwrap_or_default()));
        }
        if let Some(level) = e.get("level").and_then(Value::as_u64) {
            line.push_str(&format!(" [level={level}]"));
        }
        if let Some(v) = e.get("value").and_then(Value::as_str) {
            line.push_str(&format!(" value={}", serde_json::to_string(v).unwrap_or_default()));
        }
        if let Some(c) = e.get("checked").and_then(Value::as_bool) {
            line.push_str(if c { " [checked]" } else { " [unchecked]" });
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, parent: Option<&str>, role: &str, name: &str, kids: &[&str], backend: u64) -> Value {
        let mut n = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": kids,
            "backendDOMNodeId": backend,
        });
        if let Some(p) = parent {
            n["parentId"] = json!(p);
        }
        n
    }

    #[test]
    fn renders_nested_tree_with_refs_on_actionable_nodes_only() {
        let nodes = vec![
            node("1", None, "RootWebArea", "Shop", &["2"], 1),
            node("2", Some("1"), "generic", "", &["3", "6"], 2),
            node("3", Some("2"), "heading", "Widget", &["4"], 3),
            node("4", Some("3"), "StaticText", "Widget", &[], 4),
            node("6", Some("2"), "button", "Add to cart", &["7"], 6),
            node("7", Some("6"), "StaticText", "Add to cart", &[], 7),
        ];
        let out = render("page \"Shop\"", &nodes, &[], &Options::default());
        assert_eq!(out.text, "page \"Shop\"\n- heading \"Widget\"\n- button \"Add to cart\" [ref=6]");
    }

    #[test]
    fn what_the_page_made_clickable_gets_a_ref_and_a_container_does_not() {
        let nodes = vec![
            node("1", None, "RootWebArea", "Inbox", &["2"], 1),
            // #app listens for its children's clicks: not a control itself.
            node("2", Some("1"), "generic", "", &["3", "6", "7"], 2),
            node("3", Some("2"), "generic", "", &["4", "5"], 3),
            node("4", Some("3"), "StaticText", "Gilli", &[], 4),
            node("5", Some("3"), "StaticText", "Quam.", &[], 5),
            node("6", Some("2"), "generic", "", &[], 6),
            node("7", Some("2"), "button", "", &[], 7),
        ];
        let clicks = Clickables {
            nodes: [2, 3, 6].into_iter().collect(),
            hints: [(2, "#app".to_string()), (6, "#send-forward".to_string()), (7, "#button-1".to_string())]
                .into_iter()
                .collect(),
        };
        let out = render("page \"Inbox\"", &nodes, &[], &Options { clicks, ..Options::default() });
        assert_eq!(
            out.text,
            "page \"Inbox\"\n- clickable [ref=3]: Gilli Quam.\n- clickable #send-forward [ref=6]\n- button #button-1 [ref=7]"
        );
        // Without them, the row is bare text and the icon is not there.
        let out = render("", &nodes, &[], &Options::default());
        assert_eq!(out.text, "- text: Gilli\n- text: Quam.\n- button [ref=7]");
    }

    #[test]
    fn inlines_text_only_children_and_limits_output() {
        let nodes = vec![
            node("1", None, "RootWebArea", "", &["2", "4"], 1),
            node("2", Some("1"), "paragraph", "", &["3"], 2),
            node("3", Some("2"), "StaticText", "Returns accepted for 30 days.", &[], 3),
            node("4", Some("1"), "paragraph", "", &["5"], 4),
            node("5", Some("4"), "StaticText", "Second", &[], 5),
        ];
        let out = render("", &nodes, &[], &Options::default());
        assert_eq!(out.text, "- paragraph: Returns accepted for 30 days.\n- paragraph: Second");
        let out = render("", &nodes, &[], &Options { limit: 1, ..Options::default() });
        assert_eq!(out.truncated, 1);
        assert!(out.text.starts_with("- paragraph: Returns"));
        assert!(out.text.contains("1 more lines"));
    }

    #[test]
    fn splices_iframe_documents_and_lists_select_options() {
        let mut combo = node("4", Some("1"), "combobox", "Color", &["5"], 4);
        combo["value"] = json!({"type": "string", "value": "Blue"});
        let mut blue = node("7", Some("5"), "option", "Blue", &[], 7);
        blue["properties"] = json!([{"name": "selected", "value": {"type": "boolean", "value": true}}]);
        let nodes = vec![
            node("1", None, "RootWebArea", "Outer", &["2", "4"], 1),
            node("2", Some("1"), "Iframe", "Pay", &[], 14),
            combo,
            node("5", Some("4"), "MenuListPopup", "", &["6", "7"], 5),
            node("6", Some("5"), "option", "Red", &[], 6),
            blue,
        ];
        let mut inner = node("70", None, "RootWebArea", "Card form", &["71"], 70);
        inner["frameId"] = json!("F2");
        let frames = vec![Frame {
            owner_backend_id: 14,
            nodes: vec![inner, node("71", Some("70"), "button", "Pay now", &[], 71)],
        }];
        let out = render("", &nodes, &frames, &Options::default());
        assert_eq!(
            out.text,
            "- Iframe \"Pay\":\n  - document \"Card form\":\n    - button \"Pay now\" [ref=71]\n\
             - combobox \"Color\" [ref=4] value=\"Blue\" options: \"Red\", \"Blue (selected)\""
        );
    }

    /// What Lightpanda's tree says about a form, rendered as Chrome's reads:
    /// a select named with its label's whole text (options included) and
    /// the label text beside it, a checkbox with the default value "on",
    /// and an unnamed table cell around one checkbox and its label text.
    #[test]
    fn lightpanda_form_quirks_render_as_chrome_reads() {
        let mut combo = node("4", Some("2"), "combobox", "Currency USD EUR", &["5"], 4);
        combo["value"] = json!({"type": "string", "value": "USD"});
        let mut usd = node("6", Some("5"), "option", "USD", &[], 6);
        usd["properties"] = json!([{"name": "selected", "value": {"type": "boolean", "value": true}}]);
        let mut cb = node("9", Some("8"), "checkbox", "Email alerts for Drive", &[], 9);
        cb["value"] = json!({"type": "string", "value": "on"});
        let nodes = vec![
            node("1", None, "RootWebArea", "Prefs", &["2", "8"], 1),
            node("2", Some("1"), "group", "", &["3", "4"], 2),
            node("3", Some("2"), "StaticText", "Currency", &[], 3),
            combo,
            node("5", Some("4"), "MenuListPopup", "", &["6", "7"], 5),
            usd,
            node("7", Some("5"), "option", "EUR", &[], 7),
            node("8", Some("1"), "cell", "", &["9", "10"], 8),
            cb,
            node("10", Some("8"), "StaticText", "Email alerts for Drive", &[], 10),
        ];
        let out = render("", &nodes, &[], &Options::default());
        assert_eq!(
            out.text,
            "- group:\n  - combobox \"Currency\" [ref=4] value=\"USD\" options: \"USD (selected)\", \"EUR\"\n\
             - cell: checkbox \"Email alerts for Drive\" [ref=9]"
        );
    }

    #[test]
    fn shadow_content_renders_under_its_root_without_refs() {
        let found = vec![
            json!({"host": 1, "tag": "acme-coupon", "role": "textbox", "name": "Coupon code", "value": "SAVE10"}),
            json!({"host": 1, "tag": "acme-coupon", "role": "button", "name": "Apply coupon"}),
            json!({"host": 2, "tag": "x-prefs", "role": "checkbox", "name": "Email me", "checked": false}),
            json!({"host": 2, "tag": "x-prefs", "role": "heading", "name": "Prefs", "level": 3}),
        ];
        let text = render_shadow(&found);
        assert_eq!(text.matches("- shadow root of <").count(), 2, "{text}");
        assert!(text.contains("<acme-coupon>"), "{text}");
        assert!(text.contains("  - textbox \"Coupon code\" value=\"SAVE10\"\n"), "{text}");
        assert!(text.contains("  - checkbox \"Email me\" [unchecked]\n"), "{text}");
        assert!(text.contains("  - heading \"Prefs\" [level=3]\n"), "{text}");
        assert!(!text.contains("[ref="), "{text}");
        assert_eq!(render_shadow(&[]), "");
    }
}
