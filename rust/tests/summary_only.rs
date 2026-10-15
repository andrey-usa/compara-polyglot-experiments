//! Output is opt-in: without -o, --json or --export-dir, `compare` prints the
//! counts and writes nothing, and `--summary` says so explicitly.
//!
//! Two things have to hold, and they pull against each other: the default run
//! must not write anything (nor collect the rows a report would embed), and
//! the counts it prints must be the ones a run with a report prints. The
//! second is the risk -- the engine skips the sections, and a section is also
//! where its dropped-row total is recorded, so a count computed from the rows
//! that survived rather than from the rows that matched would come out short
//! here and nowhere else.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use compara::gendata::{Compression, Format, generate_as};

/// A number no other fixture in this process has. The tests run as parallel
/// threads of one process, and the clock this used to take cannot tell them
/// apart: macOS reports microseconds, and two fixtures made in the same one
/// shared a directory, which the first to finish deleted under the other.
fn fixture_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const BIN: &str = env!("CARGO_BIN_EXE_compara");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-summary-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        // Enough rows that every section has something in it -- changed, added,
        // removed and duplicate keys -- because a section that is empty either
        // way proves nothing about skipping it.
        generate_as(
            20_000,
            &dir.join("a.csv"),
            &dir.join("b.csv"),
            7,
            Format::Csv,
            Compression::None,
        )
        .expect("the generator");
        Fixture(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs in `dir` so a defaulted `--out` lands there and not in the test's cwd.
fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The counts line without its trailing `| turbo 0.123s`, which is a clock.
fn counts(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or_default();
    match line.rfind('|') {
        Some(at) => line[..at].trim_end().to_string(),
        None => line.to_string(),
    }
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the fixture directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "a.csv" && n != "b.csv")
        .collect();
    names.sort();
    names
}

#[test]
fn only_an_output_flag_writes_and_the_counts_never_change() {
    let fx = Fixture::new();
    let args = ["compare", "a.csv", "b.csv", "-k", "account_id,txn_id"];

    let (plain_code, plain_out, plain_err) = run(&fx.0, &args);
    assert_eq!(plain_code, 1, "a run with differences exits 1: {plain_err}");
    assert!(
        files(&fx.0).is_empty(),
        "the default run wrote {:?}",
        files(&fx.0)
    );
    assert!(
        !plain_out.contains("Report:"),
        "no report, but it says so: {plain_out}"
    );

    let mut report_args = args.to_vec();
    report_args.extend(["-o", "r.html", "--json", "r.json"]);
    let (code, out, err) = run(&fx.0, &report_args);
    assert_eq!(code, plain_code, "-o changes the exit code: {err}");
    assert_eq!(
        files(&fx.0),
        vec!["r.html".to_string(), "r.json".to_string()]
    );
    assert!(out.contains("Report: r.html"), "{out}");
    assert_eq!(
        counts(&out),
        counts(&plain_out),
        "the run with a report counts differently"
    );
    fs::remove_file(fx.path("r.html")).expect("the report");
    fs::remove_file(fx.path("r.json")).expect("the summary");

    let mut summary_args = args.to_vec();
    summary_args.push("--summary");
    let (code, out, err) = run(&fx.0, &summary_args);
    assert_eq!(code, plain_code, "--summary changes the exit code: {err}");
    assert!(
        files(&fx.0).is_empty(),
        "--summary wrote {:?}",
        files(&fx.0)
    );
    assert_eq!(
        counts(&out),
        counts(&plain_out),
        "--summary counts differently"
    );

    // An export needs no report: the same rows, with or without -o.
    let mut export = args.to_vec();
    export.extend(["--export-dir", "x"]);
    let (code, _, err) = run(&fx.0, &export);
    assert_eq!(
        code, plain_code,
        "--export-dir changes the exit code: {err}"
    );
    let changed = fs::read_to_string(fx.path("x").join("changed.csv")).expect("changed.csv");
    assert!(
        changed.lines().count() > 1,
        "--export-dir without -o wrote no rows"
    );
    export.extend(["-o", "r.html"]);
    run(&fx.0, &export);
    let with_report = fs::read_to_string(fx.path("x").join("changed.csv")).expect("changed.csv");
    assert_eq!(changed, with_report, "-o changes what --export-dir writes");
    fs::remove_dir_all(fx.path("x")).expect("the export");
    fs::remove_file(fx.path("r.html")).expect("the report");

    // A report option without a report is a mistake, not a no-op.
    let mut no_report = args.to_vec();
    no_report.push("--no-compress");
    let (code, _, err) = run(&fx.0, &no_report);
    assert_eq!(code, 2, "--no-compress without -o was accepted");
    assert!(err.contains("add -o"), "{}", err.trim());
}

#[test]
fn summary_refuses_the_flags_that_ask_for_output() {
    let fx = Fixture::new();
    // Guessing which of the two was meant is how a report silently stops
    // appearing, so each of these is an error rather than a precedence rule.
    for flag in [
        vec!["-o", "r.html"],
        vec!["--out", "r.html"],
        vec!["--json", "r.json"],
        vec!["--export-dir", "."],
    ] {
        let mut args = vec![
            "compare",
            "a.csv",
            "b.csv",
            "-k",
            "account_id,txn_id",
            "--summary",
        ];
        args.extend(flag.iter().copied());
        let (code, _, err) = run(&fx.0, &args);
        assert_eq!(code, 2, "{flag:?} was accepted beside --summary");
        assert!(
            err.contains("--summary writes nothing"),
            "{flag:?}: {}",
            err.trim()
        );
        assert!(files(&fx.0).is_empty(), "{flag:?} wrote {:?}", files(&fx.0));
    }
}
