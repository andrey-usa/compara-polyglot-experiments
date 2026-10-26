//! Bench-only contender: canonical ladder session via chromey (spider-rs fork
//! of chromiumoxide; same library name and API). Copy of ../chromiumoxide.
//!
//! Modes (argv[1]): `session` | `eval` | `cold`. Contract matches the other
//! contenders; see bench/ladder/README.md. Env: CHROME_BIN, LADDER_BASE,
//! LADDER_EXTRACT_OUT, LADDER_SHOT_OUT, LADDER_N_EVAL.

use std::env;
use std::time::Instant;

use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::{browser::HeadlessMode, Browser, BrowserConfig};
use futures::StreamExt;
use serde::Deserialize;

const EVAL_TITLE: &str = "(() => document.title)()";
const EVAL_CARD_COUNT: &str = "(() => document.querySelectorAll('.card').length)()";
const EVAL_EXTRACT: &str = r#"(() => Array.from(document.querySelectorAll('.card')).map(c => ({
  t: c.querySelector('.t').textContent,
  p: c.querySelector('.p').textContent,
  v: c.dataset.vendor
})))()"#;
const EVAL_VISIBLE: &str = "(() => document.querySelectorAll('.card:not(.hidden)').length)()";
const EVAL_ROWS: &str = "(() => document.querySelectorAll('#rows tr').length)()";

#[derive(serde::Serialize, Deserialize)]
struct Card {
    t: String,
    p: String,
    v: String,
}

async fn launch() -> anyhow::Result<Browser> {
    let chrome_bin = env::var("CHROME_BIN").unwrap_or_else(|_| "/usr/bin/google-chrome".into());
    let config = BrowserConfig::builder()
        .chrome_executable(chrome_bin)
        // Match the other contenders: new headless mode (old `--headless`
        // is a different, slower legacy path).
        .headless_mode(HeadlessMode::New)
        .args(vec![
            "--disable-blink-features=AutomationControlled",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-infobars",
            "--window-size=1440,900",
            "--no-sandbox",
            "--disable-dev-shm-usage",
        ])
        .build()
        .map_err(|e| anyhow::anyhow!("browser config: {e}"))?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| anyhow::anyhow!("launch: {e}"))?;
    tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if let Err(e) = h {
                eprintln!("[chromey] handler: {e:?}");
            }
        }
    });
    Ok(browser)
}

async fn run_session() -> anyhow::Result<serde_json::Value> {
    let base = env::var("LADDER_BASE")?;
    let mut browser = launch().await?;
    let mut counts = serde_json::Map::new();

    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
    page.goto(format!("{base}/page_a.html"))
        .await
        .map_err(|e| anyhow::anyhow!("goto A: {e}"))?;
    let title: String = page
        .evaluate(EVAL_TITLE)
        .await
        .map_err(|e| anyhow::anyhow!("eval title: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode title: {e}"))?;
    counts.insert("title_a".into(), serde_json::json!(title));
    let cards_n: usize = page
        .evaluate(EVAL_CARD_COUNT)
        .await
        .map_err(|e| anyhow::anyhow!("eval count: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode count: {e}"))?;
    counts.insert("cards_a".into(), serde_json::json!(cards_n));
    let cards: Vec<Card> = page
        .evaluate(EVAL_EXTRACT)
        .await
        .map_err(|e| anyhow::anyhow!("eval extract: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode extract: {e}"))?;
    let extract_out = env::var("LADDER_EXTRACT_OUT")?;
    std::fs::write(&extract_out, serde_json::to_string(&cards)?)?;

    page.find_element("#q")
        .await
        .map_err(|e| anyhow::anyhow!("find #q: {e}"))?
        .click()
        .await
        .map_err(|e| anyhow::anyhow!("click #q: {e}"))?
        .type_str("widget")
        .await
        .map_err(|e| anyhow::anyhow!("type: {e}"))?;
    page.find_element("#search")
        .await
        .map_err(|e| anyhow::anyhow!("find #search: {e}"))?
        .click()
        .await
        .map_err(|e| anyhow::anyhow!("click #search: {e}"))?;
    let visible: usize = page
        .evaluate(EVAL_VISIBLE)
        .await
        .map_err(|e| anyhow::anyhow!("eval visible: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode visible: {e}"))?;
    counts.insert("visible".into(), serde_json::json!(visible));
    let png = page
        .screenshot(
            chromiumoxide::page::ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .build(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("screenshot: {e}"))?;
    let shot_out = env::var("LADDER_SHOT_OUT")?;
    std::fs::write(&shot_out, &png)?;
    counts.insert("shot_bytes".into(), serde_json::json!(png.len()));

    let page2 = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page 2: {e}"))?;
    page2
        .goto(format!("{base}/page_b.html"))
        .await
        .map_err(|e| anyhow::anyhow!("goto B: {e}"))?;
    let title_b: String = page2
        .evaluate(EVAL_TITLE)
        .await
        .map_err(|e| anyhow::anyhow!("eval title_b: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode title_b: {e}"))?;
    counts.insert("title_b".into(), serde_json::json!(title_b));
    let rows: usize = page2
        .evaluate(EVAL_ROWS)
        .await
        .map_err(|e| anyhow::anyhow!("eval rows: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode rows: {e}"))?;
    counts.insert("rows_b".into(), serde_json::json!(rows));

    browser
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("close: {e}"))?;
    Ok(serde_json::Value::Object(counts))
}

