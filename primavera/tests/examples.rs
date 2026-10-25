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

#[test]
fn http_calls_share_cookies_with_the_browser() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let secrets = secrets_file();
    let (code, out) = primavera(&["run", "tests/fixtures/http.pv", "--base-url", &site.1, "--secrets", &secrets]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("1 passed, 0 failed"), "{out}");
    let (code, out) = primavera(&["run", "tests/fixtures/http-fail.pv", "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("/api/fail answered 500: db down"), "{out}");
    assert!(out.contains("GET http://127.0.0.1:9/: "), "{out}");
}

#[test]
fn feature_files_run_with_the_runs_steps_and_report_cucumber_json() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let report = std::env::temp_dir().join(format!("pv-cucumber-{}.json", std::process::id()));
    let (code, out) = primavera(&[
        "run",
        "tests/fixtures/features",
        "--base-url",
        &site.1,
        "--cucumber",
        &report.display().to_string(),
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("3 passed, 0 failed"), "{out}");
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(j[0]["name"], "Signing in");
    assert_eq!(j[0]["elements"].as_array().map(Vec::len), Some(3));
    let first = &j[0]["elements"][0];
    assert_eq!(first["steps"][0]["keyword"], "Given ");
    assert!(first["steps"].as_array().unwrap().iter().all(|s| s["result"]["status"] == "passed"), "{first}");
    // Allure results: one per scenario, the feature as a label.
    let allure = std::env::temp_dir().join(format!("pv-allure-{}", std::process::id()));
    let (code, out) = primavera(&[
        "run",
        "tests/fixtures/features",
        "--base-url",
        &site.1,
        "--allure",
        &allure.display().to_string(),
    ]);
    assert_eq!(code, 0, "{out}");
    let results: Vec<_> = std::fs::read_dir(&allure).unwrap().filter_map(Result::ok).map(|e| e.path()).collect();
    assert_eq!(results.iter().filter(|p| p.to_string_lossy().ends_with("-result.json")).count(), 3, "{results:?}");
    let one: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&results[0]).unwrap()).unwrap();
    assert!(one["labels"].to_string().contains("Signing in"), "{one}");
    std::fs::remove_dir_all(&allure).ok();
    // A failure: the step that failed, and the ones after it skipped.
    let (code, out) = primavera(&[
        "run",
        "tests/fixtures/features-fail.feature",
        "--base-url",
        &site.1,
        "--timeout-ms",
        "300",
        "--cucumber",
        &report.display().to_string(),
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Then the title is \"Nope\""), "{out}");
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let statuses: Vec<&str> = j[0]["elements"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["result"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["passed", "failed", "skipped"]);
    std::fs::remove_file(&report).ok();
}

