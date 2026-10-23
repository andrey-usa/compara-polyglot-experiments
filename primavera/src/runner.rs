//! Runs scenarios on one browser, in-process through the navigera library:
//! no session server, no socket, no process per step. Each scenario starts
//! from a clean browser context (`reset`: no cookies, storage or tabs from
//! the one before).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use navigera::protocol::{parse_args, Command, Driver, SessionConfig};
use serde_json::Value;

use crate::expect::{compare, Expect, Matcher, Prop, Subject};
use crate::suite::{interpolate, Action, Scenario, Step, Suite};

#[derive(Debug, Clone)]
pub struct Options {
    pub base: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub headed: bool,
    pub chromium: Option<String>,
    /// How long an `expect` retries (ms) unless it says otherwise.
    pub expect_ms: u64,
    /// navigera's element wait for steps (`--wait-ms`).
    pub wait_ms: Option<f64>,
    pub grep: Option<String>,
    pub tags: Vec<String>,
    /// Where failure evidence (a screenshot) goes.
    pub evidence: PathBuf,
}

/// Kept for the JSON-lines and HTML reporters (M1).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StepResult {
    pub line: usize,
    pub text: String,
    pub ms: f64,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub line: usize,
    pub text: String,
    pub message: String,
    /// Where the page was.
    pub at: Option<(String, String)>,
    /// Something earlier that likely explains it (a click that changed nothing).
    pub hint: Option<String>,
    pub screenshot: Option<PathBuf>,
    /// When the element wasn't there: what on the page comes closest,
    /// (`role "name"`, a locator for it, how close).
    pub nearest: Vec<(String, String, f64)>,
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub tags: Vec<String>,
    pub ms: f64,
    #[allow(dead_code)]
    pub steps: Vec<StepResult>,
    pub failure: Option<Failure>,
    /// 1 for the first try; more when `--retries` ran it again.
    pub attempt: u32,
    /// How the earlier tries failed (a scenario that passed after one is
    /// flaky).
    pub retried: Vec<Failure>,
    /// What it saved, for `--json`.
    pub saved: BTreeMap<String, String>,
    /// Optional steps that didn't happen.
    pub notes: Vec<String>,
    /// Which worker ran it.
    pub worker: usize,
}

impl ScenarioResult {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    pub fn flaky(&self) -> bool {
        self.passed() && !self.retried.is_empty()
    }

    /// `file::name`, the key of `.primavera/timings.json`.
    pub fn key(&self) -> String {
        format!("{}::{}", self.file.display(), self.name)
    }
}

/// A scenario to run: indexes into the run's suites.
#[derive(Debug, Clone, Copy)]
pub struct Job {
    pub suite: usize,
    pub scenario: usize,
}

impl Job {
    pub fn get<'a>(&self, suites: &'a [Suite]) -> (&'a Suite, &'a Scenario) {
        let s = &suites[self.suite];
        (s, &s.scenarios[self.scenario])
    }

    pub fn key(&self, suites: &[Suite]) -> String {
        let (s, sc) = self.get(suites);
        format!("{}::{}", s.file.display(), sc.name)
    }
}

/// Which scenarios a run takes, after `--grep` and `--tag`, in file order.
pub fn jobs(suites: &[Suite], opts: &Options) -> Vec<Job> {
    let mut out = Vec::new();
    for (si, suite) in suites.iter().enumerate() {
        for (ci, sc) in suite.scenarios.iter().enumerate() {
            if let Some(g) = &opts.grep {
                if !sc.name.to_lowercase().contains(&g.to_lowercase()) {
                    continue;
                }
            }
            if !opts.tags.is_empty() && !opts.tags.iter().any(|t| sc.tags.contains(t)) {
                continue;
            }
            out.push(Job { suite: si, scenario: ci });
        }
    }
    out
}

pub struct Runner {
    driver: Driver,
    opts: Options,
    fresh: bool,
    attempt: u32,
}

impl Runner {
    /// Start the browser. An error here is infrastructure, not a test result.
    pub fn launch(opts: Options) -> anyhow::Result<Self> {
        let config = SessionConfig {
            headed: opts.headed,
            chromium: opts.chromium.clone(),
            wait_ms: opts.wait_ms,
            ..SessionConfig::default()
        };
        let driver = Driver::launch(&config)?;
        Ok(Runner { driver, opts, fresh: true, attempt: 1 })
    }

