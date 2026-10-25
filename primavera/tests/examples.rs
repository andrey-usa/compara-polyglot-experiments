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

/// Browser tests take turns: each primavera run is several Chromes
/// already, and a small CI machine running them all at once times out.
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The test's turn with the browser (held while the guard lives), or none
/// without a browser to test with.
fn chrome() -> Option<std::sync::MutexGuard<'static, ()>> {
    let found = navigera::browser::resolve_executable_for(None, true).is_some();
    assert!(found || std::env::var_os("PRIMAVERA_REQUIRE_BROWSER").is_none(), "no Chrome/Chromium found");
    // A test that failed holding it doesn't stop the others.
    found.then(|| TURN.lock().unwrap_or_else(|e| e.into_inner()))
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
    let (Some(_turn), Some(site)) = (chrome(), site()) else {
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
    let (Some(_turn), Some(site)) = (chrome(), site()) else {
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
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
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

#[test]
fn if_blocks_look_at_the_page_as_it_is() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let (code, out) = primavera(&["run", "tests/fixtures/if.pv", "--base-url", &site.1]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("1 passed, 0 failed"), "{out}");
}

#[test]
fn ax_trees_match_partially_and_say_where_they_dont() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let (code, out) = primavera(&["run", "tests/fixtures/ax.pv", "--base-url", &site.1]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("1 passed, 0 failed"), "{out}");
    let (code, out) = primavera(&["run", "tests/fixtures/ax-fail.pv", "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("expected ax --role form --name Checkout to match the tree under it"), "{out}");
    assert!(out.contains("nearest: `- combobox \"Country\" value=\"Canada\""), "{out}");
}

#[test]
fn exec_runs_programs_and_checks_what_they_said() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let py = if cfg!(windows) { "PY=python" } else { "PY=python3" };
    let (code, out) = primavera(&["run", "tests/fixtures/exec.pv", "--base-url", &site.1, "--var", py]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("1 passed, 0 failed"), "{out}");
    let (code, out) = primavera(&["run", "tests/fixtures/exec-fail.pv", "--var", py]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("exited 2: db down"), "{out}");
    assert!(out.contains("still ran after 300 ms; stopped it"), "{out}");
}

/// A GET to the fixture site, its body.
fn fetch(site: &Site, path: &str) -> String {
    use std::io::{Read, Write};
    let hostport = site.1.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(hostport).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()
}

fn secrets_file() -> String {
    let path = std::env::temp_dir().join(format!("primavera-secrets-{}.env", std::process::id()));
    std::fs::write(&path, "# test only\nPV_TEST_PW=\"hunter22\"\n").unwrap();
    path.display().to_string()
}

#[test]
fn the_newer_subjects_and_isolation_between_scenarios() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    for workers in ["1", "2"] {
        let (code, out) = primavera(&["run", "tests/fixtures/m1.pv", "--base-url", &site.1, "--workers", workers]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("5 passed, 1 failed"), "workers {workers}: {out}");
        assert!(out.contains("FAIL  tests/fixtures/m1.pv:22  Soft expectations fail it at the end"), "{out}");
        assert!(out.contains("2 soft expectation(s) didn't hold"), "{out}");
    }
    // Every run of a scenario downloads into a directory of its own: the
    // same file name each time, not `report-2026 (2).csv`.
    let repeat = ["--grep", "Subjects", "--repeat", "3", "--workers", "2"];
    let (code, out) = primavera(&[&["run", "tests/fixtures/m1.pv", "--base-url", &site.1][..], &repeat].concat());
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("3 passed, 0 failed"), "{out}");
}

