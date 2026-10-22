//! The comparison contract, asserted against every engine.
//!
//! The point of the engine registry is that the backends are interchangeable, so
//! almost everything here runs three times.

use std::fs;
use std::path::{Path, PathBuf};

use compara::contract::{Cell, ColumnStat, Counts};
use compara::engine::{available, compare};
use compara::options::{Engine, Options};

const SIMPLE_A: &str = "id,name,qty\n1,ann,5\n2,bob,7\n3,cid,9\n";
const SIMPLE_B: &str = "id,name,qty\n1,ann,5\n2,bob,8\n4,dee,1\n";

/// A throwaway directory holding one pair of CSV files.
struct Fixture {
    dir: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fixture {
    fn new(a_body: &str, b_body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("compara-test-{}", unique()));
        fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("a.csv");
        let b = dir.join("b.csv");
        fs::write(&a, a_body).expect("write a");
        fs::write(&b, b_body).expect("write b");
        Fixture { dir, a, b }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (n << 32) ^ std::process::id() as u64
}

fn options(key: &[&str], engine: Engine) -> Options {
    let mut opt = Options::with_key(key.iter().copied());
    opt.engine = engine.label().to_string();
    opt
}

/// Runs a check against every backend that can load here.
fn each_engine(mut check: impl FnMut(Engine)) {
    for engine in Engine::CONCRETE {
        if !available(engine) {
            eprintln!("skipping {engine}: not available here");
            continue;
        }
        check(engine);
    }
}

#[test]
fn counts_are_the_same_whichever_engine_runs() {
    let f = Fixture::new(SIMPLE_A, SIMPLE_B);
    each_engine(|engine| {
        let mut opt = options(&["id"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        let want = Counts {
            a_rows: 3,
            b_rows: 3,
            a_keys: 3,
            b_keys: 3,
            matched: 2,
            unchanged: 1,
            changed: 1,
            added: 1,
            removed: 1,
            ..Counts::default()
        };
        assert_eq!(result.counts, want, "{engine} counts");
        assert!(
            !result.identical(),
            "{engine} should not call these identical"
        );
    });
}

/// The engines must agree cell for cell, not just on the totals: a report from
/// one is meant to be indistinguishable from a report from another.
#[test]
fn engines_agree_cell_for_cell() {
    let f = Fixture::new(SIMPLE_A, SIMPLE_B);
    let mut reference: Option<(String, Engine)> = None;
    each_engine(|engine| {
        let mut opt = options(&["id"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        let payload = serde_json::json!({
            "counts": result.counts, "columns": result.columns, "changed": result.changed,
            "added": result.added, "removed": result.removed,
            "dup_a": result.dup_a, "dup_b": result.dup_b,
        })
        .to_string();
        match &reference {
            None => reference = Some((payload, engine)),
            Some((want, first)) => assert_eq!(&payload, want, "{engine} diverges from {first}"),
        }
    });
}

#[test]
fn a_file_compared_with_itself_is_identical() {
    let f = Fixture::new(SIMPLE_A, SIMPLE_A);
    each_engine(|engine| {
        let mut opt = options(&["id"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert!(result.identical(), "{engine}: {:?}", result.counts);
    });
}

/// An empty field is an absent value whether or not it was quoted. Every engine
/// has to agree, or a count would change with the engine: a reader that keeps a
/// quoted empty as a zero-length string counts it as present.
#[test]
fn a_quoted_empty_field_is_absent() {
    let f = Fixture::new("k,v\n1,\n2,\"\"\n3,keep\n", "k,v\n1,\"\"\n2,\n3,keep\n");
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert_eq!(
            result.counts.changed, 0,
            "{engine}: an empty field is absent, quoted or not"
        );
    });
}

#[test]
fn the_first_occurrence_of_a_duplicated_key_joins() {
    let f = Fixture::new("k,v\n1,first\n1,second\n2,x\n", "k,v\n1,first\n2,y\n");
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert_eq!(
            (result.counts.a_dup_keys, result.counts.a_dup_rows),
            (1, 2),
            "{engine}"
        );
        // Only key 2 changed: the first occurrence of key 1 matched.
        assert_eq!(result.counts.changed, 1, "{engine}");
        assert_eq!(result.dup_a.rows.len(), 1, "{engine}");
        assert_eq!(
            result.dup_a.rows[0].last(),
            Some(&Cell::Count(2)),
            "{engine}"
        );
    });
}

#[test]
fn tolerance_trim_and_ignore_case_apply() {
    let f = Fixture::new("k,v,s\n1,10.00, Ann \n", "k,v,s\n1,10.004,ann\n");
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        opt.tolerance = 0.01;
        opt.trim = true;
        opt.ignore_case = true;
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert_eq!(result.counts.changed, 0, "{engine}");
    });
}

#[test]
fn column_stats_count_blanked_and_filled() {
    let f = Fixture::new("k,v\n1,x\n2,\n", "k,v\n1,\n2,y\n");
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        let want = ColumnStat {
            name: "v".into(),
            changed: 2,
            blanked: 1,
            filled: 1,
        };
        assert_eq!(result.columns, vec![want], "{engine}");
    });
}