    /// Another runner on this one's browser, in a browser context of its
    /// own: a worker without a browser of its own.
    pub fn fork(&self) -> anyhow::Result<Self> {
        Ok(Runner { driver: self.driver.fork()?, opts: self.opts.clone(), fresh: true, attempt: 1 })
    }

    pub fn run(&mut self, suite: &Suite, scenario: &Scenario, attempt: u32) -> ScenarioResult {
        self.attempt = attempt;
        let started = Instant::now();
        let mut scope = Scope::new(suite, scenario, &self.opts);
        let mut steps = Vec::new();
        let mut failure = None;
        // A clean browser context for every scenario but the first one of
        // the run (a new browser is clean already).
        if !self.fresh {
            if let Err(e) = self.driver.run_op(&Command::Reset {}) {
                failure = Some(Failure {
                    line: scenario.line,
                    text: "(reset)".into(),
                    message: format!("could not start from a clean browser context: {e:#}"),
                    at: None,
                    hint: None,
                    screenshot: None,
                    nearest: Vec::new(),
                });
            }
        }
        self.fresh = false;
        let all: Vec<&Step> = suite.before_each.iter().chain(&scenario.steps).collect();
        if failure.is_none() {
            for step in all {
                let t = Instant::now();
                let res = self.step(step, &mut scope, suite);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                steps.push(StepResult { line: step.line, text: step.text.clone(), ms, ok: res.is_ok() });
                if let Err(message) = res {
                    failure = Some(self.failure(step, message, &scope, suite, scenario));
                    break;
                }
            }
        }
        // `after each` runs whatever happened; its own failures count only
        // when the scenario had passed.
        for step in &suite.after_each {
            if let Err(message) = self.step(step, &mut scope, suite) {
                if failure.is_none() {
                    failure = Some(self.failure(step, message, &scope, suite, scenario));
                }
            }
        }
        ScenarioResult {
            file: suite.file.clone(),
            name: scenario.name.clone(),
            line: scenario.line,
            tags: scenario.tags.clone(),
            ms: started.elapsed().as_secs_f64() * 1000.0,
            steps,
            failure,
            attempt,
            retried: Vec::new(),
            saved: scope.saved,
            notes: scope.notes,
            worker: 0,
        }
    }

    fn failure(&mut self, step: &Step, message: String, scope: &Scope, suite: &Suite, sc: &Scenario) -> Failure {
        let at = self.driver.run(&Command::Url {}).ok().map(|v| {
            (
                v.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            )
        });
        let mut dir = evidence_name(&suite.file, &sc.name);
        if self.attempt > 1 {
            dir.push_str(&format!("-try{}", self.attempt));
        }
        let shot = self.opts.evidence.join(dir).join("screenshot.png");
        let screenshot = std::fs::create_dir_all(shot.parent().unwrap_or(Path::new(".")))
            .ok()
            .and_then(|_| {
                let cmd = parse(&["screenshot".into(), "--path".into(), shot.display().to_string()]).ok()?;
                self.driver.run(&cmd).ok()
            })
            .map(|_| shot);
        let nearest = if message.contains("no element") { self.nearest(&step.action, scope) } else { Vec::new() };
        Failure { line: step.line, text: step.text.clone(), message, at, hint: scope.noop.clone(), screenshot, nearest }
    }

    /// What on the page comes closest to the element a step asked for.
    fn nearest(&mut self, action: &Action, scope: &Scope) -> Vec<(String, String, f64)> {
        let words = match action {
            Action::Command(w) | Action::Save { command: w, .. } => w.clone(),
            Action::Expect(Expect { subject: Subject::Target { flags, .. }, .. }) => flags.clone(),
            Action::Optional(inner) => return self.nearest(inner, scope),
            _ => return Vec::new(),
        };
        let words = scope.words(&words).unwrap_or(words);
        let Some(want) = crate::nearest::wanted(&words) else { return Vec::new() };
        match self.command(&["ax".into(), "--format".into(), "json".into()]) {
            Ok(ax) => crate::nearest::rank(&ax, &want, 3),
            Err(_) => Vec::new(),
        }
    }

    fn step(&mut self, step: &Step, scope: &mut Scope, suite: &Suite) -> Result<(), String> {
        self.action(&step.action, step, scope, suite, 0)
    }

