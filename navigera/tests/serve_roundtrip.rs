//! Serve-protocol roundtrip test: drives `navigera serve` with `data:` URLs (no network).
//! Needs a real Chrome/Chromium/Edge binary; skipped where none is installed.

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

fn tool_exe() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let name = if cfg!(windows) { "navigera.exe" } else { "navigera" };
    // Integration tests live in `target/<profile>/deps/`; the tool binary is
    // two levels up in `target/<profile>/`.
    let direct = exe.with_file_name(name);
    if direct.exists() {
        return direct;
    }
    let parent = exe.parent().and_then(|p| p.parent()).map(|p| p.join(name)).expect("tool binary path");
    assert!(parent.exists(), "build the tool first: cargo build --bin navigera ({})", parent.display());
    parent
}

/// Spawn `navigera serve` with piped stdio for a scripted session.
fn spawn_serve(extra: &[&str]) -> (Child, std::io::BufWriter<std::process::ChildStdin>) {
    let tool = tool_exe();
    let mut child = Command::new(&tool)
        .arg("serve")
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn navigera serve");
    let stdin = child.stdin.take().expect("piped stdin");
    (child, std::io::BufWriter::new(stdin))
}

/// Feed one JSON line, read one JSON response line.
fn roundtrip(
    stdin: &mut std::io::BufWriter<std::process::ChildStdin>,
    stdout: &mut std::io::BufReader<std::process::ChildStdout>,
    request: &Value,
) -> Value {
    use std::io::BufRead;
    writeln!(stdin, "{}", serde_json::to_string(request).unwrap()).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).expect("response must be one JSON object")
}

