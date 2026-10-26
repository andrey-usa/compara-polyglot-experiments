//! Handing a browser over: a `--profile` seeded with `--from` from another
//! browser's profile starts signed in as that browser was, and `--natural`
//! shows a page none of the signs of an automated browser that the fixture
//! (`tests/fixtures/natural_site.py`, the checks anti-bot scripts make)
//! looks for.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

fn tool_exe() -> String {
    env!("CARGO_BIN_EXE_navigera").to_string()
}

fn python() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn chrome() -> Option<String> {
    let found = navigera::browser::resolve_executable_for(None, false);
    assert!(
        found.is_some() || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

struct Site {
    child: Child,
    url: String,
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn site() -> Option<Site> {
    let mut child = Command::new(python())
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/natural_site.py"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let url = line.trim().strip_prefix("LISTENING ")?.to_string();
    Some(Site { child, url })
}

fn session_name(test: &str) -> String {
    std::env::temp_dir().join(format!("nv-handoff-{test}-{}.sock", std::process::id())).display().to_string()
}

fn run(session: &str, args: &[&str]) -> (bool, Value) {
    let out = Command::new(tool_exe())
        .arg("--session")
        .arg(session)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run navigera");
    let text = String::from_utf8_lossy(&out.stdout);
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
        panic!("stdout must be one JSON object for {args:?}: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    });
    (out.status.success(), value)
}

fn ok(session: &str, args: &[&str]) -> Value {
    let (success, r) = run(session, args);
    assert!(success && r["ok"] == true, "{args:?} failed: {r}");
    r["result"].clone()
}

/// Quits the session (and its browser) however the test ends.
struct Session {
    name: String,
    close_browser: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut args = vec!["quit"];
        if self.close_browser {
            args.push("--close-browser");
        }
        let _ = Command::new(tool_exe()).arg("--session").arg(&self.name).args(args).output();
    }
}

/// The automation signs the fixture page found, as `name -> true`.
fn signals(session: &str, url: &str) -> Value {
    ok(session, &["goto", url]);
    ok(session, &["wait", "--js", "document.title === 'done'"]);
    let raw = ok(session, &["eval", "JSON.stringify(window.__signals)"]);
    serde_json::from_str(raw.as_str().expect("a string")).expect("signals JSON")
}

fn fired(signals: &Value) -> Vec<String> {
    signals
        .as_object()
        .expect("an object")
        .iter()
        .filter(|(k, v)| !k.starts_with("info_") && v.as_bool() == Some(true))
        .map(|(k, _)| k.clone())
        .collect()
}

#[test]
fn a_profile_seeded_from_another_browser_starts_signed_in() {
    if chrome().is_none() {
        return;
    }
    let Some(site) = site() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user-browser");
    let agent = dir.path().join("agent-profile");
    let user_s = user.display().to_string();
    let agent_s = agent.display().to_string();

    // "The user's browser": a profile that signs in to the site.
    {
        let s = Session { name: session_name("user"), close_browser: true };
        ok(&s.name, &["start", "--profile", &user_s, "--headless"]);
        let r = ok(&s.name, &["goto", &format!("{}/login", site.url)]);
        assert_eq!(r["title"], "signed in", "{r}");
    }
    // Give its browser a moment to write its cookies and storage down.
    std::thread::sleep(std::time::Duration::from_secs(1));

    let s = Session { name: session_name("agent"), close_browser: true };
    ok(&s.name, &["start", "--profile", &agent_s, "--from", &user_s, "--headless"]);
    ok(&s.name, &["goto", &format!("{}/whoami", site.url)]);
    let title = ok(&s.name, &["title"]);
    let who: Value = serde_json::from_str(title.as_str().unwrap()).unwrap();
    assert_eq!(who["user"], "ada", "localStorage came over: {who}");
    assert!(who["cookie"].as_str().unwrap().contains("seen=1"), "{who}");
    let cookies = ok(&s.name, &["cookies"]).to_string();
    assert!(cookies.contains("sid=ada-session"), "the HttpOnly cookie came over: {cookies}");
    drop(s);

    // Seeding a profile whose browser runs is refused, not done under it.
    let s = Session { name: session_name("busy"), close_browser: true };
    ok(&s.name, &["start", "--profile", &agent_s, "--headless"]);
    let (success, r) = run(&session_name("again"), &["start", "--profile", &agent_s, "--from", &user_s, "--headless"]);
    assert!(!success, "seeding a running profile must fail: {r}");
    assert!(r.to_string().contains("quit --close-browser"), "{r}");
}

#[test]
fn natural_mode_shows_no_sign_of_automation() {
    if chrome().is_none() {
        return;
    }
    let Some(site) = site() else {
        return;
    };
    let plain = Session { name: session_name("plain"), close_browser: false };
    ok(&plain.name, &["start"]);
    let before = signals(&plain.name, &format!("{}/", site.url));
    drop(plain);

    let natural = Session { name: session_name("natural"), close_browser: false };
    // chrome-headless-shell can't pass: natural mode says so instead.
    let exe = chrome().unwrap_or_default();
    if Path::new(&exe).file_stem().is_some_and(|s| s == "chrome-headless-shell" || s == "headless_shell") {
        let (success, r) = run(&natural.name, &["start", "--natural"]);
        let r = if success { run(&natural.name, &["goto", &format!("{}/", site.url)]) } else { (success, r) };
        assert!(!r.0 && r.1.to_string().contains("--natural needs the full Chrome"), "{r:?}");
        return;
    }
    ok(&natural.name, &["start", "--natural"]);
    let after = signals(&natural.name, &format!("{}/", site.url));
    assert!(fired(&after).is_empty(), "natural mode shows {:?}: {after}", fired(&after));
    assert!(!after["info_ua"].as_str().unwrap().contains("Headless"), "{after}");
    // What it changed is what headless shows: the user agent says so, in the
    // page, in workers and in requests, unless the browser is a windowed one.
    if before["info_ua"].as_str().unwrap().contains("Headless") {
        assert!(before["header_ua_headless"] == true && before["worker_ua_headless"] == true, "{before}");
    }
}

/// `show` puts a headless session on screen with its tabs and sign-ins, for
/// a person to take over (a 2FA prompt, a CAPTCHA); `hide` takes it back.
/// Both for a browser navigera launched and for a `--profile` one. Needs a
/// display: on Linux, run under `xvfb-run`.
#[test]
fn show_and_hide_keep_the_tabs_and_the_sign_in() {
    if chrome().is_none() || (cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_none()) {
        return;
    }
    let Some(site) = site() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("p").display().to_string();
    for (label, start) in [("launched", vec!["start"]), ("profile", vec!["start", "--profile", &profile, "--headless"])]
    {
        let s = Session { name: session_name(&format!("show-{label}")), close_browser: label == "profile" };
        ok(&s.name, &start);
        ok(&s.name, &["goto", &format!("{}/login", site.url)]);
        ok(&s.name, &["tab-new", &format!("{}/whoami", site.url)]);
        let shown = ok(&s.name, &["show"]);
        assert_eq!(shown["visible"], true, "{label}: {shown}");
        let tabs = ok(&s.name, &["tab-list"]);
        assert_eq!(tabs["tabs"].as_array().map(Vec::len), Some(2), "{label}: the tabs came along: {tabs}");
        assert!(tabs["url"].as_str().unwrap().ends_with("/whoami"), "{label}: the active tab stayed active: {tabs}");
        let ua = ok(&s.name, &["eval", "navigator.userAgent"]).to_string();
        assert!(!ua.contains("Headless"), "{label}: on screen is a windowed browser: {ua}");
        ok(&s.name, &["reload"]);
        let who: Value = serde_json::from_str(ok(&s.name, &["title"]).as_str().unwrap()).unwrap();
        assert_eq!(who["user"], "ada", "{label}: signed in on screen: {who}");
        let hidden = ok(&s.name, &["hide"]);
        assert_eq!(hidden["visible"], false, "{label}: {hidden}");
        ok(&s.name, &["reload"]);
        let who: Value = serde_json::from_str(ok(&s.name, &["title"]).as_str().unwrap()).unwrap();
        assert!(who["cookie"].as_str().unwrap().contains("seen=1") && who["user"] == "ada", "{label}: {who}");
    }
}