#[test]
fn auth_runs_once_and_hooks_wrap_the_file() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let secrets = secrets_file();
    let (code, out) =
        primavera(&["run", "tests/fixtures/auth.pv", "--base-url", &site.1, "--workers", "2", "--secrets", &secrets]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("5 passed, 0 failed"), "{out}");
    // Two auths, each signed in once, for five scenarios on two workers.
    assert_eq!(fetch(&site, "/logins"), "2", "{out}");
    assert_eq!(fetch(&site, "/notes"), "setup,teardown", "{out}");
    // Again: `shopper` signs in anew, `keeper` (ttl 1h) is reused.
    let (code, out) = primavera(&["run", "tests/fixtures/auth.pv", "--base-url", &site.1, "--secrets", &secrets]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(fetch(&site, "/logins"), "3", "{out}");
    // A secret never shows in what the run prints.
    let (code, out) =
        primavera(&["run", "tests/fixtures/secret-fail.pv", "--base-url", &site.1, "--secrets", &secrets]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("saw \"***\""), "{out}");
    assert!(!out.contains("hunter22"), "{out}");
    // A file whose `before all` fails runs none of its scenarios.
    let (code, out) = primavera(&["run", "tests/fixtures/before-fail.pv", "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("before all: expected title is \"not this one\""), "{out}");
    let (_, out) = primavera(&["lint", "tests/fixtures/auth.pv", "--secrets", &secrets]);
    assert!(out.contains("ok: 1 file(s), 5 scenario(s)") && !out.contains("warning"), "{out}");
}

#[test]
fn handoff_waits_only_when_it_must() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let (code, out) = primavera(&["run", "tests/fixtures/handoff.pv", "--base-url", &site.1]);
    assert_eq!(code, 0, "{out}");
    // Nobody at a screen here: it says so instead of waiting.
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        let (code, out) = primavera(&["run", "tests/fixtures/handoff-wait.pv", "--base-url", &site.1]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("handoff needs a person at a screen"), "{out}");
    }
}

#[test]
fn a_pause_leaves_the_failing_tab_to_navigera() {
    use std::io::Write;
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let mut child = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["run", "tests/fixtures/pause.pv", "--base-url", &site.1, "--pause-on-failure"])
        .args(["--evidence", &tempfile_dir()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut seen = String::new();
    let ws = loop {
        let line = rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| panic!("no pause: {seen}"));
        seen.push_str(&line);
        seen.push('\n');
        if let Some(i) = line.find("--attach ") {
            break line[i + "--attach ".len()..].trim().to_string();
        }
    };
    assert!(ws.contains("/devtools/page/"), "{ws}");
    let config = navigera::protocol::SessionConfig { attach: Some(ws), ..Default::default() };
    let mut look = navigera::protocol::Driver::launch(&config).expect("attach to the paused tab");
    let here = look.run(&navigera::protocol::Command::Url {}).unwrap();
    assert_eq!(here["title"], "paused here", "{here}");
    look.session().close();
    child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn repeat_and_shuffle_hunt_flakes() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let (code, out) =
        primavera(&["run", "tests/fixtures/flaky.pv", "--base-url", &site.1, "--repeat", "3", "--shuffle", "42"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("shuffled with seed 42"), "{out}");
    assert!(out.contains("FLAKY 2/3  tests/fixtures/flaky.pv  Fails once, then passes"), "{out}");
    assert!(out.contains("    3/3  tests/fixtures/flaky.pv  Steady"), "{out}");
    assert!(out.contains("Steady #3"), "{out}");
    // The same seed, the same order.
    let order = |out: &str| -> Vec<String> {
        out.lines()
            .filter(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
            .map(|l| l.split("  ").nth(2).unwrap_or("").to_string())
            .collect()
    };
    let (_, again) = primavera(&["list", "tests/fixtures/flaky.pv"]);
    assert_eq!(again.lines().count(), 2, "{again}");
    let site2 = own_site().unwrap();
    let (_, out2) =
        primavera(&["run", "tests/fixtures/flaky.pv", "--base-url", &site2.1, "--repeat", "3", "--shuffle", "42"]);
    assert_eq!(order(&out), order(&out2), "{out}\n{out2}");
}

#[test]
fn set_values_fill_in_their_variables() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let (code, out) = primavera(&["run", "tests/fixtures/set-expand.pv", "--base-url", &site.1, "--var", "WHO=ada"]);
    assert_eq!(code, 0, "{out}");
    // Annotations don't take the console's place.
    let (code, out) = primavera(&[
        "run",
        "tests/fixtures/set-expand.pv",
        "--base-url",
        &site.1,
        "--var",
        "WHO=ada",
        "--reporter",
        "github",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("1 passed, 0 failed"), "{out}");
}
