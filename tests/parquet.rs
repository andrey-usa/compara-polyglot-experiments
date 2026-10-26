//! The Parquet path against the CSV path, on the same rows.
//!
//! The claim the columnar path has to earn is that it is the *same* comparison:
//! the same data, in a format that stores it as columns of dictionary indices
//! rather than as lines of text, must produce the same report. So each case
//! generates a pair in both formats with `compara-gen-data`, which writes CSV
//! and Parquet from one field-by-field recipe, and asserts the whole result —
//! counts, per-column statistics, every changed cell, every added and removed
//! row, the duplicate sections. The generator writes uncompressed pages, which
//! both readers take; snappy, gzip, zstd and LZ4 pages are covered by the
//! fixtures in `tests/formats.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use compara::contract::CompareResult;
use compara::engine::compare;
use compara::options::{Engine, Options};

/// A number no other fixture in this process has. The tests run as parallel
/// threads of one process, and the clock this used to take cannot tell them
/// apart: macOS reports microseconds, and two fixtures made in the same one
/// shared a directory, which the first to finish deleted under the other.
fn fixture_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn generator() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_compara-gen-data"))
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tool: &Path, rows: &str, extra: &[&str]) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!(
            "compara-pq-{}-{}",
            std::process::id(),
            fixture_id()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        for flags in [&[][..], extra] {
            let out = Command::new(tool)
                .args(["--rows", rows, "--out-dir"])
                .arg(&dir)
                .args(["--prefix", "t"])
                .args(flags)
                .output()
                .ok()?;
            assert!(out.status.success(), "compara-gen-data failed: {out:?}");
        }
        Some(Fixture { dir })
    }

    fn path(&self, side: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("t_{side}{ext}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn options(key: &[&str], tweak: impl FnOnce(&mut Options)) -> Options {
    let mut opt = Options::with_key(key.iter().copied());
    opt.ignore = vec!["updated_at".to_string()];
    tweak(&mut opt);
    opt
}

/// Everything the contract carries, with the run's own facts removed so two
/// comparisons of the same rows in different formats are directly comparable.
fn contract(r: &CompareResult) -> String {
    let mut v = serde_json::to_value(r).expect("the result serialises");
    // `Meta` serialises flat, so the run's own facts -- which file, how long,
    // which path -- sit inside it and have to come out of there.
    for at in [Some("meta"), None] {
        let target = match at {
            Some(k) => v.get_mut(k),
            None => Some(&mut v),
        };
        if let Some(o) = target.and_then(|t| t.as_object_mut()) {
            for gone in ["a", "b", "seconds", "engine", "generated", "options"] {
                o.remove(gone);
            }
        }
    }
    serde_json::to_string(&v).unwrap()
}

fn run(path_a: &Path, path_b: &Path, opt: Options, engine: Engine) -> String {
    run_labelled(path_a, path_b, opt, engine).0
}

/// The contract, and the engine that actually ran.
fn run_labelled(
    path_a: &Path,
    path_b: &Path,
    mut opt: Options,
    engine: Engine,
) -> (String, String) {
    opt.engine = engine.to_string();
    let r = compare(path_a, path_b, &mut opt).expect("the comparison runs");
    (contract(&r), r.meta.engine.clone())
}

/// A Parquet pair has two readers behind it: the columnar path, which `auto`
/// takes, and `turbo`'s row decoder, which `--engine turbo` asks for. Both have
/// to give the answer the CSV of the same rows gives, and each is asked for by
/// name and checked to have run -- a test that asks for one and silently gets
/// the other covers half of what it says.
fn same_from_both_readers(
    from_csv: &str,
    a: &Path,
    b: &Path,
    opt: impl Fn() -> Options,
    what: &str,
) {
    for (engine, label) in [(Engine::Auto, "parquet"), (Engine::Turbo, "turbo")] {
        let (from_parquet, ran) = run_labelled(a, b, opt(), engine);
        assert_eq!(ran, label, "{what}: asked for {engine:?}, ran {ran}");
        assert_eq!(
            from_csv, from_parquet,
            "{what}: the {label} reader's report differs from the csv one"
        );
    }
}

/// One case: generate CSV and one Parquet flavour, and require the two reports
/// to agree in full.
fn same_report(rows: &str, gen_flags: &[&str], ext: &str, key: &[&str], tweak: fn(&mut Options)) {
    let tool = generator();
    let Some(f) = Fixture::new(&tool, rows, gen_flags) else {
        eprintln!("skipping: could not create the fixture");
        return;
    };
    let from_csv = run(
        &f.path("a", ".csv"),
        &f.path("b", ".csv"),
        options(key, tweak),
        Engine::Turbo,
    );
    same_from_both_readers(
        &from_csv,
        &f.path("a", ext),
        &f.path("b", ext),
        || options(key, tweak),
        &format!("{rows} rows, {gen_flags:?}"),
    );
}

const PLAIN: &[&str] = &["--format", "parquet", "--compression", "none"];

#[test]
fn parquet_matches_the_csv_it_was_written_from() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |_| {});
}

