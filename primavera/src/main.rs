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
use std::sync::{mpsc, Mutex};
use std::time::Instant;

const USAGE: &str = "\
primavera: plain-text web tests on real Chrome (through navigera)

usage:
  primavera run [<file.pv|dir>...] [options]   run scenarios (default: the current directory)
  primavera lint [<file.pv|dir>...]             check files without a browser
  primavera list [<file.pv|dir>...]             the scenarios a run would take
  primavera --version

run options:
  --base-url <url>      where relative URLs (`goto /cart`) point (else `base` in the file, PRIMAVERA_BASE_URL)
  --var <name=value>    a variable for ${name} (repeatable)
  --grep <text>         only scenarios whose name contains it
  --tag <tag>           only scenarios with this @tag (repeatable: any of them)
  --workers <n|auto>    scenarios at once, a browser each (default 1; auto: half the CPUs, at most 8)
  --retries <n>         run a failing scenario again, up to n times; one that then passes is flaky
  --fail-on-flaky       a flaky scenario fails the run
  --shard <i/n>         run the i-th of n slices (for CI matrix jobs)
  --reporter <name>     console (default), jsonl (one JSON object per scenario on stdout), github
                        (workflow annotations; on by itself under GitHub Actions); repeatable
  --junit <file>        also write JUnit XML
  --json                print what the scenarios saved, as one JSON object, at the end
  --timeout-ms <ms>     how long an expect retries (default 5000)
  --wait-ms <ms>        how long a step waits for its element (navigera's default 5000)
  --headed              show the browser
  --chromium <path>     the browser to run
  --evidence <dir>      failure screenshots (default .primavera/runs/<time>)

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
    };
    let mut junit = None;
    let mut base_flag = None;
    let mut workers = 1usize;
    let mut retries = 0u32;
    let mut fail_on_flaky = false;
    let mut shard = None;
    let mut reporters: Vec<String> = Vec::new();
    let mut json = false;
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--base-url" => base_flag = Some(val("--base-url")?),
            "--var" => {
                let kv = val("--var")?;
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("--var takes name=value, not {kv:?}"))?;
                opts.vars.insert(k.trim().to_string(), v.to_string());
            }
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
    Ok(Args { cmd, paths, opts, junit, workers, retries, fail_on_flaky, shard, reporters, json })
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
        "--version" | "-V" | "version" => {
            println!("primavera {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "lint" => match load(&args.paths) {
            Ok(suites) => {
                let n: usize = suites.iter().map(|s| s.scenarios.len()).sum();
                println!("ok: {} file(s), {n} scenario(s)", suites.len());
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
    jobs.sort_by(|a, b| {
        let t = |j: &runner::Job| timings.get(&j.key(&suites)).copied().unwrap_or(f64::INFINITY);
        t(b).partial_cmp(&t(a)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let jsonl = args.reporters.iter().any(|r| r == "jsonl");
    let console = args.reporters.iter().any(|r| r == "console");
    let github = args.reporters.iter().any(|r| r == "github");
    let workers = args.workers.min(jobs.len()).max(1);
    let queue = Mutex::new(std::collections::VecDeque::from(jobs));
    let (tx, rx) = mpsc::channel::<Event>();
    let mut results: Vec<runner::ScenarioResult> = Vec::new();
    let mut launch_errors = Vec::new();
    std::thread::scope(|scope| {
        for w in 0..workers {
            let tx = tx.clone();
            let (queue, suites, opts) = (&queue, &suites, &opts);
            let retries = args.retries;
            scope.spawn(move || worker(w, queue, suites, opts, retries, tx));
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
    let left = queue.lock().map(|q| q.len()).unwrap_or(0);
    if left > 0 {
        eprintln!("the browser didn't start: {}", launch_errors.first().map(String::as_str).unwrap_or("?"));
        eprintln!("{left} scenario(s) not run");
        if results.is_empty() {
            return ExitCode::from(3);
        }
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
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

/// One worker: its own browser, started when it takes its first scenario,
/// then scenarios from the shared queue until it is empty.
fn worker(
    id: usize,
    queue: &Mutex<std::collections::VecDeque<runner::Job>>,
    suites: &[suite::Suite],
    opts: &runner::Options,
    retries: u32,
    tx: mpsc::Sender<Event>,
) {
    let mut browser: Option<runner::Runner> = None;
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
        let mut result = r.run(suite, sc, 1);
        let mut earlier = Vec::new();
        while !result.passed() && result.attempt <= retries {
            earlier.extend(result.failure.take());
            let next = result.attempt + 1;
            result = r.run(suite, sc, next);
        }
        result.retried = earlier;
        result.worker = id;
        if tx.send(Event::Done(Box::new(result))).is_err() {
            return;
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
