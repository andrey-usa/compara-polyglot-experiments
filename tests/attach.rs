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

/// The body of a GET on the DevTools HTTP endpoint at `port`.
fn devtools_get(port: u16, path: &str) -> Option<String> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
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
    Some(body)
}

/// Page targets (id, url) of the browser serving DevTools on `port`.
fn page_targets(port: u16) -> Option<Vec<(String, String)>> {
    let json: Value = serde_json::from_str(&devtools_get(port, "/json/list")?).ok()?;
    Some(
        json.as_array()?
            .iter()
            .filter(|t| t["type"] == "page")
            .map(|t| (t["id"].as_str().unwrap_or("").to_string(), t["url"].as_str().unwrap_or("").to_string()))
            .collect(),
    )
}

/// Page targets (url list) of the browser serving DevTools on `port`.
fn page_urls(port: u16) -> Option<Vec<String>> {
    Some(page_targets(port)?.into_iter().map(|(_, url)| url).collect())
}

struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The user's own browser, with one tab of theirs open. A CI Windows
/// runner sometimes starts Chrome without it ever writing its
/// DevToolsActivePort: a second launch, on a fresh profile, gets one.
fn users_browser(exe: &str) -> (Browser, tempfile::TempDir, u16) {
    let launch = || {
        let profile = tempfile::tempdir().unwrap();
        let mut user = Browser(
            Command::new(exe)
                .args(["--headless=new", "--no-sandbox", "--no-first-run", "--remote-debugging-port=0"])
                .arg(format!("--user-data-dir={}", profile.path().display()))
                .arg("data:text/html,<title>Mine</title>users%20own%20tab")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start the user's browser"),
        );
        match active_port(profile.path(), Duration::from_secs(20)) {
            Some((port, _)) => {
                // The port can be up before the start-up tab is listed (a
                // loaded runner: the tests then found no "Mine").
                let end = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < end
                    && !page_urls(port).is_some_and(|u| u.iter().any(|u| u.contains("Mine")))
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Some((user, profile, port))
            }
            None => {
                eprintln!("no DevToolsActivePort after 20 s (exited: {:?}); launching again", user.0.try_wait());
                None
            }
        }
    };
    launch().or_else(launch).expect("DevToolsActivePort")
}

#[test]
fn attach_never_closes_the_users_browser() {
    let Some(exe) = chrome() else { return };
    let (mut user, profile, port) = users_browser(&exe);
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
fn a_tab_closed_under_an_attached_session_is_named_and_replaced() {
    let Some(exe) = chrome() else { return };
    let (_user, _profile, port) = users_browser(&exe);
    let id = page_targets(port)
        .and_then(|t| t.into_iter().find(|(_, url)| url.contains("Mine")))
        .map(|(id, _)| id)
        .expect("the user's tab");
    let s = session_name("closed");
    ok(&s, &["start", "--attach", &format!("ws://127.0.0.1:{port}/devtools/page/{id}")]);
    assert_eq!(ok(&s, &["eval", "document.title"]), "Mine");

    // The user closes the tab.
    devtools_get(port, &format!("/json/close/{id}")).expect("close the tab");
    let deadline = Instant::now() + Duration::from_secs(10);
    while page_targets(port).is_some_and(|t| t.iter().any(|(t, _)| *t == id)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Every op that needs the tab says it is gone, not a raw CDP error.
    for args in [&["eval", "document.title"][..], &["goto", "data:text/html,x"][..]] {
        let (success, r) = run(&s, args, &[]);
        let error = r["error"].as_str().unwrap_or("");
        assert!(!success && error.contains("the tab is closed") && error.contains("tab-new"), "{args:?}: {r}");
    }
    assert_eq!(ok(&s, &["status"])["closed"], true);
    let tabs = ok(&s, &["tab-list"]);
    assert_eq!(tabs["tabs"][0]["closed"], true, "{tabs}");

    // A new tab takes its place; the closed one is gone from the list.
    let tabs = ok(&s, &["tab-new", "data:text/html,<title>Again</title>"]);
    assert_eq!(tabs["tabs"].as_array().map(Vec::len), Some(1), "{tabs}");
    assert!(tabs["tabs"][0].get("closed").is_none(), "{tabs}");
    assert_eq!(ok(&s, &["eval", "document.title"]), "Again");
    assert!(ok(&s, &["status"]).get("closed").is_none());
    ok(&s, &["quit"]);
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

#[test]
fn a_tab_closed_while_an_op_waits_on_it_ends_the_op() {
    let Some(exe) = chrome() else { return };
    let (_user, _profile, port) = users_browser(&exe);
    let id = page_targets(port)
        .and_then(|t| t.into_iter().find(|(_, url)| url.contains("Mine")))
        .map(|(id, _)| id)
        .expect("the user's tab");
    let s = session_name("midop");
    ok(&s, &["start", "--attach", &format!("ws://127.0.0.1:{port}/devtools/page/{id}")]);
    let tabs = ok(&s, &["tab-new", "data:text/html,<title>Second</title>"]);
    let second = tabs["tabs"][1]["target"].as_str().expect("second tab").to_string();
    let slow = "new Promise(r => setTimeout(() => r(document.title), 20000))";
    let close_soon = |target: String| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            devtools_get(port, &format!("/json/close/{target}"));
        })
    };

    // The active tab of two: the op ends when it closes, on the other tab.
    let closer = close_soon(second);
    let started = Instant::now();
    let (success, r) = run(&s, &["eval", slow], &[]);
    closer.join().unwrap();
    assert!(!success && r["error"].as_str().unwrap_or("").contains("closed while this ran"), "{r}");
    assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
    assert_eq!(ok(&s, &["eval", "document.title"]), "Mine");

    // The only tab: the op ends, and the session knows the tab is gone.
    let closer = close_soon(id);
    let started = Instant::now();
    let (success, r) = run(&s, &["eval", slow], &[]);
    closer.join().unwrap();
    assert!(!success && r["error"].as_str().unwrap_or("").contains("the tab is closed"), "{r}");
    assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
    assert_eq!(ok(&s, &["status"])["closed"], true);
    ok(&s, &["quit"]);
}
