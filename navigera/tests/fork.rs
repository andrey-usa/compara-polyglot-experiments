//! Several drivers on one browser (`Driver::fork`): each in a browser
//! context of its own, usable from its own thread.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use navigera::protocol::{parse_args, Driver, SessionConfig};
use serde_json::Value;

fn op(driver: &mut Driver, words: &[&str]) -> Value {
    let parsed = parse_args(std::iter::once("navigera").chain(words.iter().copied()).map(String::from)).unwrap();
    driver.run(&parsed.command.expect("a command")).unwrap_or_else(|e| panic!("{words:?}: {e:#}"))
}

/// The fixture site and its URL, or None without a browser to test with.
fn site() -> Option<(std::process::Child, String)> {
    if navigera::browser::resolve_executable_for(None, true).is_none() {
        assert!(std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(), "no Chrome/Chromium found");
        return None;
    }
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut site = Command::new(python)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/interactions_site.py"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("fixture site");
    let mut line = String::new();
    BufReader::new(site.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("LISTENING ").unwrap().to_string();
    Some((site, url))
}

#[test]
fn forks_share_a_browser_and_nothing_else() {
    let Some((mut site, url)) = site() else { return };

    let owner = Driver::launch(&SessionConfig::default()).expect("launch");
    let workers: Vec<Driver> = (0..3).map(|_| owner.fork().expect("fork")).collect();
    let results: Vec<Value> = std::thread::scope(|s| {
        let handles: Vec<_> = workers
            .into_iter()
            .enumerate()
            .map(|(i, mut d)| {
                let url = url.clone();
                s.spawn(move || {
                    op(&mut d, &["goto", &format!("{url}/effects")]);
                    let me = format!("worker-{i}");
                    op(&mut d, &["eval", &format!("(localStorage.setItem('who', '{me}'), document.cookie = 'who={me}; path=/', 1)")]);
                    // Every worker sees only what it set itself.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let seen = op(&mut d, &["eval", "[localStorage.getItem('who'), document.cookie]"]);
                    let cookies = op(&mut d, &["cookies", "--all", "--json"]);
                    // A tab it opens is in its own context too.
                    op(&mut d, &["tab-new", &format!("{url}/effects")]);
                    let in_new_tab = op(&mut d, &["eval", "document.cookie"]);
                    op(&mut d, &["tab-close"]);
                    op(&mut d, &["click", "--selector", "#agree"]);
                    let checked = op(&mut d, &["get", "checked", "--selector", "#agree"]);
                    d.session().close();
                    serde_json::json!({ "me": me, "seen": seen, "cookies": cookies.as_array().map(Vec::len), "checked": checked, "new_tab": in_new_tab })
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in &results {
        let me = r["me"].as_str().unwrap();
        assert_eq!(r["seen"], serde_json::json!([me, format!("who={me}")]), "{r}");
        assert_eq!(r["cookies"], 1, "{r}");
        assert_eq!(r["checked"], true, "{r}");
        assert_eq!(r["new_tab"], format!("who={me}"), "{r}");
    }
    // The owner still works after its forks are gone, and closes the browser.
    let mut owner = owner;
    op(&mut owner, &["goto", &format!("{url}/effects")]);
    owner.session().close();
    let _ = site.kill();
    let _ = site.wait();
}

#[test]
fn routes_and_dialog_answers_are_each_sessions_own() {
    let Some((mut site, url)) = site() else { return };
    let fetch = "fetch('/event/x').then((r) => r.text())";
    let owner = Driver::launch(&SessionConfig::default()).expect("launch");
    let (mut a, mut b) = (owner.fork().expect("fork"), owner.fork().expect("fork"));
    for d in [&mut a, &mut b] {
        op(d, &["goto", &format!("{url}/effects")]);
    }
    op(&mut a, &["route", "*/event/x", "--body", "mocked"]);
    op(&mut a, &["dialog", "--dismiss"]);
    // B has a route of its own: its tabs are intercepted too, by its rules.
    op(&mut b, &["route", "*/nothing-here", "--abort"]);
    assert_eq!(op(&mut a, &["eval", fetch]), "mocked");
    assert_eq!(op(&mut b, &["eval", fetch]), "ok");
    assert_eq!(op(&mut a, &["eval", "confirm('sure?')"]), false);
    assert_eq!(op(&mut b, &["eval", "confirm('sure?')"]), true);
    // A download is listed for the session whose tab started it.
    op(&mut a, &["eval", "(location.href = '/file.csv', 1)"]);
    op(&mut a, &["wait", "--download", "report.csv"]);
    assert_eq!(op(&mut a, &["downloads", "--json"]).as_array().map(Vec::len), Some(1));
    assert_eq!(op(&mut b, &["downloads", "--json"]).as_array().map(Vec::len), Some(0));
    // B's `unroute` takes only its own rules away.
    op(&mut b, &["unroute"]);
    assert_eq!(op(&mut a, &["eval", fetch]), "mocked");
    // `reset` keeps a session's routes and answer; `reset --all` doesn't.
    op(&mut a, &["reset"]);
    op(&mut a, &["goto", &format!("{url}/effects")]);
    assert_eq!(op(&mut a, &["eval", fetch]), "mocked");
    assert_eq!(op(&mut a, &["eval", "confirm('sure?')"]), false);
    op(&mut a, &["reset", "--all"]);
    op(&mut a, &["goto", &format!("{url}/effects")]);
    assert_eq!(op(&mut a, &["eval", fetch]), "ok");
    assert_eq!(op(&mut a, &["eval", "confirm('sure?')"]), true);
    assert_eq!(op(&mut a, &["route"]), "(no routes)");
    assert_eq!(op(&mut a, &["downloads", "--json"]).as_array().map(Vec::len), Some(0));
    a.session().close();
    b.session().close();
    owner.session().close();
    let _ = site.kill();
    let _ = site.wait();
}

#[test]
fn another_session_attaches_to_one_tab_by_its_endpoint() {
    let Some((mut site, url)) = site() else { return };
    let config = SessionConfig { transport: Some("ws".into()), ..SessionConfig::default() };
    let mut first = Driver::launch(&config).expect("launch");
    op(&mut first, &["goto", &format!("{url}/effects")]);
    op(&mut first, &["eval", "(document.title = 'left here', 1)"]);
    let tab = first.session().page_endpoint().expect("a page endpoint over ws");
    assert!(tab.contains("/devtools/page/"), "{tab}");
    // The second session works in that very tab, not a window of its own.
    let mut second = Driver::launch(&SessionConfig { attach: Some(tab), ..SessionConfig::default() }).expect("attach");
    let here = op(&mut second, &["url"]);
    assert_eq!(here["title"], "left here", "{here}");
    second.session().close();
    // Leaving changed nothing for the first.
    assert_eq!(op(&mut first, &["eval", "document.title"]), "left here");
    first.session().close();
    let _ = site.kill();
    let _ = site.wait();
}
