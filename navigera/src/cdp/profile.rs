//! Where the throwaway browser profile lives.
//!
//! Every launch gets a fresh `--user-data-dir` that is deleted at close.
//! Chrome still treats it as a profile worth keeping: one cold start
//! (launch, one page, one eval) makes ~210 `fdatasync` calls for its SQLite
//! databases and other files, ~200 ms of syscall time on ext4, part of it on
//! the startup path. On Linux the profile goes to `/dev/shm` (tmpfs), where
//! those calls return at once and deleting the profile is a few unlinks in
//! RAM.
//!
//! `/dev/shm` is used only when it is tmpfs with room to spare (Docker's
//! default is 64 MB), and creating the directory there must succeed (some
//! sandboxes deny it); otherwise the profile goes to the system temp
//! directory as before. `NAVIGERA_PROFILE_DIR=<dir>` picks the parent directory
//! explicitly (diagnostics and A/B runs).
//!
//! A profile in RAM must not outlive its owner: a session server killed with
//! SIGKILL never runs its cleanup. Each profile records its owner (host, pid
//! namespace, pid of the navigera process) and its browser's process group,
//! and each launch first removes the profiles in that directory whose owner
//! is gone and whose browser has fully exited. (A browser that loses its
//! driver shuts down on its own; its network and storage services can still
//! be writing to the profile for a moment after the browser process exits,
//! and would recreate a half-deleted one.)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const PREFIX: &str = "navigera-profile-";

/// The owner record inside each profile (Chrome ignores unknown files).
const OWNER_FILE: &str = ".navigera-owner";

/// Free space `/dev/shm` must have before a profile goes there.
#[cfg(target_os = "linux")]
const SHM_MIN_FREE: u64 = 512 << 20;

/// A fresh, empty profile directory, deleted when dropped.
pub fn temp_profile() -> Result<tempfile::TempDir> {
    let dir = match std::env::var_os("NAVIGERA_PROFILE_DIR") {
        Some(base) => {
            let base = PathBuf::from(base);
            sweep_stale(&base);
            new_dir(Some(&base))
                .with_context(|| format!("create a browser profile in NAVIGERA_PROFILE_DIR={}", base.display()))?
        }
        None => match shm_profile() {
            Some(dir) => dir,
            None => {
                sweep_stale(&std::env::temp_dir());
                new_dir(None).context("create a browser profile in the temp directory")?
            }
        },
    };
    if let Some(owner) = owner_token(std::process::id()) {
        let _ = std::fs::write(dir.path().join(OWNER_FILE), owner);
    }
    Ok(dir)
}

/// Note the launched browser's process group (its pid: browsers lead their
/// own group, see `transport::own_process_group`) in the owner record.
pub fn record_browser(profile: &tempfile::TempDir, browser_pid: u32) {
    if let Some(owner) = owner_token(std::process::id()) {
        let _ = std::fs::write(profile.path().join(OWNER_FILE), format!("{owner} {browser_pid}"));
    }
}

fn new_dir(base: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    let mut b = tempfile::Builder::new();
    b.prefix(PREFIX);
    match base {
        Some(base) => b.tempdir_in(base),
        None => b.tempdir(),
    }
}

#[cfg(target_os = "linux")]
fn shm_profile() -> Option<tempfile::TempDir> {
    let shm = Path::new("/dev/shm");
    if !roomy_tmpfs(shm) {
        return None;
    }
    sweep_stale(shm);
    new_dir(Some(shm)).ok()
}

#[cfg(not(target_os = "linux"))]
fn shm_profile() -> Option<tempfile::TempDir> {
    None
}

/// True when `path` is a tmpfs with at least [`SHM_MIN_FREE`] available.
#[cfg(target_os = "linux")]
fn roomy_tmpfs(path: &Path) -> bool {
    // statfs(2) on 64-bit Linux (glibc and musl share the layout):
    // f_type at byte 0, f_bsize at 8, f_bavail at 32.
    #[cfg(target_pointer_width = "64")]
    {
        use std::os::unix::ffi::OsStrExt;
        const TMPFS_MAGIC: u64 = 0x0102_1994;
        extern "C" {
            fn statfs(path: *const std::ffi::c_char, buf: *mut u64) -> i32;
        }
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
        // Larger than struct statfs (120 bytes) on every 64-bit target.
        let mut buf = [0u64; 32];
        // SAFETY: a valid NUL-terminated path and a buffer bigger than the
        // struct the kernel fills.
        if unsafe { statfs(c_path.as_ptr(), buf.as_mut_ptr()) } != 0 {
            return false;
        }
        let (f_type, f_bsize, f_bavail) = (buf[0], buf[1], buf[4]);
        f_type == TMPFS_MAGIC && f_bavail.saturating_mul(f_bsize) >= SHM_MIN_FREE
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        let _ = path;
        false
    }
}

