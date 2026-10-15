//! Benchmarks one scale end to end and records the numbers.
//!
//! ```text
//! cargo run --release --features bench --bin compara-bench -- --rows 1m --engine turbo --out-dir bench
//! ```
//!
//! The comparison runs in a child process so peak RSS is measured honestly
//! rather than including this process's own memory. Writes
//! `bench/<scale>-<engine>.json`, appends a row to `$GITHUB_STEP_SUMMARY` under
//! Actions, and exits non-zero when a budget is exceeded. An engine that cannot
//! handle a scale is recorded as a failed row with the reason rather than
//! aborting the run, because where an engine stops is a result worth having.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use compara::error::{Error, Result};
use compara::gendata::{generate, parse_rows};
use serde_json::{Value, json};

const KEY: &str = "account_id,txn_id";
const IGNORE: &str = "updated_at";

/// The wall-clock and peak-RSS allowance for a scale, on a 4-vCPU / 16 GB
/// GitHub-hosted runner, generous by about 2x.
const BUDGETS: [(i64, f64, f64); 3] = [
    (10_000, 20.0, 1_500.0),
    (1_000_000, 120.0, 6_000.0),
    (10_000_000, 900.0, 12_000.0),
];

fn budget_for(rows: i64) -> (f64, f64) {
    for (limit, seconds, peak) in BUDGETS {
        if rows <= limit {
            return (seconds, peak);
        }
    }
    let (_, seconds, peak) = BUDGETS[BUDGETS.len() - 1];
    (seconds, peak)
}

struct Config {
    rows_arg: String,
    label: String,
    engine: String,
    out_dir: PathBuf,
    data_dir: PathBuf,
    binary: PathBuf,
    keep_data: bool,
    no_budget: bool,
    allow_failure: bool,
    threads: Option<String>,
}

fn main() -> ExitCode {
    let cfg = parse_config();
    match run(&cfg) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn parse_config() -> Config {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| argv.iter().any(|a| a == name);

    let rows_arg = value("--rows")
        .or_else(|| value("-n"))
        .unwrap_or_else(|| "10k".into());
    Config {
        label: rows_arg.to_lowercase(),
        rows_arg,
        engine: value("--engine").unwrap_or_else(|| "auto".into()),
        out_dir: PathBuf::from(
            value("--out-dir")
                .or_else(|| value("-o"))
                .unwrap_or_else(|| "bench".into()),
        ),
        data_dir: PathBuf::from(value("--data-dir").unwrap_or_else(|| "data".into())),
        // Defaults to the release binary beside this one, so the compile is not
        // counted as comparison time.
        binary: PathBuf::from(value("--binary").unwrap_or_else(default_binary)),
        keep_data: flag("--keep-data"),
        no_budget: flag("--no-budget"),
        allow_failure: flag("--allow-failure"),
        threads: value("--threads"),
    }
}

fn default_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("compara")))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "compara".into())
}

