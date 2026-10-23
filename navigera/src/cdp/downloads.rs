//! Files the page downloads: the browser saves each one under its download
//! id in a directory navigera chose (`Browser.setDownloadBehavior`
//! `allowAndName`), and reports it starting and finishing; a finished file
//! is renamed to the name the page suggested.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct Download {
    pub n: u64,
    pub guid: String,
    pub url: String,
    /// The file name the page suggested (`Content-Disposition`, `<a download>`).
    pub suggested: String,
    /// `inProgress`, `completed` or `canceled`.
    pub state: String,
    pub received: u64,
    pub total: u64,
    /// Where it was saved, once complete (none when the browser keeps it,
    /// as with `--attach`).
    pub path: Option<PathBuf>,
}

impl Download {
    pub fn to_json(&self) -> Value {
        json!({
            "n": self.n,
            "file": self.suggested,
            "path": self.path.as_ref().map(|p| p.display().to_string()),
            "bytes": if self.state == "completed" { self.received.max(self.total) } else { self.received },
            "state": match self.state.as_str() { "inProgress" => "in progress", other => other },
            "url": self.url,
        })
    }
}

#[derive(Default)]
pub struct Downloads {
    /// Where files go; `None` when the browser's own setting is kept.
    pub dir: Option<PathBuf>,
    pub items: Vec<Download>,
    next: u64,
}

impl Downloads {
    pub fn on_event(&mut self, method: &str, params: &Value) {
        let guid = params.get("guid").and_then(Value::as_str).unwrap_or("").to_string();
        match method {
            "Browser.downloadWillBegin" => {
                self.next += 1;
                let suggested = params.get("suggestedFilename").and_then(Value::as_str).unwrap_or("download");
                self.items.push(Download {
                    n: self.next,
                    guid,
                    url: params.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                    suggested: safe_name(suggested),
                    state: "inProgress".to_string(),
                    received: 0,
                    total: 0,
                    path: None,
                });
                if self.items.len() > 1000 {
                    self.items.remove(0);
                }
            }
            "Browser.downloadProgress" => {
                let dir = self.dir.clone();
                let taken: Vec<PathBuf> = self.items.iter().filter_map(|d| d.path.clone()).collect();
                let Some(d) = self.items.iter_mut().find(|d| d.guid == guid) else {
                    return;
                };
                d.received = params.get("receivedBytes").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                d.total = params.get("totalBytes").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                let state = params.get("state").and_then(Value::as_str).unwrap_or("inProgress");
                if state == "completed" && d.state != "completed" {
                    if let Some(dir) = dir {
                        d.path = Some(settle(&dir, &d.guid, &d.suggested, &taken));
                    }
                }
                d.state = state.to_string();
            }
            _ => {}
        }
    }
}

/// Rename `<dir>/<guid>` to the suggested name (`name (2).ext` if taken).
/// Left under its id when the rename fails.
fn settle(dir: &Path, guid: &str, suggested: &str, taken: &[PathBuf]) -> PathBuf {
    let from = dir.join(guid);
    let (stem, ext) = match suggested.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (suggested.to_string(), String::new()),
    };
    for i in 1..1000 {
        let name = if i == 1 { suggested.to_string() } else { format!("{stem} ({i}){ext}") };
        let to = dir.join(&name);
        if to.exists() || taken.contains(&to) {
            continue;
        }
        return match std::fs::rename(&from, &to) {
            Ok(()) => to,
            Err(_) => from,
        };
    }
    from
}

/// A file name the page can't use to leave the directory.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        "download".to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside_the_directory() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("a\\b\\report.csv"), "report.csv");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("q?.txt"), "q_.txt");
    }

    #[test]
    fn a_finished_download_gets_its_name_and_a_second_one_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = Downloads { dir: Some(dir.path().to_path_buf()), ..Default::default() };
        for guid in ["g1", "g2"] {
            std::fs::write(dir.path().join(guid), guid).unwrap();
            d.on_event(
                "Browser.downloadWillBegin",
                &json!({"guid": guid, "url": "http://x/r.csv", "suggestedFilename": "r.csv"}),
            );
            d.on_event(
                "Browser.downloadProgress",
                &json!({"guid": guid, "receivedBytes": 2, "totalBytes": 2, "state": "completed"}),
            );
        }
        let names: Vec<String> = d
            .items
            .iter()
            .map(|i| i.path.as_ref().unwrap().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["r.csv", "r (2).csv"]);
        assert_eq!(std::fs::read_to_string(dir.path().join("r (2).csv")).unwrap(), "g2");
        assert_eq!(d.items[0].to_json()["state"], "completed");
    }
}
