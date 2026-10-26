//! `--allure <dir>`: Allure 2 results, one `<uuid>-result.json` per
//! scenario run (its steps, status, times, labels) and the failure's
//! screenshot as an attachment, for `allure generate` or an Allure server.

use std::path::Path;

use serde_json::{json, Value};

use crate::runner::ScenarioResult;
use crate::suite::Suite;

/// FNV-1a: a hash that stays the same from run to run (Allure's history
/// follows a test by it).
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// A UUID-shaped id, different for every result of every run.
fn uuid(seed: u64, k: usize) -> String {
    let a = fnv(&format!("{seed}-{k}-a"));
    let b = fnv(&format!("{seed}-{k}-b"));
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xfff,
        0x8000 | (b >> 48 & 0x3fff),
        b & 0xffff_ffff_ffff
    )
}

/// One result's JSON, and the screenshot to attach (source path, file name).
fn result(r: &ScenarioResult, feature: Option<&str>, id: &str) -> (Value, Option<(std::path::PathBuf, String)>) {
    let file = r.file.display().to_string().replace('\\', "/");
    let start = r.started as u64;
    let mut at = r.started;
    let steps: Vec<Value> = r
        .steps
        .iter()
        .map(|s| {
            let (from, to) = (at as u64, (at + s.ms) as u64);
            at += s.ms;
            let mut step = json!({
                "name": s.text, "status": if s.ok { "passed" } else { "failed" }, "stage": "finished",
                "start": from, "stop": to,
            });
            if let (false, Some(f)) = (s.ok, &r.failure) {
                step["statusDetails"] = json!({"message": f.message});
            }
            step
        })
        .collect();
    let mut labels = vec![
        json!({"name": "suite", "value": file}),
        json!({"name": "framework", "value": "primavera"}),
        json!({"name": "thread", "value": format!("worker {}", r.worker)}),
    ];
    if let Some(f) = feature {
        labels.push(json!({"name": "feature", "value": f}));
    }
    labels.extend(r.tags.iter().map(|t| json!({"name": "tag", "value": t})));
    let mut out = json!({
        "uuid": id,
        "historyId": format!("{:016x}", fnv(&format!("{file}\n{}", r.name))),
        "testCaseId": format!("{:016x}", fnv(&format!("{file}\n{}", r.base_name()))),
        "fullName": format!("{file}: {}", r.name),
        "name": r.name,
        "status": if r.passed() { "passed" } else { "failed" },
        "stage": "finished",
        "start": start,
        "stop": start + r.ms as u64,
        "labels": labels,
        "steps": steps,
        "attachments": [],
    });
    let mut shot = None;
    match &r.failure {
        Some(f) => {
            out["statusDetails"] = json!({"message": f.message, "trace": format!("line {}: {}", f.line, f.text)});
            if let Some(path) = f.screenshot.as_ref().filter(|p| p.exists()) {
                let name = format!("{id}-attachment.png");
                out["attachments"] =
                    json!([{"name": "screenshot at the failure", "source": name, "type": "image/png"}]);
                shot = Some((path.clone(), name));
            }
        }
        None if r.flaky() => {
            let first = r.retried.first().map(|f| format!("passed on try {}; try 1: {}", r.attempt, f.message));
            out["statusDetails"] = json!({"flaky": true, "message": first.unwrap_or_default()});
        }
        None => {}
    }
    (out, shot)
}

/// Write the results into `dir` (made if missing; what's there stays, so
/// several runs can feed one report). How many were written.
pub fn write(results: &[ScenarioResult], suites: &[Suite], dir: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let seed = (crate::runner::now_ms() * 1000.0) as u64 ^ std::process::id() as u64;
    for (k, r) in results.iter().enumerate() {
        let feature =
            suites.iter().find(|s| s.file == r.file).and_then(|s| s.feature.as_ref()).map(|f| f.name.as_str());
        let id = uuid(seed, k);
        let (json, shot) = result(r, feature, &id);
        if let Some((from, name)) = shot {
            std::fs::copy(from, dir.join(name))?;
        }
        std::fs::write(dir.join(format!("{id}-result.json")), serde_json::to_string_pretty(&json).unwrap_or_default())?;
    }
    Ok(results.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Failure, StepResult};
    use std::path::PathBuf;

    fn case() -> ScenarioResult {
        ScenarioResult {
            file: PathBuf::from("/x/shop.pv"),
            name: "Pays".into(),
            line: 3,
            tags: vec!["smoke".into()],
            ms: 300.0,
            steps: vec![
                StepResult { line: 4, text: "goto /cart".into(), ms: 100.0, ok: true },
                StepResult { line: 5, text: "click --text Pay".into(), ms: 200.0, ok: false },
            ],
            failure: Some(Failure {
                line: 5,
                text: "click --text Pay".into(),
                message: "no element".into(),
                at: None,
                hint: None,
                screenshot: None,
                nearest: vec![],
                source: None,
            }),
            attempt: 1,
            retried: vec![],
            saved: Default::default(),
            notes: vec![],
            worker: 2,
            rep: 0,
            started: 1_700_000_000_000.0,
        }
    }

    #[test]
    fn a_result_has_steps_times_labels_and_a_stable_history_id() {
        let (j, shot) = result(&case(), Some("Checkout"), "id-1");
        assert_eq!(j["status"], "failed");
        assert_eq!((j["start"].as_u64(), j["stop"].as_u64()), (Some(1_700_000_000_000), Some(1_700_000_000_300)));
        assert_eq!(j["steps"][1]["status"], "failed");
        assert_eq!(j["steps"][1]["start"].as_u64(), Some(1_700_000_000_100));
        assert_eq!(j["statusDetails"]["message"], "no element");
        let labels = j["labels"].to_string();
        assert!(labels.contains("\"feature\"") && labels.contains("Checkout") && labels.contains("smoke"), "{labels}");
        assert!(shot.is_none());
        // The same scenario has the same history id next run; another doesn't.
        let (again, _) = result(&case(), None, "id-2");
        assert_eq!(j["historyId"], again["historyId"]);
        let other = ScenarioResult { name: "Refunds".into(), ..case() };
        assert_ne!(j["historyId"], result(&other, None, "id-3").0["historyId"]);
        // A flaky one passed, and says so.
        let flaky = ScenarioResult { failure: None, retried: vec![case().failure.unwrap()], attempt: 2, ..case() };
        let (f, _) = result(&flaky, None, "id-4");
        assert_eq!((f["status"].as_str(), f["statusDetails"]["flaky"].as_bool()), (Some("passed"), Some(true)));
        assert_ne!(uuid(1, 0), uuid(1, 1));
        assert_eq!(uuid(7, 3).len(), 36);
    }
}
