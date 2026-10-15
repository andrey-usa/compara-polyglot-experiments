#define _GNU_SOURCE

#include "parallel.h"

#include <stdatomic.h>
#include <stdlib.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#include <sys/mman.h>
#include <unistd.h>
#endif

typedef struct {
    void   (*fn)(void *ctx, unsigned part);
    void    *ctx;
    unsigned part;
} Job;

/*
 * A thread is the one thing this file needs that the two platforms spell
 * differently, so it is the one thing wrapped. Three operations, each the exact
 * shape `run_parts` below asks for -- a handle that compares false when there is
 * no thread, a start that reports failure rather than aborting, and a join that
 * also releases the handle Windows hands back.
 *
 * pthreads exists on Windows through mingw-w64's winpthreads, and this does not
 * use it: `pthread_t` is a struct there, so `tid[p] = 0` and `if (tid[p])` --
 * which is how `run_parts` records a thread it could not start -- would not
 * compile. Twenty lines of CreateThread cost less than reshaping the caller
 * around a portability library, and drop a DLL from the binary besides.
 */
#ifdef _WIN32

typedef HANDLE thread_t;

static DWORD WINAPI job_entry(LPVOID p) {
    Job *j = p;
    j->fn(j->ctx, j->part);
    return 0;
}

static int thread_start(thread_t *t, Job *j) {
    *t = CreateThread(NULL, 0, job_entry, j, 0, NULL);
    return *t ? 0 : -1;
}

static void thread_wait(thread_t t) {
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
}

#else

typedef pthread_t thread_t;

static void *job_entry(void *p) {
    Job *j = p;
    j->fn(j->ctx, j->part);
    return NULL;
}

static int thread_start(thread_t *t, Job *j) {
    return pthread_create(t, NULL, job_entry, j);
}

static void thread_wait(thread_t t) {
    pthread_join(t, NULL);
}

#endif

void run_parts(void (*fn)(void *, unsigned), void *ctx, unsigned ways) {
    if (ways <= 1) { fn(ctx, 0); return; }
    thread_t *tid = calloc(ways, sizeof *tid);
    Job       *job = calloc(ways, sizeof *job);
    if (!tid || !job) {                       /* no memory for threads: do it inline */
        for (unsigned p = 0; p < ways; p++) fn(ctx, p);
        free(tid);
        free(job);
        return;
    }
    for (unsigned p = 1; p < ways; p++) {
        job[p].fn = fn;
        job[p].ctx = ctx;
        job[p].part = p;
        if (thread_start(&tid[p], &job[p]) != 0) {
            fn(ctx, p);                       /* no thread to be had: the same work, here */
            tid[p] = 0;
        }
    }
    fn(ctx, 0);
    for (unsigned p = 1; p < ways; p++)
        if (tid[p]) thread_wait(tid[p]);
    free(tid);
    free(job);
}

void *alloc_huge(size_t bytes) {
#ifdef _WIN32
    /*
     * Plain malloc on Windows, and the header already promises that. Large
     * pages there are not a hint a process can leave on a range: they need
     * SeLockMemoryPrivilege, which an ordinary account does not hold, and
     * VirtualAlloc with MEM_LARGE_PAGES, which returns memory `free` must not
     * be given. The 2 MB alignment on its own buys nothing without the pages
     * behind it, so this asks for none of it rather than paying for the shape
     * of an optimisation it cannot have.
     */
    return malloc(bytes);
#else
    const size_t huge = (size_t)2 << 20;
    const size_t rounded = (bytes + huge - 1) & ~(huge - 1);
    void *p = NULL;
    if (posix_memalign(&p, huge, rounded) != 0) return malloc(bytes);
#ifdef MADV_HUGEPAGE
    madvise(p, rounded, MADV_HUGEPAGE);
#endif
    return p;
#endif
}

unsigned cpu_count(void) {
#ifdef _WIN32
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    return si.dwNumberOfProcessors > 0 ? (unsigned)si.dwNumberOfProcessors : 1u;
#else
    const long n = sysconf(_SC_NPROCESSORS_ONLN);
    return n > 0 ? (unsigned)n : 1u;
#endif
}

/* ------------------------------------------------------------------------- */
/* The memory budget                                                          */
/* ------------------------------------------------------------------------- */

/*
 * One process, one comparison, so a file-scope total is the whole mechanism.
 *
 * The total is *live* bytes, not bytes ever asked for, which is the difference
 * between a ceiling and a tally. It did not start that way: there was a
 * `budget_take` and nothing that gave anything back, so a run that read
 * seventeen columns one at a time, freeing each before the next, spent
 * seventeen columns' worth of a ceiling it never held more than one of. At two
 * million rows that refused at 419 MB a run whose high-water mark was 171 MB.
 *
 * It also needs to be safe from more than one thread now, which it did not used
 * to be. The comment here said every take happened on the calling thread at a
 * phase boundary; that was true when it was written and stopped being true when
 * the columnar reader started taking inside `pq_read_column`, which the column
 * workers call. Four threads doing a read-modify-write on one size_t is a race
 * whether or not it has ever lost.
 */
static size_t          budget_cap = 0;
static _Atomic size_t  budget_live = 0;
static atomic_int      budget_over = 0;

void budget_set(size_t mb) {
    budget_cap = mb ? mb * (size_t)1024 * 1024 : 0;
    atomic_store(&budget_live, 0);
    atomic_store(&budget_over, 0);
}

size_t budget_limit(void) { return budget_cap; }
size_t budget_used(void)  { return atomic_load(&budget_live); }
int    budget_exceeded(void) { return atomic_load(&budget_over); }

int budget_take(size_t bytes) {
    if (budget_cap == 0) return 0;
    size_t cur = atomic_load(&budget_live);
    for (;;) {
        /* An overflowing sum is over any ceiling, so subtract rather than add. */
        if (bytes > budget_cap - cur) { atomic_store(&budget_over, 1); return -1; }
        if (atomic_compare_exchange_weak(&budget_live, &cur, cur + bytes)) return 0;
    }
}

void budget_give(size_t bytes) {
    if (budget_cap == 0 || bytes == 0) return;
    size_t cur = atomic_load(&budget_live);
    for (;;) {
        /* Saturating, so a give that does not match a take cannot wrap the
         * total into something enormous and make every later take succeed. */
        const size_t next = bytes > cur ? 0 : cur - bytes;
        if (atomic_compare_exchange_weak(&budget_live, &cur, next)) return;
    }
}
