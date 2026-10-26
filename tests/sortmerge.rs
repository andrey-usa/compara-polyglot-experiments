//! The spill-and-merge half of the sort-merge engine.
//!
//! `compare.rs` already runs this engine alongside every other one, because it
//! enumerates `Engine::CONCRETE`. What those tests never reach is the path that
//! only runs on a file too big for one batch: sorted runs written to disk and
//! merged back. Shrinking the batch budget forces it.

use std::fs;
use std::path::{Path, PathBuf};

use compara::engine::compare;
use compara::engine::sortmerge::BATCH_BYTES_ENV;
use compara::options::{Engine, Options};

struct Fixture {
    dir: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fixture {
    fn new(a_body: &str, b_body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "compara-sortmerge-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
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

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn options(key: &[&str], engine: Engine) -> Options {
    let mut opt = Options::with_key(key.iter().copied());
    opt.engine = engine.label().to_string();
    opt
}

fn payload(result: &compara::contract::CompareResult) -> String {
    serde_json::json!({
        "counts": result.counts, "columns": result.columns, "changed": result.changed,
        "added": result.added, "removed": result.removed,
        "dup_a": result.dup_a, "dup_b": result.dup_b,
    })
    .to_string()
}

/// The batch budget is read from the environment, which is process-wide, so the
/// tests that set it run under one lock and put it back afterwards.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_tiny_batches<T>(body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var(BATCH_BYTES_ENV, "2048") };
    let out = body();
    unsafe { std::env::remove_var(BATCH_BYTES_ENV) };
    out
}

fn compare_with(
    a: &Path,
    b: &Path,
    key: &[&str],
    engine: Engine,
) -> compara::contract::CompareResult {
    let mut opt = options(key, engine);
    compare(a, b, &mut opt).expect("compare")
}

#[test]
fn spilled_runs_match_the_in_memory_sort() {
    let f = Fixture::new(
        "id,name,qty\n1,ann,5\n2,bob,7\n3,cid,9\n",
        "id,name,qty\n1,ann,5\n2,bob,8\n4,dee,1\n",
    );
    let in_memory = payload(&compare_with(&f.a, &f.b, &["id"], Engine::SortMerge));
    let spilled =
        with_tiny_batches(|| payload(&compare_with(&f.a, &f.b, &["id"], Engine::SortMerge)));
    assert_eq!(spilled, in_memory, "spilling changed the answer");
}

/// The row the answer depends on is neither the smallest nor the largest by
/// value, so a sort that is not stable, or a merge that does not break ties by
/// run, picks one of the other two.
#[test]
fn first_occurrence_survives_a_spill() {
    let f = Fixture::new(
        "k,v\n3,c\n1,first\n2,b\n1,second\n1,third\n",
        "k,v\n1,first\n2,b\n3,c\n",
    );
    let result = with_tiny_batches(|| compare_with(&f.a, &f.b, &["k"], Engine::SortMerge));
    assert_eq!(
        result.counts.changed, 0,
        "the first row for key 1 is unchanged"
    );
    assert_eq!(result.counts.a_dup_keys, 1);
    assert_eq!(result.counts.a_dup_rows, 3);
}

/// Quotes, commas, newlines and absent fields all have to survive the round trip
/// through the spill format, not only the CSV reader.
#[test]
fn the_spill_format_round_trips_awkward_fields() {
    let f = Fixture::new(
        "k,v,w\n1,\"a\"\"b\",\n2,\"has,comma\",x\n3,\"two\nlines\",y\n",
        "k,v,w\n1,\"a\"\"b\",z\n2,\"has,comma\",x\n3,\"two\nlines\",y\n",
    );
    let reference = compare_with(&f.a, &f.b, &["k"], Engine::Native);
    let spilled = with_tiny_batches(|| compare_with(&f.a, &f.b, &["k"], Engine::SortMerge));
    assert_eq!(spilled.counts, reference.counts);
    assert_ne!(
        reference.counts.changed, 0,
        "the fixture is meant to find a difference"
    );
}

/// The fixture in tests/fixtures holds every shape that has broken an engine in
/// this project, or plausibly could; its README says which row is which.
#[test]
fn every_engine_agrees_on_the_awkward_fixture() {
    let a = Path::new("tests").join("fixtures").join("awkward_a.csv");
    let b = Path::new("tests").join("fixtures").join("awkward_b.csv");

    for (trim, ignore_case) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut reference: Option<(String, Engine)> = None;
        for engine in Engine::CONCRETE {
            if !compara::engine::available(engine) {
                continue;
            }
            let mut opt = options(&["k"], engine);
            opt.trim = trim;
            opt.ignore_case = ignore_case;
            let result = compare(&a, &b, &mut opt).expect("compare");
            let got = payload(&result);
            match &reference {
                None => reference = Some((got, engine)),
                Some((want, first)) => assert_eq!(
                    &got, want,
                    "{engine} diverges from {first} at trim={trim} ignore_case={ignore_case}"
                ),
            }
        }
    }
}
