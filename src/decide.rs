//! Small decisions about a page, made by a System One model instead of the
//! agent's LLM: which element a description means (`find`), and yes/no
//! questions about what the page shows (`check`).
//!
//! The model is TypeSafe's Jev (`POST /v1/systemone`): it reads a state and
//! answers typed questions -- a choice among up to 255 options, a yes/no
//! probability, a score -- in a few hundred milliseconds, for input tokens
//! only. It writes nothing, so it cannot drive a task on its own; what it can
//! do is the grounding and checking an agent otherwise pays a full LLM turn
//! and a full page snapshot for. Configured by environment:
//!
//! - `TYPESAFE_API_KEY`: use TypeSafe's API (`https://api.typesafe.ai`).
//! - `NAVIGERA_DECIDER_URL`: another endpoint with the same API, such as a
//!   LiteLLM proxy (`http://<proxy>/typesafe/v1/systemone`) or a local mock.
//! - `NAVIGERA_DECIDER_MODEL`: the model id (default `jev-latest`).
//!
//! Plain `http://` endpoints are spoken to directly; `https://` goes through
//! `curl`, which every supported OS ships, so the binary carries no TLS stack
//! for an optional feature. The key reaches curl on stdin, never in argv.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

const DEFAULT_URL: &str = "https://api.typesafe.ai/v1/systemone";
const DEFAULT_MODEL: &str = "jev-latest";
/// Options per Choice question (the API's limit is 255).
const CHUNK: usize = 200;
/// Characters of questions per request: Jev reads 64k tokens a request, and
/// a character of an element description is about a quarter of a token.
const REQUEST_CHARS: usize = 160_000;
/// Characters of page text a `check` sends: Jev reads 32k tokens of state,
/// and accuracy drops as unrelated text grows, so less is better.
pub const CHECK_STATE_CHARS: usize = 60_000;

/// Where decisions are sent, or `None` when nothing is configured.
#[derive(Clone, Debug)]
pub struct Decider {
    url: String,
    key: Option<String>,
    model: String,
}

impl Decider {
    pub fn from_env() -> Option<Decider> {
        let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let key = var("TYPESAFE_API_KEY");
        let url = var("NAVIGERA_DECIDER_URL");
        if key.is_none() && url.is_none() {
            return None;
        }
        Some(Decider {
            url: url.unwrap_or_else(|| DEFAULT_URL.to_string()),
            key,
            model: var("NAVIGERA_DECIDER_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        })
    }

    /// The model id decisions go to.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The error for `find`/`check` when no decider is configured.
    pub fn missing(op: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{op} needs a decision model: set TYPESAFE_API_KEY (TypeSafe Jev), or NAVIGERA_DECIDER_URL \
             for an endpoint with the same API; without one, read `ax` and pass a ref"
        )
    }

    /// One evaluation: `questions` is the API's map of question id to
    /// question; the result is the `answers` map.
    pub fn ask(&self, state: &str, questions: Value) -> Result<Map<String, Value>> {
        let body = json!({ "model": self.model, "state": state, "questions": questions }).to_string();
        let mut wait = Duration::from_millis(400);
        for attempt in 0..3 {
            let (status, text) = post(&self.url, self.key.as_deref(), &body)?;
            // Rate limited or overloaded: back off and retry, as the API asks.
            if (status == 429 || status == 529) && attempt < 2 {
                std::thread::sleep(wait);
                wait *= 2;
                continue;
            }
            if status != 200 {
                bail!("the decision model answered HTTP {status}: {}", text.chars().take(300).collect::<String>());
            }
            let reply: Value = serde_json::from_str(&text).context("the decision model's reply is not JSON")?;
            return match reply.get("answers") {
                Some(Value::Object(answers)) => Ok(answers.clone()),
                _ => bail!("the decision model's reply has no answers: {}", text.chars().take(300).collect::<String>()),
            };
        }
        unreachable!("the last attempt returns")
    }
}

/// POSTs JSON and returns the status and body.
fn post(url: &str, key: Option<&str>, body: &str) -> Result<(u16, String)> {
    if let Some(rest) = url.strip_prefix("http://") {
        post_plain(rest, key, body)
    } else if url.starts_with("https://") {
        post_curl(url, key, body)
    } else {
        bail!("NAVIGERA_DECIDER_URL must start with http:// or https://, got {url:?}")
    }
}

fn post_plain(rest: &str, key: Option<&str>, body: &str) -> Result<(u16, String)> {
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().with_context(|| format!("port in {authority:?}"))?),
        None => (authority, 80),
    };
    let mut stream =
        TcpStream::connect((host, port)).with_context(|| format!("connect to the decider at {authority}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let auth = key.map(|k| format!("Authorization: Bearer {k}\r\n")).unwrap_or_default();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n{auth}\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).context("read the decider's reply")?;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("the decider's reply is not HTTP"))?;
    let body = crate::cdp::transport::complete_body(&buf)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .or_else(|| text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()))
        .unwrap_or_default();
    Ok((status, body))
}

fn post_curl(url: &str, key: Option<&str>, body: &str) -> Result<(u16, String)> {
    let mut file = tempfile::NamedTempFile::new().context("temp file for the decider request")?;
    file.write_all(body.as_bytes())?;
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "-X", "POST", "--max-time", "60", "--proto", "=https", "-H", "@-", "-w", "\n%{http_code}"])
        .arg("--data-binary")
        .arg(format!("@{}", file.path().display()))
        .arg(url)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run curl for the decider (https endpoints need curl on PATH)")?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let auth = key.map(|k| format!("Authorization: Bearer {k}\n")).unwrap_or_default();
        stdin.write_all(format!("Content-Type: application/json\n{auth}").as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("curl to the decider failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    let status = code.trim().parse::<u16>().with_context(|| format!("curl's status line {code:?}"))?;
    Ok((status, body.to_string()))
}

/// One element `find` can choose: its ref, its line in the tree, and the
/// containers it sits in (which tell two "Add to cart" buttons apart).
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub node_ref: u64,
    pub line: String,
    pub context: String,
}

