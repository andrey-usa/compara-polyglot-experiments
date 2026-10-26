//! Remote inputs: the same pair read from a local stand-in for object storage
//! (`tests/object_server.py`) must give the answer the local files give --
//! counts, column statistics and every exported row -- over S3 and plain HTTP,
//! gzip and zstd, Parquet fetched a column at a time, and a server that does
//! not do ranges. Skipped where there is no Python to run the server.
#![cfg(feature = "cloud")]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

struct Server {
    child: Child,
    url: String,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(root: &Path, extra: &[&str]) -> Option<Server> {
        let py = python()?;
        let log = root.join(format!("requests-{}.jsonl", id()));
        let mut child = Command::new(py)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/object_server.py"
            ))
            .arg("--root")
            .arg(root)
            .arg("--log")
            .arg(&log)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let url = line.trim().strip_prefix("LISTENING ")?.to_string();
        Some(Server { child, url, log })
    }

    /// The bytes the server has sent, from its request log.
    fn sent(&self) -> u64 {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .map(|v| v["sent"].as_u64().unwrap_or(0))
            .sum()
    }
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(rows: &str, flags: &[&str]) -> Dir {
    let dir = std::env::temp_dir().join(format!("compara-remote-{}-{}", std::process::id(), id()));
    let bucket = dir.join("bkt");
    std::fs::create_dir_all(&bucket).unwrap();
    for f in [&[][..], flags] {
        let out = Command::new(env!("CARGO_BIN_EXE_compara-gen-data"))
            .args(["--rows", rows, "--out-dir"])
            .arg(&bucket)
            .args(["--prefix", "t"])
            .args(f)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    Dir(dir)
}

/// Runs a comparison and returns (exit code, stderr, the summary without its
/// run facts, the exported rows).
fn compare(
    a: &str,
    b: &str,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> (i32, String, String, String) {
    let out_dir = std::env::temp_dir().join(format!(
        "compara-remote-out-{}-{}",
        std::process::id(),
        id()
    ));
    let json = out_dir.join("summary.json");
    let export = out_dir.join("export");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_compara"));
    cmd.args(["compare", a, b])
        .args(args)
        .arg("--json")
        .arg(&json)
        .arg("--export-dir")
        .arg(&export)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(bytes) = stdin {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let code = out.status.code().unwrap_or(-1);
    let mut summary = String::new();
    let mut rows = String::new();
    if let Ok(text) = std::fs::read_to_string(&json) {
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v.as_object_mut().unwrap().remove("meta");
        summary = v.to_string();
        let mut names: Vec<_> = std::fs::read_dir(&export)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        names.sort();
        for n in names {
            rows.push_str(&std::fs::read_to_string(&n).unwrap());
        }
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    (
        code,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        summary,
        rows,
    )
}

fn s3_env(url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("AWS_ENDPOINT_URL", url.to_string()),
        ("AWS_ACCESS_KEY_ID", "test".into()),
        ("AWS_SECRET_ACCESS_KEY", "test".into()),
        ("AWS_REGION", "us-east-1".into()),
    ]
}

fn borrowed<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

/// A case: A, B, the environment, and the local answer it must give.
type Case<'a> = (
    String,
    String,
    Vec<(&'a str, &'a str)>,
    &'a (i32, String, String, String),
);

const KEY: [&str; 4] = ["-k", "account_id,txn_id", "-i", "updated_at"];

/// Parquet in every layout the generator writes, over S3, a column at a time:
/// the answer is the local one, with each reader, and with only some columns.
#[test]
fn parquet_over_s3_is_the_local_answer() {
    for flags in [
        &["--format", "parquet", "--compression", "none"][..],
        &[
            "--format",
            "parquet",
            "--compression",
            "zstd",
            "--row-group-size",
            "512",
        ][..],
        &[
            "--format",
            "parquet",
            "--dict-limit",
            "175",
            "--row-group-size",
            "300",
        ][..],
        &["--format", "parquet", "--dict-limit", "1"][..],
    ] {
        let f = fixture("20k", flags);
        let Some(server) = Server::start(&f.0, &[]) else {
            eprintln!("skipping: no python to run the object server");
            return;
        };
        let env = s3_env(&server.url);
        let local = |n: &str| f.0.join("bkt").join(n).to_string_lossy().into_owned();
        for extra in [
            &[][..],
            &["-c", "amount,note"][..],
            &["--engine", "turbo"][..],
        ] {
            let args: Vec<&str> = KEY.iter().copied().chain(extra.iter().copied()).collect();
            let want = compare(
                &local("t_a.parquet"),
                &local("t_b.parquet"),
                &args,
                &[],
                None,
            );
            let got = compare(
                "s3://bkt/t_a.parquet",
                "s3://bkt/t_b.parquet",
                &args,
                &borrowed(&env),
                None,
            );
            assert_eq!(
                want.0, got.0,
                "{flags:?} {extra:?}: exit code; stderr {}",
                got.1
            );
            assert_eq!(want.2, got.2, "{flags:?} {extra:?}: summary");
            assert_eq!(want.3, got.3, "{flags:?} {extra:?}: exported rows");
            assert!(got.1.contains("downloaded"), "{}", got.1);
        }
        // Two compared columns of twenty: most of the file stays where it is.
        // Small row groups put the wanted chunks closer together than it pays
        // to skip, and are fetched nearly whole by design.
        if flags.contains(&"--row-group-size") {
            continue;
        }
        let before = server.sent();
        let args: Vec<&str> = ["-k", "account_id,txn_id", "-c", "amount"].to_vec();
        compare(
            "s3://bkt/t_a.parquet",
            "s3://bkt/t_b.parquet",
            &args,
            &borrowed(&env),
            None,
        );
        let size = std::fs::metadata(f.0.join("bkt/t_a.parquet"))
            .unwrap()
            .len()
            + std::fs::metadata(f.0.join("bkt/t_b.parquet"))
                .unwrap()
                .len();
        let sent = server.sent() - before;
        assert!(
            sent < size * 3 / 4,
            "{flags:?}: fetched {sent} of {size} bytes for three columns"
        );
    }
}

/// CSV and NDJSON over plain HTTP and S3, compressed and not, from a server
/// that does ranges and one that does not, and from standard input.
#[test]
fn text_over_http_and_s3_is_the_local_answer() {
    let f = fixture("20k", &["--format", "ndjson"]);
    let bucket = f.0.join("bkt");
    let csv_a = std::fs::read(bucket.join("t_a.csv")).unwrap();
    {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&csv_a).unwrap();
        std::fs::write(bucket.join("t_a.csv.gz"), gz.finish().unwrap()).unwrap();
        let nd = std::fs::read(bucket.join("t_b.ndjson")).unwrap();
        std::fs::write(
            bucket.join("t_b.ndjson.zst"),
            zstd::stream::encode_all(&nd[..], 3).unwrap(),
        )
        .unwrap();
    }
    let local = |n: &str| bucket.join(n).to_string_lossy().into_owned();
    let want = compare(&local("t_a.csv"), &local("t_b.csv"), &KEY, &[], None);
    let want_nd = compare(&local("t_a.ndjson"), &local("t_b.ndjson"), &KEY, &[], None);
    assert!(!want.2.is_empty());
    for extra in [&[][..], &["--no-ranges"][..]] {
        let Some(server) = Server::start(&f.0, extra) else {
            eprintln!("skipping: no python to run the object server");
            return;
        };
        let u = &server.url;
        let env = s3_env(u);
        let mut cases: Vec<Case> = vec![
            (
                format!("{u}/bkt/t_a.csv.gz"),
                format!("{u}/bkt/t_b.csv"),
                vec![],
                &want,
            ),
            (
                format!("{u}/bkt/t_a.ndjson"),
                format!("{u}/bkt/t_b.ndjson.zst"),
                vec![],
                &want_nd,
            ),
        ];
        // S3 always serves ranges; a server that does not is a plain HTTP one.
        if extra.is_empty() {
            cases.push((
                "s3://bkt/t_a.csv".into(),
                "s3://bkt/t_b.csv".into(),
                borrowed(&env),
                &want,
            ));
        }
        for (a, b, env, want) in cases {
            let got = compare(&a, &b, &KEY, &env, None);
            assert_eq!(want.0, got.0, "{extra:?} {a} {b}: exit; stderr {}", got.1);
            assert_eq!(want.2, got.2, "{extra:?} {a} {b}: summary");
            assert_eq!(want.3, got.3, "{extra:?} {a} {b}: rows");
            // A server without ranges sends the object whole: it goes to disk
            // as it comes, not into memory.
            if !extra.is_empty() && b.ends_with("t_b.csv") {
                assert!(
                    got.1.contains("whole (the server sends no ranges)"),
                    "{}",
                    got.1
                );
            }
        }
    }
    // Standard input, gzipped, against a local file.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&csv_a).unwrap();
    let got = compare(
        "-",
        &local("t_b.csv"),
        &KEY,
        &[],
        Some(&gz.finish().unwrap()),
    );
    assert_eq!(
        (want.0, &want.2, &want.3),
        (got.0, &got.2, &got.3),
        "stdin: {}",
        got.1
    );
}

/// What goes wrong says what it is: a missing object, a refused one, a
/// compression this does not read.
#[test]
fn remote_failures_name_the_problem() {
    let f = fixture("1k", &[]);
    let Some(server) = Server::start(&f.0, &[]) else {
        return;
    };
    let u = server.url.clone();
    let (code, err, ..) = compare(
        &format!("{u}/bkt/missing.csv"),
        &format!("{u}/bkt/t_b.csv"),
        &KEY,
        &[],
        None,
    );
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("not found") && err.contains("missing.csv"),
        "{err}"
    );
    let env = s3_env(&u);
    let (code, err, ..) = compare(
        "s3://bkt/missing.parquet",
        &format!("{u}/bkt/t_b.csv"),
        &KEY,
        &borrowed(&env),
        None,
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not found"), "{err}");
    drop(server);
    let Some(private) = Server::start(&f.0, &["--private"]) else {
        return;
    };
    let (code, err, ..) = compare(
        &format!("{}/bkt/t_a.csv", private.url),
        &format!("{}/bkt/t_b.csv", private.url),
        &KEY,
        &[],
        None,
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("403"), "{err}");
}

/// `columns` on a remote Parquet object reads its footer and nothing else.
#[test]
fn columns_of_a_remote_parquet_file_read_only_the_footer() {
    let f = fixture("20k", &["--format", "parquet"]);
    let Some(server) = Server::start(&f.0, &[]) else {
        return;
    };
    let out = Command::new(env!("CARGO_BIN_EXE_compara"))
        .args(["columns", &format!("{}/bkt/t_a.parquet", server.url)])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let names = String::from_utf8_lossy(&out.stdout);
    assert!(names.lines().any(|l| l == "txn_id"), "{names}");
    assert!(
        server.sent() < 70_000,
        "columns fetched {} bytes",
        server.sent()
    );
}

/// Outputs to object storage: the report, the summary and the export land in
/// the bucket as they would on disk, in one request or as a multipart upload;
/// `--json -` is the summary on standard output.
#[test]
fn outputs_reach_object_storage_and_stdout() {
    let f = fixture("20k", &[]);
    let Some(server) = Server::start(&f.0, &[]) else {
        return;
    };
    let bucket = f.0.join("bkt");
    let local = |n: &str| bucket.join(n).to_string_lossy().into_owned();
    let want = compare(&local("t_a.csv"), &local("t_b.csv"), &KEY, &[], None);
    for (one_shot, part) in [("", ""), ("1000", "200000")] {
        let mut env = s3_env(&server.url);
        if !one_shot.is_empty() {
            env.push(("COMPARA_UPLOAD_ONE_SHOT", one_shot.into()));
            env.push(("COMPARA_UPLOAD_PART", part.into()));
        }
        let prefix = format!("out{}", id());
        let out = Command::new(env!("CARGO_BIN_EXE_compara"))
            .args(["compare", &local("t_a.csv"), &local("t_b.csv")])
            .args(KEY)
            .args(["-o", &format!("s3://bkt/{prefix}/report.html")])
            .args(["--json", &format!("s3://bkt/{prefix}/summary.json")])
            .args(["--export-dir", &format!("s3://bkt/{prefix}/export/")])
            .envs(borrowed(&env))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(want.0), "{out:?}");
        let up = bucket.join(&prefix);
        assert!(std::fs::metadata(up.join("report.html")).unwrap().len() > 1000);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(up.join("summary.json")).unwrap())
                .unwrap();
        v.as_object_mut().unwrap().remove("meta");
        assert_eq!(v.to_string(), want.2, "{one_shot}: summary");
        let mut rows = String::new();
        for n in ["added.csv", "changed.csv", "removed.csv"] {
            rows.push_str(&std::fs::read_to_string(up.join("export").join(n)).unwrap());
        }
        assert_eq!(rows, want.3, "{one_shot}: exported rows");
    }
    // The summary alone on stdout; the counts line on stderr.
    let out = Command::new(env!("CARGO_BIN_EXE_compara"))
        .args(["compare", &local("t_a.csv"), &local("t_b.csv")])
        .args(KEY)
        .args(["--json", "-"])
        .output()
        .unwrap();
    let mut v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is the summary");
    v.as_object_mut().unwrap().remove("meta");
    assert_eq!(v.to_string(), want.2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("matched"));
}