/// True when the tool itself would find a browser to launch (same resolver
/// as `navigera`: $CHROME_BIN, system Chrome, Playwright/Puppeteer
/// caches). `NAVIGERA_REQUIRE_BROWSER=1` (set in CI) turns a skip into a failure,
/// so a runner without Chrome can't pass the e2e tests by skipping them.
fn chrome_available() -> bool {
    let found = navigera::browser::resolve_executable_for(None, true).is_some();
    assert!(
        found || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

/// Drive a full serve session offline: every page is a `data:` URL, so no
/// network or real browser behavior is asserted — only the protocol contract.
/// Needs a Chrome binary; skipped where none is installed.
#[test]
fn navigera_serve_protocol_roundtrip() {
    if !chrome_available() {
        eprintln!("skipping: no Chrome/Chromium/Edge binary installed");
        return;
    }
    let (mut child, mut stdin) = spawn_serve(&[]);
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdout = std::io::BufReader::new(stdout);

    // Malformed line -> ok:false, session stays alive.
    {
        use std::io::Write;
        writeln!(stdin, "not json").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response["ok"], false);
    }

    // Unknown op -> ok:false with the request id echoed back.
    let bad = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 7, "op": "frobnicate"}));
    assert_eq!(bad["id"], 7);
    assert_eq!(bad["ok"], false);

    let page: String = "data:text/html,".to_string()
        + &"<title>Tool%20Test</title><h1>Hello</h1><input id='kw' value=''>".replace(' ', "%20");

    let opened = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 1, "op": "tab-new", "url": page}));
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["result"]["active"], 1);

    let title = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 2, "op": "title"}));
    assert_eq!(title["ok"], true, "{title}");
    assert_eq!(title["result"], "Tool Test");

    let typed = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 3, "op": "fill", "selector": "#kw", "value": "600-10070"}),
    );
    assert_eq!(typed["ok"], true, "{typed}");

    let readback = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 4, "op": "eval", "expression": "() => document.querySelector('#kw').value"}),
    );
    assert_eq!(readback["ok"], true, "{readback}");
    assert_eq!(readback["result"], "600-10070");

    let tabs = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 5, "op": "tab-list"}));
    assert_eq!(tabs["ok"], true, "{tabs}");
    assert_eq!(tabs["result"]["tabs"].as_array().unwrap().len(), 2);

    // `ax` -> act on a ref -> verify: the agent loop without CSS selectors.
    let added = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 10, "op": "eval", "expression":
            "() => { const b = document.createElement('button'); b.textContent = 'Go'; b.onclick = (e) => { document.title = e.isTrusted ? 'clicked' : 'synthetic'; }; document.body.appendChild(b); return true; }"}),
    );
    assert_eq!(added["ok"], true, "{added}");
    let ax = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 11, "op": "ax", "format": "json"}));
    assert_eq!(ax["ok"], true, "{ax}");
    let nodes = ax["result"].as_array().expect("ax returns an array");
    let find = |role: &str, name: Option<&str>| -> u64 {
        nodes
            .iter()
            .find(|n| n["role"] == role && name.is_none_or(|want| n["name"] == want))
            .and_then(|n| n["ref"].as_u64())
            .unwrap_or_else(|| panic!("no {role} {name:?} in compact ax: {}", ax["result"]))
    };
    let heading = find("heading", Some("Hello"));
    assert!(heading > 0);
    assert!(
        nodes.iter().all(|n| n["role"] != "generic" && n["role"] != "InlineTextBox"),
        "compact ax drops structural wrappers: {}",
        ax["result"]
    );
    let button = find("button", Some("Go"));
    let clicked = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 12, "op": "click", "ref": button}));
    assert_eq!(clicked["ok"], true, "{clicked}");
    let title = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 13, "op": "title"}));
    assert_eq!(title["result"], "clicked", "click by ref fires the handler with a trusted (CDP Input) event");
    let textbox = find("textbox", None);
    let filled = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 14, "op": "fill", "ref": textbox, "value": "via-ref"}),
    );
    assert_eq!(filled["ok"], true, "{filled}");
    let readback = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 15, "op": "eval", "expression": "() => document.querySelector('#kw').value"}),
    );
    assert_eq!(readback["result"], "via-ref");
    let full = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 16, "op": "ax", "all": true}));
    assert!(
        full["result"].as_array().map(Vec::len).unwrap_or(0) >= nodes.len(),
        "`all` is a superset of the compact view"
    );

    // Auto-wait: an element that appears 300 ms later is still clickable.
    let scheduled = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 17, "op": "eval", "expression":
            "() => { setTimeout(() => { const b = document.createElement('button'); b.id = 'late'; b.textContent = 'Late'; b.onclick = () => { document.title = 'late-clicked'; }; document.body.appendChild(b); }, 300); return true; }"}),
    );
    assert_eq!(scheduled["ok"], true, "{scheduled}");
    let late = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 18, "op": "click", "selector": "#late", "timeout_ms": 5000}),
    );
    assert_eq!(late["ok"], true, "click waits for the element: {late}");
    let title = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 19, "op": "title"}));
    assert_eq!(title["result"], "late-clicked");
    let missing = roundtrip(
        &mut stdin,
        &mut stdout,
        &serde_json::json!({"id": 20, "op": "click", "selector": "#never", "timeout_ms": 600}),
    );
    assert_eq!(missing["ok"], false);
    assert!(
        missing["error"].as_str().unwrap_or("").contains("within"),
        "a missing element fails with the page's own message: {missing}"
    );
    let elapsed = missing["elapsed_ms"].as_f64().expect("elapsed_ms is a number");
    assert!(elapsed > 300.0 && elapsed < 3000.0, "waited ~350 ms, then failed: {missing}");

    // A JSON array on one line is a batch: one response line per command,
    // in order, and a failing command doesn't stop the rest. A leading BOM
    // (PowerShell 5 pipes one) is ignored.
    {
        let bom = '\u{feff}';
        writeln!(
            stdin,
            r#"{bom}[{{"id":30,"op":"title"}},{{"id":31,"op":"frobnicate"}},{{"id":32,"op":"eval","expression":"1+1"}}]"#
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let response: Value = serde_json::from_str(line.trim()).unwrap();
            ids.push((response["id"].as_i64().unwrap_or(-1), response["ok"].as_bool().unwrap_or(false)));
            if response["id"] == 32 {
                assert_eq!(response["result"], 2, "{response}");
            }
        }
        assert_eq!(ids, vec![(30, true), (31, false), (32, true)]);
    }

    // PowerShell's `echo '{..}\n{..}'` sends a literal backslash-n: the
    // error says so instead of only "must be a JSON object".
    {
        writeln!(stdin, r#"{{"op":"title"}}\n{{"op":"url"}}"#).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap_or("").contains("JSON array"), "{response}");
    }

    let bye = roundtrip(&mut stdin, &mut stdout, &serde_json::json!({"id": 6, "op": "quit"}));
    assert_eq!(bye["ok"], true);
    assert_eq!(bye["result"]["bye"], true);

    drop(stdin);
    let status = child.wait().expect("serve exits after quit");
    assert!(status.success(), "serve exit status: {status}");
}

