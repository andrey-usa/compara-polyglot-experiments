//! Minimal async CDP JSON-RPC client over a single websocket.
//!
//! One background task owns both halves of the socket: it forwards queued
//! outgoing messages and dispatches incoming ones — `{id, result|error}` to
//! the pending caller, everything else to the event broadcast. `send` is a
//! plain request/response with a timeout; `subscribe` + `wait_event` cover
//! the few events we need (page load).

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

type Responder = oneshot::Sender<Result<Value, String>>;

/// What Chrome answers a command sent to a tab that is gone, and what a
/// command still waiting when its tab's session ends fails with.
pub const SESSION_GONE: &str = "Session with given id not found";

/// A command waiting for its answer, and the tab session it went to.
type Pending = HashMap<u64, (Option<String>, Responder)>;

pub struct CdpClient {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    /// Shared with the pump task so responses dispatched from the read loop
    /// reach the `send` caller: every `send` inserts its responder here and the
    /// pump removes it in `dispatch`.
    pending: Arc<Mutex<Pending>>,
    /// Outgoing JSON texts for the pump task. `shutdown` takes it, which
    /// makes the pump's `rx.recv()` return `None` and the task exits,
    /// closing the websocket.
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    events: broadcast::Sender<Value>,
    /// Events broadcast so far: a subscriber that has handled this many has
    /// seen everything that arrived before the current point.
    events_sent: Arc<AtomicU64>,
    /// Set once the transport is gone (browser exited, pipe/socket closed):
    /// `send` then fails at once instead of waiting out its timeout.
    closed: Arc<AtomicBool>,
}

/// Mark the connection closed and fail every request still waiting.
fn close_all(closed: &AtomicBool, pending: &Mutex<Pending>) {
    closed.store(true, Ordering::SeqCst);
    let mut pending = pending.lock().unwrap();
    for (_, (_, responder)) in pending.drain() {
        let _ = responder.send(Err("CDP connection closed".to_string()));
    }
}

impl Clone for CdpClient {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl CdpClient {
    /// Connect to the browser-level debugger URL and start the pump task.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        // Bound the connect so a stale/unreachable DevTools URL fails in 15s
        // instead of hanging the CLI forever.
        Self::connect_within(ws_url, Duration::from_secs(15)).await
    }

