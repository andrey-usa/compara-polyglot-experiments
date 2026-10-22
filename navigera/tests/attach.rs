//! `--attach` and `--profile`: navigera works in a browser it doesn't own
//! and never closes it. "The user's browser" here is a Chrome the test
//! starts itself with remote debugging on its own profile — the same
//! `DevToolsActivePort` handshake Chrome 144+ uses for the default profile
//! once chrome://inspect/#remote-debugging is on.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn tool_exe() -> String {
    env!("CARGO_BIN_EXE_navigera").to_string()
}

/// The fixture servers are Python; Windows installs it as `python`.
fn python() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn chrome() -> Option<String> {
    let found = navigera::browser::resolve_executable_for(None, true);
    assert!(
        found.is_some() || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

fn session_name(test: &str) -> String {
    std::env::temp_dir().join(format!("nv-attach-{test}-{}.sock", std::process::id())).display().to_string()
}

/// One CLI step against a session; returns (exit ok, JSON).
fn run(session: &str, args: &[&str], env: &[(&str, &str)]) -> (bool, Value) {
    let mut cmd = Command::new(tool_exe());
    cmd.arg("--session").arg(session).args(args).stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run navigera");
    let text = String::from_utf8_lossy(&out.stdout);
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
        panic!("stdout must be one JSON object for {args:?}: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    });
    (out.status.success(), value)
}

fn ok(session: &str, args: &[&str]) -> Value {
    let (success, r) = run(session, args, &[]);
    assert!(success && r["ok"] == true, "{args:?} failed: {r}");
    r["result"].clone()
}

fn active_port(dir: &Path, within: Duration) -> Option<(u16, String)> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(dir.join("DevToolsActivePort")) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse() {
                    return Some((port, path.trim().to_string()));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Page targets (url list) of the browser serving DevTools on `port`.
fn page_urls(port: u16) -> Option<Vec<String>> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    // Chrome keeps the connection open: read exactly Content-Length bytes.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let body = loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text[..end]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                .and_then(|v| v.parse::<usize>().ok())?;
            if buf.len() >= end + 4 + len {
                break String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
            }
        }
    };
    let json: Value = serde_json::from_str(&body).ok()?;
    Some(
        json.as_array()?
            .iter()
            .filter(|t| t["type"] == "page")
            .map(|t| t["url"].as_str().unwrap_or("").to_string())
            .collect(),
    )
}

struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn attach_never_closes_the_users_browser() {
    let Some(exe) = chrome() else { return };
    let profile = tempfile::tempdir().unwrap();
    // The user's own browser, with one tab of theirs open.
    let mut user = Browser(
        Command::new(&exe)
            .args(["--headless=new", "--no-sandbox", "--no-first-run", "--remote-debugging-port=0"])
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("data:text/html,<title>Mine</title>users%20own%20tab")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the user's browser"),
    );
    let (port, _) = active_port(profile.path(), Duration::from_secs(30)).expect("DevToolsActivePort");
    let dir = profile.path().display().to_string();
    let s = session_name("user");

    let started = ok(&s, &["start", "--attach", &dir]);
    assert!(started["pid"].is_number(), "{started}");
    let state = ok(&s, &["goto", "data:text/html,<title>Agent</title><h1>agent page</h1>"]);
    assert_eq!(state["title"], "Agent");
    assert!(ok(&s, &["ax"]).as_str().unwrap_or("").contains("agent page"));
    let bye = ok(&s, &["quit"]);
    assert_eq!(bye["browser"], "left open", "{bye}");

    std::thread::sleep(Duration::from_millis(500));
    assert!(user.0.try_wait().unwrap().is_none(), "the user's browser must still run");
    let urls = page_urls(port).expect("browser still serves DevTools");
    assert!(urls.iter().any(|u| u.contains("Mine")), "the user's tab is untouched: {urls:?}");
    assert!(urls.iter().any(|u| u.contains("Agent")), "navigera's window stays too: {urls:?}");

    // A second session reuses the same browser.
    ok(&s, &["start", "--attach", &dir]);
    assert_eq!(ok(&s, &["tab-list"])["tabs"].as_array().map(Vec::len), Some(1), "only navigera's own tab");
    ok(&s, &["quit"]);
    assert!(user.0.try_wait().unwrap().is_none(), "still running after the second session");
}

#[test]
fn profile_browser_outlives_sessions_and_keeps_cookies() {
    let Some(_) = chrome() else { return };
    let site_script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/edge_site.py");
    let mut site = Browser(
        Command::new(python())
            .arg(site_script)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python fixture site"),
    );
    let mut line = String::new();
    BufReader::new(site.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let base = line.split_whitespace().nth(1).expect("LISTENING line").to_string();

    let profile = tempfile::tempdir().unwrap();
    let dir = profile.path().join("p").display().to_string();
    let s = session_name("profile");
    // Chrome for Testing builds (the CI matrix) ship no setuid sandbox.
    let env = [("NAVIGERA_EXTRA_FLAGS", "--no-sandbox")];
    let start = |s: &str| {
        let (success, r) = run(s, &["start", "--profile", &dir, "--headless"], &env);
        assert!(success && r["ok"] == true, "start --profile: {r}");
    };

    // A failed assertion must not leave the (detached) profile browser up.
    struct CloseOnDrop(String);
    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            let _ = run(&self.0, &["quit", "--close-browser"], &[]);
        }
    }

    start(&s);
    let _closer = CloseOnDrop(s.clone());
    let first = active_port(Path::new(&dir), Duration::from_secs(1)).expect("profile browser endpoint");
    ok(&s, &["goto", &format!("{base}/title")]);
    ok(&s, &["eval", "(document.cookie = 'nv=kept; max-age=3600; path=/', 1)"]);
    assert_eq!(ok(&s, &["quit"])["browser"], "left open");

    let started = Instant::now();
    start(&s);
    assert!(started.elapsed() < Duration::from_secs(10), "reconnecting is quick");
    assert_eq!(
        active_port(Path::new(&dir), Duration::from_secs(1)),
        Some(first.clone()),
        "same browser, not a new one"
    );
    ok(&s, &["goto", &format!("{base}/title")]);
    let cookie = ok(&s, &["eval", "document.cookie"]);
    assert!(cookie.as_str().unwrap_or("").contains("nv=kept"), "cookie survives the session: {cookie}");

    // Opt-in: close the profile browser itself.
    assert_eq!(ok(&s, &["quit", "--close-browser"])["browser"], "closed");
    let deadline = Instant::now() + Duration::from_secs(15);
    while page_urls(first.0).is_some() {
        assert!(Instant::now() < deadline, "the profile browser should exit after --close-browser");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn attach_needs_a_session() {
    let out = Command::new(tool_exe())
        .args(["--attach", "chrome", "goto", "example.com"])
        .stdin(Stdio::null())
        .output()
        .expect("run navigera");
    let r: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("one JSON line");
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("use a session"), "{r}");
}
