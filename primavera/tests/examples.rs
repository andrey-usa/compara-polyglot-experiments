//! The examples against navigera's bench site, and what a failure says.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

struct Site(Child, String);

impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// navigera's bench site, from the navigera checkout beside this one.
fn site() -> Option<Site> {
    let server = concat!(env!("CARGO_MANIFEST_DIR"), "/../navigera/bench/site/server.py");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut child = Command::new(python)
        .args([server, "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let url = line.trim().strip_prefix("LISTENING ")?.to_string();
    Some(Site(child, url))
}

/// primavera's own small site (`tests/fixtures/site.py`).
fn own_site() -> Option<Site> {
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut child = Command::new(python)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/site.py"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let url = line.trim().strip_prefix("LISTENING ")?.to_string();
    Some(Site(child, url))
}

fn chrome() -> bool {
    let found = navigera::browser::resolve_executable_for(None, true).is_some();
    assert!(found || std::env::var_os("PRIMAVERA_REQUIRE_BROWSER").is_none(), "no Chrome/Chromium found");
    found
}

fn primavera(args: &[&str]) -> (i32, String) {
    let evidence = tempfile_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(args)
        .args(["--evidence", &evidence])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run primavera");
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), text)
}

fn tempfile_dir() -> String {
    std::env::temp_dir().join(format!("primavera-test-{}", std::process::id())).display().to_string()
}

#[test]
fn the_examples_pass_on_the_bench_site() {
    let (true, Some(site)) = (chrome(), site()) else {
        return;
    };
    let junit = std::env::temp_dir().join(format!("primavera-junit-{}.xml", std::process::id()));
    let (code, out) =
        primavera(&["run", "examples/bench-site", "--base-url", &site.1, "--junit", &junit.display().to_string()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("10 passed, 0 failed"), "{out}");
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert!(xml.contains("tests=\"10\" failures=\"0\""), "{xml}");
}

#[test]
fn a_failure_says_what_was_expected_seen_and_why() {
    let (true, Some(site)) = (chrome(), site()) else {
        return;
    };
    let (code, out) = primavera(&["run", "tests/fixtures/failing.pv", "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("expected --selector #cart-count text is \"2\" within 600 ms; saw \"1\""), "{out}");
    assert!(out.contains("note: the step at line 10 (click --selector \"h1\") changed nothing"), "{out}");
    assert!(out.contains("screenshot "), "{out}");
    assert!(
        out.contains("in flow open-docs, failing.pv:15 `expect title contains \"Nope\" --timeout-ms 300`"),
        "{out}"
    );
    assert!(out.contains("nearest on the page:"), "{out}");
    assert!(out.contains("1. button \"Accept all\""), "{out}");
    assert!(out.contains("--role button --name \"Accept all\""), "{out}");
    assert!(out.contains("0 passed, 4 failed"), "{out}");
}

#[test]
fn lint_and_list_need_no_browser() {
    let (code, out) = primavera(&["lint", "examples"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("4 file(s), 10 scenario(s)"), "{out}");
    let (code, out) = primavera(&["list", "examples", "--tag", "smoke"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
    let bad = std::env::temp_dir().join(format!("primavera-bad-{}.pv", std::process::id()));
    std::fs::write(&bad, "scenario \"A\"\n  click 12\n").unwrap();
    let (code, out) = primavera(&["lint", &bad.display().to_string()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains(":2: refs (12) change between runs"), "{out}");
}

#[test]
fn workers_retries_and_reporters() {
    let (true, Some(site)) = (chrome(), own_site()) else { return };
    // Retried: the first try fails, the second passes, and that is flaky.
    let (code, out) =
        primavera(&["run", "tests/fixtures/flaky.pv", "--base-url", &site.1, "--retries", "1", "--workers", "2"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("FLAKY tests/fixtures/flaky.pv:1  Fails once, then passes"), "{out}");
    assert!(out.contains("failed first:"), "{out}");
    assert!(out.contains("1 passed, 1 flaky, 0 failed"), "{out}");
    // --fail-on-flaky makes it fail the run; jsonl says what happened.
    let (_, out) = primavera(&[
        "run",
        "tests/fixtures/flaky.pv",
        "--base-url",
        &site.1,
        "--grep",
        "Steady",
        "--reporter",
        "jsonl",
    ]);
    let first: serde_json::Value = serde_json::from_str(out.lines().next().unwrap_or("")).unwrap_or_default();
    assert_eq!(first["event"], "scenario", "{out}");
    assert_eq!(first["status"], "passed", "{out}");
    let site2 = own_site().unwrap();
    let (code, out) =
        primavera(&["run", "tests/fixtures/flaky.pv", "--base-url", &site2.1, "--retries", "1", "--fail-on-flaky"]);
    assert_eq!(code, 1, "{out}");
    // GitHub annotations: the failing line, no retries.
    let site3 = own_site().unwrap();
    let (code, out) = primavera(&["run", "tests/fixtures/flaky.pv", "--base-url", &site3.1, "--reporter", "github"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("::error file=tests/fixtures/flaky.pv,line=3,title=Fails once%2C then passes::"), "{out}");
}

#[test]
fn shards_split_the_scenarios() {
    let (_, all) = primavera(&["list", "examples"]);
    let mut all: Vec<&str> = all.lines().collect();
    let mut parts: Vec<String> = Vec::new();
    for i in 1..=3 {
        let (code, out) = primavera(&["list", "examples", "--shard", &format!("{i}/3")]);
        assert_eq!(code, 0, "{out}");
        assert!(!out.trim().is_empty(), "shard {i}/3 is empty");
        parts.extend(out.lines().map(str::to_string));
    }
    let mut parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    all.sort();
    parts.sort();
    assert_eq!(all, parts, "the shards cover every scenario once");
    let (code, out) = primavera(&["list", "examples", "--shard", "4/3"]);
    assert_eq!(code, 2, "{out}");
}