impl Candidate {
    fn description(&self) -> String {
        let mut d = redact(&self.line);
        if !self.context.is_empty() {
            d.push_str(" — in ");
            d.push_str(&self.context);
        }
        d.chars().take(240).collect()
    }
}

/// The actionable elements of an `ax` text tree, in page order.
///
/// Context is what an agent would read around the element: the named
/// containers above it (a dialog, a row, a form) and, inside unnamed ones
/// (an article card), the heading or first text before it.
pub fn candidates(tree: &str) -> Vec<Candidate> {
    // (indent, label) of the open ancestors, and the latest heading/text seen
    // directly under each.
    let mut stack: Vec<(usize, String, Option<String>)> = Vec::new();
    let mut out = Vec::new();
    for raw in tree.lines().skip(1) {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim_start().trim_start_matches("- ").trim_end();
        if line.is_empty() {
            continue;
        }
        while stack.last().is_some_and(|(i, _, _)| *i >= indent) {
            stack.pop();
        }
        if let Some(node_ref) = ref_of(line) {
            let mut parts: Vec<String> = Vec::new();
            for (_, label, note) in stack.iter().rev().take(3) {
                match (label.contains('"'), note) {
                    (true, _) => parts.push(label.clone()),
                    (false, Some(n)) => parts.push(n.clone()),
                    (false, None) => {}
                }
            }
            parts.reverse();
            out.push(Candidate { node_ref, line: strip_ref(line), context: parts.join(" › ") });
        }
        // A heading or a text line names the container it is in.
        if line.starts_with("heading") || line.starts_with("text:") {
            if let Some(top) = stack.last_mut() {
                if top.2.is_none() {
                    top.2 = Some(strip_ref(line).chars().take(80).collect());
                }
            }
        }
        let label = line.trim_end_matches(':').to_string();
        stack.push((indent, strip_ref(&label), None));
    }
    out
}

