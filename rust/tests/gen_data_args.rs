//! The generator's command line, which used to accept things it then ignored.
//!
//! `--rows=50m` -- the spelling every GNU tool takes -- was dropped on the floor
//! by a loop that skipped anything it did not recognise, and the run produced
//! the 10k default while printing `rust: 10000 rows` and nothing about the flag.
//! A generator that quietly writes a different dataset than the one asked for
//! wastes whatever was measured on it.

use std::path::PathBuf;
use std::process::Command;

/// A number no other fixture in this process has. The tests run as parallel
/// threads of one process, and the clock this used to take cannot tell them
/// apart: macOS reports microseconds, and two fixtures made in the same one
/// shared a directory, which the first to finish deleted under the other.
fn fixture_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const GEN: &str = env!("CARGO_BIN_EXE_compara-gen-data");

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let p = std::env::temp_dir().join(format!(
            "compara-args-{tag}-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the generator and hands back (exit code, stdout, stderr).
fn run_gen(dir: &Dir, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(GEN)
        .args(args)
        .args(["--out-dir"])
        .arg(&dir.0)
        .output()
        .expect("the generator runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_equals_form_is_honoured() {
    let dir = Dir::new("eq");
    let (code, stdout, stderr) = run_gen(&dir, &["--rows=2k", "--prefix", "t"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("2000 rows"),
        "--rows=2k has to mean 2000, not the 10k default: {stdout}"
    );
}

#[test]
fn the_separate_form_still_works() {
    let dir = Dir::new("sep");
    let (code, stdout, stderr) = run_gen(&dir, &["--rows", "2k", "--prefix", "t"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("2000 rows"), "{stdout}");
}

/// The two spellings are the same run, not merely two runs that both succeed.
#[test]
fn both_forms_write_the_same_bytes() {
    let dir = Dir::new("same");
    assert_eq!(run_gen(&dir, &["--rows=1k", "--prefix", "eq"]).0, 0);
    assert_eq!(run_gen(&dir, &["--rows", "1k", "--prefix", "sep"]).0, 0);
    for side in ["a", "b"] {
        let eq = std::fs::read(dir.0.join(format!("eq_{side}.csv"))).expect("the = run");
        let sep = std::fs::read(dir.0.join(format!("sep_{side}.csv"))).expect("the space run");
        assert_eq!(eq, sep, "side {side} differs between the two spellings");
    }
}

#[test]
fn a_typo_is_an_error_rather_than_the_default() {
    let dir = Dir::new("typo");
    let (code, stdout, stderr) = run_gen(&dir, &["--row", "2k", "--prefix", "t"]);
    assert_eq!(code, 2, "a misspelled flag must not succeed: {stdout}");
    assert!(stderr.contains("unknown option: --row"), "{stderr}");
}

/// `--threads` is a C-generator flag. Accepting and ignoring it here made a
/// harness look like it was steering something it was not.
#[test]
fn a_flag_this_generator_does_not_have_is_an_error() {
    let dir = Dir::new("threads");
    let (code, _, stderr) = run_gen(&dir, &["--threads", "8", "--rows", "2k", "--prefix", "t"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown option: --threads"), "{stderr}");
}

/// The old loop read the *next flag* as the value, so `--rows --out-dir data`
/// failed with "must be a number, got: --out-dir" and blamed the wrong thing.
#[test]
fn a_flag_left_without_a_value_says_so() {
    let dir = Dir::new("noval");
    let (code, _, stderr) = run_gen(&dir, &["--rows", "--prefix", "t"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--rows needs a value"), "{stderr}");
}

/// A negative number is a value, not the next flag.
#[test]
fn a_negative_seed_is_a_value() {
    let dir = Dir::new("seed");
    for form in [["--seed", "-1"], ["--seed=-1", "--rows=1k"]] {
        let (code, _, stderr) = run_gen(&dir, &[form[0], form[1], "--rows", "1k", "--prefix", "s"]);
        assert_eq!(code, 0, "{stderr}");
    }
}

/// It used to be `unwrap_or(7)`: a mistyped seed silently produced a different
/// dataset than the one the command named.
#[test]
fn a_seed_that_is_not_a_number_is_an_error() {
    let dir = Dir::new("badseed");
    let (code, _, stderr) = run_gen(&dir, &["--seed", "abc", "--rows", "1k", "--prefix", "t"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--seed must be a number"), "{stderr}");
}

#[test]
fn help_prints_the_usage_and_exits_clean() {
    let dir = Dir::new("help");
    let (code, stdout, _) = run_gen(&dir, &["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("usage: compara-gen-data"), "{stdout}");
    assert!(stdout.contains("--rows"), "{stdout}");
}