#[test]
fn uncompressed_matches() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |_| {});
}

#[test]
fn many_small_row_groups() {
    same_report(
        "20k",
        &[
            "--format",
            "parquet",
            "--compression",
            "none",
            "--row-group-size",
            "512",
        ],
        ".parquet",
        &["account_id", "txn_id"],
        |_| {},
    );
}

/// A dictionary budget the data crosses partway makes a column that is
/// dictionary encoded in some row groups and plain in others -- the shape a real
/// writer produces on a high-cardinality string, and the one the reader has to
/// fold into a single form.
#[test]
fn a_column_the_dictionary_gives_up_on() {
    same_report(
        "20k",
        &[
            "--format",
            "parquet",
            "--compression",
            "none",
            "--dict-limit",
            "175",
            "--row-group-size",
            "300",
        ],
        ".parquet",
        &["account_id", "txn_id"],
        |_| {},
    );
}

#[test]
fn every_column_plain() {
    same_report(
        "20k",
        &[
            "--format",
            "parquet",
            "--compression",
            "none",
            "--dict-limit",
            "1",
        ],
        ".parquet",
        &["account_id", "txn_id"],
        |_| {},
    );
}

#[test]
fn rows_that_do_not_fill_a_group() {
    same_report("1k", PLAIN, ".parquet", &["account_id", "txn_id"], |_| {});
}

/// A key column both sides store as a dictionary takes the shared-id path,
/// where the join never looks at a byte after the dictionaries are mapped.
#[test]
fn a_dictionary_key_column() {
    same_report("20k", PLAIN, ".parquet", &["currency", "status"], |_| {});
}

/// The key columns are read from a queue, `2 * key_size` jobs over as many
/// lanes as `--threads` allows, so which lane decodes which column is not fixed
/// and a job put away in the wrong slot would swap A's key column for B's. Three
/// key columns is six jobs, more than the lanes any runner here has, so the
/// lanes go round the queue more than once.
#[test]
fn a_key_wider_than_the_lanes() {
    same_report(
        "20k",
        PLAIN,
        ".parquet",
        &["account_id", "txn_id", "currency"],
        |_| {},
    );
}

/// One lane, which is the other end of that queue: every job on the calling
/// thread, in order, with nothing spawned.
#[test]
fn one_thread_reads_every_key_column() {
    same_report(
        "20k",
        PLAIN,
        ".parquet",
        &["account_id", "txn_id", "currency"],
        |o| {
            o.threads = Some(1);
        },
    );
}

