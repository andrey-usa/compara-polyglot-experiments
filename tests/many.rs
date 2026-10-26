//! A side made of several files -- a directory, or a glob -- is read as the
//! files one after another: the answer is the one the concatenated file gives,
//! in every format, and the files that cannot be one table are refused with
//! the file and the difference named.

use std::path::{Path, PathBuf};
use std::process::Command;

use compara::gendata::parquet::{Compression, Writer};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Dir {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "compara-many-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

fn generate(dir: &Path, prefix: &str, rows: &str, seed: &str, extra: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_compara-gen-data"))
        .args([
            "--rows",
            rows,
            "--seed",
            seed,
            "--prefix",
            prefix,
            "--out-dir",
        ])
        .arg(dir)
        .args(extra)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

/// Exit code, stderr, and the summary without its run facts.
fn compare(a: &str, b: &str, args: &[&str]) -> (i32, String, String) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let json = std::env::temp_dir().join(format!(
        "compara-many-summary-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&json);
    let out = Command::new(env!("CARGO_BIN_EXE_compara"))
        .args(["compare", a, b])
        .args(args)
        .arg("--json")
        .arg(&json)
        .output()
        .unwrap();
    let summary = std::fs::read_to_string(&json)
        .ok()
        .map(|t| {
            let mut v: serde_json::Value = serde_json::from_str(&t).unwrap();
            v.as_object_mut().unwrap().remove("meta");
            v.to_string()
        })
        .unwrap_or_default();
    let _ = std::fs::remove_file(&json);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        summary,
    )
}

const KEY: [&str; 4] = ["-k", "account_id,txn_id", "-i", "updated_at"];

/// The text of `files` as one CSV: the first header, every row.
fn concatenated(files: &[PathBuf], to: &Path) {
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        let text = std::fs::read_to_string(f).unwrap();
        let body = if i == 0 {
            &text[..]
        } else {
            &text[text.find('\n').unwrap() + 1..]
        };
        out.push_str(body);
    }
    std::fs::write(to, out).unwrap();
}

/// Two seeds, so the parts repeat keys across files: which occurrence comes
/// first depends on the order the parts are read in, and is checked here.
#[test]
fn a_directory_or_a_glob_is_its_files_in_path_order() {
    let d = scratch("order");
    let gen_dir = d.0.join("gen");
    for (p, rows, seed) in [("p", "20k", "1"), ("q", "7k", "2"), ("r", "3k", "3")] {
        for fmt in ["csv", "parquet", "ndjson"] {
            let extra: &[&str] = match fmt {
                "parquet" => &[
                    "--format",
                    "parquet",
                    "--compression",
                    "zstd",
                    "--row-group-size",
                    "1000",
                ],
                "ndjson" => &["--format", "ndjson"],
                _ => &[],
            };
            generate(&gen_dir, p, rows, seed, extra);
        }
    }
    // A/<date>/part-N.<fmt> (not `date=`: no partition column here), the
    // parts out of name order on disk, a Spark marker and a checksum file
    // beside them.
    for side in ["a", "b"] {
        for fmt in ["csv", "parquet", "ndjson"] {
            let root = d.0.join(format!("{fmt}-{side}"));
            for (dir, p) in [
                ("2026-10-02", "q"),
                ("2026-10-01", "p"),
                ("2026-10-03", "r"),
            ] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
                std::fs::copy(
                    gen_dir.join(format!("{p}_{side}.{fmt}")),
                    root.join(dir).join(format!("part-0.{fmt}")),
                )
                .unwrap();
            }
            std::fs::write(root.join("_SUCCESS"), "").unwrap();
            std::fs::write(root.join("2026-10-01").join(".part-0.crc"), "junk").unwrap();
        }
        let parts: Vec<PathBuf> = ["p", "q", "r"]
            .iter()
            .map(|p| gen_dir.join(format!("{p}_{side}.csv")))
            .collect();
        concatenated(&parts, &d.0.join(format!("all_{side}.csv")));
    }
    let path = |s: String| d.0.join(s).to_string_lossy().into_owned();
    let want = compare(&path("all_a.csv".into()), &path("all_b.csv".into()), &KEY);
    assert_eq!(want.0, 1, "{}", want.1);
    for fmt in ["csv", "parquet", "ndjson"] {
        let got = compare(&path(format!("{fmt}-a")), &path(format!("{fmt}-b")), &KEY);
        assert_eq!((got.0, &got.2), (want.0, &want.2), "{fmt}: {}", got.1);
        assert!(got.1.contains("3 "), "{fmt}: {}", got.1);
    }
    // Globs, a different format on each side, `**` and a class.
    let got = compare(
        &path("parquet-a/2026-10-0[1-3]/*.parquet".into()),
        &path("ndjson-b/**/part-?.ndjson".into()),
        &KEY,
    );
    assert_eq!((got.0, &got.2), (want.0, &want.2), "glob: {}", got.1);
    // The parquet engine and turbo read the joined Parquet file alike.
    let got = compare(
        &path("parquet-a".into()),
        &path("parquet-b".into()),
        &[&KEY[..], &["--engine", "turbo"]].concat(),
    );
    assert_eq!((got.0, &got.2), (want.0, &want.2), "turbo: {}", got.1);
    // `columns` and `head` read a directory too.
    let out = Command::new(env!("CARGO_BIN_EXE_compara"))
        .args(["columns", &path("parquet-a".into())])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("account_id"),
        "{out:?}"
    );
}

