/*
 * The two things every parallel phase in this port needs, in one place.
 *
 * Both were written for the columnar Parquet path first and are wanted verbatim
 * by the CSV one, which is the usual sign that they belong beside both rather
 * than inside either.
 */
#ifndef CSVDIFF_PARALLEL_H
#define CSVDIFF_PARALLEL_H

#include <stddef.h>

/*
 * One shape for every parallel phase: part 0 runs on the calling thread and the
 * rest get one each. A thread that will not spawn is not a failure -- its part
 * runs inline, slower and still right -- so this returns nothing and every
 * caller reports its own errors through its own context.
 */
void run_parts(void (*fn)(void *ctx, unsigned part), void *ctx, unsigned ways);

/*
 * A ceiling on the allocations that scale with the input, declared before they
 * are made rather than discovered when one fails.
 *
 * Checking what malloc returns is necessary and not sufficient. Under Linux's
 * default heuristic overcommit there is a band -- on a 15 GB box, around 14 GB
 * -- where malloc hands back a pointer and the kernel kills the process when
 * the pages are touched. A port that checks every return still dies there, with
 * no message and exit 137. So the sizes that scale with row count are declared
 * to `budget_take` first, and a run that would not fit says so and stops while
 * it still can.
 *
 * What this bounds is the per-row index arrays and the Parquet column buffers:
 * the allocations whose size follows the input, and the ones that are large
 * enough to matter. It is not a ceiling on the process -- the mapped files are
 * not in it, nor is a few kilobytes of bookkeeping -- so it is a budget for the
 * part that grows, which is the part that runs out.
 */
void budget_set(size_t mb);          /* 0 means no ceiling, which is the default */
size_t budget_limit(void);           /* the ceiling in bytes, 0 when unset */
size_t budget_used(void);

/* Returns 0 when `bytes` fits, or -1 when it does not; the caller reports. */
int budget_take(size_t bytes);

/* Hands back what a matching `budget_take` took, so the total tracks what is
 * held rather than what has ever been asked for. Saturates at zero: a give
 * without a take is a bug, and one that wrapped would silently lift the
 * ceiling for everything after it. */
void budget_give(size_t bytes);

/* Whether a take has been refused. Sticky, because the refusal happens deep in
 * an index build whose caller reports the error, and "out of memory" and "more
 * than you allowed" are different things to be told. */
int budget_exceeded(void);

/*
 * 2 MB-aligned, and asked for on huge pages. Uninitialised: the caller fills it,
 * because one table here wants zeroes and another wants -1.
 *
 * Worth it only for an allocation that is rare and long-lived and then probed at
 * random. A hash table over ten million keys is 128 MB, which on 4 KB pages is
 * 32,768 pages against a TLB holding perhaps 1,500, so nearly every probe takes
 * a page walk on top of its cache miss. On 2 MB pages the same table is 64
 * entries. It is *not* worth it for a buffer allocated in a loop: with
 * `transparent_hugepage` set to `madvise` each request makes the kernel compact
 * memory to find a 2 MB run, and that cost scales with the number of asks. This
 * was measured three ways and the rule is in c/README.md.
 *
 * Falls back to plain malloc where the alignment cannot be had, so a caller
 * never has to care.
 */
void *alloc_huge(size_t bytes);

/* How many parts to split into when nobody said. At least 1. */
unsigned cpu_count(void);

#endif