/// `<host> <pid namespace> <pid>`: who owns a profile, comparable across
/// processes that share the directory (containers may share `/dev/shm`).
fn owner_token(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
        let ns = std::fs::read_link("/proc/self/ns/pid").ok()?;
        Some(format!("{} {} {pid}", host.trim(), ns.display()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Remove this directory's navigera profiles that nothing uses any more:
/// the owner is from this host and pid namespace and no longer runs, and
/// no process of its browser's group is left. Profiles without an owner
/// record (being created right now) or owned elsewhere are left alone.
fn sweep_stale(base: &Path) {
    let Some(mine) = owner_token(0) else { return };
    let mine: Vec<&str> = mine.split_whitespace().collect();
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let dir = entry.path();
        let Ok(owner) = std::fs::read_to_string(dir.join(OWNER_FILE)) else { continue };
        let owner: Vec<&str> = owner.split_whitespace().collect();
        // host, pid namespace, owner pid[, browser process group]
        if owner.len() < 3 || owner[..2] != mine[..2] {
            continue;
        }
        let Ok(pid) = owner[2].parse::<u32>() else { continue };
        let group = owner.get(3).and_then(|g| g.parse::<u32>().ok());
        if running(pid) || group.is_some_and(group_running) {
            continue;
        }
        crate::timing::log(&format!("[profile] removing stale {}", dir.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `/proc/<pid>/stat` fields after the parenthesised command name:
/// state, ppid, pgrp, ...
fn stat_fields(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(stat.rsplit_once(") ")?.1.to_string())
}

/// Running, not a zombie (a killed session server stays one until its
/// parent, often an init that never reaps, collects it).
fn live_state(fields: &str) -> bool {
    !(fields.starts_with('Z') || fields.starts_with('X'))
}

fn running(pid: u32) -> bool {
    stat_fields(&pid.to_string()).is_some_and(|f| live_state(&f))
}

/// Any live process in process group `pgid` (only scanned for profiles
/// whose owner is gone, which is rare).
fn group_running(pgid: u32) -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else { return true };
    let pgid = pgid.to_string();
    procs.flatten().any(|p| {
        let name = p.file_name();
        let name = name.to_string_lossy();
        name.bytes().all(|b| b.is_ascii_digit())
            && stat_fields(&name).is_some_and(|f| live_state(&f) && f.split_whitespace().nth(2) == Some(pgid.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn stale_profiles_are_swept_live_ones_kept() {
        let base = tempfile::tempdir().unwrap();
        let make = |name: &str, owner: Option<String>| {
            let dir = base.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("Local State"), "{}").unwrap();
            if let Some(owner) = owner {
                std::fs::write(dir.join(OWNER_FILE), owner).unwrap();
            }
            dir
        };
        // A pid that is certainly gone: past the kernel's pid_max ceiling.
        let gone = owner_token(4_194_305).unwrap();
        let dead = make(&format!("{PREFIX}dead"), Some(gone.clone()));
        let live = make(&format!("{PREFIX}live"), owner_token(std::process::id()));
        let starting = make(&format!("{PREFIX}starting"), None);
        let elsewhere = make(&format!("{PREFIX}elsewhere"), Some("another-host pid:[1] 4194305".into()));
        let not_ours = make("someone-else", Some(gone.clone()));
        // Owner gone, but its browser's group still has a live process
        // (this test's own group stands in for it): still being written.
        let my_group = stat_fields("self").unwrap().split_whitespace().nth(2).unwrap().to_string();
        let shutting_down = make(&format!("{PREFIX}shutting-down"), Some(format!("{gone} {my_group}")));
        let all_gone = make(&format!("{PREFIX}all-gone"), Some(format!("{gone} 4194305")));
        sweep_stale(base.path());
        assert!(!dead.exists(), "a profile whose owner is gone is removed");
        assert!(!all_gone.exists(), "owner and browser group gone: removed");
        for kept in [live, starting, elsewhere, not_ours, shutting_down] {
            assert!(kept.exists(), "{} must be kept", kept.display());
        }
    }

    #[test]
    fn explicit_parent_directory_is_honoured() {
        let base = tempfile::tempdir().unwrap();
        // Env vars are process-wide: only this test sets this one.
        std::env::set_var("NAVIGERA_PROFILE_DIR", base.path());
        let p = temp_profile();
        std::env::remove_var("NAVIGERA_PROFILE_DIR");
        let p = p.unwrap();
        assert_eq!(p.path().parent(), Some(base.path()));
        assert!(p.path().file_name().unwrap().to_string_lossy().starts_with(PREFIX));
    }
}
