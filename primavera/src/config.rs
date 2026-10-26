//! `primavera.toml`: what a run takes by default, the app `[server]` it
//! starts, and `[env.<name>]` profiles (`--env staging`). Flags on the
//! command line win over the file; a profile wins over the file's top.
//!
//! ```toml
//! base_url = "http://127.0.0.1:3000"
//! paths = ["tests"]
//! workers = 4
//! [vars]
//! region = "us"
//! [server]
//! command = "npm run dev"
//! url = "http://127.0.0.1:3000/health"
//! [env.staging]
//! base_url = "https://staging.example.com"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Workers {
    N(u64),
    /// `"auto"`.
    Word(String),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub base_url: Option<String>,
    /// What to run when the command line names nothing.
    pub paths: Option<Vec<String>>,
    pub workers: Option<Workers>,
    pub retries: Option<u32>,
    pub fail_on_flaky: Option<bool>,
    pub timeout_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub isolation: Option<String>,
    pub reporters: Option<Vec<String>>,
    pub junit: Option<String>,
    pub html: Option<String>,
    pub cucumber: Option<String>,
    pub allure: Option<String>,
    pub secrets: Option<String>,
    pub chromium: Option<String>,
    /// `chrome` (the default) or `lightpanda`.
    pub engine: Option<String>,
    pub evidence: Option<String>,
    pub headed: Option<bool>,
    /// `on-failure`, `all` or `off`.
    pub trace: Option<String>,
    /// compara's reports for `expect download … compare`: html, json, rows.
    pub compare_report: Option<Vec<String>>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    pub server: Option<Server>,
}

/// The app under test: started before the browser, ready when `url`
/// answers, stopped (with everything it started) after the run.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// A shell command line (`sh -c`, or `cmd /C` on Windows).
    pub command: String,
    pub url: String,
    #[serde(default = "sixty")]
    pub timeout_s: u64,
    /// Where it runs, from the config file's directory.
    pub cwd: Option<String>,
    /// Use a server already answering at `url` instead of starting one
    /// (default: yes, except under CI).
    pub reuse: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn sixty() -> u64 {
    60
}

impl Profile {
    /// `over` on top of this one: its settings where it has them, its vars
    /// added.
    fn merged(mut self, over: Profile) -> Profile {
        macro_rules! take {
            ($($f:ident),*) => { $( if over.$f.is_some() { self.$f = over.$f; } )* };
        }
        take!(
            base_url,
            paths,
            workers,
            retries,
            fail_on_flaky,
            timeout_ms,
            wait_ms,
            isolation,
            reporters,
            junit,
            html,
            cucumber,
            allure,
            secrets,
            chromium,
            engine,
            evidence,
            headed,
            trace,
            compare_report,
            server
        );
        self.vars.extend(over.vars);
        self
    }

    /// The command-line flags it stands for; the paths are kept apart.
    pub fn words(&self) -> Vec<String> {
        let mut w = Vec::new();
        let mut flag = |f: &str, v: Option<String>| {
            if let Some(v) = v {
                w.extend([f.to_string(), v]);
            }
        };
        flag("--base-url", self.base_url.clone());
        flag(
            "--workers",
            self.workers.as_ref().map(|x| match x {
                Workers::N(n) => n.to_string(),
                Workers::Word(s) => s.clone(),
            }),
        );
        flag("--retries", self.retries.map(|n| n.to_string()));
        flag("--timeout-ms", self.timeout_ms.map(|n| n.to_string()));
        flag("--wait-ms", self.wait_ms.map(|n| n.to_string()));
        flag("--isolation", self.isolation.clone());
        flag("--junit", self.junit.clone());
        flag("--html", self.html.clone());
        flag("--cucumber", self.cucumber.clone());
        flag("--allure", self.allure.clone());
        flag("--secrets", self.secrets.clone());
        flag("--chromium", self.chromium.clone());
        flag("--engine", self.engine.clone());
        flag("--evidence", self.evidence.clone());
        flag("--trace", self.trace.clone());
        flag(
            "--compare-report",
            self.compare_report.as_ref().map(|k| if k.is_empty() { "none".into() } else { k.join(",") }),
        );
        for r in self.reporters.iter().flatten() {
            flag("--reporter", Some(r.clone()));
        }
        for (k, v) in &self.vars {
            flag("--var", Some(format!("{k}={v}")));
        }
        if self.fail_on_flaky == Some(true) {
            w.push("--fail-on-flaky".into());
        }
        if self.headed == Some(true) {
            w.push("--headed".into());
        }
        w
    }
}