/// Compressed parts, parts without a last newline, CRLF and a byte-order
/// mark: one table all the same.
#[test]
fn compressed_and_untidy_parts_join_cleanly() {
    let d = scratch("untidy");
    let side = d.0.join("a");
    std::fs::create_dir_all(&side).unwrap();
    std::fs::write(side.join("1.csv"), "id,v\r\n1,a\r\n2,b").unwrap();
    std::fs::write(side.join("2.csv"), "\u{feff}id,v\n3,c").unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, b"id,v\n4,d\n5,e\n").unwrap();
    std::fs::write(side.join("3.csv.gz"), gz.finish().unwrap()).unwrap();
    std::fs::write(
        side.join("4.csv.zst"),
        zstd::encode_all(&b"id,v\n6,f\n"[..], 3).unwrap(),
    )
    .unwrap();
    std::fs::write(side.join("5.csv"), "id,v\n").unwrap(); // a header and nothing else
    std::fs::write(d.0.join("b.csv"), "id,v\n1,a\n2,b\n3,c\n4,d\n5,e\n6,f\n").unwrap();
    let got = compare(
        &side.to_string_lossy(),
        &d.0.join("b.csv").to_string_lossy(),
        &["-k", "id"],
    );
    assert_eq!(got.0, 0, "{}", got.1);
    assert!(got.2.contains("\"a_rows\":6"), "{}", got.2);
}