/// Run `navigera <args>` to completion; returns (success, stdout JSON).
fn run_tool(args: &[&str]) -> (bool, Value) {
    let out = Command::new(tool_exe()).args(args).stdin(Stdio::null()).output().expect("run navigera");
    let text = String::from_utf8_lossy(&out.stdout);
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
        panic!("stdout must be one JSON object for {args:?}: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    });
    (out.status.success(), value)
}

/// Run `navigera <args>`; returns (success, raw stdout).
fn run_tool_raw(args: &[&str]) -> (bool, String) {
    let out = Command::new(tool_exe()).args(args).stdin(Stdio::null()).output().expect("run navigera");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Named session: a detached server keeps one browser warm across separate
/// processes — the way an agent's shell tool calls arrive.
#[test]
fn navigera_named_session_across_processes() {
    if !chrome_available() {
        eprintln!("skipping: no Chrome/Chromium/Edge binary installed");
        return;
    }
    let socket = std::env::temp_dir().join(format!("nv-test-{}.sock", std::process::id()));
    let socket = socket.to_str().expect("utf-8 temp path").to_string();
    let s = socket.as_str();

    let (ok, started) = run_tool(&["--session", s, "--idle-timeout-s", "120", "start"]);
    assert!(ok, "start: {started}");
    let (ok, again) = run_tool(&["--session", s, "start"]);
    assert!(ok && again["result"]["already_running"] == true, "{again}");

    let page = "data:text/html,<title>Session%20Test</title><a%20href='https://example.com/x'>Link</a>";
    let (ok, nav) = run_tool(&["--session", s, "goto", "--url", page]);
    assert!(ok, "goto: {nav}");
    let (ok, title) = run_tool(&["--session", s, "title"]);
    assert!(ok, "title: {title}");
    assert_eq!(title["result"], "Session Test", "same warm tab across calls");
    let (ok, ax) = run_tool(&["--session", s, "ax"]);
    assert!(ok, "ax: {ax}");
    let tree = ax["result"].as_str().expect("ax is a text tree by default");
    assert!(tree.starts_with("page \"Session Test\""), "{tree}");
    assert!(tree.contains("- link \"Link\" [ref="), "{tree}");
    let (ok, raw) = run_tool_raw(&["--session", s, "--raw", "title"]);
    assert!(ok && raw == "Session Test\n", "--raw prints the bare result: {raw:?}");

    // The flat JSON snapshot carries link URLs too.
    let (ok, flat) = run_tool(&["--session", s, "ax", "--format", "json"]);
    assert!(ok, "ax json: {flat}");
    let link = flat["result"].as_array().and_then(|nodes| nodes.iter().find(|n| n["role"] == "link"));
    assert!(link.and_then(|l| l["url"].as_str()).is_some_and(|u| u.ends_with("/x")), "link has its url: {flat}");

    // Scripts from a file or stdin skip the shell's quoting entirely.
    let script = std::env::temp_dir().join(format!("nv-test-{}.js", std::process::id()));
    std::fs::write(
        &script,
        "() => [...document.querySelectorAll('a')].map(a => a.textContent + \"\\\\\" + /\\d+/.source)",
    )
    .unwrap();
    let (ok, from_file) = run_tool(&["--session", s, "eval", "--file", script.to_str().unwrap()]);
    let _ = std::fs::remove_file(&script);
    assert!(ok && from_file["result"] == serde_json::json!(["Link\\\\d+"]), "eval --file: {from_file}");
    let mut piped = Command::new(tool_exe())
        .args(["--session", s, "eval", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run navigera eval -");
    piped.stdin.take().unwrap().write_all(b"document.title + '!'").unwrap();
    let out = piped.wait_with_output().unwrap();
    let from_stdin: Value = serde_json::from_slice(&out.stdout).expect("eval - prints JSON");
    assert_eq!(from_stdin["result"], "Session Test!", "eval -: {from_stdin}");

    let (ok, bye) = run_tool(&["--session", s, "quit"]);
    assert!(ok, "quit: {bye}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::path::Path::new(s).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!std::path::Path::new(s).exists(), "socket removed after quit");
    let (ok, gone) = run_tool(&["--session", s, "title"]);
    assert!(!ok && gone["ok"] == false, "no server after quit: {gone}");
    // The error quotes the last server's log, which says how it ended.
    assert!(gone["error"].as_str().unwrap_or("").contains("shut down"), "{gone}");
    let _ = std::fs::remove_file(std::path::Path::new(s).with_extension("log"));
}