#[test]
fn a_semicolon_delimiter_is_sniffed() {
    let f = Fixture::new("k;v\n1;x\n", "k;v\n1;y\n");
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert_eq!(result.counts.changed, 1, "{engine}");
    });
}

#[test]
fn sections_are_capped_but_counts_stay_exact() {
    let mut a = String::from("k,v\n");
    let mut b = String::from("k,v\n");
    for i in 0..10u8 {
        let key = (b'a' + i) as char;
        a.push_str(&format!("{key},x\n"));
        b.push_str(&format!("{key},y\n"));
    }
    let f = Fixture::new(&a, &b);
    each_engine(|engine| {
        let mut opt = options(&["k"], engine);
        opt.max_rows = 3;
        let result = compare(&f.a, &f.b, &mut opt).expect("compare");
        assert_eq!(
            result.counts.changed, 10,
            "{engine}: the count stays exact when capped"
        );
        assert_eq!(result.changed.rows.len(), 3, "{engine}");
        assert!(result.changed.truncated, "{engine}");
    });
}

#[test]
fn export_dir_holds_the_uncapped_rows() {
    let f = Fixture::new(SIMPLE_A, SIMPLE_B);
    each_engine(|engine| {
        let dir = f.dir.join(format!("export-{engine}")).join("nested");
        let mut opt = options(&["id"], engine);
        opt.export_dir = Some(dir.to_string_lossy().into_owned());
        compare(&f.a, &f.b, &mut opt).expect("compare");
        for name in ["changed.csv", "added.csv", "removed.csv"] {
            let text = fs::read_to_string(dir.join(name))
                .unwrap_or_else(|e| panic!("{engine} {name}: {e}"));
            assert_eq!(
                text.trim().lines().count(),
                2,
                "{engine} {name}: a header and one row"
            );
        }
    });
}

#[test]
fn ignore_removes_a_column_from_the_comparison() {
    let f = Fixture::new("k,x,y\n1,a,b\n", "k,x,y\n1,A,B\n");
    let mut opt = options(&["k"], Engine::Native);
    opt.ignore = vec!["x".into()];
    let result = compare(&f.a, &f.b, &mut opt).expect("compare");
    assert_eq!(result.meta.engine_meta.compared, vec!["y".to_string()]);
}

#[test]
fn a_key_missing_from_one_file_is_an_error() {
    let f = Fixture::new("k,v\n1,x\n", "j,v\n1,x\n");
    let mut opt = options(&["k"], Engine::Native);
    assert!(compare(&f.a, &f.b, &mut opt).is_err());
}

#[test]
fn a_missing_file_is_an_error() {
    let f = Fixture::new(SIMPLE_A, SIMPLE_B);
    let mut opt = options(&["id"], Engine::Native);
    assert!(compare(&f.a, Path::new("/nonexistent/nope.csv"), &mut opt).is_err());
}

/// `--threads` is the width of the whole run, and every path that spreads work
/// reads it from here. It used to be read in one engine only.
#[test]
fn the_thread_budget_is_the_flag_when_the_flag_is_set() {
    let mut opt = Options::with_key(["k"]);
    assert_eq!(
        opt.thread_budget(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    opt.threads = Some(3);
    assert_eq!(opt.thread_budget(), 3);
}

#[test]
fn bad_options_are_rejected() {
    type Mutate = fn(&mut Options);
    let cases: Vec<(&str, Mutate)> = vec![
        ("no key", |o| o.key.clear()),
        ("negative tolerance", |o| o.tolerance = -1.0),
        ("zero max rows", |o| o.max_rows = 0),
        ("unknown engine", |o| o.engine = "nope".into()),
        ("zero threads", |o| o.threads = Some(0)),
    ];
    for (name, mutate) in cases {
        let mut opt = Options::with_key(["k"]);
        mutate(&mut opt);
        assert!(opt.validate().is_err(), "validate accepted {name}");
    }
}
