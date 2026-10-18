//! The fused join: A's sweep joins each row while its pages are hot.
//!
//! Production takes this path once the input is past half the memory the
//! machine has available, so most tests force it with `COMPARA_FUSED_JOIN=1`;
//! `a_small_memory_budget_takes_the_fused_path` reaches it the way production
//! does, through `--memory` and `COMPARA_MEMORY`. The env var is process-global and the tests run as
//! parallel threads, so every test that touches it holds the mutex below while
//! the var is set: a leaked `=1` would flip the path under the OOM tests, whose
//! allocation expectations assume the ordinary sweep.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use compara::gendata::{Compression, Format, generate_as};

static FUSED_ENV: Mutex<()> = Mutex::new(());

fn fixture_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const BIN: &str = env!("CARGO_BIN_EXE_compara");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        generate_as(
            20_000,
            &dir.join("a.csv"),
            &dir.join("b.csv"),
            7,
            Format::Csv,
            Compression::None,
        )
        .expect("the generator");
        Fixture(dir)
    }

    /// Many duplicate keys whose later rows are changed, removed, and matched:
    /// the later-duplicate subtraction has to take back exactly what the sweep
    /// added.
    fn with_duplicates() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-dup-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        let mut a = String::from("id,v\n");
        let mut b = String::from("id,v\n");
        for i in 0..10_000 {
            a.push_str(&format!("{i},a{i}\n"));
            // Even keys change, odd keys match; keys >= 9000 are A-only.
            let bv = if i >= 9000 {
                String::new()
            } else if i % 2 == 0 {
                format!("b{i}\n")
            } else {
                format!("a{i}\n")
            };
            if !bv.is_empty() {
                b.push_str(&format!("{i},{bv}"));
            }
        }
        // Later duplicates: changed, matched, and removed rows.
        for i in 0..500 {
            a.push_str(&format!("{i},dup{i}\n"));
        }
        for i in 9500..10_000 {
            b.push_str(&format!("{i},extra{i}\n"));
        }
        fs::write(dir.join("a.csv"), a).expect("a.csv");
        fs::write(dir.join("b.csv"), b).expect("b.csv");
        Fixture(dir)
    }

    /// Quoted fields with embedded newlines: the split guess fails and the
    /// sink resets, so the fused parts must come out empty before the retry.
    fn with_quotes() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-q-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        let mut a = String::from("id,v\n");
        let mut b = String::from("id,v\n");
        for i in 0..8000 {
            a.push_str(&format!("{i},\"line1\nline2-{i}\"\n"));
            b.push_str(&format!("{i},\"line1\nlineX-{i}\"\n"));
        }
        fs::write(dir.join("a.csv"), a).expect("a.csv");
        fs::write(dir.join("b.csv"), b).expect("b.csv");
        Fixture(dir)
    }

    fn with_ndjson() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-j-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        let mut a = String::new();
        let mut b = String::new();
        for i in 0..6000 {
            a.push_str(&format!("{{\"id\":{i},\"v\":\"a{i}\"}}\n"));
            b.push_str(&format!(
                "{{\"id\":{i},\"v\":\"{}\"}}\n",
                if i % 3 == 0 {
                    format!("c{i}")
                } else {
                    format!("a{i}")
                }
            ));
        }
        for i in 0..200 {
            a.push_str(&format!("{{\"id\":{i},\"v\":\"dup{i}\"}}\n"));
        }
        fs::write(dir.join("a.json"), a).expect("a.json");
        fs::write(dir.join("b.json"), b).expect("b.json");
        Fixture(dir)
    }

    /// Past the chunking threshold, so four threads sweep four chunks, with a
    /// repeat after every third key: changed, and removed where the key is
    /// missing from B. Each part's first picks include repeats, so the report's
    /// lists need the repeats dropped, the places they held refilled, and the
    /// rows of every chunk after the first numbered past the ones before it.
    fn with_early_repeats() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-lists-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        let mut a = String::from("id,v,w\n");
        let mut b = String::from("id,v,w\n");
        for i in 0..300_000 {
            a.push_str(&format!("{i},a{i},x\n"));
            if i % 3 == 0 {
                a.push_str(&format!("{i},rep{i},y\n"));
            }
            if i % 7 != 0 {
                let v = if i % 2 == 0 { 'b' } else { 'a' };
                b.push_str(&format!("{i},{v}{i},x\n"));
            }
        }
        for i in 300_000..300_050 {
            b.push_str(&format!("{i},n,x\n"));
        }
        fs::write(dir.join("a.csv"), a).expect("a.csv");
        fs::write(dir.join("b.csv"), b).expect("b.csv");
        Fixture(dir)
    }

    /// Past the chunking threshold, with B's added rows interleaved among the
    /// rows A has -- one after every five hundredth -- rather than at the end:
    /// the sweep keeps the rows its matches skip over, and these are them.
    fn with_interleaved_additions() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "compara-fused-added-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        fs::create_dir_all(&dir).expect("a temp directory");
        let mut a = String::from("id,v,w\n");
        let mut b = String::from("id,v,w\n");
        for i in 0..300_000 {
            let k = 2 * i;
            a.push_str(&format!("{k},a{k},x\n"));
            if i % 997 == 5 {
                a.push_str(&format!("{k},rep{k},y\n"));
            }
            if i % 1000 != 7 {
                let v = if i % 17 == 0 { 'b' } else { 'a' };
                b.push_str(&format!("{k},{v}{k},x\n"));
            }
            if i % 500 == 3 {
                b.push_str(&format!("{},new{},z\n", k + 1, k + 1));
            }
        }
        fs::write(dir.join("a.csv"), a).expect("a.csv");
        fs::write(dir.join("b.csv"), b).expect("b.csv");
        Fixture(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs the binary in `dir`; `fused` sets `COMPARA_FUSED_JOIN=1`.
fn run(dir: &Path, args: &[&str], fused: bool) -> (i32, String, String) {
    let _guard = FUSED_ENV.lock().unwrap();
    if fused {
        unsafe { std::env::set_var("COMPARA_FUSED_JOIN", "1") };
    }
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary");
    if fused {
        unsafe { std::env::remove_var("COMPARA_FUSED_JOIN") };
    }
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The counts line without its trailing `| turbo 0.123s`, which is a clock.
fn counts(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or_default();
    match line.rfind('|') {
        Some(at) => line[..at].trim_end().to_string(),
        None => line.to_string(),
    }
}

#[test]
fn fused_summary_counts_match_the_ordinary_join() {
    let fx = Fixture::new();
    let args = [
        "compare",
        "a.csv",
        "b.csv",
        "-k",
        "account_id,txn_id",
        "--summary",
    ];
    let (code, out, err) = run(&fx.0, &args, false);
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "fused run failed: {ferr}");
    assert_eq!(counts(&fout), counts(&out), "fused --summary disagrees");
    let _ = err;
}

#[test]
fn fused_report_runs_the_agreement_check_and_matches() {
    let fx = Fixture::new();
    // The report needs the row lists, so the forced fused run also runs the
    // ordinary join and asserts the two sets of counts agree.
    let args = [
        "compare",
        "a.csv",
        "b.csv",
        "-k",
        "account_id,txn_id",
        "--json",
        "fused.json",
        "-o",
        "fused.html",
        "--no-compress",
    ];
    let (code, out, err) = run(&fx.0, &args, false);
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "fused report run failed: {ferr}");
    assert_eq!(counts(&fout), counts(&out), "fused report counts disagree");
    let plain: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.path("fused.json")).expect("fused.json"))
            .expect("json");
    let _ = (err, plain);
}

