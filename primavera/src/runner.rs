//! Runs scenarios on one browser, in-process through the navigera library:
//! no session server, no socket, no process per step. Each scenario starts
//! from a clean browser context (`reset`: no cookies, storage or tabs from
//! the one before).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use navigera::protocol::{parse_args, Command, Driver, SessionConfig};
use serde_json::Value;

use crate::expect::{compare, Expect, HttpRead, Matcher, Prop, RequestRead, Shot, Subject};
use crate::suite::{interpolate, Action, Scenario, Step, Suite};

/// The state each `auth` block saved this run (or an earlier one within
/// its TTL), shared by the workers: the first scenario that needs one runs
/// it, the others wait for it.
#[derive(Debug, Default)]
pub struct AuthCache {
    slots: Mutex<HashMap<String, AuthSlot>>,
}

/// One auth's state file once it is known, or why signing in failed.
type AuthSlot = Arc<Mutex<Option<Result<PathBuf, String>>>>;

impl AuthCache {
    fn slot(&self, name: &str) -> AuthSlot {
        self.slots.lock().map(|mut m| m.entry(name.to_string()).or_default().clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub base: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub headed: bool,
    pub chromium: Option<String>,
    /// navigera's engine: `chrome` or `lightpanda`.
    pub engine: String,
    /// How long an `expect` retries (ms) unless it says otherwise.
    pub expect_ms: u64,
    /// navigera's element wait for steps (`--wait-ms`).
    pub wait_ms: Option<f64>,
    pub grep: Option<String>,
    pub tags: Vec<String>,
    /// Where failure evidence (a screenshot) goes.
    pub evidence: PathBuf,
    /// `${run.id}`: the same for every scenario of a run, different from
    /// run to run.
    pub run_id: String,
    /// `${secret:NAME}`s from `--secrets <file>` (the environment comes
    /// first).
    pub secrets: BTreeMap<String, String>,
    /// `--pause-on-failure`: keep the browser at a failure for a look.
    pub pause: bool,
    pub auth: Arc<AuthCache>,
    /// `--update-screenshots`: a screenshot that differs becomes the baseline.
    pub update_shots: bool,
    /// `--trace`: record what each step saw.
    pub trace: crate::trace::Trace,
    /// `--compare-report`: what compara keeps when a step doesn't say.
    pub compare_report: Vec<String>,
}

impl Options {
    /// A run's defaults; `stamp` (seconds) names its evidence directory.
    pub fn new(stamp: u64) -> Self {
        Options {
            base: std::env::var("PRIMAVERA_BASE_URL").ok().filter(|v| !v.is_empty()),
            vars: BTreeMap::new(),
            headed: false,
            chromium: None,
            engine: std::env::var("NAVIGERA_ENGINE")
                .ok()
                .filter(|e| !e.trim().is_empty())
                .unwrap_or_else(|| "chrome".into()),
            expect_ms: 5000,
            wait_ms: None,
            grep: None,
            tags: Vec::new(),
            evidence: PathBuf::from(format!(".primavera/runs/{stamp}")),
            run_id: format!("{stamp}-{}", std::process::id()),
            secrets: BTreeMap::new(),
            pause: false,
            auth: Default::default(),
            update_shots: false,
            trace: Default::default(),
            compare_report: vec!["html".into()],
        }
    }
}

/// One step that ran, for the HTML report.
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
    /// Where the step that failed is written when that isn't the
    /// scenario's own line (a step of a flow, maybe in a `use`d file):
    /// file, line, text. `fix` changes that line.
    pub source: Option<(PathBuf, usize, String)>,
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub tags: Vec<String>,
    pub ms: f64,
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
    /// Which of `--repeat`'s runs (0 when not repeated); the name says it too.
    pub rep: u32,
    /// When it started (Unix ms), for reports with a timeline.
    pub started: f64,
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

impl ScenarioResult {
    /// A result that ran no steps: its `before all` failed, or it is an
    /// `after all` that did.
    pub fn failed(suite: &Suite, name: String, line: usize, tags: Vec<String>, failure: Failure) -> Self {
        ScenarioResult {
            file: suite.file.clone(),
            name,
            line,
            tags,
            ms: 0.0,
            steps: Vec::new(),
            failure: Some(failure),
            attempt: 1,
            retried: Vec::new(),
            saved: BTreeMap::new(),
            notes: Vec::new(),
            worker: 0,
            rep: 0,
            started: now_ms(),
        }
    }

    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    pub fn flaky(&self) -> bool {
        self.passed() && !self.retried.is_empty()
    }

    /// `file::name`, the key of `.primavera/timings.json`.
    pub fn key(&self) -> String {
        format!("{}::{}", self.file.display(), self.base_name())
    }

    /// The scenario's name without `--repeat`'s ` #k`.
    pub fn base_name(&self) -> &str {
        match (self.rep, self.name.rsplit_once(" #")) {
            (1.., Some((base, _))) => base,
            _ => &self.name,
        }
    }
}

/// A scenario to run: indexes into the run's suites.
#[derive(Debug, Clone, Copy)]
pub struct Job {
    pub suite: usize,
    pub scenario: usize,
    /// `--repeat`'s run number (0 when not repeated).
    pub rep: u32,
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
            out.push(Job { suite: si, scenario: ci, rep: 0 });
        }
    }
    out
}

pub struct Runner {
    driver: Driver,
    /// What launched it: Lightpanda (one context per process) is launched
    /// again for each scenario.
    config: SessionConfig,
    opts: Options,
    fresh: bool,
    attempt: u32,
    /// Which of `--repeat`'s runs is under way (0 when not repeated).
    pub rep: u32,
}

impl Runner {
    /// Start the browser. An error here is infrastructure, not a test result.
    pub fn launch(opts: Options) -> anyhow::Result<Self> {
        let config = SessionConfig {
            headed: opts.headed,
            chromium: opts.chromium.clone(),
            engine: opts.engine.clone(),
            wait_ms: opts.wait_ms,
            // A pause leaves the browser to another navigera session: it
            // attaches over the DevTools port.
            transport: opts.pause.then(|| "ws".to_string()),
            ..SessionConfig::default()
        };
        let driver = Driver::launch(&config)?;
        Ok(Runner { driver, config, opts, fresh: true, attempt: 1, rep: 0 })
    }

