//! The three input formats, and the thread count.
//!
//! Both are the same kind of risk: a change that is supposed to be invisible in
//! the answer. A file read as Parquet has to produce the counts it produces as
//! CSV, and a comparison split across four threads has to produce the counts it
//! produces on one — including *which* rows survive `--max-rows`, which is the
//! part that silently stops being true when ranges are merged out of order.

use std::fs;
use std::path::{Path, PathBuf};

use compara::contract::CompareResult;
use compara::engine::compare;
use compara::options::Options;

fn fixture(name: &str) -> PathBuf {
    Path::new("tests")
        .join("fixtures")
        .join("formats")
        .join(name)
}

fn options(key: &[&str]) -> Options {
    let mut opt = Options::with_key(key.iter().copied());
    opt.engine = "turbo".to_string();
    opt
}

fn run(a: &Path, b: &Path, opt: &mut Options) -> CompareResult {
    compare(a, b, opt)
        .unwrap_or_else(|e| panic!("comparing {} with {}: {e}", a.display(), b.display()))
}

/// Counts and per-column stats, which is what every port is held to.
fn answer(result: &CompareResult) -> String {
    let mut out = format!("{:?}", result.counts);
    for column in &result.columns {
        out.push_str(&format!(
            "\n{} {} {} {}",
            column.name, column.changed, column.blanked, column.filled
        ));
    }
    out
}

/// The same table written eight ways, compared against the same other side.
///
/// Each of these is a *writer* decision — dictionary or plain, five codecs,
/// version-2 delta pages, one row group or five — and none of them is allowed to
/// change the answer. `scripts/make_parquet_fixtures.py` writes them with
/// pyarrow, so this is a test against another implementation's output rather
/// than against our own.
#[test]
fn every_encoding_of_a_parquet_file_reads_the_same_way() {
    let b = fixture("b.csv");
    let expected = answer(&run(&fixture("a.csv"), &b, &mut options(&["id"])));

    for name in [
        "a_dict_snappy.parquet",
        "a_plain_none.parquet",
        "a_dict_gzip.parquet",
        "a_plain_zstd.parquet",
        "a_plain_lz4.parquet",
        "a_delta_v2.parquet",
        "a_dict_row_groups.parquet",
        "a.ndjson",
        "a.tsv",
    ] {
        let got = answer(&run(&fixture(name), &b, &mut options(&["id"])));
        assert_eq!(got, expected, "{name} did not read as the CSV does");
    }
}

/// Gzip, zstd and LZ4 pages are read on the columnar path, with the row
/// reader's answer, as snappy and uncompressed ones always were. Until they
/// were, any of them sent the pair row by row, and zstd is what polars writes
/// unless told otherwise. Version-2 pages still go row by row.
#[test]
fn every_codec_a_writer_picks_by_default_takes_the_columnar_path() {
    let b = fixture("b_dict_snappy.parquet");
    for (name, path) in [
        ("a_dict_snappy.parquet", "parquet"),
        ("a_plain_none.parquet", "parquet"),
        ("a_dict_gzip.parquet", "parquet"),
        ("a_plain_zstd.parquet", "parquet"),
        ("a_plain_lz4.parquet", "parquet"),
        ("a_delta_v2.parquet", "turbo"),
    ] {
        let mut columnar = options(&["id"]);
        columnar.engine = "auto".to_string();
        let got = run(&fixture(name), &b, &mut columnar);
        assert_eq!(got.meta.engine, path, "{name}");
        let want = run(&fixture(name), &b, &mut options(&["id"]));
        assert_eq!(whole(&got), whole(&want), "{name}");
    }
}

