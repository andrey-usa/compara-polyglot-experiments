//! The generated files are shared with the Python, TypeScript, Java and Go
//! generators byte for byte, so a benchmark number from any of them is directly
//! comparable.

use std::fs;

use compara::gendata::{COLUMNS, generate, parse_rows};

#[test]
fn the_shape_and_drift_are_what_the_recipe_says() {
    let dir = std::env::temp_dir().join(format!("compara-gen-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let a = dir.join("a.csv");
    let b = dir.join("b.csv");
    generate(10_000, &a, &b, 7).expect("generate");

    let a_text = fs::read_to_string(&a).expect("read a");
    let b_text = fs::read_to_string(&b).expect("read b");
    let a_lines: Vec<&str> = a_text.trim_end().lines().collect();
    let b_lines: Vec<&str> = b_text.trim_end().lines().collect();

    assert_eq!(a_lines[0], COLUMNS.join(","));
    assert_eq!(a_lines[1].split(',').count(), 20);
    // 10k rows, 1 in 1000 removed, 1 duplicate row added to A, 10 added to B, 1 duplicated in B.
    assert_eq!(a_lines.len(), 10_002);
    assert_eq!(b_lines.len(), 10_002);

    // Money never passes through a float, so a two-decimal amount is the same
    // digits in every implementation regardless of its rounding rule.
    let index = |name: &str| COLUMNS.iter().position(|c| *c == name).expect("column");
    for line in a_lines.iter().chain(&b_lines).skip(1).take(500) {
        let fields: Vec<&str> = line.split(',').collect();
        for column in ["amount", "fee", "balance"] {
            let value = fields[index(column)];
            let (_, decimals) = value
                .split_once('.')
                .unwrap_or_else(|| panic!("{column}={value}"));
            assert_eq!(decimals.len(), 2, "{column}={value} is not 2dp");
        }
        let rate = fields[index("rate")];
        assert_eq!(
            rate.split_once('.').expect("rate").1.len(),
            4,
            "rate={rate} is not 4dp"
        );
    }

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn parse_rows_accepts_the_usual_shorthands() {
    for (input, want) in [
        ("10000", 10_000i64),
        ("10k", 10_000),
        ("1m", 1_000_000),
        ("2.5M", 2_500_000),
        ("1g", 1_000_000_000),
        ("1_000", 1_000),
    ] {
        assert_eq!(parse_rows(input).expect(input), want, "parse_rows({input})");
    }
    assert!(parse_rows("many").is_err());
}
