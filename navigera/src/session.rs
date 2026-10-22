//! Named background sessions: one warm browser behind a local socket,
//! driven by one shell command per op.
//!
//! Why this exists: `serve` keeps a browser warm, but only for as long as the
//! caller holds its stdin open. An AI agent driving a shell runs each tool
//! call as a separate command, so it cannot keep that pipe alive between
//! calls — which is exactly why agents end up writing Python/Node wrapper
//! scripts around `serve`. A session server owns the browser instead:
//!
//! ```text
//! navigera --session s start              # detached server, returns at once
//! navigera --session s goto --url https://example.com
//! navigera --session s ax                 # compact a11y view with refs
//! navigera --session s click --ref 12
//! navigera --session s quit               # browser + server shut down
//! ```
//!
//! The wire format is the `serve` protocol verbatim (one JSON command per
//! line, one JSON response per line), so any client that can talk to
//! `serve` can talk to a session too.
//!
//! The socket: a Unix socket on Linux/macOS. On Windows a loopback TCP
//! port whose address and a random token sit in the session file (in the
//! user's own temp dir); a client must send the token as its first line, so
//! nothing that can only reach the port (a web page posting to localhost,
//! another user) can drive the browser.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::protocol::{self, Command, Driver, SessionConfig};

use endpoint::{connect, Listener, Stream};

#[cfg(unix)]
mod endpoint {
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    pub type Stream = UnixStream;

    pub struct Listener(UnixListener);

    pub fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path)
    }

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Listener> {
            UnixListener::bind(path).map(Listener)
        }

        pub fn accept(&self) -> io::Result<Stream> {
            self.0.accept().map(|(stream, _)| stream)
        }
    }
}

#[cfg(windows)]
mod endpoint {
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::time::Duration;

    pub type Stream = TcpStream;

    pub struct Listener {
        inner: TcpListener,
        token: String,
    }

    /// The session file holds `127.0.0.1:<port> <token>`.
    fn read_session_file(path: &Path) -> io::Result<(SocketAddr, String)> {
        let text = std::fs::read_to_string(path)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed session file");
        let (addr, token) = text.trim().split_once(' ').ok_or_else(bad)?;
        Ok((addr.parse().map_err(|_| bad())?, token.to_string()))
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        let (addr, token) = read_session_file(path)?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_nodelay(true)?;
        stream.write_all(format!("{token}\n").as_bytes())?;
        Ok(stream)
    }

    /// 128 random bits as hex: std's `RandomState` keys come from the OS
    /// RNG, so SipHash under them is unpredictable to anyone else.
    fn random_token() -> String {
        use std::hash::{BuildHasher, Hasher};
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        (0..2u64)
            .map(|i| {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(i);
                h.write_u128(nanos);
                h.write_u32(std::process::id());
                format!("{:016x}", h.finish())
            })
            .collect()
    }

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Listener> {
            let inner = TcpListener::bind("127.0.0.1:0")?;
            let token = random_token();
            // Write-then-rename: a client never reads a half-written file.
            let tmp = path.with_extension("sock.tmp");
            std::fs::write(&tmp, format!("{} {token}\n", inner.local_addr()?))?;
            std::fs::rename(&tmp, path)?;
            Ok(Listener { inner, token })
        }

        /// Next client that proves it read the session file.
        pub fn accept(&self) -> io::Result<Stream> {
            let (mut stream, _) = self.inner.accept()?;
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            // Byte by byte: anything after the token line is the first
            // command and must stay in the socket for the line reader.
            let mut line = Vec::with_capacity(40);
            let mut byte = [0u8; 1];
            while line.len() <= 128 {
                if stream.read(&mut byte)? == 0 || byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            if line.strip_suffix(b"\r").unwrap_or(&line[..]) != self.token.as_bytes() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad session token"));
            }
            stream.set_read_timeout(None)?;
            Ok(stream)
        }
    }
}

