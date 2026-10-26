//! `find`, `check` and `--find` against the Acme Supply site, with a stand-in
//! for the decision model (`bench/decide/mock_jev.py`, which answers by word
//! overlap). These test the plumbing: what navigera sends, and what it does
//! with the answers. How well the real model chooses is measured elsewhere.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

fn python() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn chrome_available() -> bool {
    let found = navigera::browser::resolve_executable_for(None, true).is_some();
    assert!(
        found || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

/// A child process that prints `LISTENING <url>` first, killed on drop.
struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(script: &str, extra: &[&str]) -> Server {
    let path = format!("{}/{script}", env!("CARGO_MANIFEST_DIR"));
    let mut child = Command::new(python())
        .arg(&path)
        .args(["--port", "0"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start a fixture server");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut line).expect("read LISTENING");
    let url = line.trim().strip_prefix("LISTENING ").expect("LISTENING line").to_string();
    Server { child, url }
}

struct Session {
    name: String,
}

impl Session {
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_navigera"))
            .arg("--session")
            .arg(&self.name)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run navigera");
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("{args:?}: not JSON: {text}"))
    }

    fn ok(&self, args: &[&str]) -> Value {
        let r = self.run(args, &[]);
        assert_eq!(r["ok"], true, "{args:?}: {r}");
        r["result"].clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.run(&["quit"], &[]);
    }
}

#[test]
fn find_check_and_find_targets_ask_the_decision_model() {
    if !chrome_available() {
        return;
    }
    let site = serve("bench/site/server.py", &[]);
    let log = std::env::temp_dir().join(format!("nv-decide-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let jev = serve("bench/decide/mock_jev.py", &["--log", log.to_str().unwrap()]);
    let s = Session { name: format!("nv-decide-{}", std::process::id()) };
    // The session server reads the decider's settings from the `start`
    // environment, as it would from the agent's.
    // The bench site's pages are a few KB, under the default preview size;
    // the session takes a smaller one from its environment.
    let started = s.run(
        &["start"],
        &[("NAVIGERA_DECIDER_URL", &jev.url), ("TYPESAFE_API_KEY", "test-key"), ("NAVIGERA_AX_PREVIEW_BYTES", "1000")],
    );
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["result"]["decider"], "jev-latest", "{started}");
    // The contract is told where the agent decides how to go on, not only
    // in the skill.
    let note = started["notes"][0].as_str().unwrap_or_default();
    assert!(note.contains("--find") && note.contains("check"), "{started}");
    s.ok(&["goto", &format!("{}/shop", site.url)]);
    // A long tree is shown as its top, with the way to act on the rest
    // without reading it, and the way to read it all.
    let read = s.run(&["ax"], &[]);
    let note = read["notes"][0].as_str().unwrap_or_default();
    assert!(note.contains("lines") && note.contains("--find") && note.contains("--limit"), "{note}");
    let preview = read["result"].as_str().unwrap();
    assert!(preview.lines().count() <= 41, "{preview}");
    assert!(preview.ends_with("more lines not shown"), "{preview}");
    // Asked for with a limit, the tree is whole, and still says acting
    // does not need it.
    let whole = s.run(&["ax", "--limit", "2000"], &[]);
    assert!(whole["result"].as_str().unwrap().lines().count() > 60, "{whole}");
    assert!(!whole["result"].as_str().unwrap().contains("more lines not shown"), "{whole}");
    assert!(whole["notes"][0].as_str().unwrap_or_default().contains("--find"), "{whole}");

    // find: one of twelve "Add to cart" buttons, told apart by its card.
    let found = s.ok(&["find", "add the brass valve to the cart"]);
    assert!(found["element"].as_str().unwrap().contains("Brass Valve"), "{found}");
    assert_eq!(found["sure"], true, "{found}");
    let tree = s.ok(&["ax", "--limit", "2000"]);
    let want = format!("[ref={}]", found["ref"]);
    assert!(tree.as_str().unwrap().lines().any(|l| l.contains(&want) && l.contains("Brass Valve")), "{tree}");

    // check: a yes and a no.
    let yes = s.ok(&["check", "Does the page say \"Showing 12 of 60 products\"?"]);
    assert_eq!(yes["answer"], true, "{yes}");
    let no = s.ok(&["check", "Does the page say \"Your order is confirmed\"?"]);
    assert_eq!(no["answer"], false, "{no}");

    // --find on an action: the click lands on the chosen element.
    let clicked = s.ok(&["click", "--find", "add the brass valve to the cart"]);
    assert_eq!(clicked["found"]["ref"], found["ref"], "{clicked}");
    s.ok(&["wait", "--text", "Cart 1"]);

    // What reached the model: the key, the model id, and only element
    // options a Choice may hold.
    let requests: Vec<Value> =
        std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(requests.len() >= 4, "{} requests", requests.len());
    for r in &requests {
        assert_eq!(r["auth"], "Bearer test-key");
        assert_eq!(r["body"]["model"], "jev-latest");
        for q in r["body"]["questions"].as_object().unwrap().values() {
            if q["type"] == "choice" {
                assert!(q["criteria"].as_object().unwrap().len() <= 255);
            }
        }
    }
    let _ = std::fs::remove_file(&log);
}

#[test]
fn without_a_decision_model_find_says_how_to_get_one() {
    if !chrome_available() {
        return;
    }
    let site = serve("bench/site/server.py", &[]);
    let s = Session { name: format!("nv-nodecide-{}", std::process::id()) };
    let started = s.run(&["start"], &[("NAVIGERA_DECIDER_URL", ""), ("TYPESAFE_API_KEY", "")]);
    assert_eq!(started["ok"], true, "{started}");
    assert!(started["result"].get("decider").is_none(), "{started}");
    assert!(started.get("notes").is_none(), "{started}");
    s.ok(&["goto", &format!("{}/shop", site.url)]);
    assert!(s.run(&["ax"], &[]).get("notes").is_none(), "no decision model, no note");
    let r = s.run(&["click", "--find", "the cart link"], &[]);
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("TYPESAFE_API_KEY"), "{r}");
}
