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

use crate::expect::{compare, Expect, Matcher, Prop, RequestRead, Subject};
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
            wait_ms: opts.wait_ms,
            // A pause leaves the browser to another navigera session: it
            // attaches over the DevTools port.
            transport: opts.pause.then(|| "ws".to_string()),
            ..SessionConfig::default()
        };
        let driver = Driver::launch(&config)?;
        Ok(Runner { driver, opts, fresh: true, attempt: 1, rep: 0 })
    }

    /// Another runner on this one's browser, in a browser context of its
    /// own: a worker without a browser of its own.
    pub fn fork(&self) -> anyhow::Result<Self> {
        Ok(Runner { driver: self.driver.fork()?, opts: self.opts.clone(), fresh: true, attempt: 1, rep: 0 })
    }

    /// A clean browser context for every scenario but the first one of
    /// the run (a new browser is clean already): no cookies, storage, tabs,
    /// routes, init scripts or `set` emulation from the one before.
    fn clean(&mut self, line: usize) -> Result<(), Box<Failure>> {
        let fresh = std::mem::replace(&mut self.fresh, false);
        if fresh {
            return Ok(());
        }
        self.driver.run_op(&Command::Reset { all: true }).map(|_| ()).map_err(|e| {
            Box::new(Failure {
                line,
                text: "(reset)".into(),
                message: format!("could not start from a clean browser context: {e:#}"),
                at: None,
                hint: None,
                screenshot: None,
                nearest: Vec::new(),
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
        let mut scope = Scope::new(suite, &scenario.name, scenario.vars.clone(), &self.opts, file_vars);
        let mut steps = Vec::new();
        let mut failure = self.clean(scenario.line).err().map(|f| *f);
        let downloads = if failure.is_none() { self.own_downloads(suite, &scenario.name) } else { None };
        let all: Vec<&Step> = suite.before_each.iter().chain(&scenario.steps).collect();
        if failure.is_none() {
            for step in all {
                let t = Instant::now();
                let res = self.step(step, &mut scope, suite);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                steps.push(StepResult { line: step.line, text: step.text.clone(), ms, ok: res.is_ok() });
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
        };
        let mask = scope.masker();
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
            let ci = std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "false");
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
        let mut aux = Runner {
            driver: self.driver.fork().map_err(|e| format!("auth {}: {e:#}", auth.name))?,
            opts: self.opts.clone(),
            fresh: true,
            attempt: 1,
            rep: 0,
        };
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
                if let (Subject::Download(_), false) = (&e.subject, e.negate) {
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
            Subject::Download(glob) => {
                let glob = scope.word(glob)?;
                let v = self.command(&["downloads".into(), "--json".into()])?;
                let list = v.as_array().cloned().unwrap_or_default();
                let name = |d: &Value| d.get("file").and_then(Value::as_str).unwrap_or("").to_string();
                let done = list.iter().rev().find(|d| {
                    crate::expect::glob(&glob, &name(d)) && d.get("state").and_then(Value::as_str) == Some("completed")
                });
                match done {
                    Some(d) => {
                        (true, d.get("path").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| name(d)))
                    }
                    None => {
                        let seen: Vec<String> = list
                            .iter()
                            .map(|d| format!("{} ({})", name(d), d.get("state").and_then(Value::as_str).unwrap_or("?")))
                            .collect();
                        (false, if seen.is_empty() { "no download".into() } else { seen.join(", ") })
                    }
                }
            }
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
