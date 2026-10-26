//! A budget that is not enough is an error, not a signal.
//!
//! The three other ports in this repository answer a memory ceiling the way they
//! answer a missing column — a line on stderr and exit 2 — and this one used to
//! answer it with `abort`, because Rust's default allocator calls
//! `handle_alloc_error` and there is no way to return from that. On the 50M-row
//! Parquet rung of the benchmark ladder that was the difference between a
//! refusal and no answer at all: C, C++ and Zig each printed what they could not
//! fit and the harness moved on, and this port died on signal 6.
//!
//! Two failures beside the abort, both worse in their way and both found by
//! walking the cap down one rung at a time:
//!
//!   * `Scope::spawn` panics when the OS will not give it a thread, which under a
//!     memory cap is exactly when it happens, because a thread's stack is a
//!     private mapping and that is what `RLIMIT_DATA` bounds. Exit 101.
//!   * printing that panic with `RUST_BACKTRACE=1` set took the backtrace lock,
//!     allocated to symbolise, failed, and reached for the same lock from the
//!     allocation error hook. The process hung, which no timeout of its own ends.
//!     It came back once, through the hook's own `RUST_BACKTRACE` branch, and
//!     showed up only on runners that export the variable -- so every run here
//!     now sets it, and the hook is expected to ignore it (see `quiet_panics`).
//!
//! So this test asserts three things about the same run: an exit code of 2, a
//! message that says what ran out, and that it finishes at all.
//!
//! Linux only, deliberately. `RLIMIT_DATA` has covered private anonymous
//! mappings since Linux 4.7, and not file-backed ones — which is what makes it
//! the right cap for a tool that maps its input: it bounds what grows with the
//! row count and leaves the mapping alone. No other platform promises that, so
//! there is nothing to assert there.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Enough rows that the index cannot fit in a few megabytes, small enough that
/// writing it costs nothing: 120,000 rows is about 3.7 MB on disk and wants
/// somewhere over 8 MB to compare.
const ROWS: usize = 120_000;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // A counter and not a clock: `Instant::now().elapsed()` is zero, which
        // gave both tests in this file the same directory, and whichever
        // finished first deleted the other's files out from under it.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "compara-oom-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        const HEADER: &str = "account_id,txn_id,amount,updated_at\n";
        let mut a = String::from(HEADER);
        let mut b = String::from(HEADER);
        for i in 0..ROWS {
            // A key that repeats every ten thousand rows, so there are duplicate
            // keys to count as well as distinct ones to index.
            let key = format!("a{},t{}", i % 9_999, i);
            a.push_str(&format!("{key},{}.{},2026-01-01\n", i % 977, i % 97));
            // One row in a hundred differs, so the comparison has changed cells
            // to hold and a report with something in it to write. An identical
            // pair would skip the part of the run that allocates most.
            if i % 100 == 0 {
                b.push_str(&format!("{key},{}.{},2026-02-02\n", i % 977, (i + 1) % 97));
            } else {
                b.push_str(&format!("{key},{}.{},2026-01-01\n", i % 977, i % 97));
            }
        }
        fs::write(dir.join("a.csv"), &a).expect("write a");
        fs::write(dir.join("b.csv"), &b).expect("write b");
        Fixture { dir }
    }

    fn a(&self) -> PathBuf {
        self.dir.join("a.csv")
    }

    fn b(&self) -> PathBuf {
        self.dir.join("b.csv")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What one run under `cap_mb` megabytes of `RLIMIT_DATA` did.
struct Run {
    code: Option<i32>,
    stderr: String,
    hung: bool,
}

/// `ulimit -d` rather than `setrlimit`: this crate has no libc dependency and is
/// not about to take one for a test. The limit applies to the `exec`ed child, so
/// what it bounds is the binary under test and nothing else.
fn run_capped(cap_mb: usize, a: &Path, b: &Path, report: &Path) -> Run {
    run_script(cap_mb * 1024, a, b, &format!("-o {}", report.display()))
}

/// The same, at `cap_kb` kilobytes and with whatever output flag is given.
fn run_script(cap_kb: usize, a: &Path, b: &Path, out_flag: &str) -> Run {
    let script = format!(
        "ulimit -d {}; exec {} compare {} {} -k account_id,txn_id {}",
        cap_kb,
        env!("CARGO_BIN_EXE_compara"),
        a.display(),
        b.display(),
        out_flag,
    );
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(&script)
        // The environment that deadlocked: set on every run rather than
        // inherited, so the test does not pass or fail with the shell it ran in.
        .env("RUST_BACKTRACE", "1")
        .env_remove("COMPARA_BACKTRACE")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bash");

    // A deadline rather than `wait`, because the failure this test exists to
    // catch included a deadlock: `wait` on a hung child is a hung test suite,
    // and a hung suite reads as an infrastructure problem rather than as this
    // bug.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().expect("wait") {
            Some(status) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let mut raw = Vec::new();
                    let _ = pipe.read_to_end(&mut raw);
                    stderr = String::from_utf8_lossy(&raw).into_owned();
                }
                return Run {
                    code: status.code(),
                    stderr,
                    hung: false,
                };
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Run {
                    code: None,
                    stderr: String::new(),
                    hung: true,
                };
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn a_budget_that_is_not_enough_is_an_error_not_a_signal() {
    let fx = Fixture::new();
    let report = fx.dir.join("out.html");

    // Every rung from "nearly enough" down to "cannot even start" has to answer
    // the same way. One cap would not do: the old code refused at some sizes and
    // aborted at others, depending on which allocation happened to be the one
    // that crossed the line.
    for cap_mb in [8, 6, 4, 2] {
        let run = run_capped(cap_mb, &fx.a(), &fx.b(), &report);
        assert!(!run.hung, "{cap_mb} MB: the run did not finish");
        assert_eq!(
            run.code,
            Some(2),
            "{cap_mb} MB: expected exit 2, got {:?}; stderr: {}",
            run.code,
            run.stderr.trim()
        );
        assert!(
            run.stderr.contains("out of memory"),
            "{cap_mb} MB: nothing in stderr says what ran out: {}",
            run.stderr.trim()
        );
        // The refusal names the structure that did not fit, which is the answer
        // to *how much would have been enough*.
        assert!(
            run.stderr.contains("needs"),
            "{cap_mb} MB: the refusal does not say how big it was: {}",
            run.stderr.trim()
        );
    }
}

#[test]
fn the_same_files_compare_when_the_budget_is_enough() {
    // Without this the test above proves only that the invocation is broken.
    let fx = Fixture::new();
    let report = fx.dir.join("out.html");
    let run = run_capped(512, &fx.a(), &fx.b(), &report);
    assert!(!run.hung, "the unconstrained run did not finish");
    assert!(
        matches!(run.code, Some(0) | Some(1)),
        "expected an answer, got {:?}; stderr: {}",
        run.code,
        run.stderr.trim()
    );
    assert!(report.exists(), "no report was written");
}

/// Every budget from too small to enough, not five of them.
///
/// The five sampled caps above are five points on a line, and the failure this
/// file exists to catch does not live at points -- it lives in a *band*, and a
/// band moves when the port's requirement changes. It moved under this test
/// without it noticing: a change that stopped the index holding the sweep
/// chunks and the table at the same time lowered what the port needs, carried
/// the band down with it past caps this test samples, and *widened* it from
/// three rungs to six. The suite stayed green.
///
/// So this scans, and it is a ratchet rather than a guarantee. Everything the
/// port allocates itself is checked -- the index, the sections, the rows, every
/// `String` in them, and the buffers the JSON, the gzip stream and the base64
/// are written into. What is left is inside `serde_json` and the compressor
/// themselves, and at the one cap where the process is a single allocation from
/// its ceiling it is a coin flip which allocation is the one that fails: the
/// port's own, which refuses, or theirs, which cannot. Repeating the scan gives
/// one or two rungs and never the same one twice in a row.
///
/// So the number below is the count of rungs that have *ever* aborted across
/// repeats, not a per-run count, and this test's job is to notice the day there
/// are three.
#[test]
fn the_band_where_it_aborts_does_not_grow() {
    // What the port allocates itself is checked, so an abort in this range is
    // inside `serde_json` or the compressor at the cap where the process is one
    // allocation from its ceiling. One pass measures one or two; three is this
    // number so that the coin flip cannot fail a run on its own, which is the
    // failure mode a scanning test has that a sampling one does not.
    //
    // It is still a ratchet with room to catch something real: dropping the
    // index's peak, which turns graceful index refusals into report-path ones
    // over a 4 MB stretch of caps, measures six. Raising this number is a
    // decision, not a fix -- find what widened it first.
    const ALLOWED: usize = 3;

    let fx = Fixture::new();
    let mut aborted = Vec::new();
    let mut answered = 0usize;
    let mut refused = 0usize;
    let report = fx.dir.join("scan.html");
    let out_flag = format!("-o {}", report.display());

    // 512 KB steps: wide enough that the whole scan is seconds, fine enough
    // that a band has nowhere to hide -- the narrowest measured here was 1 MB.
    let mut cap_kb = 4 * 1024;
    while cap_kb <= 24 * 1024 {
        let run = run_script(cap_kb, &fx.a(), &fx.b(), &out_flag);
        assert!(!run.hung, "{cap_kb} KB: the run did not finish");
        match run.code {
            // Killed by a signal: an allocation failed and took the process.
            None => aborted.push((cap_kb, run.stderr.trim().to_string())),
            Some(2) => {
                assert!(
                    run.stderr.contains("needs"),
                    "{cap_kb} KB: the refusal does not say how big it was: {}",
                    run.stderr.trim()
                );
                refused += 1;
            }
            Some(0) | Some(1) => answered += 1,
            other => panic!(
                "{cap_kb} KB: unexpected exit {other:?}; stderr: {}",
                run.stderr.trim()
            ),
        }
        cap_kb += 512;
    }

    assert!(
        aborted.len() <= ALLOWED,
        "{} of the scanned budgets aborted instead of refusing, against {ALLOWED} allowed:\n{}",
        aborted.len(),
        aborted
            .iter()
            .map(|(kb, err)| format!("  {} KB: {err}", kb))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // A scan that only ever refused, or only ever answered, is not crossing the
    // boundary and would pass on a port that had lost the ability to do either.
    assert!(refused > 0, "no cap in the range was too small to serve");
    assert!(answered > 0, "no cap in the range was enough to answer");
}

/// A CSV pair whose index does not fit the heap it may have -- a container's
/// limit, here `ulimit -d` -- is compared by the engine that spills to disk,
/// with the answer the unlimited run gives, under `--engine auto`. Named, the
/// in-memory engine refuses as before.
#[test]
fn a_csv_pair_too_big_for_the_heap_is_compared_by_spilling() {
    let dir = std::env::temp_dir().join(format!("compara-spill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let made = Command::new(env!("CARGO_BIN_EXE_compara-gen-data"))
        .args(["--rows", "2m", "--out-dir"])
        .arg(&dir)
        .args(["--prefix", "t"])
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let (a, b) = (dir.join("t_a.csv"), dir.join("t_b.csv"));
    let run = |limit: Option<u64>, engine: &str| {
        let script = format!(
            "{} exec {} compare {} {} -k account_id,txn_id -i updated_at --engine {engine}",
            limit
                .map(|kb| format!("ulimit -d {kb};"))
                .unwrap_or_default(),
            env!("CARGO_BIN_EXE_compara"),
            a.display(),
            b.display(),
        );
        Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap()
    };
    let counts = |o: &std::process::Output| {
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        // The counts, without the engine and its time at the end.
        out.rsplit_once(" | ")
            .map(|(c, _)| c.to_string())
            .unwrap_or(out)
    };
    let free = run(None, "auto");
    let tight = run(Some(100 * 1024), "auto");
    let err = String::from_utf8_lossy(&tight.stderr);
    assert_eq!(free.status.code(), tight.status.code(), "{err}");
    assert_eq!(counts(&free), counts(&tight), "{err}");
    assert!(err.contains("sortmerge"), "{err}");
    assert!(String::from_utf8_lossy(&tight.stdout).contains("sortmerge"));
    let named = run(Some(100 * 1024), "turbo");
    assert_eq!(named.status.code(), Some(2), "{:?}", named);
    assert!(String::from_utf8_lossy(&named.stderr).contains("out of memory"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A named sortmerge in a heap too small for its own buffers and threads is
/// refused before it starts, not aborted mid-run (16 MB aborted on a runner
/// with more threads).
#[test]
fn a_named_sortmerge_without_room_is_refused_up_front() {
    let f = Fixture::new();
    let run = run_script(16 * 1024, &f.a(), &f.b(), "--engine sortmerge");
    assert!(!run.hung, "hung");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("the sortmerge engine needs about"),
        "{}",
        run.stderr
    );
}
