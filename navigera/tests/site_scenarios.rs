//! Realistic end-to-end scenarios against the local Acme Supply site
//! (`bench/site/server.py`), driven exactly the way an agent drives
//! navigera: one CLI process per step against a named session, reading
//! the page through `ax` refs.
//!
//! Each step is a pattern that breaks naive browser automation: SPA routing
//! with late-rendered content, Enter-only search, infinite scroll, a cookie
//! banner, a :hover menu, a shadow-DOM coupon widget, a card form inside an
//! iframe, confirm() dialogs, a target=_blank docs tab with a collapsed
//! <details>, login with redirects, a paginated table, a support form in an
//! iframe with a file upload, and a page whose load event never fires.
//! The server records carts/orders/tickets, so the test checks what really
//! happened, not just what the page showed.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

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

fn start_site() -> Option<Site> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/bench/site/server.py");
    let mut child = Command::new(python())
        .args([script, "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let base = line.trim().strip_prefix("LISTENING ")?.to_string();
    Some(Site { child, base })
}

struct Agent {
    session: String,
    /// Recent successful steps, replayed in failure messages.
    history: std::cell::RefCell<Option<Vec<String>>>,
}

/// Page facts that explain a lost click or a missing element.
const DIAG_JS: &str = "() => ({ url: location.href, ready: document.readyState, \
    visibility: document.visibilityState, focus: document.hasFocus(), \
    scroll: [scrollX, scrollY], viewport: [innerWidth, innerHeight], \
    active: document.activeElement && document.activeElement.outerHTML.slice(0, 160), \
    hovered: [...document.querySelectorAll(':hover')].map(e => e.tagName + (e.id ? '#' + e.id : '')).join(' > '), \
    pageinfo: (document.getElementById('pageinfo') || {}).textContent, \
    events: window.__nvEvents || null, \
    body: document.body ? document.body.innerText.slice(0, 600) : null })";

/// Records visibility changes and pointer events with timestamps, so a
/// click that "did nothing" shows whether it reached the page at all.
const EVENT_LOG_JS: &str = "() => { window.__nvEvents = []; \
    const log = (what) => window.__nvEvents.push(what + '@' + Math.round(performance.now())); \
    document.addEventListener('visibilitychange', () => log('vis:' + document.visibilityState)); \
    for (const t of ['mousemove', 'mousedown', 'mouseup', 'click']) \
      document.addEventListener(t, e => log(t + ':' + (e.target.id || e.target.tagName)), true); \
    return true }";

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!(
            "{}…",
            &s[..s.char_indices().take_while(|(i, _)| *i < n).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0)]
        )
    }
}

