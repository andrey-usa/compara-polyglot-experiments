//! Allocations that can fail without taking the process with them.
//!
//! Rust's default allocator has no failure path. `Vec::with_capacity`, `vec![]`
//! and `Vec::reserve` all end in `handle_alloc_error`, which prints to stderr
//! from inside the allocator and calls `abort`. Every other port in this
//! repository answers a budget it cannot meet the same way it answers a missing
//! column — a line on stderr and exit 2:
//!
//! ```text
//! C     error: out of memory reading the parquet file
//! C++   error: std::bad_alloc
//! Zig   error: out of memory
//! ```
//!
//! This port aborted on signal 6 instead, and at fifty million rows of Parquet
//! that was the difference between a refusal and no answer at all: the three
//! that refuse still print a summary line and let the harness carry on to the
//! next port, and the one that aborts loses the run. Three threads racing into
//! `handle_alloc_error` also interleave their messages mid-word, so the one
//! line it did print was unreadable.
//!
//! So the allocations that scale with the input — one entry per row, per
//! distinct key, per uncompressed byte — go through the helpers here, which
//! return [`Error`] at the allocation that would have crossed the line. A
//! bounded allocation (a per-thread block, a header, a column name) does not
//! need them: it cannot be the one that fails, and routing it through here
//! would only add noise.
//!
//! The `what` label is not decoration. A refusal that names the structure is
//! the answer to *how much would be enough*, which is the question anyone
//! laddering `--memory-cap` is asking. `hash per row` and `the uncompressed
//! column` fail at very different budgets, and knowing which one hit the
//! ceiling is the difference between a number and a diagnosis.

use std::sync::Mutex;

use crate::{Error, Result};

/// Room held back so that a refusal can always be written.
///
/// Formatting the refusal allocates, and so does everything between it and the
/// line on stderr. When the allocation that failed was megabytes that is no
/// problem -- it was never taken -- but the checked ones include a report row's
/// few dozen bytes, and when one of those fails the process is at its ceiling
/// to the byte: the message's own allocation failed after it and aborted from
/// inside the refusal. This is released first thing in [`refuse`], which is
/// what the message and the unwinding after it are written into.
static RESERVE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
const RESERVE_BYTES: usize = 64 * 1024;

/// Sets aside the room a refusal is written into. `main` calls it first.
pub fn hold_reserve() {
    if let Ok(mut reserve) = RESERVE.lock() {
        *reserve = Vec::with_capacity(RESERVE_BYTES);
    }
}

/// The refusal itself: what was being sized, and how big it was.
fn refuse(what: &str, bytes: usize) -> Error {
    if let Ok(mut reserve) = RESERVE.lock() {
        *reserve = Vec::new();
    }
    Error::new(format!(
        "out of memory: {what} needs {}",
        human_bytes(bytes)
    ))
}

