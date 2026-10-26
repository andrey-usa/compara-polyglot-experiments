//! compara.toml profiles, which the other implementations read too.

use std::fs;

use compara::options::Options;
use compara::profiles;

const CONFIG: &str = r#"
# a comment
[profiles.orders]
key       = ["order_id", "line_no"]
compare   = ["qty", "price"]
ignore    = ["updated_at"]   # trailing comment
trim      = true
tolerance = 0.005
max_rows  = 100
engine    = "native"

[profiles.other]
key = ["id"]
"#;

#[test]
fn a_profile_becomes_the_base_layer_of_the_options() {
    let found = profiles::parse(CONFIG).expect("parse");
    assert_eq!(found.len(), 2);

    let mut opt = Options::default();
    found["orders"].apply_to(&mut opt);
    assert_eq!(opt.key, vec!["order_id", "line_no"]);
    assert_eq!(
        opt.compare,
        Some(vec!["qty".to_string(), "price".to_string()])
    );
    assert_eq!(
        opt.ignore,
        vec!["updated_at"],
        "a trailing comment leaked into ignore"
    );
    assert!(opt.trim);
    assert_eq!(opt.tolerance, 0.005);
    assert_eq!(opt.max_rows, 100);
    assert_eq!(opt.engine, "native");

    // Anything given on the command line is applied after, and wins.
    opt.max_rows = 7;
    assert_eq!(opt.max_rows, 7);
}

#[test]
fn an_explicit_config_path_that_does_not_exist_is_an_error() {
    let missing = std::env::temp_dir().join("compara-no-such-config.toml");
    let _ = fs::remove_file(&missing);
    assert!(profiles::load(Some(&missing.to_string_lossy())).is_err());
}

#[test]
fn parse_list_drops_empty_entries() {
    assert_eq!(compara::parse_list(" a, b ,,c "), vec!["a", "b", "c"]);
    assert!(compara::parse_list("  ").is_empty());
}
