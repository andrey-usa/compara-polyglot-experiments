//! The report is one self-contained file that any of the five implementations
//! could have produced.

use std::fs;
use std::path::PathBuf;

use compara::contract::{CellDiff, CompareResult};
use compara::engine::compare;
use compara::options::{Engine, Options};
use compara::render;

/// Each test gets its own directory: they run in parallel and each cleans up
/// after itself, so a shared one would be deleted out from under a sibling.
fn sample() -> (CompareResult, PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "compara-report-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    let a = dir.join("a.csv");
    let b = dir.join("b.csv");
    fs::write(&a, "id,name,qty\n1,ann,5\n2,bob,7\n3,cid,9\n").expect("write");
    fs::write(&b, "id,name,qty\n1,ann,5\n2,bob,8\n4,dee,1\n").expect("write");
    let mut opt = Options::with_key(["id"]);
    opt.engine = Engine::Native.label().to_string();
    let result = compare(&a, &b, &mut opt).expect("compare");
    (result, dir)
}

/// Extracts the payload element's mode and body.
fn payload(html: &str) -> (String, String) {
    let open = "<script id=\"payload\" type=\"application/";
    let start = html.find(open).expect("payload element") + open.len();
    let mode_end = start + html[start..].find("\">").expect("payload mode");
    let body_start = mode_end + 2;
    let body_end = body_start + html[body_start..].find("</script>").expect("payload end");
    (
        html[start..mode_end].to_string(),
        html[body_start..body_end].to_string(),
    )
}

/// The report has to work from a file:// URL with no network at all, so nothing
/// in it may point outwards.
#[test]
fn the_report_is_self_contained() {
    let (result, dir) = sample();
    let html = render(&result, true).expect("render");
    for forbidden in ["http://", "https://", "<link", "src=\"//"] {
        assert!(
            !html.contains(forbidden),
            "the report reaches outside itself: {forbidden}"
        );
    }
    for placeholder in ["__TITLE__", "__MODE__", "__PAYLOAD__"] {
        assert!(
            !html.contains(placeholder),
            "{placeholder} was left unsubstituted"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_gzip_payload_is_smaller_than_plain_json() {
    let (result, dir) = sample();
    let compressed = render(&result, true).expect("render");
    let plain = render(&result, false).expect("render");
    let (mode, body) = payload(&compressed);
    assert_eq!(mode, "gzip");
    assert!(!body.is_empty());
    assert!(
        compressed.len() < plain.len(),
        "gzip payload ({}) is no smaller than plain JSON ({})",
        compressed.len(),
        plain.len()
    );
    let _ = fs::remove_dir_all(dir);
}

/// An uncompressed payload sits inside a `<script>`, where markup in a value
/// can end the element early (`</script>`) or, subtler, keep it from ending at
/// all (`<!--<script>`, which leaves the element's own end tag as data and the
/// report's code with it). No `<` may reach the page; JSON reads `\u003c`.
#[test]
fn a_plain_payload_holds_no_markup() {
    let (mut result, dir) = sample();
    for name in ["a</script>x.csv", "a<!--<script>x.csv", "<!--"] {
        result.meta.a.name = name.to_string();
        let html = render(&result, false).expect("render");
        let (mode, body) = payload(&html);
        assert_eq!(mode, "json");
        assert!(!body.contains('<'), "markup in the payload: {name}");
        let decoded: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(decoded["meta"]["a"]["name"], name);
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_title_is_html_escaped() {
    let (mut result, dir) = sample();
    result.meta.a.name = "<img onerror=\"x\">".to_string();
    let html = render(&result, true).expect("render");
    assert!(!html.contains("<img onerror="), "the title was not escaped");
    let _ = fs::remove_dir_all(dir);
}

/// The compact `[column, a, b]` triple every implementation reads.
#[test]
fn a_cell_diff_serialises_as_a_triple() {
    let diff = CellDiff {
        column: 2,
        a: Some("x".into()),
        b: None,
    };
    assert_eq!(
        serde_json::to_string(&diff).expect("json"),
        r#"[2,"x",null]"#
    );
}
