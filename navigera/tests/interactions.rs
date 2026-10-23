//! Interactions that practice sites showed going wrong, each against an
//! offline page that behaves the same way (`tests/fixtures/interactions_site.py`):
//! a click on a covered element, elements inside frames and shadow roots,
//! multiple and non-breaking-space selects, a slow page, downloads, a modal
//! that wires its buttons once it has opened, an input under a panel, and
//! right clicks.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

fn tool_exe() -> String {
    env!("CARGO_BIN_EXE_navigera").to_string()
}

fn chrome() -> bool {
    let found = navigera::browser::resolve_executable_for(None, true);
    assert!(
        found.is_some() || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found.is_some()
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
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut child = Command::new(python)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/interactions_site.py"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let url = line.trim().strip_prefix("LISTENING ")?.to_string();
    Some(Site { child, url })
}

/// A session with its own browser, quit however the test ends.
struct Session {
    name: String,
    _downloads: Option<tempfile::TempDir>,
}

impl Session {
    fn start(test: &str, extra: &[&str]) -> Session {
        let name = std::env::temp_dir()
            .join(format!("nv-interactions-{test}-{}.sock", std::process::id()))
            .display()
            .to_string();
        let downloads = tempfile::tempdir().unwrap();
        let dir = downloads.path().display().to_string();
        let mut args = vec!["start", "--download-dir", &dir];
        args.extend_from_slice(extra);
        let s = Session { name, _downloads: Some(downloads) };
        s.ok(&args);
        s
    }

    fn run(&self, args: &[&str]) -> (bool, Value) {
        let out = Command::new(tool_exe())
            .arg("--session")
            .arg(&self.name)
            .args(args)
            // Clicks say what they saw change (`effect_trace`): failures show it.
            .env("NAVIGERA_TRACE_EFFECT", "1")
            .stdin(Stdio::null())
            .output()
            .expect("run navigera");
        let text = String::from_utf8_lossy(&out.stdout);
        let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
            panic!(
                "stdout must be one JSON object for {args:?}: {text}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.success(), value)
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (success, r) = self.run(args);
        assert!(success && r["ok"] == true, "{args:?} failed: {r}");
        r["result"].clone()
    }

    fn fails(&self, args: &[&str]) -> String {
        let (success, r) = self.run(args);
        assert!(!success && r["ok"] == false, "{args:?} should fail: {r}");
        r["error"].as_str().unwrap_or_default().to_string()
    }

    fn text(&self, args: &[&str]) -> String {
        self.ok(args).as_str().map(str::to_string).unwrap_or_else(|| self.ok(args).to_string())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = Command::new(tool_exe()).arg("--session").arg(&self.name).arg("quit").output();
    }
}

#[test]
fn a_covered_element_is_not_clicked_unless_forced() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("covered", &[]);
    s.ok(&["goto", &format!("{}/hidden-layers", site.url)]);
    s.ok(&["click", "--selector", "#greenButton"]);
    let err = s.fails(&["click", "--selector", "#greenButton", "--timeout-ms", "600"]);
    assert!(err.contains("covered by button#blueButton"), "{err}");
    assert!(err.contains("--force"), "{err}");
    assert_eq!(s.ok(&["get", "count", ".bg-warning"]), 0, "nothing reached the covered button");
    // --force: DOM events on the element itself, and the page can tell.
    let r = s.ok(&["click", "--selector", "#greenButton", "--force"]);
    assert!(r.get("synthetic_click").is_some(), "{r}");
    assert_eq!(s.ok(&["get", "count", ".bg-warning"]), 1);
}

#[test]
fn locators_reach_into_same_origin_frames() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("frames", &[]);
    s.ok(&["goto", &format!("{}/iframe-form", site.url)]);
    s.ok(&["fill", "--selector", "#email", "ada@example.com"]);
    s.ok(&["click", "--role", "button", "--name", "Subscribe"]);
    s.ok(&["wait", "--text", "Thanks ada@example.com"]);
    assert_eq!(s.text(&["get", "value", "--label", "Email"]), "ada@example.com");

    s.ok(&["goto", &format!("{}/frameset", site.url)]);
    s.ok(&["wait", "--text", "MIDDLE"]);
    assert_eq!(s.text(&["get", "text", "--text", "MIDDLE"]), "MIDDLE");
    assert_eq!(s.text(&["text", "--selector", "#content"]), "LEFT");
    assert_eq!(s.ok(&["get", "count", "#content"]), 4, "every frame's #content");

    s.ok(&["goto", &format!("{}/srcdoc", site.url)]);
    s.ok(&["click", "--text", "Deep button"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "deep clicked");
}

#[test]
fn css_selectors_reach_into_open_shadow_roots() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("shadow", &[]);
    s.ok(&["goto", &format!("{}/shadow-css", site.url)]);
    s.ok(&["click", "--selector", "#buttonGenerate"]);
    assert!(s.text(&["get", "value", "--selector", "#editField"]).starts_with("guid-"));
    s.ok(&["click", "--selector", "#nested"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#nested-out"]), "nested clicked");
    s.ok(&["wait", "--selector", "#nested"]);
}

#[test]
fn selects_add_to_a_multiple_and_ignore_nbsp() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("select", &[]);
    s.ok(&["goto", &format!("{}/multi-select", site.url)]);
    s.ok(&["select", "--selector", "#cars", "volvo"]);
    let r = s.ok(&["select", "--selector", "#cars", "--add", "audi"]);
    assert_eq!(r["selected"], serde_json::json!(["Volvo", "Audi"]), "{r}");
    assert_eq!(s.text(&["get", "text", "--selector", "#picked"]), "volvo,audi");
    // Without --add a select replaces the selection, as a plain click does.
    s.ok(&["select", "--selector", "#cars", "saab", "opel"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#picked"]), "saab,opel");

    s.ok(&["goto", &format!("{}/nbsp-select", site.url)]);
    s.ok(&["select", "--selector", "#city", "New York"]);
    assert_eq!(s.text(&["get", "value", "--selector", "#city"]), "ny");
    let err = s.fails(&["select", "--selector", "#city", "--add", "sf"]);
    assert!(err.contains("takes one option"), "{err}");
}

#[test]
fn the_element_wait_is_the_one_asked_for() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("wait", &["--wait-ms", "600"]);
    s.ok(&["goto", &format!("{}/slow?ms=1500", site.url)]);
    // The session's default applies, and the error says exactly that.
    let err = s.fails(&["click", "--selector", "#late"]);
    assert!(err.contains("within 600 ms"), "{err}");
    // A longer wait on one command.
    s.ok(&["click", "--selector", "#late", "--wait-ms", "4000"]);
    s.ok(&["wait", "--text", "late clicked"]);
}

#[test]
fn downloads_are_saved_and_waited_for() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("downloads", &[]);
    s.ok(&["goto", &format!("{}/download", site.url)]);
    s.ok(&["click", "--selector", "#dl"]);
    let r = s.ok(&["wait", "--download", "*.csv"]);
    let path = r["download"]["path"].as_str().expect("a path").to_string();
    assert!(path.ends_with("report.csv"), "{r}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "id,name\n1,Ada\n2,Grace\n");
    s.ok(&["click", "--text", "Save notes"]);
    let r = s.ok(&["wait", "--download", "notes.txt"]);
    assert_eq!(std::fs::read_to_string(r["download"]["path"].as_str().unwrap()).unwrap(), "notes from a blob\n");
    let list = s.ok(&["downloads", "--json"]);
    assert_eq!(list.as_array().map(Vec::len), Some(2), "{list}");
    s.ok(&["downloads", "--clear"]);
    let err = s.fails(&["wait", "--download", "--timeout-ms", "300"]);
    assert!(err.contains("no finished download"), "{err}");
}

#[test]
fn a_click_waits_for_a_sliding_modal_to_settle() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("modal", &[]);
    for _ in 0..2 {
        s.ok(&["goto", &format!("{}/entry-modal", site.url)]);
        s.ok(&["click", "--role", "button", "--name", "Close"]);
        s.ok(&["wait", "--gone", "#modal"]);
    }
}

#[test]
fn a_field_under_a_panel_is_uncovered_before_typing() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("overlap", &[]);
    s.ok(&["goto", &format!("{}/overlapped", site.url)]);
    s.ok(&["fill", "--selector", "#name", "Prova"]);
    assert_eq!(s.text(&["get", "value", "--selector", "#name"]), "Prova");
    assert_eq!(s.text(&["get", "text", "--selector", "#status"]), "ok");
}

