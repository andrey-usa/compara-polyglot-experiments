//! primavera: plain-text web tests on real Chrome, run by one native binary
//! through navigera. See README.md.

mod expect;
mod nearest;
mod report;
mod runner;
mod suite;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{mpsc, Condvar, Mutex};
use std::time::Instant;

/// The agent skill: the format and the commands, for an agent writing tests.
const SKILL_MD: &str = include_str!("../skills/primavera/SKILL.md");

const USAGE: &str = "\
primavera: plain-text web tests on real Chrome (through navigera)

usage:
  primavera run [<file.pv|dir>...] [options]   run scenarios (default: the current directory)
  primavera lint [<file.pv|dir>...]             check files without a browser
  primavera list [<file.pv|dir>...]             the scenarios a run would take
  primavera skill                               the agent skill (the format and commands, for an AI agent)
  primavera --version

run options:
  --base-url <url>      where relative URLs (`goto /cart`) point (else `base` in the file, PRIMAVERA_BASE_URL)
  --var <name=value>    a variable for ${name} (repeatable)
  --secrets <file>      NAME=value lines for ${secret:NAME} (the environment comes first); masked in output
  --grep <text>         only scenarios whose name contains it
  --tag <tag>           only scenarios with this @tag (repeatable: any of them)
  --workers <n|auto>    scenarios at once (default 1; auto: half the CPUs, at most 8)
  --isolation <how>     context (default): one browser, a browser context per worker; browser: a browser each
  --retries <n>         run a failing scenario again, up to n times; one that then passes is flaky
  --fail-on-flaky       a flaky scenario fails the run
  --shard <i/n>         run the i-th of n slices (for CI matrix jobs)
  --repeat <n>          run every scenario n times (flake hunting: each run is reported, then a pass count)
  --shuffle [<seed>]    run in a random order (the seed is printed; give it again to repeat the order)
  --reporter <name>     console (default), jsonl (one JSON object per scenario on stdout), github
                        (workflow annotations; on by itself under GitHub Actions); repeatable
  --junit <file>        also write JUnit XML
  --json                print what the scenarios saved, as one JSON object, at the end
  --timeout-ms <ms>     how long an expect retries (default 5000)
  --wait-ms <ms>        how long a step waits for its element (navigera's default 5000)
  --headed              show the browser
  --chromium <path>     the browser to run
  --evidence <dir>      failure screenshots (default .primavera/runs/<time>)
  --pause-on-failure    keep the browser at a failure, open to `navigera --attach`, until Enter (one worker)

exit codes: 0 all passed, 1 a scenario failed, 2 a file or an argument is wrong, 3 the browser didn't start
";

struct Args {
    cmd: String,
    paths: Vec<PathBuf>,
    opts: runner::Options,
    junit: Option<PathBuf>,
    workers: usize,
    retries: u32,
    fail_on_flaky: bool,
    shard: Option<(usize, usize)>,
    reporters: Vec<String>,
    json: bool,
    /// `context` (one browser, a context per worker) or `browser`.
    isolation: String,
    repeat: u32,
    shuffle: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| USAGE.to_string())?;
    let mut paths = Vec::new();
    let stamp =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let mut opts = runner::Options {
        base: std::env::var("PRIMAVERA_BASE_URL").ok().filter(|v| !v.is_empty()),
        vars: BTreeMap::new(),
        headed: false,
        chromium: None,
        expect_ms: 5000,
        wait_ms: None,
        grep: None,
        tags: Vec::new(),
        evidence: PathBuf::from(format!(".primavera/runs/{stamp}")),
        run_id: format!("{stamp}-{}", std::process::id()),
        secrets: BTreeMap::new(),
        pause: false,
        auth: Default::default(),
    };
    let mut junit = None;
    let mut base_flag = None;
    let mut workers = 1usize;
    let mut retries = 0u32;
    let mut fail_on_flaky = false;
    let mut shard = None;
    let mut reporters: Vec<String> = Vec::new();
    let mut json = false;
    let mut isolation = "context".to_string();
    let mut repeat = 1u32;
    let mut shuffle = None;
    let mut it = it.peekable();
    while let Some(a) = it.next() {
        if a == "--shuffle" {
            // An optional seed: the next word when it is a number.
            let seed = it.peek().and_then(|w| w.parse::<u64>().ok());
            if seed.is_some() {
                it.next();
            }
            shuffle = Some(seed.unwrap_or(stamp ^ (std::process::id() as u64).rotate_left(32)));
            continue;
        }
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--base-url" => base_flag = Some(val("--base-url")?),
            "--var" => {
                let kv = val("--var")?;
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("--var takes name=value, not {kv:?}"))?;
                opts.vars.insert(k.trim().to_string(), v.to_string());
            }
            "--secrets" => {
                let path = val("--secrets")?;
                let text = std::fs::read_to_string(&path).map_err(|e| format!("--secrets {path}: {e}"))?;
                opts.secrets = secrets_file(&text).map_err(|e| format!("--secrets {path}: {e}"))?;
            }
            "--pause-on-failure" => opts.pause = true,
            "--grep" => opts.grep = Some(val("--grep")?),
            "--tag" => opts.tags.push(val("--tag")?.trim_start_matches('@').to_string()),
            "--junit" => junit = Some(PathBuf::from(val("--junit")?)),
            "--timeout-ms" => {
                let v = val("--timeout-ms")?;
                opts.expect_ms = v.parse().map_err(|_| format!("--timeout-ms takes milliseconds, not {v:?}"))?;
            }
            "--wait-ms" => {
                let v = val("--wait-ms")?;
                opts.wait_ms = Some(v.parse().map_err(|_| format!("--wait-ms takes milliseconds, not {v:?}"))?);
            }
            "--headed" => opts.headed = true,
            "--chromium" => opts.chromium = Some(val("--chromium")?),
            "--evidence" => opts.evidence = PathBuf::from(val("--evidence")?),
            "--workers" | "-j" => {
                let v = val("--workers")?;
                workers = if v == "auto" {
                    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
                    (cpus / 2).clamp(1, 8)
                } else {
                    v.parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--workers takes a number or auto, not {v:?}"))?
                };
            }
            "--retries" => {
                let v = val("--retries")?;
                retries = v.parse().map_err(|_| format!("--retries takes a number, not {v:?}"))?;
            }
            "--fail-on-flaky" => fail_on_flaky = true,
            "--repeat" => {
                let v = val("--repeat")?;
                repeat =
                    v.parse().ok().filter(|n| *n > 0).ok_or_else(|| format!("--repeat takes a number, not {v:?}"))?;
            }
            "--shard" => {
                let v = val("--shard")?;
                let parsed = v
                    .split_once('/')
                    .and_then(|(i, n)| Some((i.parse::<usize>().ok()?, n.parse::<usize>().ok()?)))
                    .filter(|(i, n)| *n > 0 && *i >= 1 && i <= n);
                shard = Some(parsed.ok_or_else(|| format!("--shard takes i/n with 1 <= i <= n, not {v:?}"))?);
            }
            "--reporter" => {
                let v = val("--reporter")?;
                for r in v.split(',') {
                    if !matches!(r, "console" | "jsonl" | "github") {
                        return Err(format!("unknown reporter {r:?} (console, jsonl, github)"));
                    }
                    reporters.push(r.to_string());
                }
            }
            "--json" => json = true,
            "--isolation" => {
                isolation = val("--isolation")?;
                if isolation != "context" && isolation != "browser" {
                    return Err(format!("--isolation takes context or browser, not {isolation:?}"));
                }
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            f if f.starts_with('-') => return Err(format!("unknown option {f}\n\n{USAGE}")),
            p => paths.push(PathBuf::from(p)),
        }
    }
    if base_flag.is_some() {
        opts.base = base_flag;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    if reporters.is_empty() {
        reporters.push("console".into());
    }
    if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") && !reporters.iter().any(|r| r == "github") {
        reporters.push("github".into());
    }
    Ok(Args {
        cmd,
        paths,
        opts,
        junit,
        workers,
        retries,
        fail_on_flaky,
        shard,
        reporters,
        json,
        isolation,
        repeat,
        shuffle,
    })
}

