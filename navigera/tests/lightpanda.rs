//! The Lightpanda engine (`--engine lightpanda`) end to end: the Acme Supply
//! flows that differ from Chrome under the hood (dialogs answered from an
//! armed response, popups that are frames rather than targets, iframes and
//! stylesheets that `serve` only loads on request, a hit test that can't see
//! into shadow roots, accessibility names computed from content) and a
//! navigation that must wait for a slow server and the page's scripts.
//!
//! Runs when `$LIGHTPANDA_BIN` or `lightpanda` on PATH is found; with
//! `NAVIGERA_REQUIRE_LIGHTPANDA=1` (CI) a missing binary is a failure.

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

fn lightpanda() -> Option<PathBuf> {
    let found = std::env::var_os("LIGHTPANDA_BIN").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(if cfg!(windows) { "lightpanda.exe" } else { "lightpanda" }))
            .find(|p| p.is_file())
    });
    assert!(
        found.is_some() || std::env::var_os("NAVIGERA_REQUIRE_LIGHTPANDA").is_none(),
        "NAVIGERA_REQUIRE_LIGHTPANDA is set but no lightpanda binary was found"
    );
    found
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

/// Start a fixture server (`args` pick a free port) and read its base URL.
fn start_site(script: &str, args: &[&str]) -> Option<Site> {
    let script = format!("{}/{script}", env!("CARGO_MANIFEST_DIR"));
    let mut child =
        Command::new(python()).arg(&script).args(args).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let base = line.split_whitespace().nth(1);
    assert!(base.is_some() || std::env::var_os("NAVIGERA_REQUIRE_LIGHTPANDA").is_none(), "{script} did not start");
    Some(Site { child, base: base?.to_string() })
}

/// One named Lightpanda session, driven one CLI call per step like an agent.
struct Agent {
    session: String,
    bin: PathBuf,
}

impl Agent {
    fn new(name: &str, bin: PathBuf) -> Self {
        let session =
            std::env::temp_dir().join(format!("nv-lp-{name}-{}.sock", std::process::id())).display().to_string();
        let agent = Agent { session, bin };
        agent.ok(&["start", "--idle-timeout-s", "300"]);
        agent
    }