    /// The browser, as the last scenario left it.
    pub fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }

    /// Another runner on this one's browser, in a browser context of its
    /// own: a worker without a browser of its own.
    pub fn fork(&self) -> anyhow::Result<Self> {
        Ok(Runner {
            driver: self.driver.fork()?,
            config: self.config.clone(),
            opts: self.opts.clone(),
            fresh: true,
            attempt: 1,
            rep: 0,
        })
    }

    /// A clean browser context for every scenario but the first one of
    /// the run (a new browser is clean already): no cookies, storage, tabs,
    /// routes, init scripts or `set` emulation from the one before.
    fn clean(&mut self, line: usize) -> Result<(), Box<Failure>> {
        let fresh = std::mem::replace(&mut self.fresh, false);
        if fresh {
            return Ok(());
        }
        let clean = if navigera::browser::is_lightpanda(&self.config.engine) {
            Driver::launch(&self.config).map(|d| {
                std::mem::replace(&mut self.driver, d).session().close();
            })
        } else {
            self.driver.run_op(&Command::Reset { all: true }).map(|_| ())
        };
        clean.map_err(|e| {
            Box::new(Failure {
                line,
                text: "(reset)".into(),
                message: format!("could not start from a clean browser context: {e:#}"),
                at: None,
                hint: None,
                screenshot: None,
                nearest: Vec::new(),
                source: None,
            })
        })
    }

    /// `file_vars`: what the file's `before all` saved.
    pub fn run(
        &mut self,
        suite: &Suite,
        scenario: &Scenario,
        attempt: u32,
        file_vars: &BTreeMap<String, String>,
    ) -> ScenarioResult {
        self.attempt = attempt;
        let started = Instant::now();
        let started_ms = now_ms();
        let mut scope = Scope::new(suite, &scenario.name, scenario.vars.clone(), &self.opts, file_vars);
        let mut steps = Vec::new();
        let mut failure = self.clean(scenario.line).err().map(|f| *f);
        let downloads = if failure.is_none() { self.own_downloads(suite, &scenario.name) } else { None };
        let all: Vec<&Step> = suite.before_each.iter().chain(&scenario.steps).collect();
        let tracing = self.opts.trace != crate::trace::Trace::Off && failure.is_none();
        let mut trace: Vec<Value> = Vec::new();
        let mut ax_at_failure = None;
        let mut seen = (0usize, 0u64);
        if tracing {
            seen = (self.console_count(), self.last_request());
        }
        if failure.is_none() {
            for step in all {
                let t = Instant::now();
                let res = self.step(step, &mut scope, suite);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                steps.push(StepResult { line: step.line, text: step.text.clone(), ms, ok: res.is_ok() });
                if tracing {
                    trace.push(self.trace_step(step, ms, res.as_ref().err().map(String::as_str), &mut seen));
                    if res.is_err() {
                        ax_at_failure = match self.command(&["ax".into(), "--limit".into(), "0".into()]) {
                            Ok(Value::String(t)) => Some(t),
                            _ => None,
                        };
                    }
                }
                if let Err(message) = res {
                    failure = Some(self.failure(step, message, &scope, suite, &scenario.name));
                    break;
                }
            }
        }
        // `--soft` expectations that didn't hold fail the scenario now.
        if failure.is_none() && !scope.soft.is_empty() {
            let (first, _) = scope.soft[0].clone();
            let all: Vec<String> = scope.soft.iter().map(|(st, m)| format!("line {}: {m}", st.line)).collect();
            let n = all.len();
            let message = format!("{n} soft expectation(s) didn't hold:\n{}", all.join("\n"));
            failure = Some(self.failure(&first, message, &scope, suite, &scenario.name));
        }
        if let (true, Some(f)) = (self.opts.pause, &failure) {
            self.pause(f, suite);
        }
        // `after each` runs whatever happened; its own failures count only
        // when the scenario had passed.
        for step in &suite.after_each {
            if let Err(message) = self.step(step, &mut scope, suite) {
                if failure.is_none() {
                    failure = Some(self.failure(step, message, &scope, suite, &scenario.name));
                }
            }
        }
        let mut result = ScenarioResult {
            file: suite.file.clone(),
            name: scenario.name.clone(),
            line: scenario.line,
            tags: scenario.tags.clone(),
            ms: started.elapsed().as_secs_f64() * 1000.0,
            steps,
            failure,
            attempt,
            retried: Vec::new(),
            saved: BTreeMap::new(),
            notes: Vec::new(),
            worker: 0,
            rep: 0,
            started: started_ms,
        };
        let mask = scope.masker();
        if tracing && (self.opts.trace == crate::trace::Trace::All || result.failure.is_some()) {
            let dir = self.evidence_dir(suite, &scenario.name);
            let head = serde_json::json!({
                "scenario": result.name, "file": suite.file.display().to_string(), "line": result.line,
                "passed": result.failure.is_none(), "ms": result.ms,
            });
            let text: String = std::iter::once(head).chain(trace).map(|v| mask(&v.to_string()) + "\n").collect();
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join("trace.jsonl"), text);
                if let Some(ax) = ax_at_failure {
                    let _ = std::fs::write(dir.join("ax.txt"), mask(&ax));
                }
            }
        }
        result.saved = scope.saved.into_iter().map(|(k, v)| (k, mask(&v))).collect();
        result.notes = scope.notes.iter().map(|n| mask(n)).collect();
        for step in result.steps.iter_mut() {
            step.text = mask(&step.text);
        }
        if let Some(f) = result.failure.as_mut() {
            mask_failure(f, &mask);
        }
        tidy(downloads);
        result
    }

    /// The scenario's (or hook's) evidence: its screenshot, its downloads.
    fn evidence_dir(&self, suite: &Suite, label: &str) -> PathBuf {
        let mut dir = match self.rep {
            0 => evidence_name(&suite.file, label),
            k => evidence_name(&suite.file, &format!("{label} #{k}")),
        };
        if self.attempt > 1 {
            dir.push_str(&format!("-try{}", self.attempt));
        }
        self.opts.evidence.join(dir)
    }

    /// Downloads go to a directory of the scenario's own, beside its
    /// evidence, so a file name never carries over from an earlier
    /// scenario (`report (2).csv`); `tidy` removes it when nothing came.
    /// One a run before left files in (same `--evidence`) is kept, and the
    /// next free name taken.
    fn own_downloads(&mut self, suite: &Suite, label: &str) -> Option<PathBuf> {
        let base = self.evidence_dir(suite, label);
        let dir = (1..1000)
            .map(|i| if i == 1 { base.join("downloads") } else { base.join(format!("downloads-{i}")) })
            .find(|d| std::fs::read_dir(d).map_or(true, |mut e| e.next().is_none()))?;
        let cmd = parse(&["downloads".into(), "--dir".into(), dir.display().to_string()]).ok()?;
        self.driver.run(&cmd).ok().map(|_| dir)
    }

    /// A `before all` or `after all` block: in a clean context of its own;
    /// what it saved, or how it failed.
    pub fn hook(
        &mut self,
        suite: &Suite,
        steps: &[Step],
        label: &str,
        file_vars: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, Box<Failure>> {
        let line = steps.first().map(|s| s.line).unwrap_or(0);
        self.clean(line)?;
        let downloads = self.own_downloads(suite, label);
        let mut scope = Scope::new(suite, label, BTreeMap::new(), &self.opts, file_vars);
        for step in steps {
            if let Err(message) = self.step(step, &mut scope, suite) {
                let mut f = self.failure(step, format!("{label}: {message}"), &scope, suite, label);
                mask_failure(&mut f, &scope.masker());
                tidy(downloads);
                return Err(Box::new(f));
            }
        }
        tidy(downloads);
        Ok(scope.saved)
    }

    /// The browser stays as the failure left it, open to another navigera
    /// session, until Enter.
    fn pause(&mut self, f: &Failure, suite: &Suite) {
        // The tab itself, so the look is at the page the failure left.
        let ws = self.driver.session().page_endpoint().unwrap_or_else(|| self.driver.session().endpoint());
        eprintln!(
            "PAUSED  {}:{}  the browser stays as the failure left it; look at it with\n        navigera -s pv start --attach {ws}\n        navigera -s pv ax\n        Enter goes on with the run (Ctrl-C stops it)",
            suite.file.display(),
            f.line
        );
        if enter(None).is_err() {
            // No one can press Enter: an agent looking from elsewhere gets
            // a while (PRIMAVERA_PAUSE_S, default 600 s); CI none.
            let ci = under_ci();
            let secs: u64 = std::env::var("PRIMAVERA_PAUSE_S").ok().and_then(|v| v.parse().ok()).unwrap_or(if ci {
                0
            } else {
                600
            });
            if secs == 0 {
                eprintln!("        (no terminal input, and CI: not pausing; PRIMAVERA_PAUSE_S=<s> to pause anyway)");
            } else {
                eprintln!("        (no terminal input here: going on in {secs} s; PRIMAVERA_PAUSE_S sets how long)");
                std::thread::sleep(Duration::from_secs(secs));
            }
        }
    }

    fn failure(&mut self, step: &Step, message: String, scope: &Scope, suite: &Suite, label: &str) -> Failure {
        let at = self.driver.run(&Command::Url {}).ok().map(|v| {
            (
                v.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            )
        });
        let shot = self.evidence_dir(suite, label).join("screenshot.png");
        let screenshot = std::fs::create_dir_all(shot.parent().unwrap_or(Path::new(".")))
            .ok()
            .and_then(|_| {
                let cmd = parse(&["screenshot".into(), "--path".into(), shot.display().to_string()]).ok()?;
                self.driver.run(&cmd).ok()
            })
            .map(|_| shot);
        // A step of a flow failed: what comes closest is to its element.
        let inner = scope.inner.clone();
        let action = inner.as_ref().map_or(&step.action, |(_, s)| &s.action);
        let nearest = if message.contains("no element") { self.nearest(action, scope) } else { Vec::new() };
        let source = inner.map(|(file, s)| (file, s.line, s.text));
        Failure {
            line: step.line,
            text: step.text.clone(),
            message,
            at,
            hint: scope.noop.clone(),
            screenshot,
            nearest,
            source,
        }
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
        let mut ranked = match self.command(&["ax".into(), "--format".into(), "json".into()]) {
            Ok(ax) => crate::nearest::rank(&ax, &want, 3),
            Err(_) => Vec::new(),
        };
        // Unsure (nothing close, or the two best close together): with a
        // decision model configured, its pick on the page as it is goes first.
        let unsure = ranked.first().is_none_or(|t| t.2 < 0.6) || (ranked.len() > 1 && ranked[0].2 - ranked[1].2 < 0.1);
        if unsure && navigera::decide::Decider::from_env().is_some() {
            if let Some(pick) = self.decide(&want) {
                ranked.retain(|r| r.1 != pick.1);
                ranked.insert(0, pick);
                ranked.truncate(3);
            }
        }
        ranked
    }

    /// The decision model's pick for what a step wanted: the element (marked
    /// `decider`), the locator that finds it again, its probability.
    fn decide(&mut self, want: &crate::nearest::Wanted) -> Option<(String, String, f64)> {
        let what = match (&want.role, &want.name) {
            (Some(r), Some(n)) => format!("the {r} {n:?}"),
            (None, Some(n)) => format!("the element {n:?}"),
            (Some(r), None) => format!("the {r}"),
            (None, None) => return None,
        };
        let found = self.command(&["find".into(), what]).ok()?;
        let node = found["ref"].as_u64()?;
        let located = self.command(&["locate".into(), node.to_string()]).ok()?;
        let flags: Vec<String> =
            located["locators"][0]["flags"].as_array()?.iter().filter_map(|f| f.as_str().map(String::from)).collect();
        let element = found["element"].as_str().unwrap_or("").trim_start_matches("- ").to_string();
        let p = found["p"].as_f64().unwrap_or(0.0);
        Some((
            format!("{element} (decider)"),
            flags.iter().map(|f| crate::fix::quote(f)).collect::<Vec<_>>().join(" "),
            p,
        ))
    }

    fn step(&mut self, step: &Step, scope: &mut Scope, suite: &Suite) -> Result<(), String> {
        scope.inner = None;
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
            Action::Save { name, command }
                if matches!(command.first().map(String::as_str), Some("json" | "header")) =>
            {
                // `save id = json $.id`, `save to = header Location`: from the last `http`.
                let what = scope.word(command.get(1).ok_or("save NAME = json <$.path> | header <name>")?)?;
                let read = if command[0] == "json" { HttpRead::Json(what) } else { HttpRead::Header(what) };
                let v = http_read(&read, scope)?
                    .ok_or_else(|| format!("the last http answer has no {}", command.join(" ")))?;
                scope.saved.insert(name.clone(), v);
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
            Action::Expect(e) => match self.expect(e, scope) {
                // Noted; the scenario goes on and fails at its end.
                Err(m) if e.soft => {
                    scope.soft.push((step.clone(), m));
                    Ok(())
                }
                r => r,
            },
            Action::As(name) => {
                let path = self.login(name, suite)?;
                self.command(&["state".into(), "load".into(), path.display().to_string()]).map(|_| ())
            }
            Action::Handoff(until) => self.handoff(until.as_ref(), step, scope),
            Action::If { cond, steps } => {
                if self.expect(cond, scope).is_ok() {
                    for st in steps {
                        self.action(&st.action, st, scope, suite, depth)
                            .map_err(|e| format!("in the `if`, line {} `{}`: {e}", st.line, st.text))?;
                    }
                }
                Ok(())
            }
            Action::Exec { argv, timeout_ms, dir, steps } => {
                let argv = scope.words(argv)?;
                let limit = Duration::from_millis(timeout_ms.unwrap_or(60_000));
                let (code, out, err) = run_program(&argv, dir, limit, self.opts.base.as_deref())?;
                scope.saved.insert("exec.exit".into(), code.to_string());
                scope.saved.insert("exec.stdout".into(), out.trim_end_matches(['\n', '\r']).to_string());
                scope.saved.insert("exec.stderr".into(), err.trim_end_matches(['\n', '\r']).to_string());
                let exit = Subject::Var("${exec.exit}".into());
                let checked = steps.iter().any(|s| matches!(&s.action, Action::Expect(e) if e.subject == exit));
                if code != 0 && !checked {
                    let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                    let last = if last.len() > 300 { &last[..last.floor_char_boundary(300)] } else { last };
                    let said = if last.is_empty() { String::new() } else { format!(": {last}") };
                    return Err(format!("`{}` exited {code}{said}", shown(&argv)));
                }
                for st in steps {
                    self.action(&st.action, st, scope, suite, depth)
                        .map_err(|e| format!("under the `exec`, line {} `{}`: {e}", st.line, st.text))?;
                }
                Ok(())
            }
            Action::Http { method, url, headers, body, body_file, json, timeout_ms, follow, insecure, steps } => {
                let mut url = scope.word(url)?;
                if url.starts_with('/') {
                    let base = self.opts.base.as_deref().ok_or_else(|| {
                        format!("{url} is relative: give the run a base URL (`base <url>` in the file or --base-url)")
                    })?;
                    url = format!("{}{url}", base.trim_end_matches('/'));
                }
                let mut headers = scope.words(headers)?;
                let body = match (body, body_file) {
                    (Some(b), _) => Some(scope.word(b)?),
                    (None, Some(f)) => {
                        let f = scope.word(f)?;
                        Some(std::fs::read_to_string(&f).map_err(|e| format!("http body file {f}: {e}"))?)
                    }
                    (None, None) => None,
                };
                let has = |hs: &[String], name: &str| {
                    hs.iter().any(|h| h.split(':').next().is_some_and(|k| k.trim().eq_ignore_ascii_case(name)))
                };
                if *json && !has(&headers, "content-type") {
                    headers.push("Content-Type: application/json".into());
                }
                // The scenario's cookies go with it, unless it sets its own.
                if !has(&headers, "cookie") {
                    let jar = self.command(&["cookies".into(), "--url".into(), url.clone(), "--json".into()])?;
                    let pairs: Vec<String> = jar
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| Some(format!("{}={}", c.get("name")?.as_str()?, c.get("value")?.as_str()?)))
                        .collect();
                    if !pairs.is_empty() {
                        headers.push(format!("Cookie: {}", pairs.join("; ")));
                    }
                }
                let limit = Duration::from_millis(timeout_ms.unwrap_or(self.opts.expect_ms.max(30_000)));
                let how = crate::http::How { follow: *follow, insecure: *insecure };
                let r = crate::http::request(method, &url, &headers, body.as_deref(), limit, how)?;
                // And what it sets comes back to the browser.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                for v in &r.cookies {
                    if let Some(words) = crate::http::cookie_words(v, &url, now) {
                        self.command(&words)?;
                    }
                }
                scope.saved.insert("http.status".into(), r.status.to_string());
                scope.saved.insert("http.body".into(), r.body.clone());
                scope.http_headers = r.headers.clone();
                let status = Subject::Var("${http.status}".into());
                let checked = steps.iter().any(|s| matches!(&s.action, Action::Expect(e) if e.subject == status));
                if r.status >= 400 && !checked {
                    let said = r.body.trim();
                    let said = if said.len() > 200 { &said[..said.floor_char_boundary(200)] } else { said };
                    let said = if said.is_empty() { String::new() } else { format!(": {said}") };
                    return Err(format!("{method} {url} answered {}{said}", r.status));
                }
                for st in steps {
                    self.action(&st.action, st, scope, suite, depth)
                        .map_err(|e| format!("under the `http`, line {} `{}`: {e}", st.line, st.text))?;
                }
                Ok(())
            }
            Action::Gherkin(text) => Err(format!("no step matches {text:?}")),
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
                        // The innermost step that failed (set first, on the way out).
                        scope.inner.get_or_insert_with(|| (f.file.clone(), st.clone()));
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

    /// The state file of an `auth` block: saved by running it once in a
    /// browser context of its own, or reused from an earlier run within
    /// its TTL.
    fn login(&mut self, name: &str, suite: &Suite) -> Result<PathBuf, String> {
        let auth = suite.auths.get(name).ok_or_else(|| format!("no auth named {name:?}"))?;
        let slot = self.opts.auth.slot(name);
        let mut done = slot.lock().map_err(|_| "auth: a worker failed while signing in".to_string())?;
        if let Some(r) = done.as_ref() {
            return r.clone().map_err(|e| format!("auth {name} failed earlier in this run: {e}"));
        }
        // Keyed by where it signs in: another base URL, another state.
        let base = self.opts.base.clone().or_else(|| suite.base.clone()).unwrap_or_default();
        let key = base.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        let dir = std::path::absolute(".primavera/auth").unwrap_or_else(|_| PathBuf::from(".primavera/auth"));
        let path = dir.join(format!("{name}-{:08x}.json", key as u32));
        let reusable = auth.ttl_s.is_some_and(|ttl| {
            std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < ttl)
        });
        let result = if reusable { Ok(path) } else { self.sign_in(auth, suite, &dir, path) };
        *done = Some(result.clone());
        result
    }

    fn sign_in(
        &mut self,
        auth: &crate::suite::Auth,
        suite: &Suite,
        dir: &Path,
        path: PathBuf,
    ) -> Result<PathBuf, String> {
        let mut aux = self.fork().map_err(|e| format!("auth {}: {e:#}", auth.name))?;
        let label = format!("auth {}", auth.name);
        let mut scope = Scope::new(suite, &label, BTreeMap::new(), &self.opts, &BTreeMap::new());
        let file = auth.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for st in &auth.steps {
            aux.action(&st.action, st, &mut scope, suite, 0)
                .map_err(|e| scope.masker()(&format!("auth {}, {file}:{} `{}`: {e}", auth.name, st.line, st.text)))?;
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        aux.command(&["state".into(), "save".into(), path.display().to_string()])?;
        // Signed-in cookies: for this user only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(path)
    }

    /// The browser on screen for a person, until the clause holds (or
    /// Enter); then off screen again. Nothing to do when it already holds.
    fn handoff(&mut self, until: Option<&Expect>, step: &Step, scope: &mut Scope) -> Result<(), String> {
        if let Some(c) = until {
            if self.expect(&Expect { timeout_ms: Some(0), ..c.clone() }, scope).is_ok() {
                return Ok(());
            }
        }
        let show = !self.opts.headed;
        if show {
            if cfg!(target_os = "linux")
                && std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                return Err("handoff needs a person at a screen, and there is no display here (DISPLAY is unset): run it where there is one, or start signed in (`as <auth>`, `state load`)".into());
            }
            self.command(&["show".into()])?;
        }
        let what = match until {
            Some(_) => {
                format!("the run goes on once `{}` holds, or on Enter", step.text.trim_start_matches("handoff until "))
            }
            None => "press Enter when done".into(),
        };
        eprintln!("HANDOFF line {}: the browser is on screen for you; {what}", step.line);
        let res = match until {
            Some(c) => {
                let deadline = Instant::now() + Duration::from_millis(c.timeout_ms.unwrap_or(300_000));
                loop {
                    let look = Expect { timeout_ms: Some(0), ..c.clone() };
                    let entered = matches!(enter(Some(Duration::from_millis(250))), Ok(true));
                    match self.expect(&look, scope) {
                        Ok(()) => break Ok(()),
                        Err(e) if entered || Instant::now() >= deadline => {
                            break Err(format!("handoff: {e}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            None => match enter(None) {
                Ok(_) => Ok(()),
                Err(()) => {
                    Err("handoff waits for Enter, and there is no terminal input here: give it `until <expect clause>`"
                        .into())
                }
            },
        };
        if show {
            if let Err(e) = self.command(&["hide".into()]) {
                return res.and(Err(format!("handoff: back off screen: {e}")));
            }
        }
        res
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

    fn expect(&mut self, e: &Expect, scope: &mut Scope) -> Result<(), String> {
        let limit = Duration::from_millis(e.timeout_ms.unwrap_or(self.opts.expect_ms));
        let deadline = Instant::now() + limit;
        let value = scope.word(&e.value)?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (holds, actual) = self.check(e, &value, scope, left)?;
            if holds != e.negate {
                // The file that came: `${download.path}`, `${download.name}`.
                if let (Subject::Download(_) | Subject::DownloadCompare { .. }, false) = (&e.subject, e.negate) {
                    let name = Path::new(&actual).file_name().map(|n| n.to_string_lossy().into_owned());
                    scope.saved.insert("download.name".into(), name.unwrap_or_default());
                    scope.saved.insert("download.path".into(), actual);
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(describe(e, &value, &actual, limit));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// `expect screenshot`: take it (masks painted, animations stopped),
    /// crop it to its target, and compare it with the baseline beside the
    /// `.pv` file (`<file>.screenshots/<name>-<os>.png`). No baseline yet:
    /// it is written, and the step fails so a person looks at it first.
    fn screenshot(&mut self, shot: &Shot, scope: &Scope) -> Result<(bool, String), String> {
        let stem = scope.pv.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = scope.pv.parent().unwrap_or(Path::new(".")).join(format!("{stem}.screenshots"));
        let baseline = dir.join(format!("{}-{}.png", shot.name, std::env::consts::OS));
        let flags = scope.words(&shot.flags)?;
        // Still and masked while it is taken.
        let masks = serde_json::to_string(&shot.masks).unwrap_or_default();
        let freeze = format!(
            "(() => {{ const s = document.createElement('style'); s.id = '__pv_freeze'; \
             s.textContent = '*,*::before,*::after{{animation:none!important;transition:none!important;caret-color:transparent!important}}'; \
             document.documentElement.append(s); const n = []; for (const css of {masks}) {{ const all = document.querySelectorAll(css); \
             n.push(all.length); for (const e of all) {{ \
             const r = e.getBoundingClientRect(); const m = document.createElement('div'); m.className = '__pv_mask'; \
             m.style.cssText = `position:absolute;left:${{r.left + scrollX}}px;top:${{r.top + scrollY}}px;width:${{r.width}}px;height:${{r.height}}px;background:#ff00ff;z-index:2147483647;pointer-events:none`; \
             document.documentElement.append(m); }} }} return [scrollX, scrollY, devicePixelRatio, n]; }})()"
        );
        let unfreeze = "(() => { document.getElementById('__pv_freeze')?.remove(); for (const m of document.querySelectorAll('.__pv_mask')) m.remove(); return 1; })()";
        // The target in view first: scrolled inside its scroll containers
        // and frames, which a full-page capture shows only as they stand.
        if !flags.is_empty() {
            let mut w = vec!["scroll".to_string()];
            w.extend(flags.iter().cloned());
            self.command(&w)?;
        }
        let at = self.command(&["eval".into(), freeze])?;
        let n = |i: usize| at.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let (sx, sy, dpr) = (n(0), n(1), n(2).max(1.0));
        // A mask that hides nothing would let what it was for into the baseline.
        let counts = at.get(3).and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some((css, _)) = shot.masks.iter().zip(&counts).find(|(_, c)| c.as_u64() == Some(0)) {
            let _ = self.command(&["eval".into(), unfreeze.into()]);
            return Err(format!(
                "--mask {css:?} matches nothing on the page (masks are CSS selectors of the top page)"
            ));
        }
        // Its box where the capture will have it: page pixels, frames' offsets in.
        let target = if flags.is_empty() {
            None
        } else {
            let mut w = vec!["get".to_string(), "box".into()];
            w.extend(flags.iter().cloned());
            match self.command(&w) {
                Ok(b) => Some(b),
                Err(e) => {
                    let _ = self.command(&["eval".into(), unfreeze.into()]);
                    return Err(e);
                }
            }
        };
        let tmp = self.opts.evidence.join(format!("{stem}-{}-taking.png", shot.name));
        let _ = std::fs::create_dir_all(&self.opts.evidence);
        let mut words = vec!["screenshot".to_string(), "--path".into(), tmp.display().to_string()];
        if shot.full_page || !flags.is_empty() {
            words.push("--full-page".into());
        }
        let took = self.command(&words);
        let _ = self.command(&["eval".into(), unfreeze.into()]);
        took?;
        let bytes = std::fs::read(&tmp).map_err(|e| format!("screenshot {}: {e}", tmp.display()))?;
        let _ = std::fs::remove_file(&tmp);
        let mut img = crate::shot::decode(&bytes)?;
        if let Some(b) = target {
            let f = |k: &str| b.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            let px = |v: f64| (v * dpr).round().max(0.0) as u32;
            img = crate::shot::crop(&img, px(f("x") + sx), px(f("y") + sy), px(f("width")), px(f("height")));
        }
        let write = |path: &Path, img: &crate::shot::Image| -> Result<(), String> {
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            std::fs::write(path, crate::shot::encode(img)).map_err(|e| format!("{}: {e}", path.display()))
        };
        let Ok(base_bytes) = std::fs::read(&baseline) else {
            write(&baseline, &img)?;
            if self.opts.update_shots {
                return Ok((true, "baseline written".into()));
            }
            return Err(format!(
                "no baseline yet: wrote {} from this run; look at it, keep it with the tests, and run again",
                baseline.display()
            ));
        };
        let base = crate::shot::decode(&base_bytes).map_err(|e| format!("{}: {e}", baseline.display()))?;
        let verdict = crate::shot::compare(&base, &img, shot.threshold);
        let ok = match &verdict {
            Ok(d) => {
                d.pixels <= shot.max_pixels && shot.max_ratio.is_none_or(|r| d.pixels as f64 <= r * d.total as f64)
            }
            Err(_) => false,
        };
        if ok {
            return Ok((true, "it matches".into()));
        }
        if self.opts.update_shots {
            write(&baseline, &img)?;
            return Ok((true, "baseline updated".into()));
        }
        let actual = self.opts.evidence.join(format!("{stem}-{}-actual.png", shot.name));
        write(&actual, &img)?;
        Ok((
            false,
            match verdict {
                Ok(d) => {
                    let diff = self.opts.evidence.join(format!("{stem}-{}-diff.png", shot.name));
                    write(&diff, &d.image)?;
                    format!(
                        "{} of {} pixels differ ({:.2}%), {} allowed; this run {}, the differences {}",
                        d.pixels,
                        d.total,
                        100.0 * d.pixels as f64 / d.total.max(1) as f64,
                        shot.max_ratio
                            .map(|r| format!("{:.2}%", r * 100.0))
                            .unwrap_or_else(|| shot.max_pixels.to_string()),
                        actual.display(),
                        diff.display()
                    )
                }
                Err(why) => format!("{why}; this run {}", actual.display()),
            },
        ))
    }

    /// The latest finished download whose name matches `glob`: its path;
    /// or what downloads there are.
    fn download_done(&mut self, glob: &str) -> Result<(bool, String), String> {
        let v = self.command(&["downloads".into(), "--json".into()])?;
        let list = v.as_array().cloned().unwrap_or_default();
        let name = |d: &Value| d.get("file").and_then(Value::as_str).unwrap_or("").to_string();
        let done = list.iter().rev().find(|d| {
            crate::expect::glob(glob, &name(d)) && d.get("state").and_then(Value::as_str) == Some("completed")
        });
        Ok(match done {
            Some(d) => (true, d.get("path").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| name(d))),
            None => {
                let seen: Vec<String> = list
                    .iter()
                    .map(|d| format!("{} ({})", name(d), d.get("state").and_then(Value::as_str).unwrap_or("?")))
                    .collect();
                (false, if seen.is_empty() { "no download".into() } else { seen.join(", ") })
            }
        })
    }

    /// compara on a downloaded file and the expected one: nothing when they
    /// are the same, else its counts and what it wrote to the evidence
    /// directory (`report`: html, json, rows), each unless the step's own
    /// compara flags already say where.
    fn compara(&self, got: &str, expected: &str, flags: &[String], report: &[String]) -> Result<(), String> {
        let exe = std::env::var("PRIMAVERA_COMPARA").unwrap_or_else(|_| "compara".into());
        let stem = Path::new(got).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // Beside the scenario's downloads (`<evidence>/<scenario>/downloads/`).
        let dir = Path::new(got)
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.opts.evidence.clone());
        let _ = std::fs::create_dir_all(&dir);
        let given = |names: &[&str]| flags.iter().any(|f| names.contains(&f.as_str()));
        let mut outputs: Vec<(&str, &str, PathBuf)> = Vec::new();
        for (kind, flag, names, what, file) in [
            ("html", "-o", &["-o", "--out"][..], "the report", format!("{stem}-compare.html")),
            ("json", "--json", &["--json"][..], "the summary", format!("{stem}-compare.json")),
            ("rows", "--export-dir", &["--export-dir"][..], "the rows", format!("{stem}-compare-rows")),
        ] {
            if report.iter().any(|r| r == kind) && !given(names) {
                outputs.push((flag, what, dir.join(file)));
            }
        }
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["compare", got, expected]).args(flags);
        for (flag, _, path) in &outputs {
            cmd.arg(flag).arg(path);
        }
        let out = cmd.output().map_err(|e| {
            format!(
                "compare needs compara ({exe}: {e}; cargo install compara, npm i -g compara or pip install compara)"
            )
        })?;
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        let first = said.lines().find(|l| !l.trim().is_empty() && !l.starts_with("Report:")).unwrap_or("").trim();
        let written: Vec<String> = outputs.iter().map(|(_, what, path)| format!("{what} {}", path.display())).collect();
        let written = if written.is_empty() { String::new() } else { format!("; {}", written.join(", ")) };
        match out.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(format!("{got} differs from {expected}: {first}{written}")),
            _ => Err(format!("compara on {got} and {expected}: {first}")),
        }
    }

    /// How many console messages the tab has so far.
    fn console_count(&mut self) -> usize {
        match self.command(&["console".into(), "--json".into()]) {
            Ok(Value::Array(all)) => all.len(),
            _ => 0,
        }
    }

    /// The tab's latest request number.
    fn last_request(&mut self) -> u64 {
        match self.command(&["network".into()]) {
            Ok(Value::String(t)) => crate::trace::last_request(&t),
            _ => 0,
        }
    }

    /// A step's line of the trace: where the page is, and the console
    /// messages and requests since the step before (`seen`: how many
    /// messages, the last request number).
    fn trace_step(&mut self, step: &Step, ms: f64, error: Option<&str>, seen: &mut (usize, u64)) -> Value {
        let page = match self.command(&["url".into()]) {
            Ok(v) => (
                v.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            ),
            Err(_) => Default::default(),
        };
        let console = match self.command(&["console".into(), "--json".into()]) {
            Ok(Value::Array(all)) => {
                // A shorter list is a new tab's: all of it is new.
                let from = if all.len() < seen.0 { 0 } else { seen.0 };
                seen.0 = all.len();
                all[from..].iter().take(200).cloned().collect()
            }
            _ => Vec::new(),
        };
        let network = match self.command(&["network".into(), "--since".into(), seen.1.to_string()]) {
            Ok(Value::String(t)) => {
                seen.1 = crate::trace::last_request(&t).max(seen.1);
                t.lines().filter(|l| l.starts_with('#')).take(200).map(String::from).collect()
            }
            _ => Vec::new(),
        };
        crate::trace::step(step.line, &step.text, ms, error, page, console, network)
    }

    /// One look: whether the subject holds, and what was seen.
    fn check(&mut self, e: &Expect, value: &str, scope: &Scope, left: Duration) -> Result<(bool, String), String> {
        // Each read waits for its element at most this long, so an
        // expectation keeps being re-read until its own deadline. When the
        // element's absence is an answer (`hidden`, any `not …`), a read
        // doesn't wait for it to appear: that held every such look up by a
        // second (a dismissed banner took 2 s to be seen gone).
        let absence_answers =
            e.negate || matches!(&e.subject, Subject::Target { prop: Prop::State(s), .. } if s == "hidden");
        let poll_ms = if absence_answers { "0".to_string() } else { left.as_millis().clamp(50, 1000).to_string() };
        Ok(match &e.subject {
            Subject::Url | Subject::Title => {
                let v = self.command(&["url".into()])?;
                let key = if e.subject == Subject::Url { "url" } else { "title" };
                let actual = v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                (compare(&actual, e.matcher, value), actual)
            }
            Subject::Text { text, within: Some(flags) } => {
                let t = scope.word(text)?;
                let mut words = vec!["get".to_string(), "text".to_string()];
                words.extend(scope.words(flags)?);
                words.extend(["--timeout-ms".to_string(), poll_ms]);
                match self.command(&words) {
                    Ok(v) => {
                        let inside = v.as_str().unwrap_or("").to_string();
                        (inside.contains(&t), inside)
                    }
                    Err(err) => (false, err),
                }
            }
            Subject::Text { text: t, within: None } => {
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
            Subject::Accessible { selector, tags, ignore } => {
                // axe once per document: a page that moved on has lost it.
                if self.command(&["eval".into(), "typeof axe === 'object' && typeof axe.run === 'function'".into()])?
                    != Value::Bool(true)
                {
                    self.command(&["eval".into(), format!("(function () {{\n{AXE}\n}}).call(window), 1")])?;
                }
                let context = match selector {
                    Some(s) => serde_json::json!({"include": [scope.word(s)?]}).to_string(),
                    None => "document".to_string(),
                };
                let js = format!(
                    "axe.run({context}, {{ runOnly: {{ type: 'tag', values: {} }}, resultTypes: ['violations'] }}).then((r) => \
                     r.violations.filter((v) => !{}.includes(v.id)).map((v) => \
                     `${{v.id}} (${{v.impact}}, ${{v.nodes.length}}: ${{v.nodes.slice(0, 3).map((n) => n.target.join(' ')).join(', ')}}${{v.nodes.length > 3 ? ', …' : ''}}): ${{v.help}}`))",
                    serde_json::json!(tags),
                    serde_json::json!(ignore),
                );
                match self.command(&["eval".into(), js]) {
                    Ok(Value::Array(found)) if found.is_empty() => (true, "no violations".into()),
                    Ok(Value::Array(found)) => {
                        let lines: Vec<String> = found.iter().filter_map(|v| v.as_str().map(String::from)).collect();
                        (false, format!("{} violation(s): {}", lines.len(), lines.join("; ")))
                    }
                    Ok(other) => (false, format!("axe said {other}")),
                    Err(err) => (false, err),
                }
            }
            Subject::Metric(name) => {
                let v = self.command(&["eval".into(), format!("({METRIC_JS})({})", serde_json::json!(name))])?;
                match v.as_f64() {
                    Some(n) => {
                        let actual = format!("{}", (n * 1000.0).round() / 1000.0);
                        (compare(&actual, e.matcher, value), actual)
                    }
                    None => (false, "not measured yet".into()),
                }
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
            Subject::Request { method, url, status, read } => {
                let url = scope.word(url)?;
                let v = self.command(&["network".into(), "--limit".into(), "1000".into()])?;
                // `#3 POST 200 Fetch /api/cart 196 B 3 ms`; another origin's
                // URL in full, matched by its path too.
                let mut seen = Vec::new();
                let mut hits: Vec<String> = Vec::new();
                for l in v.as_str().unwrap_or("").lines() {
                    let w: Vec<&str> = l.split_whitespace().collect();
                    if w.len() < 5 {
                        continue;
                    }
                    let (n, m, st, u) = (w[0], w[1], w[2], w[4]);
                    let path = u.split_once("://").and_then(|(_, r)| r.find('/').map(|i| &r[i..])).unwrap_or(u);
                    if crate::expect::glob(&url, u) || crate::expect::glob(&url, path) {
                        seen.push(format!("{m} {u} {st}"));
                        if (method == "*" || m.eq_ignore_ascii_case(method))
                            && status.as_deref().is_none_or(|s| s == st)
                        {
                            hits.push(n.trim_start_matches('#').to_string());
                        }
                    }
                }
                let none = || if seen.is_empty() { format!("no request to {url}") } else { seen.join(", ") };
                match read {
                    RequestRead::Made => (!hits.is_empty(), none()),
                    RequestRead::Count => {
                        let n = hits.len().to_string();
                        (compare(&n, e.matcher, value), format!("{n} ({})", none()))
                    }
                    RequestRead::Json(path) => match hits.last() {
                        None => (false, none()),
                        Some(n) => {
                            let file =
                                std::env::temp_dir().join(format!("primavera-body-{}-{n}.json", std::process::id()));
                            let got = self.command(&[
                                "network".into(),
                                n.clone(),
                                "--body-file".into(),
                                file.display().to_string(),
                            ]);
                            let body = got.ok().and_then(|_| std::fs::read_to_string(&file).ok()).unwrap_or_default();
                            let _ = std::fs::remove_file(&file);
                            let steps = crate::expect::json_path(path)?;
                            match serde_json::from_str::<Value>(&body) {
                                Ok(json) => match crate::expect::json_at(&json, &steps) {
                                    Some(actual) => (compare(&actual, e.matcher, value), actual),
                                    None => (false, format!("no {path} in the response")),
                                },
                                Err(_) => {
                                    let cut: String = body.chars().take(120).collect();
                                    (false, format!("the response isn't JSON: {cut:?}"))
                                }
                            }
                        }
                    },
                }
            }
            Subject::DownloadCompare { glob, expected, flags, report } => {
                match self.download_done(&scope.word(glob)?)? {
                    (true, path) => {
                        let expected = scope.word(expected)?;
                        let flags = scope.words(flags)?;
                        let report = report.as_ref().unwrap_or(&self.opts.compare_report).clone();
                        self.compara(&path, &expected, &flags, &report)?;
                        (true, path)
                    }
                    seen => seen,
                }
            }
            Subject::Download(glob) => self.download_done(&scope.word(glob)?)?,
            Subject::Console => {
                let v = self.command(&["console".into(), "--json".into()])?;
                let texts: Vec<String> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m.get("text").and_then(Value::as_str).map(str::to_string))
                    .collect();
                let hit = texts.iter().any(|t| compare(t, e.matcher, value));
                (hit, if texts.is_empty() { "no console messages".into() } else { texts.join(" | ") })
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
            Subject::Ax { flags, tree } => {
                // The whole tree (or the target's): `--limit 0` cuts nothing.
                let mut words = vec!["ax".to_string(), "--limit".into(), "0".into()];
                words.extend(scope.words(flags)?);
                let template = crate::axtree::compile(&scope.words(tree)?)?;
                match self.command(&words) {
                    Ok(Value::String(page)) => match crate::axtree::matches(&template, &page) {
                        Ok(()) => (true, "it matches".into()),
                        Err(miss) => (false, miss),
                    },
                    Ok(other) => (false, other.to_string()),
                    Err(err) => (false, err),
                }
            }
            Subject::Screenshot(shot) => self.screenshot(shot, scope)?,
            Subject::Http(read) => match http_read(read, scope)? {
                Some(actual) if e.matcher == Matcher::Holds => (actual != "null", actual),
                Some(actual) => (compare(&actual, e.matcher, value), actual),
                None => (false, "absent".into()),
            },
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
                    Prop::State(state) if state == "empty" => {
                        // No text, and no value when it takes one.
                        let read = |r: &mut Self, what: &str| -> Result<String, String> {
                            let mut words = vec!["get".to_string(), what.to_string()];
                            words.extend(flags.clone());
                            words.extend(["--timeout-ms".to_string(), poll_ms.clone()]);
                            r.command(&words).map(|v| match v {
                                Value::String(s) => s,
                                Value::Null => String::new(),
                                other => other.to_string(),
                            })
                        };
                        match read(self, "text") {
                            Ok(text) => {
                                let value = read(self, "value").unwrap_or_default();
                                let empty = text.trim().is_empty() && value.is_empty();
                                let seen = if empty {
                                    "empty".to_string()
                                } else {
                                    format!("{:?}", if value.is_empty() { text.trim() } else { &value })
                                };
                                (empty, seen)
                            }
                            Err(err) => (false, err),
                        }
                    }
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
                    Prop::Text | Prop::Value | Prop::Attr(_) | Prop::Style(_) => {
                        let mut words = vec!["get".to_string()];
                        match prop {
                            Prop::Text => words.push("text".into()),
                            Prop::Value => words.push("value".into()),
                            Prop::Attr(name) => words.extend(["attr".to_string(), name.clone()]),
                            Prop::Style(name) => words.extend(["style".to_string(), name.clone()]),
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

/// axe-core (vendor/axe-core), put in the page by `expect accessible`.
const AXE: &str = include_str!("../vendor/axe-core/axe.min.js");

/// `expect lcp|fcp|ttfb|load|cls`: what the browser measured for the
/// current document (ms from its navigation start; `cls`: the largest
/// session window of layout shifts not caused by input), or null.
const METRIC_JS: &str = r#"async (name) => {
  const nav = performance.getEntriesByType('navigation')[0];
  const seen = (type) => new Promise((done) => {
    let got = [];
    try {
      const o = new PerformanceObserver((l) => { got = got.concat(l.getEntries()); });
      o.observe({ type, buffered: true });
      setTimeout(() => { o.disconnect(); done(got); }, 50);
    } catch (_) { done([]); }
  });
  if (name === 'ttfb') return nav ? nav.responseStart : null;
  if (name === 'load') return nav && nav.loadEventEnd > 0 ? nav.loadEventEnd : null;
  if (name === 'fcp') { const e = performance.getEntriesByName('first-contentful-paint')[0]; return e ? e.startTime : null; }
  if (name === 'lcp') { const all = await seen('largest-contentful-paint'); return all.length ? all[all.length - 1].startTime : null; }
  if (name === 'cls') {
    let best = 0, sum = 0, first = 0, last = 0;
    for (const e of (await seen('layout-shift')).filter((e) => !e.hadRecentInput)) {
      if (sum && (e.startTime - last > 1000 || e.startTime - first > 5000)) sum = 0;
      if (!sum) first = e.startTime;
      sum += e.value; last = e.startTime; best = Math.max(best, sum);
    }
    return best;
  }
  return null;
}"#;

pub(crate) fn parse(words: &[String]) -> Result<Command, String> {
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
    if let Subject::Screenshot(s) = &e.subject {
        return format!(
            "expected screenshot {} {not}to match its baseline within {} ms: {actual}",
            s.name,
            limit.as_millis()
        );
    }
    if let Subject::Accessible { selector, tags, .. } = &e.subject {
        let on = selector.as_ref().map(|s| format!(" under {s:?}")).unwrap_or_default();
        return format!(
            "expected the page{on} {not}to pass axe ({}) within {} ms; saw {actual}",
            tags.join(", "),
            limit.as_millis()
        );
    }
    if let Subject::Metric(m) = &e.subject {
        let unit = if m == "cls" { "" } else { " ms" };
        let seen = if actual.parse::<f64>().is_ok() { format!("{actual}{unit}") } else { actual.to_string() };
        return format!(
            "expected {not}{m} {} {value}{unit} within {} ms; saw {seen}",
            e.matcher.word(),
            limit.as_millis()
        );
    }
    if let Subject::Ax { flags, .. } = &e.subject {
        let on = if flags.is_empty() { String::new() } else { format!(" {}", flags.join(" ")) };
        return format!("expected ax{on} {not}to match the tree under it within {} ms: {actual}", limit.as_millis());
    }
    let what = match &e.subject {
        Subject::Url => "url".to_string(),
        Subject::Title => "title".to_string(),
        Subject::Text { text, within: None } => format!("text {text:?}"),
        Subject::Text { text, within: Some(w) } => format!("text {text:?} within {}", w.join(" ")),
        Subject::Download(g) => format!("a download {g:?}"),
        Subject::DownloadCompare { glob, expected, .. } => format!("a download {glob:?} the same as {expected}"),
        Subject::Console => "a console message".to_string(),
        Subject::Var(v) => v.clone(),
        Subject::Errors => "no page errors".to_string(),
        Subject::Js(c) => format!("js {c:?}"),
        Subject::Request { method, url, status, read } => {
            let read = match read {
                RequestRead::Made => String::new(),
                RequestRead::Json(p) => format!(" json {p}"),
                RequestRead::Count => " count".to_string(),
            };
            format!(
                "request {method} {url}{}{read}",
                status.as_ref().map(|s| format!(" status {s}")).unwrap_or_default()
            )
        }
        Subject::Cookie(c) => format!("cookie {c}"),
        Subject::Storage { area, key } => format!("storage {area} {key}"),
        // Said above.
        Subject::Ax { .. } => String::new(),
        Subject::Screenshot(s) => format!("screenshot {}", s.name),
        Subject::Metric(m) => m.clone(),
        Subject::Accessible { .. } => "accessible".to_string(),
        Subject::Http(HttpRead::Json(p)) => format!("json {p}"),
        Subject::Http(HttpRead::Header(h)) => format!("header {h}"),
        Subject::Target { flags, prop } => {
            let p = match prop {
                Prop::State(s) => s.clone(),
                Prop::Text => "text".into(),
                Prop::Value => "value".into(),
                Prop::Attr(a) => format!("attr {a}"),
                Prop::Style(a) => format!("style {a}"),
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

/// Run a program (no shell) in `dir`, reading its output as it comes, and
/// stop it at `limit`: its exit code (-1 when a signal ended it), stdout and
/// stderr (the first 16 MB of each). Something it left running that holds the
/// output open (a server started in the background) doesn't hold the run.
fn run_program(
    argv: &[String],
    dir: &Path,
    limit: Duration,
    base: Option<&str>,
) -> Result<(i32, String, String), String> {
    use std::io::Read;
    use std::process::Stdio;
    const CAP: usize = 16 << 20;
    let (program, args) = argv.split_first().ok_or("exec needs a program")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if !dir.as_os_str().is_empty() {
        cmd.current_dir(dir);
    }
    if let Some(b) = base {
        cmd.env("PRIMAVERA_BASE_URL", b);
    }
    let mut child = cmd.spawn().map_err(|e| format!("exec {program}: {e}"))?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                let mut b = sink.lock().unwrap_or_else(|e| e.into_inner());
                let room = CAP.saturating_sub(b.len());
                b.extend_from_slice(&chunk[..n.min(room)]);
            }
        });
        (buf, reader)
    };
    let (out, out_reader) = read(Box::new(child.stdout.take().expect("piped")));
    let (err, err_reader) = read(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| format!("exec {program}: {e}"))? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("`{}` still ran after {} ms; stopped it", shown(argv), limit.as_millis()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let grace = Instant::now() + Duration::from_millis(500);
    while !(out_reader.is_finished() && err_reader.is_finished()) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    let text =
        |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    Ok((status.code().unwrap_or(-1), text(&out), text(&err)))
}

/// A value of the last `http` answer: in its JSON (`$.items[0].sku`), or a
/// header; `None` when it isn't there.
fn http_read(read: &HttpRead, scope: &Scope) -> Result<Option<String>, String> {
    let body = scope.saved.get("http.body").ok_or("no `http` answer yet: an `http` step goes first")?;
    Ok(match read {
        HttpRead::Json(path) => {
            let path = crate::expect::json_path(&scope.word(path)?)?;
            let v: Value = serde_json::from_str(body).map_err(|e| format!("the answer isn't JSON ({e})"))?;
            crate::expect::json_at(&v, &path)
        }
        HttpRead::Header(name) => {
            let name = scope.word(name)?;
            let found: Vec<&str> = scope
                .http_headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
                .map(|(_, v)| v.as_str())
                .collect();
            (!found.is_empty()).then(|| found.join(", "))
        }
    })
}

/// A command line as it could be typed again: words with spaces quoted.
fn shown(argv: &[String]) -> String {
    let word = |a: &String| if a.is_empty() || a.contains(char::is_whitespace) { format!("{a:?}") } else { a.clone() };
    argv.iter().map(word).collect::<Vec<_>>().join(" ")
}

/// A downloads directory nothing landed in, and the evidence directory
/// above it when that is empty too.
fn tidy(downloads: Option<PathBuf>) {
    if let Some(dir) = downloads {
        if std::fs::remove_dir(&dir).is_ok() {
            if let Some(parent) = dir.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
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
    /// What the file's `before all` saved.
    before_all: BTreeMap<String, String>,
    run: BTreeMap<String, String>,
    file: BTreeMap<String, String>,
    /// `${scenario.name}`, `${run.id}`.
    name: String,
    run_id: String,
    secrets: BTreeMap<String, String>,
    /// The secrets' values used so far: masked in what the run prints.
    used_secrets: RefCell<BTreeSet<String>>,
    noop: Option<String>,
    /// Optional steps that didn't happen.
    notes: Vec<String>,
    /// `--soft` expectations that didn't hold, and why.
    soft: Vec<(Step, String)>,
    /// The last `http` answer's headers.
    http_headers: Vec<(String, String)>,
    /// The `.pv` file: screenshot baselines are beside it.
    pv: PathBuf,
    /// The step of a flow that failed, and its file (see `Failure::source`).
    inner: Option<(PathBuf, Step)>,
}

impl Scope {
    fn new(
        suite: &Suite,
        name: &str,
        row: BTreeMap<String, String>,
        opts: &Options,
        before_all: &BTreeMap<String, String>,
    ) -> Self {
        let mut scope = Scope {
            layers: Vec::new(),
            saved: BTreeMap::new(),
            row,
            before_all: before_all.clone(),
            run: opts.vars.clone(),
            file: suite.vars.clone(),
            name: name.to_string(),
            run_id: opts.run_id.clone(),
            secrets: opts.secrets.clone(),
            used_secrets: RefCell::new(BTreeSet::new()),
            noop: None,
            notes: Vec::new(),
            soft: Vec::new(),
            http_headers: Vec::new(),
            pv: suite.file.clone(),
            inner: None,
        };
        // A `set` value's own `${…}` (a default, `--var`, another `set`, a
        // secret, the environment) is filled in once, here; one that names
        // nothing known stays as written. Each value is interpolated once,
        // so `$$` stays a literal `$`.
        let raw = scope.file.clone();
        scope.file = raw.keys().map(|k| (k.clone(), fill_set(&scope, &raw, k, 0))).collect();
        scope
    }

    /// A flow's arguments, then what was saved, the data row, the file's
    /// `before all`, `--var`, the file's `set`s, the environment.
    fn lookup(&self, name: &str) -> Option<String> {
        if let Some(key) = name.strip_prefix("secret:") {
            let v = std::env::var(key).ok().or_else(|| self.secrets.get(key).cloned())?;
            self.used_secrets.borrow_mut().insert(v.clone());
            return Some(v);
        }
        match name {
            "run.id" => return Some(self.run_id.clone()),
            "scenario.name" => return Some(self.name.clone()),
            _ => {}
        }
        self.layers
            .last()
            .and_then(|l| l.get(name))
            .or_else(|| self.saved.get(name))
            .or_else(|| self.row.get(name))
            .or_else(|| self.before_all.get(name))
            .or_else(|| self.run.get(name))
            .or_else(|| self.file.get(name))
            .cloned()
            .or_else(|| std::env::var(name).ok())
    }

    /// [`Scope::lookup`] without the file's `set`s and the environment.
    fn above_file(&self, name: &str) -> Option<String> {
        self.layers
            .last()
            .and_then(|l| l.get(name))
            .or_else(|| self.saved.get(name))
            .or_else(|| self.row.get(name))
            .or_else(|| self.before_all.get(name))
            .or_else(|| self.run.get(name))
            .cloned()
    }

    /// Replaces the secrets' values in a text with `***`.
    fn masker(&self) -> impl Fn(&str) -> String {
        let mut values: Vec<String> = self.used_secrets.borrow().iter().filter(|v| v.len() >= 3).cloned().collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        move |text: &str| values.iter().fold(text.to_string(), |t, v| t.replace(v.as_str(), "***"))
    }

    fn word(&self, w: &str) -> Result<String, String> {
        interpolate(w, &|n| self.lookup(n))
    }

    fn words(&self, ws: &[String]) -> Result<Vec<String>, String> {
        ws.iter().map(|w| self.word(w)).collect()
    }
}

/// A `set` value with its `${…}` filled in: another `set` by its own
/// filled-in value (unless something above the file names it), anything
/// else as a scenario would see it.
fn fill_set(scope: &Scope, raw: &BTreeMap<String, String>, key: &str, depth: u32) -> String {
    let value = &raw[key];
    if !value.contains('$') || depth > 8 {
        return value.clone();
    }
    interpolate(value, &|n| {
        if n == key {
            None
        } else if raw.contains_key(n) && scope.above_file(n).is_none() {
            Some(fill_set(scope, raw, n, depth + 1))
        } else {
            scope.lookup(n)
        }
    })
    .unwrap_or_else(|_| value.clone())
}

/// A failure as the run prints and saves it: secrets used in the scenario
/// replaced by `***` wherever they could show (the message, the page's URL
/// and title, what the page offered instead).
fn mask_failure(f: &mut Failure, mask: &impl Fn(&str) -> String) {
    f.message = mask(&f.message);
    f.text = mask(&f.text);
    if let Some((url, title)) = f.at.as_mut() {
        *url = mask(url);
        *title = mask(title);
    }
    for (what, locator, _) in f.nearest.iter_mut() {
        *what = mask(what);
        *locator = mask(locator);
    }
    if let Some(hint) = f.hint.as_mut() {
        *hint = mask(hint);
    }
}

/// Lines typed on stdin, read by one thread for the whole run (pauses and
/// handoffs wait on it).
fn stdin_lines() -> &'static Mutex<mpsc::Receiver<Option<String>>> {
    static LINES: OnceLock<Mutex<mpsc::Receiver<Option<String>>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
                Ok(_) => {
                    if tx.send(Some(line)).is_err() {
                        return;
                    }
                }
            }
        });
        Mutex::new(rx)
    })
}

/// Waits for Enter, at most `wait` (none: as long as it takes): whether it
/// came; `Err` when stdin is closed.
fn enter(wait: Option<Duration>) -> Result<bool, ()> {
    let rx = stdin_lines().lock().map_err(|_| ())?;
    let got = match wait {
        Some(d) => match rx.recv_timeout(d) {
            Ok(v) => Some(v),
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        },
        None => rx.recv().ok(),
    };
    match got {
        Some(Some(_)) => Ok(true),
        _ => Err(()),
    }
}

/// Running under CI: `CI` set to anything but empty, `false` or `0`.
pub fn under_ci() -> bool {
    std::env::var("CI").is_ok_and(|v| !matches!(v.trim(), "" | "false" | "0"))
}
