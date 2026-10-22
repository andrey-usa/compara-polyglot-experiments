//! How much memory a comparison can count on: the machine's, or the
//! container's when it runs inside one.
//!
//! The text engine has two ways to join. When both files fit in memory it
//! reads each one twice -- once to index it, once to compare -- and the second
//! read is served from the page cache. When they do not fit, that second read
//! goes back to the disk, and joining A's rows while its sweep still has them
//! is the faster plan. Which side of the line a pair falls on depends on the
//! machine it runs on, so the line is drawn from what that machine reports
//! rather than from a constant.

/// What [`probe`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Memory {
    /// Physical memory, or the container's limit when that is lower.
    pub limit: u64,
    /// What the kernel says can be had now without swapping -- free memory
    /// plus the cache it would reclaim -- within the container's limit.
    pub available: u64,
}

/// The input size, in bytes, past which a pair is joined inside A's sweep.
///
/// The in-memory plan needs both files resident through its second read,
/// plus its indexes -- a fifth of the input on narrow rows, which is the
/// floor measured at 10M rows (701 MB for a 3.5 GB pair). Half of what is
/// available leaves room for both and for the rest of the machine. On an idle
/// 16 GB host that is about 7.5 GB, next to the 8 GB the line was hard-coded
/// at, which was measured on hosts of that size; on 64 GB it is 30, and in a
/// container limited to 2 GB it is 1.
///
/// [`crate::Options::memory`] overrides the probe, and so does
/// `COMPARA_MEMORY` (bytes, or with a K, M, G or T suffix). Where nothing can
/// be read, 16 GB is assumed.
pub fn streaming_threshold(budget: Option<u64>) -> u64 {
    available(budget) / 2
}

/// What a comparison may count on: the budget it was given, `COMPARA_MEMORY`,
/// or what the machine has free without swapping, in that order; 16 GB where
/// nothing can be read.
pub fn available(budget: Option<u64>) -> u64 {
    stated(budget)
        .or_else(|| probe().map(|m| m.available.min(m.limit)))
        .unwrap_or(16 << 30)
}

/// The budget someone set, by `--memory` or `COMPARA_MEMORY`, as distinct from
/// one the probe found. A stated budget is a limit to keep to; a probed one is
/// only a guide to planning.
pub fn stated(budget: Option<u64>) -> Option<u64> {
    budget.or_else(from_env)
}

fn from_env() -> Option<u64> {
    parse_size(&std::env::var("COMPARA_MEMORY").ok()?)
}

/// `"4G"`, `"512M"`, `"2048"` (bytes). Case-insensitive; an optional trailing
/// `B` or `iB` is accepted.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let lower = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (digits, shift) = match lower.chars().last()? {
        'k' => (&lower[..lower.len() - 1], 10),
        'm' => (&lower[..lower.len() - 1], 20),
        'g' => (&lower[..lower.len() - 1], 30),
        't' => (&lower[..lower.len() - 1], 40),
        _ => (lower, 0),
    };
    let n: u64 = digits.trim().parse().ok()?;
    n.checked_mul(1u64 << shift).filter(|&n| n > 0)
}

/// Reads the machine's memory, and the container's limit if there is one.
/// `None` where the platform gives no answer.
pub fn probe() -> Option<Memory> {
    let mut m = os::probe()?;
    if let Some(limit) = cgroup_limit() {
        m.limit = m.limit.min(limit);
        m.available = m.available.min(limit);
    }
    Some(m)
}

/// The tightest memory limit on this process's cgroup or any cgroup above it,
/// v2 or v1. A limit at or past 2^60 is the kernel's way of saying none.
fn cgroup_limit() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let mut best: Option<u64> = None;
        let mut take = |v: Option<u64>| {
            if let Some(v) = v.filter(|&v| v < (1u64 << 60)) {
                best = Some(best.map_or(v, |b| b.min(v)));
            }
        };
        for line in own.lines() {
            let mut parts = line.splitn(3, ':');
            let (id, controllers, path) = (parts.next(), parts.next(), parts.next());
            let (Some(id), Some(controllers), Some(path)) = (id, controllers, path) else {
                continue;
            };
            let (root, file) = if id == "0" && controllers.is_empty() {
                ("/sys/fs/cgroup", "memory.max")
            } else if controllers.split(',').any(|c| c == "memory") {
                ("/sys/fs/cgroup/memory", "memory.limit_in_bytes")
            } else {
                continue;
            };
            // Inside a container the path can name the host's hierarchy while
            // the container sees its own cgroup at the root, so the root is
            // read as well as every level of the path.
            let mut dir = std::path::PathBuf::from(root);
            take(read_number(&dir.join(file)));
            for part in path.split('/').filter(|p| !p.is_empty()) {
                dir.push(part);
                take(read_number(&dir.join(file)));
            }
        }
        best
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_number(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
mod os {
    use super::Memory;

    pub fn probe() -> Option<Memory> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |name: &str| -> Option<u64> {
            let line = text.lines().find(|l| l.starts_with(name))?;
            let kb: u64 = line[name.len()..]
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .ok()?;
            Some(kb << 10)
        };
        let limit = field("MemTotal:")?;
        // Kernels before 3.14 have no MemAvailable; free memory is the nearest
        // thing they report, and it undercounts, which only errs towards the
        // plan that is safe on a full machine.
        let available = field("MemAvailable:").or_else(|| field("MemFree:"))?;
        Some(Memory { limit, available })
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::Memory;
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    /// Physical memory. macOS keeps most of it busy as cache and compresses
    /// rather than reports what is free, so what is available is taken to be
    /// the whole of it -- the page cache gives way to a comparison as it would
    /// to any other reader.
    pub fn probe() -> Option<Memory> {
        let mut total: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated, and `total` is a u64 whose size
        // is passed in `len`, which is what hw.memsize writes.
        let rc = unsafe {
            sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut total).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && total > 0).then_some(Memory {
            limit: total,
            available: total,
        })
    }
}

#[cfg(windows)]
mod os {
    use super::Memory;

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub fn probe() -> Option<Memory> {
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set, which is
        // all the call asks of its argument.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0 && status.total_phys > 0).then_some(Memory {
            limit: status.total_phys,
            available: status.avail_phys,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod os {
    pub fn probe() -> Option<super::Memory> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("4k"), Some(4 << 10));
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("8G"), Some(8 << 30));
        assert_eq!(parse_size("8GiB"), Some(8 << 30));
        assert_eq!(parse_size("8gb"), Some(8 << 30));
        assert_eq!(parse_size(" 1T "), Some(1 << 40));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn a_budget_halves() {
        assert_eq!(streaming_threshold(Some(16 << 30)), 8 << 30);
        assert_eq!(streaming_threshold(Some(4 << 30)), 2 << 30);
    }

    #[test]
    fn this_machine_answers() {
        // Every platform CI runs on reports its memory.
        let m = probe().expect("no memory reading on this platform");
        assert!(m.limit > 0 && m.available > 0);
        assert!(m.available <= m.limit);
    }
}