#[test]
fn fused_later_duplicates_subtract_exactly() {
    let fx = Fixture::with_duplicates();
    let args = ["compare", "a.csv", "b.csv", "-k", "id", "--summary"];
    let (code, out, _) = run(&fx.0, &args, false);
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "fused run failed: {ferr}");
    assert_eq!(
        counts(&fout),
        counts(&out),
        "later-duplicate subtraction is off"
    );
    assert!(
        counts(&fout).contains("dup keys A 500"),
        "the fixture lost its duplicates: {}",
        counts(&fout)
    );
}

#[test]
fn fused_quoted_splits_reset_cleanly() {
    let fx = Fixture::with_quotes();
    let args = ["compare", "a.csv", "b.csv", "-k", "id", "--summary"];
    let (code, out, _) = run(&fx.0, &args, false);
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "fused run failed: {ferr}");
    assert_eq!(
        counts(&fout),
        counts(&out),
        "a wrong split guess leaked joined rows"
    );
}

#[test]
fn fused_ndjson_counts_match() {
    let fx = Fixture::with_ndjson();
    let args = ["compare", "a.json", "b.json", "-k", "id", "--summary"];
    let (code, out, _) = run(&fx.0, &args, false);
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "fused run failed: {ferr}");
    assert_eq!(counts(&fout), counts(&out), "fused ndjson disagrees");
}

#[test]
fn fused_thread_counts_agree() {
    let fx = Fixture::with_duplicates();
    let mut expected = String::new();
    let mut expected_code = 0;
    for threads in ["1", "2", "4"] {
        let args = [
            "compare",
            "a.csv",
            "b.csv",
            "-k",
            "id",
            "--summary",
            "--threads",
            threads,
        ];
        let (code, out, err) = run(&fx.0, &args, true);
        if expected.is_empty() {
            expected = counts(&out);
            expected_code = code;
        } else {
            assert_eq!(
                code, expected_code,
                "thread count changed the outcome: {err}"
            );
            assert_eq!(counts(&out), expected, "thread count changed the counts");
        }
    }
}

