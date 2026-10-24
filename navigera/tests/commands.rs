//! The inspection and emulation commands end to end, on Chrome and on
//! Lightpanda: console/errors/network, cookies, storage, pdf, set, route,
//! init-script, locators, get/is, dblclick, check/uncheck, keys.
//!
//! Each scenario runs on every engine it finds (`$CHROME_BIN` or a system
//! Chrome; `$LIGHTPANDA_BIN` or `lightpanda` on PATH);
//! `NAVIGERA_REQUIRE_BROWSER=1` / `NAVIGERA_REQUIRE_LIGHTPANDA=1` turn a
//! missing one into a failure.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
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

/// (engine, browser binary) pairs to run on.
fn engines() -> Vec<(&'static str, Option<PathBuf>)> {
    let mut out = Vec::new();
    let chrome = navigera::browser::resolve_executable_for(None, true).is_some();
    assert!(chrome || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(), "no Chrome found");
    if chrome {
        out.push(("chrome", None));
    }
    let lightpanda = std::env::var_os("LIGHTPANDA_BIN").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(if cfg!(windows) { "lightpanda.exe" } else { "lightpanda" }))
            .find(|p| p.is_file())
    });
    assert!(lightpanda.is_some() || std::env::var_os("NAVIGERA_REQUIRE_LIGHTPANDA").is_none(), "no lightpanda found");
    if let Some(bin) = lightpanda {
        out.push(("lightpanda", Some(bin)));
    }
    out
}

struct Site {
    child: Child,
    base: String,
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn edge_site() -> Option<Site> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/edge_site.py");
    let mut child =
        Command::new(python()).args([script, "0"]).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let base = line.split_whitespace().nth(1)?.to_string();
    Some(Site { child, base })
}

struct Agent {
    session: String,
    engine: &'static str,
    bin: Option<PathBuf>,
}

impl Agent {
    fn new(name: &str, engine: &'static str, bin: Option<PathBuf>) -> Self {
        let session = std::env::temp_dir()
            .join(format!("nv-cmd-{name}-{engine}-{}.sock", std::process::id()))
            .display()
            .to_string();
        let agent = Agent { session, engine, bin };
        agent.ok(&["start", "--idle-timeout-s", "300"]);
        agent
    }

    fn run(&self, args: &[&str]) -> Value {
        let mut cmd = Command::new(tool_exe());
        cmd.args(["--session", &self.session, "--engine", self.engine]);
        if let Some(bin) = &self.bin {
            cmd.arg("--chromium").arg(bin);
        }
        let out = cmd.args(args).stdin(Stdio::null()).output().expect("run navigera");
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(text.trim())
            .unwrap_or_else(|_| panic!("[{}] stdout must be one JSON object for {args:?}: {text}", self.engine))
    }

    fn response(&self, args: &[&str]) -> Value {
        let r = self.run(args);
        assert_eq!(r["ok"], true, "[{}] {args:?} failed: {r}", self.engine);
        r
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.response(args)["result"].clone()
    }

    fn text(&self, args: &[&str]) -> String {
        match self.ok(args) {
            Value::String(s) => s,
            other => other.to_string(),
        }
    }

    fn err(&self, args: &[&str]) -> String {
        let r = self.run(args);
        assert_eq!(r["ok"], false, "[{}] {args:?} should fail: {r}", self.engine);
        r["error"].as_str().unwrap_or_default().to_string()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new(tool_exe()).args(["--session", &self.session, "quit"]).stdin(Stdio::null()).output();
    }
}

