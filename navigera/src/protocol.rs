//! `navigera` — the Node-free realtime browser CLI library.
//!
//! This is the library behind the `navigera` binary: a Chrome-first,
//! line-oriented JSON protocol on a from-scratch CDP engine. The binary is a
//! thin wrapper — all parsing, protocol and session logic lives here so it
//! is reusable and testable.
//!
//! ```text
//! # Agents: one warm browser behind a named session, one command per step.
//! navigera --session s start
//! navigera --session s goto https://example.com
//! navigera --session s ax                 # text tree with [ref=N]
//! navigera --session s click 12           # act on a ref
//!
//! # Programs: one JSON command per stdin line.
//! navigera serve
//! {"id":1,"op":"goto","url":"https://example.com"}
//! {"id":2,"op":"eval","expression":"() => document.title"}
//! {"id":3,"op":"quit"}
//! ```

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

use crate::browser::BrowserSession;
use crate::cdp::{AxOptions, Target};

/// Default navigation / command timeout when a command carries none of its own.
pub const DEFAULT_TIMEOUT_MS: f64 = 35_000.0;

/// Default `ax` line budget (a 500-card listing is ~1500 lines).
pub const DEFAULT_AX_LIMIT: usize = 2000;

/// The skill shipped inside the binary (`navigera skill`,
/// `navigera install-skill`), so docs always match the binary version.
pub const SKILL_MD: &str = include_str!("../skills/navigera/SKILL.md");

/// Accepts a ref as a number or as `"12"`, `"@12"`, `"ref=12"`, `"e12"`
/// (the forms other agent tools print).
fn de_ref<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            n.as_u64().map(Some).ok_or_else(|| serde::de::Error::custom("ref must be a positive integer"))
        }
        Some(Value::String(s)) => parse_ref(&s)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("bad ref {s:?}; use the number from [ref=N]"))),
        Some(other) => Err(serde::de::Error::custom(format!("bad ref {other}"))),
    }
}

/// `"12"`, `"@12"`, `"ref=12"`, `"[ref=12]"`, `"e12"` -> 12.
pub fn parse_ref(raw: &str) -> Option<u64> {
    let s = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let s = s.strip_prefix("ref=").unwrap_or(s);
    let s = s.strip_prefix('@').unwrap_or(s);
    let s = s.strip_prefix('e').unwrap_or(s);
    s.parse::<u64>().ok()
}

/// A string or a list of strings (`select` values).
fn de_values<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s]),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) => Ok(s),
                other => Ok(other.to_string()),
            })
            .collect(),
        Some(other) => Ok(vec![other.to_string()]),
    }
}

/// Which element an op acts on: exactly one of these.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Clone)]
pub struct TargetArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, rename = "ref", deserialize_with = "de_ref", skip_serializing_if = "Option::is_none")]
    pub node_ref: Option<u64>,
    /// Visible text / label of the element (best match, actionable first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A description of the element, resolved to a ref by the decision
    /// model before the op runs (see `decide`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub find: Option<String>,
    /// ARIA role (`button`, `link`, `textbox`, `checkbox`, …), with `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Accessible name (with `role`, or alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A form control's label text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// `data-testid` (or `data-test-id`, `data-test`, `data-qa`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testid: Option<String>,
    /// Which match (0-based; -1 = the last), with any of the above or `selector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nth: Option<i64>,
    /// Look only inside this frame: an iframe/frame by CSS, or a chain
    /// `#outer >> #inner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
}

impl TargetArgs {
    fn target(&self, op: &str) -> anyhow::Result<Target> {
        self.optional(op)?.ok_or_else(|| {
            anyhow::anyhow!(
                "{op} needs a target: a ref from `ax` (`{op} 12`), --selector <css> or --text <visible text>"
            )
        })
    }

    fn locator(&self) -> bool {
        self.role.is_some()
            || self.name.is_some()
            || self.label.is_some()
            || self.placeholder.is_some()
            || self.testid.is_some()
    }

    fn optional(&self, op: &str) -> anyhow::Result<Option<Target>> {
        let found = self.unframed(op)?;
        match (&self.frame, found) {
            (Some(f), Some(Target::Ref(_))) => {
                anyhow::bail!("{op}: --frame {f:?} with a ref: a ref already names one element; drop --frame")
            }
            (Some(f), Some(t)) => Ok(Some(Target::InFrame(f.clone(), Box::new(t)))),
            (Some(f), None) => anyhow::bail!("{op}: --frame {f:?} needs a target in it (--selector, --text, --role …)"),
            (None, t) => Ok(t),
        }
    }

