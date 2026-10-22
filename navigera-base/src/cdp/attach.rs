//! Attaching to a browser navigera didn't launch, and keeping one open.
//!
//! `--attach <where>` connects to a Chrome that is already running:
//!
//! * `chrome` (also `beta`, `dev`, `canary`, `chromium`, `edge`): the
//!   user's everyday browser. Chrome 144+ serves remote debugging for its
//!   default profile once the user turns it on at
//!   `chrome://inspect/#remote-debugging`; it then writes the endpoint to
//!   `DevToolsActivePort` in its user data directory and asks the user to
//!   Allow each new connection. (Since Chrome 136 the
//!   `--remote-debugging-port` switch is ignored for the default profile.)
//! * a user data directory: any Chrome started with
//!   `--remote-debugging-port` and that `--user-data-dir`;
//! * `ws://…` (the browser endpoint), `http://host:port` or a bare port:
//!   a DevTools HTTP server's `/json/version` names the endpoint.
//!
//! `--profile <name|dir>` is the self-contained version: a persistent,
//! visible Chrome on its own user data directory that navigera starts when
//! it isn't running and reconnects to when it is. Sign in once; cookies,
//! settings and extensions stay. Neither mode ever closes the browser.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const ENABLE_HINT: &str = "turn on remote debugging at chrome://inspect/#remote-debugging \
     (Chrome 144+; Chrome asks you to Allow each connection), or use `--profile <name>` for \
     a separate persistent browser";

/// Default user data directory of a Chrome-family browser channel.
pub fn default_user_data_dir(channel: &str) -> Option<PathBuf> {
    let channel = channel.to_ascii_lowercase();
    if cfg!(windows) {
        let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
        let rel = match channel.as_str() {
            "chrome" | "stable" => "Google\\Chrome\\User Data",
            "beta" => "Google\\Chrome Beta\\User Data",
            "dev" => "Google\\Chrome Dev\\User Data",
            "canary" => "Google\\Chrome SxS\\User Data",
            "chromium" => "Chromium\\User Data",
            "edge" => "Microsoft\\Edge\\User Data",
            _ => return None,
        };
        Some(base.join(rel))
    } else if cfg!(target_os = "macos") {
        let base = PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support");
        let rel = match channel.as_str() {
            "chrome" | "stable" => "Google/Chrome",
            "beta" => "Google/Chrome Beta",
            "dev" => "Google/Chrome Dev",
            "canary" => "Google/Chrome Canary",
            "chromium" => "Chromium",
            "edge" => "Microsoft Edge",
            _ => return None,
        };
        Some(base.join(rel))
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        let rel = match channel.as_str() {
            "chrome" | "stable" => "google-chrome",
            "beta" => "google-chrome-beta",
            "dev" | "canary" => "google-chrome-unstable",
            "chromium" => "chromium",
            "edge" => "microsoft-edge",
            _ => return None,
        };
        Some(base.join(rel))
    }
}

/// `--profile <name>` without a path separator lives in navigera's data
/// directory; with one it is the user data directory itself.
pub fn profile_dir(name: &str) -> Result<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return Ok(PathBuf::from(name));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }
    .context("no home directory for --profile; pass a directory path instead")?;
    Ok(base.join("navigera").join("profiles").join(name))
}

/// `DevToolsActivePort` in a user data directory: the port on the first
/// line, the browser endpoint path on the second.
pub fn read_active_port(dir: &Path) -> Result<(u16, String)> {
    let file = dir.join("DevToolsActivePort");
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("no {} (the browser is not serving remote debugging)", file.display()))?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let port = lines
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .with_context(|| format!("malformed {}", file.display()))?;
    let path = lines.next().unwrap_or("").to_string();
    Ok((port, path))
}

