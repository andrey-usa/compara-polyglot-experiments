#pragma once

/*
 * The Win32 half of the one call this port makes that Windows does not have:
 * a file mapping. Threads are `std::thread` here, which is why this file is
 * shorter than the C port's -- the standard library already carries the other
 * half of the portability problem.
 *
 * This is a compatibility header, not an emulation layer: `mmap` below is
 * read-only, private, whole-file and offset-zero, because that is the only mmap
 * this port asks for. A shim that pretended to be general would be a lie the
 * next reader has to disprove.
 *
 * The shims sit at global scope so the existing `::mmap` call sites need no
 * change, and the whole Win32 section is inside the guard, so a POSIX build
 * cannot pick one up by accident.
 */

#ifdef _WIN32

#include <windows.h>

#include <io.h>
#include <cstddef>

#define PROT_READ       0x1
#define MAP_PRIVATE     0x2
#define MAP_FAILED      (reinterpret_cast<void*>(-1))

/*
 * POSIX's names for hints Windows does not take: `MapViewOfFile` decides its
 * own read-ahead and offers no per-range call to change it. So `madvise` is a
 * no-op rather than a wrong-but-plausible translation, and these numbers exist
 * only so the call sites compile unchanged.
 */
#define MADV_SEQUENTIAL 2
#define MADV_WILLNEED   3

inline void* mmap(void* addr, std::size_t len, int prot, int flags, int fd, long long off) {
    (void)addr; (void)prot; (void)flags; (void)off;
    HANDLE file = reinterpret_cast<HANDLE>(_get_osfhandle(fd));
    if (file == INVALID_HANDLE_VALUE) return MAP_FAILED;
    // A zero size maps the whole file, which is what the length argument
    // already says and what every call site here wants.
    HANDLE mapping = CreateFileMappingA(file, nullptr, PAGE_READONLY, 0, 0, nullptr);
    if (!mapping) return MAP_FAILED;
    void* view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len);
    // The view holds its own reference to the section, so closing the handle
    // here leaves the mapping alive until UnmapViewOfFile -- and keeps this
    // shim from leaking one handle per file.
    CloseHandle(mapping);
    return view ? view : MAP_FAILED;
}

inline int munmap(void* addr, std::size_t len) {
    (void)len;   // a view is unmapped whole; Windows has no partial unmap
    return UnmapViewOfFile(addr) ? 0 : -1;
}

inline int madvise(void* addr, std::size_t len, int advice) {
    (void)addr; (void)len; (void)advice;
    return 0;
}

#endif  // _WIN32

/*
 * The Microsoft runtime translates a lone \n into \r\n on the way out of a
 * text-mode handle and the pair back into one on the way in. Both halves are
 * wrong here -- the reader treats the file as bytes -- so every `open` in this
 * port carries O_BINARY, and this makes the flag a no-op everywhere else so the
 * call sites need no #ifdef. Include after <fcntl.h>, where Windows defines the
 * real one.
 */
#ifndef O_BINARY
#define O_BINARY 0
#endif