#[test]
fn a_config_starts_the_app_and_picks_a_profile() {
    let Some(_turn) = chrome() else { return };
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("pv-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let py = if cfg!(windows) { "python" } else { "python3" };
    let site = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/site.py").replace('\\', "/");
    let config = format!(
        "base_url = \"http://127.0.0.1:{port}\"\npaths = [\"t.pv\"]\n[vars]\ngreeting = \"from the file\"\n\
         [server]\ncommand = \"{py} {site} --port {port}\"\nurl = \"http://127.0.0.1:{port}/login\"\nreuse = false\n\
         [env.other.vars]\ngreeting = \"from other\"\n"
    );
    std::fs::write(dir.join("primavera.toml"), config).unwrap();
    std::fs::write(
        dir.join("t.pv"),
        "scenario \"Config\"\n  goto /login\n  expect text \"Sign in\"\n  expect ${greeting} is ${want}\n",
    )
    .unwrap();
    let bad = format!(
        "[server]\ncommand = \"{py} -c \\\"print('boom'); raise SystemExit(4)\\\"\"\nurl = \"http://127.0.0.1:{port}/\"\n"
    );
    std::fs::write(dir.join("bad.toml"), bad).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
            .arg("run")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("run primavera");
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    };
    let (code, out) = run(&["--var", "want=from the file"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("/login answers ("), "{out}");
    let (code, out) = run(&["--env", "other", "--var", "want=from other"]);
    assert_eq!(code, 0, "{out}");
    // Stopped with the run.
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "the server is still up");
    let (code, out) = run(&["--config", "bad.toml", "t.pv"]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("exited") && out.contains("boom"), "{out}");
    let (code, out) = run(&["--env", "nope"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("no profile [env.nope] (there are: other)"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn screenshots_match_their_baselines() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-shots-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pv = dir.join("look.pv");
    std::fs::write(
        &pv,
        "scenario \"The page looks as it did\"\n  goto /login\n  \
         eval \"(document.body.innerHTML = '<h1>Checkout</h1><p id=clock>${run.id}</p><button id=b style=\\\"width:120px;height:30px;background:${color}\\\">Pay</button>', 1)\"\n  \
         expect screenshot pay --selector \"#b\"\n  expect screenshot page --mask \"#clock\"\n",
    )
    .unwrap();
    let evidence = dir.join("ev").display().to_string();
    let run = |extra: &[&str]| {
        let mut args =
            vec!["run", pv.to_str().unwrap(), "--base-url", &site.1, "--evidence", &evidence, "--timeout-ms", "500"];
        args.extend_from_slice(extra);
        let out = Command::new(env!("CARGO_BIN_EXE_primavera")).args(&args).output().expect("run primavera");
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    };
    // No baseline: written, and the step fails until a person has looked.
    let (code, out) = run(&["--var", "color=blue"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no baseline yet: wrote"), "{out}");
    let (code, out) = run(&["--var", "color=blue", "--update-screenshots"]);
    assert_eq!(code, 0, "{out}");
    let os = std::env::consts::OS;
    let shots = dir.join("look.pv.screenshots");
    let pay = std::fs::read(shots.join(format!("pay-{os}.png"))).unwrap();
    assert_eq!(&pay[1..4], b"PNG");
    // The same again matches, the masked clock changing every run.
    let (code, out) = run(&["--var", "color=blue"]);
    assert_eq!(code, 0, "{out}");
    // A different button doesn't, and says where to look.
    let (code, out) = run(&["--var", "color=red"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("pixels differ") && out.contains("pay-diff.png"), "{out}");
    assert!(dir.join("ev").join("look.pv-pay-actual.png").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn element_screenshots_find_their_element_in_frames_and_scroll_containers() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-shots-el-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A button below the fold of a scroll box, another in an iframe further down.
    let solid = "width:80px;height:30px;border:0;background:rgb(0,0,255)";
    std::fs::write(
        dir.join("page.js"),
        format!(
            "() => {{ document.body.innerHTML = `<div style=\"height:60px;overflow:auto\"><div style=\"height:400px\"></div>\
             <button id=low style=\"{solid}\"></button></div><div style=\"height:300px\"></div>\
             <iframe id=f style=\"margin-left:40px;border:5px solid red\" srcdoc=\"<body style='margin:20px'><button id=framed style='{solid}'></button>\"></iframe>`; return 1; }}"
        ),
    )
    .unwrap();
    let pv = dir.join("el.pv");
    std::fs::write(
        &pv,
        format!(
            "scenario \"Elements\"\n  goto /login\n  eval --file \"{}\"\n  wait --selector \"#framed\"\n  \
             expect screenshot low --selector \"#low\"\n  expect screenshot framed --selector \"#framed\"\n\
             scenario \"A mask on nothing\"\n  goto /login\n  expect screenshot page --mask \"#not-there\"\n",
            dir.join("page.js").display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["run", pv.to_str().unwrap(), "--base-url", &site.1, "--update-screenshots", "--timeout-ms", "2000"])
        .args(["--evidence", dir.join("ev").to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("--mask \"#not-there\" matches nothing on the page"), "{text}");
    // Each crop is the button, solid blue to its corners.
    for name in ["low", "framed"] {
        let path = dir.join("el.pv.screenshots").join(format!("{name}-{}.png", std::env::consts::OS));
        let mut reader = png::Decoder::new(std::fs::File::open(&path).unwrap()).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        let px = info.color_type.samples();
        let at = |x: u32, y: u32| {
            let i = ((y * info.width + x) as usize) * px;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        let (w, h) = (info.width, info.height);
        for (x, y) in [(1, 1), (w - 2, 1), (w / 2, h / 2), (1, h - 2), (w - 2, h - 2)] {
            assert_eq!(at(x, y), (0, 0, 255), "{name} at {x},{y} of {w}x{h}: {}", path.display());
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_navigera_session_becomes_a_scenario_that_passes() {
    let (Some(_turn), Some(site)) = (chrome(), site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-from-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let name = dir.join("work.sock").display().to_string();
    let nv = |args: &[&str]| -> serde_json::Value {
        let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
            .args(["navigera", "--session", &name])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("primavera navigera");
        let text = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("{args:?}: {text}"));
        assert!(v["ok"] == true, "{args:?}: {v}");
        v["result"].clone()
    };
    // What an agent does: look, act on refs.
    nv(&["start"]);
    nv(&["goto", &format!("{}/login", site.1)]);
    let tree = nv(&["ax"]).as_str().unwrap().to_string();
    let r = |text: &str| -> String {
        let line = tree.lines().find(|l| l.contains(text)).unwrap_or_else(|| panic!("{text:?} in\n{tree}"));
        line.split("ref=").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().to_string()
    };
    nv(&["click", &r("button \"Accept all\"")]);
    nv(&["fill", &r("textbox \"Email\""), "ada@example.com"]);
    nv(&["fill", &r("textbox \"Password\""), "hunter2-not-in-the-file"]);
    nv(&["get", "value", "--label", "Email"]);
    nv(&["click", &r("link \"Shop\"")]);
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["from-session", &name, "--scenario", "Shops"])
        .output()
        .unwrap();
    nv(&["quit"]);
    let pv = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{pv}{}", String::from_utf8_lossy(&out.stderr));
    assert!(pv.contains("scenario \"Shops\"\n  goto /login\n"), "{pv}");
    assert!(pv.contains("click --role button --name \"Accept all\"\n"), "{pv}");
    assert!(pv.contains("fill --role textbox --name Email ada@example.com\n"), "{pv}");
    assert!(pv.contains("fill --role textbox --name Password ${secret:PASSWORD}\n"), "{pv}");
    assert!(!pv.contains("hunter2"), "{pv}");
    assert!(pv.contains("click --role link --name Shop\n  expect url ends-with /shop\n  expect text Shop\n"), "{pv}");
    assert!(!pv.contains("get value") && !pv.contains("left out"), "reads stay out, every ref found again: {pv}");
    // And it runs, from a fresh browser.
    let file = dir.join("shops.pv");
    std::fs::write(&file, &pv).unwrap();
    let secrets = dir.join("secrets.env");
    std::fs::write(&secrets, "PASSWORD=from-the-secrets-file\n").unwrap();
    let (code, text) =
        primavera(&["run", file.to_str().unwrap(), "--base-url", &site.1, "--secrets", secrets.to_str().unwrap()]);
    assert_eq!(code, 0, "{pv}\n{text}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn over_mcp_an_agent_explores_writes_the_test_and_runs_it() {
    use std::io::Write as _;
    let (Some(_turn), Some(site)) = (chrome(), site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .arg("mcp")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 0;
    let mut rpc = |method: &str, params: serde_json::Value| -> serde_json::Value {
        id += 1;
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{msg}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("{method}: {line}"))
    };
    let text = |r: &serde_json::Value| r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    let init = rpc("initialize", serde_json::json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["result"]["serverInfo"]["name"], "primavera", "{init}");
    rpc(
        "tools/call",
        serde_json::json!({"name": "browser_navigate", "arguments": {"url": format!("{}/login", site.1)}}),
    );
    let tree = text(&rpc("tools/call", serde_json::json!({"name": "browser_snapshot", "arguments": {}})));
    let line = tree.lines().find(|l| l.contains("button \"Accept all\"")).unwrap_or_else(|| panic!("{tree}"));
    let r: u64 =
        line.split("ref=").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
    rpc("tools/call", serde_json::json!({"name": "browser_click", "arguments": {"ref": r}}));
    let wrote = rpc(
        "tools/call",
        serde_json::json!({"name": "primavera_from_session", "arguments": {"scenario": "Consents", "out": "consent.pv"}}),
    );
    assert!(text(&wrote).contains("click --role button --name \"Accept all\""), "{wrote}");
    let ran = rpc(
        "tools/call",
        serde_json::json!({"name": "primavera_run", "arguments": {"paths": ["consent.pv"], "base_url": site.1}}),
    );
    assert_eq!(ran["result"]["isError"], false, "{}", text(&ran));
    assert!(text(&ran).ends_with("exit 0"), "{}", text(&ran));
    let failure = rpc("tools/call", serde_json::json!({"name": "primavera_failure", "arguments": {}}));
    assert!(text(&failure).contains("no failures"), "{failure}");
    drop(stdin);
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_recording_becomes_a_scenario_that_passes() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-record-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("order.pv");
    // The page uses itself (`?auto=1`): a field, a checkbox, a select, a link.
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["record", &format!("{}/form?auto=1", site.1), "--headless", "--for", "3"])
        .args(["--scenario", "Orders", "--out", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pv = std::fs::read_to_string(&file).unwrap();
    let want = "scenario \"Orders\"\n  goto /form?auto=1\n  fill --role textbox --name Email ada@example.com\n  \
                check --role checkbox --name \"Gift wrap\"\n  click --selector \"#tip >>> button\"\n  \
                select --role combobox --name Size Large\n  \
                click --role link --name \"Place order\"\n  expect url ends-with /done\n";
    assert!(pv.ends_with(want), "{pv}");
    // Run without the page using itself: the steps do it.
    std::fs::write(&file, pv.replace("?auto=1", "")).unwrap();
    let (code, text) = primavera(&["run", file.to_str().unwrap(), "--base-url", &site.1]);
    assert_eq!(code, 0, "{pv}\n{text}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_recording_extends_a_scenario_from_where_it_ends() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-extend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("order.pv");
    let start = "# An order.\nscenario \"Opens the form\"\n  goto /form?auto=2\n  expect text Order\n\nscenario \"Other\"\n  goto /done\n";
    std::fs::write(&file, start).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["record", "--extend", file.to_str().unwrap(), "--scenario", "Opens the form"])
        .args(["--base-url", &site.1, "--headless", "--for", "4"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pv = std::fs::read_to_string(&file).unwrap();
    let want = "# An order.\nscenario \"Opens the form\"\n  goto /form?auto=2\n  expect text Order\n  \
                fill --role textbox --name Email ada@example.com\n  check --role checkbox --name \"Gift wrap\"\n  \
                click --selector \"#tip >>> button\"\n  select --role combobox --name Size Large\n  \
                click --role link --name \"Place order\"\n  expect url ends-with /done\n\nscenario \"Other\"\n  goto /done\n";
    assert_eq!(pv, want);
    std::fs::write(&file, pv.replace("?auto=2", "")).unwrap();
    let (code, text) = primavera(&["run", file.to_str().unwrap(), "--base-url", &site.1]);
    assert_eq!(code, 0, "{text}");
    // A scenario that fails before its end isn't extended.
    std::fs::write(&file, "scenario \"Broken\"\n  goto /form\n  expect text Nowhere --timeout-ms 200\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["record", "--extend", file.to_str().unwrap(), "--base-url", &site.1, "--headless", "--for", "1"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("fails there, before its end"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unsure_failure_asks_the_decision_model_when_there_is_one() {
    let (Some(_turn), Some(site)) = (chrome(), site()) else { return };
    // navigera's stand-in for the model: it picks by words in common.
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mock = concat!(env!("CARGO_MANIFEST_DIR"), "/../navigera/bench/decide/mock_jev.py");
    let mut child = Command::new(python).arg(mock).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let jev = Site(child, line.trim().strip_prefix("LISTENING ").unwrap().to_string());
    let dir = std::env::temp_dir().join(format!("pv-decider-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("t.pv"),
        "scenario \"Consents\"\n  goto /login\n  click --role button --name \"Agree to the cookie banner\" --timeout-ms 300\n",
    )
    .unwrap();
    let pv = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
            .args(args)
            .current_dir(&dir)
            .env("NAVIGERA_DECIDER_URL", &jev.1)
            .env("TYPESAFE_API_KEY", "test-key")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
        )
    };
    let (code, out) = pv(&["run", "t.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("1. button \"Accept all\""), "{out}");
    assert!(out.contains("(decider)"), "{out}");
    let (code, out) = pv(&["fix", "t.pv:3", "--use", "1"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("+ click --role button --name \"Accept all\" --timeout-ms 300"), "{out}");
    let (code, out) = pv(&["run", "t.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 0, "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn accessibility_is_checked_by_axe() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-a11y-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a11y.pv");
    std::fs::write(
        &file,
        "scenario \"Fails\"\n  goto /a11y\n  expect accessible --timeout-ms 300\n\n\
         scenario \"Ignored or scoped\"\n  goto /a11y\n  expect accessible --ignore image-alt,button-name,html-has-lang\n  \
         expect accessible --selector #ok\n  goto /done\n  expect accessible --ignore html-has-lang\n",
    )
    .unwrap();
    let (code, out) = primavera(&["run", file.to_str().unwrap(), "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("1 passed") && out.contains("1 failed"), "{out}");
    assert!(out.contains("to pass axe (wcag2a, wcag2aa, wcag21a, wcag21aa)"), "{out}");
    assert!(out.contains("button-name (critical, 1: button)") && out.contains("image-alt (critical, 1: img)"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_metrics_are_budgets() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-metrics-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("budgets.pv");
    std::fs::write(
        &file,
        "scenario \"Within budget\"\n  goto /form\n  expect ttfb < 10s\n  expect fcp < 10s\n  expect lcp < 10s\n  \
         expect load < 10000ms\n  expect cls < 0.1\n\nscenario \"Over\"\n  goto /form\n  expect lcp < 0.001ms --timeout-ms 200\n",
    )
    .unwrap();
    let (code, out) = primavera(&["run", file.to_str().unwrap(), "--base-url", &site.1]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("1 passed") && out.contains("1 failed"), "{out}");
    assert!(out.contains("expected lcp < 0.001 ms within 200 ms; saw "), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_trace_records_each_step_and_reads_back() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pv = dir.join("t.pv");
    std::fs::write(
        &pv,
        "scenario \"Traced\"\n  goto /login\n  eval \"(console.error('boom-trace'), fetch('/api/n'), 1)\"\n  \
         wait --ms 200\n  expect text \"never there\" --timeout-ms 300\n",
    )
    .unwrap();
    let ev = dir.join("ev");
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["run", pv.to_str().unwrap(), "--base-url", &site.1, "--trace", "on-failure"])
        .args(["--evidence", ev.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let show = |extra: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_primavera")).arg("trace").arg(&ev).args(extra).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
    };
    // The run directory lists its traces; the scenario's shows each step.
    let list = show(&[]);
    let scenario =
        std::fs::read_dir(&ev).unwrap().filter_map(Result::ok).find(|e| e.path().join("trace.jsonl").is_file());
    let scenario = scenario.expect("a trace.jsonl").path();
    let all = Command::new(env!("CARGO_BIN_EXE_primavera")).arg("trace").arg(&scenario).output().unwrap();
    let all = String::from_utf8_lossy(&all.stdout).to_string();
    assert!(all.starts_with("FAIL ") && all.contains("\"Traced\""), "{all}{list}");
    assert!(all.contains("console error: boom-trace"), "{all}");
    assert!(all.contains("net #") && all.contains("/api/n"), "{all}");
    assert!(all.contains("/login \"t\""), "{all}");
    let one = Command::new(env!("CARGO_BIN_EXE_primavera")).arg("trace").arg(&scenario).arg("5").output().unwrap();
    let one = String::from_utf8_lossy(&one.stdout).to_string();
    assert!(one.contains("FAIL") && one.contains("the page at the failure (ax.txt)"), "{one}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_download_is_compared_with_the_expected_file() {
    let exe = std::env::var("PRIMAVERA_COMPARA").unwrap_or_else(|_| "compara".into());
    if std::process::Command::new(&exe).arg("--version").output().is_err() {
        return; // compara isn't here
    }
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-compare-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("expected.csv"), "id,name\n1,Ada\n").unwrap();
    std::fs::write(dir.join("other.csv"), "id,name\n1,Bob\n").unwrap();
    let pv = dir.join("c.pv");
    let scenario = |name: &str, file: &str| {
        format!("scenario \"{name}\"\n  goto /m1\n  click --selector \"#dl\"\n  expect download \"report-*.csv\" compare {file} --key id\n")
    };
    let rows = "scenario \"Rows and a summary\"\n  goto /m1\n  click --selector \"#dl\"\n  \
                expect download \"report-*.csv\" compare other.csv --key id --report json,rows\n";
    std::fs::write(&pv, scenario("The same", "expected.csv") + &scenario("Not the same", "other.csv") + rows).unwrap();
    let ev = dir.join("ev");
    let out = Command::new(env!("CARGO_BIN_EXE_primavera"))
        .args(["run", pv.to_str().unwrap(), "--base-url", &site.1, "--evidence", ev.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("1 passed, 2 failed"), "{text}");
    assert!(text.contains("differs from") && text.contains("changed 1") && text.contains("the report"), "{text}");
    // Each scenario's reports are beside its downloads.
    let names: Vec<String> = std::fs::read_dir(&ev)
        .unwrap()
        .flat_map(|d| std::fs::read_dir(d.unwrap().path()).into_iter().flatten())
        .map(|e| e.unwrap().path().strip_prefix(&ev).unwrap().display().to_string())
        .collect();
    assert!(names.iter().any(|n| n.ends_with("-compare.html")), "{names:?}");
    // The JSON summary and the rows, when asked for.
    assert!(text.contains("the summary") && text.contains("the rows"), "{text}");
    let rows = names.iter().find(|n| n.ends_with("-compare-rows")).expect("a rows directory");
    assert!(std::fs::read_to_string(ev.join(rows).join("changed.csv")).unwrap().contains("Ada,Bob"));
    let json = names.iter().find(|n| n.ends_with("-compare.json")).expect("a summary");
    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(ev.join(json)).unwrap()).unwrap();
    assert_eq!(summary["counts"]["changed"], 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fix_puts_a_suggestion_into_the_failing_step() {
    let (Some(_turn), Some(site)) = (chrome(), own_site()) else { return };
    let dir = std::env::temp_dir().join(format!("pv-fix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("t.pv"),
        "scenario \"Signs in\"\n  goto /login\n  click --role button --name \"Sign on\" --timeout-ms 300\n",
    )
    .unwrap();
    let pv = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_primavera")).args(args).current_dir(&dir).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
        )
    };
    let (code, out) = pv(&["run", "t.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("fix: primavera fix t.pv:3 --use 1"), "{out}");
    let (_, list) = pv(&["fix"]);
    assert!(list.contains("t.pv:3") && list.contains("--role button --name \"Sign in\""), "{list}");
    let (code, out) = pv(&["fix", "t.pv:3", "--use", "1"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("+ click --role button --name \"Sign in\" --timeout-ms 300"), "{out}");
    let (code, out) = pv(&["run", "t.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 0, "{out}");
    // The run that passed left nothing to fix.
    let (code, out) = pv(&["fix", "t.pv:3"]);
    assert_eq!(code, 2, "{out}");
    // A step of a flow in a used file is fixed where it is written.
    std::fs::write(dir.join("lib.pv"), "flow sign_in()\n  click --role button --name \"Sign on\" --timeout-ms 300\n")
        .unwrap();
    std::fs::write(dir.join("u.pv"), "use lib.pv\n\nscenario \"Signs in\"\n  goto /login\n  run sign_in\n").unwrap();
    let (code, out) = pv(&["run", "u.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("fix: primavera fix lib.pv:2 --use 1"), "{out}");
    let (code, out) = pv(&["fix", "lib.pv:2", "--use", "1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = pv(&["run", "u.pv", "--base-url", &site.1, "--evidence", "ev"]);
    assert_eq!(code, 0, "{out}");
    std::fs::remove_dir_all(&dir).ok();
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