#[test]
fn normalisation_still_takes_the_ordinary_path() {
    let fx = Fixture::new();
    let args = [
        "compare",
        "a.csv",
        "b.csv",
        "-k",
        "account_id,txn_id",
        "--summary",
        "--ignore-case",
    ];
    let (code, out, _) = run(&fx.0, &args, false);
    // Fused is refused under normalisation; the run must still agree.
    let (fcode, fout, ferr) = run(&fx.0, &args, true);
    assert_eq!(fcode, code, "run failed: {ferr}");
    assert_eq!(counts(&fout), counts(&out));
}

#[test]
fn fused_row_lists_match_the_ordinary_join() {
    let fx = Fixture::with_early_repeats();
    // Forced, the run also joins after the sweep and fails unless the report's
    // rows agree: the caps cut inside the first part (10), past it (50000,
    // the default), and past every list (200000).
    for threads in ["1", "4"] {
        for max_rows in ["10", "50000", "200000"] {
            let args = [
                "compare",
                "a.csv",
                "b.csv",
                "-k",
                "id",
                "--threads",
                threads,
                "--max-rows",
                max_rows,
                "-o",
                "report.html",
            ];
            let (code, out, _) = run(&fx.0, &args, false);
            let (fcode, fout, ferr) = run(&fx.0, &args, true);
            assert_eq!(
                fcode, code,
                "fused lists, --threads {threads} --max-rows {max_rows}: {ferr}"
            );
            assert_eq!(counts(&fout), counts(&out));
        }
    }
}

#[test]
fn fused_kept_added_rows_match_the_ordinary_join() {
    let fx = Fixture::with_interleaved_additions();
    // Forced, the run fails unless every row of the report -- the added ones
    // decoded from what the sweep kept -- agrees with the join after it.
    for threads in ["1", "4"] {
        for max_rows in ["10", "50000"] {
            let args = [
                "compare",
                "a.csv",
                "b.csv",
                "-k",
                "id",
                "--threads",
                threads,
                "--max-rows",
                max_rows,
                "-o",
                "report.html",
            ];
            let (code, out, _) = run(&fx.0, &args, false);
            let (fcode, fout, ferr) = run(&fx.0, &args, true);
            assert_eq!(
                fcode, code,
                "kept added rows, --threads {threads} --max-rows {max_rows}: {ferr}"
            );
            assert_eq!(counts(&fout), counts(&out));
        }
    }
}