#[test]
fn right_and_double_clicks() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("buttons", &[]);
    s.ok(&["goto", &format!("{}/right-click", site.url)]);
    s.ok(&["click", "--text", "Right Click Me", "--button", "right"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#msg"]), "You have done a right click");
    s.ok(&["dblclick", "--text", "Double Click Me"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#msg"]), "You have done a double click");
    let out = Command::new(tool_exe())
        .args(["--session", &s.name, "click", "--text", "Right Click Me", "--button", "back"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("left, right or middle"));
}

#[test]
fn tabs_by_target_id_and_requests_since() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("tabs", &[]);
    s.ok(&["goto", &format!("{}/right-click", site.url)]);
    s.ok(&["tab-new", &format!("{}/download", site.url)]);
    let tabs = s.ok(&["tab-list"]);
    let first = tabs["tabs"][0]["target"].as_str().expect("a target id")[..10].to_string();
    let r = s.ok(&["tab-select", &first]);
    assert!(r["url"].as_str().unwrap().ends_with("/right-click"), "{r}");

    let log = s.text(&["network"]);
    let last: u64 = log
        .lines()
        .filter_map(|l| l.strip_prefix('#')?.split_whitespace().next()?.parse().ok())
        .max()
        .expect("some requests");
    s.ok(&["goto", &format!("{}/multi-select", site.url)]);
    let since = s.text(&["network", "--since", &last.to_string()]);
    assert!(since.contains("/multi-select") && !since.contains("/right-click"), "{since}");
}

#[test]
fn an_op_after_a_late_reload_finds_the_next_page() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("wizard", &[]);
    for _ in 0..3 {
        s.ok(&["goto", &format!("{}/wizard", site.url)]);
        s.ok(&["fill", "--selector", "#name", "Ada"]);
        // The click saves with a request, and only then reloads: the next
        // op starts on the old page and has to carry on on the new one.
        s.ok(&["click", "--selector", "#next"]);
        s.ok(&["fill", "--selector", "#email", "ada@example.com"]);
        s.ok(&["click", "--selector", "#finish"]);
        assert_eq!(s.text(&["get", "text", "--selector", "#done"]), "Saved Ada / ada@example.com");
    }
}

#[test]
fn a_hidden_control_fails_fast_and_its_card_works() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("cards", &[]);
    s.ok(&["goto", &format!("{}/card-radios", site.url)]);
    let started = std::time::Instant::now();
    let err = s.fails(&["click", "--selector", "#in-s"]);
    assert!(err.contains("isn't visible"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "fails fast, not after the whole wait");
    assert_eq!(s.text(&["get", "text", "--selector", "#picked"]), "none");
    s.ok(&["click", "--role", "radio", "--name", "Medium"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#picked"]), "m");
}

#[test]
fn select_works_on_an_aria_combobox() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("combo", &[]);
    s.ok(&["goto", &format!("{}/combobox", site.url)]);
    let r = s.ok(&["select", "--selector", "#cb", "Banana"]);
    assert_eq!(r["selected"], serde_json::json!(["Banana"]), "{r}");
    assert_eq!(s.text(&["get", "text", "--selector", "#committed"]), "Banana");
    // Not among the first three shown: typed to filter.
    s.ok(&["select", "--label", "Fruit", "cherry"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#committed"]), "Cherry");
    let err = s.fails(&["select", "--selector", "#cb", "Durian", "--timeout-ms", "1200"]);
    assert!(err.contains("no option \"Durian\""), "{err}");
}

#[test]
fn fill_keys_reaches_an_app_that_keeps_only_typing() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("keys", &[]);
    s.ok(&["goto", &format!("{}/typed-only", site.url)]);
    s.ok(&["fill", "--selector", "#amt", "123"]);
    s.ok(&["click", "--selector", "#go"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#model"]), "model=", "a set value is not typing");
    s.ok(&["fill", "--selector", "#amt", "--keys", "28036"]);
    s.ok(&["click", "--selector", "#go"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#model"]), "model=28036");
}

#[test]
fn scroll_to_bottom_scrolls_the_box_itself() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("lazybox", &[]);
    s.ok(&["goto", &format!("{}/lazy-box", site.url)]);
    // Each scroll to the box's end loads 20 more; wait for them rather than
    // a fixed pause (scroll events wait for a rendering step).
    for loaded in [40, 60] {
        let r = s.ok(&["scroll", "--selector", "#list", "--to", "bottom"]);
        assert!(r["scrollTop"].as_u64().unwrap() > 0, "{r}");
        s.ok(&["wait", "--js", &format!("document.querySelectorAll('#list div').length >= {loaded}")]);
    }
    s.ok(&["click", "--text", "Item 55"]);
}

#[test]
fn clicks_only_claim_what_a_user_could_do() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("honesty", &[]);
    s.ok(&["goto", &format!("{}/honesty", site.url)]);
    // Two #dup: the first is parked off the page, the second is the one shown.
    s.ok(&["click", "--selector", "#dup"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "real");
    let err = s.fails(&["click", "--selector", "#save", "--timeout-ms", "400"]);
    assert!(err.contains("disabled"), "{err}");
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "real");
    // Enabled after 600 ms: the click waits for it.
    s.ok(&["click", "--selector", "#unlock-later"]);
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "unlocked");
    // By what it shows: the button, not the product card around it (its
    // aria-label says something longer).
    let r = s.ok(&["click", "--text", "Add to cart"]);
    assert!(r.get("no_effect").is_none(), "{r}");
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "added");
    let err = s.fails(&["fill", "--selector", "#ro", "x", "--timeout-ms", "300"]);
    assert!(err.contains("read-only"), "{err}");
    let err = s.fails(&["fill", "--selector", "#off", "x", "--timeout-ms", "300"]);
    assert!(err.contains("disabled"), "{err}");
}

#[test]
fn forms_inside_cross_origin_frames() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("xo", &[]);
    // Cross-site (localhost in a 127.0.0.1 page), then cross-origin on the
    // same site (another port).
    // By role, then by text: "Pay" in the frame matches exactly and wins
    // over the page's own "Payment details" heading and the frame's title.
    for (page, pay) in
        [("/xo-parent", ["--role", "button", "--name", "Pay"]), ("/xo-parent?same-site", ["--text", "Pay", "", ""])]
    {
        s.ok(&["goto", &format!("{}{page}", site.url)]);
        s.ok(&["fill", "--label", "Card number", "4242424242424242"]);
        s.ok(&["check", "--label", "Save card"]);
        s.ok(&["select", "--selector", "#exp", "02/28"]);
        let mut click = vec!["click"];
        click.extend(pay.iter().filter(|w| !w.is_empty()));
        let r = s.ok(&click);
        assert!(r.get("synthetic_click").is_none(), "{page}: {r}");
        // Text inside the frame, then the page's own (it hears by postMessage).
        s.ok(&["wait", "--text", "sent 4242424242424242"]);
        s.ok(&["wait", "--text", "paid 4242"]);
        assert_eq!(s.text(&["get", "text", "--selector", "#paid"]), "paid 4242 saved 02/28", "{page}");
        assert_eq!(s.text(&["get", "text", "--selector", "#status"]), "sent 4242424242424242", "{page}");
    }
    let err = s.fails(&["click", "--text", "No such button", "--timeout-ms", "400"]);
    assert!(err.contains("cross-origin frame"), "{err}");
}

#[test]
fn a_click_that_changes_nothing_says_so() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("effects", &[]);
    s.ok(&["goto", &format!("{}/effects", site.url)]);
    // Close isn't wired yet: nothing happens, and the click says so instead
    // of a plain success.
    let r = s.ok(&["click", "--selector", "#late"]);
    let note = r["no_effect"].as_str().unwrap_or_default();
    assert!(note.contains("nothing on the page changed"), "{r}");
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "none");
    let r = s.ok(&["click", "--selector", "#noop"]);
    assert!(r.get("no_effect").is_some(), "{r}");
    // Once wired, the same click works and claims nothing extra.
    s.ok(&["eval", "wire()"]);
    let r = s.ok(&["click", "--selector", "#late"]);
    assert!(r.get("no_effect").is_none(), "{r}");
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "closed");
    // Effects a DOM observer alone would miss: a checkbox's state, a value
    // set from script, a request, a change a moment later, a scrolled strip,
    // a change inside another component's shadow root.
    for (target, what) in [
        ("#agree", "checkbox"),
        ("#clear", "field cleared from script"),
        ("#send", "request"),
        ("#later", "change 80 ms later"),
        ("#next", "scrolled strip"),
        ("#copy", "copied to the clipboard"),
        ("#clip", "clipboard write"),
    ] {
        let r = s.ok(&["click", "--selector", target]);
        assert!(r.get("no_effect").is_none(), "{what}: {r}");
    }
    assert_eq!(s.ok(&["get", "checked", "--selector", "#agree"]), true);
    assert_eq!(s.text(&["get", "value", "--selector", "#q"]), "");
    assert_eq!(s.text(&["get", "text", "--selector", "#out"]), "later");
    let r = s.ok(&["click", "--role", "button", "--name", "Bump"]);
    assert!(r.get("no_effect").is_none(), "shadow root: {r}");
    // Typing into a field or opening a <select> isn't judged.
    let r = s.ok(&["click", "--selector", "#q"]);
    assert!(r.get("no_effect").is_none(), "field: {r}");

    // A page that reports every click to analytics: the report alone isn't
    // an effect, and the note says what was sent.
    s.ok(&["goto", &format!("{}/effects?telemetry", site.url)]);
    let r = s.ok(&["click", "--selector", "#noop"]);
    let note = r["no_effect"].as_str().unwrap_or_default();
    assert!(note.contains("only sent GET /event/click (200)"), "{r}");
    // Clicks that do something still say nothing extra there: a save whose
    // result shows when the server answers, a checkbox.
    let r = s.ok(&["click", "--selector", "#save"]);
    assert!(r.get("no_effect").is_none(), "slow save: {r}");
    s.ok(&["wait", "--text", "saved"]);
    let r = s.ok(&["click", "--selector", "#agree"]);
    assert!(r.get("no_effect").is_none(), "checkbox: {r}");
}

