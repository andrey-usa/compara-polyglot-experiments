//! Per-session state keyed by browser-context scope (routes, credentials,
//! dialog answers, downloads): the two cases a review of 113d9c2 found
//! wrong (a subframe's download, `reset --all` and unreported dialogs),
//! failing there and passing since the fix.

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

fn download_count(driver: &mut Driver) -> usize {
    op(driver, &["downloads", "--json"]).as_array().map(Vec::len).unwrap_or(usize::MAX)
}

/// A download started by a fork's tab from a SUBFRAME must be listed for
/// that fork, not for the owner.
///
/// 113d9c2 attributes `Browser.downloadWillBegin` by matching the event's
/// `frameId` against tab target ids (`Shared::register` maps session ->
/// target). A subframe's frame id is not its tab's target id, so the
/// lookup misses and the download falls back to the default ("") scope:
/// the fork's `downloads`/`wait --download` never see it, and it shows up
/// in the owner's list instead. FAILS on 113d9c2.
#[test]
fn subframe_download_is_listed_for_the_session_whose_tab_started_it() {
    let Some((mut site, url)) = site() else { return };
    let mut owner = Driver::launch(&SessionConfig::default()).expect("launch");
    let mut a = owner.fork().expect("fork");
    op(&mut a, &["goto", &format!("{url}/effects")]);
    // Start the download from a subframe, not the tab's main frame.
    let js = format!(
        "((u) => {{ const f = document.createElement('iframe'); f.src = u; document.body.appendChild(f); return 1; }})('{url}/file.csv')"
    );
    assert_eq!(op(&mut a, &["eval", &js]), 1);
    // Poll until the download lands in somebody's list (or time out).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let (mut na, mut no) = (0, 0);
    while std::time::Instant::now() < deadline {
        na = download_count(&mut a);
        no = download_count(&mut owner);
        if na + no > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(na + no > 0, "the subframe download was never recorded anywhere");
    assert_eq!(na, 1, "fork's tab started the download from its subframe (owner saw {no})");
    assert_eq!(no, 0, "owner must not list the fork's subframe download");
    a.session().close();
    owner.session().close();
    let _ = site.kill();
    let _ = site.wait();
}

/// Sanity: the same download from the tab's MAIN frame is attributed
/// correctly (this is the case 113d9c2's own test covers). PASSES on
/// 113d9c2; guards the probe above against fixture/attribution drift.
#[test]
fn mainframe_download_stays_with_its_session() {
    let Some((mut site, url)) = site() else { return };
    let mut owner = Driver::launch(&SessionConfig::default()).expect("launch");
    let mut a = owner.fork().expect("fork");
    op(&mut a, &["goto", &format!("{url}/effects")]);
    op(&mut a, &["eval", "(location.href = '/file.csv', 1)"]);
    op(&mut a, &["wait", "--download", "report.csv"]);
    assert_eq!(download_count(&mut a), 1);
    assert_eq!(download_count(&mut owner), 0);
    a.session().close();
    owner.session().close();
    let _ = site.kill();
    let _ = site.wait();
}

/// Run one op and keep the full outcome (result plus reported dialogs).
fn outcome(driver: &mut Driver, words: &[&str]) -> navigera::protocol::Outcome {
    let parsed = parse_args(std::iter::once("navigera").chain(words.iter().copied()).map(String::from)).unwrap();
    driver.run_op(&parsed.command.expect("a command")).unwrap_or_else(|e| panic!("{words:?}: {e:#}"))
}

/// `reset --all` must forget dialogs answered before the reset that no op
/// has reported yet: a new session would have none.
///
/// 113d9c2's `move_scope` renames pending dialog entries from the old scope
/// to the new one even when `keep` is false. Routes, credentials, the dialog
/// answer and the download list are all dropped for `reset --all`; unreported
/// dialogs are moved instead, so the reset op itself reports a dialog from
/// before the reset. FAILS on 113d9c2 (the reset outcome carries the stale
/// dialog).
#[test]
fn reset_all_forgets_unreported_dialogs() {
    let Some((mut site, url)) = site() else { return };
    let mut a = Driver::launch(&SessionConfig::default()).expect("launch");
    op(&mut a, &["goto", &format!("{url}/effects")]);
    // Fire an alert from a timer: it is answered between ops, so no op has
    // reported it yet when the reset runs.
    op(&mut a, &["eval", "setTimeout(() => alert('stale'), 300)"]);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let reset = outcome(&mut a, &["reset", "--all"]);
    assert!(
        reset.dialogs.is_empty(),
        "reset --all must not report a dialog answered before the reset: {:?}",
        reset.dialogs
    );
    a.session().close();
    let _ = site.kill();
    let _ = site.wait();
}