/// Socket path for a session: a value containing a path separator is used
/// as-is, a bare name maps to `<session dir>/<name>.sock` (see
/// [`session_dir`]; on Windows the file holds the loopback address + token).
pub fn socket_path(name: &str) -> std::io::Result<PathBuf> {
    if name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        Ok(PathBuf::from(name))
    } else {
        Ok(session_dir()?.join(format!("{name}.sock")))
    }
}

/// Where named sessions keep their socket and log, private to this user.
///
/// A socket in a shared directory (`/tmp`) under a predictable name can be
/// squatted: another user binds it first and every command (and every value
/// `fill` types) goes to them. So on Linux/macOS the directory is
/// `$XDG_RUNTIME_DIR/navigera`, or `$TMPDIR/navigera-<uid>`, created 0700,
/// and refused if it is a symlink, not ours, or open to others. Windows'
/// `%TEMP%` is already per user.
#[cfg(unix)]
pub fn session_dir() -> std::io::Result<PathBuf> {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { geteuid() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir());
    let dir = match runtime {
        Some(base) => base.join("navigera"),
        None => std::env::temp_dir().join(format!("navigera-{uid}")),
    };
    ensure_private_dir(&dir, uid)?;
    Ok(dir)
}

/// Create `dir` 0700, or accept an existing one only if it is a real
/// directory owned by `uid` that no one else can enter.
#[cfg(unix)]
fn ensure_private_dir(dir: &std::path::Path, uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    let refuse = |why: String| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing session directory {}: {why}; remove it, or pass a socket path", dir.display()),
        ))
    };
    if !meta.file_type().is_dir() {
        return refuse("not a directory (a symlink?)".into());
    }
    if meta.uid() != uid {
        return refuse(format!("owned by uid {}, not {uid}", meta.uid()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return refuse(format!("mode {:o} lets other users in", meta.permissions().mode() & 0o777));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn session_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("navigera");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Create (truncate) a session log without following a symlink planted at
/// its path.
fn create_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux's value differs by architecture; macOS/BSD use 0x100.
        const O_NOFOLLOW: i32 = if !cfg!(target_os = "linux") {
            0x100
        } else if cfg!(any(target_arch = "aarch64", target_arch = "arm")) {
            0o100000
        } else {
            0o400000
        };
        options.custom_flags(O_NOFOLLOW).mode(0o600);
    }
    options.open(path)
}

/// `$NAVIGERA_SESSION`, if set: the default session for client calls.
pub fn env_session() -> Option<String> {
    std::env::var("NAVIGERA_SESSION").ok().filter(|s| !s.trim().is_empty())
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn print_json(output: &mut dyn Write, value: &Value, config: &SessionConfig) {
    protocol::print_cli_response(output, value, config);
}

/// Session server: launch the browser, then serve the line protocol on the
/// socket, one connection at a time, until `quit` or the idle timeout.
pub fn serve_socket(config: &SessionConfig, name: &str) -> ExitCode {
    let path = match socket_path(name) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("navigera: {e}");
            return ExitCode::from(1);
        }
    };
    if connect(&path).is_ok() {
        eprintln!("navigera: a session is already listening on {}", path.display());
        return ExitCode::from(1);
    }
    // A socket file nobody answers on is left over from a crashed server.
    let _ = std::fs::remove_file(&path);

    let mut driver = match Driver::launch(config) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("navigera: {e:#}");
            return ExitCode::from(1);
        }
    };
    let listener = match Listener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("navigera: bind {}: {e}", path.display());
            driver.session().close();
            return ExitCode::from(1);
        }
    };
    eprintln!("navigera: session listening on {} ({})", path.display(), driver.session().endpoint());

    let last_activity = Arc::new(AtomicU64::new(now_s()));
    if config.idle_timeout_s > 0 {
        spawn_idle_watchdog(path.clone(), Arc::clone(&last_activity), config.idle_timeout_s);
    }

    loop {
        let Ok(stream) = listener.accept() else { continue };
        last_activity.store(now_s(), Ordering::Relaxed);
        if serve_connection(&mut driver, stream, &last_activity) {
            break;
        }
    }
    driver.session().close();
    crate::timing::report();
    // Log first: a client that finds the socket gone quotes this line.
    eprintln!("navigera: session {} shut down", path.display());
    let _ = std::fs::remove_file(&path);
    ExitCode::SUCCESS
}

