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
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub ms: f64,
    #[allow(dead_code)]
    pub steps: Vec<StepResult>,
    pub failure: Option<Failure>,
}

impl ScenarioResult {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

/// Which scenarios a run takes, after `--grep` and `--tag`.
pub fn selected<'a>(suites: &'a [Suite], opts: &Options) -> Vec<(&'a Suite, &'a Scenario)> {
    let mut out = Vec::new();
    for suite in suites {
        for sc in &suite.scenarios {
            if let Some(g) = &opts.grep {
                if !sc.name.to_lowercase().contains(&g.to_lowercase()) {
                    continue;
                }
            }
            if !opts.tags.is_empty() && !opts.tags.iter().any(|t| sc.tags.contains(t)) {
                continue;
            }
            out.push((suite, sc));
        }
    }
    out
}

pub struct Runner {
    driver: Driver,
    opts: Options,
    fresh: bool,
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
        Ok(Runner { driver, opts, fresh: true })
    }

    pub fn run(&mut self, suite: &Suite, scenario: &Scenario) -> ScenarioResult {
        let started = Instant::now();
        let mut scope = Scope::new(suite, &self.opts);
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
                });
            }
        }
        self.fresh = false;
        let all: Vec<&Step> = suite.before_each.iter().chain(&scenario.steps).collect();
        if failure.is_none() {
            for step in all {
                let t = Instant::now();
                let res = self.step(step, &mut scope);
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
            if let Err(message) = self.step(step, &mut scope) {
                if failure.is_none() {
                    failure = Some(self.failure(step, message, &scope, suite, scenario));
                }
            }
        }
        ScenarioResult {
            file: suite.file.clone(),
            name: scenario.name.clone(),
            line: scenario.line,
            ms: started.elapsed().as_secs_f64() * 1000.0,
            steps,
            failure,
        }
    }

    fn failure(&mut self, step: &Step, message: String, scope: &Scope, suite: &Suite, sc: &Scenario) -> Failure {
        let at = self.driver.run(&Command::Url {}).ok().map(|v| {
            (
                v.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            )
        });
        let shot = self.opts.evidence.join(evidence_name(&suite.file, &sc.name)).join("screenshot.png");
        let screenshot = std::fs::create_dir_all(shot.parent().unwrap_or(Path::new(".")))
            .ok()
            .and_then(|_| {
                let cmd = parse(&["screenshot".into(), "--path".into(), shot.display().to_string()]).ok()?;
                self.driver.run(&cmd).ok()
            })
            .map(|_| shot);
        Failure { line: step.line, text: step.text.clone(), message, at, hint: scope.noop.clone(), screenshot }
    }

    fn step(&mut self, step: &Step, scope: &mut Scope) -> Result<(), String> {
        match &step.action {
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
    saved: BTreeMap<String, String>,
    run: BTreeMap<String, String>,
    file: BTreeMap<String, String>,
    noop: Option<String>,
}

impl Scope {
    fn new(suite: &Suite, opts: &Options) -> Self {
        Scope { saved: BTreeMap::new(), run: opts.vars.clone(), file: suite.vars.clone(), noop: None }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        self.saved
            .get(name)
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
