//! `http <METHOD> <url>`: a request outside the page, through `curl`, which
//! every supported OS ships, so the binary carries no TLS stack. curl reads
//! its options (URL, headers, body) on stdin, so a secret in a header or the
//! body never reaches the process list.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// A curl config string: `"…"` with `\` and `"` escaped, and the control
/// characters curl unescapes.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// curl's config for one request.
fn config(method: &str, url: &str, headers: &[String], body: Option<&str>, limit: Duration) -> String {
    let mut c = format!("url = {}\nmax-time = {:.3}\n", quoted(url), limit.as_secs_f64());
    if method == "HEAD" {
        c.push_str("head\n");
    } else {
        c.push_str(&format!("request = {}\n", quoted(method)));
    }
    for h in headers {
        c.push_str(&format!("header = {}\n", quoted(h)));
    }
    if let Some(b) = body {
        c.push_str(&format!("data-binary = {}\n", quoted(b)));
    }
    c
}

/// curl `--include` output: the last header block (after any `100
/// Continue`) and the body.
fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let mut rest = raw;
    loop {
        let end = rest
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| rest.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)))
            .ok_or("no HTTP response")?;
        let head = String::from_utf8_lossy(&rest[..end.0]).into_owned();
        let body = &rest[end.0 + end.1..];
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("not an HTTP status line: {status_line:?}"))?;
        // An interim answer, then the real one.
        if (100..200).contains(&status) && body.starts_with(b"HTTP/") {
            rest = body;
            continue;
        }
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        return Ok(Response { status, headers, body: String::from_utf8_lossy(body).into_owned() });
    }
}

/// Make the request. `headers` are `Name: value` lines.
pub fn request(
    method: &str,
    url: &str,
    headers: &[String],
    body: Option<&str>,
    limit: Duration,
) -> Result<Response, String> {
    let mut child = Command::new("curl")
        .args(["-sS", "--include", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("http needs curl on the PATH: {e}"))?;
    let cfg = config(method, url, headers, body, limit);
    child.stdin.take().expect("piped").write_all(cfg.as_bytes()).map_err(|e| format!("curl: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim().trim_start_matches("curl: ");
        return Err(format!("{method} {url}: {}", if why.is_empty() { "curl failed" } else { why }));
    }
    parse_response(&out.stdout)
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An HTTP date (`Wed, 21 Oct 2015 07:28:00 GMT`, or with dashes) in Unix
/// seconds.
fn http_date(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let parts: Vec<&str> = s.split([' ', '-', ':', ',']).filter(|p| !p.is_empty()).collect();
    // [weekday,] day month year h m s [GMT]
    let at = parts.iter().position(|p| p.chars().all(|c| c.is_ascii_digit()))?;
    let (d, mon, y, h, mi, se) = (
        parts.get(at)?,
        parts.get(at + 1)?,
        parts.get(at + 2)?,
        parts.get(at + 3)?,
        parts.get(at + 4)?,
        parts.get(at + 5)?,
    );
    let m = MONTHS.iter().position(|x| mon.to_ascii_lowercase().starts_with(x))? as i64 + 1;
    let mut y: i64 = y.parse().ok()?;
    if y < 100 {
        y += if y < 70 { 2000 } else { 1900 };
    }
    let days = days_from_civil(y, m, d.parse().ok()?);
    Some(days * 86_400 + h.parse::<i64>().ok()? * 3600 + mi.parse::<i64>().ok()? * 60 + se.parse::<i64>().ok()?)
}

/// A `Set-Cookie` value as navigera `cookies` words for the scenario's
/// browser: `set <name> <value> …`, or `delete <name> …` for one that
/// expires it.
pub fn cookie_words(set_cookie: &str, url: &str, now: i64) -> Option<Vec<String>> {
    let mut parts = set_cookie.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim().to_string(), value.trim().trim_matches('"').to_string());
    if name.is_empty() {
        return None;
    }
    let (mut domain, mut path, mut expires, mut max_age) = (None, None, None, None);
    let mut flags = Vec::new();
    for attr in parts {
        let (k, v) = attr.split_once('=').map(|(k, v)| (k.trim(), v.trim())).unwrap_or((attr.trim(), ""));
        match k.to_ascii_lowercase().as_str() {
            "domain" if !v.is_empty() => domain = Some(v.trim_start_matches('.').to_string()),
            "path" if v.starts_with('/') => path = Some(v.to_string()),
            "expires" => expires = http_date(v),
            "max-age" => max_age = v.parse::<i64>().ok(),
            "httponly" => flags.push("--http-only".to_string()),
            "secure" => flags.push("--secure".to_string()),
            "samesite" if !v.is_empty() => {
                let mut s = v.to_ascii_lowercase();
                s[..1].make_ascii_uppercase();
                flags.extend(["--same-site".to_string(), s]);
            }
            _ => {}
        }
    }
    // Max-Age wins over Expires.
    let until = max_age.map(|a| now + a).or(expires);
    let mut words = match until {
        Some(t) if t <= now => vec!["cookies".to_string(), "delete".into(), name],
        _ => vec!["cookies".to_string(), "set".into(), name, value],
    };
    match domain {
        Some(d) => words.extend(["--domain".to_string(), d]),
        None => words.extend(["--url".to_string(), url.to_string()]),
    }
    if words[1] == "set" {
        if let Some(p) = path {
            words.extend(["--path".to_string(), p]);
        }
        if let Some(t) = until {
            words.extend(["--expires".to_string(), t.to_string()]);
        }
        words.extend(flags);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_quotes_what_curl_unquotes() {
        let c = config(
            "POST",
            "http://h/a",
            &["X-Note: say \"hi\"\\".into()],
            Some("{\"a\":\n1}"),
            Duration::from_millis(1500),
        );
        assert_eq!(
            c,
            "url = \"http://h/a\"\nmax-time = 1.500\nrequest = \"POST\"\nheader = \"X-Note: say \\\"hi\\\"\\\\\"\ndata-binary = \"{\\\"a\\\":\\n1}\"\n"
        );
        assert!(config("HEAD", "http://h/", &[], None, Duration::from_secs(1)).contains("\nhead\n"));
    }

    #[test]
    fn responses_skip_interim_answers() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Type: application/json\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n{\"id\":7}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body, "{\"id\":7}");
        assert_eq!(r.headers[0], ("Content-Type".to_string(), "application/json".to_string()));
        assert_eq!(r.headers.iter().filter(|(k, _)| k == "Set-Cookie").count(), 2);
        assert_eq!(parse_response(b"HTTP/2 204\r\n\r\n").unwrap().status, 204);
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn set_cookie_becomes_navigera_words() {
        let now = 1_700_000_000;
        assert_eq!(
            cookie_words("sid=abc; Path=/; HttpOnly; SameSite=lax", "http://h/api", now).unwrap().join(" "),
            "cookies set sid abc --url http://h/api --path / --http-only --same-site Lax"
        );
        assert_eq!(
            cookie_words("t=1; Domain=.shop.test; Max-Age=60; Secure", "https://shop.test/", now).unwrap().join(" "),
            format!("cookies set t 1 --domain shop.test --expires {} --secure", now + 60)
        );
        assert_eq!(
            cookie_words("sid=; Expires=Thu, 01 Jan 1970 00:00:00 GMT", "http://h/", now).unwrap().join(" "),
            "cookies delete sid --url http://h/"
        );
        assert_eq!(http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(http_date("Wed, 21-Oct-15 07:28:00 GMT"), Some(1_445_412_480));
        assert!(cookie_words("novalue", "http://h/", now).is_none());
    }
}