async fn run_eval(n: usize) -> anyhow::Result<Vec<f64>> {
    let base = env::var("LADDER_BASE")?;
    let mut browser = launch().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
    page.goto(format!("{base}/page_a.html"))
        .await
        .map_err(|e| anyhow::anyhow!("goto A: {e}"))?;
    let mut lat = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = Instant::now();
        let _: String = page
            .evaluate(EVAL_TITLE)
            .await
            .map_err(|e| anyhow::anyhow!("eval: {e}"))?
            .into_value()
            .map_err(|e| anyhow::anyhow!("decode: {e}"))?;
        lat.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    browser
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("close: {e}"))?;
    Ok(lat)
}

async fn run_cold() -> anyhow::Result<()> {
    let mut browser = launch().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
    let _: i64 = page
        .evaluate("(() => 1 + 1)()")
        .await
        .map_err(|e| anyhow::anyhow!("eval: {e}"))?
        .into_value()
        .map_err(|e| anyhow::anyhow!("decode: {e}"))?;
    browser
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("close: {e}"))?;
    Ok(())
}

async fn eval_str(page: &chromiumoxide::Page, expr: &str) -> anyhow::Result<serde_json::Value> {
    // Retry transient failures: right after navigation the execution context
    // can be torn down, and chromiumoxide surfaces "No value found" (or a
    // context error) even though a retry with a fresh context id succeeds.
    let mut last = String::new();
    for _ in 0..15 {
        match page.evaluate(expr).await {
            Ok(res) => match res.into_value::<serde_json::Value>() {
                Ok(v) => return Ok(v),
                Err(e) => last = format!("decode: {e}"),
            },
            Err(e) => last = format!("eval: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(anyhow::anyhow!("eval_str({expr:?}) failed after retries: {last}"))
}

/// Wait until an eval proves we are in a LIVE execution context of the right
/// page: `window.location.href` must succeed (proves the context isn't
/// stale/torn-down) and match the expected URL, and readyState must be
/// 'complete'. chromiumoxide's goto() resolves right after Page.navigate;
/// checking page.url() (target info, updates early) + a readyState eval (may
/// hit the *old* page's context) can both succeed while the new page's
/// context is still torn down -> later evals fail with "No value found".
async fn wait_for_url_and_ready(page: &chromiumoxide::Page, url_prefix: &str) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let href: Option<String> = page
            .evaluate("window.location.href")
            .await
            .ok()
            .and_then(|r| r.into_value::<serde_json::Value>().ok())
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let url_ok = href
            .as_ref()
            .map(|u| u.starts_with(url_prefix))
            .unwrap_or(false);
        // Only check readyState once the href probe proves a live context on
        // the right page.
        let state: Option<String> = if url_ok {
            page.evaluate("document.readyState")
                .await
                .ok()
                .and_then(|r| r.into_value::<serde_json::Value>().ok())
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        } else {
            None
        };
        if url_ok && state.as_deref() == Some("complete") {
            return Ok(());
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            return Err(anyhow::anyhow!(
                "wait_for_url_and_ready: timeout (href: {href:?}, state: {state:?})"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn run_realworld() -> anyhow::Result<serde_json::Value> {
    let mut browser = launch().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
    let t = std::time::Instant::now();
    page.goto("https://example.com")
        .await
        .map_err(|e| anyhow::anyhow!("goto: {e}"))?;
    wait_for_url_and_ready(&page, "https://example.com").await?;
    let goto_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = std::time::Instant::now();
    // Single eval for title+h1+para: the title-only eval succeeded while a
    // follow-up h1 eval hit "No value found", so avoid sequential evals here.
    let info = eval_str(&page, "({title: document.title, h1: document.querySelector('h1') ? document.querySelector('h1').textContent : null, para: document.querySelector('p') ? document.querySelector('p').textContent.trim().slice(0, 80) : null})").await?;
    let title: Option<String> = info.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
    let title_ms = t.elapsed().as_secs_f64() * 1000.0;
    let h1: Option<String> = info.get("h1").and_then(|v| v.as_str()).map(|s| s.to_string());
    let para: Option<String> = info.get("para").and_then(|v| v.as_str()).map(|s| s.to_string());
    browser.close().await.map_err(|e| anyhow::anyhow!("close: {e}"))?;
    Ok(serde_json::json!({
        "title": title, "h1": h1, "para": para,
        "ops": {"goto_ms": goto_ms, "title_ms": title_ms}
    }))
}

async fn run_browse() -> anyhow::Result<serde_json::Value> {
    let mut browser = launch().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
    let mut facts = serde_json::Map::new();
    let mut ops = serde_json::Map::new();
    // awesome list
    let t = std::time::Instant::now();
    page.goto("https://github.com/sindresorhus/awesome")
        .await
        .map_err(|e| anyhow::anyhow!("goto awesome: {e}"))?;
    wait_for_url_and_ready(&page, "https://github.com/sindresorhus/awesome").await?;
    ops.insert("awesome_goto_ms".into(), serde_json::json!(t.elapsed().as_secs_f64() * 1000.0));
    facts.insert("awesome_title".into(), eval_str(&page, "document.title ?? null").await?);
    facts.insert("awesome_links".into(), eval_str(&page, "document.querySelectorAll('a').length").await?);
    let mut scroll_ys = Vec::new();
    for i in 0..5 {
        let t = std::time::Instant::now();
        let y: f64 = eval_str(&page, "window.scrollBy({ top: 800, behavior: 'instant' }), window.scrollY")
            .await?.as_f64().unwrap_or(0.0);
        scroll_ys.push(y);
        ops.insert(format!("awesome_scroll{i}_ms"), serde_json::json!(t.elapsed().as_secs_f64() * 1000.0));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    facts.insert("awesome_scroll_ys".into(), serde_json::json!(scroll_ys));
    facts.insert("awesome_headings".into(),
        eval_str(&page, "Array.from(document.querySelectorAll('h2')).slice(0, 5).map(h => h.textContent?.trim() ?? null)").await?);
    // trending
    let t = std::time::Instant::now();
    page.goto("https://github.com/trending")
        .await
        .map_err(|e| anyhow::anyhow!("goto trending: {e}"))?;
    wait_for_url_and_ready(&page, "https://github.com/trending").await?;
    ops.insert("trending_goto_ms".into(), serde_json::json!(t.elapsed().as_secs_f64() * 1000.0));
    facts.insert("trending_title".into(), eval_str(&page, "document.title ?? null").await?);
    facts.insert("trending_repos".into(),
        eval_str(&page, "document.querySelectorAll('article.Box-row').length").await?);
    facts.insert("trending_top3".into(),
        eval_str(&page, "Array.from(document.querySelectorAll('article.Box-row h2 a')).slice(0, 3).map(a => (a.textContent ?? '').replace(/\\s+/g, ''))").await?);
    let t = std::time::Instant::now();
    page.find_element("article.Box-row h2 a")
        .await
        .map_err(|e| anyhow::anyhow!("find repo link: {e}"))?
        .click()
        .await
        .map_err(|e| anyhow::anyhow!("click: {e}"))?;
    let mut new_url: Option<String> = None;
    for _ in 0..24 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if let Ok(url) = page.url().await {
            if let Some(u) = url {
                if !u.contains("trending") {
                    new_url = Some(u);
                    break;
                }
            }
        }
    }
    ops.insert("trending_click_ms".into(), serde_json::json!(t.elapsed().as_secs_f64() * 1000.0));
    facts.insert("trending_clicked_url".into(), serde_json::json!(new_url));
    facts.insert("trending_clicked_title".into(), eval_str(&page, "document.title ?? null").await?);
    browser.close().await.map_err(|e| anyhow::anyhow!("close: {e}"))?;
    Ok(serde_json::json!({"facts": facts, "ops": ops}))
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("[chromey] fatal: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = env::args().nth(1).unwrap_or_else(|| "session".into());
    let t0 = Instant::now();
    match mode.as_str() {
        "session" => {
            let counts = run_session().await?;
            println!(
                "{}",
                serde_json::json!({"mode": mode, "wall_ms": t0.elapsed().as_secs_f64() * 1000.0, "counts": counts})
            );
        }
        "eval" => {
            let n: usize = env::var("LADDER_N_EVAL").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
            let lat = run_eval(n).await?;
            println!(
                "{}",
                serde_json::json!({"mode": mode, "wall_ms": t0.elapsed().as_secs_f64() * 1000.0})
            );
            println!("{}", serde_json::json!({"eval_ms": lat}));
        }
        "cold" => {
            run_cold().await?;
            println!(
                "{}",
                serde_json::json!({"mode": mode, "wall_ms": t0.elapsed().as_secs_f64() * 1000.0})
            );
        }
        "realworld" => {
            let rw = run_realworld().await?;
            println!(
                "{}",
                serde_json::json!({"mode": mode, "wall_ms": t0.elapsed().as_secs_f64() * 1000.0, "realworld": rw})
            );
        }
        "browse" => {
            let br = run_browse().await?;
            println!(
                "{}",
                serde_json::json!({"mode": mode, "wall_ms": t0.elapsed().as_secs_f64() * 1000.0, "realworld": br})
            );
        }
        other => return Err(format!("unknown mode {other}").into()),
    }
    Ok(())
}