pub struct Config {
    pub profile: Profile,
}

/// The config: `explicit` (`--config`), else `primavera.toml` in the
/// current directory when there is one; with `env`, that profile on top.
pub fn load(explicit: Option<&Path>, env: Option<&str>) -> Result<Option<Config>, String> {
    let file = match explicit {
        Some(p) => p.to_path_buf(),
        None if Path::new("primavera.toml").is_file() => PathBuf::from("primavera.toml"),
        None if env.is_some() => return Err("--env names a profile of primavera.toml, and there is none here".into()),
        None => return Ok(None),
    };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let profile = parse(&text, env).map_err(|e| format!("{}: {e}", file.display()))?;
    let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    Ok(Some(Config { profile: anchored(profile, dir) }))
}

fn parse(text: &str, env: Option<&str>) -> Result<Profile, String> {
    let mut table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.message().to_string())?;
    let envs = match table.remove("env") {
        Some(toml::Value::Table(t)) => t,
        Some(_) => return Err("`env` is a table of profiles: [env.<name>]".into()),
        None => toml::Table::new(),
    };
    let top: Profile = toml::Value::Table(table).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
    let Some(name) = env else { return Ok(top) };
    let over = envs.get(name).cloned().ok_or_else(|| {
        let names: Vec<&str> = envs.keys().map(String::as_str).collect();
        format!(
            "no profile [env.{name}]{}",
            if names.is_empty() { String::new() } else { format!(" (there are: {})", names.join(", ")) }
        )
    })?;
    let over: Profile = over.try_into().map_err(|e: toml::de::Error| format!("[env.{name}]: {}", e.message()))?;
    Ok(top.merged(over))
}

/// Paths in the file are from its directory.
fn anchored(mut p: Profile, dir: &Path) -> Profile {
    if dir == Path::new(".") {
        return p;
    }
    let at = |s: &mut String| {
        if Path::new(s.as_str()).is_relative() {
            *s = dir.join(s.as_str()).to_string_lossy().into_owned();
        }
    };
    for f in [&mut p.junit, &mut p.html, &mut p.cucumber, &mut p.allure, &mut p.secrets, &mut p.evidence]
        .into_iter()
        .flatten()
    {
        at(f);
    }
    p.paths.iter_mut().flatten().for_each(at);
    if let Some(s) = p.server.as_mut() {
        let mut cwd = s.cwd.clone().unwrap_or_else(|| ".".into());
        at(&mut cwd);
        s.cwd = Some(cwd);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"
base_url = "http://127.0.0.1:3000"
paths = ["tests"]
workers = "auto"
reporters = ["github"]
[vars]
region = "us"
plan = "free"
[server]
command = "npm run dev"
url = "http://127.0.0.1:3000/health"
[env.staging]
base_url = "https://staging.example.com"
retries = 2
[env.staging.vars]
region = "eu"
"#;

    #[test]
    fn profiles_go_on_top_and_become_flags() {
        let top = parse(TEXT, None).unwrap();
        assert_eq!(top.server.as_ref().unwrap().timeout_s, 60);
        assert_eq!(
            top.words().join(" "),
            "--base-url http://127.0.0.1:3000 --workers auto --reporter github --var plan=free --var region=us"
        );
        let staging = parse(TEXT, Some("staging")).unwrap();
        assert_eq!(staging.base_url.as_deref(), Some("https://staging.example.com"));
        assert_eq!(
            (staging.retries, staging.vars["region"].as_str(), staging.vars["plan"].as_str()),
            (Some(2), "eu", "free")
        );
        assert!(staging.server.is_some(), "a profile keeps the top's server");
        let e = parse(TEXT, Some("prod")).unwrap_err();
        assert!(e.contains("no profile [env.prod] (there are: staging)"), "{e}");
        assert!(parse("wokers = 2", None).unwrap_err().contains("unknown field"));
        let c = parse("compare_report = [\"json\", \"rows\"]", None).unwrap();
        assert_eq!(c.words().join(" "), "--compare-report json,rows");
        assert_eq!(parse("compare_report = []", None).unwrap().words().join(" "), "--compare-report none");
    }

    #[test]
    fn paths_are_from_the_files_directory() {
        let p = anchored(parse(TEXT, None).unwrap(), Path::new("ci"));
        assert_eq!(p.paths.unwrap(), [Path::new("ci").join("tests").to_string_lossy()]);
        assert_eq!(p.server.unwrap().cwd.unwrap(), Path::new("ci").join(".").to_string_lossy());
    }
}
