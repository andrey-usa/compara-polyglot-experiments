//! What the page said and fetched: console messages, uncaught exceptions,
//! browser log entries, and every network request, per tab. Fed by the event
//! watcher (`events.rs`), read by `console`, `errors` and `network`.
//!
//! Both logs are bounded rings: an agent asks about the last few hundred
//! entries, never the whole history of a long session.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

/// Entries kept per tab and log.
const MAX_CONSOLE: usize = 1000;
const MAX_REQUESTS: usize = 1000;
/// Longest console text kept (a logged 2 MB JSON string is not useful).
const MAX_TEXT: usize = 2000;
/// Request bodies kept for `network <n>`.
const MAX_POST_DATA: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ConsoleEntry {
    /// `log`, `info`, `warn`, `error`, `debug`.
    pub level: String,
    /// `console`, `exception` or `browser` (a Log-domain entry: failed
    /// loads, CSP and mixed-content reports).
    pub source: &'static str,
    pub text: String,
    /// `url:line` where it came from, when known.
    pub location: String,
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Session-wide number shown to the agent (`#12`).
    pub n: u64,
    pub id: String,
    pub method: String,
    pub url: String,
    /// CDP resource type: Document, XHR, Fetch, Script, Image, …
    pub kind: String,
    pub status: Option<u64>,
    pub mime: String,
    pub request_headers: Value,
    pub response_headers: Value,
    pub post_data: Option<String>,
    /// The request has a body CDP didn't inline (`Network.getRequestPostData`).
    pub has_post_data: bool,
    /// Bytes on the wire (`encodedDataLength`).
    pub size: Option<u64>,
    /// CDP monotonic timestamps (seconds).
    pub started: f64,
    pub ended: Option<f64>,
    /// Wall-clock start (seconds since the epoch), for HAR.
    pub wall: f64,
    pub failed: Option<String>,
    pub from_cache: bool,
}

#[derive(Default)]
pub struct TabLogs {
    pub console: VecDeque<ConsoleEntry>,
    pub requests: VecDeque<Request>,
    /// Uncaught exceptions not yet reported in a response note.
    pub unreported_exceptions: Vec<String>,
}