/// More threads than columns: each column's row groups are split into runs,
/// each read on a thread of its own, with both sides of a column at once. The
/// report is still the CSV's -- on many small groups, compressed and not, and
/// on a column that is dictionary encoded in some groups and plain in others,
/// so that runs meet in both forms.
#[test]
fn row_groups_read_in_runs() {
    for flags in [
        &[
            "--format",
            "parquet",
            "--compression",
            "none",
            "--row-group-size",
            "512",
        ][..],
        &[
            "--format",
            "parquet",
            "--compression",
            "zstd",
            "--row-group-size",
            "700",
        ][..],
        &[
            "--format",
            "parquet",
            "--compression",
            "none",
            "--dict-limit",
            "175",
            "--row-group-size",
            "300",
        ][..],
    ] {
        same_report("20k", flags, ".parquet", &["account_id", "txn_id"], |o| {
            o.threads = Some(16);
        });
    }
}

#[test]
fn with_trim() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |o| {
        o.trim = true;
    });
}

#[test]
fn with_empty_is_null() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |o| {
        o.empty_is_null = true;
    });
}

/// A tolerance makes equality non-transitive, so it cannot be given an id: the
/// column falls back to comparing bytes, and must still agree.
#[test]
fn with_tolerance() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |o| {
        o.tolerance = 0.01;
    });
}

#[test]
fn with_a_small_cap() {
    same_report("20k", PLAIN, ".parquet", &["account_id", "txn_id"], |o| {
        o.max_rows = 3;
    });
}

/// One side Parquet and one side text cannot take the columnar path -- there is
/// no column to compare a byte stream against -- so `turbo` decodes the Parquet
/// side into rows instead. Slower than the columnar path, and the same answer:
/// that is what this checks, against csv against csv on the same rows.
#[test]
fn a_mixed_parquet_and_text_pair_is_read_as_rows() {
    let tool = generator();
    let Some(f) = Fixture::new(&tool, "1k", PLAIN) else {
        return;
    };
    let key = &["account_id", "txn_id"];
    let from_text = run(
        &f.path("a", ".csv"),
        &f.path("b", ".csv"),
        options(key, |_| {}),
        Engine::Turbo,
    );
    let mixed = run(
        &f.path("a", ".parquet"),
        &f.path("b", ".csv"),
        options(key, |_| {}),
        Engine::Turbo,
    );
    assert_eq!(
        from_text, mixed,
        "the mixed pair differs from csv against csv"
    );

    // And it is honest about the path it took: `turbo`, not `parquet`.
    let mut opt = options(key, |_| {});
    let result = compare(&f.path("a", ".parquet"), &f.path("b", ".csv"), &mut opt)
        .expect("a mixed pair is answered");
    assert_eq!(result.meta.engine, "turbo");
}

/// An engine that cannot read Parquet says so by name rather than handing the
/// footer to a CSV parser and reporting the parse error it makes of it.
#[test]
fn an_engine_that_cannot_read_parquet_says_so() {
    let tool = generator();
    let Some(f) = Fixture::new(&tool, "1k", PLAIN) else {
        return;
    };
    let mut opt = options(&["account_id", "txn_id"], |_| {});
    opt.engine = "sortmerge".to_string();
    let err = compare(&f.path("a", ".parquet"), &f.path("b", ".csv"), &mut opt)
        .expect_err("sortmerge cannot read parquet");
    assert!(
        err.to_string().contains("reads CSV only"),
        "the error should say which: {err}"
    );
}

/// The report says which path ran -- which is not always the columnar one.
///
/// This used to assert that `--engine turbo` on a Parquet pair still reported
/// `parquet`, because the router ignored the request and always took the
/// columnar path. That was the bug rather than the contract: a zstd file was
/// refused outright by a binary that could read it, and asking for the reader
/// that could did nothing. Both halves are pinned here now.
#[test]
fn the_report_names_the_path_that_ran() {
    let tool = generator();
    let Some(f) = Fixture::new(&tool, "1k", PLAIN) else {
        return;
    };
    let (a, b) = (f.path("a", ".parquet"), f.path("b", ".parquet"));

    let mut opt = options(&["account_id", "txn_id"], |_| {});
    let by_default = compare(&a, &b, &mut opt).unwrap();
    assert_eq!(
        by_default.meta.engine, "parquet",
        "the fast path by default"
    );

    let mut opt = options(&["account_id", "txn_id"], |_| {});
    opt.engine = "turbo".to_string();
    let asked_for = compare(&a, &b, &mut opt).unwrap();
    assert_eq!(
        asked_for.meta.engine, "turbo",
        "an explicit request is honoured"
    );

    // Same rows, two readers: the counts have to agree, or one of them is wrong.
    assert_eq!(by_default.counts, asked_for.counts);
}