/// Serve one client connection; returns true once a `quit` was answered.
fn serve_connection(driver: &mut Driver, stream: Stream, last: &AtomicU64) -> bool {
    let reader = match stream.try_clone() {
        Ok(read_half) => BufReader::new(read_half),
        Err(_) => return false,
    };
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(raw) = line else { return false };
        last.store(now_s(), Ordering::Relaxed);
        let Some((responses, is_quit)) = protocol::handle_line(driver, &raw) else {
            continue;
        };
        let written = responses.iter().try_for_each(|response| protocol::write_response(&mut writer, response, false));
        last.store(now_s(), Ordering::Relaxed);
        if is_quit {
            return true;
        }
        if written.is_err() {
            return false;
        }
    }
    false
}

/// After `idle_s` without traffic, send ourselves a `quit` over the socket so
/// the browser shuts down through the normal path.
fn spawn_idle_watchdog(path: PathBuf, last: Arc<AtomicU64>, idle_s: u64) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(idle_s.clamp(1, 5)));
        if now_s().saturating_sub(last.load(Ordering::Relaxed)) < idle_s {
            continue;
        }
        eprintln!("navigera: idle for {idle_s}s, shutting the session down");
        if let Ok(mut stream) = connect(&path) {
            let _ = stream.write_all(b"{\"op\":\"quit\"}\n");
            let _ = stream.flush();
            let mut reply = String::new();
            let _ = BufReader::new(stream).read_line(&mut reply);
        }
        break;
    });
}

/// Client: send one command to the session server and print its response.
pub fn client(config: &SessionConfig, name: &str, command: &Command, output: &mut dyn Write) -> ExitCode {
    let path = match socket_path(name) {
        Ok(path) => path,
        Err(e) => return fail(output, config, e.to_string()),
    };
    let stream = match connect(&path) {
        Ok(stream) => stream,
        Err(e) => {
            let mut error = format!(
                "no navigera session at {} ({e}); start one with `navigera --session {name} start`",
                path.display()
            );
            // A session that ran before says why it stopped (idle timeout,
            // browser crash, quit) in its log, which outlives the socket.
            let log_path = path.with_extension("log");
            let tail = log_tail(&log_path, 5);
            if !tail.is_empty() {
                error.push_str(&format!("\nlast lines of {}:\n{tail}", log_path.display()));
            }
            print_json(output, &json!({ "id": null, "ok": false, "error": error }), config);
            return ExitCode::from(1);
        }
    };
    let mut request = match serde_json::to_value(command) {
        Ok(value) => value,
        Err(e) => {
            print_json(output, &json!({ "id": null, "ok": false, "error": format!("encode command: {e}") }), config);
            return ExitCode::from(1);
        }
    };
    request["id"] = json!(1);
    localize_request(&mut request, config);
    let mut writer = &stream;
    // One write: on Windows (TCP) a split line would wait out Nagle.
    if writer.write_all(format!("{request}\n").as_bytes()).and_then(|_| writer.flush()).is_err() {
        print_json(output, &json!({ "id": null, "ok": false, "error": "session closed the connection" }), config);
        return ExitCode::from(1);
    }
    // Done sending: the server sees EOF after answering and takes the next
    // client, so a crashed client can't wedge the session.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line);
    let response: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(_) => json!({
            "id": null,
            "ok": false,
            "error": "session ended without a response (see its log next to the socket)",
        }),
    };
    if matches!(command, Command::Quit { .. }) {
        // Return only once the server has let go of the socket, so a `start`
        // right after `quit` gets a fresh session, not the dying one.
        let deadline = Instant::now() + Duration::from_secs(10);
        while path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if protocol::print_cli_response(output, &response, config) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The session server runs in another directory (wherever `start` ran), so
/// file paths a client passes are made absolute in the client's own working
/// directory first: `upload ./doc.txt` and `screenshot shot.png` mean the
/// caller's files. A global `--timeout-ms` given to a client call applies to
/// that op (the server's own default was fixed at `start`).
fn localize_request(request: &mut Value, config: &SessionConfig) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let absolute = |p: &str| -> String {
        let path = std::path::Path::new(p);
        if path.is_absolute() {
            p.to_string()
        } else {
            cwd.join(path).display().to_string()
        }
    };
    match request.get("op").and_then(Value::as_str) {
        Some("upload") => {
            if let Some(files) = request.get_mut("files").and_then(Value::as_array_mut) {
                for f in files.iter_mut() {
                    if let Some(p) = f.as_str() {
                        *f = json!(absolute(p));
                    }
                }
            }
        }
        Some("screenshot") => {
            let path = match request.get("path").and_then(Value::as_str) {
                Some(p) => absolute(p),
                None => absolute(&format!(
                    "shot-{}.png",
                    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
                )),
            };
            request["path"] = json!(path);
        }
        _ => {}
    }
    // Files the server writes for the caller: relative to the caller.
    for key in ["har", "body_file", "path"] {
        if let Some(p) = request.get(key).and_then(Value::as_str) {
            request[key] = json!(absolute(p));
        }
    }
    if request.get("timeout_ms").is_none() && config.timeout_ms != crate::protocol::DEFAULT_TIMEOUT_MS {
        request["timeout_ms"] = json!(config.timeout_ms);
    }
}