/// A server that drops some ranged requests is retried through; an object
/// overwritten part-way through the download is an error, not a file stitched
/// together from two versions.
#[test]
fn transient_failures_are_retried_and_an_overwrite_is_caught() {
    let f = fixture("200k", &[]);
    let bucket = f.0.join("bkt");
    let local = |n: &str| bucket.join(n).to_string_lossy().into_owned();
    let want = compare(&local("t_a.csv"), &local("t_b.csv"), &KEY, &[], None);
    let Some(flaky) = Server::start(&f.0, &["--flaky", "3"]) else {
        return;
    };
    let u = &flaky.url;
    // Small parts, so there are enough requests for some to fail.
    let env = [("COMPARA_FETCH_CONCURRENCY", "2")];
    let got = compare(
        &format!("{u}/bkt/t_a.csv"),
        &format!("{u}/bkt/t_b.csv"),
        &KEY,
        &env,
        None,
    );
    assert_eq!(
        (want.0, &want.2, &want.3),
        (got.0, &got.2, &got.3),
        "flaky: {}",
        got.1
    );
    drop(flaky);
    let Some(changing) = Server::start(&f.0, &["--overwrite-after", "2"]) else {
        return;
    };
    let u = &changing.url;
    let (code, err, ..) = compare(
        &format!("{u}/bkt/t_a.csv"),
        &local("t_b.csv"),
        &KEY,
        &[],
        None,
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("changed while it was being read"), "{err}");
}

