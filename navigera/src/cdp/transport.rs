//! Chrome process launch and DevTools endpoint discovery.
//!
//! We launch Chrome headless with `--remote-debugging-port=0` (Chrome picks a
//! free port and prints the chosen URL) and discover that URL by reading
//! Chrome's stderr line `DevTools listening on ws://…` — the same approach
//! chromiumoxide and zendriver-rs use.
//!
//! That stderr line is emitted by Chrome as soon as the browser-level DevTools
//! WebSocket is accepting connections, so it is fast and race-free. It replaces
//! the previous two-step discovery — read the `DevToolsActivePort` file, then
//! poll the HTTP `/json/version` endpoint — whose HTTP server the browser is
//! not always ready to serve (it timed out on newer Chrome builds in CI), and
//! whose failure message was drowned out by Chrome's own DBus noise on stderr.
//!
//! Chrome's stderr is captured on a piped handle drained by a background
//! thread, so child-process DBus noise never reaches `navigera`'s stderr
//! (where the protocol lives and where launch errors are reported). That is what
//! makes a failed launch stop being "silent": the only thing on our stderr is
//! `navigera`'s own message, with a captured stderr tail attached.
//!
//! If a browser/engine doesn't announce the URL on stderr, we fall back to the
//! classic port-file + HTTP poll below.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// How long a browser has to announce its DevTools URL after spawn.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// The stderr line Chrome prints once the browser DevTools WebSocket is live.
const DEVTOOLS_LISTENING: &str = "DevTools listening on ws://";

/// Bytes of captured stderr retained for launch-failure messages.
const STDERR_TAIL_BYTES: usize = 2000;

/// Maximum stderr bytes retained in memory while capturing during discovery.
const STDERR_BUF_CAP: usize = 64 * 1024;

pub struct LaunchedChrome {
    pub child: Child,
    pub profile_dir: tempfile::TempDir,
    pub ws_url: String,
    /// Windows: the kill-on-close job holding the browser (see `procjob`).
    pub job: Option<super::procjob::ProcJob>,
}