#[derive(Default)]
pub struct Logs {
    pub tabs: HashMap<String, TabLogs>,
    next_request: u64,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One console argument as the page's own `console` would show it.
fn remote_object_text(arg: &Value) -> String {
    if let Some(v) = arg.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(v) = arg.get("unserializableValue").and_then(Value::as_str) {
        return v.to_string();
    }
    if let Some(preview) = arg.get("preview") {
        let array = preview.get("subtype").and_then(Value::as_str) == Some("array");
        let props: Vec<String> = preview
            .get("properties")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| {
                let raw = p.get("value").and_then(Value::as_str).unwrap_or("…");
                let value = if p.get("type").and_then(Value::as_str) == Some("string") {
                    format!("'{raw}'")
                } else {
                    raw.to_string()
                };
                if array {
                    value
                } else {
                    format!("{}: {value}", p.get("name").and_then(Value::as_str).unwrap_or(""))
                }
            })
            .collect();
        let overflow = if preview.get("overflow").and_then(Value::as_bool) == Some(true) { ", …" } else { "" };
        let (open, close) = if array { ("[", "]") } else { ("{", "}") };
        return format!("{open}{}{overflow}{close}", props.join(", "));
    }
    arg.get("description")
        .and_then(Value::as_str)
        .or_else(|| arg.get("type").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn location(url: Option<&str>, line: Option<u64>) -> String {
    match (url.filter(|u| !u.is_empty()), line) {
        (Some(u), Some(l)) => format!("{u}:{}", l + 1),
        (Some(u), None) => u.to_string(),
        _ => String::new(),
    }
}

impl Logs {
    /// Requests seen so far, all tabs.
    pub fn requests_started(&self) -> u64 {
        self.next_request
    }

    fn tab(&mut self, session: &str) -> &mut TabLogs {
        self.tabs.entry(session.to_string()).or_default()
    }

    fn push_console(&mut self, session: &str, entry: ConsoleEntry) {
        let tab = self.tab(session);
        if tab.console.len() >= MAX_CONSOLE {
            tab.console.pop_front();
        }
        tab.console.push_back(entry);
    }

    /// Feed one CDP event; ignores everything that isn't a log or network event.
    pub fn on_event(&mut self, method: &str, params: &Value, session: &str) {
        if session.is_empty() {
            return;
        }
        match method {
            "Runtime.consoleAPICalled" => {
                let kind = params.get("type").and_then(Value::as_str).unwrap_or("log");
                let level = match kind {
                    "error" | "assert" => "error",
                    "warning" => "warn",
                    "info" => "info",
                    "debug" => "debug",
                    _ => "log",
                };
                let text: Vec<String> = params
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(remote_object_text)
                    .collect();
                let frame = params.pointer("/stackTrace/callFrames/0");
                let loc = location(
                    frame.and_then(|f| f.get("url")).and_then(Value::as_str),
                    frame.and_then(|f| f.get("lineNumber")).and_then(Value::as_u64),
                );
                self.push_console(
                    session,
                    ConsoleEntry {
                        level: level.into(),
                        source: "console",
                        text: clip(&text.join(" "), MAX_TEXT),
                        location: loc,
                    },
                );
            }
            "Runtime.exceptionThrown" => {
                let d = params.get("exceptionDetails").cloned().unwrap_or(Value::Null);
                // `description` carries "TypeError: … \n    at …": the first
                // line is the message, the stack follows.
                let text = d
                    .pointer("/exception/description")
                    .and_then(Value::as_str)
                    .or_else(|| d.pointer("/exception/value").and_then(Value::as_str))
                    .or_else(|| d.get("text").and_then(Value::as_str))
                    .unwrap_or("exception")
                    .to_string();
                let loc = location(d.get("url").and_then(Value::as_str), d.get("lineNumber").and_then(Value::as_u64));
                let first = text.lines().next().unwrap_or("").to_string();
                self.tab(session).unreported_exceptions.push(first);
                self.push_console(
                    session,
                    ConsoleEntry {
                        level: "error".into(),
                        source: "exception",
                        text: clip(&text, MAX_TEXT),
                        location: loc,
                    },
                );
            }
            "Log.entryAdded" => {
                let e = params.get("entry").cloned().unwrap_or(Value::Null);
                let level = match e.get("level").and_then(Value::as_str).unwrap_or("info") {
                    "error" => "error",
                    "warning" => "warn",
                    "verbose" => return,
                    _ => "info",
                };
                let text = e.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                let url = e.get("url").and_then(Value::as_str);
                // Chrome asks every origin for /favicon.ico on its own: not
                // the page's error.
                if url.is_some_and(|u| u.ends_with("/favicon.ico")) {
                    return;
                }
                let loc = location(url, e.get("lineNumber").and_then(Value::as_u64));
                self.push_console(
                    session,
                    ConsoleEntry { level: level.into(), source: "browser", text: clip(&text, MAX_TEXT), location: loc },
                );
            }
            "Network.requestWillBeSent" => {
                let Some(id) = params.get("requestId").and_then(Value::as_str) else { return };
                let req = params.get("request").cloned().unwrap_or(Value::Null);
                // A redirect reuses the request id: close the hop that
                // redirected, then log the next one as its own entry.
                if let Some(redirect) = params.get("redirectResponse") {
                    let ts = params.get("timestamp").and_then(Value::as_f64);
                    if let Some(prev) =
                        self.tab(session).requests.iter_mut().rev().find(|r| r.id == id && r.ended.is_none())
                    {
                        prev.status = redirect.get("status").and_then(Value::as_u64);
                        prev.response_headers = redirect.get("headers").cloned().unwrap_or(Value::Null);
                        prev.mime = redirect.get("mimeType").and_then(Value::as_str).unwrap_or("").to_string();
                        prev.ended = ts;
                    }
                }
                self.next_request += 1;
                let entry = Request {
                    n: self.next_request,
                    id: id.to_string(),
                    method: req.get("method").and_then(Value::as_str).unwrap_or("GET").to_string(),
                    url: req.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                    kind: params.get("type").and_then(Value::as_str).unwrap_or("Other").to_string(),
                    request_headers: req.get("headers").cloned().unwrap_or(Value::Null),
                    post_data: req.get("postData").and_then(Value::as_str).map(|s| clip(s, MAX_POST_DATA)),
                    has_post_data: req.get("hasPostData").and_then(Value::as_bool) == Some(true),
                    started: params.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0),
                    wall: params.get("wallTime").and_then(Value::as_f64).unwrap_or(0.0),
                    ..Request::default()
                };
                let tab = self.tab(session);
                if tab.requests.len() >= MAX_REQUESTS {
                    tab.requests.pop_front();
                }
                tab.requests.push_back(entry);
            }
            "Network.responseReceived" => {
                let Some(id) = params.get("requestId").and_then(Value::as_str) else { return };
                let resp = params.get("response").cloned().unwrap_or(Value::Null);
                if let Some(r) = self.find_open(session, id) {
                    r.status = resp.get("status").and_then(Value::as_u64);
                    r.mime = resp.get("mimeType").and_then(Value::as_str).unwrap_or("").to_string();
                    r.response_headers = resp.get("headers").cloned().unwrap_or(Value::Null);
                    r.from_cache = resp.get("fromDiskCache").and_then(Value::as_bool) == Some(true)
                        || resp.get("fromServiceWorker").and_then(Value::as_bool) == Some(true);
                    if let Some(t) = params.get("type").and_then(Value::as_str) {
                        r.kind = t.to_string();
                    }
                }
            }
            "Network.loadingFinished" => {
                let Some(id) = params.get("requestId").and_then(Value::as_str) else { return };
                let ts = params.get("timestamp").and_then(Value::as_f64);
                let size = params.get("encodedDataLength").and_then(Value::as_f64).map(|s| s as u64);
                if let Some(r) = self.find_open(session, id) {
                    r.ended = ts.or(Some(r.started));
                    r.size = size;
                }
            }
            "Network.loadingFailed" => {
                let Some(id) = params.get("requestId").and_then(Value::as_str) else { return };
                let ts = params.get("timestamp").and_then(Value::as_f64);
                let why = if params.get("canceled").and_then(Value::as_bool) == Some(true) {
                    "canceled".to_string()
                } else {
                    params
                        .get("blockedReason")
                        .and_then(Value::as_str)
                        // `inspector`: a `route` rule failed it.
                        .map(|b| if b == "inspector" { "route".to_string() } else { format!("blocked: {b}") })
                        .or_else(|| params.get("errorText").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| "failed".into())
                };
                if let Some(r) = self.find_open(session, id) {
                    r.ended = ts.or(Some(r.started));
                    r.failed = Some(why);
                }
            }
            _ => {}
        }
    }

    fn find_open(&mut self, session: &str, id: &str) -> Option<&mut Request> {
        self.tabs.get_mut(session)?.requests.iter_mut().rev().find(|r| r.id == id)
    }

    /// Uncaught exceptions since the last call, for a response note.
    pub fn take_exceptions(&mut self, session: &str) -> Vec<String> {
        self.tabs.get_mut(session).map(|t| std::mem::take(&mut t.unreported_exceptions)).unwrap_or_default()
    }
}

/// Console filter: `errors` is `level == error`.
pub fn console_text(entries: &[ConsoleEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let tag = if e.source == "exception" { "exception".to_string() } else { e.level.clone() };
            if e.location.is_empty() {
                format!("[{tag}] {}", e.text)
            } else {
                format!("[{tag}] {}  ({})", e.text, e.location)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn console_json(entries: &[ConsoleEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|e| json!({ "level": e.level, "source": e.source, "text": e.text, "location": e.location }))
            .collect(),
    )
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    }
}

/// `url` relative to `origin` when it is on it.
fn short(url: &str, origin: &str) -> String {
    if !origin.is_empty() {
        if let Some(rest) = url.strip_prefix(origin) {
            if rest.starts_with('/') {
                return rest.to_string();
            }
        }
    }
    clip(url, 200)
}

/// One line per request: `#7 POST 201 Fetch /api/orders 512 B 23 ms`.
pub fn requests_text(requests: &[Request], origin: &str) -> String {
    requests
        .iter()
        .map(|r| {
            let status = match (&r.failed, r.status) {
                (Some(why), _) => format!("FAILED({why})"),
                (None, Some(s)) => s.to_string(),
                (None, None) => "pending".to_string(),
            };
            let mut line = format!("#{} {} {} {} {}", r.n, r.method, status, r.kind, short(&r.url, origin));
            if let Some(size) = r.size {
                line.push_str(&format!(" {}", human_bytes(size)));
            }
            if let Some(end) = r.ended {
                line.push_str(&format!(" {:.0} ms", ((end - r.started) * 1000.0).max(0.0)));
            }
            if r.from_cache {
                line.push_str(" (cache)");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The full record of one request (bodies are added by the caller).
pub fn request_json(r: &Request) -> Value {
    json!({
        "n": r.n,
        "method": r.method,
        "url": r.url,
        "type": r.kind,
        "status": r.status,
        "failed": r.failed,
        "mime": r.mime,
        "size": r.size,
        "ms": r.ended.map(|e| ((e - r.started) * 1000.0).max(0.0).round()),
        "from_cache": r.from_cache,
        "request_headers": r.request_headers,
        "post_data": r.post_data,
        "response_headers": r.response_headers,
    })
}

fn har_headers(headers: &Value) -> Value {
    Value::Array(
        headers
            .as_object()
            .into_iter()
            .flatten()
            .map(
                |(k, v)| json!({ "name": k, "value": v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()) }),
            )
            .collect(),
    )
}

/// HAR 1.2 of the logged requests (no response bodies).
pub fn har(requests: &[Request], creator_version: &str) -> Value {
    let entries: Vec<Value> = requests
        .iter()
        .map(|r| {
            let ms = r.ended.map(|e| ((e - r.started) * 1000.0).max(0.0)).unwrap_or(0.0);
            let started = chrono_like_iso(r.wall);
            let query: Vec<Value> = r
                .url
                .split_once('?')
                .map(|(_, q)| q.split('&').filter(|p| !p.is_empty()).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    json!({ "name": k, "value": v })
                })
                .collect();
            let mut request = json!({
                "method": r.method, "url": r.url, "httpVersion": "HTTP/1.1",
                "headers": har_headers(&r.request_headers), "queryString": query, "cookies": [],
                "headersSize": -1, "bodySize": r.post_data.as_ref().map(|p| p.len() as i64).unwrap_or(0),
            });
            if let Some(p) = &r.post_data {
                let mime = r
                    .request_headers
                    .as_object()
                    .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")))
                    .and_then(|(_, v)| v.as_str())
                    .unwrap_or("");
                request["postData"] = json!({ "mimeType": mime, "text": p });
            }
            json!({
                "startedDateTime": started,
                "time": ms,
                "request": request,
                "response": {
                    "status": r.status.unwrap_or(0), "statusText": r.failed.clone().unwrap_or_default(),
                    "httpVersion": "HTTP/1.1", "headers": har_headers(&r.response_headers), "cookies": [],
                    "content": { "size": r.size.unwrap_or(0), "mimeType": r.mime },
                    "redirectURL": "", "headersSize": -1, "bodySize": r.size.map(|s| s as i64).unwrap_or(-1),
                },
                "cache": {},
                "timings": { "send": 0, "wait": ms, "receive": 0 },
                "_resourceType": r.kind.to_ascii_lowercase(),
            })
        })
        .collect();
    json!({ "log": {
        "version": "1.2",
        "creator": { "name": "navigera", "version": creator_version },
        "pages": [],
        "entries": entries,
    }})
}

/// RFC 3339 UTC time from seconds since the epoch (HAR wants ISO 8601).
fn chrono_like_iso(epoch_s: f64) -> String {
    let secs = epoch_s.floor() as i64;
    let millis = ((epoch_s - secs as f64) * 1000.0).round() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_and_exceptions_are_logged_with_their_origin() {
        let mut logs = Logs::default();
        logs.on_event(
            "Runtime.consoleAPICalled",
            &json!({"type": "warning", "args": [{"type": "string", "value": "low stock"}, {"type": "number", "value": 3}],
                    "stackTrace": {"callFrames": [{"url": "http://x/app.js", "lineNumber": 9}]}}),
            "s1",
        );
        logs.on_event(
            "Runtime.exceptionThrown",
            &json!({"exceptionDetails": {"text": "Uncaught", "url": "http://x/app.js", "lineNumber": 41,
                    "exception": {"description": "TypeError: cart is undefined\n    at add (app.js:42)"}}}),
            "s1",
        );
        let entries: Vec<_> = logs.tabs["s1"].console.iter().cloned().collect();
        let text = console_text(&entries);
        assert!(text.contains("[warn] low stock 3  (http://x/app.js:10)"), "{text}");
        let arr = json!({"type": "object", "subtype": "array", "preview": {"subtype": "array", "properties": [
            {"name": "0", "value": "1"}, {"name": "1", "value": "2"}]}});
        assert_eq!(remote_object_text(&arr), "[1, 2]");
        assert!(text.contains("[exception] TypeError: cart is undefined"), "{text}");
        assert_eq!(logs.take_exceptions("s1"), vec!["TypeError: cart is undefined".to_string()]);
        assert!(logs.take_exceptions("s1").is_empty());
    }

    #[test]
    fn requests_follow_redirects_and_failures() {
        let mut logs = Logs::default();
        let send = |logs: &mut Logs, id: &str, url: &str, t: f64, redirect: Option<Value>| {
            let mut p = json!({"requestId": id, "type": "Document", "timestamp": t, "wallTime": 1.7e9,
                               "request": {"url": url, "method": "GET", "headers": {}}});
            if let Some(r) = redirect {
                p["redirectResponse"] = r;
            }
            logs.on_event("Network.requestWillBeSent", &p, "s");
        };
        send(&mut logs, "1", "http://h/old", 1.0, None);
        send(&mut logs, "1", "http://h/new", 1.05, Some(json!({"status": 301, "headers": {}})));
        logs.on_event(
            "Network.responseReceived",
            &json!({"requestId": "1", "type": "Document",
            "response": {"status": 200, "mimeType": "text/html", "headers": {"a": "b"}}}),
            "s",
        );
        logs.on_event(
            "Network.loadingFinished",
            &json!({"requestId": "1", "timestamp": 1.1, "encodedDataLength": 2048}),
            "s",
        );
        send(&mut logs, "2", "http://h/api", 1.2, None);
        logs.on_event(
            "Network.loadingFailed",
            &json!({"requestId": "2", "timestamp": 1.3, "errorText": "net::ERR_FAILED"}),
            "s",
        );
        let reqs: Vec<_> = logs.tabs["s"].requests.iter().cloned().collect();
        let text = requests_text(&reqs, "http://h");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#1 GET 301 Document /old 50 ms");
        assert_eq!(lines[1], "#2 GET 200 Document /new 2.0 KB 50 ms");
        assert!(lines[2].starts_with("#3 GET FAILED(net::ERR_FAILED) Document /api"), "{text}");
        let har = har(&reqs, "0");
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 3);
        assert_eq!(har["log"]["entries"][1]["response"]["status"], 200);
    }

    #[test]
    fn iso_time() {
        assert_eq!(chrono_like_iso(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(chrono_like_iso(1_792_475_590.483), "2026-10-20T05:53:10.483Z");
    }
}