#[test]
fn console_errors_and_network() {
    let Some(site) = edge_site() else { return };
    for (engine, bin) in engines() {
        let a = Agent::new("log", engine, bin);
        let state = a.ok(&["goto", &format!("{}/console", site.base)]);
        a.ok(&["wait", "--js", "document.title.startsWith('echo')"]);
        assert_eq!(state["url"].as_str().map(|u| u.ends_with("/console")), Some(true));
        let console = a.text(&["console"]);
        assert!(console.contains("[log] hello"), "[{engine}] {console}");
        assert!(console.contains("[warn] careful"), "[{engine}] {console}");
        assert!(a.text(&["errors"]).contains("[error] bad thing"), "[{engine}]");
        assert!(!a.text(&["errors"]).contains("careful"), "[{engine}] errors are errors only");
        // A handler that throws: the click says so.
        let r = a.response(&["click", "--selector", "#boom"]);
        let note = r["notes"][0].as_str().unwrap_or_default();
        assert!(note.contains("cartTotal is not defined"), "[{engine}] {r}");
        assert!(a.text(&["errors"]).contains("[exception] ReferenceError"), "[{engine}]");
        a.ok(&["console", "--clear"]);
        assert_eq!(a.text(&["console"]), "(no console messages)");

        let list = a.text(&["network"]);
        assert!(list.contains("GET 200 Document /console"), "[{engine}] {list}");
        let line = list
            .lines()
            .find(|l| l.contains("POST 200 Fetch /api/echo"))
            .unwrap_or_else(|| panic!("[{engine}] {list}"));
        let n = line.split_whitespace().next().unwrap().trim_start_matches('#');
        let req = a.ok(&["network", n]);
        assert_eq!(req["post_data"], r#"{"q":"widget"}"#, "[{engine}] {req}");
        assert!(req["body"].as_str().unwrap().contains("widget"), "[{engine}] {req}");
        let har = std::env::temp_dir().join(format!("nv-{engine}-{}.har", std::process::id()));
        a.ok(&["network", "--har", har.to_str().unwrap()]);
        let har: Value = serde_json::from_slice(&std::fs::read(&har).unwrap()).unwrap();
        assert!(har["log"]["entries"].as_array().unwrap().len() >= 2, "[{engine}]");
    }
}

#[test]
fn cookies_storage_pdf_init_scripts_and_routes() {
    let Some(site) = edge_site() else { return };
    let base = &site.base;
    for (engine, bin) in engines() {
        let a = Agent::new("state", engine, bin);
        a.ok(&["goto", &format!("{base}/form")]);
        a.ok(&["cookies", "set", "theme", "dark", "--same-site", "Lax"]);
        a.ok(&["cookies", "set", "token", "t1", "--http-only"]);
        let cookies = a.text(&["cookies"]);
        assert!(
            cookies.contains("theme=dark") && cookies.contains("token=t1; ") && cookies.contains("HttpOnly"),
            "[{engine}] {cookies}"
        );
        assert_eq!(a.ok(&["eval", "document.cookie"]), "theme=dark", "[{engine}] HttpOnly stays out of JS");
        a.ok(&["cookies", "delete", "theme"]);
        assert!(!a.text(&["cookies"]).contains("theme"), "[{engine}]");
        a.ok(&["cookies", "clear"]);
        assert_eq!(a.text(&["cookies"]), "(no cookies)");

        a.ok(&["storage", "set", "k", "v"]);
        assert_eq!(a.ok(&["storage", "get", "k"]), "v");
        a.ok(&["storage", "session", "set", "s", "1"]);
        assert_eq!(a.ok(&["storage", "session"]), serde_json::json!({"s": "1"}));
        a.ok(&["storage", "clear"]);
        assert_eq!(a.ok(&["storage"]), serde_json::json!({}));

        let pdf = std::env::temp_dir().join(format!("nv-{engine}-{}.pdf", std::process::id()));
        let r = a.ok(&["pdf", "--path", pdf.to_str().unwrap()]);
        assert!(r["bytes"].as_u64().unwrap() > 1000, "[{engine}] {r}");
        assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF"), "[{engine}]");

        a.ok(&["init-script", "window.__seen = (window.__seen || 0) + 1"]);
        a.ok(&["goto", &format!("{base}/form")]);
        assert_eq!(a.ok(&["eval", "window.__seen"]), 1, "[{engine}] runs once per document");
        a.ok(&["init-script", "--clear"]);
        a.ok(&["goto", &format!("{base}/form")]);
        assert_eq!(a.ok(&["eval", "String(window.__seen)"]), "undefined", "[{engine}]");

        // A mocked API and a blocked image.
        a.ok(&["route", "*/api/echo*", "--status", "200", "--body", r#"{"got":"mocked"}"#]);
        a.ok(&["route", "*.png"]);
        a.ok(&["goto", &format!("{base}/console")]);
        a.ok(&["wait", "--js", "document.title === 'echo mocked'"]);
        assert!(a.text(&["route"]).contains("*/api/echo* -> fulfill 200 (1 hits)"), "[{engine}]");
        assert_eq!(a.ok(&["unroute"])["removed"], 2);

        // HTTP basic auth.
        a.ok(&["set", "credentials", "ada", "lovelace"]);
        a.ok(&["goto", &format!("{base}/auth")]);
        assert_eq!(a.ok(&["text", "--selector", "h1"]), "welcome ada", "[{engine}]");
        a.ok(&["set", "credentials", "off"]);
    }
}

/// `state save` in one session and `state load` in a fresh one: the cookies
/// (HttpOnly included) and the page's localStorage come back, in the file
/// format Playwright's storageState uses.
#[test]
fn state_saved_in_one_session_loads_in_another() {
    let Some(site) = edge_site() else { return };
    let base = &site.base;
    for (engine, bin) in engines() {
        let file = std::env::temp_dir().join(format!("nv-state-{engine}-{}.json", std::process::id()));
        let file = file.to_str().unwrap();
        {
            let a = Agent::new("save", engine, bin.clone());
            a.ok(&["goto", &format!("{base}/form")]);
            a.ok(&["cookies", "set", "sid", "s3cret", "--http-only"]);
            a.ok(&["storage", "set", "cart", "[1,2]"]);
            let r = a.ok(&["state", "save", file]);
            assert_eq!(r["origins"], 1, "[{engine}] {r}");
            let saved: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            let sid = saved["cookies"].as_array().unwrap().iter().find(|c| c["name"] == "sid").cloned();
            let sid = sid.unwrap_or_else(|| panic!("[{engine}] {saved}"));
            assert_eq!(sid["httpOnly"], true, "[{engine}] {saved}");
            assert_eq!(saved["origins"][0]["origin"], base.as_str(), "[{engine}] {saved}");
            assert_eq!(saved["origins"][0]["localStorage"][0]["name"], "cart", "[{engine}] {saved}");
        }
        let b = Agent::new("load", engine, bin);
        let r = b.ok(&["state", "load", file]);
        assert_eq!((r["cookies"].as_u64(), r["origins"].as_u64()), (Some(1), Some(1)), "[{engine}] {r}");
        b.ok(&["goto", &format!("{base}/form")]);
        assert!(b.text(&["cookies"]).contains("sid=s3cret"), "[{engine}]");
        assert_eq!(b.ok(&["storage", "get", "cart"]), "[1,2]", "[{engine}]");
        assert!(b.err(&["state", "load", "/nonexistent/state.json"]).contains("state load"), "[{engine}]");
        let _ = std::fs::remove_file(file);
    }
}

/// HTML5 drag and drop onto a drop zone, and a slider dragged with the
/// button held, on both engines.
#[test]
fn drag_drops_a_card_and_moves_a_slider() {
    let Some(site) = edge_site() else { return };
    let base = &site.base;
    for (engine, bin) in engines() {
        let a = Agent::new("drag", engine, bin);
        a.ok(&["goto", &format!("{base}/drag")]);
        let r = a.ok(&["drag", "#card", "#done"]);
        assert_eq!(r["drag"]["html5"], true, "[{engine}] {r}");
        assert_eq!(r["drag"]["dropped"], true, "[{engine}] {r}");
        assert_eq!(a.ok(&["eval", "done.contains(card)"]), true, "[{engine}]");
        // Dropped back where nothing accepts it: reported, not an error.
        let r = a.ok(&["drag", "--from-text", "Task 1", "--to-text", "Done"]);
        assert_eq!(r["drag"]["dropped"], true, "[{engine}] {r}");
        let r = a.ok(&["drag", "#card", "#out"]);
        assert_eq!(r["drag"]["dropped"], false, "[{engine}] {r}");
        a.ok(&["drag", "#knob", "#track"]);
        assert_eq!(a.ok(&["eval", "out.textContent"]), "knob 140px", "[{engine}]");
        assert!(a.err(&["drag", "#card", "#nowhere", "--timeout-ms", "300"]).contains("drag"), "[{engine}]");
    }
}

#[test]
fn emulation() {
    let Some(site) = edge_site() else { return };
    for (engine, bin) in engines() {
        let a = Agent::new("emu", engine, bin);
        a.ok(&["goto", &format!("{}/form", site.base)]);
        a.ok(&["set", "viewport", "800", "600"]);
        assert_eq!(a.ok(&["eval", "[innerWidth, innerHeight]"]), serde_json::json!([800, 600]), "[{engine}]");
        a.ok(&["set", "geo", "52.52", "13.405"]);
        let geo = a.ok(&[
            "eval",
            "() => new Promise(r => navigator.geolocation.getCurrentPosition(p => r([p.coords.latitude, p.coords.longitude]), e => r(e.message), {timeout: 3000}))",
        ]);
        assert_eq!(geo, serde_json::json!([52.52, 13.405]), "[{engine}]");
        a.ok(&["set", "headers", "X-Probe:", "1"]);
        if engine == "chrome" {
            a.ok(&["set", "device", "Pixel", "8"]);
            assert_eq!(a.ok(&["eval", "devicePixelRatio"]), 2.625);
            a.ok(&["set", "media", "dark"]);
            assert_eq!(a.ok(&["eval", "matchMedia('(prefers-color-scheme: dark)').matches"]), true);
            a.ok(&["set", "timezone", "Asia/Tokyo"]);
            assert_eq!(a.ok(&["eval", "Intl.DateTimeFormat().resolvedOptions().timeZone"]), "Asia/Tokyo");
            // Replayed on a tab opened later.
            a.ok(&["tab-new", &format!("{}/form", site.base)]);
            assert_eq!(a.ok(&["eval", "devicePixelRatio"]), 2.625, "a new tab gets the device too");
            a.ok(&["set", "offline", "on"]);
            assert_eq!(a.ok(&["eval", "navigator.onLine"]), false, "the page is told it is offline");
            assert!(a.err(&["goto", &format!("{}/form", site.base)]).contains("INTERNET_DISCONNECTED"));
            a.ok(&["set", "offline", "off"]);
            a.ok(&["goto", &format!("{}/form", site.base)]);
            assert_eq!(a.ok(&["eval", "navigator.onLine"]), true, "and back online");
        } else {
            // Settings Lightpanda would accept and ignore are refused.
            assert!(a.err(&["set", "media", "dark"]).contains("does not emulate"));
            assert!(a.err(&["set", "timezone", "Asia/Tokyo"]).contains("at launch only"));
            assert!(a.err(&["set", "offline", "on"]).contains("does not emulate going offline"));
            a.ok(&["set", "offline", "off"]);
            assert!(a.ok(&["set", "device", "Pixel 8"])["note"].as_str().unwrap().contains("viewport only"));
        }
    }
}

#[test]
fn locators_reads_states_and_keys() {
    let Some(site) = edge_site() else { return };
    for (engine, bin) in engines() {
        let a = Agent::new("el", engine, bin);
        a.ok(&["goto", &format!("{}/form", site.base)]);
        a.ok(&["fill", "--label", "Email address", "a@b.c"]);
        assert_eq!(a.ok(&["get", "value", "--placeholder", "you@example.com"]), "a@b.c", "[{engine}]");
        a.ok(&["click", "--role", "button", "--name", "Save"]);
        assert_eq!(a.ok(&["get", "text", "#out"]), "saved", "[{engine}]");
        a.ok(&["click", "--role", "button", "--name", "cancel"]);
        assert_eq!(a.ok(&["get", "text", "#out"]), "cancel", "[{engine}] names match case-insensitively");
        a.ok(&["click", "--testid", "save-btn"]);
        assert_eq!(a.ok(&["get", "text", "--role", "listitem", "--nth", "-1"]), "three", "[{engine}]");
        assert_eq!(a.ok(&["get", "count", "li"]), 3);
        assert_eq!(a.ok(&["get", "attr", "title", "--role", "link"]), "Go");
        assert_eq!(a.ok(&["get", "style", "color", "--role", "link"]), "rgb(255, 0, 0)");
        assert_eq!(a.ok(&["is", "visible", "#hide"]), false);
        assert_eq!(a.ok(&["is", "hidden", "#hide"]), true);
        assert_eq!(a.ok(&["is", "visible", "#nope", "--timeout-ms", "100"]), false);
        assert_eq!(a.ok(&["is", "disabled", "#off"]), true);
        assert_eq!(a.ok(&["is", "editable", "#ro"]), false);

        let r = a.ok(&["check", "--label", "I accept the terms"]);
        assert_eq!((r["checked"].as_bool(), r["changed"].as_bool()), (Some(true), Some(true)), "[{engine}] {r}");
        assert_eq!(a.ok(&["check", "--label", "I accept the terms"])["changed"], false, "[{engine}] already checked");
        a.ok(&["uncheck", "#news"]);
        assert_eq!(a.ok(&["is", "checked", "#news"]), false);

        a.ok(&["dblclick", "#dbl"]);
        assert_eq!(a.ok(&["get", "text", "#dbl"]), "double true", "[{engine}] a trusted double click");

        a.ok(&["focus", "#em"]);
        assert_eq!(a.ok(&["is", "focused", "#em"]), true);
        a.ok(&["keydown", "Shift"]);
        a.ok(&["press", "a"]);
        a.ok(&["keyup", "Shift"]);
        assert_eq!(a.ok(&["get", "text", "#keys"]), "d:Shift d:A(shift) u:A u:Shift", "[{engine}]");
        a.ok(&["insert-text", "!", "--selector", "#em"]);
        assert_eq!(a.ok(&["get", "value", "#em"]), "a@b.cA!", "[{engine}]");
        a.ok(&["wait", "--idle"]);
    }
}