    fn action(
        &mut self,
        action: &Action,
        step: &Step,
        scope: &mut Scope,
        suite: &Suite,
        depth: usize,
    ) -> Result<(), String> {
        match action {
            Action::Command(words) => {
                let words = scope.words(words)?;
                let outcome = self.command(&words)?;
                // A click that changed nothing explains a failure soon after.
                scope.noop = outcome.get("no_effect").and_then(Value::as_str).map(|n| {
                    format!("the step at line {} ({}) changed nothing on the page: {n}", step.line, step.text)
                });
                Ok(())
            }
            Action::Save { name, command } => {
                let words = scope.words(command)?;
                let v = self.command(&words)?;
                let text = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                scope.saved.insert(name.clone(), text);
                Ok(())
            }
            Action::Expect(e) => self.expect(e, scope),
            Action::Optional(inner) => {
                if let Err(e) = self.action(inner, step, scope, suite, depth) {
                    scope.notes.push(format!("optional step at line {} didn't happen: {e}", step.line));
                }
                Ok(())
            }
            Action::Run { flow, args } => {
                if depth >= 16 {
                    return Err(format!("flows nest more than 16 deep at {flow} (a flow that runs itself?)"));
                }
                let f = suite.flows.get(flow).ok_or_else(|| format!("no flow named {flow:?}"))?;
                let values = scope.words(args)?;
                scope.layers.push(f.params.iter().cloned().zip(values).collect());
                let mut result = Ok(());
                for st in &f.steps {
                    if let Err(e) = self.action(&st.action, st, scope, suite, depth + 1) {
                        let file = f.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        result = Err(format!("in flow {flow}, {file}:{} `{}`: {e}", st.line, st.text));
                        break;
                    }
                }
                scope.layers.pop();
                result
            }
        }
    }

    /// One navigera command line, run in-process.
    fn command(&mut self, words: &[String]) -> Result<Value, String> {
        let words = self.absolute(words)?;
        let cmd = parse(&words)?;
        if matches!(cmd, Command::Quit { .. }) {
            return Err("`quit` isn't a step: the run closes the browser".into());
        }
        self.driver.run_op(&cmd).map(|o| o.result).map_err(|e| format!("{e:#}"))
    }

    /// `goto /cart` against the base URL.
    fn absolute(&self, words: &[String]) -> Result<Vec<String>, String> {
        let mut words = words.to_vec();
        if matches!(words.first().map(String::as_str), Some("goto" | "open" | "tab-new")) {
            if let Some(url) = words.get(1).filter(|u| u.starts_with('/')) {
                let base = self.opts.base.as_deref().ok_or_else(|| {
                    format!("{url} is relative: give the run a base URL (`base <url>` in the file or --base-url)")
                })?;
                words[1] = format!("{}{url}", base.trim_end_matches('/'));
            }
        }
        Ok(words)
    }

