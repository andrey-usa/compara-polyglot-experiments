//! primavera: plain-text web tests on real Chrome, run by one native binary
//! through navigera. See README.md.

mod expect;
mod report;
mod runner;
mod suite;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
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
  --junit <file>        also write JUnit XML
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
    Ok(Args { cmd, paths, opts, junit })
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
    if errors.is_empty() {
        Ok(suites)
    } else {
        Err(errors.join("\n"))
    }
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
                for (suite, sc) in runner::selected(&suites, &args.opts) {
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
    let picked = runner::selected(&suites, &opts);
    if picked.is_empty() {
        eprintln!("no scenario matches");
        return ExitCode::from(2);
    }
    // The first file's `base` serves when the run names none.
    if opts.base.is_none() {
        opts.base = suites.iter().find_map(|s| s.base.clone());
    }
    let mut runner = match runner::Runner::launch(opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the browser didn't start: {e:#}");
            return ExitCode::from(3);
        }
    };
    let mut results = Vec::new();
    let mut out = std::io::stdout();
    for (suite, sc) in picked {
        let r = runner.run(suite, sc);
        let _ = out.write_all(report::console_line(&r).as_bytes());
        let _ = out.flush();
        results.push(r);
    }
    drop(runner);
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("{}", report::summary(&results, ms));
    if let Some(path) = &args.junit {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, report::junit(&results, ms)) {
            eprintln!("could not write {}: {e}", path.display());
        }
    }
    if results.iter().all(|r| r.passed()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