/// `NAME=value` lines (`#` comments, optional quotes around the value).
fn secrets_file(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = line.split_once('=').ok_or_else(|| format!("line {}: expected NAME=value", i + 1))?;
        let v = v.trim();
        let v = match (v.chars().next(), v.chars().last()) {
            (Some(q @ ('"' | '\'')), Some(e)) if q == e && v.len() >= 2 => &v[1..v.len() - 1],
            _ => v,
        };
        out.insert(k.trim().to_string(), v.to_string());
    }
    Ok(out)
}

fn load(paths: &[PathBuf]) -> Result<Vec<suite::Suite>, String> {
    let files = suite::discover(paths).map_err(|e| format!("reading {:?}: {e}", paths))?;
    if files.is_empty() {
        return Err("no .pv files found".into());
    }
    let mut suites = Vec::new();
    let mut errors = Vec::new();
    for f in files {
        match std::fs::read_to_string(&f) {
            Ok(text) => match suite::parse(&f, &text) {
                Ok(s) => suites.push(s),
                Err(e) => errors.push(e.to_string()),
            },
            Err(e) => errors.push(format!("{}: {e}", f.display())),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    suite::resolve(&mut suites).map_err(|e| e.to_string())?;
    Ok(suites)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) if msg == USAGE => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    match args.cmd.as_str() {
        "skill" => {
            print!("{SKILL_MD}");
            ExitCode::SUCCESS
        }
        "--version" | "-V" | "version" => {
            println!("primavera {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "lint" => match load(&args.paths) {
            Ok(suites) => {
                // Variables nothing sets: warnings, since the environment of
                // the run (a CI secret) may still set them.
                let mut run_vars: Vec<String> = args.opts.vars.keys().cloned().collect();
                run_vars.extend(args.opts.secrets.keys().map(|k| format!("secret:{k}")));
                let mut warnings = 0;
                for s in &suites {
                    for sc in &s.scenarios {
                        for (line, name) in suite::unset_vars(s, sc, &run_vars) {
                            warnings += 1;
                            println!(
                                "warning: {}:{line}: ${{{name}}} isn't set by the file, --var, a data row or a save before it, nor in this environment",
                                s.file.display()
                            );
                        }
                    }
                }
                let n: usize = suites.iter().map(|s| s.scenarios.len()).sum();
                let w = if warnings > 0 { format!(", {warnings} warning(s)") } else { String::new() };
                println!("ok: {} file(s), {n} scenario(s){w}", suites.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        },
        "list" => match load(&args.paths) {
            Ok(suites) => {
                for job in planned(&suites, &args) {
                    let (suite, sc) = job.get(&suites);
                    let tags: String = sc.tags.iter().map(|t| format!(" @{t}")).collect();
                    println!("{}:{}  {}{tags}", suite.file.display(), sc.line, sc.name);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        },
        "run" => run(args),
        other => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Args) -> ExitCode {
    let started = Instant::now();
    let suites = match load(&args.paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut opts = args.opts.clone();
    // The first file's `base` serves when the run names none.
    if opts.base.is_none() {
        opts.base = suites.iter().find_map(|s| s.base.clone());
    }
    let mut jobs = planned(&suites, &args);
    if jobs.is_empty() {
        eprintln!("no scenario matches");
        return ExitCode::from(2);
    }
    // Longest first (from the last runs), so the slowest one doesn't start
    // last; ones never timed go first, as they may be long.
    let timings = Timings::load();
    if args.repeat > 1 {
        jobs = jobs.iter().flat_map(|j| (1..=args.repeat).map(move |rep| runner::Job { rep, ..*j })).collect();
    }
    match args.shuffle {
        Some(seed) => {
            shuffle(&mut jobs, seed);
            eprintln!("shuffled with seed {seed} (--shuffle {seed} runs this order again)");
        }
        None => jobs.sort_by(|a, b| {
            let t = |j: &runner::Job| timings.get(&j.key(&suites)).copied().unwrap_or(f64::INFINITY);
            t(b).partial_cmp(&t(a)).unwrap_or(std::cmp::Ordering::Equal)
        }),
    }

    let jsonl = args.reporters.iter().any(|r| r == "jsonl");
    let console = args.reporters.iter().any(|r| r == "console");
    let github = args.reporters.iter().any(|r| r == "github");
    // A pause holds the run at one failure: one worker.
    let workers = if opts.pause { 1 } else { args.workers.min(jobs.len()).max(1) };
    let hooks = Hooks::new(&suites, &jobs);
    // One browser, a context per worker (`--isolation context`, the
    // default), or a browser per worker. The first browser is kept here
    // and closes last.
    let mut owner = None;
    let mut forks: Vec<Option<runner::Runner>> = (0..workers).map(|_| None).collect();
    // (One worker needs no sharing: its own browser is the one browser.)
    if args.isolation == "context" && workers > 1 {
        match runner::Runner::launch(opts.clone()) {
            Ok(r) => {
                for slot in forks.iter_mut() {
                    *slot = r.fork().ok();
                }
                owner = Some(r);
            }
            Err(e) => {
                eprintln!("the browser didn't start: {e:#}");
                return ExitCode::from(3);
            }
        }
    }
    let queue = Mutex::new(std::collections::VecDeque::from(jobs));
    let (tx, rx) = mpsc::channel::<Event>();
    let mut results: Vec<runner::ScenarioResult> = Vec::new();
    let mut launch_errors = Vec::new();
    std::thread::scope(|scope| {
        for (w, fork) in forks.into_iter().enumerate() {
            let tx = tx.clone();
            let (queue, suites, opts, hooks) = (&queue, &suites, &opts, &hooks);
            let retries = args.retries;
            scope.spawn(move || worker(w, fork, queue, suites, opts, hooks, retries, tx));
        }
        drop(tx);
        let mut out = std::io::stdout();
        for ev in rx {
            match ev {
                Event::Done(r) => {
                    let mut text = String::new();
                    if github {
                        text.push_str(&report::github(&r));
                    }
                    if jsonl {
                        text.push_str(&report::jsonl(&r));
                    }
                    if console {
                        // With jsonl on stdout, the console goes to stderr.
                        if jsonl {
                            eprint!("{}", report::console_line(&r));
                        } else {
                            text.push_str(&report::console_line(&r));
                        }
                    }
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                    results.push(*r);
                }
                Event::LaunchFailed(e) => launch_errors.push(e),
            }
        }
    });
    drop(owner);
    let left = queue.lock().map(|q| q.len()).unwrap_or(0);
    if left > 0 {
        eprintln!("the browser didn't start: {}", launch_errors.first().map(String::as_str).unwrap_or("?"));
        eprintln!("{left} scenario(s) not run");
        if results.is_empty() {
            return ExitCode::from(3);
        }
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    if console && args.repeat > 1 {
        // How often each scenario passed across its runs.
        let mut tally: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for r in &results {
            let e = tally.entry(format!("{}  {}", r.file.display(), r.base_name())).or_default();
            e.1 += 1;
            if r.passed() {
                e.0 += 1;
            }
        }
        let mut text = String::from("repeats:\n");
        for (name, (ok, n)) in &tally {
            let mark = if ok == n { "    " } else { "FLAKY " };
            text.push_str(&format!("  {mark}{ok}/{n}  {name}\n"));
        }
        if jsonl {
            eprint!("{text}");
        } else {
            print!("{text}");
        }
    }
    if console {
        let line = report::summary(&results, ms);
        if jsonl {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    if jsonl {
        print!("{}", report::jsonl_summary(&results, ms));
    }
    if args.json {
        let saved: serde_json::Map<String, serde_json::Value> =
            results.iter().map(|r| (r.name.clone(), serde_json::json!(r.saved))).collect();
        println!("{}", serde_json::Value::Object(saved));
    }
    Timings::save(&results);
    if let Some(path) = &args.junit {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, report::junit(&results, ms)) {
            eprintln!("could not write {}: {e}", path.display());
        }
    }
    let failed = results.iter().any(|r| !r.passed()) || (args.fail_on_flaky && results.iter().any(|r| r.flaky()));
    if failed || left > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// A seeded Fisher-Yates shuffle (xorshift64*): the same seed, the same
/// order, on every machine.
fn shuffle(jobs: &mut [runner::Job], seed: u64) {
    let mut x = seed.max(1);
    for i in (1..jobs.len()).rev() {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let r = x.wrapping_mul(0x2545F4914F6CDD1D);
        jobs.swap(i, (r % (i as u64 + 1)) as usize);
    }
}

/// The scenarios this run takes: `--grep`, `--tag`, then `--shard` (every
/// n-th, in file order, so the slices are the same on every machine).
fn planned(suites: &[suite::Suite], args: &Args) -> Vec<runner::Job> {
    let jobs = runner::jobs(suites, &args.opts);
    match args.shard {
        Some((i, n)) => jobs.into_iter().enumerate().filter(|(k, _)| k % n == i - 1).map(|(_, j)| j).collect(),
        None => jobs,
    }
}

enum Event {
    Done(Box<runner::ScenarioResult>),
    LaunchFailed(String),
}

/// Each file's `before all` (run by the first worker that takes one of its
/// scenarios, while the others that do wait) and `after all` (run by the
/// worker that finishes its last).
struct Hooks {
    before: Mutex<Vec<Hook>>,
    ready: Condvar,
    left: Mutex<Vec<usize>>,
}

type HookResult = Result<BTreeMap<String, String>, Box<runner::Failure>>;

enum Hook {
    Idle,
    Running,
    Done(HookResult),
}

impl Hooks {
    fn new(suites: &[suite::Suite], jobs: &[runner::Job]) -> Self {
        let mut left = vec![0; suites.len()];
        for j in jobs {
            left[j.suite] += 1;
        }
        Hooks {
            before: Mutex::new(suites.iter().map(|_| Hook::Idle).collect()),
            ready: Condvar::new(),
            left: Mutex::new(left),
        }
    }

    /// File `i`'s `before all`, run once: what it saved, or how it failed.
    fn before(&self, i: usize, run: impl FnOnce() -> HookResult) -> HookResult {
        let Ok(mut st) = self.before.lock() else { return Ok(BTreeMap::new()) };
        loop {
            match &st[i] {
                Hook::Done(r) => return r.clone(),
                Hook::Running => match self.ready.wait(st) {
                    Ok(g) => st = g,
                    Err(_) => return Ok(BTreeMap::new()),
                },
                Hook::Idle => {
                    st[i] = Hook::Running;
                    drop(st);
                    let r = run();
                    if let Ok(mut st) = self.before.lock() {
                        st[i] = Hook::Done(r.clone());
                    }
                    self.ready.notify_all();
                    return r;
                }
            }
        }
    }

    /// One of file `i`'s scenarios is done: whether it was the last.
    fn finished(&self, i: usize) -> bool {
        self.left
            .lock()
            .map(|mut l| {
                l[i] = l[i].saturating_sub(1);
                l[i] == 0
            })
            .unwrap_or(false)
    }

    fn vars(&self, i: usize) -> BTreeMap<String, String> {
        match self.before.lock().as_deref().map(|b| &b[i]) {
            Ok(Hook::Done(Ok(v))) => v.clone(),
            _ => BTreeMap::new(),
        }
    }
}

/// One worker: its own browser, started when it takes its first scenario,
/// then scenarios from the shared queue until it is empty.
#[allow(clippy::too_many_arguments)]
fn worker(
    id: usize,
    fork: Option<runner::Runner>,
    queue: &Mutex<std::collections::VecDeque<runner::Job>>,
    suites: &[suite::Suite],
    opts: &runner::Options,
    hooks: &Hooks,
    retries: u32,
    tx: mpsc::Sender<Event>,
) {
    // A context on the shared browser, else a browser of its own.
    let mut browser: Option<runner::Runner> = fork;
    loop {
        let Some(job) = queue.lock().ok().and_then(|mut q| q.pop_front()) else { return };
        if browser.is_none() {
            match runner::Runner::launch(opts.clone()) {
                Ok(r) => browser = Some(r),
                Err(e) => {
                    // Give the scenario back to a worker whose browser runs.
                    if let Ok(mut q) = queue.lock() {
                        q.push_front(job);
                    }
                    let _ = tx.send(Event::LaunchFailed(format!("{e:#}")));
                    return;
                }
            }
        }
        let Some(r) = browser.as_mut() else { return };
        let (suite, sc) = job.get(suites);
        let job_rep = job.rep;
        let before = if suite.before_all.is_empty() {
            Ok(BTreeMap::new())
        } else {
            hooks.before(job.suite, || r.hook(suite, &suite.before_all, "before all", &BTreeMap::new()))
        };
        let mut result = match &before {
            Ok(vars) => {
                let mut result = r.run(suite, sc, 1, vars);
                let mut earlier = Vec::new();
                while !result.passed() && result.attempt <= retries {
                    earlier.extend(result.failure.take());
                    let next = result.attempt + 1;
                    result = r.run(suite, sc, next, vars);
                }
                result.retried = earlier;
                result
            }
            // Not run: the file's `before all` failed.
            Err(f) => runner::ScenarioResult::failed(suite, sc.name.clone(), sc.line, sc.tags.clone(), (**f).clone()),
        };
        result.worker = id;
        if job_rep > 0 {
            result.rep = job_rep;
            result.name = format!("{} #{job_rep}", result.name);
        }
        if tx.send(Event::Done(Box::new(result))).is_err() {
            return;
        }
        if hooks.finished(job.suite) && !suite.after_all.is_empty() {
            if let Err(f) = r.hook(suite, &suite.after_all, "after all", &hooks.vars(job.suite)) {
                let line = suite.after_all[0].line;
                let mut done = runner::ScenarioResult::failed(suite, "(after all)".into(), line, Vec::new(), *f);
                done.worker = id;
                if tx.send(Event::Done(Box::new(done))).is_err() {
                    return;
                }
            }
        }
    }
}

/// How long each scenario took last time (`.primavera/timings.json`), to
/// start the slow ones first.
struct Timings;

impl Timings {
    const PATH: &'static str = ".primavera/timings.json";

    fn load() -> BTreeMap<String, f64> {
        std::fs::read_to_string(Self::PATH).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(results: &[runner::ScenarioResult]) {
        let mut all = Self::load();
        for r in results {
            all.insert(r.key(), (r.ms * 10.0).round() / 10.0);
        }
        if std::fs::create_dir_all(".primavera").is_ok() {
            let _ = std::fs::write(Self::PATH, serde_json::to_string_pretty(&all).unwrap_or_default());
        }
    }
}