#[test]
fn reset_starts_clean_in_the_same_browser() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("reset", &[]);
    s.ok(&["goto", &format!("{}/effects", site.url)]);
    s.ok(&["eval", "(localStorage.setItem('k', 'v'), document.cookie = 'c=1; path=/', 1)"]);
    s.ok(&["tab-new", &format!("{}/honesty", site.url)]);
    let r = s.ok(&["reset"]);
    assert_eq!(r["tabs"].as_array().map(Vec::len), Some(1), "{r}");
    s.ok(&["goto", &format!("{}/effects", site.url)]);
    assert_eq!(s.ok(&["eval", "[localStorage.getItem('k'), document.cookie]"]), serde_json::json!([null, ""]));
    // Cookies set in the new context are the ones `cookies --all` lists.
    s.ok(&["eval", "(document.cookie = 'fresh=1; path=/', 1)"]);
    let all = s.ok(&["cookies", "--all", "--json"]);
    let names: Vec<&str> = all.as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names, ["fresh"], "{all}");
    // Downloads still land where they should in the new context.
    s.ok(&["goto", &format!("{}/download", site.url)]);
    s.ok(&["click", "--selector", "#dl"]);
    let r = s.ok(&["wait", "--download", "*.csv"]);
    assert!(r["download"]["path"].as_str().unwrap_or_default().ends_with("report.csv"), "{r}");
    // Twice in a row: the first context goes away with its tabs.
    s.ok(&["reset"]);
    assert_eq!(s.ok(&["tab-list"])["tabs"].as_array().map(Vec::len), Some(1));
}

