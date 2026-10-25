//! Browser sessions on the from-scratch CDP engine (`crate::cdp`).
//!
//! `BrowserSession` is the unit the CLI drives: one browser, a tab list with
//! an active tab, navigation with retry, and the script/text helpers the
//! agent needs. The wire protocol in `navigera.rs` is unchanged.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::cdp::{self, AxOptions, Browser, DialogPolicy, LaunchOptions, Page, Target, DEFAULT_ELEMENT_WAIT};

/// Tokio runtime for the CDP engine: 2 workers is plenty for a sequential
/// CLI, and keeps RSS/CPU far below a default multi-thread runtime.
fn engine_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("cdp-engine")
        .build()
        .context("build CDP engine runtime")
}

/// Chromium launch flags.
///
/// Beyond the basics, this is the standard automation set that go-rod,
/// chromiumoxide, Puppeteer and Playwright all ship by default: no
/// background networking, component updates, crash reporter, sync, metrics
/// or Translate competing with the page for CPU, and no throttling of
/// background tabs. `--no-startup-window` skips Chrome's initial tab (the
/// session opens its own), saving a renderer process per launch. Site
/// isolation is relaxed as go-rod does, so cross-site iframes share a
/// renderer instead of each spawning one; the throwaway profile holds no
/// one's cookies. Chrome honours only the last `--disable-features` /
/// `--enable-features` flag, so each list is a single flag.
///
/// The renderer sandbox stays on: an agent opens pages nobody vetted, and the
/// sandbox is what contains a renderer exploit. `--no-sandbox` is added only
/// where Chrome cannot have one (see [`chrome_flags`]).
const CHROME_FLAGS: &[&str] = &[
    "--disable-dev-shm-usage",
    "--disable-blink-features=AutomationControlled",
    "--no-first-run",
    "--no-default-browser-check",
    "--no-startup-window",
    "--disable-default-apps",
    "--disable-infobars",
    "--window-size=1440,900",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--disable-extensions",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-sync",
    "--metrics-recording-only",
    "--password-store=basic",
    "--use-mock-keychain",
    "--force-color-profile=srgb",
    "--disable-site-isolation-trials",
    // Not `OptimizationHints`: disabling it makes Chrome 151 segfault on the
    // first page (bisected in chrome-bisect run 37464048004; every other
    // feature here is fine there). Its hint fetches are network work that
    // `--disable-background-networking` already stops.
    "--disable-features=Translate,TranslateUI,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,CertificateTransparencyComponentUpdater,InterestFeedContentSuggestions,site-per-process",
    "--enable-features=NetworkService,NetworkServiceInProcess",
];

/// [`CHROME_FLAGS`] adjusted by two diagnostic knobs, for bisecting a
/// browser-version-specific failure in CI without a rebuild:
/// `NAVIGERA_DROP_FLAGS` (comma-separated prefixes to remove) and
/// `NAVIGERA_EXTRA_FLAGS` (space-separated flags to add).
fn chrome_flags() -> Vec<String> {
    let drop: Vec<String> = std::env::var("NAVIGERA_DROP_FLAGS")
        .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let mut flags: Vec<String> = CHROME_FLAGS
        .iter()
        .filter(|f| !drop.iter().any(|d| f.starts_with(d.as_str())))
        .map(|f| f.to_string())
        .collect();
    if no_sandbox_wanted(crate::proc::running_as_root(), std::env::var("NAVIGERA_NO_SANDBOX").ok().as_deref()) {
        flags.push(NO_SANDBOX.into());
    }
    if let Ok(extra) = std::env::var("NAVIGERA_EXTRA_FLAGS") {
        flags.extend(extra.split_whitespace().map(str::to_string));
    }
    flags
}

const NO_SANDBOX: &str = "--no-sandbox";