fn human_bytes(bytes: usize) -> String {
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

/// `Vec::with_capacity(n)` that returns instead of aborting.
///
/// Exact, not amortised: these are sized from a row count that is already known,
/// and rounding up to the next power of two would be tens of megabytes of slack
/// at the sizes where the budget is tight.
pub fn sized<T>(n: usize, what: &str) -> Result<Vec<T>> {
    let mut v = Vec::new();
    grow(&mut v, n, what)?;
    Ok(v)
}

/// `vec![value; n]` that returns instead of aborting.
///
/// The fill is written rather than taken from the kernel, which is the same
/// choice [`crate::engine`]'s tables make deliberately: a fresh anonymous
/// mapping is one shared page of zeroes until something touches it, so a table
/// nothing reads before the inserts start would fault every page in under a
/// random probe.
pub fn filled<T: Clone>(value: T, n: usize, what: &str) -> Result<Vec<T>> {
    let mut v = sized(n, what)?;
    v.resize(n, value);
    Ok(v)
}

/// `vec.reserve_exact(extra)` that returns instead of aborting.
pub fn grow<T>(v: &mut Vec<T>, extra: usize, what: &str) -> Result<()> {
    v.try_reserve_exact(extra)
        .map_err(|_| refuse(what, extra.saturating_mul(size_of::<T>())))
}

/// Room for `extra` more entries in a vector that grows as it goes, amortised.
///
/// [`grow`] is exact, which is right when the final size is known and wrong
/// here: called once per block on a vector that is still being filled, an exact
/// reserve reallocates and copies on every call, so a column's worth of hits
/// would be quadratic. This asks for at least `extra` and lets the vector take
/// the usual doubling, so the checks are amortised to nothing.
pub fn room<T>(v: &mut Vec<T>, extra: usize, what: &str) -> Result<()> {
    if v.capacity() - v.len() >= extra {
        return Ok(());
    }
    v.try_reserve(extra).map_err(|_| {
        refuse(
            what,
            v.len().saturating_add(extra).saturating_mul(size_of::<T>()),
        )
    })
}

/// `vec.push(value)` that returns instead of aborting.
///
/// The check is the one `push` already makes; what changes is what happens when
/// it says the vector is full. It sits in the sweep's per-row loop, which is the
/// hottest loop in the port, and costs nothing measurable there: a compare
/// against a value already in a register, taken once per doubling.
#[inline]
pub fn push<T>(v: &mut Vec<T>, value: T, what: &str) -> Result<()> {
    if v.len() == v.capacity() {
        room(v, 1, what)?;
    }
    v.push(value);
    Ok(())
}

/// `s.to_string()` that returns instead of aborting.
///
/// A `String` is a `Vec<u8>` that has been checked for UTF-8, and
/// `String::from_utf8` takes the vector's allocation rather than copying it --
/// so a string built this way costs exactly what `to_string` costs and can
/// still refuse. That is what makes the report path checkable without changing
/// [`crate::contract::Val`], which is `Option<String>` and shared with every
/// other engine.
///
/// One of these per cell sounds like a lot of checking. It is the check `Vec`
/// already performs; what changes is that a failure returns here instead of
/// printing from inside the allocator and calling `abort`.
pub fn string(s: &str, what: &str) -> Result<String> {
    let mut v: Vec<u8> = sized(s.len(), what)?;
    v.extend_from_slice(s.as_bytes());
    // `v` was filled from a `&str`, so it is UTF-8 by construction.
    Ok(String::from_utf8(v).unwrap_or_default())
}

/// [`string`] for a value that may be absent.
pub fn val(v: Option<&str>, what: &str) -> Result<crate::contract::Val> {
    match v {
        None => Ok(None),
        Some(s) => Ok(Some(string(s, what)?)),
    }
}

/// [`grow`] as a method, for the sites that read better that way.
pub trait TryGrow {
    /// Room for `extra` more entries, or a refusal naming `what`.
    fn try_grow(&mut self, extra: usize, what: &str) -> Result<()>;
}

impl<T> TryGrow for Vec<T> {
    fn try_grow(&mut self, extra: usize, what: &str) -> Result<()> {
        grow(self, extra, what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sized_gives_at_least_what_was_asked() {
        let v: Vec<u64> = sized(1000, "test").unwrap();
        assert!(v.capacity() >= 1000);
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn filled_writes_the_value() {
        let v = filled(7u32, 4, "test").unwrap();
        assert_eq!(v, vec![7, 7, 7, 7]);
    }

    #[test]
    fn grow_is_additive() {
        let mut v: Vec<u8> = vec![1, 2, 3];
        v.try_grow(100, "test").unwrap();
        assert!(v.capacity() >= 103);
        assert_eq!(v.len(), 3);
    }

    /// An allocation no machine can serve refuses rather than aborting. This is
    /// the whole point of the module: on the old code the process died here.
    #[test]
    fn an_impossible_size_refuses() {
        let huge = usize::MAX / 16;
        let e = sized::<u64>(huge, "hash per row").unwrap_err();
        let m = e.to_string();
        assert!(m.starts_with("out of memory: hash per row needs "), "{m}");
    }

    #[test]
    fn the_size_is_in_bytes_not_entries() {
        // A count that fits on its own and does not fit once multiplied by the
        // element size, so `try_reserve` must report `CapacityOverflow` rather
        // than ask the allocator. Asking the allocator is what the first version
        // of this test did -- 32 TB of `u64`, which Linux refuses and macOS
        // cheerfully overcommits, so it passed here and failed on the platform
        // matrix.
        let n = (isize::MAX as usize) / 4;
        let e = sized::<u64>(n, "pairs").unwrap_err();
        let m = e.to_string();
        assert!(m.starts_with("out of memory: pairs needs "), "{m}");
        // n entries would be ~2 EB of u64; the count alone is a quarter of that.
        let mb: usize = m
            .rsplit(' ')
            .nth(1)
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        assert_eq!(mb, n.saturating_mul(8) / (1024 * 1024), "{m}");
    }

    #[test]
    fn sizes_are_rendered_at_the_unit_that_says_something() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1024 * 1024), "1 MB");
        assert_eq!(human_bytes(33_554_432), "32 MB");
    }

    #[test]
    fn a_refusal_names_the_structure_and_its_size() {
        let e = refuse("the key index", 33_554_432);
        assert_eq!(e.to_string(), "out of memory: the key index needs 32 MB");
    }

    #[test]
    fn push_appends_and_grows() {
        let mut v: Vec<u32> = Vec::new();
        for i in 0..1000 {
            push(&mut v, i, "test").unwrap();
        }
        assert_eq!(v.len(), 1000);
        assert_eq!(v[999], 999);
    }

    #[test]
    fn room_is_amortised_not_exact() {
        // The point of `room` over `grow`: filling one entry at a time must not
        // reallocate every time, or a column's worth of hits is quadratic.
        let mut v: Vec<u64> = Vec::new();
        let mut reallocs = 0;
        let mut cap = v.capacity();
        for i in 0..10_000u64 {
            room(&mut v, 1, "test").unwrap();
            v.push(i);
            if v.capacity() != cap {
                cap = v.capacity();
                reallocs += 1;
            }
        }
        assert!(reallocs < 32, "{reallocs} reallocations for 10k pushes");
    }

    #[test]
    fn overflowing_the_byte_count_still_refuses() {
        // usize::MAX entries of 8 bytes overflows the multiplication; the
        // saturating form keeps it a refusal rather than a panic in debug.
        let e = sized::<u64>(usize::MAX, "pairs").unwrap_err();
        assert!(
            e.to_string().starts_with("out of memory: pairs needs "),
            "{e}"
        );
    }
}