/// zstd, which polars writes unless told otherwise, is read on the columnar
/// path, and reads as the same rows written uncompressed.
///
/// It used to fall through to `turbo`, which was itself the fix for an NYC TLC
/// trip-data file, zstd, turned away with "only uncompressed and snappy
/// parquet are read here" by a build that read zstd perfectly well through
/// `turbo`. What still falls through, version-2 pages, is held to that in
/// tests/formats.rs.
#[test]
fn a_zstd_pair_is_read_on_the_columnar_path() {
    use compara::gendata::{Compression, Format, generate_as};

    let dir = std::env::temp_dir().join(format!(
        "compara-zstd-{}-{}",
        std::process::id(),
        fixture_id()
    ));
    std::fs::create_dir_all(&dir).expect("a temp directory");

    let plain = |side: &str| dir.join(format!("plain_{side}.parquet"));
    let zstd = |side: &str| dir.join(format!("zstd_{side}.parquet"));
    generate_as(
        2_000,
        &plain("a"),
        &plain("b"),
        7,
        Format::Parquet,
        Compression::None,
    )
    .expect("the plain pair");
    generate_as(
        2_000,
        &zstd("a"),
        &zstd("b"),
        7,
        Format::Parquet,
        Compression::Zstd,
    )
    .expect("the zstd pair");

    let mut opt = options(&["account_id", "txn_id"], |_| {});
    let compressed = compare(&zstd("a"), &zstd("b"), &mut opt).expect("zstd is read, not refused");
    assert_eq!(
        compressed.meta.engine, "parquet",
        "zstd on the columnar path"
    );

    // The same rows written twice, once each way. Reading them has to give the
    // same answer, or one of the two is reading something else.
    let mut opt = options(&["account_id", "txn_id"], |_| {});
    let uncompressed = compare(&plain("a"), &plain("b"), &mut opt).expect("plain is read");
    assert_eq!(
        uncompressed.meta.engine, "parquet",
        "the columnar path, unchanged"
    );
    assert_eq!(compressed.counts, uncompressed.counts);
    assert_eq!(compressed.columns, uncompressed.columns);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A file that is simply wrong still fails, rather than being handed to the
/// other reader and reported as its problem.
#[test]
fn a_real_error_is_not_dressed_up_as_an_unsupported_codec() {
    let tool = generator();
    let Some(f) = Fixture::new(&tool, "1k", PLAIN) else {
        return;
    };
    let mut opt = options(&["no_such_column"], |_| {});
    let err = compare(&f.path("a", ".parquet"), &f.path("b", ".parquet"), &mut opt)
        .expect_err("a key column that is in neither file");
    assert!(
        err.to_string().contains("the parquet engine failed"),
        "the columnar path owns this failure: {err}"
    );
}

/// The counts do not depend on the pass that only produces a sample.
///
/// A key matches from either side or from neither, so the number of B's keys
/// with an A counterpart is exactly the pair count the A pass produces, and
/// `added` is B's distinct keys minus it. What the B pass is *for* is the
/// report's added section, which names rows -- so when nothing will read those
/// rows the pass does not run and the count is derived instead. That is a
/// second full random-probed pass over A's table skipped, and on a 2M pair it
/// is the whole of this path's join.
///
/// The derivation is the risk, so this asks for it where it is hardest: two
/// files of different sizes, so a million of B's keys have no counterpart at
/// all, and duplicate keys on both sides so "distinct" and "rows" are not the
/// same number.
#[test]
fn skipping_the_added_sample_does_not_change_the_counts() {
    let tool = generator();
    let Some(big) = Fixture::new(&tool, "20k", PLAIN) else {
        return;
    };
    let Some(small) = Fixture::new(&tool, "5k", PLAIN) else {
        return;
    };

    // Both directions: B larger than A is where `added` is large and derived,
    // A larger than B is where it is zero and must stay zero.
    for (a, b) in [
        (small.path("a", ".parquet"), big.path("b", ".parquet")),
        (big.path("a", ".parquet"), small.path("b", ".parquet")),
    ] {
        let with = compare(&a, &b, &mut options(&["account_id", "txn_id"], |_| {}))
            .expect("the comparison runs");
        let without = compare(
            &a,
            &b,
            &mut options(&["account_id", "txn_id"], |o| o.row_lists = false),
        )
        .expect("the comparison runs");

        assert_eq!(
            serde_json::to_string(&with.counts).unwrap(),
            serde_json::to_string(&without.counts).unwrap(),
            "the counts changed when the added sample was skipped ({} vs {})",
            a.display(),
            b.display()
        );
        // The case the assert above would pass vacuously on.
        assert!(
            with.counts.added > 0 || with.counts.removed > 0,
            "this pair has neither added nor removed rows, so it proves nothing"
        );
    }
}

/// A value that is only whitespace trims to "", and "" is a value: only
/// `--empty-is-null` makes it absent. The CSV engines and `turbo`'s Parquet
/// decoder always read it that way; the columnar path read it as absent under
/// `--trim` alone, so `"  "` against an empty cell was a change as CSV and no
/// change as Parquet.
///
/// `compara-gen-data` never writes such a value, which is why `with_trim` above could
/// not see it; this writes its own pair, in both formats, from one table.
#[test]
fn trim_keeps_a_value_that_trims_to_nothing() {
    use compara::gendata::parquet::{Compression, Writer};

    const COLUMNS: [&str; 3] = ["id", "v", "w"];
    let a = [
        ["k1", "  ", "x"],
        ["k2", "a", "y"],
        ["k3", "  ", "z"],
        ["k4", " b ", "w"],
    ];
    let b = [
        ["k1", "", "x"],
        ["k2", "a", "y"],
        ["k3", "\t", "z"],
        ["k4", "b", "w"],
    ];
    let dir = std::env::temp_dir().join(format!("compara-pq-trim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (side, rows) in [("a", &a), ("b", &b)] {
        let mut csv = COLUMNS.join(",") + "\n";
        for row in rows.iter() {
            csv += &(row.join(",") + "\n");
        }
        std::fs::write(dir.join(format!("{side}.csv")), csv).unwrap();
        let mut w = Writer::create(
            &dir.join(format!("{side}.parquet")),
            &COLUMNS,
            Compression::None,
            1024,
        )
        .unwrap();
        for row in rows.iter() {
            w.write_row(row.iter().copied()).unwrap();
        }
        w.finish().unwrap();
    }

    // (trim, empty_is_null, changed rows): under --trim alone, k1 is "" against
    // an empty cell -- a change -- and k3 is "" against "", which is not.
    for (trim, empty_is_null, changed) in [(true, false, 1), (true, true, 0), (false, false, 3)] {
        let opt = || {
            let mut o = Options::with_key(["id"]);
            o.trim = trim;
            o.empty_is_null = empty_is_null;
            o
        };
        let from_csv = run(&dir.join("a.csv"), &dir.join("b.csv"), opt(), Engine::Turbo);
        let counted: serde_json::Value = serde_json::from_str(&from_csv).unwrap();
        assert_eq!(
            counted["counts"]["changed"], changed,
            "csv, trim={trim} empty_is_null={empty_is_null}: {from_csv}"
        );
        same_from_both_readers(
            &from_csv,
            &dir.join("a.parquet"),
            &dir.join("b.parquet"),
            opt,
            &format!("trim={trim} empty_is_null={empty_is_null}"),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
