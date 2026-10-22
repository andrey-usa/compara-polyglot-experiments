//! `--from`: a navigera profile seeded from the user's own browser.
//!
//! `--attach` hands the agent the user's running Chrome itself, and needs
//! Chrome 144+ with remote debugging turned on and a click on Allow. `--from`
//! hands it a copy instead: the logins, cookies and site storage of one of
//! the user's profiles, copied into a `--profile` browser of the agent's own.
//! It works on any Chrome version, needs no switch turned on, leaves the
//! user's browser and windows alone, and the agent's browser can then run
//! headless (with `--natural`) while the user keeps working in theirs.
//!
//! What is copied: the cookies (`Network/`, older Chromes' `Cookies`), site
//! storage (`Local Storage`, `IndexedDB`, `WebStorage`, service worker
//! registrations), saved logins and autofill (`Login Data`, `Web Data`),
//! bookmarks and preferences; and `Local State`, which holds the key the
//! cookies are encrypted with on Windows. Not caches, history, open tabs,
//! sync or extensions. The encryption key itself stays where Chrome keeps
//! it -- the Keychain on macOS, the user's DPAPI key on Windows, the desktop
//! keyring on Linux -- so the copy opens on the same machine for the same
//! user, with the same browser. It is a snapshot: logins made later in the
//! user's browser are not in it until it is copied again.
//!
//! Chrome on Windows keeps its cookie database locked while it runs, so
//! there the user's browser has to be closed for the copy; on macOS and
//! Linux it can stay open (the copy is of the database as last written).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Entries of a profile directory that carry who the user is signed in as,
/// and what sites remember of them.
const COPIED: &[&str] = &[
    "Network",
    "Cookies",
    "Cookies-journal",
    "Local Storage",
    "IndexedDB",
    "WebStorage",
    "Service Worker/Database",
    "Login Data",
    "Login Data-journal",
    "Login Data For Account",
    "Login Data For Account-journal",
    "Web Data",
    "Web Data-journal",
    "Account Web Data",
    "Account Web Data-journal",
    "Bookmarks",
    "Preferences",
    "Secure Preferences",
];

