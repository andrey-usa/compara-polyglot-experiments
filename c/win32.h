#ifndef CSVDIFF_WIN32_H
#define CSVDIFF_WIN32_H

/*
 * The Win32 half of the two calls this port makes that Windows does not have:
 * a file mapping and a thread. Everything else it uses -- open, close, read,
 * write, fstat, malloc -- the Microsoft C runtime provides under the same names
 * MinGW puts in <unistd.h> and <fcntl.h>, so the rest of the port compiles as
 * written.
 *
 * This is a compatibility header, not an emulation layer: each shim covers the
 * exact shape this port calls, and no more. `mmap` here is read-only, private,
 * whole-file and offset-zero, because that is the only mmap the port asks for.
 * A shim that pretended to be general would be a lie that the next reader has
 * to disprove.
 *
 * Nothing here is compiled off Windows -- the whole file is inside the guard --
 * so a POSIX build cannot pick up a shim by accident.
 */

#ifdef _WIN32

#include <windows.h>

#include <direct.h>
#include <io.h>
#include <stddef.h>
#else
#include <sys/stat.h>
#include <sys/types.h>
#endif

#ifdef _WIN32

/* ------------------------------------------------------------------------- */
/* mmap                                                                       */
/* ------------------------------------------------------------------------- */

#define PROT_READ       0x1
#define MAP_PRIVATE     0x2
#define MAP_FAILED      ((void *)-1)

/*
 * The advice values are POSIX's names for hints, and Windows takes no hint of
 * this kind on an existing mapping: `MapViewOfFile` decides its own read-ahead
 * and there is no per-range call to change it. So `madvise` is a no-op here
 * rather than a wrong-but-plausible translation, and the numbers exist only so
 * the call sites compile unchanged.
 */
#define MADV_SEQUENTIAL 2
#define MADV_WILLNEED   3

static inline void *mmap(void *addr, size_t len, int prot, int flags, int fd, long long off) {
    (void)addr; (void)prot; (void)flags; (void)off;
    HANDLE file = (HANDLE)_get_osfhandle(fd);
    if (file == INVALID_HANDLE_VALUE) return MAP_FAILED;
    /*
     * A zero size maps the whole file, which is what every call site here wants
     * and what the length argument already says. Passing the length explicitly
     * would refuse a file that grew between the fstat and this call for no
     * benefit.
     */
    HANDLE mapping = CreateFileMappingA(file, NULL, PAGE_READONLY, 0, 0, NULL);
    if (!mapping) return MAP_FAILED;
    void *view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len);
    /*
     * The view holds its own reference to the section, so the handle can go now
     * and the mapping still lives until `UnmapViewOfFile`. Closing it here is
     * what keeps this shim from leaking a handle per file.
     */
    CloseHandle(mapping);
    return view ? view : MAP_FAILED;
}

static inline int munmap(void *addr, size_t len) {
    (void)len;   /* a view is unmapped whole; Windows has no partial unmap */
    return UnmapViewOfFile(addr) ? 0 : -1;
}

static inline int madvise(void *addr, size_t len, int advice) {
    (void)addr; (void)len; (void)advice;
    return 0;
}

#endif /* _WIN32 */

/* ------------------------------------------------------------------------- */
/* Directories                                                                */
/* ------------------------------------------------------------------------- */

/*
 * One directory, created. The mode argument is the whole reason this exists:
 * Microsoft's `mkdir` takes only a path, so the POSIX two-argument call does
 * not compile there -- and it is a hard error rather than a warning, since the
 * runtime declares a one-argument function of the same name.
 *
 * There are no permissions to carry across: a new directory on Windows inherits
 * its parent's ACL, and 0777 was only ever "whatever the umask allows" anyway.
 */
static inline int make_dir(const char *path) {
#ifdef _WIN32
    return _mkdir(path);
#else
    return mkdir(path, 0777);
#endif
}

/* ------------------------------------------------------------------------- */
/* Text mode                                                                  */
/* ------------------------------------------------------------------------- */

/*
 * The Microsoft runtime translates a lone \n into \r\n on the way out of a
 * text-mode handle, and the pair back into one on the way in. Both halves are
 * wrong here: the reader treats the file as bytes and the writer's whole claim
 * is that four ports produce byte-identical files. So every `open` in this port
 * carries `O_BINARY` and every `fopen` a `b`, and this makes the flag a no-op
 * everywhere else so the call sites need no `#ifdef`.
 *
 * Include this header after <fcntl.h>, which is where Windows defines the real
 * one.
 */
#ifndef O_BINARY
#define O_BINARY 0
#endif

#endif /* CSVDIFF_WIN32_H */
