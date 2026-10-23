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
    assert!(out.contains("7 passed, 0 failed"), "{out}");
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert!(xml.contains("tests=\"7\" failures=\"0\""), "{xml}");
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
    assert!(out.contains("0 passed, 2 failed"), "{out}");
}

#[test]
fn lint_and_list_need_no_browser() {
    let (code, out) = primavera(&["lint", "examples"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("2 file(s), 7 scenario(s)"), "{out}");
    let (code, out) = primavera(&["list", "examples", "--tag", "smoke"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
    let bad = std::env::temp_dir().join(format!("primavera-bad-{}.pv", std::process::id()));
    std::fs::write(&bad, "scenario \"A\"\n  click 12\n").unwrap();
    let (code, out) = primavera(&["lint", &bad.display().to_string()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains(":2: refs (12) change between runs"), "{out}");
}