impl Agent {
    /// Run one step; returns the parsed JSON response (panics on bad JSON).
    fn run(&self, args: &[&str]) -> Value {
        let out = Command::new(tool_exe())
            .arg("--session")
            .arg(&self.session)
            .args(args)
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

    /// Run a step that must succeed; returns its `result`. A failure dumps
    /// the page state (what the previous steps left behind) so a CI-only
    /// flake explains itself in the `cargo test failure` annotation.
    fn ok(&self, args: &[&str]) -> Value {
        let r = self.run(args);
        if r["ok"] != true {
            panic!("{args:?} failed: {r}\n--- page state ---\n{}", self.diag());
        }
        if let Some(last) = self.history.borrow_mut().as_mut() {
            last.push(format!("{args:?} -> {}", truncate(&r.to_string(), 300)));
        }
        r["result"].clone()
    }

    fn diag(&self) -> String {
        let state = self.run(&["eval", "--expression", DIAG_JS]);
        let ax = self.run(&["ax", "--limit", "60"]);
        let steps = self
            .history
            .borrow()
            .as_ref()
            .map(|h| h.iter().rev().take(8).rev().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        format!(
            "{}\n--- last steps ---\n{steps}\n--- ax ---\n{}",
            state["result"],
            ax["result"].as_str().unwrap_or(&ax.to_string())
        )
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

/// `[ref=N]` on the first `ax` line containing `needle`.
fn find_ref(ax: &str, needle: &str) -> String {
    let line =
        ax.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle:?} in ax:\n{ax}"));
    let start = line.find("ref=").unwrap_or_else(|| panic!("no ref on {line:?}")) + 4;
    line[start..].chars().take_while(char::is_ascii_digit).collect()
}

fn server_state(base: &str) -> Value {
    // The session's browser is the only HTTP client we have; ask the site
    // through a one-shot eval-free path: plain std TCP.
    use std::io::{Read, Write};
    let addr = base.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).expect("connect site");
    write!(s, "GET /__state HTTP/1.0\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    let json = &body[body.find("\r\n\r\n").expect("http body") + 4..];
    serde_json::from_str(json).expect("state json")
}

#[test]
fn acme_supply_shopping_docs_login_support() {
    if !chrome_available() {
        eprintln!("skipping: no Chrome/Chromium binary installed");
        return;
    }
    let Some(site) = start_site() else {
        eprintln!("skipping: python3 not available for the fixture site");
        return;
    };
    let base = site.base.clone();
    let url = |path: &str| format!("{base}{path}");
    let agent = Agent {
        session: std::env::temp_dir().join(format!("nv-site-{}.sock", std::process::id())).display().to_string(),
        history: std::cell::RefCell::new(Some(Vec::new())),
    };
    agent.ok(&["start", "--idle-timeout-s", "300"]);
    eprintln!("browser: {}", agent.ok(&["eval", "--expression", "navigator.userAgent"]));

    // --- Shop: late-rendered SPA list, cookie banner, infinite scroll ------
    let state = agent.ok(&["goto", &url("/")]);
    assert!(state["url"].as_str().unwrap().ends_with("/shop"), "/ redirects to /shop: {state}");
    agent.ok(&["click", "--text", "Accept all"]);
    agent.ok(&["wait", "--text", "Showing 12 of 60"]);
    agent.ok(&["scroll", "--to", "bottom"]);
    agent.ok(&["wait", "--text", "Showing 24 of 60"]);

    // Enter-only search: fill by ref, then press Enter in the box.
    let ax = agent.ax(&[]);
    let search = find_ref(&ax, "searchbox \"Search products\"");
    assert!(!ax.contains("- text: Search products"), "label text next to its control is dropped:\n{ax}");
    agent.ok(&["fill", &search, "brass sprocket"]);
    let state = agent.ok(&["press", "Enter", "--ref", &search]);
    assert!(state["url"].as_str().unwrap().contains("q=brass"), "SPA pushState route: {state}");
    agent.ok(&["wait", "--text", "Showing 1 of 1"]);
    for _ in 0..2 {
        agent.ok(&["click", "--text", "Add Brass Sprocket to cart"]);
    }
    agent.ok(&["wait", "--js", "document.querySelector('#cart-count').textContent === '2'"]);

    // <select> by label; SPA product page via a link ref; history back.
    agent.ok(&["goto", &url("/shop")]);
    agent.ok(&["select", "--selector", "#sort", "Price: high to low"]);
    agent.ok(&["wait", "--url", "sort=price-desc"]);
    agent.ok(&["fill", "--selector", "#q", "titanium widget"]);
    agent.ok(&["press", "Enter", "--selector", "#q"]);
    agent.ok(&["wait", "--text", "Showing 1 of 1"]);
    let ax = agent.ax(&["--selector", "#grid"]);
    let link = find_ref(&ax, "heading \"Titanium Widget\"");
    let state = agent.ok(&["click", &link]);
    assert!(state["url"].as_str().unwrap().contains("/shop/p/"), "{state}");
    agent.ok(&["wait", "--selector", "#stock"]);
    let detail = agent.ax(&["--selector", "#detail"]);
    assert!(detail.contains("In stock:"), "{detail}");
    agent.ok(&["fill", &find_ref(&detail, "spinbutton \"Quantity\""), "1"]);
    agent.ok(&["click", &find_ref(&detail, "button \"Add to cart\"")]);
    agent.ok(&["wait", "--text", "Added to cart"]);
    let state = agent.ok(&["back"]);
    assert!(state["url"].as_str().unwrap().contains("q=titanium"), "back to the search: {state}");
    agent.ok(&["forward"]);

    // A third item, removed again through a confirm() dialog.
    agent.ok(&["goto", &url("/shop?q=steel+bolt")]);
    agent.ok(&["click", "--text", "Add Steel Bolt to cart"]);
    agent.ok(&["wait", "--text", "Added to cart"]);

    // :hover menu opens on hover and shows in the snapshot.
    agent.ok(&["hover", "--text", "Account ▾"]);
    let menu = agent.ax(&["--selector", ".menu"]);
    assert!(menu.contains("link \"Orders\""), "hover opens the menu:\n{menu}");

    // --- Cart: confirm() dialog, shadow-DOM coupon ---------------------------
    agent.ok(&["goto", &url("/cart")]);
    let cart = agent.ax(&[]);
    let removed = agent.run(&["click", &find_ref(&cart, "\"Remove Steel Bolt\"")]);
    assert_eq!(removed["ok"], true, "{removed}");
    let dialogs = removed["dialogs"].as_array().expect("the confirm() is reported");
    assert_eq!(dialogs[0]["type"], "confirm");
    assert_eq!(dialogs[0]["accepted"], true);
    agent.ok(&["wait", "--gone", "[aria-label='Remove Steel Bolt']"]);
    let cart = agent.ax(&[]);
    assert!(!cart.contains("Steel Bolt"), "{cart}");
    let coupon = find_ref(&cart, "textbox \"Coupon code\"");
    agent.ok(&["fill", &coupon, "SAVE10"]);
    agent.ok(&["click", &find_ref(&cart, "button \"Apply coupon\"")]);
    agent.ok(&["wait", "--text", "Discount (SAVE10)"]);

    // --- Checkout: select, radios, iframe card form, confirm, redirect ----
    agent.ok(&["click", "--text", "Proceed to checkout"]);
    agent.ok(&["fill", "--selector", "#name", "Ada Lovelace"]);
    let checkout = agent.ax(&[]);
    agent.ok(&["select", &find_ref(&checkout, "combobox \"Country\""), "Canada"]);
    agent.ok(&["click", &find_ref(&checkout, "radio \"Express")]);
    agent.ok(&["click", "--text", "Continue to payment"]);
    agent.ok(&["wait", "--selector", "#step2:not([hidden])"]);
    let pay = agent.ax(&[]);
    assert!(pay.contains("Iframe \"Secure card payment\""), "{pay}");
    agent.ok(&["fill", &find_ref(&pay, "textbox \"Card number\""), "4242 4242 4242 4242"]);
    agent.ok(&["fill", &find_ref(&pay, "textbox \"Expiry (MM/YY)\""), "12/30"]);
    agent.ok(&["fill", &find_ref(&pay, "textbox \"CVC\""), "123"]);
    let placed = agent.run(&["click", "--text", "Place order"]);
    assert_eq!(placed["ok"], true, "{placed}");
    assert!(placed["dialogs"][0]["message"].as_str().unwrap_or("").starts_with("Place order for $"), "{placed}");
    let state = agent.ok(&["wait", "--url", "/orders/"]);
    assert!(state["title"].as_str().unwrap().contains("Order placed"), "{state}");
    let st = server_state(&base);
    let order = &st["orders"][0];
    assert_eq!(order["country"], "Canada", "{st}");
    assert_eq!(order["speed"], "express");
    assert_eq!(order["coupon"], "SAVE10");
    assert_eq!(order["name"], "Ada Lovelace");
    let items: Vec<(String, u64)> = order["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["name"].as_str().unwrap().to_string(), i["qty"].as_u64().unwrap()))
        .collect();
    assert!(items.contains(&("Brass Sprocket".into(), 2)), "{items:?}");
    assert!(items.contains(&("Titanium Widget".into(), 1)), "{items:?}");
    assert_eq!(items.len(), 2, "{items:?}");

    // --- Docs open in a new tab; the answer hides in a closed <details> ----
    let opened = agent.run(&["click", "--text", "Docs"]);
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["new_tabs"], serde_json::json!([1]), "target=_blank tab adopted: {opened}");
    assert!(opened["result"]["url"].as_str().unwrap().ends_with("/docs"), "{opened}");
    let docs = agent.ax(&[]);
    assert!(!docs.contains("37 days"), "collapsed <details> content is not in the a11y tree");
    agent.ok(&["click", &find_ref(&docs, "Returns FAQ")]);
    let docs = agent.ax(&[]);
    assert!(docs.contains("37 days") && docs.contains("8%"), "{docs}");
    assert!(docs.lines().next().unwrap().contains("(tab 1 of 2)"), "{docs}");
    agent.ok(&["tab-close"]);
    assert_eq!(agent.ok(&["url"])["tab"], 0);

    // --- Login: redirect to /login?next=, submit with Enter, 303 back -------
    let state = agent.ok(&["goto", &url("/account")]);
    assert!(state["url"].as_str().unwrap().contains("/login?next=/account"), "{state}");
    agent.ok(&["fill", "--selector", "#email", "demo@acme.test"]);
    agent.ok(&["fill", "--selector", "#password", "hunter2"]);
    let state = agent.ok(&["press", "Enter", "--selector", "#password"]);
    assert!(state["url"].as_str().unwrap().ends_with("/account"), "form POST + 303: {state}");
    agent.ok(&["reload"]);
    agent.ok(&["eval", "--expression", EVENT_LOG_JS]);
    let shot = std::env::temp_dir().join(format!("nv-shot-{}.png", std::process::id()));
    let png = agent.ok(&["screenshot", shot.to_str().unwrap()]);
    assert!(png["bytes"].as_u64().unwrap() > 1000, "{png}");
    let _ = std::fs::remove_file(&shot);
    agent.ok(&["tab-new", &url("/docs")]);
    assert_eq!(agent.ok(&["tab-select", "0"])["url"].as_str().unwrap(), url("/account"));
    agent.ok(&["tab-close", "1"]);
    agent.ok(&["click", "--text", "Next page"]);
    agent.ok(&["wait", "--text", "A-1042"]);
    let orders = agent.ax(&["--selector", "table"]);
    let row = orders.lines().skip_while(|l| !l.contains("A-1042")).take(4).collect::<Vec<_>>().join("\n");
    assert!(row.contains("Awaiting pickup"), "{row}");

    // --- Support form inside an iframe: select, radio, textarea, upload ----
    agent.ok(&["goto", &url("/support")]);
    let support = agent.ax(&[]);
    agent.ok(&["select", &find_ref(&support, "combobox \"Category\""), "Billing"]);
    agent.ok(&["click", &find_ref(&support, "radio \"High\"")]);
    agent.ok(&["fill", &find_ref(&support, "textbox \"Message\""), "Charged twice for order A-1042"]);
    let file = std::env::temp_dir().join(format!("nv-upload-{}.txt", std::process::id()));
    std::fs::write(&file, "receipt").unwrap();
    agent.ok(&["upload", &find_ref(&support, "Attachment"), file.to_str().unwrap()]);
    agent.ok(&["click", &find_ref(&support, "button \"Submit request\"")]);
    let mut done = String::new();
    for _ in 0..20 {
        done = agent.ax(&[]);
        if done.contains("was created") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(done.contains("T-501"), "{done}");
    let st = server_state(&base);
    let ticket = &st["tickets"][0];
    assert_eq!(ticket["category"], "Billing", "{st}");
    assert_eq!(ticket["priority"], "high");
    assert_eq!(ticket["attachment"], file.file_name().unwrap().to_str().unwrap());
    let _ = std::fs::remove_file(&file);

    // --- A page whose load event never fires --------------------------------
    let started = std::time::Instant::now();
    let state = agent.ok(&["goto", &url("/slow"), "--wait", "domcontentloaded"]);
    assert!(started.elapsed().as_secs_f64() < 5.0, "domcontentloaded does not wait for the hanging image");
    assert_eq!(state["title"], "Slow page");
    let slow = agent.run(&["goto", &url("/slow?again"), "--timeout-ms", "1500"]);
    assert_eq!(slow["ok"], false);
    assert!(slow["error"].as_str().unwrap().contains("--wait domcontentloaded"), "the error says what to do: {slow}");

    // --- Dialog policy: dismiss -------------------------------------------------
    agent.ok(&["dialog", "--dismiss"]);
    let answer = agent.run(&["eval", "confirm('Delete everything?')"]);
    assert_eq!(answer["result"], false, "{answer}");
    assert_eq!(answer["dialogs"][0]["accepted"], false, "{answer}");

    agent.ok(&["quit"]);
}