/// The ref of a tree line: `[ref=12]`, or among other states as in
/// `[required, ref=12]`.
fn ref_of(line: &str) -> Option<u64> {
    let (at, _) = ref_span(line)?;
    let digits: String = line[at + 4..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Where `ref=N` starts in a line, and where the text to cut for it ends
/// (its separator included).
fn ref_span(line: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = line[from..].find("ref=") {
        let at = from + i;
        let before = line[..at].chars().last();
        if matches!(before, Some('[') | Some(' ')) && line[at + 4..].starts_with(|c: char| c.is_ascii_digit()) {
            let digits = line[at + 4..].chars().take_while(char::is_ascii_digit).count();
            return Some((at, at + 4 + digits));
        }
        from = at + 4;
    }
    None
}

/// The line without its ref: `[ref=12]` goes entirely, `[required, ref=12]`
/// keeps `[required]`.
fn strip_ref(line: &str) -> String {
    let Some((at, end)) = ref_span(line) else {
        return line.to_string();
    };
    let rest = &line[end..];
    if line[..at].ends_with(" [") && rest.starts_with(']') {
        format!("{}{}", &line[..at - 2], &rest[1..])
    } else if line[..at].ends_with(", ") {
        format!("{}{}", &line[..at - 2], rest)
    } else if line[..at].ends_with('[') && rest.starts_with(", ") {
        format!("{}{}", &line[..at], &rest[2..])
    } else {
        format!("{}{}", &line[..at], rest)
    }
}

/// A tree line as it may leave the machine: what was typed into a text
/// field (an email, a card number) is replaced by whether anything was.
fn redact(line: &str) -> String {
    let role = line.trim_start().trim_start_matches("- ");
    if !(role.starts_with("textbox") || role.starts_with("searchbox")) {
        return line.to_string();
    }
    match line.find(" value=\"") {
        Some(at) => {
            let start = at + " value=\"".len();
            let end = line[start..].find('"').map(|e| start + e).unwrap_or(line.len());
            let filled = if end > start { "(filled)" } else { "" };
            format!("{} value=\"{filled}\"{}", &line[..at], line.get(end + 1..).unwrap_or(""))
        }
        None => line.to_string(),
    }
}

/// What `find` decided.
#[derive(Clone, Debug)]
pub struct Found {
    pub chosen: Candidate,
    /// The chosen option's probability.
    pub p: f64,
    /// The model's confidence in the choice as a whole (0 to 1).
    pub confidence: f64,
    /// The runners-up, most likely first.
    pub others: Vec<(Candidate, f64)>,
}

impl Found {
    pub fn to_json(&self) -> Value {
        json!({
            "ref": self.chosen.node_ref,
            "element": self.chosen.line,
            "in": self.chosen.context,
            "p": round3(self.p),
            "confidence": round3(self.confidence),
            "others": self.others.iter().map(|(c, p)| json!({"ref": c.node_ref, "element": c.line, "p": round3(*p)})).collect::<Vec<_>>(),
        })
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Picks the element `what` describes. Pages with more candidates than one
/// Choice takes are split into chunks asked in the same request (questions
/// are evaluated in parallel), and the chunk winners are asked again.
pub fn find(decider: &Decider, what: &str, page: &str, all: &[Candidate]) -> Result<Found> {
    if all.is_empty() {
        bail!("find: the page has no actionable elements");
    }
    let state = format!("Page: {page}\nLooking for the element to use for: {what}");
    let instructions = format!("Which element on the page is the one to use for: {what}?");
    let mut pool: Vec<&Candidate> = all.iter().collect();
    loop {
        let chunks: Vec<&[&Candidate]> = pool.chunks(CHUNK).collect();
        // One question per chunk, as many to a request as fit its budget.
        let mut answers = Map::new();
        let mut questions = Map::new();
        let mut size = 0;
        for (i, chunk) in chunks.iter().enumerate() {
            let criteria: Map<String, Value> =
                chunk.iter().map(|c| (format!("e{}", c.node_ref), Value::String(c.description()))).collect();
            let q = json!({ "type": "choice", "instructions": instructions, "criteria": criteria });
            let q_size = q.to_string().len();
            if !questions.is_empty() && size + q_size > REQUEST_CHARS {
                answers.extend(decider.ask(&state, Value::Object(std::mem::take(&mut questions)))?);
                size = 0;
            }
            size += q_size;
            questions.insert(format!("element{i}"), q);
        }
        answers.extend(decider.ask(&state, Value::Object(questions))?);
        let mut ranked: Vec<(&Candidate, f64, f64)> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let a = answers.get(&format!("element{i}")).ok_or_else(|| anyhow::anyhow!("no answer for chunk {i}"))?;
            let confidence = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
            let probs = a.get("probabilities").and_then(Value::as_object).cloned().unwrap_or_default();
            for c in chunk.iter() {
                let p = probs.get(&format!("e{}", c.node_ref)).and_then(Value::as_f64).unwrap_or(0.0);
                ranked.push((c, p, confidence));
            }
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        if chunks.len() == 1 {
            let (chosen, p, confidence) = ranked[0];
            return Ok(Found {
                chosen: chosen.clone(),
                p,
                confidence,
                others: ranked[1..].iter().take(3).map(|(c, p, _)| ((*c).clone(), *p)).collect(),
            });
        }
        // The best few of every chunk, asked again side by side.
        pool = ranked.iter().take(CHUNK.min(3 * chunks.len())).map(|(c, _, _)| *c).collect();
    }
}

/// The probability that the answer to `question` about the page is yes.
pub fn check(decider: &Decider, question: &str, page_text: &str) -> Result<f64> {
    // Refs mean nothing to the question; they are only tokens.
    let text: String = page_text.lines().map(|l| redact(&strip_ref(l))).collect::<Vec<_>>().join("\n");
    let state: String = text.chars().take(CHECK_STATE_CHARS).collect();
    let answers = decider.ask(&state, json!({ "check": { "type": "noul", "instructions": question } }))?;
    answers
        .get("check")
        .and_then(|a| a.get("noul"))
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("the decision model gave no yes/no answer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "page \"Shop\" http://x/shop
- banner:
  - link \"Cart 0\" [ref=9] url=/cart
- main:
  - searchbox \"Search products\" [ref=2]
  - article:
    - heading \"Nickel Hinge\" [level=3]: link [ref=91] url=/shop/p/141
    - text: $34.19
    - button \"Add to cart\" [ref=94]
  - article:
    - heading \"Brass Valve\" [level=3]: link [ref=126] url=/shop/p/143
    - button \"Add to cart\" [ref=129]
- dialog \"Cookie consent\":
  - button \"Accept all\" [ref=164]";

    #[test]
    fn candidates_carry_the_context_that_tells_them_apart() {
        let c = candidates(TREE);
        let refs: Vec<u64> = c.iter().map(|c| c.node_ref).collect();
        assert_eq!(refs, [9, 2, 91, 94, 126, 129, 164]);
        let hinge = c.iter().find(|c| c.node_ref == 94).unwrap();
        assert_eq!(hinge.line, "button \"Add to cart\"");
        assert!(hinge.context.contains("Nickel Hinge"), "{hinge:?}");
        let valve = c.iter().find(|c| c.node_ref == 129).unwrap();
        assert!(valve.context.contains("Brass Valve") && !valve.context.contains("Nickel"), "{valve:?}");
        let accept = c.iter().find(|c| c.node_ref == 164).unwrap();
        assert_eq!(accept.context, "dialog \"Cookie consent\"");
        assert_eq!(strip_ref("link \"Cart 0\" [ref=9] url=/cart"), "link \"Cart 0\" url=/cart");
        assert_eq!(ref_of("textbox \"Password\" [required, ref=4]"), Some(4));
        assert_eq!(strip_ref("textbox \"Password\" [required, ref=4]"), "textbox \"Password\" [required]");
        assert_eq!(ref_of("checkbox \"Remember\" [ref=7, checked]"), Some(7));
        assert_eq!(strip_ref("checkbox \"Remember\" [ref=7, checked]"), "checkbox \"Remember\" [checked]");
        assert_eq!(ref_of("text: prefer=3 or href=4"), None);
        assert_eq!(
            redact("  - textbox \"Card number\" [ref=5] value=\"4242 4242\" x"),
            "  - textbox \"Card number\" [ref=5] value=\"(filled)\" x"
        );
        assert_eq!(redact("combobox \"Sort\" value=\"Name\""), "combobox \"Sort\" value=\"Name\"");
    }
}