/// The last `n` lines of a session log ("" when there is none).
fn log_tail(path: &std::path::Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The engine a running session server reports (`status`), if it answers.
fn running_engine(stream: Stream) -> Option<String> {
    let mut writer = &stream;
    writer.write_all(b"{\"id\":1,\"op\":\"status\"}\n").ok()?;
    writer.flush().ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let response: Value = serde_json::from_str(line.trim()).ok()?;
    response.pointer("/result/engine").and_then(Value::as_str).map(str::to_string)
}

fn fail(output: &mut dyn Write, config: &SessionConfig, error: String) -> ExitCode {
    print_json(output, &json!({ "id": null, "ok": false, "error": error }), config);
    ExitCode::from(1)
}

/// What `start` tells an agent when a decision model is on.
pub const DECIDER_NOTE: &str = "a decision model is on: act with `click --find \"<what>\"` or \
     `fill --find \"<what>\" <value>` (any element command takes --find) and verify with \
     `check \"<yes/no question>\"`, instead of reading `ax` for refs; read `ax` only for text you must report, \
     or when --find says it is unsure.";

/// `start`: spawn a detached session server and wait until it accepts.
pub fn start(config: &SessionConfig, name: &str, output: &mut dyn Write) -> ExitCode {
    let path = match socket_path(name) {
        Ok(path) => path,
        Err(e) => return fail(output, config, e.to_string()),
    };
    let log_path = path.with_extension("log");
    let (socket_str, log_str) = (path.display().to_string(), log_path.display().to_string());
    if let Ok(stream) = connect(&path) {
        // The running session must be the browser this `start` asks for: a
        // Chrome command sent to a Lightpanda session left over from another
        // run would quietly test the wrong engine.
        let running = running_engine(stream);
        let wanted = if crate::browser::is_lightpanda(&config.engine) { "lightpanda" } else { "chrome" };
        if let Some(running) = running.filter(|r| r != wanted) {
            return fail(
                output,
                config,
                format!(
                    "session {name} is already running the {running} engine, not {wanted}; `navigera --session {name} quit` first, or pick another --session"
                ),
            );
        }
        print_json(
            output,
            &json!({ "id": null, "ok": true, "result": {
                "session": name, "socket": socket_str, "log": log_str, "already_running": true,
            }}),
            config,
        );
        return ExitCode::SUCCESS;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return fail(output, config, format!("locate navigera binary: {e}")),
    };
    let log = match create_log(&log_path) {
        Ok(file) => file,
        Err(e) => return fail(output, config, format!("create {}: {e}", log_path.display())),
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--session")
        .arg(name)
        .arg("--engine")
        .arg(&config.engine)
        .arg("--timeout-ms")
        .arg(config.timeout_ms.to_string())
        .arg("--idle-timeout-s")
        .arg(config.idle_timeout_s.to_string());
    if config.headed {
        cmd.arg("--headed");
    }
    if let Some(chromium) = &config.chromium {
        cmd.arg("--chromium").arg(chromium);
    }
    if let Some(transport) = &config.transport {
        cmd.arg("--transport").arg(transport);
    }
    if let Some(spec) = &config.attach {
        cmd.arg(format!("--attach={spec}"));
    }
    if let Some(name) = &config.profile {
        cmd.arg("--profile").arg(name);
    }
    if config.headless {
        cmd.arg("--headless");
    }
    if let Some(from) = &config.from {
        cmd.arg("--from").arg(from);
    }
    if let Some(from_profile) = &config.from_profile {
        cmd.arg("--from-profile").arg(from_profile);
    }
    if config.natural {
        cmd.arg("--natural");
    }
    cmd.arg("serve").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log));
    let mut child = match crate::proc::spawn_detached(&mut cmd) {
        Ok(child) => child,
        Err(e) => return fail(output, config, format!("spawn session server: {e}")),
    };

    // Attaching to the user's Chrome waits for them to click Allow.
    let wait = if config.attach.is_some() { 180 } else { 60 };
    if config.attach.is_some() {
        eprintln!("navigera: connecting to your browser; if Chrome asks, click Allow");
    }
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        if connect(&path).is_ok() {
            let result = json!({ "session": name, "socket": socket_str, "log": log_str, "pid": child.id() });
            // The server inherited this environment. Saying so lets an agent
            // know `find`/`check` work without reading a key variable (and
            // printing the key into its own context).
            let mut response = json!({ "id": null, "ok": true, "result": result });
            if let Some(decider) = crate::decide::Decider::from_env() {
                response["result"]["decider"] = json!(decider.model());
                // The contract, where the agent decides how to proceed: a
                // skill section further down is read once, if at all.
                response["notes"] = json!([DECIDER_NOTE]);
            }
            print_json(output, &response, config);
            return ExitCode::SUCCESS;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = log_tail(&log_path, 20);
            return fail(output, config, format!("session server exited during startup ({status}):\n{tail}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return fail(
                output,
                config,
                format!("session server did not listen within {wait}s; see {}", log_path.display()),
            );
        }
        // A failed connect costs microseconds, and the server is usually up
        // within a few hundred ms: checking every 5 ms keeps `start` from
        // waiting out a 50 ms step after it is ready. Slower after that.
        let step = if Instant::now() + Duration::from_secs(wait - 3) < deadline { 5 } else { 50 };
        std::thread::sleep(Duration::from_millis(step));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn session_dir_is_created_private_and_refused_when_not() {
        let base = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(base.path()).unwrap().uid();

        let fresh = base.path().join("fresh");
        ensure_private_dir(&fresh, uid).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
        ensure_private_dir(&fresh, uid).expect("an existing private dir is fine");

        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_private_dir(&open, uid).unwrap_err().to_string();
        assert!(err.contains("lets other users in"), "{err}");

        let link = base.path().join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        let err = ensure_private_dir(&link, uid).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");

        let err = ensure_private_dir(&fresh, uid + 1).unwrap_err().to_string();
        assert!(err.contains("owned by uid"), "{err}");
    }

    #[test]
    fn a_log_path_that_is_a_symlink_is_not_followed() {
        let base = tempfile::tempdir().unwrap();
        let victim = base.path().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let log = base.path().join("s.log");
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        assert!(create_log(&log).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        let plain = base.path().join("plain.log");
        create_log(&plain).unwrap();
        assert_eq!(std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