#[test]
fn the_mouse_goes_where_it_is_told_on_a_canvas() {
    let (true, Some(site)) = (chrome(), site()) else { return };
    let s = Session::start("mouse", &[]);
    s.ok(&["goto", &format!("{}/canvas", site.url)]);
    let log = |s: &Session| -> Vec<String> {
        let v = s.ok(&["eval", "log.splice(0)"]);
        v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
    };
    // From the canvas's corner: a press, a path, a release draws a line.
    let r = s.ok(&["mouse", "move", "10", "20", "--selector", "canvas"]);
    assert_eq!((r["mouse"]["x"].as_f64(), r["mouse"]["y"].as_f64()), (Some(60.0), Some(60.0)), "{r}");
    s.ok(&["mouse", "down"]);
    s.ok(&["mouse", "move", "110", "20", "--selector", "canvas", "--steps", "10"]);
    let r = s.ok(&["mouse", "up"]);
    assert_eq!(r["mouse"]["held"], serde_json::json!([]), "{r}");
    let events = log(&s);
    assert_eq!(events.first().map(String::as_str), Some("move 10 20 0"), "{events:?}");
    assert!(events.contains(&"mousedown 10 20 0 1".to_string()), "{events:?}");
    assert!(events.contains(&"move 60 20 1".to_string()), "{events:?}");
    assert_eq!(events.last().map(String::as_str), Some("click 110 20 0 0"), "{events:?}");
    assert!(events.contains(&"mouseup 110 20 0 0".to_string()), "{events:?}");
    assert!(s.ok(&["eval", "ink(60, 20)"]).as_u64().unwrap_or(0) > 0, "the line is drawn");
    assert_eq!(s.ok(&["eval", "ink(60, 120)"]), 0, "nothing drawn off the path");
    // Viewport coordinates, a double click, the right button, the wheel.
    s.ok(&["mouse", "click", "250", "140", "--count", "2"]);
    let events = log(&s);
    assert!(events.contains(&"dblclick 200 100 0 0".to_string()), "{events:?}");
    s.ok(&["mouse", "click", "30", "30", "--selector", "canvas", "--button", "right"]);
    let events = log(&s);
    assert!(events.contains(&"mousedown 30 30 2 2".to_string()), "{events:?}");
    // The menu opens on the press (Linux, macOS) or the release (Windows).
    assert!(events.iter().any(|e| e.starts_with("contextmenu 30 30 2 ")), "{events:?}");
    s.ok(&["mouse", "wheel", "0", "-120"]);
    assert_eq!(log(&s), ["wheel 0 -120"]);
    // Mistakes are argument errors, said before anything runs.
    let says = |args: &[&str]| {
        let out = Command::new(tool_exe()).arg("--session").arg(&s.name).args(args).output().unwrap();
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).to_string()
    };
    assert!(says(&["mouse", "jump", "1", "2"]).contains("move, down, up, click or wheel"));
    assert!(says(&["mouse", "move", "5"]).contains("no y"));
}