/// `--natural`: the browser looks like the browser it is, not like an
/// automated one. Set once per process, before the browser starts.
static NATURAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn natural mode on for the browsers this process starts.
pub fn set_natural(on: bool) {
    NATURAL.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether browsers started here run in natural mode (`--natural`, or
/// `NAVIGERA_NATURAL=1`).
pub fn natural() -> bool {
    NATURAL.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("NAVIGERA_NATURAL").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

/// How long element ops wait for their element when the op names no
/// timeout of its own: `--wait-ms` (on `start`, `serve`, `mcp` or a single
/// command), else `NAVIGERA_WAIT_MS`, else [`DEFAULT_ELEMENT_WAIT`]. A test
/// suite against a slow app sets it once instead of on every step.
static ELEMENT_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set the element wait for this process (`None` leaves it as it is).
pub fn set_element_wait(ms: Option<f64>) {
    if let Some(ms) = ms.filter(|ms| ms.is_finite() && *ms > 0.0) {
        ELEMENT_WAIT_MS.store(ms as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The element wait in force (see [`set_element_wait`]).
pub fn element_wait() -> Duration {
    let ms = ELEMENT_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if ms > 0 {
        return Duration::from_millis(ms);
    }
    std::env::var("NAVIGERA_WAIT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(DEFAULT_ELEMENT_WAIT)
}

/// Where downloads are saved: `--download-dir`, else
/// `NAVIGERA_DOWNLOAD_DIR`, else a directory of this process's own under
/// the system temp directory.
static DOWNLOAD_DIR: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Set the download directory for the browsers this process starts.
pub fn set_download_dir(dir: Option<&str>) {
    if let Some(dir) = dir.filter(|d| !d.trim().is_empty()) {
        *DOWNLOAD_DIR.lock().unwrap() = Some(std::path::PathBuf::from(dir));
    }
}

/// The download directory in force, created if missing (absolute).
pub fn download_dir() -> Result<std::path::PathBuf> {
    let dir = DOWNLOAD_DIR
        .lock()
        .unwrap()
        .clone()
        .or_else(|| std::env::var_os("NAVIGERA_DOWNLOAD_DIR").filter(|v| !v.is_empty()).map(Into::into))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("navigera-downloads-{}", std::process::id())));
    std::fs::create_dir_all(&dir).with_context(|| format!("download directory {}", dir.display()))?;
    // Not `canonicalize`: on Windows it gives a `\\?\C:\…` path, and Chrome
    // cancels every download into one of those.
    Ok(std::path::absolute(&dir).unwrap_or(dir))
}

/// What a headless Chrome shows a page that a windowed one does not, put
/// right for natural mode: the user agent says `HeadlessChrome` (in
/// `navigator.userAgent`, in workers, in every request's header), and the
/// screen is 800x600 behind a larger window. The user agent becomes the one
/// the same Chrome sends with a window -- its major version, the reduced form
/// Chrome sends -- and the screen 1920x1080. Navigator's `webdriver` flag
/// is off in every mode (`--disable-blink-features=AutomationControlled`),
/// and nothing here passes `--enable-automation`. A windowed browser needs
/// none of this. chrome-headless-shell is refused: it is a different
/// program, and its client hints, plugins and `window.chrome` say headless
/// whatever the flags.
pub fn natural_flags(exe: &str, headless: bool) -> Result<Vec<String>> {
    if !headless {
        return Ok(Vec::new());
    }
    let stem = Path::new(exe).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if matches!(stem.as_str(), "chrome-headless-shell" | "headless_shell") {
        bail!(
            "--natural needs the full Chrome: {exe} is chrome-headless-shell, which tells a page it is headless \
             whatever navigera does; point --chromium or $CHROME_BIN at Chrome"
        );
    }
    let mut flags = vec!["--screen-info={1920x1080}".to_string()];
    match chrome_major(exe) {
        Some(major) => flags.push(format!("--user-agent={}", windowed_user_agent(exe, major))),
        None => crate::timing::log(&format!("[browser] natural: no version from {exe}; user agent left as is")),
    }
    Ok(flags)
}

/// The user agent Chrome `major` sends with a window on this platform.
fn windowed_user_agent(exe: &str, major: u32) -> String {
    let platform = if cfg!(windows) {
        "Windows NT 10.0; Win64; x64"
    } else if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else {
        "X11; Linux x86_64"
    };
    let file = Path::new(exe).file_name().map(|f| f.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let edge =
        if file.contains("msedge") || file.contains("edge") { format!(" Edg/{major}.0.0.0") } else { String::new() };
    format!("Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36{edge}")
}

/// The major version of a Chrome-family executable: from the version
/// directory beside it on Windows (where `--version` opens a window instead
/// of printing), else from `--version`, which costs a launch about 30 ms, so
/// the answer is noted in the session directory against the binary's path,
/// size and modification time (an update asks again).
pub fn chrome_major(exe: &str) -> Option<u32> {
    let parse = |text: &str| -> Option<u32> {
        text.split(|c: char| c.is_whitespace() || c == '/')
            .find(|w| {
                w.split('.').count() == 4
                    && w.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            })
            .and_then(|w| w.split('.').next()?.parse().ok())
    };
    if cfg!(windows) {
        let dir = Path::new(exe).parent()?;
        return std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                // `141.0.7390.37\` in an install, `141.0.7390.37.manifest`
                // in a Chrome for Testing archive.
                let name = e.file_name().to_string_lossy().into_owned();
                parse(name.strip_suffix(".manifest").unwrap_or(&name))
            })
            .max();
    }
    let note = exe_note(exe, "chrome-version", false);
    if let Some(major) = note.as_ref().and_then(|n| std::fs::read_to_string(n).ok()).and_then(|t| t.trim().parse().ok())
    {
        return Some(major);
    }
    let out = std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let major = parse(&String::from_utf8_lossy(&out.stdout))?;
    if let Some(note) = note {
        let _ = std::fs::write(note, major.to_string());
    }
    Some(major)
}

/// Chrome refuses to start its sandbox as root (containers, CI images);
/// `NAVIGERA_NO_SANDBOX=1` turns it off explicitly.
pub(crate) fn no_sandbox_wanted(root: bool, env: Option<&str>) -> bool {
    root || env.is_some_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

pub(crate) const NO_SANDBOX_WARNING: &str =
    "navigera: this machine can't give Chrome its sandbox, so it runs without one \
     (pages are not isolated from your account). Fix: enable unprivileged user \
     namespaces or Chrome's setuid helper; NAVIGERA_NO_SANDBOX=1 silences this.";

/// Where navigera notes that `exe` could not have a sandbox on this machine.
///
/// Without the note every launch pays for one that fails first (about
/// 350 ms on a GitHub runner with Chrome for Testing). It sits in the
/// private session directory and is keyed by the binary's path, size and
/// modification time and by the boot, so a Chrome update or a reboot (after
/// which the machine may allow a sandbox) brings the attempt back.
pub(crate) fn no_sandbox_note(exe: &str) -> Option<std::path::PathBuf> {
    exe_note(exe, "no-sandbox", true)
}

/// A note about `exe` in the private session directory, named `what` and
/// keyed by the binary's path, size and modification time (and the boot,
/// when `per_boot`).
fn exe_note(exe: &str, what: &str, per_boot: bool) -> Option<std::path::PathBuf> {
    let meta = std::fs::metadata(exe).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let path = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.into());
    let boot = if per_boot {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default()
    } else {
        String::new()
    };
    let key = format!("{}\n{}\n{modified}\n{}", path.display(), meta.len(), boot.trim());
    // FNV-1a: the same across builds, which std's hasher does not promise.
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    Some(crate::session::session_dir().ok()?.join(format!("{what}-{hash:016x}")))
}

/// Whether a launch failed because this machine can't give Chrome a
/// sandbox (no unprivileged user namespaces, a setuid helper that isn't
/// set up, an AppArmor policy that blocks the namespace).
pub(crate) fn sandbox_unavailable(error: &str) -> bool {
    [
        "No usable sandbox",
        "Failed to move to new namespace",
        "SUID sandbox helper binary",
        "setuid sandbox",
        "without --no-sandbox is not supported",
        "zygote_host_impl_linux",
        "unprivileged user namespaces",
        "suid_sandbox",
        "apparmor-userns",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

/// Common Chromium/Chrome/Edge install paths used when no override is given.
fn common_executables() -> Vec<String> {
    let mut paths = Vec::new();
    // Windows: Chrome first (what the tests and CI target), Edge (always
    // installed on Windows) as the fallback.
    let program_dirs: Vec<String> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .collect();
    for p in &program_dirs {
        paths.push(format!("{p}\\Google\\Chrome\\Application\\chrome.exe"));
    }
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        paths.push(format!("{p}\\Google\\Chrome\\Application\\chrome.exe"));
    }
    for p in &program_dirs {
        paths.push(format!("{p}\\Microsoft\\Edge\\Application\\msedge.exe"));
    }
    paths.push("/usr/bin/google-chrome".into());
    paths.push("/usr/bin/google-chrome-stable".into());
    paths.push("/usr/bin/chromium".into());
    paths.push("/usr/bin/chromium-browser".into());
    paths.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    paths.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    paths
}

/// Browsers installed by Playwright / Puppeteer / `@puppeteer/browsers`
/// (newest first), so a machine with only those still works out of the box.
/// For headless sessions chrome-headless-shell comes first: it starts 2-3x
/// faster than full Chrome (no browser UI layer to bring up).
fn cached_browsers(headless: bool) -> Vec<String> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    let mut roots = vec![format!("{home}/.cache/ms-playwright")];
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        roots.push(format!("{p}/ms-playwright"));
    }
    roots.push(format!("{home}/Library/Caches/ms-playwright"));
    if let Ok(p) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        roots.insert(0, p);
    }
    let pick = |root: &str, prefix: &str, rel: &[&str]| -> Vec<String> {
        let mut dirs: Vec<String> = std::fs::read_dir(root)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(prefix))
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_by(|a, b| b.cmp(a));
        dirs.iter()
            .flat_map(|d| rel.iter().map(move |r| format!("{root}/{d}/{r}")))
            .filter(|p| Path::new(p).is_file())
            .collect()
    };
    let mut shells = Vec::new();
    let mut full = Vec::new();
    for root in &roots {
        shells.extend(pick(
            root,
            "chromium_headless_shell-",
            &[
                "chrome-linux/headless_shell",
                "chrome-headless-shell-linux64/chrome-headless-shell",
                "chrome-mac/headless_shell",
                "chrome-win/headless_shell.exe",
                "chrome-headless-shell-win64/chrome-headless-shell.exe",
            ],
        ));
        full.extend(pick(
            root,
            "chromium-",
            &[
                "chrome-linux/chrome",
                "chrome-linux64/chrome",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "chrome-win/chrome.exe",
                "chrome-win64/chrome.exe",
            ],
        ));
    }
    let puppeteer = format!("{home}/.cache/puppeteer");
    shells.extend(pick(
        &format!("{puppeteer}/chrome-headless-shell"),
        "linux-",
        &["chrome-headless-shell-linux64/chrome-headless-shell"],
    ));
    shells.extend(pick(
        &format!("{puppeteer}/chrome-headless-shell"),
        "win64-",
        &["chrome-headless-shell-win64/chrome-headless-shell.exe"],
    ));
    full.extend(pick(&format!("{puppeteer}/chrome"), "linux-", &["chrome-linux64/chrome"]));
    full.extend(pick(&format!("{puppeteer}/chrome"), "win64-", &["chrome-win64/chrome.exe"]));
    if headless {
        shells.into_iter().chain(full).collect()
    } else {
        full
    }
}

/// Resolve which executable to launch, honoring (in order) an explicit CLI
/// override, `$NAVIGERA_CHROMIUM` / `$CHROME_BIN`, the common install
/// paths, then browsers cached by Playwright/Puppeteer.
pub fn resolve_executable(cli_override: Option<&str>) -> Option<String> {
    resolve_executable_for(cli_override, true)
}

/// [`resolve_executable`] for a headed or headless launch.
pub fn resolve_executable_for(cli_override: Option<&str>, headless: bool) -> Option<String> {
    if let Some(path) = cli_override {
        if !path.trim().is_empty() {
            return Some(path.to_string());
        }
    }
    for var in ["NAVIGERA_CHROMIUM", "CHROME_BIN"] {
        if let Ok(path) = std::env::var(var) {
            if !path.trim().is_empty() {
                return Some(path);
            }
        }
    }
    common_executables()
        .into_iter()
        .find(|p| Path::new(p).exists())
        .or_else(|| cached_browsers(headless).into_iter().next())
}

/// True when `--engine` selects the Lightpanda CDP server instead of Chromium.
pub fn is_lightpanda(engine: &str) -> bool {
    matches!(engine.trim().to_ascii_lowercase().as_str(), "lightpanda" | "panda")
}

/// Resolve the `lightpanda` binary for direct `lightpanda serve` launch.
///
/// Order: explicit `--chromium <path>` (kept as the generic binary override),
/// `$LIGHTPANDA_BIN`, then `lightpanda` on `PATH`.
fn resolve_lightpanda_bin(cli_override: Option<&str>) -> Result<String> {
    if let Some(path) = cli_override {
        if !path.trim().is_empty() {
            // `--chromium` doubles as the lightpanda override, so a Chromium
            // path that leaked in from $CHROME_BIN-style defaults would be
            // launched as `chrome serve --port …` and then time out waiting
            // for a /json/version that never comes. Fail fast instead.
            let file =
                Path::new(path).file_name().map(|f| f.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if ["chrome", "chromium", "msedge", "edge", "brave"].iter().any(|b| file.contains(b)) {
                bail!(
                    "--engine lightpanda was given a Chromium binary ({path}); pass the lightpanda binary via --chromium or $LIGHTPANDA_BIN"
                );
            }
            return Ok(path.to_string());
        }
    }
    if let Ok(path) = std::env::var("LIGHTPANDA_BIN") {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    if let Ok(path) = which_lightpanda() {
        return Ok(path);
    }
    bail!("lightpanda binary not found — install it or set --chromium <path> / $LIGHTPANDA_BIN")
}

/// `lightpanda` on `PATH` (no external `which` dependency).
fn which_lightpanda() -> Result<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(if cfg!(windows) { "lightpanda.exe" } else { "lightpanda" });
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("lightpanda not on PATH")
}

/// A browser navigera connects to instead of launching its own.
#[derive(Debug, Clone)]
pub enum AttachTarget {
    /// `--attach <chrome|beta|…|user-data-dir|ws://…|http://…|port>`.
    Running(String),
    /// `--profile <name|dir>`: the persistent navigera browser on that
    /// profile, started when it isn't running.
    Profile {
        name: String,
        executable: Option<String>,
        headless: bool,
        /// `--from`: seed the profile from this browser first (a copy).
        from: Option<String>,
        /// `--from-profile`: which of its profiles.
        from_profile: Option<String>,
    },
}

/// CDP transport when none is asked for. The pipe (fds 3/4) is Unix-only
/// here; Windows uses a DevTools WebSocket on a random localhost port, and
/// a kill-on-close job object stands in for the pipe's "Chrome exits with
/// its driver" guarantee (see `cdp::procjob`).
pub const DEFAULT_TRANSPORT: &str = if cfg!(unix) { "pipe" } else { "ws" };

/// How long a click on a link/submit button or an Enter key waits for the
/// navigation it probably triggers to be requested (it can trail the input
/// event's CDP reply by a task or two). Ends early once it arrives.
const NAV_GRACE: Duration = Duration::from_millis(150);

/// One browser + tab list, driven by the from-scratch CDP engine.
pub struct BrowserSession {
    browser: std::sync::Arc<Browser>,
    pages: Vec<Page>,
    active: usize,
    /// Executable that was launched (browser binary or `lightpanda`); kept so
    /// [`BrowserSession::close`] can run engine-specific cleanup.
    exe: String,
    /// True when the underlying engine is Lightpanda (one tab; popups are
    /// followed in place).
    lightpanda: bool,
    timeout_ms: f64,
    /// (tab target, navigation generation) whose load we stopped waiting
    /// for after a full navigation timeout (a page that streams forever,
    /// a hung server). Later ops don't wait on it again.
    abandoned: std::sync::Mutex<Option<(String, u64)>>,
    /// Connected to a browser navigera doesn't own (`--attach`,
    /// `--profile`): closing only disconnects.
    attached: bool,
    /// The target id of the session's only tab, once that tab was closed
    /// (by the page, or by the user in an attached browser). The tab stays
    /// in `pages`, since every op acts on one, until `tab-new` or `reset`
    /// replaces it.
    closed: Option<String>,
    /// The user data directory of a `--profile` browser, and the browser
    /// binary that runs it.
    profile_dir: Option<std::path::PathBuf>,
    profile_exe: Option<String>,
    /// Where this session's downloads go (`downloads --dir`), when not
    /// the process's download directory; kept across `reset`.
    downloads_dir: Option<std::path::PathBuf>,
    /// `set …` overrides (keyed by what they set): applied to every open
    /// tab and replayed on tabs opened later.
    overrides: std::collections::BTreeMap<&'static str, Vec<(&'static str, serde_json::Value)>>,
    /// `init-script` sources, run before the page's own scripts in every
    /// document of every tab; and per tab (target id) the identifiers to
    /// remove them by.
    init_scripts: Vec<String>,
    init_ids: std::collections::HashMap<String, Vec<String>>,
    /// Modifiers held with `keydown` (Shift, Control, Alt, Meta): later
    /// `press`es carry them until `keyup`.
    held: std::sync::Mutex<Vec<String>>,
    /// The browser context `reset` made (its tabs are the session's).
    context: Option<String>,
    /// False for a session `fork` made: closing it closes its context, not
    /// the browser.
    owner: bool,
    /// Owns the tokio runtime; declared LAST so it drops last, after the
    /// browser/client/pages that use it.
    _runtime: std::sync::Arc<tokio::runtime::Runtime>,
}

impl BrowserSession {
    /// Launch a headless CDP browser ready for navigation.
    ///
    /// `engine` is `chrome` (Chromium/Chrome/Edge) or `lightpanda` (a
    /// Lightpanda CDP server, started directly as `lightpanda serve`).
    pub fn launch(engine: &str, headless: bool, executable: Option<&str>, nav_timeout_ms: f64) -> Result<Self> {
        Self::launch_with(engine, headless, executable, nav_timeout_ms, None)
    }

    /// [`BrowserSession::launch`] with an explicit CDP transport: `pipe`
    /// (default) or `ws` (a DevTools port, e.g. to attach Chrome DevTools).
    pub fn launch_with(
        engine: &str,
        headless: bool,
        executable: Option<&str>,
        nav_timeout_ms: f64,
        transport: Option<&str>,
    ) -> Result<Self> {
        let transport = transport
            .map(str::to_string)
            .or_else(|| std::env::var("NAVIGERA_CDP_TRANSPORT").ok())
            .unwrap_or_else(|| DEFAULT_TRANSPORT.into());
        if !matches!(transport.as_str(), "pipe" | "ws") {
            bail!("--transport takes pipe|ws, got {transport:?}");
        }
        if transport == "pipe" && !cfg!(unix) {
            bail!("--transport pipe needs Linux/macOS; Windows uses ws (the default there)");
        }
        let launch_started = std::time::Instant::now();
        let lightpanda = is_lightpanda(engine);
        let runtime = engine_runtime()?;
        let handle = runtime.handle().clone();

        let (browser, exe) = if lightpanda {
            // Lightpanda is a CDP server, not a Chromium executable: start
            // `lightpanda serve` directly and connect to its CDP endpoint.
            // No Chromium flags, no shim, no X server involved.
            let bin = resolve_lightpanda_bin(executable)?;
            let browser = Browser::launch_lightpanda(&handle, &bin)?;
            crate::timing::log(&format!("[browser] engine=lightpanda launched {} via {bin}", browser.ws_url()));
            (browser, bin)
        } else {
            // Natural mode wants the full Chrome even headless:
            // chrome-headless-shell is a different program with tells of its
            // own.
            let Some(exe) = resolve_executable_for(executable, headless && !natural()) else {
                bail!(
                    "no Chrome/Chromium/Edge found — install Chrome, or `npx @puppeteer/browsers install chrome-headless-shell@stable` and pass its path via --chromium or $CHROME_BIN"
                );
            };
            let launch = |flags: Vec<String>| -> Result<Browser> {
                let browser = Browser::launch(
                    &handle,
                    &LaunchOptions {
                        exe: exe.clone(),
                        headless,
                        chrome_flags: flags,
                        debugging_port: None,
                        pipe: transport == "pipe",
                    },
                )?;
                // A browser that cannot start its sandbox dies before the
                // first target: probe it here so the fallback below sees it.
                if let Err(e) = browser.tab_targets() {
                    let report = browser.death_report();
                    // Reaped and its profile deleted before any retry, so a
                    // fallback launch leaves no zombie or directory behind.
                    browser.close();
                    anyhow::bail!("{e:#} ({report})");
                }
                Ok(browser)
            };
            let mut flags = chrome_flags();
            if natural() {
                flags.extend(natural_flags(&exe, headless)?);
            }
            let note = no_sandbox_note(&exe);
            if !flags.iter().any(|f| f == NO_SANDBOX) && note.as_ref().is_some_and(|n| n.exists()) {
                eprintln!("{NO_SANDBOX_WARNING}");
                flags.push(NO_SANDBOX.into());
            }
            let browser = match launch(flags.clone()) {
                Ok(browser) => browser,
                Err(e) if !flags.iter().any(|f| f == NO_SANDBOX) && sandbox_unavailable(&format!("{e:#}")) => {
                    eprintln!("{NO_SANDBOX_WARNING}");
                    if let Some(note) = &note {
                        // The session directory is ours alone (0700, checked),
                        // so nothing else can have planted a link here.
                        let _ = std::fs::write(note, format!("{exe}\n"));
                    }
                    flags.push(NO_SANDBOX.into());
                    launch(flags)?
                }
                Err(e) => return Err(e),
            };
            crate::timing::log(&format!(
                "[browser] engine=chrome launched {} via {exe} (headless={headless})",
                browser.ws_url(),
            ));
            (browser, exe)
        };
        crate::timing::record("browser_up", launch_started);
        let page_started = std::time::Instant::now();
        let page = if lightpanda {
            browser.new_page_lightpanda(Some("about:blank"))?
        } else {
            // The first command is where a browser that died at startup
            // (missing libraries, bad flag) shows up: say why.
            browser.new_page(Some("about:blank")).map_err(|e| anyhow::anyhow!("{e:#} ({})", browser.death_report()))?
        };
        crate::timing::record("first_page", page_started);
        if !lightpanda {
            // Best effort: a browser without the call just doesn't report
            // downloads (`downloads` then says there are none).
            if let Ok(dir) = download_dir() {
                let _ = browser.enable_downloads(Some(&dir));
            }
        }
        crate::timing::record("launch_total", launch_started);
        Ok(Self {
            browser: std::sync::Arc::new(browser),
            pages: vec![page],
            active: 0,
            exe,
            lightpanda,
            timeout_ms: nav_timeout_ms,
            abandoned: std::sync::Mutex::new(None),
            context: None,
            owner: true,
            attached: false,
            closed: None,
            profile_dir: None,
            profile_exe: None,
            downloads_dir: None,
            overrides: Default::default(),
            init_scripts: Vec::new(),
            init_ids: Default::default(),
            held: Default::default(),
            _runtime: std::sync::Arc::new(runtime),
        })
    }

    /// Work in a browser that is already running (`--attach`) or in the
    /// persistent `--profile` browser (started first if needed). The agent
    /// gets a window of its own; closing the session only disconnects —
    /// the browser, its profile and every window stay open.
    pub fn attach(target: &AttachTarget, nav_timeout_ms: f64) -> Result<Self> {
        let started = std::time::Instant::now();
        let runtime = engine_runtime()?;
        let handle = runtime.handle().clone();
        let profile_dir = match target {
            AttachTarget::Profile { name, .. } => Some(cdp::attach::profile_dir(name)?),
            AttachTarget::Running(_) => None,
        };
        let mut profile_exe = None;
        // A tab's own endpoint: work in that tab, not a new one.
        let adopt = match target {
            AttachTarget::Running(spec) => cdp::attach::page_endpoint(spec),
            AttachTarget::Profile { .. } => None,
        };
        let (ws_url, what) = match target {
            AttachTarget::Running(spec) => match &adopt {
                Some((http, _)) => (cdp::attach::resolve(http)?, format!("attach {spec}")),
                None => (cdp::attach::resolve(spec)?, format!("attach {spec}")),
            },
            AttachTarget::Profile { name, executable, headless, from, from_profile } => {
                let dir = cdp::attach::profile_dir(name)?;
                let Some(exe) = resolve_executable_for(executable.as_deref(), *headless && !natural()) else {
                    bail!("no Chrome/Chromium/Edge found for --profile — pass --chromium <path> or set $CHROME_BIN");
                };
                let mut what = format!("profile {}", dir.display());
                if let Some(from) = from {
                    let copied = cdp::import::import_profile(from, from_profile.as_deref(), &dir, name)?;
                    eprintln!("navigera: {copied}");
                    what = format!("{what}, {copied}");
                }
                let extra = if natural() { natural_flags(&exe, *headless)? } else { Vec::new() };
                let url = cdp::attach::ensure_profile_browser(&exe, &dir, *headless, &extra)?;
                profile_exe = Some(exe);
                (url, what)
            }
        };
        // Chrome asks the user to Allow a connection to their own profile:
        // give them time to click.
        let browser = Browser::connect(&handle, &ws_url, Duration::from_secs(120))?;
        crate::timing::log(&format!("[browser] {what}: connected to {ws_url}"));
        crate::timing::record("browser_up", started);
        let page = match &adopt {
            Some((_, id)) => browser.attach_page(id, "").with_context(|| format!("no tab {id} in that browser"))?,
            None => browser.new_page_in(Some("about:blank"), true)?,
        };
        // A `--profile` browser is navigera's: its downloads go where ours
        // do. The user's own browser (`--attach`) keeps its own setting;
        // navigera only listens.
        let dir = if profile_dir.is_some() { download_dir().ok() } else { None };
        let _ = browser.enable_downloads(dir.as_deref());
        crate::timing::record("launch_total", started);
        Ok(Self {
            browser: std::sync::Arc::new(browser),
            pages: vec![page],
            active: 0,
            exe: what,
            lightpanda: false,
            timeout_ms: nav_timeout_ms,
            abandoned: std::sync::Mutex::new(None),
            context: None,
            owner: true,
            attached: true,
            closed: None,
            profile_dir,
            profile_exe,
            downloads_dir: None,
            overrides: Default::default(),
            init_scripts: Vec::new(),
            init_ids: Default::default(),
            held: Default::default(),
            _runtime: std::sync::Arc::new(runtime),
        })
    }

    /// True when connected to a browser navigera doesn't own.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Ask the browser itself to shut down (`quit --close-browser` on an
    /// attached session): Chrome closes its windows and saves its profile.
    pub fn close_browser(&self) -> Result<()> {
        self.browser.close_remote()?;
        // A `--profile` browser is done when its profile is free again, so
        // the next `start` on it (or a `--from` that replaces it) finds no
        // browser still on its way out.
        if let Some(dir) = &self.profile_dir {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                let lock = std::fs::symlink_metadata(dir.join("SingletonLock")).is_ok();
                if cdp::attach::live_endpoint(dir).is_none() && !lock {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    }

    fn timeout(&self, override_ms: Option<f64>) -> Duration {
        Duration::from_secs_f64(override_ms.unwrap_or(self.timeout_ms) / 1000.0)
    }

    /// (element auto-wait, CDP command timeout) for an element op: the op's
    /// own `timeout_ms` bounds the wait; without one, the element wait is
    /// [`element_wait`] (by default 5 s: fast feedback on a wrong selector).
    fn element_budget(&self, override_ms: Option<f64>) -> (Duration, Duration) {
        let wait = match override_ms {
            Some(ms) => Duration::from_secs_f64(ms / 1000.0),
            None => element_wait(),
        };
        (wait, self.timeout(None).max(wait + Duration::from_secs(1)))
    }

    fn active_tab(&self) -> &Page {
        &self.pages[self.active]
    }

    /// This session's downloads so far (`clear` empties its list afterwards).
    /// Save this session's downloads in `dir` from now on (created if
    /// missing), across `reset`; other sessions keep theirs. A test runner
    /// gives each test its own, so names don't carry over between tests.
    pub fn set_downloads_dir(&mut self, dir: &std::path::Path) -> Result<std::path::PathBuf> {
        if self.lightpanda {
            bail!("downloads --dir needs the chrome engine");
        }
        std::fs::create_dir_all(dir).with_context(|| format!("download directory {}", dir.display()))?;
        // Not `canonicalize` (see `download_dir`).
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        self.browser.enable_downloads_in(Some(&dir), self.context.as_deref())?;
        self.downloads_dir = Some(dir.clone());
        Ok(dir)
    }

    pub fn downloads(&self, clear: bool) -> Vec<cdp::downloads::Download> {
        let scope = self.scope();
        let mut d = self.browser.shared().downloads.lock().unwrap();
        let out: Vec<_> = d.items.iter().filter(|x| x.scope == scope).cloned().collect();
        if clear {
            d.items.retain(|x| x.scope != scope);
        }
        out
    }

    /// Wait for a finished download whose file name matches `glob`
    /// (`*`, `?`; empty: any). A canceled match is an error.
    pub fn wait_download(&self, glob: &str, timeout_ms: Option<f64>) -> Result<cdp::downloads::Download> {
        let (wait, _) = self.element_budget(timeout_ms.or(Some(self.timeout_ms)));
        let deadline = std::time::Instant::now() + wait;
        let matches = |d: &cdp::downloads::Download| glob.is_empty() || cdp::glob_match(glob, &d.suggested);
        loop {
            let items = self.downloads(false);
            if let Some(d) = items.iter().rev().find(|d| matches(d) && d.state == "completed") {
                return Ok(d.clone());
            }
            if let Some(d) = items.iter().rev().find(|d| matches(d) && d.state == "canceled") {
                bail!("download {} was canceled ({})", d.suggested, d.url);
            }
            if std::time::Instant::now() >= deadline {
                let seen: Vec<String> = items.iter().map(|d| format!("{} ({})", d.suggested, d.state)).collect();
                bail!(
                    "no finished download{} within {} ms (seen: {})",
                    if glob.is_empty() { String::new() } else { format!(" matching {glob:?}") },
                    wait.as_millis(),
                    if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Number of open pages (tabs) in this session.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Index of the active tab.
    pub fn active_page(&self) -> usize {
        self.active
    }

    /// Target ids of all open tabs.
    pub fn page_targets(&self) -> Vec<String> {
        self.pages.iter().map(|p| p.target_id().to_string()).collect()
    }

    /// Open a new tab and make it active; returns its index.
    pub fn new_page(&mut self) -> Result<usize> {
        if self.lightpanda {
            // Lightpanda's CDP is single-target: `Target.createTarget` replies
            // `TargetAlreadyLoaded` as soon as a page exists.
            bail!("the lightpanda engine supports one tab only; use the chrome engine for tabs");
        }
        // In this session's own browser context (a fork's, or the one
        // `reset` made), not the default one.
        let page = self.browser.new_page_with(Some("about:blank"), false, self.context.as_deref())?;
        self.apply_overrides(&page);
        self.apply_init_scripts(&page);
        self.pages.push(page);
        self.active = self.pages.len() - 1;
        self.forget_closed();
        Ok(self.active)
    }

    /// A clean slate in the same browser: a new browser context (no
    /// cookies, storage or cache from before, like a fresh incognito
    /// window) with one blank tab; the session's old tabs close. `set`
    /// emulation, routes, credentials, init scripts and the dialog answer
    /// carry over, unless `all`: then the session starts as a new one
    /// would. Tens of milliseconds, against a few hundred for a new
    /// browser.
    pub fn reset(&mut self, all: bool) -> Result<()> {
        if self.lightpanda {
            bail!("reset needs the chrome engine (Lightpanda has one context per process): start a new session");
        }
        // Events already sent (a dialog answered a moment ago) are handled
        // first, so they are moved along, or dropped by `--all`, with the rest.
        self.browser.caught_up();
        let context = self.browser.create_context()?;
        if all {
            self.downloads_dir = None;
        }
        // The user's own browser (`--attach`) keeps its download setting,
        // unless the session chose a directory.
        let dir = match &self.downloads_dir {
            Some(own) => Some(own.clone()),
            None if self.attached && self.profile_dir.is_none() => None,
            None => download_dir().ok(),
        };
        let _ = self.browser.enable_downloads_in(dir.as_deref(), Some(&context));
        // Before the new tab exists, so its first requests see its routes.
        self.browser.shared().move_scope(&self.scope(), &context, !all);
        if all {
            self.overrides.clear();
            self.init_scripts.clear();
            self.init_ids.clear();
        }
        let page = self.browser.new_page_with(Some("about:blank"), false, Some(&context))?;
        self.apply_overrides(&page);
        self.apply_init_scripts(&page);
        let timeout = self.timeout(None);
        for old in self.pages.drain(..) {
            let _ = old.close_target(timeout);
        }
        self.pages.push(page);
        self.active = 0;
        self.closed = None;
        if let Some(old) = self.context.replace(context) {
            let _ = self.browser.dispose_context(&old);
        }
        if let Ok(mut a) = self.abandoned.lock() {
            *a = None;
        }
        self.bring_to_front()
    }

    /// Another session on the same browser, in a browser context of its
    /// own (cookies, storage, cache and tabs apart), for running several
    /// sessions at once without a browser each. Routes, credentials, init
    /// scripts, `set` emulation and the dialog answer are each session's
    /// own (a fork starts with none); the process-wide settings (element
    /// wait, download directory, natural mode) are shared. Closing a fork
    /// closes its context; the browser closes with the session it was
    /// forked from.
    pub fn fork(&self) -> Result<BrowserSession> {
        if self.lightpanda {
            bail!("the lightpanda engine has one context per process: start another session instead");
        }
        self.browser.shared().forked.store(true, std::sync::atomic::Ordering::Relaxed);
        let context = self.browser.create_context()?;
        let dir = if self.attached && self.profile_dir.is_none() { None } else { download_dir().ok() };
        let _ = self.browser.enable_downloads_in(dir.as_deref(), Some(&context));
        let page = self.browser.new_page_with(Some("about:blank"), false, Some(&context))?;
        Ok(BrowserSession {
            browser: std::sync::Arc::clone(&self.browser),
            pages: vec![page],
            active: 0,
            exe: self.exe.clone(),
            lightpanda: false,
            timeout_ms: self.timeout_ms,
            abandoned: std::sync::Mutex::new(None),
            attached: self.attached,
            closed: None,
            profile_dir: None,
            profile_exe: None,
            downloads_dir: None,
            overrides: Default::default(),
            init_scripts: Vec::new(),
            init_ids: Default::default(),
            held: std::sync::Mutex::new(Vec::new()),
            context: Some(context),
            owner: false,
            _runtime: std::sync::Arc::clone(&self._runtime),
        })
    }

    /// Switch the active tab by zero-based index.
    pub fn select_page(&mut self, index: usize) -> Result<()> {
        if index >= self.pages.len() {
            bail!("no tab {index} (have {})", self.pages.len());
        }
        self.active = index;
        self.bring_to_front()
    }

    /// Make the active tab the foreground one.
    fn bring_to_front(&self) -> Result<()> {
        self.browser.activate_target(self.active_tab().target_id())
    }

    /// Close one tab by index; the last remaining tab cannot be closed.
    pub fn close_page(&mut self, index: usize) -> Result<()> {
        if self.pages.len() <= 1 {
            bail!("refusing to close the session's only tab; use `quit` instead");
        }
        if index >= self.pages.len() {
            bail!("no tab {index} (have {})", self.pages.len());
        }
        let timeout = self.timeout(None);
        self.pages[index].close_target(timeout)?;
        self.pages.remove(index);
        let closed_active = index == self.active;
        if self.active >= self.pages.len() {
            self.active = self.pages.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        if closed_active {
            // The tab we act on next was in the background.
            self.bring_to_front()?;
        }
        Ok(())
    }

    /// Adopt tabs the page opened itself (`target=_blank`, `window.open`)
    /// and drop tabs the page closed (`window.close()`). A newly opened tab
    /// becomes active, as it would in a visible browser. Returns the
    /// indices of adopted tabs.
    pub fn sync_tabs(&mut self) -> Vec<usize> {
        self.browser.caught_up();
        let shared = std::sync::Arc::clone(self.browser.shared());
        let mut adopted = Vec::new();
        let forked = shared.forked.load(std::sync::atomic::Ordering::Relaxed);
        let mine: Vec<String> = self.pages.iter().map(|p| p.target_id().to_string()).collect();
        let opened = if forked {
            shared.take_opened_by(|opener| mine.iter().any(|t| t == opener))
        } else {
            shared.take_opened()
        };
        for (target_id, opener) in opened {
            if self.pages.iter().any(|p| p.target_id() == target_id) {
                continue;
            }
            // In someone else's browser, only tabs our own tabs opened are
            // ours; the user's popups stay theirs.
            if self.attached && !self.pages.iter().any(|p| p.target_id() == opener) {
                continue;
            }
            match self.browser.attach_page(&target_id, &self.scope()) {
                Ok(page) => {
                    self.apply_overrides(&page);
                    self.apply_init_scripts(&page);
                    self.pages.push(page);
                    self.active = self.pages.len() - 1;
                    adopted.push(self.active);
                }
                Err(e) => eprintln!("[browser] could not adopt new tab {target_id}: {e:#}"),
            }
        }
        let mine: Vec<String> = self.pages.iter().map(|p| p.target_id().to_string()).collect();
        let destroyed =
            if forked { shared.take_destroyed_by(|t| mine.iter().any(|m| m == t)) } else { shared.take_destroyed() };
        for target_id in destroyed {
            let Some(index) = self.pages.iter().position(|p| p.target_id() == target_id) else { continue };
            if self.pages.len() <= 1 {
                if self.target_is_gone(&target_id) {
                    self.closed = Some(target_id);
                }
                continue;
            }
            shared.unregister(self.pages[index].session_id());
            self.pages.remove(index);
            if self.active >= self.pages.len() {
                self.active = self.pages.len() - 1;
            } else if index < self.active {
                self.active -= 1;
            }
        }
        adopted
    }

    /// The target id of the session's only tab, if it was closed.
    pub fn closed_tab(&self) -> Option<&str> {
        self.closed.as_deref()
    }

    /// An op failed because Chrome no longer knows the tab's session: the
    /// tab closed and its event may not have been read yet. Catch up and,
    /// if it was the only tab, remember it is gone.
    pub fn tab_gone(&mut self) {
        self.sync_tabs();
        if self.closed.is_some() {
            return;
        }
        // The session can end a moment before the browser stops listing
        // its target: give it a second.
        let target = self.active_tab().target_id().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !self.target_is_gone(&target) {
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.pages.len() == 1 {
            self.closed = Some(target);
        } else {
            // Another tab is left: go on in it, as when the event is read.
            self.browser.shared().unregister(self.pages[self.active].session_id());
            self.pages.remove(self.active);
            self.active = self.active.min(self.pages.len() - 1);
        }
    }

    /// The browser no longer lists the tab. Asked before a session gives
    /// up on its only tab, so that an event for a tab that lives on (a
    /// target swapped mid-navigation) can't strand the session; when the
    /// browser can't say, the tab is taken to be there.
    fn target_is_gone(&self, target_id: &str) -> bool {
        !self.lightpanda && self.browser.tab_targets().is_ok_and(|tabs| !tabs.iter().any(|(id, _)| id == target_id))
    }

    /// Drop the closed tab once another one is open.
    fn forget_closed(&mut self) {
        let Some(target) = self.closed.take() else { return };
        if self.pages.len() <= 1 {
            return;
        }
        if let Some(index) = self.pages.iter().position(|p| p.target_id() == target) {
            self.browser.shared().unregister(self.pages[index].session_id());
            self.pages.remove(index);
            if index < self.active {
                self.active -= 1;
            }
        }
    }

    /// Lightpanda keeps one tab: a page it opened with `target=_blank` or
    /// `window.open` is loaded in the active tab instead, as if the agent had
    /// followed it to the new tab. Returns a note per page followed.
    pub fn follow_popups(&self, timeout_ms: Option<f64>) -> Vec<String> {
        if !self.lightpanda {
            return Vec::new();
        }
        self.browser.caught_up();
        let shared = self.browser.shared();
        let urls: Vec<String> =
            shared.take_popups().into_iter().filter(|u| !u.is_empty() && u != "about:blank").collect();
        let Some(url) = urls.last() else { return Vec::new() };
        let note = match self.goto_wait(url, "load", timeout_ms) {
            Ok(()) => {
                format!("the page opened {url} in a new tab; the lightpanda engine has one tab, so it was loaded here")
            }
            Err(e) => format!("the page opened {url} in a new tab; loading it here failed: {e:#}"),
        };
        // The navigation we just made is not a popup.
        shared.take_popups();
        vec![note]
    }

    /// The active tab's console log, oldest first (`clear` empties it).
    pub fn console(&self, clear: bool) -> Vec<cdp::logs::ConsoleEntry> {
        self.browser.caught_up();
        let mut logs = self.browser.shared().logs.lock().unwrap();
        match logs.tabs.get_mut(self.active_tab().session_id()) {
            Some(tab) if clear => std::mem::take(&mut tab.console).into(),
            Some(tab) => tab.console.iter().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// The active tab's network requests, oldest first (`clear` empties the log).
    pub fn requests(&self, clear: bool) -> Vec<cdp::logs::Request> {
        self.browser.caught_up();
        let mut logs = self.browser.shared().logs.lock().unwrap();
        match logs.tabs.get_mut(self.active_tab().session_id()) {
            Some(tab) if clear => std::mem::take(&mut tab.requests).into(),
            Some(tab) => tab.requests.iter().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// A logged request's response body: (body, base64-encoded?).
    pub fn response_body(&self, request_id: &str) -> Result<(String, bool)> {
        let r = self.active_tab().send_cdp(
            "Network.getResponseBody",
            serde_json::json!({ "requestId": request_id }),
            self.timeout(None),
        )?;
        Ok((
            r.get("body").and_then(|b| b.as_str()).unwrap_or_default().to_string(),
            r.get("base64Encoded").and_then(|b| b.as_bool()).unwrap_or(false),
        ))
    }

    /// A logged request's body when the event didn't carry it.
    pub fn request_post_data(&self, request_id: &str) -> Result<String> {
        let r = self.active_tab().send_cdp(
            "Network.getRequestPostData",
            serde_json::json!({ "requestId": request_id }),
            self.timeout(None),
        )?;
        Ok(r.get("postData").and_then(|b| b.as_str()).unwrap_or_default().to_string())
    }

    /// Replace one kind of override (`viewport`, `media`, …) on every tab,
    /// and for tabs opened later. `calls` are CDP (method, params) sent to
    /// each tab; `reset` undoes the previous ones first.
    pub fn set_override(
        &mut self,
        kind: &'static str,
        calls: Vec<(&'static str, serde_json::Value)>,
        reset: Vec<(&'static str, serde_json::Value)>,
    ) -> Result<()> {
        let timeout = self.timeout(None);
        for page in &self.pages {
            for (method, params) in reset.iter().chain(calls.iter()) {
                page.send_cdp(method, params.clone(), timeout).map_err(|e| anyhow::anyhow!("{method}: {e:#}"))?;
            }
        }
        if calls.is_empty() {
            self.overrides.remove(kind);
        } else {
            self.overrides.insert(kind, calls);
        }
        Ok(())
    }

    /// Read a property of the target element (`get`): text, value, attr,
    /// html, outer, box, style, checked.
    pub fn get_prop(
        &self,
        target: &Target,
        what: &str,
        name: Option<&str>,
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().on_element(
            target,
            "get",
            cdp::page_js::GET_EL,
            &[serde_json::json!(what), serde_json::json!(name)],
            wait,
            timeout,
        )
    }

    /// How many elements match a CSS selector (no waiting).
    pub fn count(&self, selector: &str) -> Result<u64> {
        let n: serde_json::Value = self
            .active_tab()
            .evaluate(&format!("{}.all({}).length", cdp::DEEP, serde_json::json!(selector)), self.timeout(None))?;
        Ok(n.as_u64().unwrap_or(0))
    }

    /// An element state (`is`). A missing element is not visible, not
    /// enabled, not checked: false (true for `hidden`), after a short wait.
    pub fn is_state(&self, target: &Target, state: &str, timeout_ms: Option<f64>) -> Result<bool> {
        let (_, timeout) = self.element_budget(timeout_ms);
        let wait = timeout_ms.map(|ms| Duration::from_secs_f64(ms / 1000.0)).unwrap_or(Duration::from_millis(500));
        match self.active_tab().on_element(
            target,
            "is",
            cdp::page_js::IS_EL,
            &[serde_json::json!(state)],
            wait,
            timeout,
        ) {
            Ok(v) => Ok(v.as_bool().unwrap_or(false)),
            Err(e) if format!("{e:#}").contains("no element") => Ok(state == "hidden"),
            Err(e) => Err(e),
        }
    }

    /// Drag `from` onto `to` (see [`cdp::Page::drag`]); waits for any
    /// navigation the drop starts.
    pub fn drag(&self, from: &Target, to: &Target, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let r = self.active_tab().drag(from, to, wait, timeout)?;
        self.settle_for(timeout_ms);
        Ok(r)
    }

    pub fn dblclick(&self, target: &Target, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let point = self.active_tab().dblclick(target, wait, timeout)?;
        self.settle_for(timeout_ms);
        Ok(point)
    }

    /// The locators that find `target`'s element again (`locate`).
    pub fn locate(&self, target: &Target, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().locate(target, wait, timeout)
    }

    pub fn focus(&self, target: &Target, timeout_ms: Option<f64>) -> Result<bool> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let v = self.active_tab().on_element(target, "focus", cdp::page_js::FOCUS_EL, &[], wait, timeout)?;
        Ok(v.as_bool().unwrap_or(false))
    }

    /// Make a checkbox/radio/switch checked (`want`) by clicking it when it
    /// isn't; fails when the click didn't change it.
    pub fn set_checked(&self, target: &Target, want: bool, timeout_ms: Option<f64>) -> Result<bool> {
        let checked = |s: &Self| -> Result<bool> {
            Ok(s.get_prop(target, "checked", None, timeout_ms)?.as_bool().unwrap_or(false)
                || s.is_state(target, "checked", timeout_ms)?)
        };
        if checked(self)? == want {
            return Ok(false);
        }
        self.click_target(target, &cdp::ClickOptions::default(), timeout_ms)?;
        if checked(self)? != want {
            bail!("clicking it did not {} it", if want { "check" } else { "uncheck" });
        }
        Ok(true)
    }

    pub fn key_event(&self, key: &str, down: bool) -> Result<()> {
        self.active_tab().key_event(key, down, self.timeout(None))?;
        let modifier = ["Shift", "Control", "Alt", "Meta"].into_iter().find(|m| m.eq_ignore_ascii_case(key));
        if let Some(m) = modifier {
            let mut held = self.held.lock().unwrap();
            held.retain(|h| h != m);
            if down {
                held.push(m.to_string());
            }
        }
        Ok(())
    }

    pub fn insert_text(&self, target: Option<&Target>, text: &str, timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().insert_text(target, text, wait, timeout)
    }

    /// Run `source` before the page's own scripts in every new document of
    /// every tab (and in the current document right away). Returns how
    /// many scripts are installed.
    pub fn add_init_script(&mut self, source: &str) -> Result<usize> {
        let timeout = self.timeout(None);
        for page in &self.pages {
            let r = page
                .send_cdp(
                    "Page.addScriptToEvaluateOnNewDocument",
                    serde_json::json!({ "source": source, "runImmediately": true }),
                    timeout,
                )
                .map_err(|e| anyhow::anyhow!("Page.addScriptToEvaluateOnNewDocument: {e:#}"))?;
            if let Some(id) =
                r.get("identifier").map(|i| i.as_str().map(str::to_string).unwrap_or_else(|| i.to_string()))
            {
                self.init_ids.entry(page.target_id().to_string()).or_default().push(id);
            }
        }
        self.init_scripts.push(source.to_string());
        Ok(self.init_scripts.len())
    }

    /// Every cookie, and the localStorage of each origin open in a tab, as
    /// Playwright's `storageState` writes them, so a file from either tool
    /// loads in the other.
    pub fn storage_state(&self) -> Result<serde_json::Value> {
        let cookies = self.cookies(true, None)?;
        let cookies: Vec<serde_json::Value> = cookies
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                serde_json::json!({
                    "name": c["name"],
                    "value": c["value"],
                    "domain": c["domain"],
                    "path": c["path"],
                    "expires": c.get("expires").cloned().unwrap_or(serde_json::json!(-1)),
                    "httpOnly": c.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false),
                    "secure": c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false),
                    "sameSite": c.get("sameSite").and_then(|v| v.as_str()).unwrap_or("Lax"),
                })
            })
            .collect();
        let mut origins: Vec<serde_json::Value> = Vec::new();
        for page in &self.pages {
            let Ok(here) = page.evaluate(
                "() => { try { return { origin: location.origin, \
                 localStorage: Object.entries(localStorage).map(([name, value]) => ({ name, value })) }; } \
                 catch (e) { return null; } }",
                self.timeout(None),
            ) else {
                continue;
            };
            let origin = here.get("origin").and_then(|o| o.as_str()).unwrap_or("null");
            let empty = here.get("localStorage").and_then(|l| l.as_array()).is_none_or(Vec::is_empty);
            if origin == "null" || empty || origins.iter().any(|o| o["origin"] == origin) {
                continue;
            }
            origins.push(here);
        }
        Ok(serde_json::json!({ "cookies": cookies, "origins": origins }))
    }

    /// Put a [`BrowserSession::storage_state`] file back: its cookies now, and
    /// each origin's localStorage into the tab already on that origin and,
    /// through an init script, into any document of that origin that does
    /// not have the key yet. Returns how many cookies and origins it read.
    pub fn load_storage_state(&mut self, state: &serde_json::Value) -> Result<(usize, usize)> {
        let cookies = state.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        for c in &cookies {
            let mut p = serde_json::json!({
                "name": c["name"],
                "value": c["value"],
                "domain": c["domain"],
                "path": c.get("path").cloned().unwrap_or(serde_json::json!("/")),
            });
            for key in ["httpOnly", "secure", "sameSite"] {
                if let Some(v) = c.get(key) {
                    p[key] = v.clone();
                }
            }
            if let Some(e) = c.get("expires").and_then(|e| e.as_f64()).filter(|e| *e > 0.0) {
                p["expires"] = serde_json::json!(e);
            }
            self.set_cookie(p).with_context(|| format!("state load: cookie {}", c["name"]))?;
        }
        let origins = state.get("origins").and_then(|o| o.as_array()).cloned().unwrap_or_default();
        if !origins.is_empty() {
            let data = serde_json::to_string(&origins)?;
            // Set-if-absent: what the page itself stores later is kept.
            let script = format!(
                "(() => {{ for (const o of {data}) {{ if (o.origin !== location.origin) continue; \
                 try {{ for (const {{ name, value }} of o.localStorage || []) \
                 if (localStorage.getItem(name) === null) localStorage.setItem(name, value); }} catch (e) {{}} }} }})()"
            );
            self.add_init_script(&script)?;
        }
        Ok((cookies.len(), origins.len()))
    }

    /// Remove every `init-script`; returns how many there were.
    pub fn clear_init_scripts(&mut self) -> usize {
        let timeout = self.timeout(None);
        for page in &self.pages {
            for id in self.init_ids.remove(page.target_id()).unwrap_or_default() {
                let _ = page.send_cdp(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    serde_json::json!({ "identifier": id }),
                    timeout,
                );
            }
        }
        self.init_ids.clear();
        std::mem::take(&mut self.init_scripts).len()
    }

    pub fn init_scripts(&self) -> &[String] {
        &self.init_scripts
    }

    fn apply_init_scripts(&mut self, page: &Page) {
        let timeout = self.timeout(None);
        for source in &self.init_scripts {
            match page.send_cdp(
                "Page.addScriptToEvaluateOnNewDocument",
                serde_json::json!({ "source": source, "runImmediately": true }),
                timeout,
            ) {
                Ok(r) => {
                    if let Some(id) = r.get("identifier").and_then(|i| i.as_str()) {
                        self.init_ids.entry(page.target_id().to_string()).or_default().push(id.to_string());
                    }
                }
                Err(e) => eprintln!("[browser] init-script on a new tab: {e:#}"),
            }
        }
    }

    /// The key of this session's routes, credentials and dialog answer:
    /// its browser context ("" for the default one). Other sessions on the
    /// same browser (forks) have their own.
    fn scope(&self) -> String {
        self.context.clone().unwrap_or_default()
    }

    /// Add a `route` rule (a rule with the same pattern is replaced).
    pub fn add_route(&mut self, route: cdp::Route) -> Result<()> {
        {
            let mut all = self.browser.shared().routes.lock().unwrap();
            let routes = all.entry(self.scope()).or_default();
            routes.retain(|r| r.pattern != route.pattern);
            routes.push(route);
        }
        self.sync_fetch()
    }

    /// Remove the rule for `pattern`, or every rule; returns how many went.
    pub fn remove_routes(&mut self, pattern: Option<&str>) -> Result<usize> {
        let removed = {
            let mut all = self.browser.shared().routes.lock().unwrap();
            let routes = all.entry(self.scope()).or_default();
            let before = routes.len();
            routes.retain(|r| pattern.is_some_and(|p| r.pattern != p));
            before - routes.len()
        };
        self.sync_fetch()?;
        Ok(removed)
    }

    pub fn routes(&self) -> Vec<cdp::Route> {
        self.browser.shared().routes.lock().unwrap().get(&self.scope()).cloned().unwrap_or_default()
    }

    /// HTTP auth credentials for every origin this session's tabs visit
    /// (`None`: answer challenges the browser's way, i.e. fail them).
    pub fn set_credentials(&mut self, credentials: Option<(String, String)>) -> Result<()> {
        {
            let mut all = self.browser.shared().credentials.lock().unwrap();
            match credentials {
                Some(c) => all.insert(self.scope(), c),
                None => all.remove(&self.scope()),
            };
        }
        self.sync_fetch()
    }

    /// Fetch interception on every tab, for exactly the URLs a rule names
    /// (all of them once credentials are set): unrouted requests never wait
    /// on navigera.
    fn sync_fetch(&mut self) -> Result<()> {
        let shared = self.browser.shared();
        let scope = self.scope();
        let auth = shared.credentials.lock().unwrap().contains_key(&scope);
        let patterns: Vec<serde_json::Value> = if auth {
            vec![serde_json::json!({ "urlPattern": "*" })]
        } else {
            shared
                .routes
                .lock()
                .unwrap()
                .get(&scope)
                .into_iter()
                .flatten()
                .map(|r| serde_json::json!({ "urlPattern": r.pattern }))
                .collect()
        };
        if patterns.is_empty() {
            return self.set_override("fetch", vec![], vec![("Fetch.disable", serde_json::json!({}))]);
        }
        self.set_override(
            "fetch",
            vec![("Fetch.enable", serde_json::json!({ "patterns": patterns, "handleAuthRequests": auth }))],
            vec![],
        )
    }

    /// Browser-wide permission for the pages' origins (geolocation).
    pub fn grant(&self, permission: &str) {
        let _ = self.browser.grant_permission(permission);
    }

    fn apply_overrides(&self, page: &Page) {
        let timeout = self.timeout(None);
        for calls in self.overrides.values() {
            for (method, params) in calls {
                if let Err(e) = page.send_cdp(method, params.clone(), timeout) {
                    eprintln!("[browser] {method} on a new tab: {e:#}");
                }
            }
        }
    }

    /// Cookies the current page sends (`url`: that URL's), or every cookie.
    pub fn cookies(&self, all: bool, url: Option<&str>) -> Result<serde_json::Value> {
        let tab = self.active_tab();
        let r = if all {
            // After `reset` (or in a fork) the tabs live in a context of
            // their own: its cookies, not the default context's.
            match &self.context {
                Some(c) => self.browser.context_cookies(Some(c))?,
                None => tab.send_cdp("Storage.getCookies", serde_json::json!({}), self.timeout(None))?,
            }
        } else {
            let urls = vec![url.map(str::to_string).unwrap_or_else(|| self.url())];
            tab.send_cdp("Network.getCookies", serde_json::json!({ "urls": urls }), self.timeout(None))?
        };
        Ok(r.get("cookies").cloned().unwrap_or_else(|| serde_json::json!([])))
    }

    /// `Network.setCookie` with these parameters.
    pub fn set_cookie(&self, cookie: serde_json::Value) -> Result<()> {
        let r = self.active_tab().send_cdp("Network.setCookie", cookie, self.timeout(None))?;
        if r.get("success").and_then(|v| v.as_bool()) == Some(false) {
            bail!("the browser refused the cookie (check the domain/url, and Secure for SameSite=None)");
        }
        Ok(())
    }

    pub fn delete_cookies(&self, params: serde_json::Value) -> Result<()> {
        self.active_tab().send_cdp("Network.deleteCookies", params, self.timeout(None)).map(|_| ())
    }

    pub fn clear_cookies(&self) -> Result<()> {
        self.active_tab().send_cdp("Network.clearBrowserCookies", serde_json::json!({}), self.timeout(None)).map(|_| ())
    }

    /// The active tab printed to PDF.
    pub fn pdf(&self, landscape: bool, background: bool) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let r = self.active_tab().send_cdp(
            "Page.printToPDF",
            serde_json::json!({ "landscape": landscape, "printBackground": background }),
            self.timeout(None).max(Duration::from_secs(60)),
        )?;
        let data =
            r.get("data").and_then(|d| d.as_str()).ok_or_else(|| anyhow::anyhow!("printToPDF returned no data"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    /// Uncaught exceptions in the active tab not reported yet.
    pub fn take_exceptions(&self) -> Vec<String> {
        self.browser.caught_up();
        self.browser.shared().logs.lock().unwrap().take_exceptions(self.active_tab().session_id())
    }

    /// Dialogs answered since the last call (`{type, message, accepted}`).
    pub fn take_dialogs(&self) -> Vec<serde_json::Value> {
        self.browser.caught_up();
        self.browser.shared().take_dialogs(&self.scope())
    }

    /// How future `alert`/`confirm`/`prompt` dialogs in this session's tabs
    /// are answered.
    pub fn set_dialog_policy(&self, accept: bool, prompt_text: Option<String>) {
        self.browser.set_dialog_policy(&self.scope(), DialogPolicy { accept, prompt_text });
    }

    /// Wait for a navigation an earlier action started in the active tab.
    /// Never fails: after the full navigation timeout the load is
    /// abandoned (later ops don't wait on it again) and the op goes on with
    /// whatever document is there. Returns true when the tab settled.
    pub fn settle(&self) -> bool {
        self.settle_within(self.timeout(None))
    }

    /// [`BrowserSession::settle`] bounded by `budget` (an op's own
    /// timeout). Only a wait that used the whole navigation timeout marks
    /// the load abandoned; a shorter op budget leaves it for the next op.
    pub fn settle_within(&self, budget: Duration) -> bool {
        // A navigation the last command started may already be reported
        // (Lightpanda sends it before the command's response), or be
        // reported only once the engine handles another message (one a
        // script scheduled): ask for that message, then let the watcher
        // record what came before deciding there is nothing to wait for.
        self.active_tab().flush_events(Duration::from_millis(500));
        self.browser.caught_up();
        let tab = self.active_tab();
        let key = (tab.target_id().to_string(), tab.nav_generation());
        if self.abandoned.lock().ok().is_some_and(|a| a.as_ref() == Some(&key)) {
            return false;
        }
        if tab.settle(budget).is_ok() {
            return true;
        }
        if budget >= self.timeout(None) {
            if let Ok(mut a) = self.abandoned.lock() {
                *a = Some(key);
            }
        }
        false
    }

    /// Settle after an action: up to the op's own `timeout_ms` if it gave
    /// one, else the navigation timeout.
    fn settle_for(&self, timeout_ms: Option<f64>) -> bool {
        self.settle_within(
            timeout_ms.map(|ms| Duration::from_secs_f64(ms / 1000.0)).unwrap_or_else(|| self.timeout(None)),
        )
    }

    /// Let fetch/XHR data the active tab is loading arrive before a read
    /// (see [`Page::network_quiet`]).
    pub fn network_quiet(&self, quiet: Duration, max: Duration) -> bool {
        self.active_tab().network_quiet(quiet, max)
    }

    /// The default navigation timeout (`--timeout-ms`).
    pub fn nav_timeout(&self) -> Duration {
        self.timeout(None)
    }

    /// True while the active tab's document is still loading (before
    /// DOMContentLoaded), e.g. after a settle gave up.
    pub fn is_loading(&self) -> bool {
        self.active_tab().is_loading()
    }

    /// Current URL and title in one round trip.
    pub fn url_title(&self) -> (String, String) {
        let v = self.active_tab().evaluate("[location.href, document.title]", self.timeout(None)).unwrap_or_default();
        let get = |i: usize| v.get(i).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        (get(0), get(1))
    }

    /// Current page URL.
    pub fn url(&self) -> String {
        self.active_tab().url(self.timeout(None)).unwrap_or_default()
    }

    /// Navigate the active tab and wait for `load`.
    pub fn goto(&self, url: &str) -> Result<()> {
        self.goto_wait(url, "load", None)
    }

    /// Navigate the active tab; `wait` is `load`, `domcontentloaded` or
    /// `commit`. Retries benign aborts (third-party trackers can abort the
    /// main-frame load mid-flight).
    pub fn goto_wait(&self, url: &str, wait: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.goto_or_download(url, wait, timeout_ms).map(|_| ())
    }

    /// [`BrowserSession::goto_wait`]; true when the URL turned out to be a
    /// download (the page stays where it was; `downloads` lists the file).
    pub fn goto_or_download(&self, url: &str, wait: &str, timeout_ms: Option<f64>) -> Result<bool> {
        if !matches!(wait, "load" | "domcontentloaded" | "commit") {
            bail!("--wait takes load|domcontentloaded|commit, got {wait:?}");
        }
        // Honors --timeout-ms (was a hard-coded 40 s per attempt).
        let timeout = self.timeout(timeout_ms);
        let max_attempts = 3;
        let mut attempt = 0;
        let downloads_before = self.downloads(false).iter().map(|d| d.n).max().unwrap_or(0);
        let requests_before = self.requests(false).last().map(|r| r.n).unwrap_or(0);
        loop {
            match self.active_tab().navigate(url, wait, timeout) {
                Ok(_) => return Ok(false),
                Err(e) => {
                    let msg = format!("{e:#}");
                    // ERR_ABORTED is the outcome, not a hiccup, when the URL
                    // answered with no content (the browser stays put) or
                    // turned into a download (saved: `downloads` lists it).
                    // Trying again would fetch it again: a download link
                    // downloaded the file four times.
                    if msg.contains("ERR_ABORTED") {
                        let status = self
                            .requests(false)
                            .into_iter()
                            .filter(|r| r.n > requests_before && r.kind == "Document")
                            .filter_map(|r| r.status)
                            .next_back();
                        if let Some(code @ (204 | 205)) = status {
                            bail!("{msg}: the server answered {code} (no content), so the browser stays on the page it was on");
                        }
                        let end = std::time::Instant::now() + Duration::from_secs(1);
                        loop {
                            if self.downloads(false).iter().any(|d| d.n > downloads_before) {
                                return Ok(true);
                            }
                            if std::time::Instant::now() >= end {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    attempt += 1;
                    let transient = msg.contains("ERR_ABORTED") || msg.contains("ERR_CONNECTION_RESET");
                    if transient && attempt <= max_attempts {
                        eprintln!("[browser] nav hiccup (attempt {attempt}): {msg}");
                        std::thread::sleep(Duration::from_millis(500 * attempt as u64));
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Navigate without waiting for load (commit semantics).
    pub fn goto_commit(&self, url: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.active_tab().navigate_commit(url, self.timeout(timeout_ms))
    }

    pub fn back(&self) -> Result<()> {
        self.active_tab().history(-1, self.timeout(None))
    }

    pub fn forward(&self) -> Result<()> {
        self.active_tab().history(1, self.timeout(None))
    }

    pub fn reload(&self) -> Result<()> {
        self.active_tab().reload(self.timeout(None))
    }

    /// Run a JS expression in the active tab; result must be JSON.
    pub fn evaluate<T: DeserializeOwned>(&self, expression: &str) -> Result<T> {
        self.evaluate_with_timeout(expression, None)
    }

    /// Run a JS expression with an explicit timeout override.
    pub fn evaluate_with_timeout<T: DeserializeOwned>(&self, expression: &str, timeout_ms: Option<f64>) -> Result<T> {
        let value = self.active_tab().evaluate(expression, self.timeout(timeout_ms))?;
        Ok(serde_json::from_value(value)?)
    }

    /// Click the element `target` names (trusted mouse click when hittable).
    /// If the click starts a navigation, waits for the new document.
    pub fn click_target(
        &self,
        target: &Target,
        opts: &cdp::ClickOptions,
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let before = self.active_tab().activity();
        let point = self.active_tab().click(target, opts, wait, timeout)?;
        if point.get("nav").and_then(|v| v.as_bool()) == Some(true) {
            self.active_tab().navigation_started_since(before, NAV_GRACE);
        }
        self.settle_for(timeout_ms);
        Ok(point)
    }

    /// Raw mouse input (see [`cdp::Page::mouse`]); a click waits for any
    /// navigation it starts.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse(
        &self,
        action: &str,
        at: Option<(f64, f64)>,
        button: &str,
        count: u32,
        steps: u32,
        delta: (f64, f64),
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (_, timeout) = self.element_budget(timeout_ms);
        let page = self.active_tab();
        let before = page.activity();
        let r = page.mouse(action, at, button, count, steps, delta, timeout)?;
        if action == "click" {
            page.navigation_started_since(before, NAV_GRACE);
            self.settle_for(timeout_ms);
        }
        Ok(r)
    }

    /// Where `mouse` goes for a target: `dx`,`dy` from its top-left corner,
    /// or its center, in top-level viewport pixels; scrolled into view
    /// first when that point is out of it. Fails when the point can't be
    /// brought into the viewport, or the element is in a cross-origin frame.
    pub fn mouse_point(&self, target: &Target, d: Option<(f64, f64)>, timeout_ms: Option<f64>) -> Result<(f64, f64)> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let (dx, dy) = match d {
            Some((x, y)) => (serde_json::json!(x), serde_json::json!(y)),
            None => (serde_json::Value::Null, serde_json::Value::Null),
        };
        let v = self.active_tab().on_element(target, "mouse", cdp::page_js::MOUSE_AT, &[dx, dy], wait, timeout)?;
        if v.get("xframe").is_some() {
            bail!("the element is in a cross-origin frame: aim with viewport coordinates (`get box` of its frame, plus the offset)");
        }
        let (x, y) = (v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0));
        if v.get("outside").is_some() {
            bail!(
                "the point ({x:.0}, {y:.0}) is outside the viewport ({}x{}) even with the element scrolled into view: \
                 an offset beyond the element?",
                v["width"],
                v["height"]
            );
        }
        Ok((x, y))
    }

    /// The viewport's size in CSS pixels.
    pub fn viewport_size(&self) -> Result<(f64, f64)> {
        let v: Vec<f64> = self.evaluate("[innerWidth, innerHeight]")?;
        Ok((v.first().copied().unwrap_or(0.0), v.get(1).copied().unwrap_or(0.0)))
    }

    pub fn hover(&self, target: &Target, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().hover(target, wait, timeout)
    }

    pub fn fill_target(&self, target: &Target, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().fill(target, value, wait, timeout)
    }

    /// `fill --keys`: select what the field holds and type over it, key by
    /// key; Backspace clears it for an empty value.
    pub fn fill_keys(&self, target: &Target, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let page = self.active_tab();
        if page.on_element(target, "fill", cdp::page_js::FOCUS_SELECT_ALL, &[], wait, timeout)?
            != serde_json::Value::Bool(true)
        {
            bail!("the field did not take focus (disabled, hidden, or not editable); nothing was typed");
        }
        if value.is_empty() {
            page.press(None, "Backspace", wait, timeout)?;
        } else {
            page.type_text(None, value, wait, timeout)?;
        }
        self.settle_for(timeout_ms);
        Ok(())
    }

    pub fn select(
        &self,
        target: &Target,
        values: &[String],
        add: bool,
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let chosen = match self.active_tab().select(target, values, add, wait, timeout) {
            Err(e) if format!("{e:#}").contains("not a <select>") => {
                self.select_combobox(target, values, timeout_ms)?
            }
            other => other?,
        };
        self.settle_for(timeout_ms);
        Ok(chosen)
    }

    /// `select` on a custom dropdown (an ARIA combobox: react-select and
    /// the like), the way a user does it: focus its input, open it
    /// (ArrowDown, or a click where that doesn't), find the option by its
    /// visible text in whatever listbox opened (often a portal at the end of
    /// `<body>`), typing to filter when it isn't listed, and click it.
    fn select_combobox(
        &self,
        target: &Target,
        values: &[String],
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let page = self.active_tab();
        let mut chosen = Vec::new();
        for value in values {
            let started = std::time::Instant::now();
            let prep = page.on_element(target, "select", cdp::page_js::COMBO_FOCUS, &[], wait, timeout)?;
            if prep.get("combobox").and_then(serde_json::Value::as_bool) != Some(true) {
                bail!(
                    "the element is {}, not a <select> or a combobox (role=combobox, aria-haspopup=listbox); \
                     click it and pick the option by ref from `ax`",
                    prep.get("tag").and_then(serde_json::Value::as_str).unwrap_or("something else")
                );
            }
            let listbox = prep.get("listbox").cloned().unwrap_or(serde_json::Value::Null);
            let find = |typed: bool| -> Result<serde_json::Value> {
                page.evaluate(
                    &format!(
                        "({})({}, {}, {}, {})",
                        cdp::page_js::COMBO_PICK,
                        serde_json::json!(value),
                        listbox,
                        serde_json::json!(typed),
                        cdp::DEEP
                    ),
                    timeout,
                )
            };
            let keys = prep.get("keys").and_then(serde_json::Value::as_bool) == Some(true);
            let mut opened = prep.get("expanded").and_then(serde_json::Value::as_bool) == Some(true);
            let (mut tried_keys, mut tried_click, mut typed) = (false, false, false);
            let mut last = std::time::Instant::now();
            let mut opened_at: Option<std::time::Instant> = opened.then(std::time::Instant::now);
            let picked = loop {
                if !opened && keys && !tried_keys {
                    // Keys go to the focused input.
                    page.press(None, "ArrowDown", wait, timeout)?;
                    tried_keys = true;
                    last = std::time::Instant::now();
                } else if !opened && !tried_click && (!keys || last.elapsed() > Duration::from_millis(300)) {
                    // A widget that ignores ArrowDown opens on a click.
                    self.click_target(target, &cdp::ClickOptions::default(), timeout_ms)?;
                    tried_click = true;
                    last = std::time::Instant::now();
                }
                let found = find(typed)?;
                opened = found.get("open").and_then(serde_json::Value::as_bool) == Some(true);
                if opened && opened_at.is_none() {
                    opened_at = Some(std::time::Instant::now());
                }
                if found.get("picked").and_then(serde_json::Value::as_bool) == Some(true) {
                    break found;
                }
                if keys && !typed && opened_at.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
                    // The open list doesn't have it: type to filter, as a
                    // user does with a long or lazily loaded list.
                    page.type_text(None, value, wait, timeout)?;
                    typed = true;
                    continue;
                }
                if started.elapsed() >= wait {
                    let seen = found.get("options").cloned().unwrap_or_default();
                    bail!(
                        "no option {value:?} in the {} (options seen: {seen})",
                        if opened { "open listbox" } else { "combobox: it didn't open on ArrowDown or a click" }
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let token = picked.get("token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            self.click_target(
                &Target::Selector(format!("[data-nv-pick=\"{token}\"]")),
                &cdp::ClickOptions::default(),
                timeout_ms,
            )
            .map_err(|e| anyhow::anyhow!("clicking option {value:?}: {e:#}"))?;
            let _ = page.evaluate(
                &format!(
                    "() => {{ for (const e of {}.all('[data-nv-pick=\"{token}\"]')) e.removeAttribute('data-nv-pick'); return true; }}",
                    cdp::DEEP
                ),
                timeout,
            );
            chosen.push(picked.get("label").cloned().unwrap_or(serde_json::Value::Null));
        }
        Ok(serde_json::Value::Array(chosen))
    }

    pub fn press(&self, target: Option<&Target>, key: &str, timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let before = self.active_tab().activity();
        // Modifiers held with `keydown` apply, as on a real keyboard.
        let held = self.held.lock().unwrap().clone();
        let chord = held
            .iter()
            .filter(|m| !key.split('+').any(|k| k.eq_ignore_ascii_case(m)))
            .map(String::as_str)
            .chain(std::iter::once(key))
            .collect::<Vec<_>>()
            .join("+");
        let may_navigate = self.active_tab().press(target, &chord, wait, timeout)?;
        if may_navigate {
            // Enter in a form submits it (implicit submission); on a link
            // it follows it.
            self.active_tab().navigation_started_since(before, NAV_GRACE);
        }
        self.settle_for(timeout_ms);
        Ok(())
    }

    pub fn type_text(&self, target: Option<&Target>, text: &str, timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().type_text(target, text, wait, timeout)?;
        self.settle_for(timeout_ms);
        Ok(())
    }

    pub fn scroll(
        &self,
        target: Option<&Target>,
        dy: Option<f64>,
        to: Option<&str>,
        timeout_ms: Option<f64>,
    ) -> Result<serde_json::Value> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().scroll(target, dy, to, wait, timeout)
    }

    pub fn upload(&self, target: &Target, files: &[String], timeout_ms: Option<f64>) -> Result<()> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        self.active_tab().upload(target, files, wait, timeout)
    }

    /// Poll `js_predicate` (an expression returning a boolean) every 100 ms
    /// until true or `timeout_ms` (default: the element wait) runs out.
    /// Survives navigations (the predicate is re-evaluated in each new
    /// document); returns the waited milliseconds.
    pub fn wait_until(&self, js_predicate: &str, what: &str, timeout_ms: Option<f64>) -> Result<f64> {
        self.wait_until_or_in_frames(js_predicate, false, what, timeout_ms)
    }

    /// [`Self::wait_until`], also satisfied when the predicate holds inside a
    /// cross-origin frame (`in_frames`: a single `--text`/`--selector` wait,
    /// for text a payment or login frame shows).
    pub fn wait_until_or_in_frames(
        &self,
        js_predicate: &str,
        in_frames: bool,
        what: &str,
        timeout_ms: Option<f64>,
    ) -> Result<f64> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let started = std::time::Instant::now();
        let mut polls = 0u32;
        loop {
            // A navigation in flight: wait for it, but never past this
            // op's own budget.
            self.settle_within(wait.saturating_sub(started.elapsed()));
            if let Ok(true) = self.active_tab().check(js_predicate, timeout) {
                return Ok((started.elapsed().as_secs_f64() * 1000.0).round());
            }
            // Frames are listed only after a miss, and every few polls.
            if in_frames && polls.is_multiple_of(3) && self.active_tab().true_in_a_frame(js_predicate, timeout) {
                return Ok((started.elapsed().as_secs_f64() * 1000.0).round());
            }
            polls += 1;
            if started.elapsed() >= wait {
                bail!("wait: {what} not met within {} ms", wait.as_millis());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// [`BrowserSession::wait_until_or_in_frames`] inside the one frame a
    /// `--frame` chain names.
    pub fn wait_until_in_frame(
        &self,
        chain: &str,
        js_predicate: &str,
        what: &str,
        timeout_ms: Option<f64>,
    ) -> Result<f64> {
        let (wait, timeout) = self.element_budget(timeout_ms);
        let started = std::time::Instant::now();
        loop {
            self.settle_within(wait.saturating_sub(started.elapsed()));
            match self.active_tab().check_in_frame(chain, js_predicate, timeout) {
                Ok(true) => return Ok((started.elapsed().as_secs_f64() * 1000.0).round()),
                Err(e) if format!("{e:#}").contains("not a frame") => return Err(e),
                _ => {}
            }
            if started.elapsed() >= wait {
                bail!("wait: {what} in frame {chain:?} not met within {} ms", wait.as_millis());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Click the first element matching `selector`.
    pub fn click(&self, selector: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.click_target(&Target::Selector(selector.to_string()), &cdp::ClickOptions::default(), timeout_ms)
            .map(|_| ())
    }

    /// Fill the first matching field with `value`.
    pub fn fill(&self, selector: &str, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.fill_target(&Target::Selector(selector.to_string()), value, timeout_ms)
    }

    /// Click the node with this `backendNodeId` (an `ax` ref).
    pub fn click_ref(&self, backend_node_id: u64, timeout_ms: Option<f64>) -> Result<()> {
        self.click_target(&Target::Ref(backend_node_id), &cdp::ClickOptions::default(), timeout_ms).map(|_| ())
    }

    /// Fill the node with this `backendNodeId` (an `ax` ref) with `value`.
    pub fn fill_ref(&self, backend_node_id: u64, value: &str, timeout_ms: Option<f64>) -> Result<()> {
        self.fill_target(&Target::Ref(backend_node_id), value, timeout_ms)
    }

    /// First element's text content for `selector`, if present.
    pub fn text(&self, selector: &str) -> Result<Option<String>> {
        self.text_with_timeout(selector, None)
    }

    /// First element's text content with an explicit timeout override.
    pub fn text_with_timeout(&self, selector: &str, timeout_ms: Option<f64>) -> Result<Option<String>> {
        self.active_tab().text_content(selector, self.timeout(timeout_ms))
    }

    /// Page title of the active tab.
    pub fn title(&self) -> Result<String> {
        self.active_tab().title(self.timeout(None))
    }

    /// Accessibility snapshot of the active tab (text tree by default).
    pub fn ax(&self, opts: &AxOptions, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        self.active_tab().ax_tree(self.timeout(timeout_ms), opts)
    }

    /// Legacy flat accessibility snapshot (`[{ref, role, name, value?}]`).
    pub fn ax_tree(&self, max_depth: Option<u32>, all: bool, timeout_ms: Option<f64>) -> Result<serde_json::Value> {
        self.ax(&AxOptions { max_depth, json: true, all, ..AxOptions::default() }, timeout_ms)
    }

    /// Viewport PNG screenshot bytes (`full_page` captures beyond viewport).
    pub fn screenshot_png(&self, full_page: bool) -> Result<Vec<u8>> {
        self.active_tab().screenshot_png(self.timeout(None), full_page)
    }

    /// True when this session drives Lightpanda rather than Chromium.
    pub fn is_lightpanda(&self) -> bool {
        self.lightpanda
    }

    /// The CDP WebSocket endpoint this session is attached to.
    pub fn endpoint(&self) -> String {
        self.browser.ws_url().to_string()
    }

    /// The active tab's own endpoint (`ws://host:port/devtools/page/<id>`),
    /// for another session to `--attach` to and work in that very tab; none
    /// over the pipe transport.
    pub fn page_endpoint(&self) -> Option<String> {
        let ws = self.endpoint();
        let (hostport, _) = ws.strip_prefix("ws://")?.split_once("/devtools/")?;
        Some(format!("ws://{hostport}/devtools/page/{}", self.active_tab().target_id()))
    }

    /// Shut the browser down. (Lightpanda is now launched directly as
    /// `lightpanda serve` and reaped like Chrome; the old shim's `--stop`
    /// cleanup step no longer applies — the real binary has no such flag.)
    pub fn close(&self) {
        // Never panic from close: a panicking shutdown turns a successful
        // session into a failed process exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // A browser navigera doesn't own keeps running: take the
            // context `reset` opened in it away first. A fork only closes
            // its own context.
            if let (true, Some(context)) = (self.attached || !self.owner, self.context.as_deref()) {
                let _ = self.browser.dispose_context(context);
                // Its routes, answers and downloads go with it.
                self.browser.shared().drop_scope(context);
            }
            if self.owner {
                self.browser.close();
            }
        }));
    }

    /// `Some(status)` once the browser process is gone.
    pub fn browser_exit(&self) -> Option<String> {
        self.browser.exit_status().map(|_| self.browser.death_report())
    }

    /// The launched executable (browser binary or `lightpanda`).
    pub fn executable(&self) -> &str {
        &self.exe
    }

    /// The user data directory of a `--profile` browser.
    pub fn profile_dir(&self) -> Option<&std::path::Path> {
        self.profile_dir.as_deref()
    }

    /// The browser binary of a `--profile` browser.
    pub fn profile_executable(&self) -> Option<&str> {
        self.profile_exe.as_deref()
    }

    /// The navigation timeout this session was given, in ms.
    pub fn timeout_ms(&self) -> f64 {
        self.timeout_ms
    }

    /// Every tab's current URL, in tab order.
    pub fn tab_urls(&self) -> Vec<String> {
        self.pages.iter().map(|p| p.url(self.timeout(None)).unwrap_or_default()).collect()
    }
}

// Re-exported for the rare caller that still names the engine explicitly.
pub use cdp::{Browser as CdpBrowser, Page as CdpPage};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_stays_on_unless_chrome_cannot_have_one() {
        assert!(!CHROME_FLAGS.contains(&"--no-sandbox"));
        assert!(!no_sandbox_wanted(false, None));
        assert!(!no_sandbox_wanted(false, Some("0")));
        assert!(!no_sandbox_wanted(false, Some("")));
        assert!(no_sandbox_wanted(true, None), "root");
        assert!(no_sandbox_wanted(false, Some("1")));
        assert!(sandbox_unavailable("[FATAL:zygote_host_impl_linux.cc(132)] No usable sandbox! If you are running"));
        assert!(sandbox_unavailable("distro that has disabled unprivileged user namespaces with AppArmor"));
        assert!(!sandbox_unavailable("net::ERR_NAME_NOT_RESOLVED"));
    }
}
