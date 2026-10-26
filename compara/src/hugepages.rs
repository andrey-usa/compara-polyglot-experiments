//! Large buffers on transparent huge pages.
//!
//! A comparison allocates most of its memory a few large buffers at a time --
//! a hash per row, a decoded Parquet column, a match list -- and touches each
//! page of them once while filling it. With four-kilobyte pages that is a
//! fault per page: on the 2M-row Parquet pair about seventy thousand of them,
//! a fifth of the process's CPU time spent in the kernel, and more of it once
//! four threads fault at once. Linux keeps huge pages for regions that ask
//! (`transparent_hugepage` is `madvise` on most distributions), so the
//! allocator asks for every buffer of a few megabytes or more: one fault per
//! two megabytes. The same request the C library makes under
//! `GLIBC_TUNABLES=glibc.malloc.hugetlb=1`, without needing the environment.
//!
//! Smaller allocations pass straight through, and so does everything on other
//! systems. The advice is only advice: where the kernel has no huge pages to
//! give, the buffer gets small ones, as it would have anyway.

use std::alloc::{GlobalAlloc, Layout, System};

/// The system allocator, advising huge pages for large buffers.
pub struct HugePages;

/// Buffers this large or larger are advised; below it a buffer spans too few
/// huge pages to be worth a system call.
#[cfg(target_os = "linux")]
const FROM: usize = 4 << 20;

#[cfg(target_os = "linux")]
fn advise(p: *mut u8, size: usize) {
    use std::ffi::{c_int, c_void};
    unsafe extern "C" {
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }
    const MADV_HUGEPAGE: c_int = 14;
    const PAGE: usize = 4096;
    if size < FROM || p.is_null() {
        return;
    }
    // madvise takes a page-aligned start; the pages around the buffer are its
    // allocator's own and harmless to advise. The kernel backs only the whole
    // two-megabyte stretches inside with huge pages.
    let start = p as usize & !(PAGE - 1);
    let end = (p as usize + size).next_multiple_of(PAGE);
    // SAFETY: the range covers memory the allocator mapped for this buffer,
    // rounded out to its pages; MADV_HUGEPAGE changes no contents and a
    // failure (an old kernel, THP compiled out) leaves the range as it was.
    unsafe {
        madvise(start as *mut c_void, end - start, MADV_HUGEPAGE);
    }
}

#[cfg(not(target_os = "linux"))]
fn advise(_: *mut u8, _: usize) {}

// SAFETY: every call is forwarded to `System` with the caller's arguments, and
// the advice afterwards neither moves nor frees the block.
unsafe impl GlobalAlloc for HugePages {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as given.
        let p = unsafe { System.alloc(layout) };
        advise(p, layout.size());
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as given.
        let p = unsafe { System.alloc_zeroed(layout) };
        advise(p, layout.size());
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as given.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded as given.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        // A block that grew past the line, or moved to a fresh mapping, has
        // not been advised yet.
        advise(p, new_size);
        p
    }
}