    fn expect(&mut self, e: &Expect, scope: &Scope) -> Result<(), String> {
        let limit = Duration::from_millis(e.timeout_ms.unwrap_or(self.opts.expect_ms));
        let deadline = Instant::now() + limit;
        let value = scope.word(&e.value)?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (holds, actual) = self.check(e, &value, scope, left)?;
            if holds != e.negate {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(describe(e, &value, &actual, limit));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// One look: whether the subject holds, and what was seen.
    fn check(&mut self, e: &Expect, value: &str, scope: &Scope, left: Duration) -> Result<(bool, String), String> {
        // Each read waits for its element at most this long, so an
        // expectation keeps being re-read until its own deadline.
        let poll_ms = left.as_millis().clamp(50, 1000).to_string();
        Ok(match &e.subject {
            Subject::Url | Subject::Title => {
                let v = self.command(&["url".into()])?;
                let key = if e.subject == Subject::Url { "url" } else { "title" };
                let actual = v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                (compare(&actual, e.matcher, value), actual)
            }
            Subject::Text(t) => {
                let t = scope.word(t)?;
                if e.negate {
                    // Gone everywhere a user could read it: look now, again later.
                    let js = format!(
                        "(() => {{ const t = {}; const docs = [document]; for (const f of document.querySelectorAll('iframe,frame')) {{ try {{ if (f.contentDocument) docs.push(f.contentDocument); }} catch (_) {{}} }} return docs.some((d) => d.body && d.body.innerText.includes(t)); }})()",
                        serde_json::to_string(&t).unwrap_or_default()
                    );
                    let v = self.command(&["eval".into(), js])?;
                    let present = v.as_bool() == Some(true);
                    (present, if present { "present".into() } else { "absent".into() })
                } else {
                    // navigera's own text wait: frames included.
                    match self.command(&[
                        "wait".into(),
                        "--text".into(),
                        t,
                        "--timeout-ms".into(),
                        left.as_millis().max(1).to_string(),
                    ]) {
                        Ok(_) => (true, "present".into()),
                        Err(_) => (false, "absent".into()),
                    }
                }
            }
            Subject::Var(raw) => {
                let actual = scope.word(raw)?;
                (compare(&actual, e.matcher, value), actual)
            }
            Subject::Js(code) => {
                let code = scope.word(code)?;
                match self.command(&["eval".into(), code]) {
                    Ok(v) => {
                        let actual = match &v {
                            Value::String(s) => s.clone(),
                            Value::Null => "null".into(),
                            other => other.to_string(),
                        };
                        let holds = if e.matcher == Matcher::Holds {
                            !matches!(&v, Value::Null | Value::Bool(false))
                                && v != serde_json::json!(0)
                                && v != serde_json::json!("")
                        } else {
                            compare(&actual, e.matcher, value)
                        };
                        (holds, actual)
                    }
                    Err(err) => (false, err),
                }
            }
            Subject::Request { method, url, status } => {
                let url = scope.word(url)?;
                let v = self.command(&["network".into(), "--limit".into(), "1000".into()])?;
                // `#3 POST 200 Fetch /api/cart 196 B 3 ms`; another origin's
                // URL in full, matched by its path too.
                let mut seen = Vec::new();
                let mut hit = false;
                for l in v.as_str().unwrap_or("").lines() {
                    let w: Vec<&str> = l.split_whitespace().collect();
                    if w.len() < 5 {
                        continue;
                    }
                    let (m, st, u) = (w[1], w[2], w[4]);
                    let path = u.split_once("://").and_then(|(_, r)| r.find('/').map(|i| &r[i..])).unwrap_or(u);
                    if crate::expect::glob(&url, u) || crate::expect::glob(&url, path) {
                        seen.push(format!("{m} {u} {st}"));
                        if (method == "*" || m.eq_ignore_ascii_case(method))
                            && status.as_deref().is_none_or(|s| s == st)
                        {
                            hit = true;
                        }
                    }
                }
                let actual = if seen.is_empty() { format!("no request to {url}") } else { seen.join(", ") };
                (hit, actual)
            }
            Subject::Cookie(name) => {
                let name = scope.word(name)?;
                let v = self.command(&["cookies".into(), "--all".into(), "--json".into()])?;
                let found = v.as_array().and_then(|a| {
                    a.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(name.as_str())).cloned()
                });
                match found {
                    Some(c) => {
                        let val = c.get("value").and_then(Value::as_str).unwrap_or("").to_string();
                        (compare(&val, e.matcher, value), val)
                    }
                    None => (false, "no such cookie".into()),
                }
            }
            Subject::Storage { area, key } => {
                let key = scope.word(key)?;
                match self.command(&["storage".into(), area.clone(), "get".into(), key]) {
                    Ok(Value::String(val)) => (compare(&val, e.matcher, value), val),
                    Ok(_) => (false, "no such key".into()),
                    Err(err) => (false, err),
                }
            }
            Subject::Errors => {
                let v = self.command(&["errors".into(), "--json".into()])?;
                let list = v.as_array().cloned().unwrap_or_default();
                (
                    list.is_empty(),
                    if list.is_empty() { "none".into() } else { serde_json::to_string(&list).unwrap_or_default() },
                )
            }
            Subject::Target { flags, prop } => {
                let flags = scope.words(flags)?;
                match prop {
                    Prop::State(state) => {
                        let (asked, flip) =
                            if state == "unchecked" { ("checked", true) } else { (state.as_str(), false) };
                        let mut words = vec!["is".to_string(), asked.to_string()];
                        words.extend(flags);
                        words.extend(["--timeout-ms".to_string(), poll_ms]);
                        match self.command(&words) {
                            Ok(v) => {
                                let b = (v.as_bool() == Some(true)) != flip;
                                (b, if b { state.clone() } else { format!("not {state}") })
                            }
                            Err(err) => (false, err),
                        }
                    }
                    Prop::Count => {
                        let css = flags.get(1).cloned().unwrap_or_default();
                        let v = self.command(&["get".into(), "count".into(), css])?;
                        let actual = v.to_string();
                        (compare(&actual, e.matcher, value), actual)
                    }
                    Prop::Text | Prop::Value | Prop::Attr(_) => {
                        let mut words = vec!["get".to_string()];
                        match prop {
                            Prop::Text => words.push("text".into()),
                            Prop::Value => words.push("value".into()),
                            Prop::Attr(name) => words.extend(["attr".to_string(), name.clone()]),
                            _ => unreachable!(),
                        }
                        words.extend(flags);
                        words.extend(["--timeout-ms".to_string(), poll_ms]);
                        match self.command(&words) {
                            Ok(v) => {
                                let actual = match v {
                                    Value::String(s) => s.trim().to_string(),
                                    Value::Null => String::new(),
                                    other => other.to_string(),
                                };
                                (compare(&actual, e.matcher, value), actual)
                            }
                            Err(err) => (false, err),
                        }
                    }
                }
            }
        })
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.driver.session().close();
    }
}

fn parse(words: &[String]) -> Result<Command, String> {
    let parsed =
        parse_args(std::iter::once("navigera".to_string()).chain(words.iter().cloned())).map_err(|e| match e {
            navigera::protocol::ArgsError::Help(_) => format!("`{}` needs more", words.join(" ")),
            navigera::protocol::ArgsError::Invalid(m) => m.lines().next().unwrap_or("").to_string(),
        })?;
    if parsed.start || parsed.mcp || parsed.local.is_some() {
        return Err(format!("`{}` isn't a browser step", words.first().map(String::as_str).unwrap_or("")));
    }
    parsed.command.ok_or_else(|| format!("`{}` isn't a browser step", words.join(" ")))
}

fn describe(e: &Expect, value: &str, actual: &str, limit: Duration) -> String {
    let not = if e.negate { "not " } else { "" };
    let what = match &e.subject {
        Subject::Url => "url".to_string(),
        Subject::Title => "title".to_string(),
        Subject::Text(t) => format!("text {t:?}"),
        Subject::Var(v) => v.clone(),
        Subject::Errors => "no page errors".to_string(),
        Subject::Js(c) => format!("js {c:?}"),
        Subject::Request { method, url, status } => {
            format!("request {method} {url}{}", status.as_ref().map(|s| format!(" status {s}")).unwrap_or_default())
        }
        Subject::Cookie(c) => format!("cookie {c}"),
        Subject::Storage { area, key } => format!("storage {area} {key}"),
        Subject::Target { flags, prop } => {
            let p = match prop {
                Prop::State(s) => s.clone(),
                Prop::Text => "text".into(),
                Prop::Value => "value".into(),
                Prop::Attr(a) => format!("attr {a}"),
                Prop::Count => "count".into(),
            };
            format!("{} {p}", flags.join(" "))
        }
    };
    let want = if e.matcher == Matcher::Holds { String::new() } else { format!(" {} {value:?}", e.matcher.word()) };
    let seen = if actual.len() > 300 {
        format!("{}…", &actual[..actual.floor_char_boundary(300)])
    } else {
        actual.to_string()
    };
    format!("expected {not}{what}{want} within {} ms; saw {seen:?}", limit.as_millis())
}

fn evidence_name(file: &Path, scenario: &str) -> String {
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("scenario");
    let slug: String = scenario
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("{stem}-{}", &slug[..slug.len().min(60)])
}

/// Variables a scenario sees: what it saved, the run's `--var`s, the
/// file's `set`s, then the environment.
struct Scope {
    /// Flow arguments, innermost last.
    layers: Vec<BTreeMap<String, String>>,
    saved: BTreeMap<String, String>,
    /// The scenario's data row.
    row: BTreeMap<String, String>,
    run: BTreeMap<String, String>,
    file: BTreeMap<String, String>,
    noop: Option<String>,
    /// Optional steps that didn't happen.
    notes: Vec<String>,
}

impl Scope {
    fn new(suite: &Suite, scenario: &Scenario, opts: &Options) -> Self {
        Scope {
            layers: Vec::new(),
            saved: BTreeMap::new(),
            row: scenario.vars.clone(),
            run: opts.vars.clone(),
            file: suite.vars.clone(),
            noop: None,
            notes: Vec::new(),
        }
    }

    /// A flow's arguments, then what was saved, the data row, `--var`,
    /// the file's `set`s, the environment.
    fn lookup(&self, name: &str) -> Option<String> {
        self.layers
            .last()
            .and_then(|l| l.get(name))
            .or_else(|| self.saved.get(name))
            .or_else(|| self.row.get(name))
            .or_else(|| self.run.get(name))
            .or_else(|| self.file.get(name))
            .cloned()
            .or_else(|| std::env::var(name).ok())
    }

    fn word(&self, w: &str) -> Result<String, String> {
        interpolate(w, &|n| self.lookup(n))
    }

    fn words(&self, ws: &[String]) -> Result<Vec<String>, String> {
        ws.iter().map(|w| self.word(w)).collect()
    }
}