fn run(cfg: &Config) -> Result<ExitCode> {
    let rows = parse_rows(&cfg.rows_arg)?;
    fs::create_dir_all(&cfg.out_dir)?;
    fs::create_dir_all(&cfg.data_dir)?;

    let a = cfg.data_dir.join(format!("{}_a.csv", cfg.label));
    let b = cfg.data_dir.join(format!("{}_b.csv", cfg.label));
    let mut gen_seconds = 0.0;
    if file_size(&a) == 0 || file_size(&b) == 0 {
        let start = Instant::now();
        generate(rows, &a, &b, 7)?;
        gen_seconds = round(start.elapsed().as_secs_f64(), 1);
        println!("rust: generated {rows} rows x 20 columns in {gen_seconds}s");
    }

    let report = cfg
        .out_dir
        .join(format!("{}-{}.html", cfg.label, cfg.engine));
    let summary = cfg
        .out_dir
        .join(format!("{}-{}-summary.json", cfg.label, cfg.engine));
    let _ = fs::remove_file(&summary); // never read a stale summary as this run's result

    let mut args: Vec<String> = ["compare"].iter().map(|s| s.to_string()).collect();
    args.extend([
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
        "-k".into(),
        KEY.into(),
        "-i".into(),
        IGNORE.into(),
        "--engine".into(),
        cfg.engine.clone(),
        "-o".into(),
        report.to_string_lossy().into_owned(),
        "--json".into(),
        summary.to_string_lossy().into_owned(),
    ]);
    if let Some(threads) = &cfg.threads {
        args.extend(["--threads".to_string(), threads.clone()]);
    }

    let (status, stderr, wall) = run_child(&cfg.binary, &args)?;
    let peak_mb = round(peak_rss_kb(&stderr) as f64 / 1024.0, 1);
    let input_mb = round((file_size(&a) + file_size(&b)) as f64 / 1e6, 1);
    let runner = runner_description();

    let fail = |why: String| -> Result<ExitCode> {
        record_failure(
            cfg,
            rows,
            gen_seconds,
            input_mb,
            peak_mb,
            wall,
            status,
            &why,
            &runner,
        )?;
        eprintln!("compare exited with {status:?}: {why}");
        clean_data(cfg, &a, &b);
        Ok(if cfg.allow_failure {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        })
    };

    if !matches!(status, Some(0) | Some(1)) {
        return fail(classify(status, &stderr));
    }
    let Some(counts) = read_counts(&summary) else {
        // The child reported success but wrote nothing usable. Record it as a
        // failure rather than crashing the harness, and never present it as a result.
        return fail("no summary written".to_string());
    };

    let report_mb = round(file_size(&report) as f64 / 1e6, 2);
    let total = counts["a_rows"].as_i64().unwrap_or(0) + counts["b_rows"].as_i64().unwrap_or(0);
    let per_second = if wall > 0.0 {
        (total as f64 / wall).round() as i64
    } else {
        0
    };

    write_json(
        &cfg.out_dir
            .join(format!("{}-{}.json", cfg.label, cfg.engine)),
        &json!({
            "rows": rows, "scale": cfg.label, "engine": cfg.engine,
            "generate_seconds": gen_seconds, "compare_seconds": wall,
            "peak_rss_mb": peak_mb, "input_mb": input_mb, "report_mb": report_mb,
            "rows_per_second": per_second, "counts": counts, "runner": runner,
        }),
    )?;

    let (budget_seconds, budget_peak) = budget_for(rows);
    let ok = wall <= budget_seconds && peak_mb <= budget_peak;
    let verdict = if ok {
        "pass".to_string()
    } else {
        format!("over budget ({budget_seconds:.0}s / {budget_peak:.0} MB)")
    };
    let line = format!(
        "| {} | {} | {} MB | {}s | **{}s** | {}/s | {} MB | {} MB | {} | {} | {} | {} |",
        cfg.label,
        cfg.engine,
        num(input_mb),
        gen_seconds,
        wall,
        comma(per_second),
        num(peak_mb),
        report_mb,
        comma(counts["changed"].as_i64().unwrap_or(0)),
        comma(counts["added"].as_i64().unwrap_or(0)),
        comma(counts["removed"].as_i64().unwrap_or(0)),
        verdict
    );
    append_summary(&line);
    println!("{line}");

    clean_data(cfg, &a, &b);
    Ok(if ok || cfg.no_budget {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Runs the comparison and returns its exit status, stderr and wall time.
fn run_child(binary: &Path, args: &[String]) -> Result<(Option<i32>, String, f64)> {
    let start = Instant::now();
    // The child reports its own peak RSS on stderr, so the figure excludes this process.
    let output = Command::new(binary)
        .args(args)
        .env("COMPARA_PRINT_PEAK_RSS", "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::new(format!("cannot run {}: {e}", binary.display())))?;
    let wall = round(start.elapsed().as_secs_f64(), 2);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    eprint!("{stderr}");
    Ok((output.status.code(), stderr, wall))
}

/// Records a scale an engine could not handle: a JSON row plus a line in the
/// results table.
#[allow(clippy::too_many_arguments)]
fn record_failure(
    cfg: &Config,
    rows: i64,
    gen_seconds: f64,
    input_mb: f64,
    peak_mb: f64,
    wall: f64,
    status: Option<i32>,
    why: &str,
    runner: &str,
) -> Result<()> {
    write_json(
        &cfg.out_dir
            .join(format!("{}-{}.json", cfg.label, cfg.engine)),
        &json!({
            "rows": rows, "scale": cfg.label, "engine": cfg.engine,
            "generate_seconds": gen_seconds, "compare_seconds": Value::Null,
            "peak_rss_mb": if peak_mb > 0.0 { json!(peak_mb) } else { Value::Null },
            "input_mb": input_mb, "report_mb": Value::Null,
            "rows_per_second": Value::Null, "counts": Value::Null,
            "failed": why, "exit_status": status, "wall_before_failure": wall, "runner": runner,
        }),
    )?;
    let peak = if peak_mb > 0.0 {
        format!("{} MB", num(peak_mb))
    } else {
        "-".to_string()
    };
    let line = format!(
        "| {} | {} | {} MB | {}s | **{why}** after {wall}s | - | {peak} | - | - | - | - | failed |",
        cfg.label,
        cfg.engine,
        num(input_mb),
        gen_seconds
    );
    append_summary(&line);
    println!("{line}");
    Ok(())
}

/// Turns a dead child process into a short honest reason for the results table.
fn classify(status: Option<i32>, stderr: &str) -> String {
    if stderr.contains("memory allocation of") || stderr.contains("Out of memory") {
        return "Rust allocation failure".to_string();
    }
    if stderr.contains("No space left on device") {
        return "disk full".to_string();
    }
    match status {
        Some(137) => "OOM killed".to_string(),
        Some(139) => "segfault".to_string(),
        // A process killed by a signal has no exit code of its own.
        None => "killed by a signal".to_string(),
        Some(code) => format!("exit {code}"),
    }
}

fn read_counts(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    parsed.get("counts").filter(|c| c.is_object()).cloned()
}

fn peak_rss_kb(stderr: &str) -> i64 {
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix("PEAK_RSS_KB"))
        .filter_map(|kb| kb.trim().parse::<i64>().ok())
        .max()
        .unwrap_or(0)
}

fn runner_description() -> String {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!(
        "{} {} rust {} {cpus} cpu",
        std::env::consts::OS,
        std::env::consts::ARCH,
        option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("stable")
    )
}

fn clean_data(cfg: &Config, a: &Path, b: &Path) {
    if cfg.keep_data {
        return;
    }
    let _ = fs::remove_file(a);
    let _ = fs::remove_file(b);
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    fs::write(path, text).map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))
}

fn append_summary(line: &str) {
    let Some(step) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let path = PathBuf::from(step);
    let fresh = file_size(&path) == 0;
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if fresh {
        let _ = writeln!(
            file,
            "| scale | engine | input | generate | compare | throughput | peak RSS | report \
             | changed | added | removed | budget |\n|---|---|---|---|---|---|---|---|---|---|---|---|"
        );
    }
    let _ = writeln!(file, "{line}");
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn round(v: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (v * f).round() / f
}

/// Prints a whole number without a pointless ".0", and one decimal otherwise.
fn num(v: f64) -> String {
    if v == v.trunc() {
        comma(v as i64)
    } else {
        format!("{v:.1}")
    }
}

fn comma(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