/// Guard that reaps/terminates the spawned browser on drop unless disarmed.
/// Ensures a failed launch never leaks a Chrome process (whose DevTools stderr
/// we captured) — a failed `target.createTarget`-style discovery previously
/// left browser processes running.
struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn as_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("guard disarmed")
    }
    /// Take ownership back (e.g. to hand the child to the session) and stop
    /// killing on drop.
    fn disarm(mut self) -> Child {
        self.0.take().expect("guard disarmed")
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Launch `exe` headless (or headed) with a fresh profile; return the child,
/// the profile dir (deleted on drop), and the browser-level WS debugger URL.
pub fn launch_chrome(
    exe: &str,
    headless: bool,
    chrome_flags: &[String],
    debugging_port: Option<u16>,
) -> Result<LaunchedChrome> {
    let profile_dir = super::profile::temp_profile()?;

    let mut cmd = Command::new(exe);
    if headless {
        cmd.arg("--headless=new");
    }
    // `=0` lets Chrome pick a free port; we read it back from its stderr line. A
    // fixed port is only used by engines whose launcher needs it up front (e.g.
    // the lightpanda shim), in which case we still learn the URL from the stderr
    // line when the engine supports it.
    if let Some(port) = debugging_port {
        cmd.arg(format!("--remote-debugging-port={port}"));
    } else {
        cmd.arg("--remote-debugging-port=0");
    }
    cmd.arg(format!("--user-data-dir={}", profile_dir.path().display()))
        // No start URL: with `--no-startup-window` (see BrowserSession) Chrome
        // opens no initial tab, so the only renderer started is the session's own
        // page instead of an extra, never-used about:blank tab.
        .args(chrome_flags)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Pipe stderr: scanned for the DevTools URL, captured for diagnostics, and
        // kept off our inherited stderr so Chrome child-process noise can't mask the
        // `navigera` error reporting that lives on our stderr.
        .stderr(Stdio::piped());

    own_process_group(&mut cmd);
    let mut child = cmd.spawn().context("spawn chrome")?;
    super::profile::record_browser(&profile_dir, child.id());
    // Before Chrome starts its own children, so they land in the job too.
    let job = super::procjob::ProcJob::contain(&child);
    let stderr = child.stderr.take().context("chrome was spawned without a stderr pipe")?;
    let mut kill = KillOnDrop(Some(child));
    let capture = Capture::start(stderr);

    let deadline = Instant::now() + DISCOVERY_TIMEOUT;

    // Phase 1: scan Chrome's stderr for `DevTools listening on ws://…`. Fast and
    // reliable on Chromium/Chrome/Edge (>= M109).
    let ws_url = loop {
        // Block on the capture thread's announcement (wakes the moment the
        // line arrives) instead of sleep-polling, which cost up to 20 ms of
        // every launch.
        if let Some(url) = capture.wait_url(Duration::from_millis(20)) {
            break Some(url);
        }
        match kill.as_mut().try_wait() {
            // Real failure: Chrome exited non-zero during launch.
            Ok(Some(status)) if !status.success() => {
                thread::sleep(Duration::from_millis(20));
                let _ = kill.disarm().wait(); // already exited; avoid double-kill noise
                return Err(anyhow!(
                    "chrome exited during launch (status {status}); captured stderr:\n{}",
                    capture.tail()
                ));
            }
            // The launcher stub exited cleanly (e.g. the Edge stub) while the
            // real browser runs on as a grandchild. Stop scanning stderr and
            // fall through to the port-file/HTTP fallback.
            Ok(Some(_)) => break None,
            _ => {}
        }
        if Instant::now() >= deadline {
            break None;
        }
    };

    let ws_url = match ws_url {
        Some(url) => url,
        None => {
            // Phase 2 (fallback): engines/browsers that don't print the stderr
            // line — recover the URL from the DevToolsActivePort file plus an
            // HTTP `/json/version` poll. Both tolerate a launcher-stub exit.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let port = read_devtools_port(&profile_dir, kill.as_mut())?;
            poll_ws_url(port, remaining, kill.as_mut())?
        }
    };

    // Keep draining the stderr pipe for the lifetime of the browser so Chrome
    // never blocks on a full pipe buffer; the thread ends itself on Chrome exit.
    capture.detach();

    Ok(LaunchedChrome { child: kill.disarm(), profile_dir, ws_url, job })
}

/// A browser launched with `--remote-debugging-pipe`: CDP runs over two
/// anonymous pipes (Chrome reads commands on fd 3, writes replies on fd 4;
/// messages are NUL-terminated JSON), the transport Playwright uses.
///
/// Compared with `--remote-debugging-port`: no DevTools HTTP/WebSocket server
/// to start and no handshake, and — more important for agents on shared
/// machines — no TCP port any other local process could attach to.
#[cfg(unix)]
pub struct LaunchedPipe {
    pub child: Child,
    pub profile_dir: tempfile::TempDir,
    /// Our end of Chrome's fd 4 (replies and events).
    pub from_browser: std::os::fd::OwnedFd,
    /// Our end of Chrome's fd 3 (commands).
    pub to_browser: std::os::fd::OwnedFd,
    /// Chrome's stderr, drained for its lifetime; the bounded tail explains
    /// a browser that dies at launch or mid-session.
    pub stderr: StderrTail,
}

/// Bounded tail of a browser's stderr, kept for error messages.
/// (Only the Unix pipe launch fills one.)
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct StderrTail(Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>);

impl StderrTail {
    /// A tail of `text` whose stream has already ended.
    #[cfg(test)]
    fn of(text: &str) -> Self {
        StderrTail(Arc::new(Mutex::new(text.as_bytes().to_vec())), Arc::new(AtomicBool::new(true)))
    }

    /// Whether the stream has ended: the browser and every helper that
    /// shared its stderr are gone, and every line they wrote is in the tail.
    pub fn closed(&self) -> bool {
        self.1.load(Ordering::Acquire)
    }

    /// The last lines of stderr, minus known harmless noise (DBus, fonts).
    pub fn tail(&self) -> String {
        let b = self.0.lock().map(|b| b.clone()).unwrap_or_default();
        let text = String::from_utf8_lossy(&b);
        let lines: Vec<&str> =
            text.lines().filter(|l| !l.is_empty() && !l.contains("dbus") && !l.contains("Fontconfig")).collect();
        let joined = lines[lines.len().saturating_sub(6)..].join("\n");
        let start = joined.len().saturating_sub(800);
        joined[joined.ceil_char_boundary(start)..].to_string()
    }

    /// The first fatal error Chrome logged (`[pid:tid:time:FATAL:file.cc:N]
    /// message`): the reason it died, which the stack dump that follows a
    /// crash pushes out of `tail`.
    pub fn first_fatal(&self) -> Option<String> {
        let b = self.0.lock().ok()?;
        let text = String::from_utf8_lossy(&b);
        let line = text.lines().find(|l| l.contains(":FATAL:"))?;
        Some(line.chars().take(400).collect())
    }
}

#[cfg(unix)]
pub fn launch_chrome_pipe(exe: &str, headless: bool, chrome_flags: &[String]) -> Result<LaunchedPipe> {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    // Linux and macOS value; the temporaries close at exec, only the
    // dup2'd 3 and 4 (which never carry O_CLOEXEC) reach Chrome.
    const F_DUPFD_CLOEXEC: i32 = if cfg!(target_os = "linux") { 1030 } else { 67 };

    let profile_dir = super::profile::temp_profile()?;
    // Chrome reads commands from fd 3 and writes to fd 4.
    let (cmd_read, cmd_write) = std::io::pipe().context("pipe for CDP commands")?;
    let (reply_read, reply_write) = std::io::pipe().context("pipe for CDP replies")?;
    let child_in = cmd_read.as_raw_fd();
    let child_out = reply_write.as_raw_fd();

    let mut cmd = Command::new(exe);
    if headless {
        cmd.arg("--headless=new");
    }
    cmd.arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", profile_dir.path().display()))
        .args(chrome_flags)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Drained by a thread (and copied to $NAVIGERA_CHROME_LOG): the tail goes
        // into the error when the browser dies.
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls (fcntl/dup2) between fork and exec.
    // Move both ends above fd 10 first so dup2 onto 3/4 can't clobber one
    // with the other; dup2'd fds don't inherit O_CLOEXEC, so 3 and 4 survive
    // exec while every other pipe end (CLOEXEC from std::io::pipe) closes.
    unsafe {
        cmd.pre_exec(move || {
            let a = fcntl(child_in, F_DUPFD_CLOEXEC, 10);
            let b = fcntl(child_out, F_DUPFD_CLOEXEC, 10);
            if a < 0 || b < 0 || dup2(a, 3) < 0 || dup2(b, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    own_process_group(&mut cmd);
    let mut child = cmd.spawn().with_context(|| format!("spawn {exe} --remote-debugging-pipe"))?;
    super::profile::record_browser(&profile_dir, child.id());
    // The child's ends live on in Chrome; close ours so EOF propagates.
    drop(cmd_read);
    drop(reply_write);
    let stderr = child
        .stderr
        .take()
        .map(|e| Capture::start(e).tail_handle())
        .unwrap_or_else(|| StderrTail(Arc::default(), Arc::new(AtomicBool::new(true))));
    Ok(LaunchedPipe {
        child,
        profile_dir,
        from_browser: OwnedFd::from(reply_read),
        to_browser: OwnedFd::from(cmd_write),
        stderr,
    })
}

/// Unix: the browser leads a process group of its own, which every helper
/// it starts (zygotes, renderers, GPU, network and storage services)
/// inherits, so `close` kills the whole tree with one signal. Killing only
/// the browser process left the network and storage services alive for a
/// few ms, writing `Default/Reporting and NEL` and friends into a profile
/// that was being deleted (3 leaked profile dirs in 24 local cold runs).
fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// `NAVIGERA_CHROME_LOG=<file>`: append the browser's own stderr there (crash
/// reasons, sandbox errors) for diagnosing a browser that died mid-session.
fn chrome_log() -> Option<std::fs::File> {
    let path = std::env::var_os("NAVIGERA_CHROME_LOG")?;
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

/// The captured stderr line carrying the browser DevTools WebSocket URL, if any.
fn extract_devtools_url(line: &str) -> Option<String> {
    let start = line.find(DEVTOOLS_LISTENING)?;
    let rest = &line[start + DEVTOOLS_LISTENING.len()..];
    rest.split_whitespace().next().map(|url| format!("ws://{url}"))
}

/// Background drain of a child's stderr: records the DevTools URL the moment
/// Chrome prints it, and retains a bounded tail of stderr for diagnostics while
/// discovery is in progress.
struct Capture {
    url: std::sync::mpsc::Receiver<String>,
    buf: Arc<Mutex<Vec<u8>>>,
    capturing: Arc<AtomicBool>,
    #[cfg_attr(not(unix), allow(dead_code))]
    closed: Arc<AtomicBool>,
}

impl Capture {
    fn start(stderr: ChildStderr) -> Self {
        let (url_tx, url) = std::sync::mpsc::channel();
        let buf = Arc::new(Mutex::new(Vec::new()));
        let capturing = Arc::new(AtomicBool::new(true));
        let closed = Arc::new(AtomicBool::new(false));
        let buf_clone = Arc::clone(&buf);
        let capturing_clone = Arc::clone(&capturing);
        let closed_clone = Arc::clone(&closed);

        thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr).lines();
            let mut announced = false;
            let mut log = chrome_log();
            for line in reader.map_while(Result::ok) {
                if let Some(f) = log.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                if !announced {
                    if let Some(u) = extract_devtools_url(&line) {
                        announced = true;
                        let _ = url_tx.send(u);
                    }
                }
                if capturing_clone.load(Ordering::Relaxed) {
                    let mut b = match buf_clone.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    b.extend_from_slice(line.as_bytes());
                    b.push(b'\n');
                    if b.len() > STDERR_BUF_CAP {
                        // Keep only the most recent half to bound memory.
                        let drain = b.len() - STDERR_BUF_CAP / 2;
                        b.drain(..drain);
                    }
                }
            }
            closed_clone.store(true, Ordering::Release);
        });

        Self { url, buf, capturing, closed }
    }

    /// The tail as a browser keeps it, with the end-of-stream flag.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn tail_handle(&self) -> StderrTail {
        StderrTail(Arc::clone(&self.buf), Arc::clone(&self.closed))
    }

    /// Wait up to `timeout` for Chrome to announce its DevTools URL.
    fn wait_url(&self, timeout: Duration) -> Option<String> {
        match self.url.recv_timeout(timeout) {
            Ok(url) => Some(url),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            // stderr closed without the line: don't spin while the caller
            // checks the child's exit status.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(timeout);
                None
            }
        }
    }

    /// Last `STDERR_TAIL_BYTES` chars of captured stderr (best-effort).
    fn tail(&self) -> String {
        let b = self.buf.lock().expect("capture lock poisoned");
        let start = b.len().saturating_sub(STDERR_TAIL_BYTES);
        String::from_utf8_lossy(&b[start..]).into_owned()
    }

    /// Detach the drain thread: keep reading Chrome's stderr for the browser's
    /// lifetime so the pipe never back-pressures Chrome. Stop retaining stderr
    /// so the captured tail is bounded to the discovery window.
    fn detach(self) {
        self.capturing.store(false, Ordering::Relaxed);
    }
}

/// Read the DevTools port Chrome chose (from the DevToolsActivePort file).
///
/// Tolerates a launcher stub (e.g. Edge's) exiting `0`: the real browser keeps
/// running as a grandchild and still writes this file, so an early stub exit is
/// not treated as a launch failure here.
fn read_devtools_port(profile_dir: &tempfile::TempDir, child: &mut Child) -> Result<u16> {
    let port_file = profile_dir.path().join("DevToolsActivePort");
    let start = Instant::now();
    loop {
        if let Ok(content) = std::fs::read_to_string(&port_file) {
            if let Some(line) = content.lines().next() {
                if let Ok(port) = line.trim().parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                anyhow::bail!("chrome exited during launch (status {status})");
            }
            _ => {}
        }
        if start.elapsed() > DISCOVERY_TIMEOUT {
            anyhow::bail!("timed out waiting for DevToolsActivePort file");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Poll the DevTools HTTP server (`/json/version`) for the browser-level
/// WebSocket URL, without a launcher child to watch (for servers like
/// `lightpanda serve` that we spawn and manage directly).
pub fn poll_ws_url_standalone(port: u16, timeout: Duration) -> Result<String> {
    let start = Instant::now();
    let mut logged = false;
    loop {
        if !logged && start.elapsed() > Duration::from_secs(5) {
            eprintln!("[transport] still waiting for /json/version on 127.0.0.1:{port} ...");
            logged = true;
        }
        match http_get_body("127.0.0.1", port, "/json/version") {
            Ok(body) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(url) = v.get("webSocketDebuggerUrl").and_then(|u| u.as_str()) {
                        return Ok(url.to_string());
                    }
                }
            }
            Err(e) => {
                if start.elapsed() > timeout {
                    anyhow::bail!("lightpanda serve did not expose /json/version on 127.0.0.1:{port}: {e:#}");
                }
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("timed out waiting for /json/version on 127.0.0.1:{port}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Poll the DevTools HTTP server (`/json/version`) for the browser-level
/// WebSocket URL. Tolerates a launcher-stub exit like [`read_devtools_port`].
fn poll_ws_url(port: u16, timeout: Duration, child: &mut Child) -> Result<String> {
    let start = Instant::now();
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                anyhow::bail!("chrome exited during launch (status {status}) after {attempts} polls");
            }
            _ => {}
        }
        match http_get_body("127.0.0.1", port, "/json/version") {
            Ok(body) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(url) = v.get("webSocketDebuggerUrl").and_then(|u| u.as_str()) {
                        return Ok(url.to_string());
                    }
                }
            }
            Err(e) => {
                if attempts.is_multiple_of(40) {
                    eprintln!("[browser] /json/version poll {attempts}: {e:#}");
                }
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("timed out waiting for chrome DevTools on port {port} after {attempts} polls");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Minimal blocking HTTP GET; returns the response body.
///
/// Stops as soon as the body is complete (Content-Length or a chunked
/// terminator) instead of reading to EOF: keep-alive servers such as
/// `lightpanda serve` answer `/json/version` but never close the socket, so
/// a read-to-EOF client sat in `read` until its 5 s timeout (EAGAIN) on
/// every poll and never saw the URL.
pub(crate) fn http_get_body(host: &str, port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // NB: Chrome's DevTools HTTP server rejects HTTP/1.0 outright.
    // With the port: Chrome builds the WebSocket URL it returns from it.
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(body) = complete_body(&buf) {
            return Ok(String::from_utf8_lossy(&body).into_owned());
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    // EOF: whatever follows the headers is the body.
    let resp = String::from_utf8_lossy(&buf);
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .or_else(|| resp.split_once("\n\n").map(|(_, b)| b))
        .ok_or_else(|| anyhow::anyhow!("malformed http response"))?;
    Ok(body.to_string())
}

/// The response body once `buf` holds all of it (by Content-Length or
/// chunked encoding); `None` while more bytes are needed or the framing is
/// unknown (then the caller reads to EOF).
pub(crate) fn complete_body(buf: &[u8]) -> Option<Vec<u8>> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let body = &buf[header_end + 4..];
    let header = |name: &str| head.lines().find_map(|line| line.strip_prefix(name)).map(|v| v.trim().to_string());
    if let Some(len) = header("content-length:").and_then(|v| v.parse::<usize>().ok()) {
        return (body.len() >= len).then(|| body[..len].to_vec());
    }
    if header("transfer-encoding:").is_some_and(|v| v.contains("chunked")) {
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let line_end = at + body.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
            let size_hex = String::from_utf8_lossy(&body[at..line_end]);
            let size = usize::from_str_radix(size_hex.split(';').next()?.trim(), 16).ok()?;
            let data = line_end + 2;
            if size == 0 {
                return Some(out);
            }
            out.extend_from_slice(body.get(data..data + size)?);
            at = data + size + 2;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crash_keeps_its_fatal_line_past_the_stack_dump() {
        let mut log = String::from("[1:1:1011/030707.3:FATAL:zygote_host_impl_linux.cc:130] No usable sandbox! ...\n");
        log.push_str("Received signal 6\n");
        for i in 0..30 {
            log.push_str(&format!(" r{i}: 0000000000000006 r13: 0000235000148000\n"));
        }
        log.push_str("[end of stack trace]\n");
        let stderr = StderrTail::of(&log);
        assert!(!stderr.tail().contains("No usable sandbox"));
        assert!(stderr.first_fatal().is_some_and(|f| f.contains("No usable sandbox")));
        assert_eq!(StderrTail::of("").first_fatal(), None);
    }

    use super::*;

    #[test]
    fn extracts_devtools_url_from_chrome_stderr() {
        let line = "DevTools listening on ws://127.0.0.1:65408/devtools/browser/abc-123";
        assert_eq!(extract_devtools_url(line), Some("ws://127.0.0.1:65408/devtools/browser/abc-123".to_string()));
    }

    #[test]
    fn http_body_completes_without_eof() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(complete_body(resp).as_deref(), Some(&b"hello"[..]));
        assert_eq!(complete_body(&resp[..resp.len() - 1]), None, "needs all 5 bytes");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(complete_body(chunked).as_deref(), Some(&b"abcde"[..]));
        assert_eq!(complete_body(b"HTTP/1.1 200 OK\r\n\r\nbody"), None, "unframed: read to EOF");
    }

    /// A keep-alive server that answers but never closes (lightpanda's
    /// `/json/version`) must not stall the poll until the read timeout.
    #[test]
    fn http_get_returns_before_keep_alive_server_closes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req);
            let body = r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:1/x"}"#;
            write!(sock, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            std::thread::sleep(Duration::from_secs(3)); // hold the socket open
        });
        let started = Instant::now();
        let body = http_get_body("127.0.0.1", port, "/json/version").unwrap();
        assert!(body.contains("webSocketDebuggerUrl"), "{body}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        server.join().unwrap();
    }

    #[test]
    fn ignores_non_devtools_stderr() {
        assert_eq!(extract_devtools_url("[0123/456789.123456:ERROR:chrome.cpp] noise"), None);
    }
}
