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

#[test]
fn forks_share_a_browser_and_nothing_else() {
    if navigera::browser::resolve_executable_for(None, true).is_none() {
        assert!(std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(), "no Chrome/Chromium found");
        return;
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
                    op(&mut d, &["click", "--selector", "#agree"]);
                    let checked = op(&mut d, &["get", "checked", "--selector", "#agree"]);
                    d.session().close();
                    serde_json::json!({ "me": me, "seen": seen, "cookies": cookies.as_array().map(Vec::len), "checked": checked })
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
    }
    // The owner still works after its forks are gone, and closes the browser.
    let mut owner = owner;
    op(&mut owner, &["goto", &format!("{url}/effects")]);
    owner.session().close();
    let _ = site.kill();
    let _ = site.wait();
}
