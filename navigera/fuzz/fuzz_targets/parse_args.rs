#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let argv: Vec<String> = std::iter::once("navigera".to_string())
        .chain(s.split('\0').map(|w| w.to_string()))
        .collect();
    let _ = navigera::protocol::parse_args(argv);
});