/// True when something accepts TCP connections on 127.0.0.1:`port`.
fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// The live browser endpoint in a user data directory, if its browser runs.
fn live_endpoint(dir: &Path) -> Option<String> {
    let (port, path) = read_active_port(dir).ok()?;
    if path.is_empty() || !port_open(port) {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

/// The browser WebSocket endpoint for an `--attach` value.
pub fn resolve(spec: &str) -> Result<String> {
    let spec = spec.trim();
    if spec.starts_with("ws://") || spec.starts_with("wss://") {
        return Ok(spec.to_string());
    }
    let http = spec
        .strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/').to_string())
        .or_else(|| spec.parse::<u16>().ok().map(|port| format!("127.0.0.1:{port}")));
    if let Some(hostport) = http {
        let (host, port) = hostport
            .rsplit_once(':')
            .and_then(|(h, p)| Some((h.to_string(), p.parse::<u16>().ok()?)))
            .with_context(|| format!("--attach {spec}: expected http://host:port or a port"))?;
        let body = super::transport::http_get_body(&host, port, "/json/version")
            .with_context(|| format!("--attach {spec}: no DevTools HTTP server there"))?;
        let v: serde_json::Value = serde_json::from_str(&body).context("/json/version is not JSON")?;
        let url = v
            .get("webSocketDebuggerUrl")
            .and_then(|u| u.as_str())
            .context("/json/version has no webSocketDebuggerUrl")?;
        return Ok(url.to_string());
    }
    let (dir, what) = match default_user_data_dir(spec) {
        Some(dir) => (dir, format!("{spec}'s default profile")),
        None => (PathBuf::from(spec), format!("user data directory {spec}")),
    };
    let (port, path) =
        read_active_port(&dir).map_err(|e| anyhow::anyhow!("--attach {spec}: {e:#}. For {what}: {ENABLE_HINT}"))?;
    if path.is_empty() {
        bail!("--attach {spec}: {} has no endpoint path", dir.join("DevToolsActivePort").display());
    }
    if !port_open(port) {
        bail!(
            "--attach {spec}: nothing listens on port {port} from {} (the browser closed?). For {what}: {ENABLE_HINT}",
            dir.join("DevToolsActivePort").display()
        );
    }
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

/// `--profile`: the endpoint of the persistent browser on `dir`, starting
/// it first when it isn't running. The browser is detached from navigera
/// (its own process group, out of any job) and outlives every session.
pub fn ensure_profile_browser(exe: &str, dir: &Path, headless: bool) -> Result<String> {
    if let Some(url) = live_endpoint(dir) {
        crate::timing::log(&format!("[attach] profile browser already running: {url}"));
        return Ok(url);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // A file left by a crashed browser would be read as the new endpoint.
    let _ = std::fs::remove_file(dir.join("DevToolsActivePort"));
    let mut cmd = Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", dir.display()))
        .arg("--remote-debugging-port=0")
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    if headless {
        cmd.arg("--headless=new");
    }
    if crate::proc::running_as_root() {
        // Chrome refuses to start its sandbox as root (containers, CI).
        cmd.arg("--no-sandbox");
    }
    // The diagnostic knob that adds flags to every launch (CI uses it for
    // Chrome for Testing builds, which ship no setuid sandbox helper).
    if let Ok(extra) = std::env::var("NAVIGERA_EXTRA_FLAGS") {
        cmd.args(extra.split_whitespace());
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child =
        crate::proc::spawn_detached(&mut cmd).with_context(|| format!("start {exe} for profile {}", dir.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(url) = live_endpoint(dir) {
            crate::timing::log(&format!("[attach] started profile browser {exe}: {url}"));
            return Ok(url);
        }
        if let Ok(Some(status)) = child.try_wait() {
            // A Chrome already running on this profile without remote
            // debugging takes the new window over and the launcher exits.
            bail!(
                "{exe} exited ({status}) without serving remote debugging on {}: if a browser \
                 already has this profile open without navigera, close it and run again",
                dir.display()
            );
        }
        if Instant::now() >= deadline {
            bail!("the profile browser did not start serving remote debugging within 30 s ({})", dir.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_devtools_active_port() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("DevToolsActivePort"), "9222\n/devtools/browser/abc\n").unwrap();
        assert_eq!(read_active_port(dir.path()).unwrap(), (9222, "/devtools/browser/abc".to_string()));
        std::fs::write(dir.path().join("DevToolsActivePort"), "x\n").unwrap();
        assert!(read_active_port(dir.path()).is_err());
    }

    #[test]
    fn channels_map_to_user_data_dirs() {
        let chrome = default_user_data_dir("chrome").expect("chrome has a default dir");
        let s = chrome.display().to_string();
        assert!(s.contains("Chrome") || s.contains("google-chrome"), "{s}");
        assert!(default_user_data_dir("edge").is_some());
        assert!(default_user_data_dir("/some/dir").is_none());
    }

    #[test]
    fn missing_endpoint_explains_how_to_enable_it() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(&dir.path().display().to_string()).unwrap_err().to_string();
        assert!(err.contains("chrome://inspect/#remote-debugging"), "{err}");
        assert!(resolve("ws://127.0.0.1:1/devtools/browser/x").unwrap().starts_with("ws://"));
    }

    #[test]
    fn profile_names_live_in_the_data_dir() {
        let p = profile_dir("work").unwrap().display().to_string();
        assert!(p.contains("navigera") && p.ends_with("work"), "{p}");
        let explicit = if cfg!(windows) { "C:\\x\\y" } else { "/x/y" };
        assert_eq!(profile_dir(explicit).unwrap(), PathBuf::from(explicit));
    }
}