    fn unframed(&self, op: &str) -> anyhow::Result<Option<Target>> {
        let given = [
            self.node_ref.is_some(),
            self.text.is_some(),
            self.find.is_some(),
            self.selector.is_some() || self.locator(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if given > 1 {
            anyhow::bail!("{op}: give only one of ref, --text, --find, or --selector/--role/--name/--label/--placeholder/--testid");
        }
        if self.find.is_some() {
            anyhow::bail!("{op}: --find was not resolved to an element");
        }
        if self.locator() || self.nth.is_some() {
            if self.nth.is_some() && !self.locator() && self.selector.is_none() {
                anyhow::bail!(
                    "{op}: --nth picks among the matches of --selector, --role, --label, --placeholder or --testid"
                );
            }
            return Ok(Some(Target::Locator(crate::cdp::Locator {
                role: self.role.clone(),
                name: self.name.clone(),
                label: self.label.clone(),
                placeholder: self.placeholder.clone(),
                testid: self.testid.clone(),
                css: self.selector.clone(),
                nth: self.nth,
            })));
        }
        Ok(if let Some(r) = self.node_ref {
            Some(Target::Ref(r))
        } else if let Some(s) = &self.selector {
            Some(Target::Selector(s.clone()))
        } else {
            self.text.clone().map(Target::Text)
        })
    }
}

/// One command from the model, either a CLI subcommand or a serve-mode line.
#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
#[serde(tag = "op")]
pub enum Command {
    /// Open the browser (serve mode only; one-shots launch implicitly).
    #[serde(rename = "open")]
    Open {
        #[serde(default)]
        url: Option<String>,
    },
    /// Navigate the live page.
    #[serde(rename = "goto", alias = "navigate")]
    Goto {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    #[serde(rename = "back")]
    Back {},
    #[serde(rename = "forward")]
    Forward {},
    #[serde(rename = "reload")]
    Reload {},
    /// Files the pages downloaded.
    #[serde(rename = "downloads")]
    Downloads {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// Trusted mouse click on the target.
    #[serde(rename = "click")]
    Click {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
        /// `left` (default), `right` (opens a context menu) or `middle`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<String>,
        /// DOM events on the element, covered or not (skips the checks a
        /// user's click passes).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// Set an input's value (native setter + input/change events).
    #[serde(rename = "fill")]
    Fill {
        #[serde(flatten)]
        target: TargetArgs,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
        /// Clear the field and type the value key by key, for inputs whose
        /// app only takes what is typed (observable models).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keys: bool,
    },
    /// Move the mouse over the target (hover menus, tooltips).
    #[serde(rename = "hover")]
    Hover {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Raw mouse input at a point: `move`, `down`, `up`, `click`, `wheel`
    /// (x and y are then the scroll deltas). Viewport CSS pixels, or from
    /// the target's top-left corner when one is given (a canvas).
    #[serde(rename = "mouse")]
    Mouse {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<u32>,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Choose `<select>` option(s) by value or label.
    #[serde(rename = "select")]
    Select {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, alias = "values", deserialize_with = "de_values")]
        value: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
        /// Add to the selection of a multiple select instead of replacing it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        add: bool,
    },
    /// Press a key or chord (`Enter`, `Control+a`), optionally on a target.
    #[serde(rename = "press")]
    Press {
        key: String,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Type text key by key (into the target, or the focused element).
    #[serde(rename = "type")]
    Type {
        value: String,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Scroll the window (`by` px, or `to` top|bottom) or a target into view.
    #[serde(rename = "scroll")]
    Scroll {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Set the files of an `<input type=file>`.
    #[serde(rename = "upload")]
    Upload {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, alias = "file", deserialize_with = "de_values")]
        files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Wait until every given condition holds.
    #[serde(rename = "wait")]
    Wait {
        /// A visible element matching this CSS selector.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        /// This text anywhere in the page's visible text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The URL contains this substring.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// No visible element matches this selector any more.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gone: Option<String>,
        /// A JS expression that becomes truthy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        js: Option<String>,
        /// Look for `selector`, `text` and `gone` only inside this frame
        /// (CSS, or a chain `#outer >> #inner`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<String>,
        /// Just sleep this long.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<f64>,
        /// No fetch/XHR in flight for 500 ms (network idle).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        idle: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
        /// A finished download whose file name matches this glob ("" any).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        download: Option<String>,
    },
    /// How `alert`/`confirm`/`prompt` dialogs are answered from now on.
    #[serde(rename = "dialog")]
    Dialog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accept: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_text: Option<String>,
    },
    /// First element's textContent for the selector.
    #[serde(rename = "text")]
    Text {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// The element a description means, chosen by the decision model.
    #[serde(rename = "find")]
    Find {
        what: String,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// The probability that a yes/no question about the page is yes.
    #[serde(rename = "check")]
    Check {
        question: String,
        #[serde(default)]
        timeout_ms: Option<f64>,
    },
    /// Evaluate a JS expression; result must be JSON-serializable.
    #[serde(rename = "eval", alias = "evaluate")]
    Eval {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Page title.
    #[serde(rename = "title")]
    Title {},
    /// Accessibility snapshot: an indented text tree with `[ref=N]` on
    /// actionable nodes (iframes included). `format: "json"` returns the
    /// older flat `[{ref, role, name, value?}]` list (`all: true` = raw).
    #[serde(rename = "ax", alias = "snapshot")]
    Ax {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_depth: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        all: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        /// `"all"`: a ref on every node, not just actionable ones.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refs: Option<String>,
        /// Only the subtree of this element.
        #[serde(flatten)]
        scope: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Console messages, uncaught exceptions and browser log entries of
    /// the active tab, oldest first.
    #[serde(rename = "console")]
    Console {
        /// Only this level and above: `debug`, `log`, `info`, `warn`, `error`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        /// Empty the log after reading it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
        /// `json`: a list of `{level, source, text, location}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// `console --level error`: exceptions, console.error, failed loads.
    #[serde(rename = "errors")]
    Errors {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// The active tab's network requests, one line each; with `n`, one
    /// request in full (headers, post data, response body).
    #[serde(rename = "network")]
    Network {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u64>,
        /// Only URLs containing this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// Only this resource type (`fetch`, `xhr`, `document`, `script`, …).
        #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Only failed requests and HTTP errors (status >= 400).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        failed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
        /// Write the (filtered) log as a HAR 1.2 file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        har: Option<String>,
        /// With `n`: write the response body to this file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_file: Option<String>,
        /// Only requests after #n ("what did that click fire?").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    /// Cookies of the current page (or all of them), or set / delete / clear.
    #[serde(rename = "cookies")]
    Cookies {
        /// `list` (default), `set`, `delete`, `clear`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// The URL the cookie belongs to (default: the current page).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Unix time in seconds; none = a session cookie.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires: Option<f64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        http_only: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        secure: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        same_site: Option<String>,
        /// `list`: every cookie in the browser, not just the page's.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// Cookies and localStorage to a file, or back from one, in Playwright's
    /// `storageState` format: sign in once, save, and load in a new session.
    #[serde(rename = "state")]
    State {
        /// `save` or `load`.
        action: String,
        path: String,
    },
    /// The page as a PDF file.
    #[serde(rename = "pdf")]
    Pdf {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        landscape: bool,
        /// Print background colors and images.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        background: bool,
    },
    /// localStorage / sessionStorage of the current page.
    #[serde(rename = "storage")]
    Storage {
        /// `local` (default) or `session`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        area: Option<String>,
        /// `get` (default), `set`, `remove`, `clear`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Emulate a device or network condition for every tab: `viewport`,
    /// `device`, `media`, `geo`, `offline`, `headers`, `ua`, `locale`,
    /// `timezone` (`off` resets one).
    #[serde(rename = "set")]
    Set {
        what: String,
        #[serde(default, alias = "value", deserialize_with = "de_values")]
        values: Vec<String>,
        /// `viewport`: device pixel ratio.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
        /// `viewport`: a mobile viewport (meta viewport, overlay scrollbars).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mobile: bool,
        /// `geo`: accuracy in meters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accuracy: Option<f64>,
    },
    /// Intercept requests whose URL matches `pattern` (`*` and `?` globs):
    /// fail them, or answer with `status`/`body`. Without a pattern: list
    /// the rules.
    #[serde(rename = "route")]
    Route {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        abort: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<String>,
    },
    /// Remove the rule for `pattern`, or all rules.
    #[serde(rename = "unroute")]
    Unroute {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// A script that runs before the page's own scripts in every document
    /// of every tab (auth shims, feature flags, mocks of `Date`/`fetch`).
    /// Without a source: list them; `clear`: remove them.
    #[serde(rename = "init-script")]
    InitScript {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
    },
    /// A property of an element: `text`, `value`, `attr <name>`, `html`,
    /// `outer`, `box`, `style <property>`; `count` of a selector's matches.
    #[serde(rename = "get")]
    Get {
        what: String,
        /// `attr`: the attribute; `style`: the CSS property.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_of: Option<String>,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Whether an element is `visible`, `hidden`, `enabled`, `disabled`,
    /// `checked`, `editable` or `focused` (a missing element: not visible).
    #[serde(rename = "is")]
    Is {
        state: String,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Drag one element onto another: HTML5 drag and drop for a
    /// `draggable` source, a held mouse moved across for anything else.
    #[serde(rename = "drag")]
    Drag {
        from: TargetArgs,
        to: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    #[serde(rename = "dblclick")]
    Dblclick {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    #[serde(rename = "focus")]
    Focus {
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Check (or uncheck) a checkbox, radio or switch: clicks it only when
    /// its state differs.
    #[serde(rename = "set-checked")]
    SetChecked {
        checked: bool,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Hold a key down (`Shift`, `Control`, a character) without releasing it.
    #[serde(rename = "keydown")]
    KeyDown { key: String },
    #[serde(rename = "keyup")]
    KeyUp { key: String },
    /// Insert text at the caret in one go (paste / IME commit): no key events.
    #[serde(rename = "insert-text")]
    InsertText {
        value: String,
        #[serde(flatten)]
        target: TargetArgs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<f64>,
    },
    /// Current page URL and tab state.
    #[serde(rename = "url")]
    Url {},
    /// What this session runs: engine, browser executable, CDP endpoint,
    /// tabs, active URL.
    #[serde(rename = "status")]
    Status {},
    /// PNG screenshot to disk; result is the written path + byte length.
    #[serde(rename = "screenshot")]
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full_page: Option<bool>,
    },
    /// List open tabs: index, target id, and whether it is the active tab.
    #[serde(rename = "tab-list")]
    TabList {},
    /// Put the browser on screen, with the same tabs and sign-ins: hand the
    /// session to a person (a login, a 2FA prompt, a CAPTCHA).
    #[serde(rename = "show")]
    Show {},
    /// Take it off screen again (headless), with the same tabs and sign-ins.
    #[serde(rename = "hide")]
    Hide {},
    /// A clean slate: a new browser context with one blank tab. `all`: also
    /// forget `set` emulation, routes, credentials, init scripts and the
    /// dialog answer, as a new session would have none.
    Reset {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    /// Open a fresh tab (optionally at a URL) and make it active.
    #[serde(rename = "tab-new")]
    TabNew {
        #[serde(default)]
        url: Option<String>,
    },
    /// Switch the active tab by zero-based index.
    #[serde(rename = "tab-select")]
    TabSelect {
        #[serde(default)]
        index: usize,
        /// A tab's `target` id (or its first characters) from `tab-list`:
        /// stays put when tabs open and close, unlike an index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Close one tab by index (defaults to the active tab).
    #[serde(rename = "tab-close")]
    TabClose {
        #[serde(default)]
        index: Option<usize>,
    },
    /// Alias of `tab-close`.
    #[serde(rename = "close-page")]
    ClosePage {
        #[serde(default)]
        index: Option<usize>,
    },
    /// Close the session and exit (serve mode). An attached session
    /// (`--attach`, `--profile`) only disconnects, unless `close_browser`.
    #[serde(rename = "quit", alias = "close")]
    Quit {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        close_browser: bool,
    },
}

/// JSON response for one command.
#[derive(Debug, Serialize, PartialEq)]
pub struct Response {
    pub id: Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Dialogs the page opened during this op and how they were answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialogs: Option<Vec<Value>>,
    /// Tabs the page opened during this op (now in the tab list; the last
    /// one is active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_tabs: Option<Vec<usize>>,
    /// Things the engine did differently from what the agent asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<String>>,
    /// Server-side time for the op, in milliseconds with µs resolution.
    pub elapsed_ms: f64,
}

/// Launch/session settings shared by one-shot and serve mode.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Browser engine (`chrome` by default — the validated production path).
    pub engine: String,
    pub headed: bool,
    pub chromium: Option<String>,
    pub timeout_ms: f64,
    pub pretty: bool,
    /// Print only the result (strings raw), errors as `error: …` on stderr.
    pub raw: bool,
    /// CDP transport: `pipe` (default on Linux/macOS) or `ws` (DevTools
    /// port; the default on Windows).
    pub transport: Option<String>,
    /// Named background session (`--session <name|socket path>`): `serve`
    /// listens on it, `start` spawns that server detached, and every other
    /// command becomes a client call against the warm browser behind it.
    pub session: Option<String>,
    /// A session server with no traffic for this long shuts its browser down
    /// (0 = never). Keeps forgotten agent sessions from leaking Chrome.
    pub idle_timeout_s: u64,
    /// `--attach <where>`: work in a browser that is already running
    /// (default `chrome`: the user's own, via chrome://inspect remote
    /// debugging) instead of launching one. Never closes it.
    pub attach: Option<String>,
    /// `--profile <name|dir>`: a persistent, visible navigera browser on
    /// its own profile, started when needed and kept open between sessions.
    pub profile: Option<String>,
    /// `--headless` for a `--profile` browser (visible by default).
    pub headless: bool,
    /// `--from <browser|user-data-dir>`: seed the `--profile` browser's
    /// profile from the user's own browser (a copy: logins, cookies, site
    /// storage), replacing what it held.
    pub from: Option<String>,
    /// `--from-profile <name|dir|email>`: which of that browser's profiles
    /// (default: the one it used last).
    pub from_profile: Option<String>,
    /// `--natural`: a browser that looks like the browser it is, not like an
    /// automated one (see `browser::natural_flags`).
    pub natural: bool,
    /// `--wait-ms`: how long element ops wait for their element when they
    /// name no timeout (see `browser::element_wait`).
    pub wait_ms: Option<f64>,
    /// `--download-dir`: where downloads are saved (see
    /// `browser::download_dir`).
    pub download_dir: Option<String>,
}

/// Before `ax`/`screenshot`: no fetch/XHR for this long (Playwright's
/// `networkidle` uses the same 500 ms) …
const NETWORK_QUIET: std::time::Duration = std::time::Duration::from_millis(500);
/// … waiting at most this long (long polls, chatty analytics).
const NETWORK_QUIET_MAX: std::time::Duration = std::time::Duration::from_secs(3);

/// Default idle shutdown for session servers.
pub const DEFAULT_IDLE_TIMEOUT_S: u64 = 1800;

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            engine: "chrome".to_string(),
            headed: false,
            chromium: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            pretty: false,
            raw: false,
            transport: None,
            session: None,
            idle_timeout_s: DEFAULT_IDLE_TIMEOUT_S,
            attach: None,
            profile: None,
            headless: false,
            from: None,
            from_profile: None,
            natural: false,
            wait_ms: None,
            download_dir: None,
        }
    }
}

impl SessionConfig {
    /// The browser to connect to instead of launching one, if any.
    pub fn attach_target(&self) -> Option<crate::browser::AttachTarget> {
        if let Some(spec) = &self.attach {
            return Some(crate::browser::AttachTarget::Running(spec.clone()));
        }
        self.profile.as_ref().map(|name| crate::browser::AttachTarget::Profile {
            name: name.clone(),
            executable: self.chromium.clone(),
            headless: self.headless,
            from: self.from.clone(),
            from_profile: self.from_profile.clone(),
        })
    }
}

/// `--from` and `--from-profile` seed a `--profile` browser and mean nothing
/// without one.
fn check_profile_flags(config: &SessionConfig) -> Result<(), String> {
    if (config.from.is_some() || config.from_profile.is_some()) && config.profile.is_none() {
        return Err("--from copies your browser's profile into a `--profile <name>` browser: add --profile".into());
    }
    if config.from_profile.is_some() && config.from.is_none() {
        return Err("--from-profile picks a profile of the browser --from names: add --from chrome (or another)".into());
    }
    Ok(())
}

/// `--attach` takes an optional value; the default is the user's Chrome.
const DEFAULT_ATTACH: &str = "chrome";

/// True when `word` is a command name (so it can't be `--attach`'s value).
fn is_command_word(word: &str) -> bool {
    matches!(word, "serve" | "mcp" | "skill" | "install-skill" | "profiles" | "help")
        || OPS.iter().any(|(name, _, _)| *name == word)
}

/// Pull `--attach [value]` out of argv; a bare `--attach` means `chrome`.
fn extract_attach(args: &mut std::collections::VecDeque<String>) -> Result<Option<String>, String> {
    let mut found = None;
    let mut rest = std::collections::VecDeque::new();
    while let Some(arg) = args.pop_front() {
        let value = if arg == "--attach" {
            match args.front() {
                Some(next) if !next.starts_with('-') && !is_command_word(next) => args.pop_front(),
                _ => Some(DEFAULT_ATTACH.to_string()),
            }
        } else if let Some(v) = arg.strip_prefix("--attach=") {
            Some(v.to_string())
        } else {
            rest.push_back(arg);
            continue;
        };
        if found.is_some() {
            return Err("duplicate --attach".into());
        }
        found = value;
    }
    *args = rest;
    Ok(found)
}

/// Commands that never touch a browser.
#[derive(Debug, PartialEq)]
pub enum Local {
    /// Print the embedded SKILL.md.
    Skill,
    /// Write SKILL.md to `<dir>/navigera/SKILL.md`.
    InstallSkill {
        dir: String,
    },
    Version,
    /// List the browsers and profiles a session can be handed.
    Profiles,
}

/// Result of [`parse_args`]: launch config plus an optional one-shot command.
#[derive(Debug, PartialEq)]
pub struct ParsedArgs {
    pub config: SessionConfig,
    /// `None` selects serve mode (stdin/stdout protocol, or the socket when
    /// `config.session` is set).
    pub command: Option<Command>,
    /// `start`: spawn a detached session server for `config.session`.
    pub start: bool,
    /// A command that runs without a browser.
    pub local: Option<Local>,
    /// `mcp`: serve the Model Context Protocol on stdin/stdout.
    pub mcp: bool,
}

/// One op's CLI documentation: (name, synopsis, what it does).
const OPS: &[(&str, &str, &str)] = &[
    ("start", "[--attach [<where>] | --profile <name> [--from <browser> [--from-profile <p>]] [--headless]] [--natural]", "spawn a detached session server + headless browser (needs --session); --attach/--profile use a browser that stays open; --from seeds the profile with a copy of your browser's sign-ins"),
    ("goto", "<url> [--wait load|domcontentloaded|commit]", "navigate the active tab (default: wait for load)"),
    ("ax", "[--selector <css> | <ref>] [--limit <lines>] [--refs all] [--format json]", "accessibility snapshot: indented tree, [ref=N] on actionable nodes"),
    ("click", "<ref> | --selector <css> | --text <visible text> [--button right|middle] [--force]", "trusted mouse click once the element stops moving; refuses an element covered by another (--force: DOM events anyway); waits for any navigation it starts"),
    ("fill", "<ref> <value> | --selector <css> --value <v> [--keys]", "set an input/textarea's value; --keys clears it and types the value key by key (for apps that ignore a value set directly)"),
    ("type", "<text> [--selector <css> | --ref <n>]", "type key by key into the target or the focused element"),
    ("press", "<key> [--selector <css> | --ref <n>]", "press a key or chord: Enter, Tab, Escape, ArrowDown, Control+a"),
    ("select", "<ref> <option>... | --selector <css> --value <option> [--add]", "choose <select> option(s) by value or label (whitespace-insensitive); --add keeps the options already chosen"),
    ("hover", "<ref> | --selector <css> | --text <t>", "move the mouse over an element (hover menus)"),
    ("mouse", "move <x> <y> [--steps n] | down | up | click <x> <y> [--count 2] | wheel <dx> <dy>  [--button right|middle] [<target>]", "raw mouse at a point, for what has no element to aim at (a canvas, a map, a game): viewport CSS pixels (as in a screenshot), or from the target's top-left corner; the pointer stays where it went and held buttons stay held, so down, moves, up drags or draws"),
    ("scroll", "[--by <px> | --to top|bottom] [<ref> | --selector <css>]", "scroll the page, or an element into view"),
    ("upload", "<ref> <file>... | --selector <css> --file <path>", "set the files of an <input type=file>"),
    ("wait", "[--selector <css>] [--text <t>] [--url <part>] [--gone <css>] [--js <expr>] [--idle] [--download [<glob>]] [--ms <n>]", "wait until every condition holds (default up to 5 s; --timeout-ms); --idle: no fetch/XHR for 500 ms; --download: a finished download"),
    ("downloads", "[--clear] [--json]", "files the pages downloaded: #n state bytes path (url)"),
    ("eval", "<js> | --file <path.js> | -", "evaluate JS (arrow functions are called); prints the JSON result; `-` reads the script from stdin"),
    ("text", "--selector <css>", "textContent of the first match"),
    ("find", "<description>", "the element a description means, chosen by a decision model (TYPESAFE_API_KEY); element ops also take --find <description>"),
    ("check", "<yes/no question>", "probability that the answer about the page is yes, from a decision model (TYPESAFE_API_KEY)"),
    ("title", "", "page title"),
    ("console", "[--level error|warn|info|log|debug] [--limit <n>] [--clear] [--json]", "console messages, uncaught exceptions and browser log entries of the active tab"),
    ("errors", "[--limit <n>] [--clear] [--json]", "only the errors: exceptions, console.error, failed loads"),
    ("network", "[<n>] [--filter <text>] [--type fetch|xhr|document|…] [--failed] [--since <n>] [--limit <n>] [--clear] [--har <file>] [--body-file <file>]", "the active tab's requests, one line each (#n METHOD STATUS type url size time); `network <n>` shows one in full with its response body"),
    ("cookies", "[set <name> <value> [--url <u> | --domain <d>] [--path <p>] [--expires <unix s>] [--http-only] [--secure] [--same-site Lax|Strict|None] | delete <name> [--url <u>] | clear] [--all] [--json]", "the current page's cookies (--all: every cookie), or set, delete or clear them"),
    ("pdf", "[--path <f.pdf>] [--landscape] [--background]", "the page as a PDF file"),
    ("storage", "[local|session] [get [<key>] | set <key> <value> | remove <key> | clear]", "the current page's localStorage (or sessionStorage)"),
    ("state", "save|load <file.json>", "every cookie and the open tabs' localStorage to a file, or back (Playwright storageState format): sign in once, reuse it in later sessions"),
    ("set", "viewport <w> <h> [--scale <dpr>] [--mobile] | device <name> | media dark|light|print|reduced-motion | geo <lat> <lon> | offline on|off | headers <Name: value>… | ua <string> | locale <tag> | timezone <zone> | credentials <user> <password>  (`off` resets)", "emulate a device or network condition in every tab (and in tabs opened later)"),
    ("route", "[<url glob> [--abort | --status <n> [--body <text> | --body-file <f>] [--content-type <t>] [--header 'Name: value']]]", "fail or answer requests whose URL matches (* and ? globs); no flags = fail; no pattern = list the rules"),
    ("unroute", "[<url glob>]", "remove one route rule, or all"),
    ("init-script", "[<js> | --file <path.js> | --clear]", "run a script before the page's own in every document of every tab; no argument lists them"),
    ("get", "text|value|html|outer|box|attr <name>|style <prop> <target> | count <css>", "read one property of an element (target: ref, --selector, --text, --role/--name, --label, --placeholder, --testid, --nth; --frame <css> to look only inside that frame)"),
    ("is", "visible|hidden|enabled|disabled|checked|editable|focused <target>", "true/false; a missing element is not visible"),
    ("dblclick", "<target>", "trusted double click"),
    ("drag", "<from> <to> | --from-text <t> --to-text <t>", "drag one element onto another (each a ref or CSS selector): HTML5 drag and drop for a draggable source, a held mouse moved across otherwise"),
    ("focus", "<target>", "focus an element"),
    ("check", "<target> | <yes/no question>", "check a checkbox/radio/switch (clicks only if needed); with a question instead of a target, the decision model's yes/no"),
    ("uncheck", "<target>", "uncheck a checkbox/switch"),
    ("keydown", "<key>", "hold a key (Shift, Control, a character) until keyup"),
    ("keyup", "<key>", "release a held key"),
    ("insert-text", "<text> [<target>]", "insert text at the caret in one go (paste/IME), no key events"),
    ("url", "", "active tab state {tab, tabs, url, title}"),
    ("status", "", "the session's engine, browser executable, CDP endpoint and tab state"),
    ("back", "", "history back"),
    ("forward", "", "history forward"),
    ("reload", "", "reload the page"),
    ("screenshot", "[--path <f.png>] [--full-page]", "PNG of the viewport (or whole page)"),
    ("reset", "[--all]", "a clean slate in the same browser: a new browser context (no cookies, storage or cache from before) with one blank tab; the old tabs close (`set`, routes, init scripts and the dialog answer stay; --all drops them too)"),
    ("tab-new", "[<url>]", "open a tab and make it active"),
    ("tab-list", "", "list tabs"),
    ("show", "", "put the browser on screen with the same tabs and sign-ins, to hand the session to a person (a login, 2FA, a CAPTCHA); `hide` takes it back off"),
    ("hide", "", "take the browser off screen again (headless), with the same tabs and sign-ins"),
    ("tab-select", "<index> | <target id>", "switch the active tab (a target id from tab-list stays right when tabs open and close)"),
    ("tab-close", "[<index>]", "close a tab (default: active)"),
    ("dialog", "--accept [--prompt-text <t>] | --dismiss", "how alert/confirm/prompt are answered (default: accept; every dialog is reported)"),
    ("quit", "[--close-browser]", "close the browser and the session server; attached (--attach/--profile): only disconnect, unless --close-browser"),
    ("mcp", "[--headed] [--profile <name> | --attach [<where>]]", "serve the Model Context Protocol on stdio (for Cline, Roo, Kilo, Continue, VS Code, Claude Desktop, Cursor); the browser starts on the first tool call"),
    ("profiles", "", "list what a session can be handed: your browsers' profiles (name, email, whether --attach can reach them now) and navigera's own --profile browsers"),
    ("skill", "", "print the agent skill (usage guide) for this version"),
    ("install-skill", "[--dir <skills dir>] [--claude | --continue] [--global]", "install the skill for agents (default ./.agents/skills, read by Cline and Kilo; --claude: ./.claude/skills; --continue: ./.continue/skills)"),
];

/// Help text for `--help` / argument errors.
pub fn usage(program: &str) -> String {
    let mut out = format!(
        "navigera {} — drive headless Chrome over CDP, one shell command per step.\n\n\
         AGENT LOOP:\n  \
         {program} --session s start\n  \
         {program} --session s goto https://example.com\n  \
         {program} --session s ax                     # read: tree with [ref=N]\n  \
         {program} --session s click 12               # act on a ref\n  \
         {program} --session s fill 31 \"hello\"\n  \
         {program} --session s quit\n\n\
         COMMANDS:\n",
        env!("CARGO_PKG_VERSION")
    );
    for (name, args, what) in OPS {
        out.push_str(&format!("  {name:<13} {args}\n  {:<13} {what}\n", ""));
    }
    out.push_str(&format!(
        "\nGLOBAL FLAGS (before or after the command):\n  \
         --session <name>       warm browser behind a local socket ($NAVIGERA_SESSION)\n  \
         --raw                  print only the result (strings unquoted); errors on stderr\n  \
         --timeout-ms <ms>      navigation/command timeout (default {DEFAULT_TIMEOUT_MS}); per op: element wait (default 5000)\n  \
         --chromium <path>      browser binary ($CHROME_BIN); --engine lightpanda ($NAVIGERA_ENGINE); --headed\n  \
         --transport ws         CDP over a DevTools port instead of a private pipe (to attach DevTools; Windows default)\n  \
         --idle-timeout-s <s>   session server idle shutdown (default {DEFAULT_IDLE_TIMEOUT_S}, 0 = never)\n  \
         --attach [chrome|edge|<dir>|<port>]  with `start`: use your running browser (Chrome 144+: chrome://inspect/#remote-debugging); never closes it\n  \
         --profile <name>       with `start`: persistent visible navigera browser, kept open between sessions (--headless to hide)\n  \
         --from <browser|dir>   with --profile: first copy your own browser's sign-ins into it (cookies, site storage, saved logins); --from-profile <dir|name|email>\n  \
         --natural              look like the browser it is: no HeadlessChrome user agent, a real screen size ($NAVIGERA_NATURAL=1)\n  \
         --pretty               pretty-print JSON\n\n\
         Output: one JSON object per command, {{\"ok\":true,\"result\":…}} or {{\"ok\":false,\"error\":…}}; exit 1 on error.\n\
         Without --session each command launches and closes its own browser (one-shot).\n\
         Programs: `{program} serve` reads JSON commands on stdin, e.g. {{\"id\":1,\"op\":\"goto\",\"url\":\"…\"}}.\n\
         More: `{program} help <command>`, `{program} skill`."
    ));
    out
}

/// `help <op>`.
pub fn op_help(program: &str, op: &str) -> Option<String> {
    let (name, args, what) = OPS.iter().find(|(n, ..)| *n == op)?;
    Some(format!("usage: {program} [--session <s>] {name} {args}\n{what}"))
}

/// Nearest known command for a typo, plus aliases other agent CLIs use.
fn suggest(op: &str) -> Option<&'static str> {
    let aliases: &[(&str, &str)] = &[
        ("open-url", "goto"),
        ("visit", "goto"),
        ("go", "goto"),
        ("evaluate", "eval"),
        ("js", "eval"),
        ("exec", "eval"),
        ("key", "press"),
        ("keypress", "press"),
        ("check", "click"),
        ("tap", "click"),
        ("dblclick", "click"),
        ("input", "fill"),
        ("clear", "fill"),
        ("tabs", "tab-list"),
        ("tab", "tab-list"),
        ("exit", "quit"),
        ("stop", "quit"),
        ("tree", "ax"),
        ("a11y", "ax"),
        ("accessibility", "ax"),
        ("refresh", "reload"),
        ("sleep", "wait"),
        ("waitfor", "wait"),
        ("shot", "screenshot"),
    ];
    if let Some((_, to)) = aliases.iter().find(|(a, _)| a.eq_ignore_ascii_case(op)) {
        return Some(to);
    }
    fn dist(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut cur = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
        }
        prev[b.len()]
    }
    let extra: &[(&str, &str)] = &[("navigate", "goto"), ("snapshot", "ax"), ("evaluate", "eval"), ("close", "quit")];
    OPS.iter()
        .map(|(n, ..)| (*n, *n))
        .chain(aliases.iter().chain(extra.iter()).copied())
        .map(|(name, to)| (to, dist(&op.to_ascii_lowercase(), name)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(n, _)| n)
}

fn next_value(args: &mut std::collections::VecDeque<String>, flag: &str) -> Result<String, String> {
    args.pop_front().ok_or_else(|| format!("missing value after {flag}"))
}

/// Why [`parse_args`] failed: `Help` is a deliberate `--help` (exit 0),
/// `Invalid` is bad usage (exit 2).
#[derive(Debug, PartialEq)]
pub enum ArgsError {
    /// Usage text, requested explicitly (`--help`) or because no args given.
    Help(String),
    /// Invalid invocation; message may embed the usage text.
    Invalid(String),
}

/// A path the caller gave, made absolute against the caller's directory
/// (a session server runs elsewhere).
fn caller_path(dir: String) -> String {
    let path = std::path::Path::new(&dir);
    if path.is_absolute() {
        return dir;
    }
    std::env::current_dir().map(|c| c.join(path).display().to_string()).unwrap_or(dir)
}

fn parse_f64(raw: &str, flag: &str) -> Result<f64, String> {
    raw.parse::<f64>().map_err(|_| format!("{flag} must be a number, got {raw:?}"))
}

fn parse_index(raw: &str) -> Result<usize, String> {
    raw.parse::<usize>().map_err(|_| format!("tab index must be a number, got {raw:?}"))
}

/// Scan every `--flag value` (or `--flag=value`) out of the remaining argv.
fn extract_values(args: &mut std::collections::VecDeque<String>, flag: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    let mut rest = std::collections::VecDeque::new();
    let with_eq = format!("{flag}=");
    while let Some(arg) = args.pop_front() {
        if arg == flag {
            values.push(next_value(args, flag)?);
        } else if let Some(v) = arg.strip_prefix(&with_eq) {
            values.push(v.to_string());
        } else {
            rest.push_back(arg);
        }
    }
    *args = rest;
    Ok(values)
}

/// Scan one `--flag value` out of the remaining argv (errors on duplicates).
fn extract_value(args: &mut std::collections::VecDeque<String>, flag: &str) -> Result<Option<String>, String> {
    let mut values = extract_values(args, flag)?;
    if values.len() > 1 {
        return Err(format!("duplicate {flag}"));
    }
    Ok(values.pop())
}

/// Scan one boolean `--flag` out of the remaining argv.
fn extract_present(args: &mut std::collections::VecDeque<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

/// Global flags may appear before *or* after the command (`eval … --pretty`).
fn consume_globals(args: &mut std::collections::VecDeque<String>, config: &mut SessionConfig) -> Result<(), String> {
    if let Some(raw) = extract_value(args, "--engine")? {
        config.engine = raw;
    }
    if extract_present(args, "--headed") {
        config.headed = true;
    }
    if let Some(path) = extract_value(args, "--chromium")? {
        config.chromium = Some(path);
    }
    if extract_present(args, "--pretty") {
        config.pretty = true;
    }
    if extract_present(args, "--raw") {
        config.raw = true;
    }
    if let Some(t) = extract_value(args, "--transport")? {
        config.transport = Some(t);
    }
    if let Some(name) = extract_value(args, "--session")?.or(extract_value(args, "-s")?) {
        config.session = Some(name);
    }
    if let Some(raw) = extract_value(args, "--idle-timeout-s")? {
        config.idle_timeout_s =
            raw.parse::<u64>().map_err(|_| format!("--idle-timeout-s must be whole seconds, got {raw:?}"))?;
    }
    if let Some(spec) = extract_attach(args)? {
        config.attach = Some(spec);
    }
    if let Some(name) = extract_value(args, "--profile")? {
        config.profile = Some(name);
    }
    if extract_present(args, "--headless") {
        config.headless = true;
    }
    if let Some(from) = extract_value(args, "--from")? {
        config.from = Some(from);
    }
    if let Some(from_profile) = extract_value(args, "--from-profile")? {
        config.from_profile = Some(from_profile);
    }
    if let Some(raw) = extract_value(args, "--wait-ms")? {
        config.wait_ms = Some(parse_f64(&raw, "--wait-ms")?);
    }
    if let Some(dir) = extract_value(args, "--download-dir")? {
        config.download_dir = Some(caller_path(dir));
    }
    if extract_present(args, "--natural") {
        config.natural = true;
    }
    check_profile_flags(config)?;
    if config.attach.is_some() && config.profile.is_some() {
        return Err("--attach and --profile are alternatives: pick one".into());
    }
    if (config.attach.is_some() || config.profile.is_some()) && crate::browser::is_lightpanda(&config.engine) {
        return Err("--attach/--profile drive Chrome; drop --engine lightpanda".into());
    }
    Ok(())
}

type Args = std::collections::VecDeque<String>;

fn invalid<T>(msg: impl Into<String>) -> Result<T, ArgsError> {
    Err(ArgsError::Invalid(msg.into()))
}

fn str_flag(args: &mut Args, flag: &str) -> Result<Option<String>, ArgsError> {
    extract_value(args, flag).map_err(ArgsError::Invalid)
}

fn num_flag(args: &mut Args, flag: &str) -> Result<Option<f64>, ArgsError> {
    match str_flag(args, flag)? {
        Some(raw) => Ok(Some(parse_f64(&raw, flag).map_err(ArgsError::Invalid)?)),
        None => Ok(None),
    }
}

/// `--selector` / `--ref` / `--text` flags, or a leading positional: a ref
/// (`12`, `@12`, `e12`, `ref=12`) when it parses as one, else a selector.
fn target_args(args: &mut Args, positional_target: bool) -> Result<TargetArgs, ArgsError> {
    let mut t = TargetArgs {
        selector: str_flag(args, "--selector")?,
        node_ref: match str_flag(args, "--ref")? {
            Some(raw) => Some(parse_ref(&raw).ok_or_else(|| {
                ArgsError::Invalid(format!("--ref must be a number from [ref=N] in `ax`, got {raw:?}"))
            })?),
            None => None,
        },
        text: str_flag(args, "--text")?,
        find: str_flag(args, "--find")?,
        role: str_flag(args, "--role")?,
        name: str_flag(args, "--name")?,
        label: str_flag(args, "--label")?,
        placeholder: str_flag(args, "--placeholder")?,
        testid: str_flag(args, "--testid")?,
        nth: match str_flag(args, "--nth")? {
            Some(raw) => Some(raw.parse::<i64>().map_err(|_| {
                ArgsError::Invalid(format!("--nth takes an index (0, 1, -1 for the last), got {raw:?}"))
            })?),
            None => None,
        },
        frame: str_flag(args, "--frame")?,
    };
    if positional_target && t == TargetArgs::default() {
        if let Some(first) = args.front().filter(|a| !a.starts_with("--")).cloned() {
            args.pop_front();
            match parse_ref(&first) {
                Some(r) => t.node_ref = Some(r),
                None => t.selector = Some(first),
            }
        }
    }
    Ok(t)
}

/// The remaining positional words as one string (`find add to cart` works
/// unquoted).
fn rest_words(args: &mut Args) -> Option<String> {
    let mut words = Vec::new();
    while let Some(w) = positional(args) {
        words.push(w);
    }
    (!words.is_empty()).then(|| words.join(" "))
}

fn positional(args: &mut Args) -> Option<String> {
    if args.front().is_some_and(|a| !a.starts_with("--")) {
        args.pop_front()
    } else {
        None
    }
}

/// Parse a full argv (`argv[0]` = program name) into a session config and,
/// for one-shot invocation, the single command to run.
///
/// `Err(ArgsError::Help)` means usage was requested (`--help`, `help`, or no
/// arguments at all); every other failure is `ArgsError::Invalid`.
pub fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Result<ParsedArgs, ArgsError> {
    let mut argv = argv.into_iter();
    let program = argv.next().unwrap_or_else(|| "navigera".to_string());
    let program =
        std::path::Path::new(&program).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(program);
    let mut args: Args = argv.collect();
    let mut config = SessionConfig::default();
    // `--engine` wins; the variable sets it for every command of a shell
    // (an agent never has to repeat the flag).
    if let Some(engine) = std::env::var("NAVIGERA_ENGINE").ok().filter(|e| !e.trim().is_empty()) {
        config.engine = engine.trim().to_string();
    }

    // Leading globals (the only place --timeout-ms is global; after a
    // command it is that op's own timeout).
    while args.front().is_some_and(|a| a.starts_with('-')) {
        let flag = args.pop_front().expect("front checked");
        let (flag, inline) = match flag.split_once('=') {
            Some((f, v)) if f.starts_with("--") || f == "-s" => (f.to_string(), Some(v.to_string())),
            _ => (flag, None),
        };
        let mut value = |name: &str| -> Result<String, ArgsError> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => next_value(&mut args, name).map_err(ArgsError::Invalid),
            }
        };
        match flag.as_str() {
            "--engine" => config.engine = value("--engine")?,
            "--headed" => config.headed = true,
            "--chromium" => config.chromium = Some(value("--chromium")?),
            "--timeout-ms" => {
                let raw = value("--timeout-ms")?;
                config.timeout_ms = parse_f64(&raw, "--timeout-ms").map_err(ArgsError::Invalid)?;
            }
            "--pretty" => config.pretty = true,
            "--raw" => config.raw = true,
            "--transport" => config.transport = Some(value("--transport")?),
            "--session" | "-s" => config.session = Some(value("--session")?),
            "--attach" => {
                config.attach = Some(match &inline {
                    Some(v) => v.clone(),
                    None => match args.front() {
                        Some(next) if !next.starts_with('-') && !is_command_word(next) => {
                            args.pop_front().expect("front checked")
                        }
                        _ => DEFAULT_ATTACH.to_string(),
                    },
                })
            }
            "--profile" => config.profile = Some(value("--profile")?),
            "--headless" => config.headless = true,
            "--from" => config.from = Some(value("--from")?),
            "--from-profile" => config.from_profile = Some(value("--from-profile")?),
            "--natural" => config.natural = true,
            "--download-dir" => config.download_dir = Some(caller_path(value("--download-dir")?)),
            "--wait-ms" => {
                let raw = value("--wait-ms")?;
                config.wait_ms = Some(parse_f64(&raw, "--wait-ms").map_err(ArgsError::Invalid)?);
            }
            "--idle-timeout-s" => {
                let raw = value("--idle-timeout-s")?;
                config.idle_timeout_s = raw
                    .parse::<u64>()
                    .map_err(|_| ArgsError::Invalid(format!("--idle-timeout-s must be whole seconds, got {raw:?}")))?;
            }
            "-V" | "--version" => {
                return Ok(ParsedArgs { config, command: None, start: false, local: Some(Local::Version), mcp: false })
            }
            "-h" | "--help" => return Err(ArgsError::Help(usage(&program))),
            other => {
                return invalid(format!("unknown flag {other:?} (see `{program} --help`)"));
            }
        }
    }

    let Some(op) = args.pop_front() else {
        return Err(ArgsError::Help(usage(&program)));
    };
    if op == "-h" || op == "--help" || op == "help" {
        return match args.front() {
            Some(topic) => op_help(&program, topic)
                .map(ArgsError::Help)
                .map(Err)
                .unwrap_or_else(|| invalid(format!("no command {topic:?}; see `{program} --help`"))),
            None => Err(ArgsError::Help(usage(&program))),
        };
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        if let Some(help) = op_help(&program, &op) {
            return Err(ArgsError::Help(help));
        }
    }
    consume_globals(&mut args, &mut config).map_err(ArgsError::Invalid)?;
    let parsed = |config, command, start, local| Ok(ParsedArgs { config, command, start, local, mcp: false });

    match op.as_str() {
        "serve" | "start" => {
            if let Some(raw) = str_flag(&mut args, "--timeout-ms")? {
                config.timeout_ms = parse_f64(&raw, "--timeout-ms").map_err(ArgsError::Invalid)?;
            }
            if let Some(extra) = args.front() {
                return invalid(format!("unexpected argument {extra:?} after {op}"));
            }
            return parsed(config, None, op == "start", None);
        }
        "mcp" => {
            if let Some(raw) = str_flag(&mut args, "--timeout-ms")? {
                config.timeout_ms = parse_f64(&raw, "--timeout-ms").map_err(ArgsError::Invalid)?;
            }
            if let Some(extra) = args.front() {
                return invalid(format!("unexpected argument {extra:?} after mcp"));
            }
            return Ok(ParsedArgs { config, command: None, start: false, local: None, mcp: true });
        }
        "skill" => return parsed(config, None, false, Some(Local::Skill)),
        "profiles" => {
            if let Some(extra) = args.front() {
                return invalid(format!("unexpected argument {extra:?} after profiles"));
            }
            return parsed(config, None, false, Some(Local::Profiles));
        }
        "install-skill" => {
            let claude = extract_present(&mut args, "--claude");
            let continue_ = extract_present(&mut args, "--continue");
            let global = extract_present(&mut args, "--global");
            let dir = match str_flag(&mut args, "--dir")? {
                Some(d) => d,
                None => {
                    // Cline and Kilo read .agents/skills, Claude Code
                    // .claude/skills, Continue .continue/skills.
                    let base = match (claude, continue_) {
                        (true, _) => ".claude/skills",
                        (false, true) => ".continue/skills",
                        (false, false) => ".agents/skills",
                    };
                    if global {
                        let home = std::env::var("HOME")
                            .or_else(|_| std::env::var("USERPROFILE"))
                            .unwrap_or_else(|_| ".".into());
                        format!("{home}/{base}")
                    } else {
                        base.to_string()
                    }
                }
            };
            return parsed(config, None, false, Some(Local::InstallSkill { dir }));
        }
        _ => {}
    }

    let timeout_ms = num_flag(&mut args, "--timeout-ms")?;
    let need = |name: &str, what: &str| {
        ArgsError::Invalid(format!("{op} needs {what}; usage: {}", op_help(&program, name).unwrap_or_default()))
    };
    let command = match op.as_str() {
        "open" => Command::Open {
            url: match str_flag(&mut args, "--url")? {
                Some(u) => Some(u),
                None => positional(&mut args),
            },
        },
        "goto" | "navigate" => Command::Goto {
            url: match str_flag(&mut args, "--url")? {
                Some(u) => u,
                None => positional(&mut args).ok_or_else(|| need("goto", "a URL"))?,
            },
            wait: str_flag(&mut args, "--wait")?,
            timeout_ms,
        },
        "back" => Command::Back {},
        "forward" => Command::Forward {},
        "reload" => Command::Reload {},
        "click" => {
            let button = str_flag(&mut args, "--button")?;
            if let Some(b) = &button {
                if !matches!(b.as_str(), "left" | "right" | "middle") {
                    return invalid(format!("click --button takes left, right or middle, got {b:?}"));
                }
            }
            let force = extract_present(&mut args, "--force");
            Command::Click { target: target_args(&mut args, true)?, timeout_ms, button, force }
        }
        "hover" => Command::Hover { target: target_args(&mut args, true)?, timeout_ms },
        "mouse" => {
            let action = positional(&mut args).ok_or_else(|| need("mouse", "move, down, up, click or wheel"))?;
            if !matches!(action.as_str(), "move" | "down" | "up" | "click" | "wheel") {
                return Err(ArgsError::Invalid(format!("mouse takes move, down, up, click or wheel, not {action:?}")));
            }
            let button = str_flag(&mut args, "--button")?;
            let count = num_flag(&mut args, "--count")?.map(|v| v as u32);
            let steps = num_flag(&mut args, "--steps")?.map(|v| v as u32);
            let target = target_args(&mut args, false)?;
            let mut nums = Vec::new();
            if matches!(action.as_str(), "move" | "click" | "wheel") {
                for what in ["x", "y"] {
                    let raw = positional(&mut args)
                        .ok_or_else(|| need("mouse", &format!("{action} <x> <y> (got no {what})")))?;
                    nums.push(parse_f64(&raw, &format!("mouse {action} {what}")).map_err(ArgsError::Invalid)?);
                }
            }
            Command::Mouse {
                action,
                x: nums.first().copied(),
                y: nums.get(1).copied(),
                button,
                count,
                steps,
                target,
                timeout_ms,
            }
        }
        "fill" => {
            let keys = extract_present(&mut args, "--keys");
            let target = target_args(&mut args, true)?;
            let value = match str_flag(&mut args, "--value")? {
                Some(v) => v,
                None => {
                    positional(&mut args).ok_or_else(|| need("fill", "a value (`fill <ref> <value>` or --value)"))?
                }
            };
            Command::Fill { target, value, timeout_ms, keys }
        }
        "select" => {
            let add = extract_present(&mut args, "--add");
            let target = target_args(&mut args, true)?;
            let mut value = extract_values(&mut args, "--value").map_err(ArgsError::Invalid)?;
            while let Some(v) = positional(&mut args) {
                value.push(v);
            }
            if value.is_empty() {
                return Err(need("select", "an option value or label"));
            }
            Command::Select { target, value, timeout_ms, add }
        }
        "press" => {
            let key = match str_flag(&mut args, "--key")? {
                Some(k) => k,
                None => positional(&mut args).ok_or_else(|| need("press", "a key (Enter, Tab, Control+a, …)"))?,
            };
            Command::Press { key, target: target_args(&mut args, false)?, timeout_ms }
        }
        "type" => {
            let value = match str_flag(&mut args, "--value")? {
                Some(v) => v,
                None => positional(&mut args).ok_or_else(|| need("type", "the text to type"))?,
            };
            Command::Type { value, target: target_args(&mut args, false)?, timeout_ms }
        }
        "scroll" => Command::Scroll {
            by: num_flag(&mut args, "--by")?,
            to: str_flag(&mut args, "--to")?,
            target: target_args(&mut args, true)?,
            timeout_ms,
        },
        "upload" => {
            let target = target_args(&mut args, true)?;
            let mut files = extract_values(&mut args, "--file").map_err(ArgsError::Invalid)?;
            while let Some(f) = positional(&mut args) {
                files.push(f);
            }
            if files.is_empty() {
                return Err(need("upload", "a file path"));
            }
            Command::Upload { target, files, timeout_ms }
        }
        "wait" => {
            let mut cmd = Command::Wait {
                selector: str_flag(&mut args, "--selector")?,
                text: str_flag(&mut args, "--text")?,
                url: str_flag(&mut args, "--url")?,
                gone: str_flag(&mut args, "--gone")?,
                js: str_flag(&mut args, "--js")?,
                frame: str_flag(&mut args, "--frame")?,
                ms: num_flag(&mut args, "--ms")?,
                idle: extract_present(&mut args, "--idle")
                    || match str_flag(&mut args, "--load")?.as_deref() {
                        None => false,
                        Some("networkidle" | "idle") => true,
                        Some(other) => return invalid(format!("wait --load takes networkidle, got {other:?}")),
                    },
                timeout_ms,
                download: match args.iter().position(|a| a == "--download") {
                    Some(i) => {
                        args.remove(i);
                        match args.get(i) {
                            Some(next) if !next.starts_with('-') => args.remove(i),
                            _ => Some(String::new()),
                        }
                    }
                    None => None,
                },
            };
            if let (Some(p), Command::Wait { selector, ms, .. }) = (positional(&mut args), &mut cmd) {
                match p.parse::<f64>() {
                    Ok(n) => *ms = Some(n),
                    Err(_) => *selector = Some(p),
                }
            }
            if matches!(
                &cmd,
                Command::Wait {
                    selector: None,
                    text: None,
                    url: None,
                    gone: None,
                    js: None,
                    ms: None,
                    idle: false,
                    download: None,
                    ..
                }
            ) {
                return Err(need("wait", "a condition"));
            }
            cmd
        }
        "dialog" => {
            let accept = extract_present(&mut args, "--accept");
            let dismiss = extract_present(&mut args, "--dismiss");
            if accept == dismiss {
                return Err(need("dialog", "exactly one of --accept or --dismiss"));
            }
            Command::Dialog { accept: Some(accept), prompt_text: str_flag(&mut args, "--prompt-text")? }
        }
        "text" => Command::Text {
            selector: match str_flag(&mut args, "--selector")? {
                Some(s) => s,
                None => positional(&mut args).ok_or_else(|| need("text", "--selector <css>"))?,
            },
            timeout_ms,
        },
        "find" => Command::Find {
            what: rest_words(&mut args).ok_or_else(|| need("find", "a description of the element"))?,
            timeout_ms,
        },
        // `check <target>` checks a checkbox; `check <question>` asks the
        // decision model. A target is a ref or a targeting flag.
        "check" | "uncheck"
            if op == "uncheck"
                || args.iter().any(|a| {
                    matches!(
                        a.as_str(),
                        "--selector"
                            | "--ref"
                            | "--text"
                            | "--find"
                            | "--role"
                            | "--name"
                            | "--label"
                            | "--placeholder"
                            | "--testid"
                            | "--nth"
                            | "--frame"
                    )
                })
                || (args.len() == 1 && parse_ref(&args[0]).is_some()) =>
        {
            Command::SetChecked { checked: op == "check", target: target_args(&mut args, true)?, timeout_ms }
        }
        "check" => Command::Check {
            question: rest_words(&mut args).ok_or_else(|| need("check", "a yes/no question about the page"))?,
            timeout_ms,
        },
        "get" => {
            let what = positional(&mut args).ok_or_else(|| need("get", "what to read"))?;
            match what.as_str() {
                "count" => Command::Get {
                    what,
                    name_of: None,
                    target: TargetArgs {
                        selector: Some(match str_flag(&mut args, "--selector")? {
                            Some(s) => s,
                            None => positional(&mut args).ok_or_else(|| need("get", "a CSS selector to count"))?,
                        }),
                        ..TargetArgs::default()
                    },
                    timeout_ms,
                },
                "title" => Command::Title {},
                "url" => Command::Url {},
                "attr" | "style" => {
                    let name_of =
                        Some(positional(&mut args).ok_or_else(|| need("get", "the attribute or CSS property name"))?);
                    Command::Get { what, name_of, target: target_args(&mut args, true)?, timeout_ms }
                }
                _ => Command::Get { what, name_of: None, target: target_args(&mut args, true)?, timeout_ms },
            }
        }
        "is" => Command::Is {
            state: positional(&mut args).ok_or_else(|| need("is", "a state (visible, enabled, checked, …)"))?,
            target: target_args(&mut args, true)?,
            timeout_ms,
        },
        "dblclick" => Command::Dblclick { target: target_args(&mut args, true)?, timeout_ms },
        "drag" | "drag-drop" => {
            let side = |text: Option<String>, args: &mut Args| -> Result<TargetArgs, ArgsError> {
                if text.is_some() {
                    return Ok(TargetArgs { text, ..TargetArgs::default() });
                }
                let raw = positional(args).ok_or_else(|| need("drag", "<from> <to> (refs or selectors)"))?;
                Ok(match parse_ref(&raw) {
                    Some(r) => TargetArgs { node_ref: Some(r), ..TargetArgs::default() },
                    None => TargetArgs { selector: Some(raw), ..TargetArgs::default() },
                })
            };
            let (from_text, to_text) = (str_flag(&mut args, "--from-text")?, str_flag(&mut args, "--to-text")?);
            let from = side(from_text, &mut args)?;
            let to = side(to_text, &mut args)?;
            Command::Drag { from, to, timeout_ms }
        }
        "focus" => Command::Focus { target: target_args(&mut args, true)?, timeout_ms },
        "keydown" | "keyup" => {
            let key = positional(&mut args).ok_or_else(|| need(&op, "a key"))?;
            if op == "keydown" {
                Command::KeyDown { key }
            } else {
                Command::KeyUp { key }
            }
        }
        "insert-text" => {
            let value = match str_flag(&mut args, "--value")? {
                Some(v) => v,
                None => positional(&mut args).ok_or_else(|| need("insert-text", "the text"))?,
            };
            Command::InsertText { value, target: target_args(&mut args, true)?, timeout_ms }
        }
        "eval" | "evaluate" => {
            // `--file <path>` and `-` (stdin) keep the script away from the
            // shell's quoting rules (PowerShell mangles quotes and backslashes).
            let expression = match (str_flag(&mut args, "--file")?, str_flag(&mut args, "--expression")?) {
                (Some(path), None) => std::fs::read_to_string(&path)
                    .map_err(|e| ArgsError::Invalid(format!("eval --file {path}: {e}")))?,
                (None, Some(e)) => e,
                (Some(_), Some(_)) => return invalid("eval takes --file or an expression, not both"),
                (None, None) => match positional(&mut args) {
                    Some(dash) if dash == "-" => {
                        let mut script = String::new();
                        io::Read::read_to_string(&mut io::stdin(), &mut script)
                            .map_err(|e| ArgsError::Invalid(format!("eval -: read stdin: {e}")))?;
                        script
                    }
                    Some(e) => e,
                    None => return Err(need("eval", "a JS expression, --file <path> or - (stdin)")),
                },
            };
            Command::Eval { expression: expression.trim_start_matches('\u{feff}').to_string(), timeout_ms }
        }
        "title" => Command::Title {},
        "ax" | "snapshot" => Command::Ax {
            max_depth: num_flag(&mut args, "--max-depth")?.map(|v| v as u32),
            all: extract_present(&mut args, "--all").then_some(true),
            // The CLI defaults to the text tree; the serve protocol keeps
            // its original JSON list when `format` is absent.
            format: Some(str_flag(&mut args, "--format")?.unwrap_or_else(|| "text".into())),
            limit: num_flag(&mut args, "--limit")?.map(|v| v as usize),
            refs: str_flag(&mut args, "--refs")?,
            scope: target_args(&mut args, true)?,
            timeout_ms,
        },
        "console" => Command::Console {
            level: str_flag(&mut args, "--level")?,
            limit: num_flag(&mut args, "--limit")?.map(|v| v as usize),
            clear: extract_present(&mut args, "--clear"),
            format: extract_present(&mut args, "--json").then(|| "json".to_string()),
        },
        "errors" => Command::Errors {
            limit: num_flag(&mut args, "--limit")?.map(|v| v as usize),
            clear: extract_present(&mut args, "--clear"),
            format: extract_present(&mut args, "--json").then(|| "json".to_string()),
        },
        "downloads" => Command::Downloads {
            clear: extract_present(&mut args, "--clear"),
            format: extract_present(&mut args, "--json").then(|| "json".to_string()),
        },
        "network" => Command::Network {
            filter: str_flag(&mut args, "--filter")?,
            kind: str_flag(&mut args, "--type")?,
            failed: extract_present(&mut args, "--failed"),
            limit: num_flag(&mut args, "--limit")?.map(|v| v as usize),
            clear: extract_present(&mut args, "--clear"),
            har: str_flag(&mut args, "--har")?,
            body_file: str_flag(&mut args, "--body-file")?,
            since: num_flag(&mut args, "--since")?.map(|v| v as u64),
            n: match positional(&mut args) {
                Some(raw) => Some(
                    raw.trim_start_matches('#')
                        .parse::<u64>()
                        .map_err(|_| ArgsError::Invalid(format!("network: {raw:?} is not a request number")))?,
                ),
                None => None,
            },
        },
        "cookies" | "cookie" => {
            let action = match args.front().map(String::as_str) {
                Some("set" | "delete" | "clear" | "list" | "get") => args.pop_front(),
                _ => None,
            };
            let mut cmd = Command::Cookies {
                action: action.map(|a| if a == "get" { "list".into() } else { a }),
                name: None,
                value: None,
                url: str_flag(&mut args, "--url")?,
                domain: str_flag(&mut args, "--domain")?,
                path: str_flag(&mut args, "--path")?,
                expires: num_flag(&mut args, "--expires")?,
                http_only: extract_present(&mut args, "--http-only"),
                secure: extract_present(&mut args, "--secure"),
                same_site: str_flag(&mut args, "--same-site")?,
                all: extract_present(&mut args, "--all"),
                format: extract_present(&mut args, "--json").then(|| "json".to_string()),
            };
            if let Command::Cookies { action, name, value, .. } = &mut cmd {
                match action.as_deref() {
                    Some("set") => {
                        *name = Some(positional(&mut args).ok_or_else(|| need("cookies", "a cookie name"))?);
                        *value = Some(positional(&mut args).ok_or_else(|| need("cookies", "a cookie value"))?);
                    }
                    Some("delete") => {
                        *name = Some(positional(&mut args).ok_or_else(|| need("cookies", "a cookie name"))?);
                    }
                    _ => {}
                }
            }
            cmd
        }
        "state" => {
            let action = positional(&mut args).ok_or_else(|| need("state", "save or load"))?;
            if action != "save" && action != "load" {
                return Err(ArgsError::Invalid(format!("state takes save|load, got {action:?}")));
            }
            let path = positional(&mut args).ok_or_else(|| need("state", "a file"))?;
            Command::State { action, path }
        }
        "pdf" => Command::Pdf {
            path: match str_flag(&mut args, "--path")? {
                Some(p) => Some(p),
                None => positional(&mut args),
            },
            landscape: extract_present(&mut args, "--landscape"),
            background: extract_present(&mut args, "--background"),
        },
        "storage" => {
            let area = match args.front().map(String::as_str) {
                Some("local" | "session") => args.pop_front(),
                _ => None,
            };
            let action = match args.front().map(String::as_str) {
                Some("get" | "set" | "remove" | "clear") => args.pop_front(),
                _ => None,
            };
            let key = positional(&mut args);
            let value = positional(&mut args);
            match (action.as_deref(), &key, &value) {
                (Some("set"), Some(_), Some(_)) | (Some("remove"), Some(_), None) | (Some("clear"), None, None) => {}
                (None | Some("get"), _, None) => {}
                _ => return Err(need("storage", "`get [<key>]`, `set <key> <value>`, `remove <key>` or `clear`")),
            }
            Command::Storage { area, action, key, value }
        }
        "set" | "emulate" => {
            let what = positional(&mut args).ok_or_else(|| need("set", "what to set"))?;
            let scale = num_flag(&mut args, "--scale")?;
            let mobile = extract_present(&mut args, "--mobile");
            let accuracy = num_flag(&mut args, "--accuracy")?;
            let mut values = Vec::new();
            while let Some(v) = positional(&mut args) {
                values.push(v);
            }
            Command::Set { what, values, scale, mobile, accuracy }
        }
        "route" => Command::Route {
            abort: extract_present(&mut args, "--abort"),
            status: num_flag(&mut args, "--status")?.map(|v| v as u16),
            body: str_flag(&mut args, "--body")?,
            body_file: str_flag(&mut args, "--body-file")?,
            content_type: str_flag(&mut args, "--content-type")?,
            headers: extract_values(&mut args, "--header").map_err(ArgsError::Invalid)?,
            pattern: positional(&mut args),
        },
        "unroute" => Command::Unroute { pattern: positional(&mut args) },
        "init-script" | "addinitscript" => {
            let clear = extract_present(&mut args, "--clear");
            let source = match str_flag(&mut args, "--file")? {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .map_err(|e| ArgsError::Invalid(format!("init-script --file {path}: {e}")))?,
                ),
                None => positional(&mut args),
            };
            if clear && source.is_some() {
                return invalid("init-script takes a script or --clear, not both");
            }
            Command::InitScript { source, clear }
        }
        "url" => Command::Url {},
        "status" => Command::Status {},
        // Mostly for sessions (`--session s quit` stops the server); as a
        // one-shot it just launches and closes.
        "quit" | "close" => Command::Quit { close_browser: extract_present(&mut args, "--close-browser") },
        "tab-list" => Command::TabList {},
        "show" => Command::Show {},
        "hide" => Command::Hide {},
        "reset" => Command::Reset { all: extract_present(&mut args, "--all") },
        "tab-new" => Command::TabNew {
            url: match str_flag(&mut args, "--url")? {
                Some(u) => Some(u),
                None => positional(&mut args),
            },
        },
        "tab-select" => match str_flag(&mut args, "--index")?.or_else(|| positional(&mut args)) {
            Some(raw) => match parse_index(&raw) {
                Ok(index) => Command::TabSelect { index, target: None },
                Err(_) if raw.len() >= 4 && raw.chars().all(|c| c.is_ascii_hexdigit()) => {
                    Command::TabSelect { index: 0, target: Some(raw) }
                }
                Err(e) => return invalid(e),
            },
            None => return Err(need("tab-select", "a tab index or target id")),
        },
        "tab-close" | "close-page" => {
            let index = str_flag(&mut args, "--index")?
                .or_else(|| positional(&mut args))
                .map(|raw| parse_index(&raw))
                .transpose()
                .map_err(ArgsError::Invalid)?;
            if op == "tab-close" {
                Command::TabClose { index }
            } else {
                Command::ClosePage { index }
            }
        }
        "screenshot" => Command::Screenshot {
            path: match str_flag(&mut args, "--path")? {
                Some(p) => Some(p),
                None => positional(&mut args),
            },
            full_page: extract_present(&mut args, "--full-page").then_some(true),
        },
        unknown => {
            let hint = suggest(unknown).map(|s| format!(" — did you mean `{s}`?")).unwrap_or_default();
            return invalid(format!(
                "unknown command {unknown:?}{hint}\ncommands: {}\n(`{program} --help` for usage)",
                OPS.iter().map(|(n, ..)| *n).collect::<Vec<_>>().join(", ")
            ));
        }
    };
    if let Some(extra) = args.front() {
        let hint = if extra.starts_with("--") {
            format!("; usage: {}", op_help(&program, &op).unwrap_or_default())
        } else {
            " (quote arguments that contain spaces)".to_string()
        };
        return invalid(format!("unexpected argument {extra:?} for `{op}`{hint}"));
    }
    // `--wait-ms` on one command: that op's element wait, unless it names
    // its own timeout.
    let command = match config.wait_ms {
        Some(ms) if command.clone().target_mut().is_some() && op_timeout_ms(&command).is_none() => {
            let mut v = serde_json::to_value(&command).map_err(|e| ArgsError::Invalid(e.to_string()))?;
            v["timeout_ms"] = json!(ms);
            serde_json::from_value(v).unwrap_or(command)
        }
        _ => command,
    };
    parsed(config, Some(command), false, None)
}

/// Install the embedded skill into `<dir>/navigera/SKILL.md`.
pub fn install_skill(dir: &str) -> anyhow::Result<String> {
    let path = std::path::Path::new(dir).join("navigera");
    std::fs::create_dir_all(&path)?;
    let file = path.join("SKILL.md");
    std::fs::write(&file, SKILL_MD)?;
    Ok(file.display().to_string())
}

/// A live browser plus the policy defaults the model gets for free.
///
/// Construct once per session; every [`Command`] runs against the same warm
/// Chrome (no relaunch between ops).
/// A tree longer than this, read while a decision model is on, is shown as a
/// preview of its first [`AX_PREVIEW_LINES`] lines, with a note that acting
/// does not need the rest.
const AX_DECIDER_LINES: usize = 60;

/// The lines a plain `ax` shows of a large tree while a decision model is on.
///
/// The note alone, after the whole tree, came too late: the agent had read
/// the tree by the time it was told it need not, and in 12 runs it reached
/// for `--find` or `check` unasked in 3. The preview makes the cheap path the
/// default one; `ax --limit <n>` (or a scope) still reads everything.
const AX_PREVIEW_LINES: usize = 40;

/// How large a tree is before a plain `ax` shows a preview of it
/// (`NAVIGERA_AX_PREVIEW_BYTES`, default 16 KB, `0` for never).
///
/// Size, not length: the pages where deciding paid were documentation
/// indexes of 56 and 95 KB, where `--find` cut the agent's tokens by a third
/// (eval run 39733028440). A 170-line form is 8 KB, read once for every ref
/// it needs; previewed at a line count, the agent went around the preview and
/// spent 63% more.
fn ax_preview_bytes() -> usize {
    std::env::var("NAVIGERA_AX_PREVIEW_BYTES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(16 * 1024)
}

/// The first `keep` lines of `tree`, and how many lines were left out.
fn ax_preview(tree: &str, keep: usize) -> (String, usize) {
    let total = tree.lines().count();
    if total <= keep {
        return (tree.to_string(), 0);
    }
    let shown: Vec<&str> = tree.lines().take(keep).collect();
    (shown.join("\n"), total - keep)
}

pub struct Driver {
    session: BrowserSession,
}

/// One op's result plus side effects the agent should know about.
pub struct Outcome {
    pub result: Value,
    pub dialogs: Vec<Value>,
    pub new_tabs: Vec<usize>,
    pub notes: Vec<String>,
}

impl Driver {
    /// Launch the engine described by `config` (Chrome by default).
    pub fn launch(config: &SessionConfig) -> anyhow::Result<Self> {
        crate::browser::set_natural(config.natural);
        crate::browser::set_element_wait(config.wait_ms);
        crate::browser::set_download_dir(config.download_dir.as_deref());
        if let Some(target) = config.attach_target() {
            let session = BrowserSession::attach(&target, config.timeout_ms)
                .map_err(|e| anyhow::anyhow!("could not connect to the browser: {e:#}"))?;
            return Ok(Self { session });
        }
        let session = BrowserSession::launch_with(
            &config.engine,
            !config.headed,
            config.chromium.as_deref(),
            config.timeout_ms,
            config.transport.as_deref(),
        )
        .map_err(|e| {
            anyhow::anyhow!("browser launch failed (engine={}, headed={}): {e:#}", config.engine, config.headed)
        })?;
        Ok(Self { session })
    }

    /// The active page (tab) this driver's ops target.
    pub fn session(&self) -> &BrowserSession {
        &self.session
    }

    /// Another driver on the same browser, in a browser context of its own
    /// (see [`BrowserSession::fork`]): one browser for several sessions at
    /// once.
    pub fn fork(&self) -> anyhow::Result<Self> {
        Ok(Self { session: self.session.fork()? })
    }

    /// Active tab state the model needs after every op.
    fn state_json(&self) -> Value {
        let (url, title) = self.session.url_title();
        let mut state = json!({
            "tab": self.session.active_page(),
            "tabs": self.session.page_count(),
            "url": url,
            "title": title,
        });
        // The page is still being parsed (we stopped waiting for it): the
        // agent should `wait` for what it needs rather than trust `ax`.
        if self.session.is_loading() {
            state["loading"] = json!(true);
        }
        state
    }

    /// Tab inventory for `tab-list` / `tab-new` results.
    /// `console` / `errors`: the active tab's log at `min` level or above.
    fn console_result(&self, min: u8, limit: Option<usize>, clear: bool, format: Option<&str>) -> Value {
        let rank = |level: &str| match level {
            "debug" => 0,
            "log" => 1,
            "info" => 2,
            "warn" => 3,
            _ => 4,
        };
        let all = self.session.console(clear);
        let mut picked: Vec<_> = all.into_iter().filter(|e| rank(&e.level) >= min).collect();
        let limit = limit.unwrap_or(200);
        if picked.len() > limit {
            picked.drain(..picked.len() - limit);
        }
        if format == Some("json") {
            return crate::cdp::logs::console_json(&picked);
        }
        if picked.is_empty() {
            return json!(if min >= 4 { "(no errors)" } else { "(no console messages)" });
        }
        json!(crate::cdp::logs::console_text(&picked))
    }

    /// `network`: the request list, one request in full, or a HAR file.
    #[allow(clippy::too_many_arguments)]
    fn network(
        &self,
        n: Option<u64>,
        filter: Option<&str>,
        kind: Option<&str>,
        failed: bool,
        limit: Option<usize>,
        clear: bool,
        har: Option<&str>,
        body_file: Option<&str>,
        since: Option<u64>,
    ) -> anyhow::Result<Value> {
        let all = self.session.requests(clear && n.is_none());
        if let Some(n) = n {
            let r = all
                .iter()
                .find(|r| r.n == n)
                .ok_or_else(|| anyhow::anyhow!("network: no request #{n} (older ones are dropped after 1000)"))?;
            let mut out = crate::cdp::logs::request_json(r);
            if r.post_data.is_none() && r.has_post_data {
                if let Ok(data) = self.session.request_post_data(&r.id) {
                    out["post_data"] = json!(data);
                }
            }
            match self.session.response_body(&r.id) {
                Ok((body, base64)) => {
                    let bytes = if base64 {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.decode(&body).unwrap_or_default()
                    } else {
                        body.clone().into_bytes()
                    };
                    if let Some(path) = body_file {
                        std::fs::write(path, &bytes).map_err(|e| anyhow::anyhow!("network --body-file {path}: {e}"))?;
                        out["body_file"] = json!(path);
                        out["body_bytes"] = json!(bytes.len());
                    } else if base64 {
                        out["body"] =
                            json!(format!("({} bytes of binary data; --body-file <path> saves it)", bytes.len()));
                    } else {
                        const MAX: usize = 8 * 1024;
                        let text = String::from_utf8_lossy(&bytes);
                        out["body"] = if text.len() > MAX {
                            let cut = text
                                .char_indices()
                                .take_while(|(i, _)| *i < MAX)
                                .last()
                                .map(|(i, c)| i + c.len_utf8())
                                .unwrap_or(0);
                            json!(format!("{}… ({} bytes; --body-file <path> saves all)", &text[..cut], bytes.len()))
                        } else {
                            json!(text)
                        };
                    }
                }
                Err(e) => out["body"] = json!(format!("(unavailable: {e:#})")),
            }
            return Ok(out);
        }
        let kind = kind.map(str::to_ascii_lowercase);
        let mut picked: Vec<_> = all
            .into_iter()
            .filter(|r| filter.is_none_or(|f| r.url.contains(f)))
            .filter(|r| kind.as_ref().is_none_or(|k| r.kind.to_ascii_lowercase() == *k))
            .filter(|r| !failed || r.failed.is_some() || r.status.is_some_and(|s| s >= 400))
            .filter(|r| since.is_none_or(|n| r.n > n))
            .collect();
        if let Some(path) = har {
            let har = crate::cdp::logs::har(&picked, env!("CARGO_PKG_VERSION"));
            std::fs::write(path, serde_json::to_vec_pretty(&har)?)
                .map_err(|e| anyhow::anyhow!("network --har {path}: {e}"))?;
            return Ok(json!({ "har": path, "entries": picked.len() }));
        }
        let limit = limit.unwrap_or(100);
        if picked.len() > limit {
            picked.drain(..picked.len() - limit);
        }
        if picked.is_empty() {
            return Ok(json!("(no requests)"));
        }
        Ok(json!(crate::cdp::logs::requests_text(&picked, &origin_of(&self.session.url()))))
    }

    /// `show` / `hide`: the same session in a browser on screen, or off it.
    /// A `--profile` browser restarts on its own profile, which keeps
    /// everything; a browser navigera launched is replaced by one carrying its
    /// cookies and the open tabs' storage (`state save` / `load`). Either way
    /// the tabs reopen at their pages and the active one stays active.
    fn relaunch(&mut self, visible: bool) -> anyhow::Result<Value> {
        let what = if visible { "show" } else { "hide" };
        if self.session.is_lightpanda() {
            anyhow::bail!("{what}: Lightpanda has no window; use the chrome engine");
        }
        if self.session.is_attached() && self.session.profile_dir().is_none() {
            anyhow::bail!("{what}: this is your own browser (--attach); it is already where you see it");
        }
        let urls = self.session.tab_urls();
        let active = self.session.active_page();
        let exe = self.session.profile_executable().unwrap_or(self.session.executable()).to_string();
        let timeout_ms = self.session.timeout_ms();
        let new = match self.session.profile_dir() {
            Some(dir) => {
                self.session.close_browser()?;
                let target = crate::browser::AttachTarget::Profile {
                    name: dir.display().to_string(),
                    executable: Some(exe),
                    headless: !visible,
                    from: None,
                    from_profile: None,
                };
                BrowserSession::attach(&target, timeout_ms)
            }
            None => {
                let state = self.session.storage_state()?;
                let mut new = BrowserSession::launch_with("chrome", !visible, Some(&exe), timeout_ms, None);
                if let Ok(new) = &mut new {
                    new.load_storage_state(&state)?;
                }
                new
            }
        }
        .map_err(|e| {
            anyhow::anyhow!(
                "{what}: could not start the browser {}: {e:#}",
                if visible { "on screen (is there a display?)" } else { "headless" }
            )
        })?;
        let old = std::mem::replace(&mut self.session, new);
        if !old.is_attached() {
            old.close();
        }
        for (i, url) in urls.iter().enumerate() {
            if i > 0 {
                self.session.new_page().map_err(|e| anyhow::anyhow!("{what}: reopen tab {i}: {e:#}"))?;
            }
            if !url.is_empty() && url != "about:blank" {
                let _ = self.session.goto(url);
            }
        }
        if active < urls.len() {
            let _ = self.session.select_page(active);
        }
        let mut state = self.state_json();
        state["visible"] = json!(visible);
        Ok(state)
    }

    fn tabs_json(&self) -> Value {
        let active = self.session.active_page();
        let tabs: Vec<Value> = self
            .session
            .page_targets()
            .into_iter()
            .enumerate()
            .map(|(index, target)| json!({ "index": index, "target": target, "active": index == active }))
            .collect();
        json!({ "tabs": tabs, "active": active, "url": self.session.url() })
    }

    /// Run one op; returns the JSON `result` value.
    pub fn run(&mut self, command: &Command) -> anyhow::Result<Value> {
        self.run_op(command).map(|o| o.result)
    }

    /// Run one op, reporting dialogs answered and tabs opened along the way.
    pub fn run_op(&mut self, command: &Command) -> anyhow::Result<Outcome> {
        let mut new_tabs = self.session.sync_tabs();
        // A navigation an earlier op started (link click, JS redirect) must
        // finish before we read or act on the page — bounded by this op's
        // own timeout, and never fatal: a page that never finishes loading
        // must not wedge the session. Ops that navigate or don't read the
        // page skip it.
        let reads_page = !matches!(
            command,
            Command::Quit { .. }
                | Command::Dialog { .. }
                | Command::TabList {}
                | Command::Show {}
                | Command::Hide {}
                | Command::Status {}
                | Command::Console { .. }
                | Command::Errors { .. }
                | Command::Network { .. }
                | Command::Downloads { .. }
                | Command::Cookies { .. }
                | Command::State { .. }
                | Command::Route { .. }
                | Command::InitScript { .. }
                | Command::Unroute { .. }
                | Command::Set { .. }
                | Command::TabNew { .. }
                | Command::Reset { .. }
                | Command::TabSelect { .. }
                | Command::TabClose { .. }
                | Command::ClosePage { .. }
                | Command::Goto { .. }
                | Command::Open { .. }
                | Command::Reload {}
                | Command::Back {}
                | Command::Forward {}
                | Command::Wait {
                    ms: Some(_),
                    selector: None,
                    text: None,
                    url: None,
                    gone: None,
                    js: None,
                    idle: false,
                    ..
                }
        );
        if reads_page {
            let budget = op_timeout_ms(command)
                .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0))
                .unwrap_or_else(|| self.session.nav_timeout());
            self.session.settle_within(budget);
            // Snapshots read what the page shows: give data it is still
            // fetching (an SPA's "Loading…" list) a moment to land, so the
            // agent doesn't spend a turn on a placeholder.
            if matches!(
                command,
                Command::Ax { .. } | Command::Screenshot { .. } | Command::Find { .. } | Command::Check { .. }
            ) || command.find_target().is_some()
            {
                self.session.network_quiet(NETWORK_QUIET, budget.min(NETWORK_QUIET_MAX));
            }
        }
        // `--find <description>`: the decision model picks the element and the
        // op runs on its ref; the result says which one it was.
        let mut found = None;
        let resolved;
        let command = match command.find_target() {
            Some(what) => {
                let f = self.find(&what, op_timeout_ms(command))?;
                let min = find_min();
                if f.p < min {
                    anyhow::bail!(
                        "--find {what:?}: not sure which element that is (best p={:.2} < {min}): {}; pass a ref instead",
                        f.p,
                        std::iter::once((&f.chosen, f.p))
                            .chain(f.others.iter().map(|(c, p)| (c, *p)))
                            .map(|(c, p)| format!("[ref={}] {} ({p:.2})", c.node_ref, c.line))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                resolved = command.with_ref(f.chosen.node_ref);
                found = Some(f);
                &resolved
            }
            None => command,
        };
        let result = self.dispatch(command).map_err(|e| {
            let msg = format!("{e:#}");
            if msg.contains("CDP connection closed") || msg.contains("CDP client is shut down") {
                let how = self.session.browser_exit().unwrap_or_else(|| "connection lost".into());
                anyhow::anyhow!("{msg} — the browser is gone ({how}); run `quit`, then `start` again")
            } else {
                e
            }
        });
        let mut notes = if result.is_ok() { self.session.follow_popups(op_timeout_ms(command)) } else { Vec::new() };
        // Uncaught exceptions while this op ran (a handler that crashed is
        // why a click "did nothing"): one line, `errors` has the rest.
        if !matches!(command, Command::Console { .. } | Command::Errors { .. }) {
            let thrown = self.session.take_exceptions();
            if let Some(first) = thrown.first() {
                let more = if thrown.len() > 1 { format!(" (+{} more)", thrown.len() - 1) } else { String::new() };
                let first: String = first.chars().take(160).collect();
                notes.push(format!("the page threw {first}{more}; `errors` lists them"));
            }
        }
        let opened = self.session.sync_tabs();
        if !opened.is_empty() {
            // The op opened a tab and the session switched to it: let its
            // page load before reporting it (bounded like any settle).
            let budget = op_timeout_ms(command)
                .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0))
                .unwrap_or_else(|| self.session.nav_timeout());
            self.session.settle_within(budget);
        }
        new_tabs.extend(opened);
        let dialogs = self.session.take_dialogs();
        let mut result = match (result, new_tabs.is_empty() && notes.is_empty()) {
            // A click that opened a tab: report the state of the new tab.
            (Ok(r), false) if r.get("tab").is_some() => Ok(self.state_json()),
            (r, _) => r,
        }?;
        if let (Some(f), Some(obj)) = (found, result.as_object_mut()) {
            obj.insert("found".into(), json!({ "ref": f.chosen.node_ref, "element": f.chosen.line, "p": f.p }));
        }
        // A long tree read while a decision model is on: a plain `ax` shows
        // its top, and says that acting needs no more of it. Asked for with a
        // limit, a depth or a scope, the tree is what was asked for, and the
        // note still says acting does not need it.
        if let (Command::Ax { max_depth, all, limit, scope, .. }, Value::String(tree)) = (command, &mut result) {
            let lines = tree.lines().count();
            if lines > AX_DECIDER_LINES && crate::decide::Decider::from_env().is_some() {
                let threshold = ax_preview_bytes();
                let plain = max_depth.is_none()
                    && *all != Some(true)
                    && limit.is_none()
                    && scope.optional("ax").ok().flatten().is_none()
                    && threshold > 0
                    && tree.len() > threshold;
                if plain {
                    let (shown, hidden) = ax_preview(tree, AX_PREVIEW_LINES);
                    *tree = format!("{shown}\n... {hidden} more lines not shown");
                    notes.push(format!(
                        "the first {AX_PREVIEW_LINES} of {lines} lines: with the decision model on, \
                         `click --find \"<what>\"`, `fill --find \"<what>\" <value>` (any element command \
                         takes --find) and `check \"<yes/no question>\"` act and verify on the whole page \
                         without reading it; `ax --limit {DEFAULT_AX_LIMIT}` (MCP: browser_snapshot with limit) \
                         shows every line, for text you must report."
                    ));
                } else {
                    notes.push(format!(
                        "this tree is {lines} lines; with the decision model on, `click --find \"<what>\"`, \
                         `fill --find \"<what>\" <value>` and `check \"<yes/no question>\"` act and verify \
                         without reading it. Read it for text you must report."
                    ));
                }
            }
        }
        Ok(Outcome { result, dialogs, new_tabs, notes })
    }

    /// The page as `find` and `check` see it: the whole text tree.
    fn page_tree(&self, timeout_ms: Option<f64>) -> anyhow::Result<String> {
        let opts = AxOptions { max_depth: None, json: false, all: false, all_refs: false, limit: 20_000, scope: None };
        match self.session.ax(&opts, timeout_ms).map_err(|e| anyhow::anyhow!("ax: {e:#}"))? {
            Value::String(tree) => Ok(tree),
            other => Ok(other.to_string()),
        }
    }

    fn find(&self, what: &str, timeout_ms: Option<f64>) -> anyhow::Result<crate::decide::Found> {
        let decider = crate::decide::Decider::from_env().ok_or_else(|| crate::decide::Decider::missing("find"))?;
        let tree = self.page_tree(timeout_ms)?;
        let page = tree.lines().next().unwrap_or("").to_string();
        crate::decide::find(&decider, what, &page, &crate::decide::candidates(&tree))
            .map_err(|e| anyhow::anyhow!("find {what:?}: {e:#}"))
    }

    fn dispatch(&mut self, command: &Command) -> anyhow::Result<Value> {
        let s = &self.session;
        match command {
            Command::Find { what, timeout_ms } => {
                let f = self.find(what, *timeout_ms)?;
                let mut out = f.to_json();
                out["sure"] = json!(f.p >= find_min());
                Ok(out)
            }
            Command::Check { question, timeout_ms } => {
                let decider =
                    crate::decide::Decider::from_env().ok_or_else(|| crate::decide::Decider::missing("check"))?;
                let tree = self.page_tree(*timeout_ms)?;
                let yes = crate::decide::check(&decider, question, &tree)
                    .map_err(|e| anyhow::anyhow!("check {question:?}: {e:#}"))?;
                Ok(json!({ "question": question, "yes": (yes * 1000.0).round() / 1000.0, "answer": yes >= 0.5 }))
            }
            Command::Open { url } => {
                if let Some(url) = url {
                    self.goto(url, None, None)?;
                }
                Ok(self.state_json())
            }
            Command::Goto { url, wait, timeout_ms } => {
                self.goto(url, wait.as_deref(), *timeout_ms)?;
                Ok(self.state_json())
            }
            Command::Back {} => {
                s.back().map_err(|e| anyhow::anyhow!("back: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Forward {} => {
                s.forward().map_err(|e| anyhow::anyhow!("forward: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Reload {} => {
                s.reload().map_err(|e| anyhow::anyhow!("reload: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Click { target, timeout_ms, button, force } => {
                let t = target.target("click")?;
                let opts = crate::cdp::ClickOptions { button: button.clone().unwrap_or_default(), force: *force };
                let point = s
                    .click_target(&t, &opts, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("click {}: {e:#}", t.label()))?;
                let mut state = self.state_json();
                // Lightpanda lays nothing out, so a DOM-event click there is
                // the normal case, not a sign of an overlay.
                if point.get("synthetic").and_then(Value::as_bool) == Some(true) && !s.is_lightpanda() {
                    // The agent should know the click was not a real one.
                    state["synthetic_click"] = json!(if point.get("dropped").and_then(Value::as_bool) == Some(true) {
                        "the browser did not deliver the mouse events (twice); dispatched DOM events instead"
                            .to_string()
                    } else if point.get("covered").and_then(Value::as_bool) == Some(true) {
                        format!(
                            "element is covered by {}; dispatched DOM events instead (close the overlay if the click had no effect)",
                            point.get("covered_by").and_then(Value::as_str).unwrap_or("another element")
                        )
                    } else {
                        "element has no visible box; dispatched DOM events instead".to_string()
                    });
                }
                if let Some(trace) = point.get("effect_trace") {
                    state["effect_trace"] = trace.clone();
                }
                if let Some(fx) = point.get("no_effect") {
                    // Clicked, but the page did nothing: often a control
                    // the page hasn't wired up yet.
                    let ms = fx.get("watched_ms").and_then(Value::as_u64).unwrap_or(0);
                    let sent: Vec<&str> = fx
                        .get("requests")
                        .and_then(Value::as_array)
                        .map(|r| r.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let only = match sent.len() {
                        0 => String::new(),
                        n if n <= 3 => format!("; it only sent {}", sent.join(", ")),
                        n => format!("; it only sent {} and {} more requests", sent[..2].join(", "), n - 2),
                    };
                    state["no_effect"] = json!(format!(
                        "the click reached the element, but nothing on the page changed in {ms} ms \
                         (DOM, focus, form values, URL, scroll, clipboard, dialogs, tabs, downloads){only}. \
                         The page may not have wired it up yet, or may answer later: check for what the \
                         click should do (`wait --text ...`) before clicking again"
                    ));
                }
                Ok(state)
            }
            Command::Mouse { action, x, y, button, count, steps, target, timeout_ms } => {
                // From a target's top-left corner, when one is named.
                let origin = match target.optional("mouse")? {
                    Some(t) => {
                        let b = s
                            .get_prop(&t, "box", None, *timeout_ms)
                            .map_err(|e| anyhow::anyhow!("mouse {action} {}: {e:#}", t.label()))?;
                        (b["x"].as_f64().unwrap_or(0.0), b["y"].as_f64().unwrap_or(0.0))
                    }
                    None => (0.0, 0.0),
                };
                let (at, delta) = match (action.as_str(), x, y) {
                    ("wheel", Some(dx), Some(dy)) => (None, (*dx, *dy)),
                    (_, Some(x), Some(y)) => (Some((origin.0 + x, origin.1 + y)), (0.0, 0.0)),
                    _ => (None, (0.0, 0.0)),
                };
                let r = s
                    .mouse(
                        action,
                        at,
                        button.as_deref().unwrap_or("left"),
                        count.unwrap_or(1),
                        steps.unwrap_or(1),
                        delta,
                        *timeout_ms,
                    )
                    .map_err(|e| anyhow::anyhow!("mouse {action}: {e:#}"))?;
                let mut state = self.state_json();
                state["mouse"] = r;
                Ok(state)
            }
            Command::Hover { target, timeout_ms } => {
                let t = target.target("hover")?;
                let point = s.hover(&t, *timeout_ms).map_err(|e| anyhow::anyhow!("hover {}: {e:#}", t.label()))?;
                let mut state = self.state_json();
                if point.get("synthetic").and_then(Value::as_bool) == Some(true) {
                    state["synthetic_hover"] = json!(
                        "element is covered or has no box; sent DOM mouseover events only (CSS :hover will not apply)"
                    );
                }
                Ok(state)
            }
            Command::Fill { target, value, timeout_ms, keys } => {
                let t = target.target("fill")?;
                if *keys {
                    s.fill_keys(&t, value, *timeout_ms).map_err(|e| anyhow::anyhow!("fill {}: {e:#}", t.label()))?;
                } else {
                    s.fill_target(&t, value, *timeout_ms).map_err(|e| anyhow::anyhow!("fill {}: {e:#}", t.label()))?;
                }
                Ok(self.state_json())
            }
            Command::Select { target, value, timeout_ms, add } => {
                let t = target.target("select")?;
                let chosen = s
                    .select(&t, value, *add, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("select {}: {e:#}", t.label()))?;
                let mut state = self.state_json();
                state["selected"] = chosen;
                Ok(state)
            }
            Command::Press { key, target, timeout_ms } => {
                let t = target.optional("press")?;
                s.press(t.as_ref(), key, *timeout_ms).map_err(|e| anyhow::anyhow!("press {key}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Type { value, target, timeout_ms } => {
                let t = target.optional("type")?;
                s.type_text(t.as_ref(), value, *timeout_ms).map_err(|e| anyhow::anyhow!("type: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Scroll { target, by, to, timeout_ms } => {
                let t = target.optional("scroll")?;
                s.scroll(t.as_ref(), *by, to.as_deref(), *timeout_ms).map_err(|e| anyhow::anyhow!("scroll: {e:#}"))
            }
            Command::Upload { target, files, timeout_ms } => {
                let t = target.target("upload")?;
                s.upload(&t, files, *timeout_ms).map_err(|e| anyhow::anyhow!("upload {}: {e:#}", t.label()))?;
                Ok(json!({ "files": files.len() }))
            }
            Command::Wait { selector, text, url, gone, js, frame, ms, idle, timeout_ms, download } => {
                if let Some(ms) = ms {
                    std::thread::sleep(std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0));
                }
                if let Some(glob) = download {
                    let started = std::time::Instant::now();
                    let d = s.wait_download(glob, *timeout_ms).map_err(|e| anyhow::anyhow!("wait: {e:#}"))?;
                    if [selector, text, url, gone, js].iter().all(|c| c.is_none()) && !*idle {
                        let mut state = self.state_json();
                        state["download"] = d.to_json();
                        state["waited_ms"] = json!(started.elapsed().as_millis() as u64);
                        return Ok(state);
                    }
                }
                if *idle {
                    let max = timeout_ms
                        .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0))
                        .unwrap_or(std::time::Duration::from_secs(10));
                    let started = std::time::Instant::now();
                    if !s.network_quiet(NETWORK_QUIET, max) {
                        anyhow::bail!("wait: the network was not idle within {} ms", max.as_millis());
                    }
                    if [selector, text, url, gone, js].iter().all(|c| c.is_none()) {
                        let mut state = self.state_json();
                        state["waited_ms"] = json!(started.elapsed().as_millis() as u64);
                        return Ok(state);
                    }
                }
                let mut checks = Vec::new();
                let mut what = Vec::new();
                const VISIBLE: &str = "(el) => !!el && el.getClientRects().length > 0 \
                     && (el.ownerDocument.defaultView || window).getComputedStyle(el).visibility !== 'hidden'";
                let deep = crate::cdp::DEEP;
                // Any match counts: a hidden template copy ahead of the
                // visible element must not decide the wait. Same-origin
                // frames and open shadow roots are part of the page.
                if let Some(sel) = selector {
                    checks.push(format!("{deep}.all({}).some({VISIBLE})", json!(sel)));
                    what.push(format!("selector {sel:?} visible"));
                }
                if let Some(sel) = gone {
                    checks.push(format!("!{deep}.all({}).some({VISIBLE})", json!(sel)));
                    what.push(format!("selector {sel:?} gone"));
                }
                if let Some(t) = text {
                    checks.push(format!("{deep}.text().includes({})", json!(t)));
                    what.push(format!("text {t:?}"));
                }
                if let Some(u) = url {
                    checks.push(format!("location.href.includes({})", json!(u)));
                    what.push(format!("url contains {u:?}"));
                }
                if let Some(expr) = js {
                    checks.push(format!("!!(() => {{ try {{ const v = ({expr}\n); return typeof v === 'function' ? v() : v; }} catch (e) {{ return false; }} }})()"));
                    what.push(format!("js {expr:?}"));
                }
                if checks.is_empty() {
                    return Ok(json!({ "waited_ms": ms.unwrap_or(0.0) }));
                }
                if let Some(chain) = frame {
                    if url.is_some() || js.is_some() {
                        anyhow::bail!(
                            "wait --frame scopes --selector, --text and --gone; --url and --js are the page's"
                        );
                    }
                    let waited =
                        s.wait_until_in_frame(chain, &checks.join(" && "), &what.join(" and "), *timeout_ms)?;
                    let mut state = self.state_json();
                    state["waited_ms"] = json!(waited);
                    return Ok(state);
                }
                // One --text or --selector wait also looks in cross-origin
                // frames (text shown by a payment or sign-in frame).
                let in_frames = checks.len() == 1 && (text.is_some() || selector.is_some());
                let waited =
                    s.wait_until_or_in_frames(&checks.join(" && "), in_frames, &what.join(" and "), *timeout_ms)?;
                let mut state = self.state_json();
                state["waited_ms"] = json!(waited);
                Ok(state)
            }
            Command::Downloads { clear, format } => {
                let items = s.downloads(*clear);
                if format.as_deref() == Some("json") {
                    return Ok(json!(items.iter().map(|d| d.to_json()).collect::<Vec<_>>()));
                }
                if items.is_empty() {
                    return Ok(json!("(no downloads)"));
                }
                let lines: Vec<String> = items
                    .iter()
                    .map(|d| {
                        let j = d.to_json();
                        format!(
                            "#{} {} {} B {}  ({})",
                            d.n,
                            j["state"].as_str().unwrap_or(""),
                            j["bytes"],
                            j["path"].as_str().unwrap_or(&d.suggested),
                            d.url
                        )
                    })
                    .collect();
                Ok(json!(lines.join("\n")))
            }
            Command::Dialog { accept, prompt_text } => {
                let accept = accept.unwrap_or(true);
                s.set_dialog_policy(accept, prompt_text.clone());
                Ok(json!({ "dialogs": if accept { "accept" } else { "dismiss" } }))
            }
            Command::Text { selector, timeout_ms } => {
                let text = s
                    .text_with_timeout(selector, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("text {selector:?}: {e:#}"))?;
                Ok(json!(text))
            }
            Command::Eval { expression, timeout_ms } => {
                let value: Value =
                    s.evaluate_with_timeout(expression, *timeout_ms).map_err(|e| anyhow::anyhow!("eval: {e:#}"))?;
                Ok(value)
            }
            Command::Title {} => {
                let title = s.title().map_err(|e| anyhow::anyhow!("title: {e:#}"))?;
                Ok(json!(title))
            }
            Command::Ax { max_depth, all, format, limit, refs, scope, timeout_ms } => {
                let json_format = match format.as_deref() {
                    // Serve-protocol clients that predate the text tree get
                    // the JSON list they always got.
                    None | Some("json") => true,
                    Some("text") => *all == Some(true),
                    Some(other) => anyhow::bail!("ax --format takes text|json, got {other:?}"),
                };
                if json_format && (scope.selector.is_some() || scope.node_ref.is_some() || scope.text.is_some()) {
                    anyhow::bail!("ax: --selector/ref scoping needs the text tree (drop --format json)");
                }
                let opts = AxOptions {
                    max_depth: *max_depth,
                    json: json_format,
                    all: all.unwrap_or(false),
                    all_refs: refs.as_deref() == Some("all"),
                    limit: limit.unwrap_or(DEFAULT_AX_LIMIT),
                    scope: scope.optional("ax")?,
                };
                let mut tree = s.ax(&opts, *timeout_ms).map_err(|e| anyhow::anyhow!("ax: {e:#}"))?;
                if let (Value::Array(nodes), Some(n)) = (&mut tree, limit) {
                    nodes.truncate(*n);
                }
                match tree {
                    Value::String(text) if self.session.page_count() > 1 => Ok(Value::String(
                        format!(
                            "{} (tab {} of {})",
                            text.lines().next().unwrap_or(""),
                            self.session.active_page(),
                            self.session.page_count()
                        ) + &text[text.find('\n').unwrap_or(text.len())..],
                    )),
                    other => Ok(other),
                }
            }
            Command::Console { level, limit, clear, format } => {
                let min = match level.as_deref() {
                    None | Some("debug") => 0,
                    Some("log") => 1,
                    Some("info") => 2,
                    Some("warn" | "warning") => 3,
                    Some("error") => 4,
                    Some(other) => anyhow::bail!("console --level takes debug|log|info|warn|error, got {other:?}"),
                };
                Ok(self.console_result(min, *limit, *clear, format.as_deref()))
            }
            Command::Errors { limit, clear, format } => Ok(self.console_result(4, *limit, *clear, format.as_deref())),
            Command::Network { n, filter, kind, failed, limit, clear, har, body_file, since } => self.network(
                *n,
                filter.as_deref(),
                kind.as_deref(),
                *failed,
                *limit,
                *clear,
                har.as_deref(),
                body_file.as_deref(),
                *since,
            ),
            Command::Cookies {
                action,
                name,
                value,
                url,
                domain,
                path,
                expires,
                http_only,
                secure,
                same_site,
                all,
                format,
            } => match action.as_deref().unwrap_or("list") {
                "list" => {
                    let cookies = s.cookies(*all, url.as_deref())?;
                    if format.as_deref() == Some("json") {
                        return Ok(cookies);
                    }
                    Ok(json!(cookies_text(&cookies)))
                }
                "set" => {
                    let mut c = json!({ "name": name, "value": value });
                    match (url, domain) {
                        (_, Some(d)) => c["domain"] = json!(d),
                        (Some(u), None) => c["url"] = json!(u),
                        (None, None) => c["url"] = json!(s.url()),
                    }
                    if let Some(p) = path {
                        c["path"] = json!(p);
                    } else if domain.is_some() {
                        c["path"] = json!("/");
                    }
                    if let Some(e) = expires {
                        c["expires"] = json!(e);
                    }
                    if *http_only {
                        c["httpOnly"] = json!(true);
                    }
                    if *secure {
                        c["secure"] = json!(true);
                    }
                    if let Some(ss) = same_site {
                        let ss = match ss.to_ascii_lowercase().as_str() {
                            "lax" => "Lax",
                            "strict" => "Strict",
                            "none" => "None",
                            other => anyhow::bail!("cookies --same-site takes Lax|Strict|None, got {other:?}"),
                        };
                        c["sameSite"] = json!(ss);
                    }
                    s.set_cookie(c)?;
                    Ok(json!({ "set": name }))
                }
                "delete" => {
                    let mut p = json!({ "name": name });
                    match (url, domain) {
                        (_, Some(d)) => p["domain"] = json!(d),
                        (Some(u), None) => p["url"] = json!(u),
                        (None, None) => p["url"] = json!(s.url()),
                    }
                    s.delete_cookies(p)?;
                    Ok(json!({ "deleted": name }))
                }
                "clear" => {
                    s.clear_cookies()?;
                    Ok(json!({ "cleared": true }))
                }
                other => anyhow::bail!("cookies: unknown action {other:?} (list|set|delete|clear)"),
            },
            Command::State { action, path } if action == "save" => {
                let state = self.session.storage_state()?;
                let (cookies, origins) =
                    (state["cookies"].as_array().map_or(0, Vec::len), state["origins"].as_array().map_or(0, Vec::len));
                std::fs::write(path, serde_json::to_string_pretty(&state)? + "\n")
                    .map_err(|e| anyhow::anyhow!("state save {path}: {e}"))?;
                Ok(json!({ "saved": path, "cookies": cookies, "origins": origins }))
            }
            Command::State { path, .. } => {
                let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("state load {path}: {e}"))?;
                let state: Value =
                    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("state load {path}: not JSON ({e})"))?;
                let (cookies, origins) = self.session.load_storage_state(&state)?;
                Ok(json!({ "loaded": path, "cookies": cookies, "origins": origins }))
            }
            Command::Pdf { path, landscape, background } => {
                let path = path.clone().unwrap_or_else(|| {
                    format!(
                        "page-{}.pdf",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    )
                });
                let bytes = s.pdf(*landscape, *background).map_err(|e| anyhow::anyhow!("pdf: {e:#}"))?;
                std::fs::write(&path, &bytes).map_err(|e| anyhow::anyhow!("pdf: writing {path}: {e:#}"))?;
                let path = std::path::absolute(&path).map(|p| p.display().to_string()).unwrap_or(path);
                Ok(json!({ "path": path, "bytes": bytes.len() }))
            }
            Command::Storage { area, action, key, value } => {
                let area = match area.as_deref().unwrap_or("local") {
                    "local" => "localStorage",
                    "session" => "sessionStorage",
                    other => anyhow::bail!("storage takes local|session, got {other:?}"),
                };
                let js = match (action.as_deref().unwrap_or("get"), key, value) {
                    ("get", Some(k), _) => format!("{area}.getItem({})", json!(k)),
                    ("get", None, _) => format!("Object.fromEntries(Object.entries({area}))"),
                    ("set", Some(k), Some(v)) => format!("({area}.setItem({}, {}), true)", json!(k), json!(v)),
                    ("remove", Some(k), _) => format!("({area}.removeItem({}), true)", json!(k)),
                    ("clear", _, _) => format!("({area}.clear(), true)"),
                    (other, ..) => anyhow::bail!("storage: {other} needs a key (set: and a value)"),
                };
                s.evaluate_with_timeout::<Value>(&js, None).map_err(|e| anyhow::anyhow!("storage: {e:#}"))
            }
            Command::Route { pattern: None, .. } => {
                let routes = self.session.routes();
                if routes.is_empty() {
                    return Ok(json!("(no routes)"));
                }
                Ok(json!(routes
                    .iter()
                    .map(|r| {
                        let what = match &r.action {
                            crate::cdp::RouteAction::Abort => "abort".to_string(),
                            crate::cdp::RouteAction::Fulfill { status, .. } => format!("fulfill {status}"),
                        };
                        format!("{} -> {what} ({} hits)", r.pattern, r.hits)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")))
            }
            Command::Route { pattern: Some(pattern), abort, status, body, body_file, content_type, headers } => {
                let fulfill = status.is_some() || body.is_some() || body_file.is_some();
                if *abort && fulfill {
                    anyhow::bail!("route: --abort or a response (--status/--body), not both");
                }
                let action = if fulfill {
                    use base64::Engine as _;
                    let bytes = match (body, body_file) {
                        (Some(_), Some(_)) => anyhow::bail!("route: --body or --body-file, not both"),
                        (Some(b), None) => b.clone().into_bytes(),
                        (None, Some(f)) => {
                            std::fs::read(f).map_err(|e| anyhow::anyhow!("route --body-file {f}: {e}"))?
                        }
                        (None, None) => Vec::new(),
                    };
                    let mut hs: Vec<(String, String)> = headers
                        .iter()
                        .map(|h| {
                            h.split_once(':')
                                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                                .ok_or_else(|| anyhow::anyhow!("route --header {h:?}: use 'Name: value'"))
                        })
                        .collect::<anyhow::Result<_>>()?;
                    let ctype = content_type.clone().unwrap_or_else(|| {
                        let t = String::from_utf8_lossy(&bytes);
                        let t = t.trim_start();
                        if t.starts_with('{') || t.starts_with('[') {
                            "application/json"
                        } else if t.starts_with('<') {
                            "text/html"
                        } else {
                            "text/plain"
                        }
                        .to_string()
                    });
                    if !hs.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                        hs.push(("Content-Type".into(), ctype));
                    }
                    crate::cdp::RouteAction::Fulfill {
                        status: status.unwrap_or(200),
                        headers: hs,
                        body_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }
                } else {
                    crate::cdp::RouteAction::Abort
                };
                let what = match &action {
                    crate::cdp::RouteAction::Abort => "abort".to_string(),
                    crate::cdp::RouteAction::Fulfill { status, .. } => format!("fulfill {status}"),
                };
                self.session
                    .add_route(crate::cdp::Route { pattern: pattern.clone(), action, hits: 0 })
                    .map_err(|e| anyhow::anyhow!("route: {e:#}"))?;
                Ok(json!({ "route": pattern, "action": what }))
            }
            Command::InitScript { source: Some(source), .. } => {
                let n = self.session.add_init_script(source).map_err(|e| anyhow::anyhow!("init-script: {e:#}"))?;
                Ok(json!({ "init_scripts": n }))
            }
            Command::InitScript { source: None, clear: true } => {
                Ok(json!({ "removed": self.session.clear_init_scripts() }))
            }
            Command::InitScript { source: None, clear: false } => {
                let scripts = self.session.init_scripts();
                if scripts.is_empty() {
                    return Ok(json!("(no init scripts)"));
                }
                Ok(json!(scripts
                    .iter()
                    .enumerate()
                    .map(|(i, src)| format!(
                        "{}: {}",
                        i + 1,
                        src.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect::<String>()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")))
            }
            Command::Unroute { pattern } => {
                let removed =
                    self.session.remove_routes(pattern.as_deref()).map_err(|e| anyhow::anyhow!("unroute: {e:#}"))?;
                Ok(json!({ "removed": removed }))
            }
            Command::Set { what, values, .. } if what == "credentials" || what == "auth" => {
                let creds = match values.as_slice() {
                    [off] if matches!(off.as_str(), "off" | "reset" | "none" | "clear") => None,
                    [user, pass] => Some((user.clone(), pass.clone())),
                    _ => anyhow::bail!("set credentials <user> <password> (or `off`)"),
                };
                let on = creds.is_some();
                self.session.set_credentials(creds).map_err(|e| anyhow::anyhow!("set credentials: {e:#}"))?;
                Ok(json!({ "credentials": if on { "set" } else { "off" } }))
            }
            Command::Set { what, values, scale, mobile, accuracy } => {
                let plan = crate::emulate::plan(what, values, *scale, *mobile, *accuracy, s.is_lightpanda())?;
                if let Some(permission) = plan.grant {
                    self.session.grant(permission);
                }
                self.session
                    .set_override(plan.kind, plan.calls, plan.reset)
                    .map_err(|e| anyhow::anyhow!("set {what}: {e:#}"))?;
                Ok(plan.result)
            }
            Command::Get { what, name_of, target, timeout_ms } => {
                if what == "count" {
                    let sel = target.selector.as_deref().unwrap_or("*");
                    return Ok(json!(s.count(sel).map_err(|e| anyhow::anyhow!("get count {sel:?}: {e:#}"))?));
                }
                if !matches!(what.as_str(), "text" | "value" | "attr" | "html" | "outer" | "box" | "style" | "checked")
                {
                    anyhow::bail!("get takes text, value, attr <name>, html, outer, box, style <prop>, checked, count <css>, title or url; got {what:?}");
                }
                let t = target.target("get")?;
                s.get_prop(&t, what, name_of.as_deref(), *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("get {what} {}: {e:#}", t.label()))
            }
            Command::Is { state, target, timeout_ms } => {
                let t = target.target("is")?;
                Ok(json!(s
                    .is_state(&t, state, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("is {state} {}: {e:#}", t.label()))?))
            }
            Command::Drag { from, to, timeout_ms } => {
                let (f, t) = (from.target("drag")?, to.target("drag")?);
                let r = s
                    .drag(&f, &t, *timeout_ms)
                    .map_err(|e| anyhow::anyhow!("drag {} to {}: {e:#}", f.label(), t.label()))?;
                let mut out = self.state_json();
                out["drag"] = r;
                Ok(out)
            }
            Command::Dblclick { target, timeout_ms } => {
                let t = target.target("dblclick")?;
                s.dblclick(&t, *timeout_ms).map_err(|e| anyhow::anyhow!("dblclick {}: {e:#}", t.label()))?;
                Ok(self.state_json())
            }
            Command::Focus { target, timeout_ms } => {
                let t = target.target("focus")?;
                let focused = s.focus(&t, *timeout_ms).map_err(|e| anyhow::anyhow!("focus {}: {e:#}", t.label()))?;
                Ok(json!({ "focused": focused }))
            }
            Command::SetChecked { checked, target, timeout_ms } => {
                let t = target.target(if *checked { "check" } else { "uncheck" })?;
                let changed = s.set_checked(&t, *checked, *timeout_ms).map_err(|e| {
                    anyhow::anyhow!("{} {}: {e:#}", if *checked { "check" } else { "uncheck" }, t.label())
                })?;
                let mut state = self.state_json();
                state["checked"] = json!(checked);
                state["changed"] = json!(changed);
                Ok(state)
            }
            Command::KeyDown { key } => {
                s.key_event(key, true).map_err(|e| anyhow::anyhow!("keydown: {e:#}"))?;
                Ok(json!({ "down": key }))
            }
            Command::KeyUp { key } => {
                s.key_event(key, false).map_err(|e| anyhow::anyhow!("keyup: {e:#}"))?;
                Ok(json!({ "up": key }))
            }
            Command::InsertText { value, target, timeout_ms } => {
                let t = target.optional("insert-text")?;
                s.insert_text(t.as_ref(), value, *timeout_ms).map_err(|e| anyhow::anyhow!("insert-text: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Url {} => Ok(self.state_json()),
            Command::Status {} => {
                let mut state = self.state_json();
                state["engine"] = json!(if self.session.is_lightpanda() { "lightpanda" } else { "chrome" });
                state["executable"] = json!(self.session.executable());
                state["endpoint"] = json!(self.session.endpoint());
                state["attached"] = json!(self.session.is_attached());
                state["version"] = json!(env!("CARGO_PKG_VERSION"));
                Ok(state)
            }
            Command::TabList {} => Ok(self.tabs_json()),
            Command::Show {} => self.relaunch(true),
            Command::Hide {} => self.relaunch(false),
            Command::Reset { all } => {
                self.session.reset(*all).map_err(|e| anyhow::anyhow!("reset: {e:#}"))?;
                Ok(self.tabs_json())
            }
            Command::TabNew { url } => {
                self.session.new_page().map_err(|e| anyhow::anyhow!("tab-new: {e:#}"))?;
                if let Some(url) = url {
                    self.goto(url, None, None)?;
                }
                Ok(self.tabs_json())
            }
            Command::TabSelect { index, target } => {
                let index = match target {
                    Some(id) => {
                        let ids = self.session.page_targets();
                        let hits: Vec<usize> = ids
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| t.to_ascii_lowercase().starts_with(&id.to_ascii_lowercase()))
                            .map(|(i, _)| i)
                            .collect();
                        match hits.as_slice() {
                            [i] => *i,
                            [] => anyhow::bail!("tab-select: no tab with target {id} (`tab-list` shows them)"),
                            _ => anyhow::bail!("tab-select: {id} matches {} tabs; give more of the id", hits.len()),
                        }
                    }
                    None => *index,
                };
                self.session.select_page(index).map_err(|e| anyhow::anyhow!("tab-select {index}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::TabClose { index } | Command::ClosePage { index } => {
                let index = index.unwrap_or_else(|| self.session.active_page());
                self.session.close_page(index).map_err(|e| anyhow::anyhow!("tab-close {index}: {e:#}"))?;
                Ok(self.state_json())
            }
            Command::Screenshot { path, full_page } => {
                let path = path.clone().unwrap_or_else(|| {
                    format!(
                        "shot-{}.png",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    )
                });
                let bytes =
                    s.screenshot_png(full_page.unwrap_or(false)).map_err(|e| anyhow::anyhow!("screenshot: {e:#}"))?;
                std::fs::write(&path, &bytes).map_err(|e| anyhow::anyhow!("screenshot: writing {path}: {e:#}"))?;
                // Report where it really went (a relative path is relative
                // to this process, which may not be the caller's directory).
                let path = std::path::absolute(&path).map(|p| p.display().to_string()).unwrap_or(path);
                Ok(json!({ "path": path, "bytes": bytes.len() }))
            }
            // Attached: only the connection ends; the browser and every
            // window (navigera's included) stay open.
            Command::Quit { close_browser: true } if s.is_attached() => {
                s.close_browser()?;
                Ok(json!({ "bye": true, "browser": "closed" }))
            }
            Command::Quit { .. } if s.is_attached() => Ok(json!({ "bye": true, "browser": "left open" })),
            Command::Quit { .. } => Ok(json!({ "bye": true })),
        }
    }

    /// Navigate the active page, applying the engine-aware policy.
    fn goto(&self, url: &str, wait: Option<&str>, timeout_ms: Option<f64>) -> anyhow::Result<()> {
        let url = normalize_url(url);
        self.session
            .goto_wait(&url, wait.unwrap_or("load"), timeout_ms)
            .map_err(|e| anyhow::anyhow!("goto {url}: {e:#}"))
    }
}

/// `example.com` -> `https://example.com`; `localhost:3000` -> `http://…`.
/// Anything with a scheme (http:, file:, data:, about:) is left alone.
/// The op's own `timeout_ms`, if it carries one.
/// The probability `--find` needs before it acts (`NAVIGERA_FIND_MIN`,
/// default 0.5); below it the op fails and lists the candidates.
fn find_min() -> f64 {
    std::env::var("NAVIGERA_FIND_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5)
}

impl Command {
    fn target_mut(&mut self) -> Option<&mut TargetArgs> {
        match self {
            Command::Click { target, .. }
            | Command::Fill { target, .. }
            | Command::Hover { target, .. }
            | Command::Mouse { target, .. }
            | Command::Select { target, .. }
            | Command::Press { target, .. }
            | Command::Type { target, .. }
            | Command::Scroll { target, .. }
            | Command::Upload { target, .. }
            | Command::Get { target, .. }
            | Command::Is { target, .. }
            | Command::Dblclick { target, .. }
            | Command::Focus { target, .. }
            | Command::SetChecked { target, .. }
            | Command::InsertText { target, .. } => Some(target),
            Command::Ax { scope, .. } => Some(scope),
            _ => None,
        }
    }

    /// The description given with `--find`, if any.
    fn find_target(&self) -> Option<String> {
        self.clone().target_mut().and_then(|t| t.find.clone())
    }

    /// This command with its `--find` replaced by a ref.
    fn with_ref(&self, node_ref: u64) -> Command {
        let mut c = self.clone();
        if let Some(t) = c.target_mut() {
            t.find = None;
            t.node_ref = Some(node_ref);
        }
        c
    }
}

fn op_timeout_ms(command: &Command) -> Option<f64> {
    let v = serde_json::to_value(command).ok()?;
    v.get("timeout_ms").and_then(Value::as_f64)
}

pub fn normalize_url(url: &str) -> String {
    let u = url.trim();
    let has_scheme = u.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && !rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    });
    if has_scheme {
        return u.to_string();
    }
    let local = ["localhost", "127.", "0.0.0.0", "[::1]"].iter().any(|p| u.starts_with(p));
    format!("{}://{u}", if local { "http" } else { "https" })
}

/// Parse and run one protocol line against the live driver.
///
/// Returns `None` for blank lines, else the response plus whether the line
/// was `quit` (the caller owns shutdown). Shared by stdin `serve` and the
/// socket session server so both speak exactly the same protocol.
///
/// A line holding a JSON array is a batch: each command runs in order and
/// gets its own response line; a failure doesn't stop the rest, `quit` does.
/// (One line per batch sidesteps shells that don't turn `\n` into newlines.)
pub(crate) fn handle_line(driver: &mut Driver, raw: &str) -> Option<(Vec<Response>, bool)> {
    // PowerShell 5 pipes text to native programs with a UTF-8 BOM.
    let raw = raw.trim_start_matches('\u{feff}');
    if raw.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => {
            let mut responses = Vec::with_capacity(items.len());
            for item in items {
                let (response, is_quit) = handle_value(driver, item);
                responses.push(response);
                if is_quit {
                    return Some((responses, true));
                }
            }
            Some((responses, false))
        }
        Ok(value) => {
            let (response, is_quit) = handle_value(driver, value);
            Some((vec![response], is_quit))
        }
        Err(_) => {
            let mut error = "each line must be a JSON object (or an array of them)".to_string();
            if raw.contains("}\\n{") {
                error.push_str(
                    "; this line has a literal \\n between commands (PowerShell's echo doesn't \
                     turn \\n into a newline): send a JSON array on one line, or pipe a file",
                );
            }
            Some((vec![err_response(Value::Null, error, Instant::now())], false))
        }
    }
}

fn handle_value(driver: &mut Driver, value: Value) -> (Response, bool) {
    let (id, command) = match value {
        Value::Object(mut map) => {
            let id = map.remove("id").unwrap_or(Value::Null);
            match serde_json::from_value::<Command>(Value::Object(map)) {
                Ok(command) => (id, command),
                Err(e) => return (err_response(id, format!("bad command: {e}"), Instant::now()), false),
            }
        }
        _ => return (err_response(Value::Null, "each command must be a JSON object".into(), Instant::now()), false),
    };
    let started = Instant::now();
    let is_quit = matches!(command, Command::Quit { .. });
    let response = match driver.run_op(&command) {
        Ok(outcome) => outcome_response(id, outcome, started),
        Err(e) => {
            let mut r = err_response(id, format!("{e:#}"), started);
            let dialogs = driver.session().take_dialogs();
            if !dialogs.is_empty() {
                r.dialogs = Some(dialogs);
            }
            r
        }
    };
    (response, is_quit)
}

pub(crate) fn write_response(output: &mut dyn Write, response: &Response, pretty: bool) -> io::Result<()> {
    let line = if pretty { serde_json::to_string_pretty(response) } else { serde_json::to_string(response) }
        .expect("response serializes");
    writeln!(output, "{line}")?;
    output.flush()
}

/// Print a response for a human/agent CLI call: the JSON envelope, or with
/// `--raw` just the result (strings unquoted; notes and errors on stderr).
/// Returns whether the op succeeded.
pub fn print_cli_response(output: &mut dyn Write, response: &Value, config: &SessionConfig) -> bool {
    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !config.raw {
        let text = if config.pretty { serde_json::to_string_pretty(response) } else { serde_json::to_string(response) }
            .unwrap_or_else(|_| response.to_string());
        let _ = writeln!(output, "{text}");
        let _ = output.flush();
        return ok;
    }
    for d in response.get("dialogs").and_then(Value::as_array).into_iter().flatten() {
        eprintln!(
            "note: {} dialog {:?} was {}",
            d.get("type").and_then(Value::as_str).unwrap_or("dialog"),
            d.get("message").and_then(Value::as_str).unwrap_or(""),
            if d.get("accepted").and_then(Value::as_bool) == Some(true) { "accepted" } else { "dismissed" }
        );
    }
    for n in response.get("notes").and_then(Value::as_array).into_iter().flatten() {
        eprintln!("note: {}", n.as_str().unwrap_or_default());
    }
    if let Some(tabs) = response.get("new_tabs").and_then(Value::as_array) {
        eprintln!(
            "note: the page opened {} new tab(s); now on tab {}",
            tabs.len(),
            tabs.last().unwrap_or(&Value::Null)
        );
    }
    if ok {
        match response.get("result") {
            Some(Value::String(s)) => {
                let _ = writeln!(output, "{s}");
            }
            Some(other) => {
                let _ = writeln!(
                    output,
                    "{}",
                    if config.pretty {
                        serde_json::to_string_pretty(other).unwrap_or_default()
                    } else {
                        other.to_string()
                    }
                );
            }
            None => {}
        }
    } else {
        eprintln!("error: {}", response.get("error").and_then(Value::as_str).unwrap_or("unknown error"));
    }
    let _ = output.flush();
    ok
}

/// One line per cookie: `name=value; domain=…; path=/; expires=…; HttpOnly; Secure; SameSite=Lax`.
fn cookies_text(cookies: &Value) -> String {
    let lines: Vec<String> = cookies
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let get = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
            let value: String = get("value").chars().take(120).collect();
            let mut line = format!("{}={value}; domain={}; path={}", get("name"), get("domain"), get("path"));
            match c.get("expires").and_then(Value::as_f64) {
                Some(e) if e > 0.0 => line.push_str(&format!("; expires={}", e as i64)),
                _ => line.push_str("; session"),
            }
            for (flag, label) in [("httpOnly", "HttpOnly"), ("secure", "Secure")] {
                if c.get(flag).and_then(Value::as_bool) == Some(true) {
                    line.push_str(&format!("; {label}"));
                }
            }
            if !get("sameSite").is_empty() {
                line.push_str(&format!("; SameSite={}", get("sameSite")));
            }
            line
        })
        .collect();
    if lines.is_empty() {
        "(no cookies)".into()
    } else {
        lines.join("\n")
    }
}

/// `scheme://host[:port]` of a URL ("" when it has none).
fn origin_of(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return String::new() };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{scheme}://{host}")
}

fn outcome_response(id: Value, outcome: Outcome, started: Instant) -> Response {
    let mut r = ok_response(id, outcome.result, started);
    if !outcome.dialogs.is_empty() {
        r.dialogs = Some(outcome.dialogs);
    }
    if !outcome.new_tabs.is_empty() {
        r.new_tabs = Some(outcome.new_tabs);
    }
    if !outcome.notes.is_empty() {
        r.notes = Some(outcome.notes);
    }
    r
}

fn ok_response(id: Value, result: Value, started: Instant) -> Response {
    Response {
        id,
        ok: true,
        result: Some(result),
        error: None,
        dialogs: None,
        new_tabs: None,
        notes: None,
        elapsed_ms: elapsed_ms(started),
    }
}

/// Elapsed milliseconds, rounded to the microsecond.
fn elapsed_ms(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0
}

fn err_response(id: Value, error: String, started: Instant) -> Response {
    Response {
        id,
        ok: false,
        result: None,
        error: Some(error),
        dialogs: None,
        new_tabs: None,
        notes: None,
        elapsed_ms: elapsed_ms(started),
    }
}

/// Peak RSS of this process in KB (VmHWM from /proc/self/status), if readable.
/// Used for the NAVIGERA_RSS_REPORT diagnostic.
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Serve mode: launch one warm browser, then read JSON commands from `input`
/// and write one JSON response per line to `output`.
///
/// Returns the process exit code (`0` after `quit` or EOF, `1` on launch /
/// IO failure). Logs go to stderr so stdout stays protocol-clean.
pub fn serve(config: &SessionConfig, input: &mut dyn BufRead, output: &mut dyn Write) -> ExitCode {
    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("navigera: {e:#}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "navigera: live session on {} (engine={}); send JSON lines, {{\"op\":\"quit\"}} to exit",
        driver.session().endpoint(),
        config.engine,
    );

    for raw in input.lines() {
        let raw = match raw {
            Ok(line) => line,
            Err(e) => {
                eprintln!("navigera: stdin read failed: {e}");
                return ExitCode::from(1);
            }
        };
        let Some((responses, is_quit)) = handle_line(&mut driver, &raw) else {
            continue;
        };
        // Serve mode is a line protocol: one response == one line, always.
        // (`--pretty` would split a response across lines and desync any
        // line-reading client; it only applies to one-shot output.)
        for response in &responses {
            if let Err(e) = write_response(output, response, false) {
                eprintln!("[navigera] failed to write response: {e:#}");
                return ExitCode::from(1);
            }
        }
        if is_quit {
            driver.session().close();
            crate::timing::report();
            if std::env::var("NAVIGERA_RSS_REPORT").is_ok() {
                eprintln!("[navigera] peak RSS at quit: {:?} KB", peak_rss_kb());
            }
            return ExitCode::SUCCESS;
        }
    }
    // EOF: shut the session down cleanly.
    driver.session().close();
    crate::timing::report();
    eprintln!("navigera: stdin closed, session shut down");
    if std::env::var("NAVIGERA_RSS_REPORT").is_ok() {
        eprintln!("[navigera] peak RSS at EOF-shutdown: {:?} KB", peak_rss_kb());
    }
    ExitCode::SUCCESS
}

/// One-shot mode: launch the browser, run one command, close, write response.
pub fn oneshot(config: &SessionConfig, command: &Command, output: &mut dyn Write) -> ExitCode {
    let started = Instant::now();
    if config.attach.is_some() || config.profile.is_some() {
        // Each one-shot command would open (and abandon) a window in the
        // user's browser, and Chrome would ask to Allow every one of them.
        let flag = if config.attach.is_some() { "--attach" } else { "--profile" };
        let error = format!(
            "{flag} keeps one connection to a browser that stays open: use a session, \
             e.g. `navigera -s me start {flag} {}` then `navigera -s me <command>`",
            config.attach.as_deref().or(config.profile.as_deref()).unwrap_or("")
        );
        print_cli_response(output, &json!({ "id": null, "ok": false, "error": error }), config);
        return ExitCode::from(2);
    }
    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("navigera: {e:#}");
            return ExitCode::from(1);
        }
    };
    let response = match driver.run_op(command) {
        Ok(outcome) => outcome_response(Value::Null, outcome, started),
        Err(e) => err_response(Value::Null, format!("{e:#}"), started),
    };
    driver.session().close();
    crate::timing::report();
    let value = serde_json::to_value(&response).unwrap_or(Value::Null);
    if print_cli_response(output, &value, config) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Run a browser-less command (`skill`, `install-skill`, `--version`).
pub fn run_local(local: &Local, config: &SessionConfig, output: &mut dyn Write) -> ExitCode {
    match local {
        Local::Version => {
            let _ = writeln!(output, "navigera {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Local::Skill => {
            let _ = write!(output, "{SKILL_MD}");
            ExitCode::SUCCESS
        }
        Local::Profiles => {
            let response = json!({ "id": null, "ok": true, "result": crate::cdp::import::list_profiles() });
            if print_cli_response(output, &response, config) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Local::InstallSkill { dir } => {
            let response = match install_skill(dir) {
                Ok(path) => json!({ "id": null, "ok": true, "result": { "installed": path } }),
                Err(e) => json!({ "id": null, "ok": false, "error": format!("install-skill: {e:#}") }),
            };
            if print_cli_response(output, &response, config) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ax_preview_keeps_the_top_and_counts_the_rest() {
        let tree: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let (shown, hidden) = super::ax_preview(&tree, 40);
        assert_eq!(hidden, 60);
        assert_eq!(shown.lines().count(), 40);
        assert!(shown.starts_with("line 1\n") && shown.ends_with("line 40"), "{shown}");
        let short = "a\nb\n";
        assert_eq!(super::ax_preview(short, 40), (short.to_string(), 0));
    }

    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn cmd(items: &[&str]) -> Command {
        let mut all = vec!["navigera"];
        all.extend_from_slice(items);
        parse_args(argv(&all)).expect("valid invocation").command.expect("a command")
    }

    #[test]
    fn parses_one_shot_eval_with_trailing_global_flag() {
        let parsed = parse_args(argv(&["navigera", "eval", "--expression", "() => 40 + 2", "--pretty"]))
            .expect("valid invocation");
        assert!(parsed.config.pretty, "globals may trail the command");
        assert_eq!(parsed.config.engine, "chrome");
        assert_eq!(parsed.command, Some(Command::Eval { expression: "() => 40 + 2".into(), timeout_ms: None }));
    }

    #[test]
    fn parses_globals_before_command() {
        let parsed = parse_args(argv(&[
            "navigera",
            "--headed",
            "--engine",
            "chrome",
            "--chromium",
            "C:/chrome.exe",
            "goto",
            "--url",
            "https://example.com",
            "--wait",
            "domcontentloaded",
        ]))
        .expect("valid invocation");
        assert!(parsed.config.headed);
        assert_eq!(parsed.config.engine, "chrome");
        assert_eq!(parsed.config.chromium.as_deref(), Some("C:/chrome.exe"));
        assert_eq!(
            parsed.command,
            Some(Command::Goto {
                url: "https://example.com".into(),
                wait: Some("domcontentloaded".into()),
                timeout_ms: None,
            })
        );
    }

    #[test]
    fn positional_arguments_like_other_agent_clis() {
        assert_eq!(
            cmd(&["goto", "example.com"]),
            Command::Goto { url: "example.com".into(), wait: None, timeout_ms: None }
        );
        assert_eq!(
            cmd(&["click", "@e12"]),
            Command::Click {
                target: TargetArgs { node_ref: Some(12), ..Default::default() },
                timeout_ms: None,
                button: None,
                force: false
            }
        );
        assert_eq!(
            cmd(&["click", "#buy", "--timeout-ms", "900"]),
            Command::Click {
                target: TargetArgs { selector: Some("#buy".into()), ..Default::default() },
                timeout_ms: Some(900.0),
                button: None,
                force: false,
            }
        );
        assert_eq!(
            cmd(&["fill", "31", "hello world"]),
            Command::Fill {
                target: TargetArgs { node_ref: Some(31), ..Default::default() },
                value: "hello world".into(),
                timeout_ms: None,
                keys: false,
            }
        );
        assert_eq!(
            cmd(&["click", "--text", "Add to cart"]),
            Command::Click {
                target: TargetArgs { text: Some("Add to cart".into()), ..Default::default() },
                timeout_ms: None,
                button: None,
                force: false,
            }
        );
        assert_eq!(
            cmd(&["press", "Enter"]),
            Command::Press { key: "Enter".into(), target: TargetArgs::default(), timeout_ms: None }
        );
        assert_eq!(
            cmd(&["select", "7", "Canada"]),
            Command::Select {
                target: TargetArgs { node_ref: Some(7), ..Default::default() },
                value: vec!["Canada".into()],
                timeout_ms: None,
                add: false,
            }
        );
        assert_eq!(
            cmd(&["eval", "document.title"]),
            Command::Eval { expression: "document.title".into(), timeout_ms: None }
        );
        assert_eq!(
            cmd(&["wait", "--text", "Order placed"]),
            Command::Wait {
                selector: None,
                text: Some("Order placed".into()),
                url: None,
                gone: None,
                js: None,
                frame: None,
                ms: None,
                idle: false,
                timeout_ms: None,
                download: None,
            }
        );
        assert_eq!(
            cmd(&["wait", "250"]),
            Command::Wait {
                selector: None,
                text: None,
                url: None,
                gone: None,
                js: None,
                frame: None,
                ms: Some(250.0),
                idle: false,
                timeout_ms: None,
                download: None,
            }
        );
        assert_eq!(cmd(&["tab-select", "1"]), Command::TabSelect { index: 1, target: None });
        assert_eq!(cmd(&["tab-select", "9F2A0C"]), Command::TabSelect { index: 0, target: Some("9F2A0C".into()) });
        assert_eq!(cmd(&["snapshot"]), cmd(&["ax"]));
        assert_eq!(cmd(&["close"]), Command::Quit { close_browser: false });
        assert_eq!(cmd(&["quit", "--close-browser"]), Command::Quit { close_browser: true });
    }

    #[test]
    fn urls_get_a_scheme() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(normalize_url("127.0.0.1:8000"), "http://127.0.0.1:8000");
        assert_eq!(normalize_url("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(normalize_url("file:///tmp/a.html"), "file:///tmp/a.html");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    #[test]
    fn serve_mode_has_no_command() {
        let parsed = parse_args(argv(&["navigera", "serve", "--timeout-ms", "5000"])).expect("valid invocation");
        assert!(parsed.command.is_none());
        assert_eq!(parsed.config.timeout_ms, 5000.0);
    }

    #[test]
    fn help_and_invalid_invocations_are_distinguished() {
        let help = parse_args(argv(&["navigera", "--help"])).expect_err("help");
        assert!(matches!(help, ArgsError::Help(_)), "explicit help is Help, not Invalid: {help:?}");
        let no_args = parse_args(argv(&["navigera"])).expect_err("no args");
        assert!(matches!(no_args, ArgsError::Help(_)));
        match parse_args(argv(&["navigera", "help", "click"])).expect_err("op help") {
            ArgsError::Help(text) => assert!(text.contains("usage:") && text.contains("click"), "{text}"),
            other => panic!("{other:?}"),
        }
        match parse_args(argv(&["navigera", "fill", "--help"])).expect_err("op help") {
            ArgsError::Help(text) => assert!(text.contains("fill"), "{text}"),
            other => panic!("{other:?}"),
        }

        let unknown = parse_args(argv(&["navigera", "navigat"])).expect_err("unknown");
        match unknown {
            ArgsError::Invalid(message) => {
                assert!(message.contains("unknown command"), "{message}");
                assert!(message.contains("did you mean `goto`") || message.contains("commands:"), "{message}");
            }
            other => panic!("unknown command must be Invalid: {other:?}"),
        }
        match parse_args(argv(&["navigera", "evaluat"])).expect_err("typo") {
            ArgsError::Invalid(message) => assert!(message.contains("did you mean `eval`"), "{message}"),
            other => panic!("{other:?}"),
        }

        let missing = parse_args(argv(&["navigera", "goto"])).expect_err("missing url");
        assert!(matches!(missing, ArgsError::Invalid(_)));
        let extra = parse_args(argv(&["navigera", "title", "x"])).expect_err("extra");
        assert!(matches!(extra, ArgsError::Invalid(m) if m.contains("unexpected argument")));
    }

    #[test]
    fn tab_commands_parse_indexes() {
        assert_eq!(cmd(&["tab-select", "--index", "2"]), Command::TabSelect { index: 2, target: None });
        assert_eq!(cmd(&["tab-close"]), Command::TabClose { index: None });
        assert!(parse_args(argv(&["navigera", "tab-select"])).is_err());
        assert!(parse_args(argv(&["navigera", "tab-select", "--index", "x"])).is_err());
    }

    #[test]
    fn command_deserializes_from_protocol_json() {
        let command: Command =
            serde_json::from_str(r##"{"op":"fill","selector":"#kw","value":"600-10070","timeout_ms":10000}"##)
                .expect("valid command");
        assert_eq!(
            command,
            Command::Fill {
                target: TargetArgs { selector: Some("#kw".into()), ..Default::default() },
                value: "600-10070".into(),
                timeout_ms: Some(10000.0),
                keys: false,
            }
        );
        let command: Command = serde_json::from_str(r#"{"op":"click","ref":"@e7"}"#).expect("string ref");
        assert_eq!(
            command,
            Command::Click {
                target: TargetArgs { node_ref: Some(7), ..Default::default() },
                timeout_ms: None,
                button: None,
                force: false
            }
        );
        let command: Command = serde_json::from_str(r#"{"op":"select","ref":3,"values":["a","b"]}"#).expect("values");
        assert!(
            matches!(command, Command::Select { ref value, .. } if value == &vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn click_targets_are_exclusive() {
        let both =
            parse_args(argv(&["navigera", "click", "--ref", "1", "--selector", "a"])).expect("parses").command.unwrap();
        let mut driver_free = match both {
            Command::Click { target, .. } => target,
            _ => unreachable!(),
        };
        assert!(driver_free.target("click").is_err(), "selector and ref are mutually exclusive");
        driver_free = TargetArgs::default();
        assert!(driver_free.target("click").is_err(), "a target is required");
        assert!(parse_args(argv(&["navigera", "fill", "--value", "x"])).is_ok_and(|p| matches!(
            p.command,
            Some(Command::Fill { ref target, .. }) if *target == TargetArgs::default()
        )));
    }

    #[test]
    fn commands_round_trip_through_json_for_session_clients() {
        for command in [
            Command::Goto { url: "https://example.com".into(), wait: None, timeout_ms: None },
            Command::Click {
                target: TargetArgs { node_ref: Some(9), ..Default::default() },
                timeout_ms: Some(500.0),
                button: None,
                force: false,
            },
            Command::Ax {
                max_depth: None,
                all: Some(true),
                format: None,
                limit: Some(10),
                refs: None,
                scope: TargetArgs { selector: Some("main".into()), ..Default::default() },
                timeout_ms: None,
            },
            Command::Select {
                target: TargetArgs { text: Some("Country".into()), ..Default::default() },
                value: vec!["CA".into()],
                timeout_ms: None,
                add: false,
            },
            Command::Wait {
                selector: None,
                text: Some("Done".into()),
                url: None,
                gone: None,
                js: None,
                frame: None,
                ms: None,
                idle: false,
                timeout_ms: None,
                download: None,
            },
            Command::Quit { close_browser: false },
            Command::Quit { close_browser: true },
        ] {
            let wire = serde_json::to_string(&command).expect("serializes");
            let back: Command = serde_json::from_str(&wire).expect("deserializes");
            assert_eq!(back, command, "{wire}");
        }
        let wire = serde_json::to_string(&Command::Click {
            target: TargetArgs { node_ref: Some(9), ..Default::default() },
            timeout_ms: None,
            button: None,
            force: false,
        })
        .unwrap();
        assert_eq!(wire, r#"{"op":"click","ref":9}"#);
    }

    #[test]
    fn session_flags_and_start() {
        let parsed = parse_args(argv(&["navigera", "--session", "work", "--idle-timeout-s", "60", "start"]))
            .expect("valid invocation");
        assert!(parsed.start);
        assert!(parsed.command.is_none());
        assert_eq!(parsed.config.session.as_deref(), Some("work"));
        assert_eq!(parsed.config.idle_timeout_s, 60);

        let parsed =
            parse_args(argv(&["navigera", "title", "--session", "work", "--raw"])).expect("trailing --session");
        assert_eq!(parsed.config.session.as_deref(), Some("work"));
        assert!(parsed.config.raw);
        assert!(!parsed.start);
        assert_eq!(parse_args(argv(&["navigera", "-s=w", "title"])).unwrap().config.session.as_deref(), Some("w"));
        assert_eq!(parse_args(argv(&["navigera", "title", "-s", "w"])).unwrap().config.session.as_deref(), Some("w"));
        let parsed = parse_args(argv(&["navigera", "--session=work", "install-skill", "--claude"])).expect("local");
        assert_eq!(parsed.local, Some(Local::InstallSkill { dir: ".claude/skills".into() }));
    }

    #[test]
    fn attach_and_profile_flags() {
        let attach = |args: &[&str]| parse_args(argv(args)).map(|p| (p.config.attach, p.start));
        // Bare --attach means the user's Chrome, wherever it sits.
        assert_eq!(attach(&["navigera", "-s", "me", "start", "--attach"]).unwrap(), (Some("chrome".into()), true));
        assert_eq!(attach(&["navigera", "-s", "me", "--attach", "start"]).unwrap(), (Some("chrome".into()), true));
        assert_eq!(
            attach(&["navigera", "-s", "me", "start", "--attach", "--headless"]).unwrap().0.as_deref(),
            Some("chrome")
        );
        assert_eq!(attach(&["navigera", "-s", "me", "start", "--attach", "edge"]).unwrap().0.as_deref(), Some("edge"));
        assert_eq!(attach(&["navigera", "-s", "me", "--attach=9222", "start"]).unwrap().0.as_deref(), Some("9222"));
        assert_eq!(
            attach(&["navigera", "--attach", "/tmp/ud", "-s", "me", "start"]).unwrap().0.as_deref(),
            Some("/tmp/ud")
        );
        let p = parse_args(argv(&["navigera", "-s", "me", "start", "--profile", "work", "--headless"])).unwrap();
        assert_eq!(p.config.profile.as_deref(), Some("work"));
        assert!(p.config.headless);
        assert!(parse_args(argv(&["navigera", "-s", "me", "start", "--attach", "--profile", "w"])).is_err());
        // --from seeds a --profile; --natural goes anywhere.
        let p = parse_args(argv(&[
            "navigera",
            "-s",
            "me",
            "start",
            "--profile",
            "agent",
            "--from",
            "chrome",
            "--from-profile",
            "Work",
            "--natural",
        ]))
        .unwrap();
        assert_eq!(p.config.from.as_deref(), Some("chrome"));
        assert_eq!(p.config.from_profile.as_deref(), Some("Work"));
        assert!(p.config.natural);
        let p = parse_args(argv(&["navigera", "--natural", "-s", "me", "start"])).unwrap();
        assert!(p.config.natural);
        assert!(parse_args(argv(&["navigera", "-s", "me", "start", "--from", "chrome"])).is_err());
        assert!(parse_args(argv(&["navigera", "-s", "me", "start", "--profile", "a", "--from-profile", "x"])).is_err());
        assert_eq!(parse_args(argv(&["navigera", "profiles"])).unwrap().local, Some(Local::Profiles));
        assert!(parse_args(argv(&["navigera", "--engine", "lightpanda", "-s", "me", "start", "--attach"])).is_err());
    }

    #[test]
    fn response_serializes_protocol_shape() {
        let ok = ok_response(serde_json::json!(7), serde_json::json!({"url": "https://example.com/"}), Instant::now());
        let line = serde_json::to_string(&ok).expect("serializes");
        assert!(line.contains("\"id\":7"));
        assert!(line.contains("\"ok\":true"));
        assert!(!line.contains("error") && !line.contains("dialogs"), "successful responses omit `error`: {line}");

        let err = err_response(Value::Null, "boom".into(), Instant::now());
        let line = serde_json::to_string(&err).expect("serializes");
        assert!(line.contains("\"ok\":false") && line.contains("\"error\":\"boom\""));
    }

    #[test]
    fn refs_parse_in_every_spelling() {
        for raw in ["12", "@12", "e12", "ref=12", "[ref=12]", " 12 "] {
            assert_eq!(parse_ref(raw), Some(12), "{raw}");
        }
        assert_eq!(parse_ref("#buy"), None);
        assert_eq!(parse_ref("button"), None);
    }
}

#[cfg(test)]
mod fork_send {
    /// A forked driver is moved into a worker thread.
    #[test]
    fn a_driver_can_move_to_another_thread() {
        fn is_send<T: Send>() {}
        is_send::<super::Driver>();
    }
}