/// Copies a profile of the browser `from` names -- a channel (`chrome`,
/// `beta`, `dev`, `canary`, `chromium`, `edge`) or a user data directory --
/// into the user data directory `dest` of the `--profile` called `name`,
/// as its default profile. `which` picks the profile by directory (`Profile
/// 1`), display name or signed-in email; the default is the one the browser
/// used last. Returns a line saying what was copied.
pub fn import_profile(from: &str, which: Option<&str>, dest: &Path, name: &str) -> Result<String> {
    let (source, label) = match super::attach::default_user_data_dir(from) {
        Some(dir) => (dir, from.to_string()),
        None => (PathBuf::from(from), format!("user data directory {from}")),
    };
    if !source.is_dir() {
        bail!("--from {from}: no {} (is that browser installed?)", source.display());
    }
    if same_dir(&source, dest) {
        bail!("--from {from}: that is the --profile's own directory");
    }
    if super::attach::live_endpoint(dest).is_some() {
        bail!(
            "--from: the browser of profile {name} is running; `quit --close-browser` it first, so its \
             profile can be replaced"
        );
    }
    let local_state: Value = std::fs::read_to_string(source.join("Local State"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let profile = pick_profile(&local_state, which, &source).with_context(|| format!("--from {from}"))?;
    let profile_dir = source.join(&profile);
    if !profile_dir.is_dir() {
        bail!("--from {from}: no profile {profile} in {}", source.display());
    }

    // Replace what the destination held: only a directory navigera made,
    // or none at all -- never a directory of the user's that happens to be
    // named.
    if dest.exists() {
        let ours = dest.join("Default").is_dir() || dest.join("Local State").is_file();
        let empty = std::fs::read_dir(dest).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !ours && !empty {
            bail!(
                "--from: {} is not empty and not a browser profile; give --profile a new name or an empty directory",
                dest.display()
            );
        }
        std::fs::remove_dir_all(dest).with_context(|| format!("clear {}", dest.display()))?;
    }
    let target = dest.join("Default");
    std::fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;

    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in COPIED {
        let src = profile_dir.join(entry);
        if !src.exists() {
            continue;
        }
        copy_tree(&src, &target.join(entry), &mut files, &mut bytes).map_err(|e| {
            let locked = e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33);
            if locked {
                anyhow::anyhow!(
                    "--from {from}: {} is in use: close {label} and run again (Chrome on Windows locks \
                     its cookies while it runs), or use --attach (Chrome 144+)",
                    src.display()
                )
            } else {
                anyhow::anyhow!("--from {from}: copy {}: {e}", src.display())
            }
        })?;
    }
    settle_preferences(&target.join("Preferences"));
    write_local_state(&local_state, &profile, &dest.join("Local State"))?;

    let info = &local_state["profile"]["info_cache"][&profile];
    let who = [info["user_name"].as_str(), info["name"].as_str()]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    Ok(format!("copied {label} profile {profile}{who} into profile {name}: {files} files, {} KB", bytes / 1024))
}

/// The profile directory to copy: `which` as a directory name, a display
/// name or an email; else the one the browser used last; else `Default`.
fn pick_profile(local_state: &Value, which: Option<&str>, source: &Path) -> Result<String> {
    let cache = local_state["profile"]["info_cache"].as_object();
    let Some(which) = which else {
        let last = local_state["profile"]["last_used"].as_str().unwrap_or("Default");
        return Ok(if source.join(last).is_dir() { last.to_string() } else { "Default".to_string() });
    };
    if source.join(which).is_dir() && cache.is_none_or(|c| c.contains_key(which) || which == "Default") {
        return Ok(which.to_string());
    }
    if let Some(cache) = cache {
        let wanted = which.to_lowercase();
        for (dir, info) in cache {
            let names = [info["name"].as_str(), info["user_name"].as_str(), info["gaia_name"].as_str()];
            if names.into_iter().flatten().any(|n| n.to_lowercase() == wanted) {
                return Ok(dir.clone());
            }
        }
        let known: Vec<String> = cache
            .iter()
            .map(|(dir, info)| {
                let label = [info["name"].as_str(), info["user_name"].as_str()]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{dir} ({label})")
            })
            .collect();
        bail!("no profile {which:?}; it has {}", known.join("; "));
    }
    bail!("no profile {which:?}")
}

/// `Local State` for the copy: the source's, with its encryption keys, and
/// the copied profile as the only one, under the name `Default`.
fn write_local_state(source: &Value, profile: &str, to: &Path) -> Result<()> {
    let mut state = if source.is_object() { source.clone() } else { serde_json::json!({}) };
    let info = source["profile"]["info_cache"].get(profile).cloned().unwrap_or_else(|| serde_json::json!({}));
    if let Some(p) = state.get_mut("profile").and_then(Value::as_object_mut) {
        p.insert("info_cache".into(), serde_json::json!({ "Default": info }));
        p.insert("last_used".into(), "Default".into());
        p.insert("last_active_profiles".into(), serde_json::json!(["Default"]));
        p.remove("profiles_order");
    }
    std::fs::write(to, serde_json::to_vec(&state)?).with_context(|| format!("write {}", to.display()))
}

/// The copy's preferences: closed cleanly (no "restore pages?" bubble), and
/// opening on a new tab rather than the user's startup pages.
fn settle_preferences(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut prefs) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if let Some(p) = prefs.as_object_mut() {
        let profile = p.entry("profile").or_insert_with(|| serde_json::json!({}));
        if let Some(profile) = profile.as_object_mut() {
            profile.insert("exit_type".into(), "Normal".into());
            profile.insert("exited_cleanly".into(), true.into());
        }
        let session = p.entry("session").or_insert_with(|| serde_json::json!({}));
        if let Some(session) = session.as_object_mut() {
            session.insert("restore_on_startup".into(), 5.into());
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&prefs) {
        let _ = std::fs::write(path, bytes);
    }
}

fn copy_tree(src: &Path, dst: &Path, files: &mut u64, bytes: &mut u64) -> std::io::Result<()> {
    let meta = std::fs::metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            // A lock file is the running browser's, not data.
            if name == "LOCK" || name == "SingletonLock" {
                continue;
            }
            copy_tree(&entry.path(), &dst.join(&name), files, bytes)?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        *bytes += std::fs::copy(src, dst)?;
        *files += 1;
    }
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_browser(root: &Path) {
        std::fs::create_dir_all(root.join("Default/Network")).unwrap();
        std::fs::create_dir_all(root.join("Profile 1/Local Storage/leveldb")).unwrap();
        std::fs::create_dir_all(root.join("Profile 1/Cache/Cache_Data")).unwrap();
        std::fs::write(root.join("Profile 1/Cookies"), "c").unwrap();
        std::fs::write(root.join("Profile 1/Local Storage/leveldb/000003.log"), "ls").unwrap();
        std::fs::write(root.join("Profile 1/Local Storage/leveldb/LOCK"), "").unwrap();
        std::fs::write(root.join("Profile 1/Cache/Cache_Data/f_000001"), "big").unwrap();
        std::fs::write(root.join("Profile 1/History"), "h").unwrap();
        std::fs::write(
            root.join("Profile 1/Preferences"),
            r#"{"profile":{"exit_type":"Crashed"},"session":{"restore_on_startup":4}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("Local State"),
            r#"{"os_crypt":{"encrypted_key":"k"},"profile":{"last_used":"Profile 1","info_cache":{
                "Default":{"name":"Person 1","user_name":""},
                "Profile 1":{"name":"Work","user_name":"me@example.com"}}}}"#,
        )
        .unwrap();
    }

    #[test]
    fn copies_the_signed_in_state_of_one_profile_and_nothing_else() {
        let src = tempfile::tempdir().unwrap();
        fake_browser(src.path());
        let dest = tempfile::tempdir().unwrap();
        let dest = dest.path().join("agent");
        let note = import_profile(&src.path().display().to_string(), None, &dest, "agent").unwrap();
        assert!(note.contains("Profile 1") && note.contains("me@example.com"), "{note}");
        assert!(dest.join("Default/Cookies").is_file());
        assert!(dest.join("Default/Local Storage/leveldb/000003.log").is_file());
        assert!(!dest.join("Default/Local Storage/leveldb/LOCK").exists());
        assert!(!dest.join("Default/Cache").exists());
        assert!(!dest.join("Default/History").exists());
        let state: Value = serde_json::from_str(&std::fs::read_to_string(dest.join("Local State")).unwrap()).unwrap();
        assert_eq!(state["os_crypt"]["encrypted_key"], "k");
        assert_eq!(state["profile"]["last_used"], "Default");
        assert_eq!(state["profile"]["info_cache"]["Default"]["user_name"], "me@example.com");
        let prefs: Value =
            serde_json::from_str(&std::fs::read_to_string(dest.join("Default/Preferences")).unwrap()).unwrap();
        assert_eq!(prefs["profile"]["exit_type"], "Normal");
        assert_eq!(prefs["session"]["restore_on_startup"], 5);
    }

    #[test]
    fn a_profile_is_picked_by_directory_name_or_email() {
        let src = tempfile::tempdir().unwrap();
        fake_browser(src.path());
        let s = src.path().display().to_string();
        let dest = tempfile::tempdir().unwrap();
        let d = dest.path().join("a");
        assert!(import_profile(&s, Some("Default"), &d, "a").unwrap().contains("profile Default"));
        assert!(import_profile(&s, Some("work"), &d, "a").unwrap().contains("Profile 1"));
        assert!(import_profile(&s, Some("ME@example.com"), &d, "a").unwrap().contains("Profile 1"));
        let err = format!("{:#}", import_profile(&s, Some("nobody"), &d, "a").unwrap_err());
        assert!(err.contains("Profile 1 (Work, me@example.com)"), "{err}");
    }

    #[test]
    fn a_users_own_directory_is_never_replaced() {
        let src = tempfile::tempdir().unwrap();
        fake_browser(src.path());
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("notes.txt"), "mine").unwrap();
        let err = import_profile(&src.path().display().to_string(), None, dest.path(), "x").unwrap_err();
        assert!(err.to_string().contains("not empty and not a browser profile"), "{err}");
        assert!(dest.path().join("notes.txt").is_file());
    }
}
