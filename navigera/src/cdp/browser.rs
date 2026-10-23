//! Browser-level CDP: launch, tab targets, shutdown.

use std::process::Child;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use std::sync::Arc;

use super::client::CdpClient;
use super::events::{self, DialogPolicy, Shared};
use super::page::Page;
use super::transport::{self, LaunchedChrome};

const CMD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LaunchOptions {
    pub exe: String,
    pub headless: bool,
    /// Extra Chromium flags (skipped for non-Chromium engines).
    pub chrome_flags: Vec<String>,
    /// Pre-picked remote debugging port (used by engines whose launcher
    /// needs to know it, e.g. the Lightpanda shim).
    pub debugging_port: Option<u16>,
    /// CDP over `--remote-debugging-pipe` instead of a WebSocket port.
    pub pipe: bool,
}

/// A launched browser: one CDP client on the browser target, plus the child
/// process handle so `close` can reap it without orphans.
pub struct Browser {
    handle: tokio::runtime::Handle,
    client: CdpClient,
    child: Mutex<Option<Child>>,
    /// The child leads its own process group (every browser we launch on
    /// Unix): close kills the whole group, not just the browser process.
    kill_group: bool,
    /// Kept alive for the session; the temp profile is deleted by `close`
    /// (or on drop). `None` for engines that need no profile (Lightpanda).
    profile_dir: Mutex<Option<tempfile::TempDir>>,
    ws_url: String,
    /// Chrome's stderr tail (pipe launches), for "why did it die" errors.
    stderr: Option<transport::StderrTail>,
    /// Dialog log, popups and per-tab navigation state (see `events`).
    shared: Arc<Shared>,
    /// Windows: kill-on-close job holding the browser tree, so Chrome dies
    /// with this process even when it is killed (see `procjob`).
    job: Option<super::procjob::ProcJob>,
}