/// Either side may be in any format, and the two need not agree: a CSV export
/// compares against the Parquet a warehouse emits.
#[test]
fn the_two_sides_need_not_be_in_the_same_format() {
    let expected = answer(&run(
        &fixture("a.csv"),
        &fixture("b.csv"),
        &mut options(&["id"]),
    ));
    for (a, b) in [
        ("a.ndjson", "b.csv"),
        ("a.csv", "b.ndjson"),
        ("a.tsv", "b.tsv"),
        ("a.tsv", "b.csv"),
        ("a.csv", "b.tsv"),
        ("a.tsv", "b_dict_snappy.parquet"),
        ("a_dict_snappy.parquet", "b.csv"),
        ("a.csv", "b_dict_snappy.parquet"),
        ("a.ndjson", "b_dict_snappy.parquet"),
        ("a_dict_snappy.parquet", "b_dict_snappy.parquet"),
    ] {
        let got = answer(&run(&fixture(a), &fixture(b), &mut options(&["id"])));
        assert_eq!(got, expected, "{a} against {b}");
    }
}

/// A tab-separated file is a CSV file with another separator, and every engine
/// reads it as one: the whole answer -- counts, column statistics, every
/// changed, added, removed and duplicated row -- and the export files are the
/// CSV pair's, byte for byte.
#[test]
fn tsv_reads_as_csv_on_every_engine() {
    let dir = std::env::temp_dir().join(format!("compara-tsv-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for engine in ["turbo", "sortmerge", "native"] {
        let read = |ext: &str| {
            let to = dir.join(format!("{engine}-{ext}"));
            let mut opt = options(&["id"]);
            opt.engine = engine.to_string();
            opt.export_dir = Some(to.to_string_lossy().into_owned());
            let got = run(
                &fixture(&format!("a.{ext}")),
                &fixture(&format!("b.{ext}")),
                &mut opt,
            );
            (whole(&got), exported(&to))
        };
        let (csv, tsv) = (read("csv"), read("tsv"));
        assert_eq!(tsv.0, csv.0, "{engine}: the answer");
        assert_eq!(tsv.1, csv.1, "{engine}: the export");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A typed Parquet column has to become the text a CSV of the same data holds,
/// and `typed.csv` is that text: written from the same values by the fixture
/// script, so this compares the reader's rendering against a stated expectation
/// rather than against another copy of itself.
#[test]
fn typed_columns_render_as_the_csv_of_the_same_data() {
    let result = run(
        &fixture("typed.parquet"),
        &fixture("typed.csv"),
        &mut options(&["id"]),
    );
    assert_eq!(
        result.counts.changed,
        0,
        "a typed column rendered differently from the CSV: {:?}",
        result
            .columns
            .iter()
            .filter(|c| c.changed > 0)
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(result.counts.matched, 8);
}

/// A file with quoted newlines and doubled quotes, big enough that the engine
/// splits it into chunks.
///
/// This is where threading can actually break: a chunk boundary that lands
/// inside a quoted field would start parsing mid-row, and the only reason it
/// does not is the quote-parity count. The rows are deliberately awkward.
fn awkward_pair(dir: &Path) -> (PathBuf, PathBuf) {
    let mut a = String::from("k,v,w\n");
    let mut b = String::from("k,v,w\n");
    for i in 0..100_000 {
        let quoted = format!("\"line {i}\nsecond, with \"\"quotes\"\" in it\"");
        a.push_str(&format!("K{i},{quoted},{i}\n"));
        let value = if i % 7 == 0 { i + 1 } else { i };
        b.push_str(&format!("K{i},{quoted},{value}\n"));
    }
    let (a_path, b_path) = (dir.join("a.csv"), dir.join("b.csv"));
    fs::write(&a_path, a).expect("the A file");
    fs::write(&b_path, b).expect("the B file");
    (a_path, b_path)
}

#[test]
fn the_thread_count_does_not_change_the_answer() {
    let dir = std::env::temp_dir().join(format!("compara-threads-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("a temp dir");
    let (a, b) = awkward_pair(&dir);
    assert!(
        fs::metadata(&a).expect("the A file").len() > 4 << 20,
        "the fixture must be over the chunking threshold or this tests nothing"
    );

    let mut reference: Option<String> = None;
    for threads in [1usize, 2, 3, 4, 8, 16] {
        let mut opt = options(&["k"]);
        opt.threads = Some(threads);
        opt.max_rows = 25; // small enough that the cap decides which rows are kept
        let result = run(&a, &b, &mut opt);
        let mut got = answer(&result);
        // The kept rows, not just how many: merging ranges out of order would
        // keep a different twenty-five and leave every count intact.
        got.push_str(&format!("\n{:?}", result.changed));
        match &reference {
            None => reference = Some(got),
            Some(want) => assert_eq!(&got, want, "{threads} threads gave a different answer"),
        }
    }
    fs::remove_dir_all(&dir).ok();
}

/// The same, for a columnar file: the rows come from the Parquet reader rather
/// than from a chunked scan, and they are hashed in parallel ranges.
#[test]
fn the_thread_count_does_not_change_a_parquet_answer() {
    let mut reference: Option<String> = None;
    for threads in [1usize, 2, 4, 8] {
        let mut opt = options(&["id"]);
        opt.threads = Some(threads);
        let got = answer(&run(
            &fixture("a_dict_row_groups.parquet"),
            &fixture("b_dict_snappy.parquet"),
            &mut opt,
        ));
        match &reference {
            None => reference = Some(got),
            Some(want) => assert_eq!(&got, want, "{threads} threads gave a different answer"),
        }
    }
}

/// A file that is not what its name says is read as what it is, and a file that
/// is nothing at all is refused by name rather than misread.
#[test]
fn the_format_comes_from_the_bytes_rather_than_the_name() {
    let dir = std::env::temp_dir().join(format!("compara-sniff-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("a temp dir");
    let csv_named_parquet = dir.join("a.parquet");
    fs::copy(fixture("a.csv"), &csv_named_parquet).expect("a copy");
    let expected = answer(&run(
        &fixture("a.csv"),
        &fixture("b.csv"),
        &mut options(&["id"]),
    ));
    let got = answer(&run(
        &csv_named_parquet,
        &fixture("b.csv"),
        &mut options(&["id"]),
    ));
    assert_eq!(got, expected);

    let truncated = dir.join("truncated.parquet");
    let mut bytes = fs::read(fixture("a_dict_snappy.parquet")).expect("the fixture");
    bytes.truncate(bytes.len() / 2);
    fs::write(&truncated, bytes).expect("the truncated file");
    let error = compare(&truncated, &fixture("b.csv"), &mut options(&["id"]))
        .expect_err("half a Parquet file is not readable");
    assert!(
        format!("{error}").contains("truncated"),
        "the error should say what is wrong with the file: {error}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A typed Parquet value has one rendering, and a CSV of the same data need not
/// use it: `0.50` for the DOUBLE 0.5, `12345` for the DECIMAL 12345.00, a space
/// for the `T` of a timestamp, a zero fraction or a `Z`. Against a typed column
/// those are the same value; before, each was a changed cell, so a CSV compared
/// with its own export to Parquet came out changed on every row.
#[test]
fn a_typed_value_is_compared_by_value_rather_than_by_its_spelling() {
    let dir = std::env::temp_dir().join(format!("compara-typed-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let header = "id,count,big,ratio,flag,day,at,money\n";
    let rows = [
        "K-0000,0,0,0.0,true,1970-01-01,2024-01-01 00:00:00,0",
        "K-0001,1,1,0.50,false,2024-01-01,2024-01-01 12:34:56.000,1.0",
        "K-0002,-1,-1,-0.250,true,2024-02-29,1970-01-01T00:00:00Z,-1",
        "K-0003,42,1099511627776,1.50,true,1969-12-31,2026-09-07 22:15:00,12345",
        "K-0004,1000,-1099511627776,3.0,false,2000-03-01,1999-12-31 23:59:59+00:00,-12345.0",
        "K-0005,-1000,7,1e10,false,1999-12-31,2000-01-01 00:00:01,999999",
        "K-0006,2147483647,8,1.0e-10,true,2038-01-19,2024-06-30 06:30:30.000000,100",
        "K-0007,-2147483648,9,2.250,false,2026-09-07,2024-12-25 18:45:15,-100",
    ];
    let respelled = dir.join("respelled.csv");
    fs::write(&respelled, format!("{header}{}\n", rows.join("\n"))).expect("write");
    let parquet = fixture("typed.parquet");
    for (a, b) in [(&respelled, &parquet), (&parquet, &respelled)] {
        let result = run(a, b, &mut options(&["id"]));
        assert_eq!(
            (result.counts.matched, result.counts.changed),
            (8, 0),
            "{}",
            answer(&result)
        );
    }

    // A real change in a typed column is still one, and --tolerance still
    // applies to the numbers.
    let mut changed = rows.map(String::from);
    changed[7] = "K-0007,-2147483648,9,2.26,false,2026-09-07,2024-12-25 18:45:16,-100.01".into();
    let drifted = dir.join("drifted.csv");
    fs::write(&drifted, format!("{header}{}\n", changed.join("\n"))).expect("write");
    let result = run(&drifted, &parquet, &mut options(&["id"]));
    let per_column: Vec<_> = result
        .columns
        .iter()
        .filter(|c| c.changed > 0)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        (result.counts.changed, per_column),
        (1, vec!["ratio", "at", "money"])
    );
    let mut opt = options(&["id"]);
    opt.tolerance = 0.02;
    let result = run(&drifted, &parquet, &mut opt);
    let per_column: Vec<_> = result
        .columns
        .iter()
        .filter(|c| c.changed > 0)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!((result.counts.changed, per_column), (1, vec!["at"]));

    // Between two CSVs there is no type to go by: the text is the value.
    let result = run(&respelled, &fixture("typed.csv"), &mut options(&["id"]));
    assert_eq!(result.counts.changed, 8, "{}", answer(&result));
    let _ = fs::remove_dir_all(&dir);
}

/// Everything a comparison reports but the run's own facts (which files, how
/// long, which path), so two paths over one pair can be held to each other
/// cell for cell.
fn whole(r: &CompareResult) -> String {
    let mut v = serde_json::to_value(r).expect("the result serialises");
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

/// A Parquet pair whose columns hold numbers, dates, times and booleans is
/// compared a column at a time, one word per value, instead of being turned
/// into rows of text: before, a single such column sent the whole pair to the
/// row-by-row reader. The answer has to be that reader's -- every count, every
/// changed cell as it reads, every added, removed and duplicated row -- under
/// each option that changes how a value is compared.
#[test]
fn a_parquet_pair_of_word_columns_is_compared_without_making_rows() {
    type Tweak = fn(&mut Options);
    let tweaks: [(&str, Tweak); 6] = [
        ("defaults", |_| {}),
        ("--trim", |o| o.trim = true),
        ("--ignore-case", |o| o.ignore_case = true),
        ("--empty-is-null", |o| o.empty_is_null = true),
        ("--tolerance 0.3", |o| o.tolerance = 0.3),
        ("--max-rows 3", |o| o.max_rows = 3),
    ];
    let columnar = |a: &str, b: &str, tweak: Tweak| {
        let mut opt = options(&["id"]);
        opt.engine = "auto".to_string();
        tweak(&mut opt);
        run(&fixture(a), &fixture(b), &mut opt)
    };
    for (a, b) in [
        ("words_a.parquet", "words_b.parquet"),
        ("words_a_plain.parquet", "words_b_plain.parquet"),
        ("words_a_zstd.parquet", "words_b_zstd.parquet"),
    ] {
        for (label, tweak) in tweaks {
            let got = columnar(a, b, tweak);
            assert_eq!(
                got.meta.engine, "parquet",
                "{a} {b} {label}: not the columnar path"
            );
            let mut rows = options(&["id"]);
            tweak(&mut rows);
            let want = run(&fixture(a), &fixture(b), &mut rows);
            assert_eq!(whole(&got), whole(&want), "{a} against {b}, {label}");
        }
    }

    // The fixture's drift, so the agreement above is about something: every
    // column changes somewhere, and B's -0.0 and NaN are A's 0.0 and NaN.
    let r = columnar("words_a.parquet", "words_b.parquet", |_| {});
    assert_eq!(
        (r.counts.added, r.counts.removed, r.counts.b_dup_keys),
        (1, 1, 1)
    );
    for c in &r.columns {
        assert!(c.changed > 0, "{} never changes", c.name);
    }
    let changed = |name: &str| r.columns.iter().find(|c| c.name == name).unwrap().changed;
    assert_eq!((changed("ratio"), changed("single")), (30, 7));

    // A column the two files store as different types is not compared word for
    // word: `small` is INT64 in one, `money` an INT64 decimal against a
    // fixed-length one in the other. Those pairs read row by row, and the
    // answer is the same data's.
    let want = whole(&r);
    for b in ["words_b_wider.parquet", "words_b_plain.parquet"] {
        let got = columnar("words_a.parquet", b, |_| {});
        assert_eq!(got.meta.engine, "turbo", "{b}");
        assert_eq!(whole(&got), want, "{b}");
    }
}

/// `--memory` on two Parquet files decides how many columns are read at once,
/// and a budget too small for the key columns, their index and one column of
/// each file is refused before anything is read. The columnar path used to
/// take any budget and ignore it.
#[test]
fn a_parquet_pair_keeps_to_a_stated_memory_budget() {
    let (a, b) = (fixture("words_a.parquet"), fixture("words_b.parquet"));
    let mut opt = options(&["id"]);
    opt.engine = "auto".to_string();
    let want = whole(&run(&a, &b, &mut opt.clone()));

    let mut tight = opt.clone();
    tight.memory = Some(4 << 10);
    let err = compare(&a, &b, &mut tight).expect_err("a 4 KB budget is refused");
    assert!(
        err.to_string().contains("more than the 0 MB budget"),
        "{err}"
    );

    let mut enough = opt.clone();
    enough.memory = Some(64 << 20);
    let got = run(&a, &b, &mut enough);
    assert_eq!(got.meta.engine, "parquet");
    assert_eq!(whole(&got), want);
}

/// The three files `--export-dir` writes, as they read back.
fn exported(dir: &Path) -> [String; 3] {
    ["added.csv", "removed.csv", "changed.csv"].map(|name| {
        fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{}: {e}", dir.join(name).display()))
    })
}

/// Writes a Parquet pair from rows of text: dictionary pages, or with `plain`
/// plain ones.
fn parquet_pair(
    dir: &Path,
    stem: &str,
    names: &[&str],
    a: &[Vec<&str>],
    b: &[Vec<&str>],
    plain: bool,
) -> (PathBuf, PathBuf) {
    use compara::gendata::parquet::{Compression, Writer};
    let write = |path: &Path, rows: &[Vec<&str>]| {
        let mut w = Writer::create(path, names, Compression::None, 1 << 20).expect("a writer");
        if plain {
            w = w.with_dictionary_limit(0);
        }
        for row in rows {
            w.write_row(row.iter().copied()).expect("a row");
        }
        w.finish().expect("the footer");
    };
    let (pa, pb) = (
        dir.join(format!("{stem}_a.parquet")),
        dir.join(format!("{stem}_b.parquet")),
    );
    write(&pa, a);
    write(&pb, b);
    (pa, pb)
}

/// Values no generator writes on its own: a separator, quotes, a line break
/// and a carriage return inside a value, empty and padded values, letters in
/// both cases and outside ASCII, a key that is empty in one of its columns and
/// one empty in both, and a repeated key.
fn awkward_parquet(dir: &Path, plain: bool) -> (PathBuf, PathBuf) {
    let a: Vec<Vec<&str>> = vec![
        vec!["1", "x", "a,b", "say \"hi\""],
        vec!["2", "x", "line\nbreak", "  padded "],
        vec!["3", "", "MiXeD", "ünïcödé"],
        vec!["4", "y", "1.0", ""],
        vec!["5", "y", "gone", "r\rr"],
        vec!["2", "x", "a repeat", "is not compared"],
    ];
    let b: Vec<Vec<&str>> = vec![
        vec!["", "", "an empty key", "sorts last"],
        vec!["6", "z", "new", "comma, \"quote\""],
        vec!["4", "y", "1", ""],
        vec!["3", "", "mixed", "ünïcödé"],
        vec!["2", "x", "line\nbreak", " padded "],
        vec!["1", "x", "a;b", "say \"hi\""],
    ];
    let stem = if plain {
        "awkward_plain"
    } else {
        "awkward_dict"
    };
    parquet_pair(dir, stem, &["k1", "k2", "v", "w"], &a, &b, plain)
}

/// `--export-dir` on two Parquet files is written by the columnar path rather
/// than refused, and the files are the row reader's, byte for byte -- every
/// row, in key order, quoted the same way -- under each option that changes
/// how a value reads or compares. Asking for an export does not change the
/// report either.
#[test]
fn a_parquet_pair_exports_what_the_row_reader_exports() {
    type Tweak = fn(&mut Options);
    let tweaks: [(&str, Tweak); 6] = [
        ("defaults", |_| {}),
        ("--trim", |o| o.trim = true),
        ("--ignore-case", |o| o.ignore_case = true),
        ("--empty-is-null", |o| o.empty_is_null = true),
        ("--tolerance 0.3", |o| o.tolerance = 0.3),
        ("--max-rows 1", |o| o.max_rows = 1),
    ];
    let dir = std::env::temp_dir().join(format!("compara-pq-export-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a scratch directory");
    let mut pairs: Vec<(PathBuf, PathBuf, &[&str])> = vec![
        (
            fixture("words_a.parquet"),
            fixture("words_b.parquet"),
            &["id"],
        ),
        (
            fixture("words_a_plain.parquet"),
            fixture("words_b_plain.parquet"),
            &["id"],
        ),
        (
            fixture("a_dict_snappy.parquet"),
            fixture("b_dict_snappy.parquet"),
            &["id"],
        ),
    ];
    for plain in [false, true] {
        let (a, b) = awkward_parquet(&dir, plain);
        pairs.push((a, b, &["k1", "k2"]));
    }
    let (columnar_dir, rows_dir) = (dir.join("columnar"), dir.join("rows"));
    for (a, b, key) in &pairs {
        for (label, tweak) in tweaks {
            let what = format!("{} against {}, {label}", a.display(), b.display());
            let mut opt = options(key);
            opt.engine = "auto".to_string();
            tweak(&mut opt);
            let report = run(a, b, &mut opt.clone());
            opt.export_dir = Some(columnar_dir.to_string_lossy().into_owned());
            let got = run(a, b, &mut opt);
            assert_eq!(got.meta.engine, "parquet", "{what}: not the columnar path");
            assert_eq!(
                whole(&got),
                whole(&report),
                "{what}: the export changed the report"
            );

            let mut rows = options(key);
            tweak(&mut rows);
            rows.export_dir = Some(rows_dir.to_string_lossy().into_owned());
            run(a, b, &mut rows);
            assert_eq!(exported(&columnar_dir), exported(&rows_dir), "{what}");
        }
    }

    // What the awkward pair's agreement is about: the values come out quoted
    // where they have to be, and the key that is empty in both columns sorts
    // after every key that is not.
    let (a, b) = awkward_parquet(&dir, true);
    let mut opt = options(&["k1", "k2"]);
    opt.engine = "auto".to_string();
    opt.export_dir = Some(columnar_dir.to_string_lossy().into_owned());
    run(&a, &b, &mut opt);
    let [added, removed, changed] = exported(&columnar_dir);
    assert_eq!(
        added,
        "k1,k2,v,w\n6,z,new,\"comma, \"\"quote\"\"\"\n,,an empty key,sorts last\n"
    );
    assert_eq!(removed, "k1,k2,v,w\n5,y,gone,\"r\rr\"\n");
    assert!(
        changed.contains("\"a,b\",a;b,\"say \"\"hi\"\"\",\"say \"\"hi\"\"\""),
        "{changed}"
    );
    assert!(
        changed.contains("\"line\nbreak\",\"line\nbreak\",  padded , padded "),
        "{changed}"
    );
    assert!(
        changed.contains("3,,MiXeD,mixed,ünïcödé,ünïcödé"),
        "{changed}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// An export of more rows than one encoded block, on several lanes: the blocks
/// are written in order whatever order they finish in, and the rows -- stored
/// out of key order, every one of them changed -- come out sorted.
#[test]
fn a_large_parquet_export_is_written_in_key_order() {
    let dir = std::env::temp_dir().join(format!("compara-pq-export-big-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a scratch directory");
    let n = 20_000;
    let keys: Vec<String> = (0..n).map(|i| format!("K-{:05}", (i * 7919) % n)).collect();
    let vals: Vec<(String, String)> = (0..n).map(|i| (format!("a{i}"), format!("b{i}"))).collect();
    let a: Vec<Vec<&str>> = (0..n)
        .map(|i| vec![keys[i].as_str(), vals[i].0.as_str(), "same"])
        .collect();
    let b: Vec<Vec<&str>> = (0..n)
        .rev()
        .map(|i| vec![keys[i].as_str(), vals[i].1.as_str(), "same"])
        .collect();
    let (pa, pb) = parquet_pair(&dir, "big", &["id", "v", "w"], &a, &b, false);

    let rows_dir = dir.join("rows");
    let mut rows = options(&["id"]);
    rows.export_dir = Some(rows_dir.to_string_lossy().into_owned());
    run(&pa, &pb, &mut rows);
    let want = exported(&rows_dir);
    assert_eq!(want[2].lines().count(), n + 1, "every row is changed");
    assert!(want[2].lines().nth(1).unwrap().starts_with("K-00000,"));

    for threads in [1, 2, 3] {
        let to = dir.join(format!("columnar-{threads}"));
        let mut opt = options(&["id"]);
        opt.engine = "auto".to_string();
        opt.threads = Some(threads);
        opt.export_dir = Some(to.to_string_lossy().into_owned());
        let got = run(&pa, &pb, &mut opt);
        assert_eq!(got.meta.engine, "parquet");
        assert_eq!(exported(&to), want, "{threads} threads");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A UTF-8 byte order mark (what Excel and many Windows tools write first) is
/// not part of the first column's name, in any engine that reads the file. It
/// was, in the byte-level engine: the key `id` became `\u{feff}id` and the
/// comparison failed with "key column(s) missing".
#[test]
fn a_byte_order_mark_is_not_part_of_the_first_column() {
    let dir = std::env::temp_dir().join(format!("compara-bom-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let with_bom = |name: &str| {
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(fs::read(fixture(name)).expect("fixture"));
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write");
        path
    };
    let (a_bom, b_bom, a_json_bom) = (with_bom("a.csv"), with_bom("b.csv"), with_bom("a.ndjson"));
    let (a, b) = (fixture("a.csv"), fixture("b.csv"));
    for engine in ["turbo", "sortmerge", "native"] {
        let mut opt = options(&["id"]);
        opt.engine = engine.to_string();
        let expected = answer(&run(&a, &b, &mut opt.clone()));
        assert_eq!(
            answer(&run(&a_bom, &b, &mut opt.clone())),
            expected,
            "{engine}: BOM on A"
        );
        assert_eq!(
            answer(&run(&a, &b_bom, &mut opt.clone())),
            expected,
            "{engine}: BOM on B"
        );
        assert_eq!(
            answer(&run(&a_bom, &b_bom, &mut opt)),
            expected,
            "{engine}: BOM on both"
        );
    }
    let expected = answer(&run(&fixture("a.ndjson"), &b, &mut options(&["id"])));
    assert_eq!(
        answer(&run(&a_json_bom, &b, &mut options(&["id"]))),
        expected,
        "BOM before JSON"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// sortmerge and native read CSV only. Named on a Parquet or JSON input they
/// are refused; before, a Parquet pair quietly took the columnar path and
/// reported it, as if the named engine had been an option it could ignore.
#[test]
fn a_csv_only_engine_named_for_parquet_or_json_is_refused() {
    let pairs = [
        ("a_plain_none.parquet", "b_dict_snappy.parquet"),
        ("a_plain_none.parquet", "b.csv"),
        ("a.ndjson", "b.csv"),
        ("a.csv", "b.ndjson"),
    ];
    for engine in ["sortmerge", "native"] {
        for (a, b) in pairs {
            let mut opt = options(&["id"]);
            opt.engine = engine.to_string();
            let err = compare(&fixture(a), &fixture(b), &mut opt).expect_err("refused");
            assert!(
                err.to_string().contains("reads CSV only"),
                "{engine} {a} {b}: {err}"
            );
        }
    }
    for engine in ["auto", "turbo"] {
        for (a, b) in pairs {
            let mut opt = options(&["id"]);
            opt.engine = engine.to_string();
            run(&fixture(a), &fixture(b), &mut opt);
        }
    }
}

/// A nested object or array is a cell of its JSON text, in newline-delimited
/// JSON and in a top-level array alike. Before, it was read as absent, so two
/// different objects compared as the same missing value and the change went
/// unreported. The text is compared as written: the same object with another
/// key order or spacing is a change, which is what the README says.
#[test]
fn a_nested_json_value_is_compared_as_its_text() {
    let dir = std::env::temp_dir().join(format!("compara-nested-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("a temp dir");
    let a = dir.join("a.ndjson");
    let b = dir.join("b.ndjson");
    let b_array = dir.join("b.json");
    fs::write(
        &a,
        concat!(
            "{\"id\":\"1\",\"meta\":{\"a\":1,\"b\":[1,2]},\"tags\":[],\"v\":\"x\"}\n",
            "{\"id\":\"2\",\"meta\":{\"a\":2},\"tags\":[\"p\",\"q\"],\"v\":\"y\"}\n",
            "{\"id\":\"3\",\"meta\":{\"a\":3,\"b\":\"}\"},\"v\":\"z\"}\n",
            "{\"id\":\"4\",\"meta\":{\"a\":4,\"b\":5},\"v\":\"w\"}\n",
        ),
    )
    .unwrap();
    let rows = [
        // changed: one element of a nested array
        "{\"id\":\"1\",\"meta\":{\"a\":1,\"b\":[1,3]},\"tags\":[],\"v\":\"x\"}",
        // unchanged, nested object and array alike
        "{\"id\":\"2\",\"meta\":{\"a\":2},\"tags\":[\"p\",\"q\"],\"v\":\"y\"}",
        // unchanged: a brace inside a string is text, not structure
        "{\"id\":\"3\",\"meta\":{\"a\":3,\"b\":\"}\"},\"v\":\"z\"}",
        // changed: the same object, keys in another order
        "{\"id\":\"4\",\"meta\":{\"b\":5,\"a\":4},\"v\":\"w\"}",
    ];
    fs::write(&b, rows.join("\n") + "\n").unwrap();
    fs::write(&b_array, format!("[\n  {}\n]\n", rows.join(",\n  "))).unwrap();

    for other in [&b, &b_array] {
        let result = run(&a, other, &mut options(&["id"]));
        assert_eq!(result.counts.matched, 4, "{}", answer(&result));
        assert_eq!(result.counts.changed, 2, "{}", answer(&result));
        let meta = result.columns.iter().find(|c| c.name == "meta").unwrap();
        assert_eq!(meta.changed, 2, "{}", answer(&result));
        let tags = result.columns.iter().find(|c| c.name == "tags").unwrap();
        assert_eq!(tags.changed, 0, "{}", answer(&result));
        let shown = whole(&result);
        assert!(shown.contains(r#"{\"a\":1,\"b\":[1,2]}"#), "{shown}");
        assert!(shown.contains(r#"{\"a\":1,\"b\":[1,3]}"#), "{shown}");
    }
    fs::remove_dir_all(&dir).ok();
}
