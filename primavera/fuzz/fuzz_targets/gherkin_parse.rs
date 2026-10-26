#![no_main]
use libfuzzer_sys::fuzz_target;
use std::path::Path;
fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = pvlib::gherkin::parse(Path::new("fuzz.feature"), &s);
    let _ = pvlib::gherkin::step_pattern(&s);
});