    /// [`CdpClient::connect`] with its own bound: attaching to a user's
    /// Chrome waits while Chrome asks them to allow the connection.
    pub async fn connect_within(ws_url: &str, bound: Duration) -> Result<Self> {
        let (ws, _) = tokio::time::timeout(bound, tokio_tungstenite::connect_async(ws_url))
            .await
            .map_err(|_| anyhow::anyhow!("CDP websocket connect to {ws_url} timed out"))?
            .with_context(|| format!("CDP websocket connect to {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, _) = broadcast::channel::<Value>(512);
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_pump = Arc::clone(&pending);
        let event_tx_pump = event_tx.clone();
        let events_sent = Arc::new(AtomicU64::new(0));
        let sent_pump = Arc::clone(&events_sent);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_pump = Arc::clone(&closed);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    outgoing = rx.recv() => {
                        match outgoing {
                            Some(text) => {
                                let msg = tokio_tungstenite::tungstenite::Message::Text(text.into());
                                if sink.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            None => break, // client dropped: shut down
                        }
                    }
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                dispatch(&text, &pending_pump, &event_tx_pump, &sent_pump);
                            }
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                            Some(Ok(_)) => {} // ping/pong/binary: ignore
                            Some(Err(_)) => break,
                        }
                    }
                }
            }
            // Connection lost: fail everything still pending.
            close_all(&closed_pump, &pending_pump);
        });

        Ok(Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                pending,
                writer: Mutex::new(Some(tx)),
                events: event_tx,
                events_sent,
                closed,
            }),
        })
    }

    /// Connect over `--remote-debugging-pipe` fds: NUL-terminated JSON
    /// messages, one writer task and one reader task.
    #[cfg(unix)]
    pub async fn connect_pipe(from_browser: std::os::fd::OwnedFd, to_browser: std::os::fd::OwnedFd) -> Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut reader =
            tokio::net::unix::pipe::Receiver::from_owned_fd(from_browser).context("CDP pipe (browser -> us)")?;
        let mut writer =
            tokio::net::unix::pipe::Sender::from_owned_fd(to_browser).context("CDP pipe (us -> browser)")?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, _) = broadcast::channel::<Value>(512);
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let (closed_w, pending_w) = (Arc::clone(&closed), Arc::clone(&pending));
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let mut bytes = text.into_bytes();
                bytes.push(0);
                if writer.write_all(&bytes).await.is_err() {
                    // Chrome is gone (EPIPE): the request just queued would
                    // otherwise wait out its whole timeout.
                    close_all(&closed_w, &pending_w);
                    break;
                }
            }
            // Dropping the writer closes Chrome's command pipe: it exits.
        });
        let pending_pump = Arc::clone(&pending);
        let event_tx_pump = event_tx.clone();
        let events_sent = Arc::new(AtomicU64::new(0));
        let sent_pump = Arc::clone(&events_sent);
        let closed_pump = Arc::clone(&closed);
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut chunk = vec![0u8; 256 * 1024];
            let mut scanned = 0;
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        while let Some(pos) = buf[scanned..].iter().position(|b| *b == 0) {
                            let end = scanned + pos;
                            if let Ok(text) = std::str::from_utf8(&buf[..end]) {
                                dispatch(text, &pending_pump, &event_tx_pump, &sent_pump);
                            }
                            buf.drain(..=end);
                            scanned = 0;
                        }
                        scanned = buf.len();
                    }
                }
            }
            close_all(&closed_pump, &pending_pump);
        });
        Ok(Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                pending,
                writer: Mutex::new(Some(tx)),
                events: event_tx,
                events_sent,
                closed,
            }),
        })
    }

    /// Send one CDP command and await its `{result}` (or `{error}`).
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        trace(method, &params);
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if self.inner.closed.load(Ordering::SeqCst) {
            anyhow::bail!("CDP connection closed during {method}");
        }
        self.inner.pending.lock().unwrap().insert(id, (session_id.map(str::to_string), tx));
        // The transport may have closed (and drained `pending`) between the
        // check and the insert.
        if self.inner.closed.load(Ordering::SeqCst) {
            self.inner.pending.lock().unwrap().remove(&id);
            anyhow::bail!("CDP connection closed during {method}");
        }

        let mut msg = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session) = session_id {
            msg["sessionId"] = Value::String(session.to_string());
        }
        if let Some(writer) = self.inner.writer.lock().unwrap().as_ref() {
            if writer.send(msg.to_string()).is_err() {
                self.inner.pending.lock().unwrap().remove(&id);
                anyhow::bail!("CDP client is shut down");
            }
        } else {
            self.inner.pending.lock().unwrap().remove(&id);
            anyhow::bail!("CDP client is shut down");
        }

        let response = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| anyhow::anyhow!("CDP command timed out: {method}"))?
            .map_err(|_| anyhow::anyhow!("CDP connection closed during {method}"))?;
        response.map_err(|e| anyhow::anyhow!("CDP error in {method}: {e}"))
    }

    /// How many events have been broadcast to subscribers so far.
    pub fn events_sent(&self) -> u64 {
        self.inner.events_sent.load(Ordering::SeqCst)
    }

    /// Subscribe to CDP events (method-keyed messages without `id`).
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
    }

    /// Wait for the first event matching `pred`.
    pub async fn wait_event(&self, mut pred: impl FnMut(&Value) -> bool, timeout: Duration) -> Result<Value> {
        let mut rx = self.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if pred(&event) {
                            return Ok(event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => anyhow::bail!("CDP event stream ended: {e}"),
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for CDP event"))?
    }

    /// Stop the pump task; the websocket closes when the task drops it.
    /// After this, `send` fails fast.
    pub fn shutdown(&self) {
        self.inner.writer.lock().unwrap().take();
    }
}

/// `NAVIGERA_CDP_TRACE=<file>`: append every CDP command this process sends as one
/// JSON line `{"method", "params"}` with values reduced to their shape
/// (short strings kept so enum values can be checked). CI validates the
/// trace against the running browser's own `/json/protocol`
/// (`tools/cdp_check.py`), so a renamed method or parameter in a new Chrome
/// fails a test run instead of an agent's session.
fn trace(method: &str, params: &Value) {
    use std::io::Write as _;
    static FILE: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("NAVIGERA_CDP_TRACE")?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    let Some(file) = file else { return };
    // One write() per line: several navigera processes append to the
    // same file in parallel, and O_APPEND keeps single writes whole.
    let mut line = serde_json::json!({ "method": method, "params": shape(params, 0) }).to_string();
    line.push('\n');
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

/// A value's shape for the trace: objects/arrays recursed (bounded), short
/// strings kept verbatim (enum candidates), everything else as a type tag.
fn shape(value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(map) if depth < 4 => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v, depth + 1))).collect())
        }
        Value::Array(items) if depth < 4 => {
            Value::Array(items.first().map(|v| shape(v, depth + 1)).into_iter().collect())
        }
        Value::String(s) if s.len() <= 40 && !s.contains(char::is_whitespace) => Value::String(s.clone()),
        Value::String(_) => Value::String("<string>".into()),
        Value::Number(_) => Value::String("<number>".into()),
        Value::Bool(_) => Value::String("<boolean>".into()),
        Value::Null => Value::Null,
        Value::Object(_) => Value::String("<object>".into()),
        Value::Array(_) => Value::String("<array>".into()),
    }
}

fn dispatch(text: &str, pending: &Mutex<Pending>, events: &broadcast::Sender<Value>, sent: &AtomicU64) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let responder = pending.lock().unwrap().remove(&id);
        if let Some((_, responder)) = responder {
            if let Some(error) = value.get("error") {
                let _ = responder.send(Err(error.to_string()));
            } else {
                let result = value.get("result").cloned().unwrap_or(Value::Null);
                let _ = responder.send(Ok(result));
            }
        }
        return;
    }
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        // The tab closed (or was detached): Chrome never answers what was
        // still waiting on it, so it fails now rather than at its timeout.
        if method == "Target.detachedFromTarget" {
            if let Some(gone) = value.pointer("/params/sessionId").and_then(Value::as_str) {
                let mut pending = pending.lock().unwrap();
                let ids: Vec<u64> =
                    pending.iter().filter(|(_, (s, _))| s.as_deref() == Some(gone)).map(|(id, _)| *id).collect();
                for id in ids {
                    if let Some((_, responder)) = pending.remove(&id) {
                        let _ = responder.send(Err(format!("{SESSION_GONE}: the tab's session ended while this ran")));
                    }
                }
            }
        }
        sent.fetch_add(1, Ordering::SeqCst);
        let _ = events.send(value);
    }
}