/// A prefix, a glob, or a name that is a prefix rather than an object reads
/// every part under it as one table, the local directory's answer; Parquet
/// parts only as far as the comparison reads them, gzip parts expanded.
#[test]
fn parts_under_a_prefix_are_one_table() {
    let dir = std::env::temp_dir().join(format!(
        "compara-remote-many-{}-{}",
        std::process::id(),
        id()
    ));
    let d = Dir(dir.clone());
    let gen_dir = d.0.join("gen");
    for (p, rows, seed) in [("p", "30k", "1"), ("q", "10k", "2")] {
        for extra in [
            &[][..],
            // Uncompressed, so a column's chunk is worth not fetching.
            &["--format", "parquet", "--compression", "none"][..],
        ] {
            let out = Command::new(env!("CARGO_BIN_EXE_compara-gen-data"))
                .args(["--rows", rows, "--seed", seed, "--prefix", p, "--out-dir"])
                .arg(&gen_dir)
                .args(extra)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
    }
    let bucket = d.0.join("bkt");
    for side in ["a", "b"] {
        for fmt in ["csv", "parquet"] {
            for (n, p) in [(1, "p"), (2, "q")] {
                let to = bucket.join(format!("{fmt}-{side}/date=2026-10-0{n}/part-0.{fmt}"));
                std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                std::fs::copy(gen_dir.join(format!("{p}_{side}.{fmt}")), &to).unwrap();
            }
            std::fs::write(bucket.join(format!("{fmt}-{side}/_SUCCESS")), "").unwrap();
        }
        // The CSV parts again, gzipped, in the same partitions.
        for (n, p) in [(1, "p"), (2, "q")] {
            let to = bucket.join(format!("gz-{side}/date=2026-10-0{n}/part-0.csv.gz"));
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&std::fs::read(gen_dir.join(format!("{p}_{side}.csv"))).unwrap())
                .unwrap();
            std::fs::write(&to, gz.finish().unwrap()).unwrap();
        }
    }
    let local = |p: &str| bucket.join(p).to_string_lossy().into_owned();
    let want = compare(&local("csv-a"), &local("csv-b"), &KEY, &[], None);
    assert_eq!(want.0, 1, "{}", want.1);
    let Some(server) = Server::start(&d.0, &[]) else {
        return;
    };
    let env = s3_env(&server.url);
    let env = borrowed(&env);
    for (a, b) in [
        ("s3://bkt/csv-a/", "s3://bkt/csv-b/"),
        ("s3://bkt/parquet-a", "s3://bkt/parquet-b"),
        (
            "s3://bkt/parquet-a/date=*/*.parquet",
            "s3://bkt/csv-b/**/part-?.csv",
        ),
        ("s3://bkt/gz-a/", "s3://bkt/gz-b/**/*.csv.gz"),
        // Partitioned text, read whole and given its column on the way.
        ("s3://bkt/csv-a/", "s3://bkt/gz-b/"),
    ] {
        let got = compare(a, b, &KEY, &env, None);
        assert_eq!(
            (got.0, &got.2, &got.3),
            (want.0, &want.2, &want.3),
            "{a} {b}: {}",
            got.1
        );
    }
    // One compared column, far from the key, of the Parquet parts: their footers and three
    // columns' chunks, not the parts.
    let total: u64 = ["a", "b"]
        .iter()
        .flat_map(|s| (1..=2).map(move |n| format!("parquet-{s}/date=2026-10-0{n}/part-0.parquet")))
        .map(|p| std::fs::metadata(bucket.join(p)).unwrap().len())
        .sum();
    let before = server.sent();
    let narrow = [&KEY[..], &["-c", "risk_flag"]].concat();
    let got = compare(
        "s3://bkt/parquet-a/",
        "s3://bkt/parquet-b/",
        &narrow,
        &env,
        None,
    );
    let local_narrow = compare(&local("parquet-a"), &local("parquet-b"), &narrow, &[], None);
    assert_eq!(
        (got.0, &got.2),
        (local_narrow.0, &local_narrow.2),
        "{}",
        got.1
    );
    let sent = server.sent() - before;
    assert!(sent < total / 2, "sent {sent} of {total}: {}", got.1);
    for (spec, says) in [
        ("s3://bkt/nothing/", "no objects under"),
        ("s3://bkt/csv-a/*.parquet", "no objects match"),
        ("s3://bkt/nothing", "not found"),
    ] {
        let got = compare(spec, &local("csv-b"), &KEY, &env, None);
        assert_eq!(got.0, 2, "{spec}: {}", got.1);
        assert!(got.1.contains(says), "{spec}: {}", got.1);
    }
}

