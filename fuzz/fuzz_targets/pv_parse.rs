#![no_main]
use libfuzzer_sys::fuzz_target;
use std::path::Path;
fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = pvlib::suite::parse(Path::new("fuzz.pv"), &s);
    let _ = pvlib::suite::tokenize(&s);
    let _ = pvlib::suite::csv_rows(&s);
    let words: Vec<String> = s.split_whitespace().map(|w| w.to_string()).collect();
    let _ = pvlib::suite::parse_words(&words);
});