    fn run(&self, args: &[&str]) -> Value {
        let out = Command::new(tool_exe())
            .args(["--session", &self.session, "--engine", "lightpanda", "--chromium"])
            .arg(&self.bin)
            .args(args)
            // `serve` is headless: no X server, no xvfb-run.
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .output()
            .expect("run navigera");
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(text.trim()).unwrap_or_else(|_| {
            panic!(
                "stdout must be one JSON object for {args:?}: {text}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    /// The whole response of a step that must succeed.
    fn ok_response(&self, args: &[&str]) -> Value {
        let r = self.run(args);
        assert_eq!(r["ok"], true, "{args:?} failed: {r}");
        r
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.ok_response(args)["result"].clone()
    }

    fn ax(&self, extra: &[&str]) -> String {
        let mut args = vec!["ax"];
        args.extend_from_slice(extra);
        self.ok(&args).as_str().expect("ax returns text").to_string()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new(tool_exe()).args(["--session", &self.session, "quit"]).stdin(Stdio::null()).output();
    }
}

fn server_state(base: &str) -> Value {
    use std::io::{Read, Write};
    let addr = base.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).expect("connect site");
    write!(s, "GET /__state HTTP/1.0\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    serde_json::from_str(&body[body.find("\r\n\r\n").expect("http body") + 4..]).expect("state json")
}

#[test]
fn goto_waits_for_a_slow_server_and_the_load_event() {
    let Some(bin) = lightpanda() else {
        eprintln!("skipping: no lightpanda binary");
        return;
    };
    let Some(site) = start_site("tests/fixtures/edge_site.py", &["0"]) else {
        eprintln!("skipping: python3 not available for the fixture site");
        return;
    };
    let agent = Agent::new("slow", bin);
    // The server answers after 1.5 s; goto used to give up after 500 ms
    // and report success on the old page.
    let started = std::time::Instant::now();
    let state = agent.ok(&["goto", &format!("{}/slow?ms=1500", site.base)]);
    assert!(started.elapsed() >= std::time::Duration::from_millis(1400), "returned before the response");
    assert_eq!(state["title"], "slow", "{state}");
    // Scripts ran before goto returned: the load event has fired.
    let ready = agent.ok(&["eval", "document.readyState"]);
    assert_eq!(ready, "complete");
}

#[test]
fn acme_supply_on_lightpanda() {
    let Some(bin) = lightpanda() else {
        eprintln!("skipping: no lightpanda binary");
        return;
    };
    let Some(site) = start_site("bench/site/server.py", &["--port", "0"]) else {
        eprintln!("skipping: python3 not available for the fixture site");
        return;
    };
    let base = site.base.clone();
    let url = |path: &str| format!("{base}{path}");
    let agent = Agent::new("acme", bin);

    // Orders table: every cell readable (Lightpanda names the table with
    // all of its text and leaves the cells unnamed).
    agent.ok(&["goto", &url("/account")]);
    // Lightpanda says `*****` for every password field; the snapshot says
    // what the field holds, as Chrome's does.
    let login = agent.ax(&[]);
    assert!(login.contains("textbox \"Password\" [required, ref="), "{login}");
    assert!(!login.contains("*****"), "an empty password field reads as filled:\n{login}");
    agent.ok(&["fill", "--selector", "#email", "demo@acme.test"]);
    agent.ok(&["fill", "--selector", "#password", "hunter2"]);
    let login = agent.ax(&[]);
    assert!(login.contains("value=\"•••••••\""), "{login}");
    agent.ok(&["press", "Enter", "--selector", "#password"]);
    agent.ok(&["click", "--text", "Next page"]);
    let table = agent.ax(&["--selector", "table"]);
    assert!(table.contains("- cell: A-1042"), "cells carry their text:\n{table}");
    assert!(table.contains("Awaiting pickup"), "{table}");
    assert!(!table.contains("table \""), "the table is not named with its own text:\n{table}");

    // Docs open in a new tab (target=_blank): one tab here, so the docs
    // load in it and the response says so.
    agent.ok(&["goto", &url("/shop")]);
    let r = agent.ok_response(&["click", "--text", "Docs"]);
    assert!(r["result"]["url"].as_str().unwrap().ends_with("/docs"), "{r}");
    let note = r["notes"][0].as_str().unwrap_or_default();
    assert!(note.contains("/docs") && note.contains("one tab"), "{r}");
    agent.ok(&["click", "--text", "Returns FAQ"]);
    let faq = agent.ax(&["--selector", "#returns-faq"]);
    assert!(faq.contains("37 days"), "{faq}");

    // Shadow-DOM coupon: no hit test inside shadow roots, and no 3 s wait
    // for one either.
    agent.ok(&["goto", &url("/shop?q=brass+sprocket")]);
    agent.ok(&["click", "--text", "Accept all"]);
    agent.ok(&["click", "--text", "Add Brass Sprocket to cart"]);
    agent.ok(&["wait", "--js", "document.querySelector('#cart-count').textContent === '1'"]);
    agent.ok(&["goto", &url("/cart")]);
    // Lightpanda's tree stops at the shadow root; the snapshot still shows
    // what is inside it, and how to reach it.
    let cart = agent.ax(&[]);
    assert!(cart.contains("- shadow root of <acme-coupon>"), "{cart}");
    assert!(cart.contains("  - textbox \"Coupon code\"\n"), "{cart}");
    assert!(cart.contains("  - button \"Apply coupon\""), "{cart}");
    assert!(!agent.ax(&["--selector", "table"]).contains("shadow root"), "a scope leaves it out");
    agent.ok(&["fill", "--text", "Coupon code", "SAVE10"]);
    let started = std::time::Instant::now();
    let r = agent.ok_response(&["click", "--text", "Apply coupon"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "coupon click took {:?}", started.elapsed());
    assert!(r["result"].get("synthetic_click").is_none(), "no overlay warning on lightpanda: {r}");
    agent.ok(&["wait", "--text", "Discount (SAVE10)"]);

    // Checkout: card fields live in an iframe (loaded only with
    // --load-resources iframe); Place order asks confirm(), which must
    // return true for the order to be placed.
    // The button's click handler sets location.href: Lightpanda reports
    // that navigation only after the click's reply, and the click still
    // returns the page it leads to rather than the cart, `loading`.
    let r = agent.ok_response(&["click", "--text", "Proceed to checkout"]);
    assert!(r["result"]["url"].as_str().unwrap().ends_with("/checkout"), "{r}");
    assert!(r["result"].get("loading").is_none(), "{r}");
    agent.ok(&["fill", "--selector", "#name", "Ada Lovelace"]);
    agent.ok(&["select", "--selector", "#country", "Canada"]);
    agent.ok(&["click", "--selector", "input[value=express]"]);
    agent.ok(&["click", "--text", "Continue to payment"]);
    let pay = agent.ax(&[]);
    assert!(pay.contains("Card number"), "the payment iframe is loaded:\n{pay}");
    agent.ok(&[
        "eval",
        "() => { const d = document.querySelector('#payframe').contentDocument; \
         for (const [id, v] of [['card','4242 4242 4242 4242'],['exp','12/30'],['cvc','123']]) \
         { const el = d.getElementById(id); el.value = v; el.dispatchEvent(new Event('input')); } return true; }",
    ]);
    let r = agent.ok_response(&["click", "--text", "Place order"]);
    assert_eq!(r["dialogs"][0]["type"], "confirm", "{r}");
    assert_eq!(r["dialogs"][0]["accepted"], true, "{r}");
    agent.ok(&["wait", "--url", "/orders/"]);
    assert_eq!(server_state(&base)["orders"].as_array().map(Vec::len), Some(1), "one order placed");

    // A dismiss policy reaches the page too, and is reported as such.
    agent.ok(&["dialog", "--dismiss"]);
    let r = agent.ok_response(&["eval", "() => confirm('Really?')"]);
    assert_eq!(r["result"], false, "{r}");
    assert_eq!(r["dialogs"][0]["accepted"], false, "{r}");
    agent.ok(&["dialog", "--accept", "--prompt-text", "Ada"]);
    assert_eq!(agent.ok(&["eval", "() => prompt('Name?')"]), "Ada");

    // A long settings form reads as on Chrome: checkboxes without the
    // default value "on" that looks like a state, a select named by its
    // label alone, one line per table cell.
    agent.ok(&["goto", &url("/preferences")]);
    let prefs = agent.ax(&[]);
    assert!(!prefs.contains("value=\"on\""), "{prefs}");
    assert!(prefs.contains("- combobox \"Currency\" [ref="), "{prefs}");
    assert!(prefs.contains("- cell: checkbox \"Email alerts for Drive\" [checked, ref="), "{prefs}");
    assert!(prefs.lines().count() < 190, "{} lines", prefs.lines().count());
    assert!(prefs.contains("- checkbox \"Price-drop alerts\" [ref="), "{prefs}");
    assert!(prefs.contains("- checkbox \"Weekly newsletter\" [checked, ref="), "{prefs}");
    // A click changes the state the tree shows. Lightpanda's own tree keeps
    // the `checked` attribute the page loaded with, so a ticked box read as
    // unticked and an agent clicked it back.
    agent.ok(&["click", "--text", "Price-drop alerts"]);
    agent.ok(&["click", "--text", "Weekly newsletter"]);
    agent.ok(&["click", "--text", "Compact"]);
    let prefs = agent.ax(&[]);
    assert!(prefs.contains("- checkbox \"Price-drop alerts\" [checked, ref="), "{prefs}");
    assert!(prefs.contains("- checkbox \"Weekly newsletter\" [ref="), "{prefs}");
    assert!(prefs.contains("- radio \"Compact\" [checked, ref="), "{prefs}");
    assert!(prefs.contains("- radio \"Comfortable\" [ref="), "{prefs}");

    // Support form inside an iframe: in the tree, with refs.
    agent.ok(&["goto", &url("/support")]);
    let support = agent.ax(&[]);
    assert!(support.contains("button \"Submit request\" [ref="), "{support}");
}

#[test]
fn start_refuses_a_session_running_another_engine() {
    let Some(bin) = lightpanda() else {
        eprintln!("skipping: no lightpanda binary");
        return;
    };
    let agent = Agent::new("engine", bin);
    let status = agent.ok(&["status"]);
    assert_eq!(status["engine"], "lightpanda", "{status}");
    // The same session name without --engine asks for Chrome.
    let out = Command::new(tool_exe()).args(["--session", &agent.session, "start"]).output().expect("run navigera");
    let r: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"].as_str().unwrap().contains("already running the lightpanda engine"), "{r}");
    // Asking for the engine it runs is fine.
    assert_eq!(agent.ok(&["start"])["already_running"], true);
}