impl Browser {
    /// Launch `lightpanda serve` directly (no Chromium flags, no shim) and
    /// connect the browser-level CDP session.
    ///
    /// Lightpanda is a CDP server, not a Chromium executable: it must be
    /// started as `lightpanda serve --host … --port …`, and the browser
    /// WebSocket URL is discovered via its `/json/version` endpoint.
    pub fn launch_lightpanda(handle: &tokio::runtime::Handle, bin: &str) -> Result<Self> {
        let port = free_port().context("pick a free port for lightpanda serve")?;
        let program = bin.to_string();
        let args = lightpanda_serve_args(port);
        // Write child output to a temp file for diagnostics (not piped to avoid
        // the pipe-buffer deadlock). The file is read only on poll failure.
        let log_path = std::env::temp_dir().join(format!("lightpanda-serve-{port}.log"));
        let log_file = std::fs::File::create(&log_path).ok();
        let log_file_err = log_file.as_ref().and_then(|f| f.try_clone().ok());
        let mut cmd = std::process::Command::new(&program);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .args(&args)
            .envs(LIGHTPANDA_ENV.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(log_file.map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()))
            .stderr(log_file_err.map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()))
            .spawn()
            .with_context(|| format!("spawn `{program} {}`", args.join(" ")))?;
        let ws_url = match transport::poll_ws_url_standalone(port, Duration::from_secs(60)) {
            Ok(url) => url,
            Err(e) => {
                let log_tail = std::fs::read_to_string(&log_path)
                    .map(|s| {
                        let lines: Vec<&str> = s.lines().collect();
                        let start = lines.len().saturating_sub(30);
                        lines[start..].join("\n")
                    })
                    .unwrap_or_default();
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "`{program} {}` did not come up on 127.0.0.1:{port}: {e:#}\nlog tail ({log_path:?}):\n{log_tail}",
                    args.join(" ")
                );
            }
        };
        crate::timing::log(&format!("[browser] lightpanda serve on 127.0.0.1:{port} -> {ws_url}"));
        let client = handle.block_on(CdpClient::connect(&ws_url)).context("CDP connect to lightpanda")?;
        let shared = Arc::new(Shared::default());
        shared.single_target.store(true, std::sync::atomic::Ordering::Relaxed);
        events::spawn(handle, &client, Arc::clone(&shared));
        Ok(Self {
            handle: handle.clone(),
            client,
            child: Mutex::new(Some(child)),
            kill_group: cfg!(unix),
            profile_dir: Mutex::new(None),
            ws_url,
            stderr: None,
            shared,
            job: None,
        })
    }

    /// Launch the engine and connect the browser-level CDP session.
    pub fn launch(handle: &tokio::runtime::Handle, opts: &LaunchOptions) -> Result<Self> {
        let started = std::time::Instant::now();
        #[cfg(unix)]
        if opts.pipe && opts.debugging_port.is_none() {
            let launched =
                transport::launch_chrome_pipe(&opts.exe, opts.headless, &opts.chrome_flags).context("launch chrome")?;
            crate::timing::record("spawn", started);
            let client = {
                let _guard = handle.enter();
                handle.block_on(CdpClient::connect_pipe(launched.from_browser, launched.to_browser))
            }
            .context("CDP pipe connect")?;
            let mut browser =
                Self::finish(handle, client, Some(launched.child), Some(launched.profile_dir), "pipe".into());
            browser.stderr = Some(launched.stderr);
            return Ok(browser);
        }
        let LaunchedChrome { child, profile_dir, ws_url, job } =
            transport::launch_chrome(&opts.exe, opts.headless, &opts.chrome_flags, opts.debugging_port)
                .context("launch chrome")?;
        crate::timing::record("devtools_url", started);
        let connect_started = std::time::Instant::now();
        let client = handle.block_on(CdpClient::connect(&ws_url)).context("CDP connect")?;
        crate::timing::record("ws_connect", connect_started);
        let mut browser = Self::finish(handle, client, Some(child), Some(profile_dir), ws_url);
        browser.job = job;
        Ok(browser)
    }

    /// Connect to a browser someone else runs (`--attach`, `--profile`).
    /// navigera owns no process here: `close` only disconnects, and the
    /// browser, its windows and tabs stay as they are.
    pub fn connect(handle: &tokio::runtime::Handle, ws_url: &str, bound: Duration) -> Result<Self> {
        let started = std::time::Instant::now();
        let client = handle
            .block_on(CdpClient::connect_within(ws_url, bound))
            .with_context(|| format!("connect to the running browser at {ws_url}"))?;
        crate::timing::record("ws_connect", started);
        Ok(Self::finish(handle, client, None, None, ws_url.to_string()))
    }

    fn finish(
        handle: &tokio::runtime::Handle,
        client: CdpClient,
        child: Option<Child>,
        profile_dir: Option<tempfile::TempDir>,
        ws_url: String,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        events::spawn(handle, &client, Arc::clone(&shared));
        // Popups (`target=_blank`, `window.open`) announce themselves as
        // `Target.targetCreated` with an `openerId`; the session adopts them
        // as tabs. One round trip, before any page exists.
        if std::env::var_os("NAVIGERA_NO_DISCOVER").is_none() {
            let _ = handle.block_on(client.send(
                "Target.setDiscoverTargets",
                json!({ "discover": true }),
                None,
                CMD_TIMEOUT,
            ));
        }
        Self {
            handle: handle.clone(),
            client,
            // Launched browsers lead their own process group (see
            // `transport::own_process_group`); attached ones have no child.
            kill_group: cfg!(unix) && child.is_some(),
            child: Mutex::new(child),
            profile_dir: Mutex::new(profile_dir),
            ws_url,
            shared,
            stderr: None,
            job: None,
        }
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.handle.block_on(fut)
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Open a new tab target, attach a session, enable Page/Runtime domains.
    /// Create a page on Lightpanda: it requires an explicit browser context
    /// per connection (one context + one page per process).
    pub fn new_page_lightpanda(&self, url: Option<&str>) -> Result<Page> {
        self.block_on(async {
            let ctx = self
                .client
                .send("Target.createBrowserContext", json!({}), None, CMD_TIMEOUT)
                .await
                .context("Target.createBrowserContext")?;
            let browser_context_id = ctx
                .get("browserContextId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createBrowserContext: no browserContextId"))?
                .to_string();
            let target = self
                .client
                .send(
                    "Target.createTarget",
                    json!({
                        "url": url.unwrap_or("about:blank"),
                        "browserContextId": browser_context_id,
                    }),
                    None,
                    CMD_TIMEOUT,
                )
                .await
                .context("Target.createTarget")?;
            let target_id = target
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createTarget: no targetId"))?
                .to_string();
            let session = self
                .client
                .send("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None, CMD_TIMEOUT)
                .await
                .context("Target.attachToTarget")?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attachToTarget: no sessionId"))?
                .to_string();
            self.shared.register(&session_id, &target_id, "");
            let page = Page::new(
                self.handle.clone(),
                self.client.clone(),
                session_id.clone(),
                target_id,
                Arc::clone(&self.shared),
            );
            page.enable().await?;
            events::arm_dialog(&self.client, &self.shared, &session_id).await;
            Ok(page)
        })
    }

    /// Wait (briefly) until the event watcher has handled every event that
    /// arrived so far (see [`Shared::caught_up`]).
    pub fn caught_up(&self) -> bool {
        self.shared.caught_up(&self.client, Duration::from_millis(250))
    }

    /// Report downloads (`Browser.downloadWillBegin`/`downloadProgress`),
    /// saving them in `dir`, or where the browser would (no `dir`: a browser
    /// that is the user's own keeps its own setting).
    pub fn enable_downloads(&self, dir: Option<&std::path::Path>) -> Result<()> {
        self.enable_downloads_in(dir, None)
    }

    /// [`Browser::enable_downloads`] for one browser context (the default
    /// one without `context`).
    pub fn enable_downloads_in(&self, dir: Option<&std::path::Path>, context: Option<&str>) -> Result<()> {
        let mut params = match dir {
            Some(d) => {
                json!({ "behavior": "allowAndName", "downloadPath": d.display().to_string(), "eventsEnabled": true })
            }
            None => json!({ "behavior": "default", "eventsEnabled": true }),
        };
        if let Some(context) = context {
            params["browserContextId"] = json!(context);
        }
        self.block_on(self.client.send("Browser.setDownloadBehavior", params, None, CMD_TIMEOUT))?;
        self.shared.downloads.lock().unwrap().dir = dir.map(std::path::Path::to_path_buf);
        Ok(())
    }

    /// Grant a permission (e.g. `geolocation`) to every origin.
    pub fn grant_permission(&self, permission: &str) -> Result<()> {
        self.block_on(self.client.send(
            "Browser.setPermission",
            json!({ "permission": { "name": permission }, "setting": "granted" }),
            None,
            CMD_TIMEOUT,
        ))
        .map(|_| ())
    }

    /// The watcher's shared state (dialog log, popups, navigation).
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// How dialogs in `scope` (a session's browser context, "" for the
    /// default one) are answered from now on.
    pub fn set_dialog_policy(&self, scope: &str, policy: DialogPolicy) {
        self.shared.policy.lock().unwrap().insert(scope.to_string(), policy);
        if self.shared.is_single_target() {
            let sessions: Vec<String> = self.shared.sessions.lock().unwrap().keys().cloned().collect();
            for session in sessions {
                self.block_on(events::arm_dialog(&self.client, &self.shared, &session));
            }
        }
    }

    /// Attach to a page target the browser opened on its own (a popup) and
    /// enable it like a tab we created. `scope`: the browser context of the
    /// session it belongs to ("" for the default one).
    pub fn attach_page(&self, target_id: &str, scope: &str) -> Result<Page> {
        self.block_on(async {
            let session = self
                .client
                .send("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None, CMD_TIMEOUT)
                .await
                .context("Target.attachToTarget")?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attachToTarget: no sessionId"))?
                .to_string();
            self.shared.register(&session_id, target_id, scope);
            let page = Page::new(
                self.handle.clone(),
                self.client.clone(),
                session_id.clone(),
                target_id.to_string(),
                Arc::clone(&self.shared),
            );
            page.enable().await?;
            // The popup's navigation started before we attached: read where
            // it stands so the next op waits for it instead of reading the
            // initial about:blank.
            let target_url = self
                .client
                .send("Target.getTargetInfo", json!({ "targetId": target_id }), None, CMD_TIMEOUT)
                .await
                .ok()
                .and_then(|i| i.pointer("/targetInfo/url").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            let doc = self
                .client
                .send(
                    "Runtime.evaluate",
                    json!({ "expression": "[document.readyState, location.href]", "returnByValue": true }),
                    Some(&session_id),
                    CMD_TIMEOUT,
                )
                .await
                .ok()
                .and_then(|r| r.pointer("/result/value").cloned())
                .unwrap_or_default();
            let ready = doc.get(0).and_then(Value::as_str).unwrap_or("complete");
            let href = doc.get(1).and_then(Value::as_str).unwrap_or("");
            let pending = href == "about:blank" && !target_url.is_empty() && target_url != "about:blank";
            self.shared.seed_nav(&session_id, ready, pending);
            Ok(page)
        })
    }

    pub fn new_page(&self, url: Option<&str>) -> Result<Page> {
        self.new_page_in(url, false)
    }

    /// [`Browser::new_page`] in a window of its own: in a user's browser
    /// the agent works beside their tabs, not among them.
    pub fn new_page_in(&self, url: Option<&str>, new_window: bool) -> Result<Page> {
        self.new_page_with(url, new_window, None)
    }

    /// A new browser context: its own cookies, storage and cache, as a
    /// fresh incognito profile has.
    pub fn create_context(&self) -> Result<String> {
        let r = self.block_on(self.client.send("Target.createBrowserContext", json!({}), None, CMD_TIMEOUT))?;
        r.get("browserContextId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("createBrowserContext: no browserContextId"))
    }

    /// Every cookie of a browser context (the default one without
    /// `context`). Asked of the browser itself: a page's session may not
    /// name a context.
    pub fn context_cookies(&self, context: Option<&str>) -> Result<Value> {
        let params = match context {
            Some(c) => json!({ "browserContextId": c }),
            None => json!({}),
        };
        self.block_on(self.client.send("Storage.getCookies", params, None, CMD_TIMEOUT))
    }

    /// Close a context made by [`Browser::create_context`] and its tabs.
    pub fn dispose_context(&self, id: &str) -> Result<()> {
        self.block_on(self.client.send(
            "Target.disposeBrowserContext",
            json!({ "browserContextId": id }),
            None,
            CMD_TIMEOUT,
        ))
        .map(|_| ())
    }

    /// A tab, in a window of its own or not, in a context or the default one.
    pub fn new_page_with(&self, url: Option<&str>, new_window: bool, context: Option<&str>) -> Result<Page> {
        let mut params = json!({ "url": url.unwrap_or("about:blank") });
        if new_window {
            params["newWindow"] = json!(true);
        }
        if let Some(context) = context {
            params["browserContextId"] = json!(context);
        }
        self.block_on(async {
            let target = self
                .client
                .send("Target.createTarget", params, None, CMD_TIMEOUT)
                .await
                .context("Target.createTarget")?;
            let target_id = target
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("createTarget: no targetId"))?
                .to_string();
            let session = self
                .client
                .send("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None, CMD_TIMEOUT)
                .await
                .context("Target.attachToTarget")?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attachToTarget: no sessionId"))?
                .to_string();
            self.shared.register(&session_id, &target_id, context.unwrap_or(""));
            let page = Page::new(
                self.handle.clone(),
                self.client.clone(),
                session_id.clone(),
                target_id,
                Arc::clone(&self.shared),
            );
            page.enable().await?;
            events::arm_dialog(&self.client, &self.shared, &session_id).await;
            Ok(page)
        })
    }

    /// List open tab targets: (target_id, url).
    pub fn tab_targets(&self) -> Result<Vec<(String, String)>> {
        self.block_on(async {
            let targets = self.client.send("Target.getTargets", json!({}), None, CMD_TIMEOUT).await?;
            let mut out = Vec::new();
            if let Some(list) = targets.get("targetInfos").and_then(Value::as_array) {
                for t in list {
                    if t.get("type").and_then(Value::as_str) != Some("page") {
                        continue;
                    }
                    let id = t.get("targetId").and_then(Value::as_str).unwrap_or("");
                    let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                    out.push((id.to_string(), url.to_string()));
                }
            }
            Ok(out)
        })
    }

    /// `Browser.close`: the browser shuts down gracefully (profile saved).
    /// The reply may never come: the connection drops as it exits.
    pub fn close_remote(&self) -> Result<()> {
        let _ = self.block_on(self.client.send("Browser.close", json!({}), None, Duration::from_secs(5)));
        Ok(())
    }

    pub fn activate_target(&self, target_id: &str) -> Result<()> {
        self.block_on(async {
            self.client.send("Target.activateTarget", json!({ "targetId": target_id }), None, CMD_TIMEOUT).await?;
            Ok(())
        })
    }

    pub fn close_target(&self, target_id: &str) -> Result<()> {
        self.block_on(async {
            self.client.send("Target.closeTarget", json!({ "targetId": target_id }), None, CMD_TIMEOUT).await?;
            Ok(())
        })
    }

    /// If the browser process has exited (crashed, killed), how.
    /// Exit status plus the browser's last stderr lines, for errors when
    /// the browser died (at launch or mid-session).
    pub fn death_report(&self) -> String {
        // The pipe closes as soon as the browser dies, often before the drain
        // thread has read its last lines, and those say why ("No usable
        // sandbox!", which the launch fallback looks for). Wait for the end
        // of the stream while the browser is dying; a browser still running
        // after 50 ms is not dying, and its error needs no stderr.
        if let Some(stderr) = &self.stderr {
            let started = std::time::Instant::now();
            while !stderr.closed() && started.elapsed() < Duration::from_millis(300) {
                if started.elapsed() > Duration::from_millis(50) && self.exit_status().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let status = self.exit_status().unwrap_or_else(|| "no exit status yet".into());
        let tail = self.stderr.as_ref().map(|s| s.tail()).filter(|t| !t.is_empty());
        let fatal = self.stderr.as_ref().and_then(|s| s.first_fatal());
        let fatal = fatal.filter(|f| !tail.as_ref().is_some_and(|t| t.contains(f.as_str())));
        let mut report = format!("exited: {status}");
        if let Some(fatal) = fatal {
            report.push_str(&format!("; it logged: {fatal}"));
        }
        if let Some(tail) = tail {
            report.push_str(&format!("; its stderr ends with:\n{tail}"));
        }
        report
    }

    pub fn exit_status(&self) -> Option<String> {
        let mut slot = self.child.lock().ok()?;
        let child = slot.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            _ => None,
        }
    }

    /// Shut the browser down: kill it, delete its profile, reap it.
    ///
    /// The profile is a throwaway temp dir, so Chrome's graceful shutdown
    /// (flushing prefs, history, caches) saves nothing we keep — it only
    /// costs time: navigera used to send `Browser.close` and wait up to
    /// 500 ms for the process to exit, which every session and cold start
    /// paid. go-rod (leakless) and chromiumoxide (kill-on-drop) don't wait
    /// for it either. Renderer and helper processes exit on their own once
    /// the browser process is gone.
    ///
    /// On Unix nothing here waits for the kernel to tear the killed browser
    /// down (~20 ms for Chrome): SIGKILL can't be ignored, the profile is
    /// deleted meanwhile (unlinking files a dying process still holds is
    /// fine, and a removed directory takes no new entries), and the zombie
    /// is reaped on a background thread — or by init once we exit. Only if
    /// the delete fails (a file appeared mid-walk) do we wait and retry.
    /// Windows can't delete files a process still holds open, so there the
    /// browser is reaped first.
    pub fn close(&self) {
        let started = std::time::Instant::now();
        self.client.shutdown();
        let profile = self.profile_dir.lock().ok().and_then(|mut slot| slot.take());
        let Some(mut child) = self.child.lock().ok().and_then(|mut slot| slot.take()) else {
            drop(profile);
            crate::timing::record("close", started);
            return;
        };
        if self.kill_group {
            // Negative pid = the whole process group (wrapper and all).
            #[cfg(unix)]
            {
                extern "C" {
                    fn kill(pid: i32, sig: i32) -> i32;
                }
                // SAFETY: plain syscall; the group was created at spawn.
                unsafe {
                    kill(-(child.id() as i32), 9);
                }
            }
        }
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = child.kill();
        let deleted = cfg!(unix) && profile.as_ref().is_none_or(|p| std::fs::remove_dir_all(p.path()).is_ok());
        if deleted {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        } else {
            let _ = child.wait();
        }
        // Deletes the profile if it is still there (a no-op otherwise).
        drop(profile);
        crate::timing::record("close", started);
    }
}

/// Environment for `lightpanda serve`: no usage telemetry.
const LIGHTPANDA_ENV: &[(&str, &str)] = &[("LIGHTPANDA_DISABLE_TELEMETRY", "1")];

/// `lightpanda serve` arguments. `serve` loads no sub-resources by default:
/// iframes (card and support widgets) would be empty documents, and without
/// stylesheets `display:none` content would count as visible.
fn lightpanda_serve_args(port: u16) -> Vec<String> {
    let port = port.to_string();
    ["serve", "--host", "127.0.0.1", "--port", &port, "--load-resources", "iframe", "--load-resources", "stylesheet"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Pick a free localhost TCP port by binding to port 0 and releasing it.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind 127.0.0.1:0 for a free port")?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightpanda_serve_loads_iframes_and_stylesheets_without_telemetry() {
        let args = lightpanda_serve_args(9333);
        assert_eq!(&args[..5], ["serve", "--host", "127.0.0.1", "--port", "9333"]);
        let loads: Vec<&str> = args.windows(2).filter(|w| w[0] == "--load-resources").map(|w| w[1].as_str()).collect();
        assert_eq!(loads, ["iframe", "stylesheet"]);
        assert!(!args.iter().any(|a| a.contains("xvfb")), "serve is headless: {args:?}");
        assert_eq!(LIGHTPANDA_ENV, [("LIGHTPANDA_DISABLE_TELEMETRY", "1")]);
    }
}
