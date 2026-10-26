//! The `[server]` of `primavera.toml`: the app under test, started before
//! the browser and stopped, with everything it started, after the run.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Server;

pub struct Running {
    child: Option<Child>,
}

/// Whether something answers at `url` (any status under 500).
fn answers(url: &str) -> bool {
    let how = crate::http::How { follow: false, insecure: true };
    crate::http::request("GET", url, &[], None, Duration::from_secs(2), how).is_ok_and(|r| r.status < 500)
}

fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}

/// Start it and wait until its URL answers; `reuse` one already there.
pub fn start(s: &Server) -> Result<Running, String> {
    let reuse = s.reuse.unwrap_or(!crate::runner::under_ci());
    if answers(&s.url) {
        if reuse {
            eprintln!("server: {} already answers; using it", s.url);
            return Ok(Running { child: None });
        }
        return Err(format!(
            "server: something already answers at {} (stop it, or set `reuse = true` in [server])",
            s.url
        ));
    }
    let log = PathBuf::from(".primavera").join("server.log");
    let _ = std::fs::create_dir_all(".primavera");
    let out = std::fs::File::create(&log).map_err(|e| format!("server: {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| format!("server: {e}"))?;
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &s.command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &s.command]);
        c
    };
    if let Some(dir) = &s.cwd {
        cmd.current_dir(dir);
    }
    cmd.envs(&s.env).stdin(Stdio::null()).stdout(out).stderr(err);
    let child = cmd.spawn().map_err(|e| format!("server: `{}`: {e}", s.command))?;
    let mut running = Running { child: Some(child) };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(s.timeout_s);
    loop {
        if let Some(status) = running.child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            running.child = None;
            return Err(format!(
                "server: `{}` exited ({status}) before {} answered; the end of {}:\n{}",
                s.command,
                s.url,
                log.display(),
                tail(&log)
            ));
        }
        if answers(&s.url) {
            eprintln!("server: {} answers ({:.1} s)", s.url, started.elapsed().as_secs_f64());
            return Ok(running);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "server: {} didn't answer within {} s; the end of {}:\n{}",
                s.url,
                s.timeout_s,
                log.display(),
                tail(&log)
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Every process below `pid` (its children's children too).
#[cfg(not(windows))]
fn descendants(pid: u32) -> Vec<u32> {
    let out = Command::new("pgrep").args(["-P", &pid.to_string()]).output();
    let kids: Vec<u32> = out
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default();
    kids.iter().flat_map(|&k| std::iter::once(k).chain(descendants(k))).collect()
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
        }
        #[cfg(not(windows))]
        {
            let tree: Vec<String> =
                std::iter::once(child.id()).chain(descendants(child.id())).map(|p| p.to_string()).collect();
            // Stopped means every process of it gone, not only the shell: one
            // below it can hold the port a moment after the shell exits.
            let gone = |pid: &str| match Command::new("ps").args(["-o", "stat=", "-p", pid]).output() {
                Ok(o) => {
                    let stat = String::from_utf8_lossy(&o.stdout);
                    stat.trim().is_empty() || stat.trim().starts_with('Z')
                }
                Err(_) => true,
            };
            let mut stopped = |within: Duration| {
                let deadline = Instant::now() + within;
                loop {
                    let reaped = child.try_wait().ok().flatten().is_some();
                    if reaped && tree[1..].iter().all(|p| gone(p)) {
                        return true;
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            };
            let _ = Command::new("kill").arg("-TERM").args(&tree).output();
            if !stopped(Duration::from_secs(5)) {
                let _ = Command::new("kill").arg("-KILL").args(&tree).stderr(Stdio::null()).output();
                stopped(Duration::from_secs(2));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