/// Runs the binary in `dir` with `env` added to its environment only -- not
/// this process's, which the other tests share.
fn run_with(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env_remove("COMPARA_FUSED_JOIN")
        .env_remove("COMPARA_PARALLEL_INSERT")
        .env_remove("COMPARA_NARROW_HASH")
        .env_remove("COMPARA_MEMORY")
        .env_remove("COMPARA_PHASES")
        .envs(env.iter().copied())
        .output()
        .expect("the binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The report's data, without what differs between two runs of the same
/// comparison: the clock, and the time it was made.
fn report_payload(path: &Path) -> String {
    let html = fs::read_to_string(path).expect("the report");
    let open = "<script id=\"payload\" type=\"application/json\">";
    let start = html.find(open).expect("a plain payload") + open.len();
    let end = start + html[start..].find("</script>").expect("the payload's end");
    let mut doc: serde_json::Value = serde_json::from_str(&html[start..end]).expect("json");
    if let Some(meta) = doc.get_mut("meta").and_then(|m| m.as_object_mut()) {
        meta.remove("generated");
        meta.remove("seconds");
    }
    doc.to_string()
}

#[test]
fn the_parallel_insertion_builds_the_serial_index() {
    // Forced at a size the threshold would leave serial, on four threads,
    // through both the ordinary join and the one inside the sweep: the counts,
    // the duplicate sections, and every row of the report must be the serial
    // insertion's, repeated keys and all.
    for fx in [Fixture::with_duplicates(), Fixture::with_early_repeats()] {
        for fused in [false, true] {
            let fused_env = if fused { "1" } else { "0" };
            let args = [
                "compare",
                "a.csv",
                "b.csv",
                "-k",
                "id",
                "--threads",
                "4",
                "--max-rows",
                "1000",
                "--no-compress",
                "-o",
                "report.html",
            ];
            let (code, out, err) = run_with(&fx.0, &args, &[("COMPARA_FUSED_JOIN", fused_env)]);
            assert!(code == 0 || code == 1, "serial run failed: {err}");
            let serial = report_payload(&fx.path("report.html"));
            // Narrowed, different keys share a hash: the parallel insertion
            // takes such a match for a repeat until it proves it, finds it is
            // not, and leaves the index to the serial insertion, which tells
            // the keys apart -- as the join after it must.
            for narrow in ["", "10"] {
                let (pcode, pout, perr) = run_with(
                    &fx.0,
                    &args,
                    &[
                        ("COMPARA_FUSED_JOIN", fused_env),
                        ("COMPARA_PARALLEL_INSERT", "1"),
                        ("COMPARA_NARROW_HASH", narrow),
                    ],
                );
                assert_eq!(
                    pcode, code,
                    "parallel, fused={fused} narrow={narrow:?}: {perr}"
                );
                assert_eq!(
                    counts(&pout),
                    counts(&out),
                    "fused={fused} narrow={narrow:?}"
                );
                assert_eq!(
                    report_payload(&fx.path("report.html")),
                    serial,
                    "the parallel insertion's report differs, fused={fused} narrow={narrow:?}"
                );
            }
        }
    }
}

#[test]
fn a_small_memory_budget_takes_the_fused_path() {
    // Unforced: the budget alone decides. A pair past half of it is joined
    // inside A's sweep, one under it after both sweeps -- and the report is
    // the same either way, row for row.
    let fx = Fixture::with_early_repeats();
    let args = |memory: &'static str| {
        [
            "compare",
            "a.csv",
            "b.csv",
            "-k",
            "id",
            "--threads",
            "4",
            "--max-rows",
            "1000",
            "--no-compress",
            "-o",
            "report.html",
            "--memory",
            memory,
        ]
    };
    let phases = [("COMPARA_PHASES", "1")];

    let (code, out, err) = run_with(&fx.0, &args("64G"), &phases);
    assert!(code == 0 || code == 1, "in-memory run failed: {err}");
    assert!(
        !err.contains("fused join"),
        "64G took the fused path:\n{err}"
    );
    let ordinary = report_payload(&fx.path("report.html"));

    let (fcode, fout, ferr) = run_with(&fx.0, &args("64K"), &phases);
    assert_eq!(fcode, code, "fused run failed: {ferr}");
    assert!(
        ferr.contains("fused join"),
        "64K did not take the fused path:\n{ferr}"
    );
    assert_eq!(counts(&fout), counts(&out));
    assert_eq!(report_payload(&fx.path("report.html")), ordinary);

    // The environment says the same thing for a caller that cannot pass a flag.
    let plain: Vec<&str> = args("1")[..12].to_vec();
    let (ecode, eout, eerr) = run_with(
        &fx.0,
        &plain,
        &[("COMPARA_PHASES", "1"), ("COMPARA_MEMORY", "64K")],
    );
    assert_eq!(ecode, code, "COMPARA_MEMORY run failed: {eerr}");
    assert!(
        eerr.contains("fused join"),
        "COMPARA_MEMORY=64K did not take the fused path:\n{eerr}"
    );
    assert_eq!(counts(&eout), counts(&out));
}

#[test]
fn a_memory_flag_that_is_not_a_size_is_refused() {
    let fx = Fixture::new();
    for bad in ["lots", "0", "8X"] {
        let args = [
            "compare",
            "a.csv",
            "b.csv",
            "-k",
            "account_id,txn_id",
            "--summary",
            "--memory",
            bad,
        ];
        let (code, _, err) = run_with(&fx.0, &args, &[]);
        assert_eq!(code, 2, "--memory {bad} was accepted");
        assert!(err.contains("--memory"), "--memory {bad}: {err}");
    }
}

#[test]
fn parallel_insert_off_switch_forces_serial_index_insert() {
    let fx = Fixture::new();
    let args = [
        "compare",
        "a.csv",
        "b.csv",
        "-k",
        "account_id,txn_id",
        "--summary",
        "--threads",
        "4",
    ];

    let (code_on, out_on, err_on) = run_with(
        &fx.0,
        &args,
        &[("COMPARA_PHASES", "1"), ("COMPARA_PARALLEL_INSERT", "1")],
    );
    assert!(
        code_on == 0 || code_on == 1,
        "parallel insert run failed: {err_on}"
    );
    assert!(
        err_on.contains("index insert (parallel)"),
        "COMPARA_PARALLEL_INSERT=1 did not take parallel index insert:\n{err_on}"
    );

    let (code_off, out_off, err_off) = run_with(
        &fx.0,
        &args,
        &[("COMPARA_PHASES", "1"), ("COMPARA_PARALLEL_INSERT", "0")],
    );
    assert_eq!(code_off, code_on, "serial insert run failed: {err_off}");
    assert!(
        err_off.contains("index insert (serial)"),
        "COMPARA_PARALLEL_INSERT=0 did not take serial index insert:\n{err_off}"
    );
    assert_eq!(counts(&out_on), counts(&out_off));
}