/// Runs `compara compare` with the environment cleared of every credential
/// the test process may carry, and `env` set; exit code and stderr.
fn compare_bare(a: &str, b: &str, env: &[(&str, &str)]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_compara"));
    cmd.args(["compare", a, b]).args(KEY);
    for (k, _) in std::env::vars() {
        if k.starts_with("AWS_") || k.starts_with("AZURE_") || k.starts_with("GOOGLE_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd
        .env("HOME", std::env::temp_dir().join("compara-no-home"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// With no credentials anywhere, a public bucket is read unsigned and says
/// so; a private one still fails on the credentials. A profile whose keys come
/// from a `credential_process` -- what SSO and assumed roles reduce to -- is
/// run for them.
#[test]
fn public_buckets_need_no_credentials_and_profiles_run_their_process() {
    let f = fixture("20k", &[]);
    let bucket = f.0.join("bkt");
    let local = |n: &str| bucket.join(n).to_string_lossy().into_owned();
    let want = compare(&local("t_a.csv"), &local("t_b.csv"), &KEY, &[], None);
    let Some(public) = Server::start(&f.0, &[]) else {
        return;
    };
    let endpoint = [
        ("AWS_ENDPOINT_URL", public.url.as_str()),
        ("AWS_REGION", "us-east-1"),
    ];
    let (code, err) = compare_bare("s3://bkt/t_a.csv", "s3://bkt/t_b.csv", &endpoint);
    assert_eq!(code, want.0, "{err}");
    assert!(err.contains("unsigned"), "{err}");
    drop(public);

    let Some(private) = Server::start(&f.0, &["--private"]) else {
        return;
    };
    let endpoint = [
        ("AWS_ENDPOINT_URL", private.url.as_str()),
        ("AWS_REGION", "us-east-1"),
    ];
    let (code, err) = compare_bare("s3://bkt/t_a.csv", "s3://bkt/t_b.csv", &endpoint);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("No usable credentials"), "{err}");

    let home = f.0.join("home");
    std::fs::create_dir_all(home.join(".aws")).unwrap();
    std::fs::write(
        home.join(".aws/config"),
        "[profile ci]\nregion = us-east-1\ncredential_process = echo '{\"Version\": 1, \
         \"AccessKeyId\": \"AKIDEXAMPLE\", \"SecretAccessKey\": \"secret\"}'\n",
    )
    .unwrap();
    let home = home.to_string_lossy().into_owned();
    let env = [
        ("AWS_ENDPOINT_URL", private.url.as_str()),
        ("AWS_PROFILE", "ci"),
        ("HOME", home.as_str()),
    ];
    if cfg!(unix) {
        let (code, err) = compare_bare("s3://bkt/t_a.csv", "s3://bkt/t_b.csv", &env);
        assert_eq!(code, want.0, "{err}");
        assert!(!err.contains("unsigned"), "{err}");
    }
}

/// A table at a cloud metadata address (where a machine's credentials come
/// from) is refused: given directly, or reached by a redirect.
#[test]
fn metadata_addresses_are_not_read() {
    let run = |url: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_compara"))
            .args(["head", url])
            .env_remove("COMPARA_ALLOW_LINK_LOCAL")
            .output()
            .expect("run compara");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (code, err) = run("http://169.254.169.254/latest/meta-data/x.csv");
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("link-local"), "{err}");
    // A server of our own that sends every request there.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/x.csv\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let (code, err) = run(&format!("http://127.0.0.1:{port}/redir.csv"));
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("link-local"), "{err}");
}