#[test]
fn parts_that_are_not_one_table_are_refused_by_name() {
    let d = scratch("refused");
    let w = |p: &str, text: &str| {
        let path = d.0.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    w("one.csv", "id,v\n1,a\n");
    w("header/1.csv", "id,v\n1,a\n");
    w("header/2.csv", "id,w\n2,b\n");
    w("mixed/1.csv", "id,v\n1,a\n");
    w("mixed/2.ndjson", "{\"id\":2,\"v\":\"b\"}\n");
    // A Delta table inside the directory named, not the directory itself.
    w("lake/orders/part-0.parquet", "x");
    w("lake/orders/_delta_log/00000000000000000000.json", "{}");
    w("iceberg/data/0.parquet", "x");
    w("iceberg/metadata/v1.metadata.json", "{}");
    std::fs::create_dir_all(d.0.join("empty")).unwrap();
    w("empty/_SUCCESS", "");
    let one = d.0.join("one.csv").to_string_lossy().into_owned();
    for (side, says) in [
        ("header", "has the header `id,w` where"),
        ("mixed", "must be of one format"),
        ("lake", "holds a Delta table at orders/"),
        ("iceberg", "is an Iceberg table"),
        ("empty", "no files in"),
        ("nothing/*.csv", "no files match"),
    ] {
        let got = compare(&d.0.join(side).to_string_lossy(), &one, &["-k", "id"]);
        assert_eq!(got.0, 2, "{side}: {}", got.1);
        assert!(got.1.contains(says), "{side}: {}", got.1);
    }

    // Parquet parts must agree on their columns, nullability and types; the
    // error names the column.
    let pq = |p: &str, names: &[&str], rows: &[&[&str]]| {
        let path = d.0.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut w = Writer::create(&path, names, Compression::None, 100).unwrap();
        for r in rows {
            w.write_row(r.iter().copied()).unwrap();
        }
        w.finish().unwrap();
    };
    pq("pq/1.parquet", &["id", "v"], &[&["1", "a"]]);
    pq("pq/2.parquet", &["id", "w"], &[&["2", "b"]]);
    let got = compare(&d.0.join("pq").to_string_lossy(), &one, &["-k", "id"]);
    assert_eq!(got.0, 2, "{}", got.1);
    assert!(got.1.contains("has w instead of v"), "{}", got.1);
    pq("same/1.parquet", &["id", "v"], &[&["1", "a"], &["2", "b"]]);
    pq("same/2.parquet", &["id", "v"], &[&["3", "c"]]);
    let got = compare(&d.0.join("same").to_string_lossy(), &one, &["-k", "id"]);
    assert_eq!(got.0, 1, "{}", got.1);
    assert!(got.2.contains("\"a_rows\":3"), "{}", got.2);
}

/// `name=value` directories are columns: keyed on one, rows that repeat a key
/// in different partitions are different rows, and the answer is the one a
/// flat file holding the column gives -- for Parquet, CSV and JSON parts,
/// with either Parquet reader.
#[test]
fn partition_directories_are_columns() {
    let d = scratch("partitions");
    let gen_dir = d.0.join("gen");
    for (p, rows, seed) in [("p", "20k", "1"), ("q", "8k", "2")] {
        for extra in [
            &[][..],
            &["--format", "parquet"][..],
            &["--format", "ndjson"][..],
        ] {
            generate(&gen_dir, p, rows, seed, extra);
        }
    }
    for side in ["a", "b"] {
        for fmt in ["csv", "parquet", "ndjson"] {
            for (date, p) in [("2026-10-01", "p"), ("2026-10-02", "q")] {
                let dir =
                    d.0.join(format!("{fmt}-{side}/date={date}/region=eu%2Fwest"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::copy(
                    gen_dir.join(format!("{p}_{side}.{fmt}")),
                    dir.join(format!("part-0.{fmt}")),
                )
                .unwrap();
            }
        }
        // The flat equivalent: every row with its date and region.
        let mut flat = String::new();
        for (date, p) in [("2026-10-01", "p"), ("2026-10-02", "q")] {
            let text = std::fs::read_to_string(gen_dir.join(format!("{p}_{side}.csv"))).unwrap();
            let mut lines = text.lines();
            let header = lines.next().unwrap();
            if flat.is_empty() {
                flat.push_str(&format!("{header},date,region\n"));
            }
            for l in lines {
                flat.push_str(&format!("{l},{date},eu/west\n"));
            }
        }
        std::fs::write(d.0.join(format!("flat_{side}.csv")), flat).unwrap();
    }
    let path = |s: &str| d.0.join(s).to_string_lossy().into_owned();
    let key = ["-k", "date,region,account_id,txn_id", "-i", "updated_at"];
    let want = compare(&path("flat_a.csv"), &path("flat_b.csv"), &key);
    assert_eq!(want.0, 1, "{}", want.1);
    assert!(want.2.contains("\"a_dup_keys\":"), "{}", want.2);
    for (a, b, extra) in [
        ("csv-a", "csv-b", &[][..]),
        ("parquet-a", "parquet-b", &[][..]),
        ("parquet-a", "parquet-b", &["--engine", "turbo"][..]),
        ("ndjson-a", "ndjson-b", &[][..]),
        ("parquet-a", "flat_b.csv", &[][..]),
        ("ndjson-a", "csv-b", &[][..]),
    ] {
        let args = [&key[..], extra].concat();
        let got = compare(&path(a), &path(b), &args);
        assert_eq!(
            (got.0, &got.2),
            (want.0, &want.2),
            "{a} {b} {extra:?}: {}",
            got.1
        );
        assert!(
            got.1.contains("partitioned by date, region"),
            "{a}: {}",
            got.1
        );
    }
}

/// `--exit-zero`: differences are reported and the process still succeeds,
/// for a scheduled job; an error is still an error.
#[test]
fn exit_zero_reports_differences_without_failing() {
    let d = scratch("exit-zero");
    std::fs::write(d.0.join("a.csv"), "id,v\n1,a\n2,b\n").unwrap();
    std::fs::write(d.0.join("b.csv"), "id,v\n1,a\n2,c\n").unwrap();
    let a = d.0.join("a.csv").to_string_lossy().into_owned();
    let b = d.0.join("b.csv").to_string_lossy().into_owned();
    let got = compare(&a, &b, &["-k", "id"]);
    assert_eq!(got.0, 1, "{}", got.1);
    let got = compare(&a, &b, &["-k", "id", "--exit-zero"]);
    assert_eq!(got.0, 0, "{}", got.1);
    assert!(got.2.contains("\"changed\":1"), "{}", got.2);
    let got = compare(&a, &b, &["-k", "nope", "--exit-zero"]);
    assert_eq!(got.0, 2, "{}", got.1);
}

/// A Delta table is the files its log says, at the latest version or at
/// `@vN`: a version compares like the directory of just its files, and what
/// the log cannot vouch for is refused.
#[test]
fn delta_tables_are_read_at_a_version() {
    let d = scratch("delta");
    let gen_dir = d.0.join("gen");
    for (p, rows, seed) in [("p", "20k", "1"), ("q", "8k", "2"), ("r", "5k", "3")] {
        generate(&gen_dir, p, rows, seed, &["--format", "parquet"]);
    }
    let t = d.0.join("t");
    let place = |root: &Path, date: &str, file: &str, p: &str| {
        let dir = root.join(format!("date={date}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(gen_dir.join(format!("{p}_a.parquet")), dir.join(file)).unwrap();
    };
    place(&t, "2026-10-01", "part-0.parquet", "p");
    place(&t, "2026-10-02", "part-0.parquet", "q");
    place(&t, "2026-10-03", "part 0.parquet", "r");
    // Version 0 adds days 1 and 2; version 1 removes day 2 and adds day 3.
    let log = t.join("_delta_log");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        log.join("00000000000000000000.json"),
        concat!(
            "{\"protocol\":{\"minReaderVersion\":1,\"minWriterVersion\":2}}\n",
            "{\"metaData\":{\"id\":\"t\",\"partitionColumns\":[\"date\"],\"configuration\":{}}}\n",
            "{\"add\":{\"path\":\"date=2026-10-01/part-0.parquet\",\"size\":1,\"dataChange\":true}}\n",
            "{\"add\":{\"path\":\"date=2026-10-02/part-0.parquet\",\"size\":1,\"dataChange\":true}}\n",
        ),
    )
    .unwrap();
    std::fs::write(
        log.join("00000000000000000001.json"),
        concat!(
            "{\"remove\":{\"path\":\"date=2026-10-02/part-0.parquet\",\"dataChange\":true}}\n",
            "{\"add\":{\"path\":\"date=2026-10-03/part%200.parquet\",\"size\":1,\"dataChange\":true}}\n",
        ),
    )
    .unwrap();
    // The same versions as plain directories.
    let v0 = d.0.join("v0");
    place(&v0, "2026-10-01", "part-0.parquet", "p");
    place(&v0, "2026-10-02", "part-0.parquet", "q");
    let v1 = d.0.join("v1");
    place(&v1, "2026-10-01", "part-0.parquet", "p");
    place(&v1, "2026-10-03", "part 0.parquet", "r");
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let key = ["-k", "date,account_id,txn_id"];
    let t_s = path(&t);
    for (a, b) in [
        (format!("{t_s}@v0"), path(&v0)),
        (t_s.clone(), path(&v1)),
        (format!("{t_s}@v1"), path(&v1)),
    ] {
        let got = compare(&a, &b, &key);
        assert_eq!(got.0, 0, "{a} vs {b}: {}", got.1);
    }
    let got = compare(&format!("{t_s}@v0"), &t_s, &key);
    assert_eq!(got.0, 1, "{}", got.1);
    assert!(
        got.2.contains("\"added\":5000") && got.2.contains("\"removed\":8000"),
        "{}",
        got.2
    );
    assert!(
        got.1.contains("Delta version 0") && got.1.contains("Delta version 1"),
        "{}",
        got.1
    );
    // A version that is not there, and a file the log names that is gone.
    let got = compare(&format!("{t_s}@v7"), &t_s, &key);
    assert!(got.0 == 2 && got.1.contains("latest is 1"), "{}", got.1);
    std::fs::remove_file(t.join("date=2026-10-02/part-0.parquet")).unwrap();
    let got = compare(&format!("{t_s}@v0"), &t_s, &key);
    assert!(got.0 == 2 && got.1.contains("no longer there"), "{}", got.1);
    // Deletion vectors are refused, not read as if the rows were there.
    std::fs::write(
        log.join("00000000000000000002.json"),
        "{\"add\":{\"path\":\"date=2026-10-01/part-0.parquet\",\"deletionVector\":{\"storageType\":\"u\",\"pathOrInlineDv\":\"x\",\"sizeInBytes\":1,\"cardinality\":1}}}\n",
    )
    .unwrap();
    let got = compare(&t_s, &path(&v1), &key);
    assert!(
        got.0 == 2 && got.1.contains("deletion vectors"),
        "{}",
        got.1
    );
}
